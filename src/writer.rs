use ahash::AHashMap;
use crossbeam_queue::ArrayQueue;
use crossterm::{
    cursor::{Hide, MoveTo, MoveToNextLine, Show},
    event::{
        poll, read, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute, queue,
    style::{
        Attribute, Attributes, Color, SetAttribute, SetAttributes, SetBackgroundColor,
        SetForegroundColor,
    },
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, DisableLineWrap, EnableLineWrap,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
    Result,
};
use rayon::prelude::*;
use smallvec::SmallVec;
use std::{
    fs::File,
    io::Write,
    sync::atomic::Ordering,
    sync::Arc,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthChar;

use crate::shared::{RpChar, RpLine};

type SearchPositionArr = SmallVec<[SearchPosition; 4]>;
const OUTBUF_SIZE: usize = 1024 * 20;

#[cfg(unix)]
fn get_output() -> File {
    File::create("/dev/tty").expect("Can't open tty")
}

#[cfg(windows)]
fn get_output() -> File {
    File::create("CON:").expect("Can't open con")
}

#[derive(Clone, Copy)]
pub struct SearchPosition {
    start: u32,
}

#[derive(Clone, PartialEq, Eq)]
pub enum PromptState {
    Normal,
    Number(usize),
    Search(String),
}

impl PromptState {
    pub fn take(&mut self) -> Self {
        std::mem::replace(self, Self::Normal)
    }
}

#[derive(Clone, Copy)]
pub enum ScrollSize {
    One,
    HalfPage,
    Page,
    End,
}

impl ScrollSize {
    pub fn calculate(self, terminal_line: usize) -> usize {
        match self {
            Self::One => 1,
            Self::HalfPage => terminal_line / 2,
            Self::Page => terminal_line,
            Self::End => usize::MAX,
        }
    }
}

#[derive(Clone, Copy)]
pub enum KeyBehavior {
    Quit,

    Down(ScrollSize),
    Up(ScrollSize),

    SearchNext,
    SearchPrev,

    NormalMode,
    Number(u32),
    Search,
}

fn default_keymap() -> AHashMap<KeyEvent, KeyBehavior> {
    let mut dict = AHashMap::new();

    macro_rules! keymap {
        ($($modifier:expr => [$(($code:expr, $behavior:expr),)*],)*) => {
            $(
                $(
                    dict.insert(KeyEvent::new($code, $modifier), $behavior);
                )*
            )*
        }
    }

    keymap! {
        KeyModifiers::NONE => [
            (KeyCode::Enter, KeyBehavior::Down(ScrollSize::One)),
            (KeyCode::Down, KeyBehavior::Down(ScrollSize::One)),
            (KeyCode::Char('j'), KeyBehavior::Down(ScrollSize::One)),

            (KeyCode::Up, KeyBehavior::Up(ScrollSize::One)),
            (KeyCode::Char('k'), KeyBehavior::Up(ScrollSize::One)),

            (KeyCode::Char('u'), KeyBehavior::Up(ScrollSize::HalfPage)),
            (KeyCode::Char('d'), KeyBehavior::Down(ScrollSize::HalfPage)),
            (KeyCode::Left, KeyBehavior::Up(ScrollSize::HalfPage)),
            (KeyCode::Right, KeyBehavior::Down(ScrollSize::HalfPage)),

            (KeyCode::Char('f'), KeyBehavior::Down(ScrollSize::Page)),
            (KeyCode::Char(' '), KeyBehavior::Down(ScrollSize::Page)),
            (KeyCode::Char('b'), KeyBehavior::Up(ScrollSize::Page)),
            (KeyCode::PageDown, KeyBehavior::Down(ScrollSize::Page)),
            (KeyCode::PageUp, KeyBehavior::Up(ScrollSize::Page)),

            (KeyCode::Esc, KeyBehavior::NormalMode),
            (KeyCode::Home, KeyBehavior::Up(ScrollSize::End)),
            (KeyCode::End, KeyBehavior::Down(ScrollSize::End)),
            (KeyCode::Char('g'), KeyBehavior::Up(ScrollSize::End)),

            (KeyCode::Char('q'), KeyBehavior::Quit),

            (KeyCode::Char('/'), KeyBehavior::Search),
            (KeyCode::Char('n'), KeyBehavior::SearchNext),

            (KeyCode::Char('0'), KeyBehavior::Number(0)),
            (KeyCode::Char('1'), KeyBehavior::Number(1)),
            (KeyCode::Char('2'), KeyBehavior::Number(2)),
            (KeyCode::Char('3'), KeyBehavior::Number(3)),
            (KeyCode::Char('4'), KeyBehavior::Number(4)),
            (KeyCode::Char('5'), KeyBehavior::Number(5)),
            (KeyCode::Char('6'), KeyBehavior::Number(6)),
            (KeyCode::Char('7'), KeyBehavior::Number(7)),
            (KeyCode::Char('8'), KeyBehavior::Number(8)),
            (KeyCode::Char('9'), KeyBehavior::Number(9)),
        ],
        KeyModifiers::SHIFT => [
            (KeyCode::Char('G'), KeyBehavior::Down(ScrollSize::End)),
            (KeyCode::Char('N'), KeyBehavior::SearchPrev),
            (KeyCode::Char('Q'), KeyBehavior::Quit),
        ],
        KeyModifiers::CONTROL => [
            (KeyCode::Char('u'), KeyBehavior::Up(ScrollSize::HalfPage)),
            (KeyCode::Char('d'), KeyBehavior::Down(ScrollSize::HalfPage)),
            (KeyCode::Char('f'), KeyBehavior::Down(ScrollSize::Page)),
            (KeyCode::Char('v'), KeyBehavior::Down(ScrollSize::Page)),
            (KeyCode::Char('b'), KeyBehavior::Up(ScrollSize::Page)),

            (KeyCode::Char('e'), KeyBehavior::Down(ScrollSize::One)),
            (KeyCode::Char('n'), KeyBehavior::Down(ScrollSize::One)),

            (KeyCode::Char('y'), KeyBehavior::Up(ScrollSize::One)),
            (KeyCode::Char('k'), KeyBehavior::Up(ScrollSize::One)),
            (KeyCode::Char('p'), KeyBehavior::Up(ScrollSize::One)),

            (KeyCode::Char('d'), KeyBehavior::Quit),
            (KeyCode::Char('c'), KeyBehavior::Quit),
        ],
    }

    dict
}

pub struct UiContext<'b> {
    rx: Arc<ArrayQueue<RpLine<'b>>>,
    lines: Vec<RpLine<'b>>,
    reflowed_lines: Vec<RpLine<'b>>,
    reflowed_lines_associations: Vec<Vec<usize>>,
    search_positions: Vec<SearchPositionArr>,
    reflowed_search_positions: Vec<SearchPositionArr>,
    search_char_len: usize,
    output: File,
    output_buf: Vec<u8>,
    scroll: usize,
    size_ctx: SizeContext,
    prev_wrap: usize,
    keymap: AHashMap<KeyEvent, KeyBehavior>,
    need_redraw: bool,
    need_reflow: bool,
    prompt_outdated: bool,
    prompt_state: PromptState,
    prompt: String,
}

impl<'b> UiContext<'b> {
    pub fn new(rx: Arc<ArrayQueue<RpLine<'b>>>) -> Result<Self> {
        enable_raw_mode()?;

        let mut output = get_output();

        execute!(
            output,
            EnterAlternateScreen,
            EnableMouseCapture,
            DisableLineWrap,
            Hide
        )?;

        let mut size_ctx = SizeContext::new();
        let (x, y) = crossterm::terminal::size()?;
        size_ctx.resize(x as usize, y as usize);

        Ok(Self {
            rx,
            lines: Vec::with_capacity(1024),
            reflowed_lines: Vec::with_capacity(1024),
            reflowed_lines_associations: Vec::new(),
            scroll: 0,
            output_buf: vec![0; OUTBUF_SIZE],
            search_positions: Vec::new(),
            reflowed_search_positions: Vec::new(),
            search_char_len: 0,
            size_ctx,
            keymap: default_keymap(),
            need_redraw: true,
            need_reflow: true,
            prev_wrap: 0,
            prompt_state: PromptState::Normal,
            prompt_outdated: true,
            prompt: String::with_capacity(256),
            output,
        })
    }

    fn max_scroll(&self) -> usize {
        self.reflowed_lines
            .len()
            .saturating_sub(self.size_ctx.calculate_real_size(&self.reflowed_lines).0)
    }

    pub fn update(&mut self) -> Result<()> {
        if self.need_reflow {
            self.reflowed_lines.clear();
            self.reflowed_lines_associations.clear();
            for line in &self.lines {
                // if just line break
                if line.len() < 1 {
                    self.reflowed_lines.push(&line);
                    self.reflowed_lines_associations.push(vec!{ self.reflowed_lines.len() - 1 });
                    continue;
                }

                let mut takes: usize = 0;
                let mut line_indexes = vec!{} as Vec<usize>;
                loop {
                    let start = takes * (self.size_ctx.terminal_column() - 1);
                    let end = std::cmp::min(start + self.size_ctx.terminal_column() - 1, line.len());

                    if start < line.len() {
                        self.reflowed_lines.push(&line[start..end]);
                        line_indexes.push(self.reflowed_lines.len() - 1);
                        takes += 1;
                        continue;
                    }

                    break;
                }

                self.reflowed_lines_associations.push(line_indexes);
            }

            if !self.reflowed_search_positions.is_empty() {
                self.reflow_search();
            }

            self.need_reflow = false;
        }

        if self.need_redraw {
            #[cfg(feature = "logging")]
            log::debug!("REDRAW");

            self.output_buf.clear();

            queue!(self.output_buf, MoveTo(0, 0))?;

            let mut ch_writer = ChWriter::new(self.size_ctx.terminal_column());
            let (real, margin) = self
                .size_ctx
                .calculate_real_size(&self.reflowed_lines[self.scroll..]);
            let end = self.scroll + real;

            #[cfg(feature = "logging")]
            log::debug!("margin: {}", margin);
            for _ in 0..margin {
                queue!(
                    self.output_buf,
                    Clear(ClearType::CurrentLine),
                    MoveToNextLine(1)
                )?;
            }

            if self.reflowed_search_positions.is_empty() {
                let mut iter = self.reflowed_lines[self.scroll..end].iter();
                while let Some(line) = iter.next() {
                    queue!(self.output_buf, Clear(ClearType::CurrentLine))?;
                    ch_writer.write_slice(&mut self.output_buf, line)?;
                    ch_writer.pos = 0;
                    queue!(self.output_buf, MoveToNextLine(1))?;
                }
            } else {
                let mut iter = self.reflowed_lines[self.scroll..end]
                    .iter()
                    .zip(self.reflowed_search_positions[self.scroll..end].iter());
                
                let mut overflow = 0 as usize;
                while let Some((line, search)) = iter.next() {
                    queue!(self.output_buf, Clear(ClearType::CurrentLine))?;
                    
                    let mut prev_pos = 0;
                    
                    if overflow > 0 {
                        ch_writer.write_slice_reverse(&mut self.output_buf, &line[0..overflow])?;
                        prev_pos = overflow;
                        overflow = 0;
                    }

                    for pos in search.iter() {
                        let start = pos.start as usize;
                        let mut end = start + self.search_char_len;

                        if end > line.len() {
                            overflow = end - line.len();
                            end = line.len();
                        }

                        if start > end {
                            panic!("wtf is happening");
                        }
                        
                        ch_writer.write_slice(&mut self.output_buf, &line[prev_pos..start])?;
                        ch_writer.write_slice_reverse(&mut self.output_buf, &line[start..end])?;
                        prev_pos = end;
                    }
                    ch_writer.write_slice(&mut self.output_buf, &line[prev_pos..])?;
                    ch_writer.pos = 0;
                    queue!(self.output_buf, MoveToNextLine(1))?;
                }
            }

            self.prev_wrap = ch_writer.wrap;
            queue!(self.output_buf, SetAttribute(Attribute::Reset),)?;
            self.update_prompt();
            self.write_prompt()?;
            #[cfg(feature = "logging")]
            log::trace!("Write {} bytes", self.output_buf.len());
            self.output.write(&self.output_buf)?;
            self.output.flush()?;
            self.need_redraw = false;
        } else if self.prompt_outdated {
            self.update_prompt();
            self.redraw_prompt()?;
        }

        Ok(())
    }

    fn write_prompt(&mut self) -> Result<()> {
        let lines = self.size_ctx.terminal_line();
        queue!(
            self.output_buf,
            MoveTo(0, lines as _),
            Clear(ClearType::CurrentLine)
        )?;
        self.output_buf.extend_from_slice(self.prompt.as_bytes());
        Ok(())
    }

    fn redraw_prompt(&mut self) -> Result<()> {
        self.output_buf.clear();
        self.write_prompt()?;
        self.output.write_all(&self.output_buf)?;
        self.output.flush()?;

        Ok(())
    }

    pub fn push_line(&mut self, line: RpLine<'b>) {
        if self.lines.len() < self.size_ctx.terminal_line() {
            self.need_redraw = true;
        }
        self.prompt_outdated = true;
        self.lines.push(line);
    }

    fn update_prompt(&mut self) {
        if self.prompt_outdated {
            use std::fmt::Write;
            self.prompt.clear();

            match self.prompt_state {
                PromptState::Normal => {
                    write!(
                        self.prompt,
                        "{}lines {}-{}/{}",
                        SetAttribute(Attribute::Reverse),
                        self.scroll + 1,
                        (self.scroll + self.size_ctx.terminal_line() - self.prev_wrap),
                        self.reflowed_lines.len(),
                    )
                    .ok();

                    if self.scroll == self.max_scroll() {
                        self.prompt.push_str(" (END)");
                    }

                    write!(self.prompt, "{}", SetAttribute(Attribute::Reset),).ok();
                }
                PromptState::Number(n) => {
                    write!(self.prompt, ":{}", n).ok();
                }
                PromptState::Search(ref s) => {
                    write!(
                        self.prompt,
                        "{}/{}{}",
                        SetAttribute(Attribute::Reverse),
                        s,
                        SetAttribute(Attribute::Reset),
                    )
                    .ok();
                }
            }

            self.prompt_outdated = false;
        }
    }

    fn goto_scroll(&mut self, idx: usize) {
        let new_scroll = idx.min(self.max_scroll());
        if new_scroll != self.scroll {
            self.scroll = new_scroll;
            self.need_redraw = true;
            self.prompt_outdated = true;
        }
    }

    fn scroll_down(&mut self, idx: usize) {
        self.goto_scroll(self.scroll.saturating_add(idx));
    }

    fn scroll_up(&mut self, idx: usize) {
        self.goto_scroll(self.scroll.saturating_sub(idx));
    }

    fn move_search(&mut self, forward: bool) {
        let next = self.reflowed_search_positions[self.scroll..]
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, p)| (i + self.scroll, p));

        let prev = self.reflowed_search_positions[0..self.scroll].iter().enumerate();

        let line = if forward {
            next.chain(prev)
                .find_map(|(line, p)| if !p.is_empty() { Some(line) } else { None })
        } else {
            prev.rev()
                .chain(next.rev())
                .find_map(|(line, p)| if !p.is_empty() { Some(line) } else { None })
        };

        if let Some(line) = line {
            self.goto_scroll(line);
        }
    }

    fn search(&mut self, needle: &str) {
        if !self.search_positions.is_empty() {
            self.need_redraw = true;
            self.search_positions.clear();
        }

        let char_count = needle.chars().count();
        self.search_char_len = char_count;

        if char_count == 0 {
            return;
        }

        #[cfg(feature = "logging")]
        log::debug!("Search: {:?}", needle);

        self.need_redraw = true;

        self.lines
            .par_iter()
            .map(|chars| {
                let mut arr = SearchPositionArr::new();

                for i in 0..chars.len() {
                    if chars[i..]
                        .iter()
                        .take(char_count)
                        .map(|c| c.ch)
                        .eq(needle.chars())
                    {
                        arr.push(SearchPosition { start: i as u32 });
                    }
                }

                arr
            })
            .collect_into_vec(&mut self.search_positions);
        
        // remove duplicate matches
        for search_positions in &mut self.search_positions {
            let mut indexes_to_remove = vec!{} as Vec<usize>;
            let mut previous_start = None as Option<u32>;
            for (index, search_position) in search_positions.iter().enumerate() {
                match previous_start {
                    Some(start) => {
                        // if the start of this match is still part of previous match, set to remove
                        if (start + self.search_char_len as u32) > search_position.start {
                            indexes_to_remove.push(index);
                        } else {                        
                            previous_start = Some(search_position.start);
                        }
                    },
                    None => {
                        previous_start = Some(search_position.start);
                        continue
                    }
                }
            }

            // remove index from behind because removing items reindexes the vec
            for index in indexes_to_remove.iter().rev() {
                search_positions.remove(*index);
            }
        }

        self.reflow_search();

        self.move_search(true);
    }

    // convert self.search_positions' indexes to match reflowed lines
    fn reflow_search(&mut self) {
        // clear, then initialize with same length as reflowed lines'
        self.reflowed_search_positions.clear();
        self.reflowed_search_positions.resize(self.reflowed_lines.len(), SmallVec::new());
        
        // iterate current self.search_positions
        for (index, search_positions) in self.search_positions.iter().enumerate() {
            // get reflowed lines' indexes (original_line_index => [reflowed_line_index1, reflowed_line_index2, ...])
            let linked_reflowed_lines = self.reflowed_lines_associations.get(index);
            if linked_reflowed_lines.is_none() { continue }
            let linked_reflowed_lines = linked_reflowed_lines.unwrap();

            // take note of positions and their new reflowed line indexes
            let mut new_positions = vec!{} as Vec<Vec<usize>>;
            new_positions.resize(linked_reflowed_lines.len(), vec!{});
            for position in search_positions {
                let cut_index = position.start as usize / (self.size_ctx.terminal_column() - 1);
                let index_in_cut = position.start as usize % (self.size_ctx.terminal_column() - 1);
                new_positions[cut_index].push(index_in_cut);
            }

            // push into self.reflowed_search_positions
            for (cut_index, start_indexes) in new_positions.iter().enumerate() {
                let mut indexes = SmallVec::new();
                for start_index in start_indexes { indexes.push(SearchPosition { start: *start_index as u32 }) }
                self.reflowed_search_positions[linked_reflowed_lines[cut_index]] = indexes;
            }
        };
    }

    pub fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..
            }) => {
                if self.prompt_state == PromptState::Normal {
                    self.scroll_up(1);
                }
            }
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..
            }) => {
                if self.prompt_state == PromptState::Normal {
                    self.scroll_down(1);
                }
            }
            Event::Key(ke) => {
                if let PromptState::Search(ref mut s) = self.prompt_state {
                    if !ke
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        match ke.code {
                            KeyCode::Char(c) => {
                                s.push(c);
                                self.prompt_outdated = true;
                                return Ok(false);
                            }
                            KeyCode::Backspace => {
                                if s.pop().is_none() {
                                    self.prompt_state = PromptState::Normal;
                                }

                                self.prompt_outdated = true;
                                return Ok(false);
                            }
                            KeyCode::Enter => {
                                let needle = std::mem::take(s);
                                self.search(&needle);
                                self.prompt_state = PromptState::Normal;
                                self.prompt_outdated = true;
                                return Ok(false);
                            }
                            _ => {}
                        }
                    }
                }

                match self.keymap.get(&ke) {
                    Some(b) => match b {
                        KeyBehavior::NormalMode => {
                            self.prompt_state.take();
                            self.search("");
                            self.prompt_outdated = true;
                        }
                        KeyBehavior::Search => {
                            self.prompt_state = PromptState::Search(String::new());
                            self.prompt_outdated = true;
                        }
                        KeyBehavior::SearchNext => {
                            self.move_search(true);
                        }
                        KeyBehavior::SearchPrev => {
                            self.move_search(false);
                        }
                        KeyBehavior::Number(n) => match self.prompt_state {
                            PromptState::Number(ref mut pn) => {
                                *pn = *pn * 10 + (*n as usize);
                                self.prompt_outdated = true;
                            }
                            _ => {
                                self.prompt_state = PromptState::Number(*n as usize);
                                self.prompt_outdated = true;
                            }
                        },
                        KeyBehavior::Up(size) => {
                            let size = size.calculate(self.size_ctx.terminal_line());
                            let n = match self.prompt_state.take() {
                                PromptState::Number(n) => n,
                                _ => 1,
                            };
                            self.scroll_up(size.wrapping_mul(n));
                        }
                        KeyBehavior::Down(size) => {
                            let size = size.calculate(self.size_ctx.terminal_line());
                            let n = match self.prompt_state.take() {
                                PromptState::Number(n) => n,
                                _ => 1,
                            };
                            self.scroll_down(size.wrapping_mul(n));
                        }
                        KeyBehavior::Quit => {
                            return Ok(true);
                        }
                    },
                    None => {}
                }
            }
            Event::Resize(x, y) => {
                self.size_ctx.resize(x as usize, y as usize);
                self.need_reflow = true;
                self.need_redraw = true;
                self.prompt_outdated = true;
            }
            _ => {}
        };

        Ok(false)
    }

    pub fn run(&mut self) -> Result<()> {
        const BULK_LINE: usize = 5000;
        const FPS: u64 = 30;
        const TICK: Duration = Duration::from_nanos(Duration::from_secs(1).as_nanos() as u64 / FPS);

        let mut prev_time = Instant::now();

        loop {
            if !crate::RUN.load(Ordering::Acquire) {
                return Ok(());
            }

            // non blocking
            while poll(Duration::from_nanos(0))? {
                let e = read()?;

                if self.handle_event(e)? {
                    return Ok(());
                }
            }

            let mut line_count = 0;

            // receive lines max BULK_LINE
            while let Some(line) = self.rx.pop() {
                self.push_line(line);

                line_count += 1;

                if line_count >= BULK_LINE {
                    break;
                }
            }

            self.update()?;

            if let Some(sleep) = TICK.checked_sub(prev_time.elapsed()) {
                std::thread::sleep(sleep);
            }

            prev_time = Instant::now();
        }
    }
}

impl<'b> Drop for UiContext<'b> {
    fn drop(&mut self) {
        execute!(
            self.output,
            Show,
            EnableLineWrap,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
        .ok();
        disable_raw_mode().ok();
    }
}

struct ChWriter {
    terminal_column: usize,
    wrap: usize,
    pos: usize,
    current_color: Color,
    current_bgcolor: Color,
    current_attribute: Attributes,
}

impl ChWriter {
    pub fn new(terminal_column: usize) -> Self {
        Self {
            terminal_column,
            wrap: 0,
            pos: 0,
            current_color: Color::Reset,
            current_bgcolor: Color::Reset,
            current_attribute: Attributes::default(),
        }
    }

    pub fn write_slice_reverse(&mut self, out: &mut Vec<u8>, chars: &[RpChar]) -> Result<()> {
        chars.iter().copied().try_for_each(|mut ch| {
            ch.attribute.set(Attribute::Reverse);
            self.write(out, ch)
        })?;
        self.current_attribute.unset(Attribute::Reverse);
        queue!(out, SetAttribute(Attribute::NoReverse))
    }

    pub fn write_slice(&mut self, out: &mut Vec<u8>, chars: &[RpChar]) -> Result<()> {
        chars.iter().copied().try_for_each(|ch| self.write(out, ch))
    }

    pub fn write(&mut self, out: &mut Vec<u8>, ch: RpChar) -> Result<()> {
        if self.current_attribute != ch.attribute {
            queue!(out, SetAttributes(ch.attribute))?;
            // Reset attribute also reset colors
            if ch.attribute.has(Attribute::Reset) {
                self.current_color = Color::Reset;
                self.current_bgcolor = Color::Reset;
            }
            self.current_attribute = ch.attribute;
        }
        if ch.foreground != self.current_color {
            queue!(out, SetForegroundColor(ch.foreground))?;
            self.current_color = ch.foreground;
        }
        if ch.background != self.current_bgcolor {
            queue!(out, SetBackgroundColor(ch.background))?;
            self.current_bgcolor = ch.background;
        }

        let width = ch.ch.width().unwrap_or(0);

        if self.pos + width > self.terminal_column {
            queue!(out, MoveToNextLine(1), Clear(ClearType::CurrentLine))?;
            self.wrap += 1;
            self.pos = width;
        } else {
            self.pos += width;
        }

        write!(out, "{}", ch.ch)?;

        Ok(())
    }
}

#[derive(Default, Clone)]
struct SizeContext {
    terminal_column: usize,
    terminal_line: usize,
}

impl SizeContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calculate_real_size(&self, lines: &[RpLine]) -> (usize, usize) {
        let mut real = 0;
        let mut left = self.terminal_line;
        for line in lines.iter().rev() {
            let size = line_line_size(line, self.terminal_column);
            match left.checked_sub(size) {
                Some(n) => {
                    real += 1;
                    left = n;
                }
                None => {
                    break;
                }
            }
        }
        (real, left)
    }

    pub fn resize(&mut self, terminal_column: usize, terminal_line: usize) {
        self.terminal_column = terminal_column;
        self.terminal_line = {
            // reduce by one on unix, keep on windows
            #[cfg(unix)]
            { terminal_line - 1 }
            #[cfg(windows)]
            { terminal_line }
        };
    }

    pub fn terminal_column(&self) -> usize {
        self.terminal_column
    }

    pub fn terminal_line(&self) -> usize {
        self.terminal_line
    }
}

fn line_line_size(l: RpLine, column: usize) -> usize {
    let width = line_width(l);

    if width == 0 {
        1
    } else if width % column == 0 {
        width / column
    } else {
        (width / column) + 1
    }
}

fn line_width(l: RpLine) -> usize {
    l.iter().map(|c| c.ch.width().unwrap_or(0)).sum::<usize>()
}
