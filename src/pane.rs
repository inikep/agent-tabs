use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, TermMode};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::style::Style;

use crate::config::Harness;
use crate::keys::bracketed_paste;
use crate::render::cell_style;

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub const OUTER_TERMINAL_ENV: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "STY",
    "ZELLIJ",
    "ZELLIJ_PANE_ID",
    "ZELLIJ_SESSION_NAME",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_LISTEN_ON",
    "WEZTERM_PANE",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_UNIX_SOCKET",
    "ALACRITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "GHOSTTY_RESOURCES_DIR",
    "VTE_VERSION",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "VSCODE_INJECTION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows.max(1) as usize
    }

    fn columns(&self) -> usize {
        self.cols.max(1) as usize
    }
}

#[derive(Default)]
struct SharedState {
    title: Option<String>,
    window: Option<WindowSize>,
}

#[derive(Clone)]
pub struct Listener {
    writer: Option<SharedWriter>,
    state: Arc<Mutex<SharedState>>,
}

impl Listener {
    fn reply(&self, text: &str) {
        if let Some(writer) = &self.writer
            && let Ok(mut w) = writer.lock()
        {
            let _ = w.write_all(text.as_bytes());
            let _ = w.flush();
        }
    }
}

pub fn xterm_color(index: usize) -> Rgb {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    let (r, g, b) = match index {
        0..=15 => BASE[index],
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            (v, v, v)
        }
        257 => (0x00, 0x00, 0x00),
        _ => (0xe5, 0xe5, 0xe5),
    };
    Rgb { r, g, b }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.reply(&text),
            Event::TextAreaSizeRequest(format) => {
                let window = self.state.lock().ok().and_then(|s| s.window);
                if let Some(window) = window {
                    self.reply(&format(window));
                }
            }
            Event::ColorRequest(index, format) => self.reply(&format(xterm_color(index))),
            Event::Title(title) => {
                if let Ok(mut s) = self.state.lock() {
                    s.title = Some(title);
                }
            }
            Event::ResetTitle => {
                if let Ok(mut s) = self.state.lock() {
                    s.title = None;
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// A disabled harness that has not been started yet.
    Stopped,
    Running,
    Exited(Option<u32>),
    Failed(String),
}

pub struct Pane {
    pub harness: Harness,
    pub term: Arc<Mutex<Term<Listener>>>,
    state: Arc<Mutex<SharedState>>,
    writer: Option<SharedWriter>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Arc<Mutex<Box<dyn Child + Send + Sync>>>>,
    exited: Arc<AtomicBool>,
    last_output: Arc<Mutex<Option<Instant>>>,
    failure: Option<String>,
    started: bool,
    /// The mouse selection, kept here as well as in the terminal: the terminal
    /// drops a selection whenever the harness erases a line it touches, which
    /// Ink harnesses (cursor-agent) do on every redraw.
    held: Mutex<Option<(GridPoint, GridPoint)>>,
    size: Size,
    pub introduced: bool,
}

/// A cell counted from the oldest line of scrollback, so it stays on the same
/// text while new output scrolls the screen.
type GridPoint = (usize, Column);

impl Pane {
    pub fn spawn(
        harness: Harness,
        cwd: &std::path::Path,
        size: Size,
        scrollback: usize,
        dirty: Arc<AtomicBool>,
        log: Option<Box<dyn Write + Send>>,
    ) -> Self {
        let state = Arc::new(Mutex::new(SharedState::default()));
        match Self::start(&harness, cwd, size, scrollback, state.clone(), dirty, log) {
            Ok(started) => Self {
                harness,
                term: started.term,
                state,
                writer: Some(started.writer),
                master: Some(started.master),
                child: Some(started.child),
                exited: started.exited,
                last_output: started.last_output,
                failure: None,
                started: true,
                held: Mutex::new(None),
                size,
                introduced: false,
            },
            Err(err) => {
                let mut pane = Self::stopped(harness, size, scrollback);
                pane.failure = Some(format!("{err:#}"));
                pane.started = true;
                pane
            }
        }
    }

    /// A tab for `harness` with no process behind it.
    pub fn stopped(harness: Harness, size: Size, scrollback: usize) -> Self {
        let state = Arc::new(Mutex::new(SharedState::default()));
        let listener = Listener {
            writer: None,
            state: state.clone(),
        };
        let config = TermConfig {
            scrolling_history: scrollback,
            ..TermConfig::default()
        };
        let term = Term::new(config, &size, listener);
        Self {
            harness,
            term: Arc::new(Mutex::new(term)),
            state,
            writer: None,
            master: None,
            child: None,
            exited: Arc::new(AtomicBool::new(true)),
            last_output: Arc::new(Mutex::new(None)),
            failure: None,
            started: false,
            held: Mutex::new(None),
            size,
            introduced: false,
        }
    }

    fn start(
        harness: &Harness,
        cwd: &std::path::Path,
        size: Size,
        scrollback: usize,
        state: Arc<Mutex<SharedState>>,
        dirty: Arc<AtomicBool>,
        mut log: Option<Box<dyn Write + Send>>,
    ) -> Result<Started> {
        let pty = native_pty_system()
            .openpty(pty_size(size))
            .context("opening pseudo-terminal")?;
        let mut cmd = CommandBuilder::new(&harness.command);
        cmd.args(&harness.args);
        cmd.cwd(cwd);
        for key in OUTER_TERMINAL_ENV
            .iter()
            .copied()
            .chain(harness.env_remove.iter().map(String::as_str))
        {
            cmd.env_remove(key);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "agent-tabs");
        for (key, value) in &harness.env {
            cmd.env(key, value);
        }
        let child = pty
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting `{}`", harness.command))?;
        drop(pty.slave);

        let writer: SharedWriter = Arc::new(Mutex::new(
            pty.master.take_writer().context("opening PTY writer")?,
        ));
        let mut reader = pty
            .master
            .try_clone_reader()
            .context("opening PTY reader")?;
        if let Ok(mut s) = state.lock() {
            s.window = Some(window_size(size));
        }
        let listener = Listener {
            writer: Some(writer.clone()),
            state,
        };
        let config = TermConfig {
            scrolling_history: scrollback,
            ..TermConfig::default()
        };
        let term = Arc::new(Mutex::new(Term::new(config, &size, listener)));
        let exited = Arc::new(AtomicBool::new(false));
        let last_output = Arc::new(Mutex::new(None));

        let reader_term = term.clone();
        let reader_exited = exited.clone();
        let reader_last = last_output.clone();
        thread::Builder::new()
            .name(format!("pty-{}", harness.name))
            .spawn(move || {
                let mut parser: Processor = Processor::new();
                let mut buf = [0u8; 65536];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Some(log) = log.as_mut() {
                                let _ = log.write_all(&buf[..n]);
                            }
                            if let Ok(mut term) = reader_term.lock() {
                                parser.advance(&mut *term, &buf[..n]);
                            }
                            if let Ok(mut last) = reader_last.lock() {
                                *last = Some(Instant::now());
                            }
                            dirty.store(true, Ordering::Release);
                        }
                    }
                }
                if let Some(log) = log.as_mut() {
                    let _ = log.flush();
                }
                reader_exited.store(true, Ordering::Release);
                dirty.store(true, Ordering::Release);
            })
            .context("starting PTY reader thread")?;

        Ok(Started {
            term,
            writer,
            master: pty.master,
            child: Arc::new(Mutex::new(child)),
            exited,
            last_output,
        })
    }

    pub fn name(&self) -> &str {
        &self.harness.name
    }

    pub fn status(&self) -> Status {
        if let Some(failure) = &self.failure {
            return Status::Failed(failure.clone());
        }
        if !self.started {
            return Status::Stopped;
        }
        if !self.exited.load(Ordering::Acquire) {
            return Status::Running;
        }
        let code = self
            .child
            .as_ref()
            .and_then(|c| c.lock().ok()?.try_wait().ok().flatten())
            .map(|status| status.exit_code());
        Status::Exited(code)
    }

    pub fn last_output(&self) -> Option<Instant> {
        self.last_output.lock().ok().and_then(|last| *last)
    }

    pub fn is_busy(&self, window: Duration) -> bool {
        self.last_output
            .lock()
            .ok()
            .and_then(|last| *last)
            .is_some_and(|t| t.elapsed() < window)
    }

    pub fn title(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.title.clone())
    }

    pub fn mode(&self) -> TermMode {
        self.term
            .lock()
            .map(|t| *t.mode())
            .unwrap_or(TermMode::NONE)
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Some(writer) = &self.writer
            && let Ok(mut w) = writer.lock()
        {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn screen_tail(&self, lines: usize) -> String {
        let text = self.screen_text();
        let all: Vec<&str> = text.lines().map(str::trim_end).collect();
        let end = all
            .iter()
            .rposition(|l| !l.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        all[end.saturating_sub(lines)..end].join("\n")
    }

    pub fn screen_text(&self) -> String {
        let Ok(term) = self.term.lock() else {
            return String::new();
        };
        let content = term.renderable_content();
        let mut text = String::new();
        let mut last_line = None;
        for cell in content.display_iter {
            if last_line.is_some_and(|line| line != cell.point.line) {
                text.push('\n');
            }
            last_line = Some(cell.point.line);
            text.push(cell.c);
        }
        text
    }

    fn point_at(term: &Term<Listener>, col: u16, row: u16) -> Point {
        let offset = term.grid().display_offset() as i32;
        let row = (row as usize).min(term.screen_lines().saturating_sub(1)) as i32;
        let col = (col as usize).min(term.columns().saturating_sub(1));
        Point::new(Line(row - offset), Column(col))
    }

    fn grid_point_at(term: &Term<Listener>, col: u16, row: u16) -> GridPoint {
        let point = Self::point_at(term, col, row);
        let history = term.grid().history_size() as i32;
        ((point.line.0 + history).max(0) as usize, point.column)
    }

    fn apply_selection(term: &mut Term<Listener>, (anchor, head): (GridPoint, GridPoint)) {
        let history = term.grid().history_size() as i32;
        let last = term.screen_lines() as i32 - 1;
        let point = |(line, column): GridPoint| {
            Point::new(Line((line as i32 - history).clamp(-history, last)), column)
        };
        let mut selection = Selection::new(SelectionType::Simple, point(anchor), Side::Left);
        selection.update(point(head), Side::Right);
        term.selection = Some(selection);
    }

    pub fn start_selection(&self, col: u16, row: u16) {
        self.set_selection(col, row, true);
    }

    pub fn update_selection(&self, col: u16, row: u16) {
        self.set_selection(col, row, false);
    }

    fn set_selection(&self, col: u16, row: u16, start: bool) {
        let (Ok(mut term), Ok(mut held)) = (self.term.lock(), self.held.lock()) else {
            return;
        };
        let point = Self::grid_point_at(&term, col, row);
        let selection = match *held {
            Some((anchor, _)) if !start => (anchor, point),
            _ => (point, point),
        };
        *held = Some(selection);
        Self::apply_selection(&mut term, selection);
    }

    pub fn clear_selection(&self) {
        if let Ok(mut held) = self.held.lock() {
            *held = None;
        }
        if let Ok(mut term) = self.term.lock() {
            term.selection = None;
        }
    }

    /// Puts the mouse selection back into the terminal if a redraw dropped it.
    pub fn restore_selection(&self) {
        let (Ok(mut term), Ok(held)) = (self.term.lock(), self.held.lock()) else {
            return;
        };
        if let Some(selection) = *held {
            Self::apply_selection(&mut term, selection);
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.restore_selection();
        let term = self.term.lock().ok()?;
        term.selection_to_string()
            .filter(|text| !text.trim().is_empty())
    }

    pub fn history_mark(&self) -> usize {
        self.term
            .lock()
            .map(|t| t.grid().history_size())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn text_since(&self, mark: usize, max_lines: usize) -> String {
        self.styled_since(mark, max_lines)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The lines since `mark`, each with the style of every char in it, with
    /// runs of blank lines collapsed and at most `max_lines` kept.
    pub fn styled_since(&self, mark: usize, max_lines: usize) -> Vec<(String, Vec<Style>)> {
        let Ok(term) = self.term.lock() else {
            return Vec::new();
        };
        let colors = term.colors();
        let grid = term.grid();
        let history = grid.history_size();
        let back = history.saturating_sub(mark).min(history) as i32;
        let columns = grid.columns();
        let mut lines: Vec<(String, Vec<Style>)> = Vec::new();
        for line in -back..grid.screen_lines() as i32 {
            let row = &grid[Line(line)];
            let mut text = String::new();
            let mut styles = Vec::new();
            for col in 0..columns {
                let cell = &row[Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let style = cell_style(cell.fg, cell.bg, cell.flags, colors);
                text.push(cell.c);
                styles.push(style);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra);
                    styles.extend(extra.iter().map(|_| style));
                }
            }
            let trimmed = text.trim_end().to_string();
            styles.truncate(trimmed.chars().count());
            lines.push((trimmed, styles));
        }
        let mut compact: Vec<(String, Vec<Style>)> = Vec::new();
        for line in lines {
            if line.0.is_empty() && compact.last().is_some_and(|l| l.0.is_empty()) {
                continue;
            }
            compact.push(line);
        }
        while compact.last().is_some_and(|l| l.0.is_empty()) {
            compact.pop();
        }
        while compact.first().is_some_and(|l| l.0.is_empty()) {
            compact.remove(0);
        }
        let skip = compact.len().saturating_sub(max_lines);
        compact.split_off(skip)
    }

    pub fn ready_for_prompt(&self) -> bool {
        matches!(self.status(), Status::Running) && self.mode().contains(TermMode::BRACKETED_PASTE)
    }

    pub fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.write(&bracketed_paste(text, bracketed));
    }

    pub fn send_prompt(&self, text: &str, submit_delay: Duration) {
        self.scroll(Scroll::Bottom);
        self.paste(&ending_outside_a_mention(text));
        let Some(writer) = self.writer.clone() else {
            return;
        };
        thread::spawn(move || {
            thread::sleep(submit_delay);
            if let Ok(mut w) = writer.lock() {
                let _ = w.write_all(b"\r");
                let _ = w.flush();
            }
        });
    }

    pub fn scroll(&self, scroll: Scroll) {
        if let Ok(mut term) = self.term.lock() {
            term.scroll_display(scroll);
        }
    }

    pub fn display_offset(&self) -> usize {
        self.term
            .lock()
            .map(|t| t.grid().display_offset())
            .unwrap_or(0)
    }

    pub fn resize(&mut self, size: Size) {
        if size == self.size {
            return;
        }
        self.size = size;
        if let Some(master) = &self.master {
            let _ = master.resize(pty_size(size));
        }
        if let Ok(mut s) = self.state.lock() {
            s.window = Some(window_size(size));
        }
        if let Ok(mut term) = self.term.lock() {
            term.resize(size);
        }
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn signal_group_if_leader_alive(&self, signal: i32) -> bool {
        let Some(child) = &self.child else {
            return false;
        };
        let Ok(mut child) = child.lock() else {
            return false;
        };
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        let Some(pgid) = child
            .process_id()
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|&pid| pid > 0)
        else {
            return false;
        };
        unsafe {
            libc::killpg(pgid, signal);
        }
        true
    }

    pub fn hang_up(&self) {
        self.signal_group_if_leader_alive(libc::SIGHUP);
    }

    pub fn has_exited(&self) -> bool {
        match &self.child {
            Some(child) => child
                .lock()
                .map(|mut c| !matches!(c.try_wait(), Ok(None)))
                .unwrap_or(true),
            None => true,
        }
    }

    pub fn kill(&mut self) {
        if self.signal_group_if_leader_alive(libc::SIGKILL)
            && let Some(child) = &self.child
            && let Ok(mut c) = child.lock()
        {
            let _ = c.wait();
        }
    }
}

pub fn shutdown(panes: &mut [Pane], grace: Duration) {
    for pane in panes.iter() {
        pane.hang_up();
    }
    let start = Instant::now();
    while start.elapsed() < grace && !panes.iter().all(Pane::has_exited) {
        thread::sleep(Duration::from_millis(25));
    }
    for pane in panes.iter_mut() {
        pane.kill();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.kill();
    }
}

struct Started {
    term: Arc<Mutex<Term<Listener>>>,
    writer: SharedWriter,
    master: Box<dyn MasterPty + Send>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    exited: Arc<AtomicBool>,
    last_output: Arc<Mutex<Option<Instant>>>,
}

/// `text`, plus a space if its last word starts with `$` or `@`. With the
/// cursor in such a word, codex shows a skill or file popup and its Enter
/// picks from the popup instead of sending the prompt; claude's `@` file
/// popup does the same.
fn ending_outside_a_mention(text: &str) -> std::borrow::Cow<'_, str> {
    let last_word = text.rsplit(char::is_whitespace).next().unwrap_or("");
    if last_word.starts_with(['$', '@']) {
        format!("{text} ").into()
    } else {
        text.into()
    }
}

fn pty_size(size: Size) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn window_size(size: Size) -> WindowSize {
    WindowSize {
        num_lines: size.rows.max(1),
        num_cols: size.cols.max(1),
        cell_width: 8,
        cell_height: 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn shell(script: &str) -> Harness {
        Harness {
            name: "sh".into(),
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            ..Harness::default()
        }
    }

    fn screen_text(pane: &Pane) -> String {
        let term = pane.term.lock().unwrap();
        let content = term.renderable_content();
        let mut lines = vec![String::new(); term.screen_lines()];
        for cell in content.display_iter {
            let row = (cell.point.line.0 + content.display_offset as i32) as usize;
            if row < lines.len() {
                lines[row].push(cell.c);
            }
        }
        lines
            .iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_for(pane: &Pane, mut done: impl FnMut(&Pane) -> bool) {
        let start = Instant::now();
        while !done(pane) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out; screen:\n{}",
                screen_text(pane)
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn spawn(script: &str) -> Pane {
        Pane::spawn(
            shell(script),
            std::path::Path::new("."),
            Size { cols: 40, rows: 10 },
            100,
            Arc::new(AtomicBool::new(false)),
            None,
        )
    }

    #[test]
    fn renders_child_output_and_reports_exit() {
        let pane = spawn("printf 'hello\\r\\n\\033[1mworld\\033[0m'; exit 3");
        wait_for(&pane, |p| matches!(p.status(), Status::Exited(_)));
        let text = screen_text(&pane);
        assert!(text.starts_with("hello\nworld"), "{text}");
        assert_eq!(pane.status(), Status::Exited(Some(3)));
    }

    #[test]
    fn answers_cursor_position_queries() {
        let pane = spawn(
            "stty raw -echo; printf 'ab\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -c | tr -d ' \\n'; printf '\\r\\n'; sleep 0.2",
        );
        wait_for(&pane, |p| screen_text(p).contains("[1;3R"));
    }

    #[test]
    fn a_prompt_never_ends_inside_a_mention() {
        assert_eq!(
            ending_outside_a_mention("Read $PROMPT_DIR/review.md"),
            "Read $PROMPT_DIR/review.md "
        );
        assert_eq!(
            ending_outside_a_mention("look at\n@src/app.rs"),
            "look at\n@src/app.rs "
        );
        assert_eq!(ending_outside_a_mention("cost: 5$"), "cost: 5$");
        assert_eq!(
            ending_outside_a_mention("Read $X.md\nthen stop"),
            "Read $X.md\nthen stop"
        );
        assert_eq!(ending_outside_a_mention("$X "), "$X ");
    }

    #[test]
    fn send_prompt_types_text_then_submits() {
        let pane = spawn("read line; printf 'got:%s\\r\\n' \"$line\"; sleep 0.5");
        thread::sleep(Duration::from_millis(200));
        pane.send_prompt("ping", Duration::from_millis(50));
        wait_for(&pane, |p| screen_text(p).contains("got:ping"));
    }

    #[test]
    fn resize_updates_child_size() {
        let mut pane = spawn("sleep 0.3; stty size; sleep 0.5");
        pane.resize(Size { cols: 70, rows: 12 });
        wait_for(&pane, |p| screen_text(p).contains("12 70"));
    }

    #[test]
    fn missing_command_is_reported() {
        let mut harness = shell("");
        harness.command = "definitely-not-a-real-command-xyz".into();
        let pane = Pane::spawn(
            harness,
            std::path::Path::new("."),
            Size { cols: 40, rows: 10 },
            100,
            Arc::new(AtomicBool::new(false)),
            None,
        );
        assert!(matches!(
            pane.status(),
            Status::Failed(_) | Status::Exited(_)
        ));
    }

    #[test]
    fn outer_terminal_variables_are_hidden() {
        let pane = Pane::spawn(
            Harness {
                name: "sh".into(),
                command: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'tmux=[%s] prog=[%s] term=[%s]' \"$TMUX\" \"$TERM_PROGRAM\" \"$TERM\""
                        .into(),
                ],
                env: BTreeMap::from([("TMUX".to_string(), "explicit".to_string())]),
                env_remove: Vec::new(),
                ..Harness::default()
            },
            std::path::Path::new("."),
            Size { cols: 80, rows: 4 },
            100,
            Arc::new(AtomicBool::new(false)),
            None,
        );
        wait_for(&pane, |p| screen_text(p).contains("term=["));
        assert!(
            screen_text(&pane).contains("tmux=[explicit] prog=[agent-tabs] term=[xterm-256color]"),
            "{}",
            screen_text(&pane)
        );
    }

    #[test]
    fn shutdown_stops_the_whole_process_group() {
        let marker =
            std::env::temp_dir().join(format!("agent-tabs-grandchild-{}", std::process::id()));
        let script = format!("sleep 300 & echo $! > {}; wait", marker.display());
        let mut panes = vec![spawn(&script)];
        let start = Instant::now();
        let grandchild = loop {
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "grandchild did not start"
            );
            thread::sleep(Duration::from_millis(20));
        };
        shutdown(&mut panes, Duration::from_secs(2));
        let deadline = Instant::now();
        while unsafe { libc::kill(grandchild, 0) } == 0 {
            assert!(
                deadline.elapsed() < Duration::from_secs(5),
                "grandchild {grandchild} survived shutdown"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(panes[0].has_exited());
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn text_since_mark_includes_scrolled_history() {
        let pane = spawn("seq 1 30; echo MARK; sleep 0.5; seq 100 140; sleep 2");
        wait_for(&pane, |p| screen_text(p).contains("MARK"));
        let mark = pane.history_mark();
        wait_for(&pane, |p| screen_text(p).contains("140"));
        let text = pane.text_since(mark, 1000);
        assert!(text.contains("100") && text.ends_with("140"), "{text}");
        assert!(!text.contains("\n5\n"), "{text}");
        let tail = pane.text_since(mark, 3);
        assert_eq!(tail, "138\n139\n140");
    }

    #[test]
    fn selection_returns_text_across_lines() {
        let pane = spawn("printf 'first line\\r\\nsecond line'; sleep 2");
        wait_for(&pane, |p| screen_text(p).contains("second line"));
        pane.start_selection(6, 0);
        pane.update_selection(5, 1);
        assert_eq!(pane.selection_text().unwrap(), "line\nsecond");
        pane.clear_selection();
        assert!(pane.selection_text().is_none());
    }

    #[test]
    fn xterm_palette_values() {
        assert_eq!(
            xterm_color(1),
            Rgb {
                r: 0xcd,
                g: 0,
                b: 0
            }
        );
        assert_eq!(xterm_color(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            xterm_color(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(xterm_color(232), Rgb { r: 8, g: 8, b: 8 });
    }
}
