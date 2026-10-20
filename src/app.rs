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

use crate::composer::{Composer, ComposerAction};
use crate::config::{Config, Harness};
use crate::keys;
use crate::pane::{Pane, Size, Status};
use crate::render::draw_term;

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
    config: Config,
    cwd: PathBuf,
    log_dir: Option<PathBuf>,
    dirty: Arc<AtomicBool>,
    pane_size: Size,
    tab_spans: Vec<(u16, u16)>,
    message: Option<(String, Instant)>,
    pending: Vec<(usize, String)>,
    dialogs: regex::RegexSet,
    quit: bool,
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
        let mut app = Self {
            panes: Vec::new(),
            active: 0,
            mode: Mode::Normal,
            composer: Composer::new(config.harnesses.len()),
            config,
            cwd,
            log_dir,
            dirty,
            pane_size,
            tab_spans: Vec::new(),
            message: None,
            pending: Vec::new(),
            dialogs: regex::RegexSet::empty(),
            quit: false,
        };
        app.dialogs = app.config.dialog_regex()?;
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

    fn select(&mut self, index: usize) {
        if index < self.panes.len() {
            self.active = index;
        }
    }

    fn cycle(&mut self, delta: isize) {
        let n = self.panes.len() as isize;
        self.active = ((self.active as isize + delta).rem_euclid(n)) as usize;
    }

    fn open_composer(&mut self, all: bool) {
        let targets: Vec<usize> = if all {
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
            if pane.display_offset() > 0 {
                pane.scroll(Scroll::Bottom);
            }
            pane.write(&bytes);
        }
    }

    fn on_prefix_key(&mut self, key: KeyEvent) {
        self.mode = Mode::Normal;
        if is_prefix(&key) {
            self.panes[self.active].write(&[0x1c]);
            return;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='9') => self.select(c as usize - '1' as usize),
            KeyCode::Char('n') | KeyCode::Right | KeyCode::Tab => self.cycle(1),
            KeyCode::Char('p') | KeyCode::Left | KeyCode::BackTab => self.cycle(-1),
            KeyCode::Char('a') | KeyCode::Char('b') => self.open_composer(true),
            KeyCode::Char('s') | KeyCode::Char('c') | KeyCode::Enter => self.open_composer(false),
            KeyCode::Char('[') | KeyCode::PageUp => {
                self.panes[self.active].scroll(Scroll::PageUp);
                self.mode = Mode::Scroll;
            }
            KeyCode::Char('r') => match self.panes[self.active].status() {
                Status::Running => self.mode = Mode::ConfirmRestart,
                _ => self.restart_active(),
            },
            KeyCode::Char('q') => self.mode = Mode::ConfirmQuit,
            KeyCode::Char('x') => {
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
                let delay = Duration::from_millis(self.config.submit_delay_ms);
                let mut sent = Vec::new();
                let mut queued = Vec::new();
                for &i in &targets {
                    if self.readiness(i) == Readiness::Ready {
                        self.panes[i].send_prompt(&text, delay);
                        sent.push(self.panes[i].name().to_string());
                    } else {
                        self.pending.push((i, text.clone()));
                        queued.push(self.panes[i].name().to_string());
                    }
                }
                self.log_prompt(&text, &targets);
                let mut summary = Vec::new();
                if !sent.is_empty() {
                    summary.push(format!("sent to {}", sent.join(", ")));
                }
                if !queued.is_empty() {
                    summary.push(format!("queued for {} until ready", queued.join(", ")));
                }
                self.flash(summary.join("; "));
                self.mode = Mode::Normal;
            }
        }
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
            Mode::Normal | Mode::Scroll => {
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
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
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
        frame.render_widget(Paragraph::new(Line::from(spans)), tabs_area);

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
        let pane = &self.panes[self.active];
        let hint = |s: &str| Span::raw(s.to_string()).dark_gray();
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

    fn draw_composer(&self, frame: &mut Frame, pane_area: Rect) -> Option<Position> {
        let lines: Vec<&str> = self.composer.text().split('\n').collect();
        let max_height = (pane_area.height / 2).max(3);
        let height = (lines.len() as u16 + 2).clamp(3, max_height);
        let area = Rect::new(
            pane_area.x,
            pane_area.bottom().saturating_sub(height),
            pane_area.width,
            height,
        );

        let mut title = vec![Span::raw(" Prompt → ")];
        for (i, pane) in self.panes.iter().enumerate() {
            let on = self.composer.targets[i];
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
        let (line, col) = self.composer.cursor_line_col();
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
            Line::from("  q            quit (stops all harnesses)"),
            Line::from("  Ctrl+\\       send a literal Ctrl+\\ to the harness"),
            Line::from(""),
            Line::from("Prompt composer:").bold(),
            Line::from("  Enter send · Alt+Enter / Ctrl+J newline · Alt+1-9 toggle target"),
            Line::from("  Ctrl+A toggle all · ↑/↓ history · Esc cancel"),
            Line::from(""),
            Line::from("Tab markers: ● producing output · ⧗ prompt queued · ✗ exited · ! failed"),
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
            dialog_patterns: crate::config::default_dialog_patterns(),
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
            dialog_patterns: crate::config::default_dialog_patterns(),
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
            dialog_patterns: crate::config::default_dialog_patterns(),
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
