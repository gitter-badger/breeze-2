#![allow(dead_code)]

mod prelude;

use self::prelude::*;

use std::path::Path;
use std::sync::Arc;

use std::cmp::min;
use std::io::{self, Write};
use structopt::StructOpt;
use termion::color;
use termion::event::Key;
use termion::input::TermRead;
use termion::raw::IntoRawMode;
use termion::screen::*;

use ropey::Rope;

mod coord;
mod idx;
mod opts;
mod selection;

use crate::{coord::*, idx::Idx, selection::*};

#[derive(Default)]
struct CachingAnsciWriter {
    buf: Vec<u8>,
    cur_fg: Option<u8>,
    cur_bg: Option<u8>,
}

impl CachingAnsciWriter {
    fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    fn reset_color(&mut self) -> io::Result<()> {
        if self.cur_fg.is_some() {
            self.cur_fg = None;
            write!(&mut self.buf, "{}", color::Fg(color::Reset),)?;
        }

        if self.cur_bg.is_some() {
            self.cur_bg = None;
            write!(&mut self.buf, "{}", color::Bg(color::Reset),)?;
        }
        Ok(())
    }

    fn change_color(&mut self, fg: color::AnsiValue, bg: color::AnsiValue) -> io::Result<()> {
        if self.cur_fg != Some(fg.0) {
            self.cur_fg = Some(fg.0);
            write!(&mut self.buf, "{}", color::Fg(fg),)?;
        }

        if self.cur_bg != Some(bg.0) {
            self.cur_bg = Some(bg.0);
            write!(&mut self.buf, "{}", color::Bg(bg),)?;
        }
        Ok(())
    }
}

impl io::Write for CachingAnsciWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.buf.flush()
    }
}

fn sub_rope(text: &Rope, start: usize, end: usize) -> Rope {
    let mut sub = text.clone();

    assert!(start <= end);

    if end < text.len_chars() {
        sub.remove(end..text.len_chars());
    }
    sub.remove(..start);

    sub
}

/// Buffer
///
/// A file opened for edition + some state around
#[derive(Debug, Clone)]
struct Buffer {
    text: ropey::Rope,
    selections: Vec<SelectionUnaligned>,
    primary_sel_i: usize,
}

impl Default for Buffer {
    fn default() -> Self {
        let sel = SelectionUnaligned::default();
        Self {
            text: Rope::default(),
            selections: vec![sel],
            primary_sel_i: 0,
        }
    }
}

impl Buffer {
    fn from_text(text: Rope) -> Self {
        Self {
            text: text,
            ..default()
        }
    }

    fn for_each_selection<F, R>(&self, mut f: F) -> Vec<R>
    where
        F: FnMut(&SelectionUnaligned, &Rope) -> R,
    {
        let Self {
            ref selections,
            ref text,
            ..
        } = *self;

        selections.iter().map(|sel| f(sel, text)).collect()
    }

    fn for_each_selection_mut<F, R>(&mut self, mut f: F) -> Vec<R>
    where
        F: FnMut(&mut SelectionUnaligned, &mut Rope) -> R,
    {
        let Self {
            ref mut selections,
            ref mut text,
            ..
        } = *self;

        selections.iter_mut().map(|sel| f(sel, text)).collect()
    }

    fn for_each_enumerated_selection<F, R>(&self, mut f: F) -> Vec<R>
    where
        F: FnMut(usize, &SelectionUnaligned, &Rope) -> R,
    {
        let Self {
            ref selections,
            ref text,
            ..
        } = *self;

        selections
            .iter()
            .enumerate()
            .map(|(i, sel)| f(i, sel, text))
            .collect()
    }
    fn for_each_enumerated_selection_mut<F, R>(&mut self, mut f: F) -> Vec<R>
    where
        F: FnMut(usize, &mut SelectionUnaligned, &mut Rope) -> R,
    {
        let Self {
            ref mut selections,
            ref mut text,
            ..
        } = *self;

        selections
            .iter_mut()
            .enumerate()
            .map(|(i, sel)| f(i, sel, text))
            .collect()
    }

    fn is_idx_selected(&self, idx: Idx) -> bool {
        self.selections
            .iter()
            .any(|sel| sel.align(&self.text).is_idx_inside(idx))
    }

    fn reverse_selections(&mut self) {
        self.for_each_selection_mut(|sel, _text| *sel = sel.reversed());
    }

    fn insert(&mut self, ch: char) {
        let mut insertion_points = self
            .for_each_enumerated_selection(|i, sel, text| (i, sel.cursor.align(text).to_idx(text)));
        insertion_points.sort_by_key(|&(_, idx)| idx);
        insertion_points.reverse();

        // we insert from the back, fixing idx past the insertion every time
        // this is O(n^2) while it could be O(n)
        for (i, (_, idx)) in insertion_points.iter().enumerate() {
            self.text.insert_char(idx.0, ch);
            for fixing_i in 0..=i {
                let fixing_sel = &mut self.selections[insertion_points[fixing_i].0];
                fixing_sel.cursor = fixing_sel.cursor.forward(&self.text);
                *fixing_sel = fixing_sel.collapsed();
            }
        }
    }

    fn delete(&mut self) -> Vec<Rope> {
        let res = self.for_each_enumerated_selection_mut(|i, sel, text| {
            let range = sel.align(text).sorted_range_usize();
            let yanked = sub_rope(text, range.start, range.end);
            *sel = sel.collapsed();
            (yanked, i, range)
        });
        let mut removal_points = vec![];
        let mut yanked = vec![];

        for (y, i, r) in res.into_iter() {
            removal_points.push((i, r));
            yanked.push(y);
        }

        self.remove_ranges(removal_points);

        yanked
    }

    fn yank(&mut self) -> Vec<Rope> {
        self.for_each_selection_mut(|sel, text| {
            let range = sel.align(text).sorted_range_usize();
            sub_rope(text, range.start, range.end)
        })
    }

    fn paste(&mut self, yanked: &[Rope]) {
        let mut insertion_points = self
            .for_each_enumerated_selection(|i, sel, text| (i, sel.cursor.align(text).to_idx(text)));
        insertion_points.sort_by_key(|&(_, idx)| idx);
        insertion_points.reverse();

        // we insert from the back, fixing idx past the insertion every time
        // this is O(n^2) while it could be O(n)
        for (i, (_, idx)) in insertion_points.iter().enumerate() {
            if let Some(to_yank) = yanked.get(i) {
                for chunk in to_yank.chunks() {
                    self.text.insert(idx.0, chunk);
                }
                {
                    let fixing_sel = &mut self.selections[insertion_points[i].0];
                    if fixing_sel.align(&self.text).is_forward().unwrap_or(true) {
                        fixing_sel.anchor = fixing_sel.cursor;
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward_n(to_yank.len_chars(), &self.text);
                    } else {
                        fixing_sel.anchor =
                            fixing_sel.cursor.forward_n(to_yank.len_chars(), &self.text);
                    }
                }
                for fixing_i in 0..i {
                    let fixing_sel = &mut self.selections[insertion_points[fixing_i].0];
                    if *idx <= fixing_sel.cursor.align(&self.text).to_idx(&self.text) {
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward_n(to_yank.len_chars(), &self.text);
                    }
                    if *idx <= fixing_sel.anchor.align(&self.text).to_idx(&self.text) {
                        fixing_sel.anchor =
                            fixing_sel.anchor.forward_n(to_yank.len_chars(), &self.text);
                    }
                }
            }
        }
    }

    fn paste_extend(&mut self, yanked: &[Rope]) {
        let mut insertion_points = self
            .for_each_enumerated_selection(|i, sel, text| (i, sel.cursor.align(text).to_idx(text)));
        insertion_points.sort_by_key(|&(_, idx)| idx);
        insertion_points.reverse();

        // we insert from the back, fixing idx past the insertion every time
        // this is O(n^2) while it could be O(n)
        for (i, (_, idx)) in insertion_points.iter().enumerate() {
            if let Some(to_yank) = yanked.get(i) {
                for chunk in to_yank.chunks() {
                    self.text.insert(idx.0, chunk);
                }
                {
                    let fixing_sel = &mut self.selections[insertion_points[i].0];
                    if fixing_sel.align(&self.text).is_forward().unwrap_or(true) {
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward_n(to_yank.len_chars(), &self.text);
                    } else {
                        fixing_sel.anchor =
                            fixing_sel.anchor.forward_n(to_yank.len_chars(), &self.text);
                    }
                }
                for fixing_i in 0..i {
                    let fixing_sel = &mut self.selections[insertion_points[fixing_i].0];
                    if *idx <= fixing_sel.cursor.align(&self.text).to_idx(&self.text) {
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward_n(to_yank.len_chars(), &self.text);
                    }
                    if *idx <= fixing_sel.anchor.align(&self.text).to_idx(&self.text) {
                        fixing_sel.anchor =
                            fixing_sel.anchor.forward_n(to_yank.len_chars(), &self.text);
                    }
                }
            }
        }
    }

    /// Remove text at given ranges
    ///
    /// `removal_points` contains list of `(selection_index, range)`,
    fn remove_ranges(&mut self, mut removal_points: Vec<(usize, std::ops::Range<usize>)>) {
        removal_points.sort_by_key(|&(_, ref range)| range.start);
        removal_points.reverse();

        // we remove from the back, fixing idx past the removal every time
        // this is O(n^2) while it could be O(n)
        for (_, (_, range)) in removal_points.iter().enumerate() {
            self.sub_to_every_selection_after(Idx(range.start), range.len());
            // remove has to be after fixes, otherwise to_idx conversion
            // will use the new buffer content, which will give wrong results
            self.text.remove(range.clone());
        }
    }

    fn backspace(&mut self) {
        let removal_points = self.for_each_enumerated_selection_mut(|i, sel, text| {
            let sel_aligned = sel.align(text);
            let range = (sel_aligned.cursor.0 - 1)..sel_aligned.cursor.0;
            *sel = sel.collapsed();

            (i, range)
        });

        self.remove_ranges(removal_points);
    }

    fn add_to_every_selection_after(&mut self, idx: Idx, offset: usize) {
        self.for_each_selection_mut(|sel, text| {
            let cursor_idx = sel.cursor.to_idx(text);
            let anchor_idx = sel.cursor.to_idx(text);

            if idx <= cursor_idx {
                sel.cursor = Idx(cursor_idx.0.saturating_add(offset))
                    .to_coord(text)
                    .into();
            }
            if idx <= anchor_idx {
                sel.anchor = Idx(anchor_idx.0.saturating_add(offset))
                    .to_coord(text)
                    .into();
            }
        });
    }

    fn sub_to_every_selection_after(&mut self, idx: Idx, offset: usize) {
        self.for_each_selection_mut(|sel, text| {
            let cursor_idx = sel.cursor.to_idx(text);
            let anchor_idx = sel.anchor.to_idx(text);
            if idx < cursor_idx {
                sel.cursor = Idx(cursor_idx.0.saturating_sub(offset))
                    .to_coord(text)
                    .into();
            }
            if idx < anchor_idx {
                sel.anchor = Idx(anchor_idx.0.saturating_sub(offset))
                    .to_coord(text)
                    .into();
            }
        });
    }

    fn move_cursor<F>(&mut self, f: F)
    where
        F: Fn(CoordUnaligned, &Rope) -> CoordUnaligned,
    {
        self.for_each_selection_mut(|sel, text| {
            let new_cursor = f(sel.cursor, text);
            sel.anchor = sel.cursor;
            sel.cursor = new_cursor;
        });
    }

    fn move_cursor_2<F>(&mut self, f: F)
    where
        F: Fn(CoordUnaligned, &Rope) -> (CoordUnaligned, CoordUnaligned),
    {
        self.for_each_selection_mut(|sel, text| {
            let (new_anchor, new_cursor) = f(sel.cursor, text);
            sel.anchor = new_anchor;
            sel.cursor = new_cursor;
        });
    }

    fn extend_cursor<F>(&mut self, f: F)
    where
        F: Fn(CoordUnaligned, &Rope) -> CoordUnaligned,
    {
        self.for_each_selection_mut(|sel, text| {
            sel.cursor = f(sel.cursor, text);
        });
    }

    fn extend_cursor_2<F>(&mut self, f: F)
    where
        F: Fn(CoordUnaligned, &Rope) -> (CoordUnaligned, CoordUnaligned),
    {
        self.for_each_selection_mut(|sel, text| {
            let (_new_anchor, new_cursor) = f(sel.cursor, text);
            sel.cursor = new_cursor;
        });
    }
    fn change_selection<F>(&mut self, f: F)
    where
        F: Fn(CoordUnaligned, CoordUnaligned, &Rope) -> (CoordUnaligned, CoordUnaligned),
    {
        self.for_each_selection_mut(|sel, text| {
            let (new_cursor, new_anchor) = f(sel.cursor, sel.anchor, text);
            sel.anchor = new_anchor;
            sel.cursor = new_cursor;
        });
    }

    fn move_cursor_backward(&mut self) {
        self.move_cursor(CoordUnaligned::backward);
    }

    fn move_cursor_forward(&mut self) {
        self.move_cursor(CoordUnaligned::forward);
    }

    fn move_cursor_down(&mut self) {
        self.move_cursor(CoordUnaligned::down_unaligned);
    }

    fn move_cursor_up(&mut self) {
        self.move_cursor(CoordUnaligned::up_unaligned);
    }

    fn move_cursor_forward_word(&mut self) {
        self.move_cursor_2(CoordUnaligned::forward_word)
    }

    fn move_cursor_backward_word(&mut self) {
        self.move_cursor_2(CoordUnaligned::backward_word)
    }

    fn cursor_pos(&self) -> Coord {
        self.selections[0].cursor.align(&self.text)
    }

    fn move_line(&mut self) {
        self.change_selection(|cursor, _anchor, text| {
            (
                cursor.forward_past_line_end(text),
                cursor.backward_to_line_start(text),
            )
        });
    }

    fn extend_line(&mut self) {
        self.change_selection(|cursor, anchor, text| {
            let anchor = min(cursor, anchor);

            (
                cursor.forward_past_line_end(text),
                if anchor.column == 0 {
                    anchor
                } else {
                    anchor.backward_to_line_start(text)
                },
            )
        });
    }
}

/// Mode handles keypresses
trait Mode {
    /// Transform state into next state
    fn handle(&self, state: State, key: Key) -> State;
    fn name(&self) -> &str;
}

struct InsertMode;
struct NormalMode;

impl Mode for InsertMode {
    fn name(&self) -> &str {
        "insert"
    }

    fn handle(&self, mut state: State, key: Key) -> State {
        match key {
            Key::Esc => {
                state.modes.pop();
            }
            Key::Char('\n') => {
                state.buffer.insert('\n');
            }
            Key::Backspace => {
                state.buffer.backspace();
            }
            Key::Left => {
                state.buffer.move_cursor_backward();
            }
            Key::Right => {
                state.buffer.move_cursor_forward();
            }
            Key::Up => {
                state.buffer.move_cursor_up();
            }
            Key::Down => {
                state.buffer.move_cursor_down();
            }
            Key::Char(ch) => {
                if !ch.is_control() {
                    state.buffer.insert(ch);
                }
            }
            _ => {}
        }
        state
    }
}

impl Mode for NormalMode {
    fn name(&self) -> &str {
        "normal"
    }

    fn handle(&self, mut state: State, key: Key) -> State {
        match key {
            Key::Char('q') => {
                state.quit = true;
            }
            Key::Left => {
                state.buffer.move_cursor_backward();
            }
            Key::Right => {
                state.buffer.move_cursor_forward();
            }
            Key::Up => {
                state.buffer.move_cursor_up();
            }
            Key::Down => {
                state.buffer.move_cursor_down();
            }
            Key::Char('i') => {
                state.modes.push(Arc::new(InsertMode));
            }
            Key::Char('h') => {
                state.buffer.move_cursor(CoordUnaligned::backward);
            }
            Key::Char('H') => {
                state.buffer.extend_cursor(CoordUnaligned::backward);
            }
            Key::Char('l') => {
                state.buffer.move_cursor(CoordUnaligned::forward);
            }
            Key::Char('L') => {
                state.buffer.extend_cursor(CoordUnaligned::forward);
            }
            Key::Char('j') => {
                state.buffer.move_cursor(CoordUnaligned::down_unaligned);
            }
            Key::Char('J') => {
                state.buffer.extend_cursor(CoordUnaligned::down_unaligned);
            }
            Key::Char('k') => {
                state.buffer.move_cursor(CoordUnaligned::up_unaligned);
            }
            Key::Char('K') => {
                state.buffer.extend_cursor(CoordUnaligned::up_unaligned);
            }
            Key::Char('d') => {
                state.yanked = state.buffer.delete();
            }
            Key::Char('c') => {
                state.yanked = state.buffer.delete();
                state.modes.push(Arc::new(InsertMode));
            }
            Key::Char('y') => {
                state.yanked = state.buffer.yank();
            }
            Key::Char('p') => {
                state.buffer.paste(&state.yanked);
            }
            Key::Char('P') => {
                state.buffer.paste_extend(&state.yanked);
            }
            Key::Char('w') => {
                state.buffer.move_cursor_2(CoordUnaligned::forward_word);
            }
            Key::Char('W') => {
                state.buffer.extend_cursor_2(CoordUnaligned::forward_word);
            }
            Key::Char('b') => {
                state.buffer.move_cursor_2(CoordUnaligned::backward_word);
            }
            Key::Char('B') => {
                state.buffer.extend_cursor_2(CoordUnaligned::backward_word);
            }
            Key::Char('x') => {
                state.buffer.move_line();
            }
            Key::Char('X') => {
                state.buffer.extend_line();
            }
            Key::Char('\'') | Key::Alt(';') => {
                state.buffer.reverse_selections();
            }
            _ => {}
        }
        state
    }
}

/// The editor state
#[derive(Default, Clone)]
struct State {
    quit: bool,
    modes: Vec<Arc<Mode>>,
    buffer: Buffer,
    yanked: Vec<Rope>,
}

/// The editor instance
///
/// Screen drawing + state handling
struct Breeze {
    state: State,
    screen: AlternateScreen<termion::raw::RawTerminal<std::io::Stdout>>,
    display_cols: usize,
    display_rows: usize,
}

impl Breeze {
    fn init() -> Result<Self> {
        let screen = AlternateScreen::from(std::io::stdout().into_raw_mode().unwrap());

        let mut state = State::default();
        state.modes.push(Arc::new(NormalMode));
        let (cols, rows) = termion::terminal_size()?;
        Ok(Self {
            state,
            display_cols: cols as usize,
            display_rows: rows as usize,
            screen,
        })
    }

    fn open(&mut self, path: &Path) -> Result<()> {
        let text = Rope::from_reader(std::io::BufReader::new(std::fs::File::open(path)?))?;
        self.state.buffer = Buffer::from_text(text);
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        self.draw_buffer()?;

        let stdin = std::io::stdin();
        for c in stdin.keys() {
            match c {
                Ok(key) => {
                    self.state = self
                        .state
                        .modes
                        .last()
                        .expect("at least one mode")
                        .handle(self.state.clone(), key);
                }
                Err(e) => panic!("{}", e),
            }

            if self.state.quit {
                return Ok(());
            }
            self.draw_buffer()?;
        }
        Ok(())
    }

    fn draw_buffer(&mut self) -> Result<()> {
        self.screen.write_all(&self.draw_to_buf())?;
        self.screen.flush()?;
        Ok(())
    }

    fn draw_to_buf(&self) -> Vec<u8> {
        let mut buf = CachingAnsciWriter::default();

        buf.reset_color().unwrap();

        write!(&mut buf, "{}", termion::clear::All).unwrap();
        let mut ch_idx = 0;
        for (line_i, line) in self
            .state
            .buffer
            .text
            .lines()
            .enumerate()
            .take(self.display_rows)
        {
            write!(&mut buf, "{}", termion::cursor::Goto(1, line_i as u16 + 1)).unwrap();
            for (char_i, ch) in line.chars().enumerate().take(self.display_cols) {
                let in_selection = self.state.buffer.is_idx_selected(Idx(ch_idx + char_i));
                let ch = if ch == '\n' {
                    if in_selection {
                        '·'
                    } else {
                        ' '
                    }
                } else {
                    ch
                };

                if in_selection {
                    buf.change_color(color::AnsiValue(16), color::AnsiValue(4))
                        .unwrap();
                    write!(&mut buf, "{}", ch).unwrap();
                } else {
                    buf.reset_color().unwrap();
                    write!(&mut buf, "{}", ch).unwrap();
                }
            }
            ch_idx += line.len_chars();
        }

        // status
        buf.reset_color().unwrap();
        write!(
            &mut buf,
            "{}{}",
            termion::cursor::Goto(1, self.display_rows as u16),
            self.state.modes.last().unwrap().name(),
        )
        .unwrap();

        // cursor
        let cursor = self.state.buffer.cursor_pos();
        write!(
            &mut buf,
            "\x1b[6 q{}{}",
            termion::cursor::Goto(cursor.column as u16 + 1, cursor.line as u16 + 1),
            termion::cursor::Show,
        )
        .unwrap();
        buf.into_vec()
    }
}

fn main() -> Result<()> {
    let opt = opts::Opts::from_args();
    let mut brz = Breeze::init()?;

    if let Some(path) = opt.input {
        brz.open(&path)?;
    }

    brz.run()?;
    Ok(())
}
