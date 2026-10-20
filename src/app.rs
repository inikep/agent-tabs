use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::UnicodeWidthStr;

use crate::clipboard::Clipboard;
use crate::composer::{Composer, ComposerAction};
use crate::config::{Config, Harness};
use crate::keys;
use crate::pane::{Pane, Size, Status};
use crate::render::draw_term;
use crate::summary::{Capture, Round, SummaryState, run_summarizer};

const BUSY_WINDOW: Duration = Duration::from_millis(1500);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Prefix,
    Compose,
    Scroll,
    Help,
    ConfirmQuit,
    ConfirmRestart,
}

pub struct App {
    panes: Vec<Pane>,
    active: usize,
    mode: Mode,
    composer: Composer,
    summary_input: Composer,
    config: Config,
    cwd: PathBuf,
    log_dir: Option<PathBuf>,
    dirty: Arc<AtomicBool>,
    pane_size: Size,
    tab_spans: Vec<(u16, u16)>,
    message: Option<(String, Instant)>,
    pending: Vec<(usize, String)>,
    dialogs: regex::RegexSet,
    rounds: Vec<Round>,
    summary_view: usize,
    summary_focus: usize,
    summary_folded: Vec<bool>,
    summary_hits: Vec<Option<usize>>,
    summary_area: Rect,
    summary_rows: Vec<String>,
    summary_selection: Option<((usize, u16), (usize, u16))>,
    drag: Option<Drag>,
    clipboard: Clipboard,
    inputs: regex::RegexSet,
    summary_scroll: u16,
    summary_unseen: bool,
    quit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    Pane { index: usize, moved: bool },
    Summary { anchor: (usize, u16), moved: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Ready,
    NoPaste,
    Dialog,
    NotRunning,
}

pub fn is_prefix(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    let line_style = line.style;
    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            cells.push((c, span.style));
        }
    }
    let total: usize = cells.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
    if total <= width {
        return vec![line];
    }
    let indent = cells
        .iter()
        .take_while(|(c, _)| *c == ' ')
        .count()
        .min(width / 2);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < cells.len() {
        let prefix = if first { 0 } else { indent };
        let room = width.saturating_sub(prefix).max(1);
        let mut used = 0;
        let mut end = start;
        while end < cells.len() {
            let w = cells[end].0.width().unwrap_or(0);
            if used + w > room {
                break;
            }
            used += w;
            end += 1;
        }
        if end == start {
            end = start + 1;
        }
        if end < cells.len()
            && let Some(space) = cells[start..end].iter().rposition(|(c, _)| *c == ' ')
            && space > 0
        {
            end = start + space + 1;
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        if prefix > 0 {
            spans.push(Span::raw(" ".repeat(prefix)));
        }
        let mut text = String::new();
        let mut style = cells[start].1;
        for &(c, s) in &cells[start..end] {
            if s != style && !text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut text), style));
            }
            style = s;
            text.push(c);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        rows.push(Line::from(spans).style(line_style));
        start = end;
        while !first
            && start < cells.len()
            && cells[start].0 == ' '
            && start > 0
            && cells[start - 1].0 == ' '
        {
            start += 1;
        }
        first = false;
    }
    rows
}

fn columns_of(text: &str, from: u16, to: u16) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut col: u16 = 0;
    let mut out = String::new();
    for c in text.chars() {
        let width = c.width().unwrap_or(0) as u16;
        if col >= from && col <= to {
            out.push(c);
        }
        col = col.saturating_add(width.max(if width == 0 { 0 } else { 1 }));
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl App {
    pub fn new(config: Config, cwd: PathBuf, log_dir: Option<PathBuf>, size: Size) -> Result<Self> {
        if let Some(dir) = &log_dir {
            std::fs::create_dir_all(dir)?;
        }
        let dirty = Arc::new(AtomicBool::new(true));
        let pane_size = Self::pane_size_for(size);
        let clipboard = Clipboard::new(config.osc52, config.system_clipboard);
        let mut app = Self {
            panes: Vec::new(),
            active: 0,
            mode: Mode::Normal,
            composer: Composer::new(config.harnesses.len()),
            summary_input: Composer::new(config.harnesses.len()),
            config,
            cwd,
            log_dir,
            dirty,
            pane_size,
            tab_spans: Vec::new(),
            message: None,
            pending: Vec::new(),
            dialogs: regex::RegexSet::empty(),
            rounds: Vec::new(),
            summary_view: 0,
            summary_focus: 0,
            summary_folded: Vec::new(),
            summary_hits: Vec::new(),
            summary_area: Rect::default(),
            summary_rows: Vec::new(),
            summary_selection: None,
            drag: None,
            clipboard,
            inputs: regex::RegexSet::empty(),
            summary_scroll: 0,
            summary_unseen: false,
            quit: false,
        };
        app.dialogs = app.config.dialog_regex()?;
        app.inputs = app.config.input_regex()?;
        let harnesses = app.config.harnesses.clone();
        for harness in harnesses {
            let pane = app.spawn_pane(harness);
            app.panes.push(pane);
        }
        Ok(app)
    }

    fn pane_size_for(size: Size) -> Size {
        Size {
            cols: size.cols.max(1),
            rows: size.rows.saturating_sub(2).max(1),
        }
    }

    fn spawn_pane(&self, harness: Harness) -> Pane {
        let log = self.log_dir.as_ref().and_then(|dir| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{}.log", harness.name)))
                .ok()
                .map(|f| Box::new(f) as Box<dyn Write + Send>)
        });
        Pane::spawn(
            harness,
            &self.cwd,
            self.pane_size,
            self.config.scrollback,
            self.dirty.clone(),
            log,
        )
    }

    fn log_prompt(&self, text: &str, targets: &[usize]) {
        let Some(dir) = &self.log_dir else { return };
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("prompts.log"))
        else {
            return;
        };
        let names: Vec<&str> = targets.iter().map(|&i| self.panes[i].name()).collect();
        let _ = writeln!(
            file,
            "--- {} -> {}\n{}\n",
            now_secs(),
            names.join(","),
            text
        );
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
    }

    pub fn deliver_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let delay = Duration::from_millis(self.config.submit_delay_ms);
        let mut delivered = Vec::new();
        let mut dropped = Vec::new();
        let mut waiting = Vec::new();
        for (i, text) in std::mem::take(&mut self.pending) {
            match self.readiness(i) {
                Readiness::Ready => {
                    self.mark_delivered(i);
                    self.panes[i].send_prompt(&text, delay);
                    delivered.push(self.panes[i].name().to_string());
                }
                Readiness::NotRunning => dropped.push(self.panes[i].name().to_string()),
                Readiness::NoPaste | Readiness::Dialog => waiting.push((i, text)),
            }
        }
        self.pending = waiting;
        if !delivered.is_empty() {
            self.flash(format!("sent queued prompt to {}", delivered.join(", ")));
        }
        if !dropped.is_empty() {
            self.flash(format!(
                "dropped queued prompt: {} is not running",
                dropped.join(", ")
            ));
        }
    }

    fn readiness(&self, index: usize) -> Readiness {
        let pane = &self.panes[index];
        if !matches!(pane.status(), Status::Running) {
            Readiness::NotRunning
        } else if !pane.ready_for_prompt() {
            Readiness::NoPaste
        } else if self.dialogs.is_match(&pane.screen_text()) {
            Readiness::Dialog
        } else {
            Readiness::Ready
        }
    }

    fn needs_input(&self, index: usize) -> bool {
        matches!(self.panes[index].status(), Status::Running)
            && self.inputs.is_match(&self.panes[index].screen_tail(20))
    }

    fn has_pending(&self, index: usize) -> bool {
        self.pending.iter().any(|(i, _)| *i == index)
    }

    pub fn shutdown(&mut self) {
        crate::pane::shutdown(&mut self.panes, SHUTDOWN_GRACE);
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut last_tick = Instant::now();
        let mut redraw = true;
        while !self.quit {
            if !self.pending.is_empty() {
                self.deliver_pending();
                redraw = true;
            }
            if self.tick_summary() {
                redraw = true;
            }
            if redraw || self.dirty.swap(false, Ordering::AcqRel) {
                terminal.draw(|frame| self.draw(frame))?;
                redraw = false;
            }
            if event::poll(Duration::from_millis(16))? {
                self.handle_event(event::read()?);
                while event::poll(Duration::ZERO)? {
                    self.handle_event(event::read()?);
                }
                redraw = true;
            } else if last_tick.elapsed() >= Duration::from_millis(500) {
                last_tick = Instant::now();
                redraw = true;
            }
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => self.on_paste(&text),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Resize(cols, rows) => self.resize(Size { cols, rows }),
            _ => {}
        }
    }

    pub fn resize(&mut self, size: Size) {
        self.pane_size = Self::pane_size_for(size);
        let pane_size = self.pane_size;
        for pane in &mut self.panes {
            pane.resize(pane_size);
        }
    }

    fn summary_tab(&self) -> usize {
        self.panes.len()
    }

    fn on_summary(&self) -> bool {
        self.active == self.summary_tab()
    }

    fn select(&mut self, index: usize) {
        if index <= self.summary_tab() {
            self.active = index;
            if self.on_summary() {
                self.summary_unseen = false;
            }
        }
    }

    fn cycle(&mut self, delta: isize) {
        let n = self.summary_tab() as isize + 1;
        let next = ((self.active as isize + delta).rem_euclid(n)) as usize;
        self.select(next);
    }

    fn open_composer(&mut self, all: bool) {
        let targets: Vec<usize> = if all || self.on_summary() {
            (0..self.panes.len()).collect()
        } else {
            vec![self.active]
        };
        self.composer.open(&targets);
        self.mode = Mode::Compose;
    }

    fn restart_active(&mut self) {
        let harness = self.panes[self.active].harness.clone();
        let name = harness.name.clone();
        crate::pane::shutdown(
            std::slice::from_mut(&mut self.panes[self.active]),
            SHUTDOWN_GRACE,
        );
        self.panes[self.active] = self.spawn_pane(harness);
        self.flash(format!("restarted {name}"));
    }

    fn on_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Normal => self.on_normal_key(key),
            Mode::Prefix => self.on_prefix_key(key),
            Mode::Compose => self.on_compose_key(key),
            Mode::Scroll => self.on_scroll_key(key),
            Mode::Help => self.mode = Mode::Normal,
            Mode::ConfirmQuit => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.quit = true;
                }
            }
            Mode::ConfirmRestart => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.restart_active();
                }
            }
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        if is_prefix(&key) {
            self.mode = Mode::Prefix;
            return;
        }
        if key.modifiers == KeyModifiers::ALT
            && let KeyCode::Char(c @ '1'..='9') = key.code
        {
            self.select(c as usize - '1' as usize);
            return;
        }
        if self.on_summary() {
            self.on_summary_key(key);
            return;
        }
        let pane = &self.panes[self.active];
        let mode = pane.mode();
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            && !mode.contains(TermMode::ALT_SCREEN)
        {
            pane.scroll(if key.code == KeyCode::PageUp {
                Scroll::PageUp
            } else {
                Scroll::PageDown
            });
            self.mode = Mode::Scroll;
            return;
        }
        if let Some(bytes) = keys::encode(&key, mode.contains(TermMode::APP_CURSOR)) {
            pane.clear_selection();
            if pane.display_offset() > 0 {
                pane.scroll(Scroll::Bottom);
            }
            pane.write(&bytes);
        }
    }

    fn on_prefix_key(&mut self, key: KeyEvent) {
        self.mode = Mode::Normal;
        if is_prefix(&key) {
            if let Some(pane) = self.panes.get(self.active) {
                pane.write(&[0x1c]);
            }
            return;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='9') => self.select(c as usize - '1' as usize),
            KeyCode::Char('n') | KeyCode::Right | KeyCode::Tab => self.cycle(1),
            KeyCode::Char('p') | KeyCode::Left | KeyCode::BackTab => self.cycle(-1),
            KeyCode::Char('a') | KeyCode::Char('b') => self.open_composer(true),
            KeyCode::Char('s') | KeyCode::Char('c') | KeyCode::Enter => self.open_composer(false),
            KeyCode::Char('[') | KeyCode::PageUp if !self.on_summary() => {
                self.panes[self.active].scroll(Scroll::PageUp);
                self.mode = Mode::Scroll;
            }
            KeyCode::Char('r') if self.on_summary() => self.regenerate_summary(),
            KeyCode::Char('r') => match self.panes[self.active].status() {
                Status::Running => self.mode = Mode::ConfirmRestart,
                _ => self.restart_active(),
            },
            KeyCode::Char('q') => self.mode = Mode::ConfirmQuit,
            KeyCode::Char('x') if !self.on_summary() => {
                let before = self.pending.len();
                let active = self.active;
                self.pending.retain(|(i, _)| *i != active);
                let removed = before - self.pending.len();
                let name = self.panes[active].name().to_string();
                self.flash(format!("cancelled {removed} queued prompt(s) for {name}"));
            }
            KeyCode::Char('?') | KeyCode::Char('h') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn on_compose_key(&mut self, key: KeyEvent) {
        if is_prefix(&key) {
            self.mode = Mode::Normal;
            return;
        }
        match self.composer.handle_key(&key) {
            ComposerAction::None => {}
            ComposerAction::Cancel => self.mode = Mode::Normal,
            ComposerAction::Submit { text, targets } => {
                self.submit_prompt(&text, &targets);
                self.mode = Mode::Normal;
            }
        }
    }

    fn submit_prompt(&mut self, text: &str, targets: &[usize]) {
        let delay = Duration::from_millis(self.config.submit_delay_ms);
        let mut sent = Vec::new();
        let mut queued = Vec::new();
        if targets.len() >= 2 {
            self.start_round(text, targets);
        }
        for &i in targets {
            if self.readiness(i) == Readiness::Ready {
                self.mark_delivered(i);
                self.panes[i].send_prompt(text, delay);
                sent.push(self.panes[i].name().to_string());
            } else {
                self.pending.push((i, text.to_string()));
                queued.push(self.panes[i].name().to_string());
            }
        }
        self.log_prompt(text, targets);
        let mut summary = Vec::new();
        if !sent.is_empty() {
            summary.push(format!("sent to {}", sent.join(", ")));
        }
        if !queued.is_empty() {
            summary.push(format!("queued for {} until ready", queued.join(", ")));
        }
        self.flash(summary.join("; "));
    }

    fn on_scroll_key(&mut self, key: KeyEvent) {
        let pane = &self.panes[self.active];
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => pane.scroll(Scroll::Delta(1)),
            KeyCode::Down | KeyCode::Char('j') => pane.scroll(Scroll::Delta(-1)),
            KeyCode::PageUp | KeyCode::Char('b') => pane.scroll(Scroll::PageUp),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
                pane.scroll(Scroll::PageDown)
            }
            KeyCode::Home | KeyCode::Char('g') => pane.scroll(Scroll::Top),
            KeyCode::End | KeyCode::Char('G') => pane.scroll(Scroll::Bottom),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                pane.scroll(Scroll::Bottom);
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        if self.mode == Mode::Scroll
            && self.panes[self.active].display_offset() == 0
            && key.code == KeyCode::PageDown
        {
            self.mode = Mode::Normal;
        }
    }

    fn on_paste(&mut self, text: &str) {
        match self.mode {
            Mode::Compose => self.composer.insert_str(text),
            Mode::Normal if self.on_summary() => self.summary_input.insert_str(text),
            Mode::Normal | Mode::Scroll if !self.on_summary() => {
                self.mode = Mode::Normal;
                let pane = &self.panes[self.active];
                pane.scroll(Scroll::Bottom);
                pane.paste(text);
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if mouse.row == 0 {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
                && let Some(i) = self
                    .tab_spans
                    .iter()
                    .position(|&(a, b)| mouse.column >= a && mouse.column < b)
            {
                self.select(i);
            }
            return;
        }
        if matches!(self.mode, Mode::Normal | Mode::Scroll) {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if self.begin_drag(mouse.column, mouse.row) {
                        return;
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.continue_drag(mouse.column, mouse.row);
                    return;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.end_drag(mouse.column, mouse.row);
                    return;
                }
                _ => {}
            }
        }
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        if self.on_summary() {
            self.summary_scroll = if up {
                self.summary_scroll.saturating_sub(3)
            } else {
                self.summary_scroll.saturating_add(3)
            };
            return;
        }
        let pane = &self.panes[self.active];
        let mode = pane.mode();
        let col = mouse.column + 1;
        let row = mouse.row;
        if mode.intersects(TermMode::MOUSE_MODE) {
            let button = if up { 64 } else { 65 };
            if mode.contains(TermMode::SGR_MOUSE) {
                pane.write(format!("\x1b[<{button};{col};{row}M").as_bytes());
            } else {
                let enc = |v: u16| (32 + v).min(255) as u8;
                pane.write(&[0x1b, b'[', b'M', 32 + button, enc(col), enc(row)]);
            }
        } else if mode.contains(TermMode::ALT_SCREEN) {
            let arrow = if up { b"\x1b[A" } else { b"\x1b[B" };
            for _ in 0..3 {
                pane.write(arrow);
            }
        } else {
            pane.scroll(Scroll::Delta(if up { 3 } else { -3 }));
            self.mode = if pane.display_offset() > 0 {
                Mode::Scroll
            } else {
                Mode::Normal
            };
        }
    }

    fn tab_label(&self, index: usize) -> (String, Style) {
        let pane = &self.panes[index];
        let (marker, color) = match pane.status() {
            Status::Running if self.has_pending(index) => (" ⧗", Color::Yellow),
            Status::Running if self.needs_input(index) => (" ?", Color::Magenta),
            Status::Running if pane.is_busy(BUSY_WINDOW) => (" ●", Color::Green),
            Status::Running => ("", Color::Reset),
            Status::Exited(_) => (" ✗", Color::Red),
            Status::Failed(_) => (" !", Color::Red),
        };
        let text = format!(" {} {}{} ", index + 1, pane.name(), marker);
        let mut style = Style::default().fg(color);
        if index == self.active {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        (text, style)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [tabs_area, pane_area, status_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        let mut spans = Vec::new();
        self.tab_spans.clear();
        let mut x = tabs_area.x;
        for i in 0..self.panes.len() {
            let (text, style) = self.tab_label(i);
            let width = text.width() as u16;
            self.tab_spans.push((x, x + width));
            x += width + 1;
            spans.push(Span::styled(text, style));
            spans.push(Span::raw("│").dark_gray());
        }
        let (text, style) = self.summary_tab_label();
        let width = text.width() as u16;
        self.tab_spans.push((x, x + width));
        spans.push(Span::styled(text, style));
        frame.render_widget(Paragraph::new(Line::from(spans)), tabs_area);

        if self.on_summary() {
            let input_cursor = self.draw_summary(frame, pane_area);
            frame.render_widget(Paragraph::new(self.status_line()), status_area);
            let cursor = match self.mode {
                Mode::Normal => input_cursor,
                Mode::Compose => self.draw_composer(frame, pane_area),
                Mode::Help => {
                    self.draw_help(frame, pane_area);
                    None
                }
                _ => None,
            };
            if let Some(pos) = cursor {
                frame.set_cursor_position(pos);
            }
            return;
        }

        let pane = &self.panes[self.active];
        let cursor = if let Some(failure) = pane.failure() {
            let text = format!(
                "{} failed to start:\n\n{failure}\n\nCtrl+\\ r to retry",
                pane.name()
            );
            frame.render_widget(
                Paragraph::new(text).red().wrap(Wrap { trim: false }),
                pane_area,
            );
            None
        } else {
            let cursor = match pane.term.lock() {
                Ok(term) => draw_term(&term, pane_area, frame.buffer_mut()),
                Err(_) => None,
            };
            if let Status::Exited(code) = pane.status() {
                let code = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                let banner = format!(
                    " {} exited (code {code}) — Ctrl+\\ r to restart ",
                    pane.name()
                );
                let bar = Rect::new(
                    pane_area.x,
                    pane_area.bottom().saturating_sub(1),
                    pane_area.width,
                    1,
                );
                frame.render_widget(Clear, bar);
                frame.render_widget(Paragraph::new(banner).white().on_red(), bar);
            }
            cursor
        };

        frame.render_widget(Paragraph::new(self.status_line()), status_area);

        let mut final_cursor = if self.mode == Mode::Normal {
            cursor
        } else {
            None
        };
        match self.mode {
            Mode::Compose => final_cursor = self.draw_composer(frame, pane_area),
            Mode::Help => self.draw_help(frame, pane_area),
            _ => {}
        }
        if let Some(pos) = final_cursor {
            frame.set_cursor_position(pos);
        }
    }

    fn status_line(&self) -> Line<'static> {
        let hint = |s: &str| Span::raw(s.to_string()).dark_gray();
        if self.on_summary() && matches!(self.mode, Mode::Normal | Mode::Help) {
            let mut spans = vec![Span::raw(" Σ summary ").black().on_magenta()];
            if let Some((text, at)) = &self.message
                && at.elapsed() < Duration::from_secs(4)
            {
                spans.push(Span::raw(format!(" {text}")).yellow());
            }
            spans.push(hint(
                "  Enter send to all · Alt+Enter newline · Ctrl+R summarize · Ctrl+P/N round · PgUp/PgDn scroll",
            ));
            return Line::from(spans);
        }
        let pane = &self.panes[self.active.min(self.panes.len() - 1)];
        match self.mode {
            Mode::Prefix => Line::from(vec![
                Span::raw(" PREFIX ").black().on_yellow(),
                hint(
                    " 1-9 tab · n/p next/prev · a prompt all · s prompt this · [ scroll · r restart · q quit · ? help · Esc cancel",
                ),
            ]),
            Mode::Compose => Line::from(vec![
                Span::raw(" PROMPT ").black().on_cyan(),
                hint(
                    " Enter send · Alt+Enter/Ctrl+J newline · Alt+1-9 toggle target · Ctrl+A all · ↑↓ history · Esc cancel",
                ),
            ]),
            Mode::Scroll => Line::from(vec![
                Span::raw(" SCROLL ").black().on_magenta(),
                hint(&format!(
                    " offset {} · ↑↓ j/k line · PgUp/PgDn page · g/G top/bottom · Esc/q back",
                    pane.display_offset()
                )),
            ]),
            Mode::ConfirmQuit => Line::from(vec![
                Span::raw(" QUIT ").white().on_red(),
                Span::raw(" Stop all harnesses and quit? y/N"),
            ]),
            Mode::ConfirmRestart => Line::from(vec![
                Span::raw(" RESTART ").white().on_red(),
                Span::raw(format!(
                    " {} is running. Kill and restart it? y/N",
                    pane.name()
                )),
            ]),
            Mode::Normal | Mode::Help => {
                let mut spans = vec![Span::raw(format!(" {} ", pane.name())).black().on_green()];
                if let Some((text, at)) = &self.message
                    && at.elapsed() < Duration::from_secs(4)
                {
                    spans.push(Span::raw(format!(" {text}")).yellow());
                }
                let title = pane.title().unwrap_or_else(|| {
                    std::iter::once(pane.harness.command.as_str())
                        .chain(pane.harness.args.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                spans.push(Span::raw(format!(" {title}")));
                spans.push(hint("   Ctrl+\\ a prompt all · Ctrl+\\ ? help"));
                Line::from(spans)
            }
        }
    }

    fn prompt_box_height(composer: &Composer, area: Rect) -> u16 {
        let lines = composer.text().split('\n').count() as u16;
        let max_height = (area.height / 2).max(3);
        (lines + 2).clamp(3, max_height)
    }

    fn draw_composer(&self, frame: &mut Frame, pane_area: Rect) -> Option<Position> {
        let height = Self::prompt_box_height(&self.composer, pane_area);
        let area = Rect::new(
            pane_area.x,
            pane_area.bottom().saturating_sub(height),
            pane_area.width,
            height,
        );
        self.draw_prompt_box(frame, area, &self.composer, " Prompt → ")
    }

    fn draw_prompt_box(
        &self,
        frame: &mut Frame,
        area: Rect,
        composer: &Composer,
        heading: &'static str,
    ) -> Option<Position> {
        let lines: Vec<&str> = composer.text().split('\n').collect();
        let mut title = vec![Span::raw(heading)];
        for (i, pane) in self.panes.iter().enumerate() {
            let on = composer.targets[i];
            let label = format!(
                "[{}] {} {}{} ",
                if on { "x" } else { " " },
                i + 1,
                pane.name(),
                match self.readiness(i) {
                    Readiness::Ready => "",
                    Readiness::NoPaste => " (starting)",
                    Readiness::Dialog => " (dialog open)",
                    Readiness::NotRunning => " (not running)",
                }
            );
            title.push(if on {
                Span::raw(label).cyan().bold()
            } else {
                Span::raw(label).dark_gray()
            });
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().cyan())
            .title(Line::from(title));
        let inner = block.inner(area);
        let (line, col) = composer.cursor_line_col();
        let visible = inner.height as usize;
        let first = line.saturating_sub(visible.saturating_sub(1));
        let shown: Vec<Line> = lines
            .iter()
            .skip(first)
            .take(visible)
            .map(|l| Line::raw(l.to_string()))
            .collect();

        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(shown).block(block), area);

        let before = lines.get(line).map(|l| {
            let byte = l.char_indices().nth(col).map(|(i, _)| i).unwrap_or(l.len());
            l[..byte].width() as u16
        })?;
        let x = (inner.x + before).min(inner.right().saturating_sub(1));
        let y = inner.y + (line - first) as u16;
        Some(Position::new(x, y))
    }

    fn draw_help(&self, frame: &mut Frame, pane_area: Rect) {
        let text = vec![
            Line::from("agent-tabs").bold(),
            Line::from(""),
            Line::from("Typing goes straight to the harness in the active tab."),
            Line::from(""),
            Line::from("Ctrl+\\ then:").bold(),
            Line::from("  1-9          switch to tab N  (also Alt+1-9 without prefix)"),
            Line::from("  n / p        next / previous tab  (also Tab / Shift+Tab, ← →)"),
            Line::from("  a            compose a prompt for ALL harnesses"),
            Line::from("  s / Enter    compose a prompt for this harness"),
            Line::from("  [ / PgUp     scroll back  (also Shift+PgUp, mouse wheel)"),
            Line::from("  r            restart this harness"),
            Line::from("  x            cancel prompts queued for this harness"),
            Line::from(
                "  r (Σ tab)    summarize the latest answers now (also Ctrl+R in the Σ tab)",
            ),
            Line::from("  q            quit (stops all harnesses)"),
            Line::from("  Ctrl+\\       send a literal Ctrl+\\ to the harness"),
            Line::from(""),
            Line::from("Mouse: drag with the left button to select text; it's copied on release."),
            Line::from(""),
            Line::from("Prompt composer:").bold(),
            Line::from("  Enter send · Alt+Enter / Ctrl+J newline · Alt+1-9 toggle target"),
            Line::from("  Ctrl+A toggle all · ↑/↓ history · Esc cancel"),
            Line::from(""),
            Line::from(
                "Tab markers: ● output · ⧗ prompt queued · ? waiting for your answer · ✗ exited",
            ),
            Line::from("Σ summary: a prompt sent to 2+ harnesses is summarized once all go quiet."),
            Line::from("Type in the Σ tab's prompt box and press Enter to send to all harnesses."),
            Line::from("A prompt waits in the queue while its harness is starting or shows a"),
            Line::from("dialog such as a folder-trust prompt, and is sent once it's at its input."),
            Line::from(""),
            Line::from("Press any key to close.").dark_gray(),
        ];
        let width = 72.min(pane_area.width);
        let height = (text.len() as u16 + 2).min(pane_area.height);
        let area = Rect::new(
            pane_area.x + (pane_area.width - width) / 2,
            pane_area.y + (pane_area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Help ")),
            area,
        );
    }

    fn start_round(&mut self, text: &str, targets: &[usize]) {
        if let Some(last) = self.rounds.last()
            && last.state() == SummaryState::Waiting
        {
            last.set_state(SummaryState::Superseded);
        }
        let names = targets
            .iter()
            .map(|&i| self.panes[i].name().to_string())
            .collect();
        self.rounds.push(Round::new(
            text.to_string(),
            targets.to_vec(),
            names,
            now_secs(),
        ));
        self.summary_folded.clear();
        self.view_round(self.rounds.len() - 1);
    }

    fn mark_delivered(&mut self, pane: usize) {
        let mark = self.panes[pane].history_mark();
        let Some(round) = self.rounds.last_mut() else {
            return;
        };
        if round.state() != SummaryState::Waiting {
            return;
        }
        if let Some(slot) = round.slot(pane)
            && round.delivered[slot].is_none()
        {
            round.delivered[slot] = Some((Instant::now(), mark));
        }
    }

    fn capture(&self, pane: usize, mark: usize, finished: bool) -> Capture {
        Capture {
            text: self.panes[pane].text_since(mark, self.config.summary.max_lines),
            finished,
        }
    }

    pub fn tick_summary(&mut self) -> bool {
        let Some(index) = self.rounds.len().checked_sub(1) else {
            return false;
        };
        let state = self.rounds[index].state();
        if state == SummaryState::Superseded || self.rounds[index].all_finished() {
            return false;
        }
        let quiet = Duration::from_secs(self.config.summary.quiet_secs);
        let timed_out = self.rounds[index].started.elapsed()
            >= Duration::from_secs(self.config.summary.wait_timeout_secs);
        let mut changed = false;
        for slot in 0..self.rounds[index].targets.len() {
            let existing = self.rounds[index].captures[slot]
                .as_ref()
                .map(|c| c.finished);
            if existing == Some(true) {
                continue;
            }
            let pane = self.rounds[index].targets[slot];
            let running = matches!(self.panes[pane].status(), Status::Running);
            let capture = match self.rounds[index].delivered[slot] {
                Some((at, mark)) => {
                    let quiet_now = at.elapsed() >= quiet
                        && !self.panes[pane].is_busy(quiet)
                        && !self.needs_input(pane);
                    if !running || quiet_now {
                        Some(self.capture(pane, mark, true))
                    } else if timed_out && existing.is_none() {
                        Some(self.capture(pane, mark, false))
                    } else {
                        None
                    }
                }
                None if existing.is_none() && (!running || timed_out) => {
                    Some(self.capture(pane, self.panes[pane].history_mark(), false))
                }
                None => None,
            };
            if capture.is_some() {
                self.rounds[index].captures[slot] = capture;
                changed = true;
            }
        }
        if !self.config.summary.auto || !self.rounds[index].all_captured() {
            return changed;
        }
        let rerun = self.rounds[index].compared_unfinished && self.rounds[index].all_finished();
        match self.rounds[index].state() {
            SummaryState::Waiting => self.start_summarizer(index),
            SummaryState::Done { .. } | SummaryState::Failed(_) if rerun => {
                self.start_summarizer(index);
                self.flash("answers completed; comparing again");
            }
            _ => {}
        }
        changed
    }

    fn spawn_summary_job(
        &self,
        state: Arc<std::sync::Mutex<SummaryState>>,
        input: String,
        log: Option<String>,
    ) {
        if let Ok(mut s) = state.lock() {
            *s = SummaryState::Running {
                since: Instant::now(),
            };
        }
        let config = self.config.summary.clone();
        let dirty = self.dirty.clone();
        let log_dir = self.log_dir.clone();
        std::thread::spawn(move || {
            let cwd = std::env::temp_dir().join("agent-tabs-summary");
            let _ = std::fs::create_dir_all(&cwd);
            let started = Instant::now();
            let next = match run_summarizer(&config, &input, &cwd) {
                Ok(text) => SummaryState::Done {
                    text,
                    took: started.elapsed(),
                },
                Err(err) => SummaryState::Failed(format!("{err:#}")),
            };
            if let (Some(dir), Some(header)) = (log_dir, log)
                && let Ok(mut file) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("summaries.log"))
            {
                let body = match &next {
                    SummaryState::Done { text, .. } => text.clone(),
                    SummaryState::Failed(err) => format!("(failed) {err}"),
                    _ => String::new(),
                };
                let _ = writeln!(file, "{header}\n\n{body}\n");
            }
            if let Ok(mut s) = state.lock() {
                *s = next;
            }
            dirty.store(true, Ordering::Release);
        });
    }

    fn start_summarizer(&mut self, index: usize) {
        for slot in 0..self.rounds[index].targets.len() {
            if self.rounds[index].captures[slot].is_none() {
                let pane = self.rounds[index].targets[slot];
                let mark = self.rounds[index].delivered[slot]
                    .map(|(_, mark)| mark)
                    .unwrap_or_else(|| self.panes[pane].history_mark());
                self.rounds[index].captures[slot] = Some(self.capture(pane, mark, false));
            }
        }
        if matches!(self.rounds[index].state(), SummaryState::Running { .. }) {
            return;
        }
        self.rounds[index].compared_unfinished = !self.rounds[index].all_finished();
        let round = &self.rounds[index];
        let input = round.input();
        let header = format!(
            "--- {} -> {}\n{}",
            round.started_at,
            round.names.join(","),
            round.prompt
        );
        let state = round.state.clone();
        self.spawn_summary_job(state, input, Some(header));
        self.summary_unseen = !self.on_summary();
    }

    fn regenerate_summary(&mut self) {
        if self.rounds.is_empty() {
            let targets: Vec<usize> = (0..self.panes.len())
                .filter(|&i| matches!(self.panes[i].status(), Status::Running))
                .collect();
            if targets.is_empty() {
                self.flash("no running harnesses to summarize");
                return;
            }
            let names = targets
                .iter()
                .map(|&i| self.panes[i].name().to_string())
                .collect();
            let mut round = Round::new(
                "(no prompt: summarize what each agent currently shows)".into(),
                targets,
                names,
                now_secs(),
            );
            round.captures = round
                .targets
                .iter()
                .map(|&i| Some(self.capture(i, 0, true)))
                .collect();
            self.rounds.push(round);
            self.summary_folded.clear();
            self.view_round(0);
        }
        let index = self.summary_view.min(self.rounds.len() - 1);
        if matches!(self.rounds[index].state(), SummaryState::Running { .. }) {
            self.flash("summary already running");
            return;
        }
        self.summary_scroll = 0;
        self.start_summarizer(index);
    }

    fn on_summary_key(&mut self, key: KeyEvent) {
        let last = self.rounds.len().saturating_sub(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.pane_size.rows / 2;
        let sections = self.summary_sections();
        match key.code {
            KeyCode::Char('r') if ctrl => self.regenerate_summary(),
            KeyCode::Char('p') if ctrl => self.view_round(self.summary_view.saturating_sub(1)),
            KeyCode::Char('n') if ctrl => self.view_round((self.summary_view + 1).min(last)),
            KeyCode::Char('o') if ctrl && sections > 0 => {
                self.toggle_section(self.summary_focus.min(sections - 1));
            }
            KeyCode::Char('e') if ctrl && sections > 0 => {
                let fold = !self.summary_folded.iter().all(|f| *f);
                self.summary_folded.iter_mut().for_each(|f| *f = fold);
            }
            KeyCode::Tab if sections > 0 => {
                self.summary_focus = (self.summary_focus + 1) % sections
            }
            KeyCode::BackTab if sections > 0 => {
                self.summary_focus = (self.summary_focus + sections - 1) % sections
            }
            KeyCode::Up if ctrl => self.summary_scroll = self.summary_scroll.saturating_sub(1),
            KeyCode::Down if ctrl => self.summary_scroll = self.summary_scroll.saturating_add(1),
            KeyCode::PageUp => self.summary_scroll = self.summary_scroll.saturating_sub(page),
            KeyCode::PageDown => self.summary_scroll = self.summary_scroll.saturating_add(page),
            _ => {
                self.summary_input.select_all();
                match self.summary_input.handle_key(&key) {
                    ComposerAction::None => {}
                    ComposerAction::Cancel => self.summary_input.clear(),
                    ComposerAction::Submit { text, targets } => {
                        self.submit_prompt(&text, &targets);
                        self.summary_scroll = 0;
                    }
                }
            }
        }
    }

    fn summary_sections(&self) -> usize {
        self.rounds
            .get(self.summary_view)
            .map(|r| r.targets.len() + 1)
            .unwrap_or(0)
    }

    fn view_round(&mut self, index: usize) {
        if index != self.summary_view || self.summary_folded.len() != self.summary_sections() {
            self.summary_view = index;
            self.summary_scroll = 0;
            self.summary_focus = 0;
            let agents = self.summary_sections().saturating_sub(1);
            self.summary_folded = std::iter::repeat_n(true, agents).chain([false]).collect();
            if self.summary_sections() == 0 {
                self.summary_folded.clear();
            }
        }
    }

    fn toggle_section(&mut self, section: usize) {
        if let Some(folded) = self.summary_folded.get_mut(section) {
            *folded = !*folded;
            self.summary_focus = section;
        }
    }

    fn summary_tab_label(&self) -> (String, Style) {
        let (marker, color) = match self.rounds.last().map(Round::state) {
            Some(SummaryState::Waiting) => (" ⧗", Color::Yellow),
            Some(SummaryState::Running { .. }) => (" …", Color::Cyan),
            Some(SummaryState::Done { .. }) if self.summary_unseen => (" ●", Color::Green),
            Some(SummaryState::Failed(_)) => (" !", Color::Red),
            _ => ("", Color::Reset),
        };
        let text = format!(" {} Σ summary{} ", self.summary_tab() + 1, marker);
        let mut style = Style::default().fg(color);
        if self.on_summary() {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        (text, style)
    }

    fn agent_status(&self, round: &Round, slot: usize) -> (String, Color) {
        let pane = round.targets[slot];
        match &round.captures[slot] {
            Some(capture) if capture.finished => ("✓ done".into(), Color::Reset),
            Some(_)
                if self.panes[pane]
                    .is_busy(Duration::from_secs(self.config.summary.quiet_secs)) =>
            {
                ("captured unfinished, still working".into(), Color::Yellow)
            }
            Some(_) if self.needs_input(pane) => (
                "captured unfinished, waiting for your answer".into(),
                Color::Magenta,
            ),
            Some(_) => ("captured unfinished".into(), Color::Yellow),
            None if round.delivered[slot].is_none() => ("prompt queued".into(), Color::Yellow),
            None if self.needs_input(pane) => ("waiting for your answer".into(), Color::Magenta),
            None if self.panes[pane]
                .is_busy(Duration::from_secs(self.config.summary.quiet_secs)) =>
            {
                ("working".into(), Color::Green)
            }
            None => ("settling".into(), Color::Green),
        }
    }

    fn section_header(
        &self,
        index: usize,
        label: String,
        status: (String, Color),
        headline: Option<&str>,
    ) -> Line<'static> {
        let folded = self.summary_folded.get(index).copied().unwrap_or(false);
        let mut name = Span::raw(format!("{} {label}", if folded { "▸" } else { "▾" })).bold();
        if index == self.summary_focus {
            name = name.reversed();
        }
        let mut spans = vec![
            name,
            Span::raw("  "),
            Span::styled(status.0, Style::default().fg(status.1)),
        ];
        if folded && let Some(headline) = headline {
            let short: String = headline.chars().take(110).collect();
            spans.push(Span::raw(format!("  — {short}")).dark_gray());
        }
        Line::from(spans)
    }

    fn summary_lines(&self) -> Vec<(Line<'static>, Option<usize>)> {
        let plain = |lines: Vec<Line<'static>>| lines.into_iter().map(|l| (l, None)).collect();
        let Some(round) = self.rounds.get(self.summary_view) else {
            return plain(vec![
                Line::from("No summaries yet.").bold(),
                Line::from(""),
                Line::from(
                    "Type a prompt in the box below and press Enter to send it to every harness.",
                ),
                Line::from(format!(
                    "Each harness's own answer appears here as soon as it's done, and all answers are compared by `{}` once every harness has been quiet for {}s.",
                    self.config.summary.command_line(),
                    self.config.summary.quiet_secs
                )),
                Line::from(""),
                Line::from("Press Ctrl+R to compare what the harnesses show right now."),
            ]);
        };
        let mut lines: Vec<(Line<'static>, Option<usize>)> = plain(vec![
            Line::from(vec![
                Span::raw(format!(
                    "Round {} of {}",
                    self.summary_view + 1,
                    self.rounds.len()
                ))
                .bold(),
                Span::raw(format!("  ·  {}", round.names.join(", "))).dark_gray(),
                Span::raw("   click or Ctrl+O to fold · Tab focus · Ctrl+E fold all").dark_gray(),
            ]),
            Line::from(""),
        ]);
        for (i, text) in round.prompt.lines().take(6).enumerate() {
            let label = if i == 0 { "Prompt: " } else { "        " };
            lines.push((
                Line::from(vec![Span::raw(label).bold(), Span::raw(text.to_string())]),
                None,
            ));
        }
        lines.push((Line::from(""), None));

        for slot in 0..round.targets.len() {
            let answer = round.answer(slot);
            let headline = answer.as_deref().and_then(crate::summary::headline);
            lines.push((
                self.section_header(
                    slot,
                    round.names[slot].clone(),
                    self.agent_status(round, slot),
                    headline.as_deref(),
                ),
                Some(slot),
            ));
            if !self.summary_folded.get(slot).copied().unwrap_or(false)
                && let Some(answer) = &answer
            {
                if answer.trim().is_empty() {
                    lines.push((Line::from("    (no answer captured)").dark_gray(), None));
                }
                lines.extend(
                    crate::summary::reflow(answer)
                        .into_iter()
                        .map(|l| (Line::from(format!("    {l}")), None)),
                );
            }
            lines.push((Line::from(""), None));
        }

        let overall = round.targets.len();
        let state = round.state();
        let status = match &state {
            SummaryState::Waiting if round.all_captured() && !self.config.summary.auto => (
                "all answers captured, press Ctrl+R to compare".to_string(),
                Color::Cyan,
            ),
            SummaryState::Waiting => (
                "waiting for every harness to finish".to_string(),
                Color::Yellow,
            ),
            SummaryState::Running { since } => (
                format!(
                    "comparing with `{}`… {}s",
                    self.config.summary.command_line(),
                    since.elapsed().as_secs()
                ),
                Color::Cyan,
            ),
            SummaryState::Done { took, .. } => {
                (format!("compared in {}s", took.as_secs()), Color::Reset)
            }
            SummaryState::Failed(_) => (
                "comparison failed, press Ctrl+R to retry".to_string(),
                Color::Red,
            ),
            SummaryState::Superseded => (
                "a newer prompt was sent first; press Ctrl+R to compare anyway".to_string(),
                Color::DarkGray,
            ),
        };
        let headline = match &state {
            SummaryState::Done { text, .. } => {
                text.lines().find(|l| !l.trim().is_empty()).map(str::trim)
            }
            _ => None,
        };
        lines.push((
            self.section_header(overall, "Σ comparison".into(), status, headline),
            Some(overall),
        ));
        if !self.summary_folded.get(overall).copied().unwrap_or(false) {
            match &state {
                SummaryState::Done { text, .. } => {
                    lines.extend(text.lines().map(|l| (Line::from(format!("    {l}")), None)))
                }
                SummaryState::Failed(err) => lines.extend(
                    err.lines()
                        .map(|l| (Line::from(format!("    {l}")).red(), None)),
                ),
                _ => {}
            }
        }
        lines
    }

    fn draw_summary(&mut self, frame: &mut Frame, area: Rect) -> Option<Position> {
        let mut input = self.summary_input.clone();
        input.select_all();
        let input_height = Self::prompt_box_height(&input, area);
        let [view_area, input_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(input_height)]).areas(area);
        let mut rows = Vec::new();
        let mut hits = Vec::new();
        for (line, section) in self.summary_lines() {
            for row in wrap_line(line, view_area.width.max(1) as usize) {
                rows.push(row);
                hits.push(section);
            }
        }
        let max_scroll = rows.len().saturating_sub(view_area.height as usize) as u16;
        self.summary_scroll = self.summary_scroll.min(max_scroll);
        self.summary_hits = hits;
        self.summary_area = view_area;
        self.summary_rows = rows.iter().map(|l| l.to_string()).collect();
        frame.render_widget(
            Paragraph::new(rows).scroll((self.summary_scroll, 0)),
            view_area,
        );
        if let Some((a, b)) = self.summary_selection {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            let buf = frame.buffer_mut();
            for y in 0..view_area.height {
                let row = y as usize + self.summary_scroll as usize;
                if row < start.0 || row > end.0 {
                    continue;
                }
                let from = if row == start.0 { start.1 } else { 0 };
                let to = if row == end.0 {
                    end.1
                } else {
                    view_area.width.saturating_sub(1)
                };
                for x in from..=to.min(view_area.width.saturating_sub(1)) {
                    let cell = &mut buf[(view_area.x + x, view_area.y + y)];
                    cell.modifier.toggle(Modifier::REVERSED);
                }
            }
        }
        self.draw_prompt_box(frame, input_area, &input, " Prompt all → ")
    }

    fn pane_rect(&self) -> Rect {
        Rect::new(0, 1, self.pane_size.cols, self.pane_size.rows)
    }

    fn clamp_to(area: Rect, column: u16, row: u16) -> (u16, u16) {
        (
            column.clamp(area.x, area.right().saturating_sub(1)) - area.x,
            row.clamp(area.y, area.bottom().saturating_sub(1)) - area.y,
        )
    }

    fn inside(area: Rect, column: u16, row: u16) -> bool {
        column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
    }

    fn begin_drag(&mut self, column: u16, row: u16) -> bool {
        if self.on_summary() {
            let area = self.summary_area;
            if !Self::inside(area, column, row) {
                return false;
            }
            let anchor = (
                (row - area.y) as usize + self.summary_scroll as usize,
                column - area.x,
            );
            self.summary_selection = None;
            self.drag = Some(Drag::Summary {
                anchor,
                moved: false,
            });
            return true;
        }
        let area = self.pane_rect();
        if !Self::inside(area, column, row) {
            return false;
        }
        let pane = &self.panes[self.active];
        pane.clear_selection();
        pane.start_selection(column - area.x, row - area.y);
        self.drag = Some(Drag::Pane {
            index: self.active,
            moved: false,
        });
        true
    }

    fn continue_drag(&mut self, column: u16, row: u16) {
        match self.drag {
            Some(Drag::Pane { index, .. }) => {
                let (col, row) = Self::clamp_to(self.pane_rect(), column, row);
                self.panes[index].update_selection(col, row);
                self.drag = Some(Drag::Pane { index, moved: true });
            }
            Some(Drag::Summary { anchor, .. }) => {
                let area = self.summary_area;
                let (col, row) = Self::clamp_to(area, column, row);
                let head = (row as usize + self.summary_scroll as usize, col);
                self.summary_selection = Some((anchor, head));
                self.drag = Some(Drag::Summary {
                    anchor,
                    moved: true,
                });
            }
            None => {}
        }
    }

    fn end_drag(&mut self, column: u16, row: u16) {
        match self.drag.take() {
            Some(Drag::Pane { index, moved }) => {
                let text = moved.then(|| self.panes[index].selection_text()).flatten();
                match text {
                    Some(text) => {
                        let message = self.clipboard.copy(&text);
                        self.flash(message);
                    }
                    None => self.panes[index].clear_selection(),
                }
            }
            Some(Drag::Summary { moved: false, .. }) => {
                self.summary_selection = None;
                self.summary_click(column, row);
            }
            Some(Drag::Summary { moved: true, .. }) => {
                if let Some(text) = self.summary_selected_text() {
                    let message = self.clipboard.copy(&text);
                    self.flash(message);
                }
            }
            None => {}
        }
    }

    fn summary_selected_text(&self) -> Option<String> {
        let (a, b) = self.summary_selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let mut out = Vec::new();
        for row in start.0..=end.0.min(self.summary_rows.len().saturating_sub(1)) {
            let text = self.summary_rows.get(row)?;
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 { end.1 } else { u16::MAX };
            out.push(columns_of(text, from, to).trim_end().to_string());
        }
        let text = out.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    fn summary_click(&mut self, column: u16, row: u16) {
        let area = self.summary_area;
        if column < area.x || column >= area.right() || row < area.y || row >= area.bottom() {
            return;
        }
        let index = (row - area.y) as usize + self.summary_scroll as usize;
        if let Some(Some(section)) = self.summary_hits.get(index).copied() {
            self.toggle_section(section);
        }
    }

    #[cfg(test)]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[cfg(test)]
    pub fn active(&self) -> usize {
        self.active
    }

    #[cfg(test)]
    pub fn pane(&self, index: usize) -> &Pane {
        &self.panes[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::thread;

    fn test_summary() -> crate::summary::SummaryConfig {
        crate::summary::SummaryConfig {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "printf 'SUMMARY:'; grep -c 'status=\"finished\"' ".into(),
            ],
            quiet_secs: 1,
            run_timeout_secs: 10,
            ..Default::default()
        }
    }

    fn script_harness(name: &str, script: &str) -> Harness {
        Harness {
            name: name.into(),
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            env_remove: Vec::new(),
        }
    }

    const ECHO_LOOP: &str = "stty -echo; while read l; do printf 'got:%s\\r\\n' \"$l\"; done";

    fn cat_harness(name: &str) -> Harness {
        script_harness(name, &format!("printf '\\033[?2004h'; {ECHO_LOOP}"))
    }

    fn app_with(harnesses: Vec<Harness>) -> App {
        let config = Config {
            submit_delay_ms: 30,
            scrollback: 100,
            mouse: true,
            osc52: false,
            system_clipboard: false,
            dialog_patterns: crate::config::default_dialog_patterns(),
            input_patterns: crate::config::default_input_patterns(),
            summary: test_summary(),
            harnesses,
        };
        App::new(
            config,
            PathBuf::from("."),
            None,
            Size { cols: 60, rows: 12 },
        )
        .unwrap()
    }

    fn wait_until(app: &mut App, mut done: impl FnMut(&App) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out");
            app.deliver_pending();
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn compose_all(app: &mut App, text: &str) {
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE));
        type_text(app, text);
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn prompt_waits_until_harness_accepts_paste() {
        let late = format!("sleep 0.6; printf '\\033[?2004h'; {ECHO_LOOP}");
        let mut app = app_with(vec![cat_harness("ready"), script_harness("late", &late)]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "hello");
        assert!(app.has_pending(1));
        assert!(!app.has_pending(0));
        wait_for(app.pane(0), "got:hello");
        assert!(
            !screen(app.pane(1)).contains("hello"),
            "prompt typed before the harness was ready"
        );
        wait_until(&mut app, |a| !a.has_pending(1));
        wait_for(app.pane(1), "got:hello");
    }

    #[test]
    fn prompt_waits_while_a_trust_dialog_is_shown() {
        let dialog = format!(
            "printf '\\033[?2004hDo you trust the contents of this project?'; sleep 0.6; printf '\\033[2J\\033[H'; {ECHO_LOOP}"
        );
        let mut app = app_with(vec![script_harness("dialog", &dialog)]);
        wait_for(app.pane(0), "Do you trust");
        compose_all(&mut app, "after");
        assert!(app.has_pending(0));
        assert_eq!(app.readiness(0), Readiness::Dialog);
        wait_until(&mut app, |a| !a.has_pending(0));
        wait_for(app.pane(0), "got:after");
    }

    fn summary_text(app: &App) -> String {
        app.summary_lines()
            .iter()
            .map(|(l, _)| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_summary(app: &mut App, needle: &str) {
        let start = Instant::now();
        loop {
            app.deliver_pending();
            app.tick_summary();
            if summary_text(app).contains(needle) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "missing {needle:?} in:\n{}",
                summary_text(app)
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn broadcast_is_summarized_after_harnesses_go_quiet() {
        let mut app = app(&["a", "b"]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "question");
        assert_eq!(app.rounds.len(), 1);
        wait_for(app.pane(0), "got:question");
        wait_for(app.pane(1), "got:question");
        wait_summary(&mut app, "SUMMARY:2");
        let input = app.rounds[0].input();
        assert!(input.contains("<prompt>\nquestion\n</prompt>"), "{input}");
        assert!(
            input.contains("<agent name=\"a\" status=\"finished\">"),
            "{input}"
        );
        assert!(input.contains("got:question"), "{input}");
    }

    #[test]
    fn harness_waiting_for_permission_is_not_treated_as_done() {
        let asking = format!(
            "printf '\\033[?2004h'; read l; printf 'Do you want to proceed?\\r\\n 1. Yes\\r\\n'; sleep 2.5; printf '\\033[2J\\033[Hanswered\\r\\n'; {ECHO_LOOP}"
        );
        let mut app = app_with(vec![cat_harness("a"), script_harness("asker", &asking)]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "go");
        wait_for(app.pane(1), "Do you want to proceed?");
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(2000) {
            app.tick_summary();
            assert!(
                app.rounds[0].captures[1].is_none(),
                "captured while waiting for permission"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(app.needs_input(1));
        assert_eq!(app.tab_label(1).0, " 2 asker ? ");
        assert!(summary_text(&app).contains("waiting for your answer"));
        wait_summary(&mut app, "SUMMARY:2");
        assert!(app.rounds[0].input().contains("answered"));
    }

    fn replying_harness(name: &str) -> Harness {
        script_harness(
            name,
            &format!(
                "printf '\\033[?2004h'; stty -echo; while read l; do printf '› %s\\r\\n• Ran ls\\r\\n  └ out.txt\\r\\n• answer from {name}\\r\\n────────────────\\r\\n› Ask me\\r\\n' \"$l\"; done"
            ),
        )
    }

    fn draw_app(app: &mut App, cols: u16, rows: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(cols, rows);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn sections_show_each_harness_original_answer_folded() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "what now");
        wait_summary(&mut app, "SUMMARY:2");
        let text = summary_text(&app);
        assert!(text.contains("▸ a  ✓ done  — • answer from a"), "{text}");
        assert!(!text.contains("    • answer from a"), "{text}");
        assert!(text.contains("▾ Σ comparison"), "{text}");
        assert_eq!(app.rounds[0].answer(0).unwrap(), "• answer from a");

        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        app.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let text = summary_text(&app);
        assert!(
            text.contains("▾ a  ✓ done\n    • answer from a") && !text.contains("Ran ls"),
            "{text}"
        );
        app.handle_event(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(!summary_text(&app).contains("▾ "));
        app.handle_event(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(!summary_text(&app).contains("▸ "));
    }

    #[test]
    fn clicking_a_section_header_toggles_it_and_view_has_no_border() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "click test");
        wait_summary(&mut app, "SUMMARY:2");
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        let buf = draw_app(&mut app, 60, 20);
        let rows = buffer_rows(&buf);
        assert!(
            rows[1].starts_with("Round 1 of 2") || rows[1].starts_with("Round 1 of 1"),
            "{rows:#?}"
        );
        let header = rows
            .iter()
            .position(|r| r.starts_with("▸ b"))
            .expect("b header");
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: header as u16,
            modifiers: KeyModifiers::NONE,
        }));
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            2,
            header as u16,
        );
        assert!(summary_text(&app).contains("▾ b"), "{}", summary_text(&app));
        let rows = buffer_rows(&draw_app(&mut app, 60, 20));
        assert!(
            rows.iter().any(|r| r.starts_with("    • answer from b")),
            "{rows:#?}"
        );
        assert!(
            rows[1..6]
                .iter()
                .all(|r| !r.contains('│') && !r.contains('┌')),
            "{rows:#?}"
        );
    }

    fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.handle_event(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    #[test]
    fn dragging_in_a_pane_selects_and_copies_text() {
        let mut app = app_with(vec![script_harness(
            "p",
            "printf 'alpha beta\\r\\ngamma delta'; sleep 5",
        )]);
        wait_for(app.pane(0), "gamma delta");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 6, 1);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 2, 2);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 2, 2);
        assert_eq!(app.clipboard.last.as_deref(), Some("beta\ngam"));
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 1, 1);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 1, 1);
        assert!(
            app.pane(0).selection_text().is_none(),
            "a plain click clears the selection"
        );
    }

    #[test]
    fn dragging_in_summary_copies_and_click_still_folds() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "copy test");
        wait_summary(&mut app, "SUMMARY:2");
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        let rows = buffer_rows(&draw_app(&mut app, 60, 20));
        let prompt_row = rows
            .iter()
            .position(|r| r.starts_with("Prompt: copy test"))
            .unwrap() as u16;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            8,
            prompt_row,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            16,
            prompt_row,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            16,
            prompt_row,
        );
        assert_eq!(app.clipboard.last.as_deref(), Some("copy test"));
        let rows = buffer_rows(&draw_app(&mut app, 60, 20));
        let header = rows.iter().position(|r| r.starts_with("▸ a")).unwrap() as u16;
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 1, header);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 1, header);
        assert!(summary_text(&app).contains("▾ a"));
    }

    #[test]
    fn wrap_line_breaks_at_spaces_and_keeps_indent() {
        let rows = wrap_line(Line::from("    alpha beta gamma delta"), 16);
        let text: Vec<String> = rows.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, ["    alpha beta ", "    gamma delta"]);
        assert_eq!(wrap_line(Line::from("short"), 16).len(), 1);
        let long = wrap_line(Line::from("x".repeat(40)), 16);
        assert_eq!(long.len(), 3);
    }

    #[test]
    fn unfinished_capture_is_updated_and_comparison_rerun() {
        let slow = "printf '\\033[?2004h'; stty -echo; read l; printf '› %s\\r\\n' \"$l\"; for i in 1 2 3 4 5 6 7 8; do printf '• working %s\\r\\n' $i; sleep 0.4; done; printf '• final answer\\r\\n────────────────\\r\\n› Ask me\\r\\n'; sleep 30";
        let mut app = app_with(vec![replying_harness("fast"), script_harness("slow", slow)]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "long task");
        wait_for(app.pane(1), "working 2");
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        app.handle_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.rounds[0].compared_unfinished);
        assert!(!app.rounds[0].captures[1].as_ref().unwrap().finished);
        wait_summary(&mut app, "SUMMARY:");
        wait_summary(&mut app, "captured unfinished, still working");
        wait_summary(&mut app, "SUMMARY:2");
        assert!(app.rounds[0].all_finished());
        assert!(!app.rounds[0].compared_unfinished);
        assert!(app.rounds[0].answer(1).unwrap().ends_with("• final answer"));
        assert!(
            summary_text(&app).contains("▸ slow  ✓ done  — • final answer"),
            "{}",
            summary_text(&app)
        );
    }

    #[test]
    fn single_target_prompt_does_not_start_a_round() {
        let mut app = app(&["a", "b"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('s'), KeyModifiers::NONE));
        type_text(&mut app, "solo");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.rounds.is_empty());
    }

    #[test]
    fn summary_tab_r_summarizes_current_screens() {
        let mut app = app(&["a", "b"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        assert!(app.on_summary());
        assert!(summary_text(&app).contains("No summaries yet"));
        app.handle_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        wait_summary(&mut app, "SUMMARY:2");
    }

    #[test]
    fn typing_in_summary_tab_sends_to_all_harnesses() {
        let mut app = app(&["a", "b", "c"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('4'), KeyModifiers::ALT));
        assert!(app.on_summary());
        type_text(&mut app, "ready");
        app.handle_event(Event::Paste(" set".into()));
        assert_eq!(app.summary_input.text(), "ready set");
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(!app.on_summary(), "Alt+digit still switches tabs");
        app.handle_event(key(KeyCode::Char('4'), KeyModifiers::ALT));
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.on_summary());
        assert_eq!(app.summary_input.text(), "");
        for i in 0..3 {
            wait_for(app.pane(i), "got:ready set");
        }
        assert_eq!(app.rounds.len(), 1);
        assert_eq!(app.rounds[0].targets, [0, 1, 2]);
    }

    #[test]
    fn summary_tab_keeps_commands_on_control_keys() {
        let mut app = app(&["a", "b"]);
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        type_text(&mut app, "rpn");
        assert_eq!(app.summary_input.text(), "rpn");
        assert!(app.rounds.is_empty());
        app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.summary_input.text(), "");
        app.handle_event(key(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.summary_scroll > 0);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(app.active(), 0);
    }

    #[test]
    fn newer_broadcast_supersedes_a_waiting_round() {
        let mut app = app(&["a", "b"]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "first");
        compose_all(&mut app, "second");
        assert_eq!(app.rounds.len(), 2);
        assert_eq!(app.rounds[0].state(), SummaryState::Superseded);
        assert_eq!(app.rounds[1].state(), SummaryState::Waiting);
    }

    #[test]
    fn prompt_is_never_typed_as_keystrokes() {
        let mut app = app_with(vec![script_harness("raw", ECHO_LOOP)]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "quit");
        thread::sleep(Duration::from_millis(300));
        app.deliver_pending();
        assert!(app.has_pending(0));
        assert!(!screen(app.pane(0)).contains("got:"));
    }

    #[test]
    fn queued_prompt_can_be_cancelled_and_is_dropped_on_exit() {
        let mut app = app_with(vec![
            script_harness("raw", ECHO_LOOP),
            script_harness("gone", "sleep 0.4"),
        ]);
        thread::sleep(Duration::from_millis(100));
        compose_all(&mut app, "x");
        assert!(app.has_pending(0) && app.has_pending(1));
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!app.has_pending(0));
        wait_until(&mut app, |a| !a.has_pending(1));
        assert!(matches!(app.pane(1).status(), Status::Exited(_)));
    }

    fn app(names: &[&str]) -> App {
        let config = Config {
            submit_delay_ms: 30,
            scrollback: 100,
            mouse: true,
            osc52: false,
            system_clipboard: false,
            dialog_patterns: crate::config::default_dialog_patterns(),
            input_patterns: crate::config::default_input_patterns(),
            summary: test_summary(),
            harnesses: names.iter().map(|n| cat_harness(n)).collect(),
        };
        App::new(
            config,
            PathBuf::from("."),
            None,
            Size { cols: 60, rows: 12 },
        )
        .unwrap()
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_event(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn screen(pane: &Pane) -> String {
        let term = pane.term.lock().unwrap();
        let content = term.renderable_content();
        let mut out = String::new();
        for cell in content.display_iter {
            out.push(cell.c);
        }
        out
    }

    fn wait_for(pane: &Pane, needle: &str) {
        let start = Instant::now();
        while !screen(pane).contains(needle) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "missing {needle:?} in {:?}",
                screen(pane)
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn prefix_switches_tabs() {
        let mut app = app(&["a", "b", "c"]);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        assert_eq!(app.mode(), Mode::Prefix);
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::NONE));
        assert_eq!(app.active(), 2);
        app.handle_event(key(KeyCode::Char('4'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.active(), 3, "the summary tab follows the harness tabs");
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.active(), 0);
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.active(), 1);
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn typing_goes_to_active_pane_only() {
        let mut app = app(&["a", "b"]);
        thread::sleep(Duration::from_millis(200));
        type_text(&mut app, "hello");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        wait_for(app.pane(0), "got:hello");
        assert!(!screen(app.pane(1)).contains("got:"));
    }

    #[test]
    fn broadcast_prompt_reaches_all_targets() {
        let mut app = app(&["a", "b", "c"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.mode(), Mode::Compose);
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        type_text(&mut app, "ping");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode(), Mode::Normal);
        wait_for(app.pane(0), "got:ping");
        wait_for(app.pane(2), "got:ping");
        thread::sleep(Duration::from_millis(200));
        assert!(!screen(app.pane(1)).contains("got:ping"));
    }

    #[test]
    fn single_prompt_targets_active_pane() {
        let mut app = app(&["a", "b"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('s'), KeyModifiers::NONE));
        type_text(&mut app, "solo");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        wait_for(app.pane(1), "got:solo");
        thread::sleep(Duration::from_millis(200));
        assert!(!screen(app.pane(0)).contains("got:solo"));
    }

    #[test]
    fn quit_requires_confirmation() {
        let mut app = app(&["a"]);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(app.mode(), Mode::ConfirmQuit);
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(!app.quit);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_event(key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.quit);
    }

    #[test]
    fn prompts_are_logged() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-test-{}", std::process::id()));
        let config = Config {
            submit_delay_ms: 10,
            scrollback: 100,
            mouse: true,
            osc52: false,
            system_clipboard: false,
            dialog_patterns: crate::config::default_dialog_patterns(),
            input_patterns: crate::config::default_input_patterns(),
            summary: test_summary(),
            harnesses: vec![cat_harness("a")],
        };
        let mut app = App::new(
            config,
            PathBuf::from("."),
            Some(dir.clone()),
            Size { cols: 40, rows: 8 },
        )
        .unwrap();
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE));
        type_text(&mut app, "logged");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        let log = std::fs::read_to_string(dir.join("prompts.log")).unwrap();
        assert!(log.contains("-> a\nlogged"), "{log}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
