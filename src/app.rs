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
    working: regex::RegexSet,
    summary_scroll: u16,
    summary_unseen: bool,
    model_patterns: Vec<Vec<regex::Regex>>,
    target_hits: std::cell::RefCell<Vec<TargetHit>>,
    prompt_widths: std::cell::RefCell<std::collections::HashMap<PromptBox, usize>>,
    exit_span: (u16, u16),
    quit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PromptBox {
    Composer,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetHit {
    kind: PromptBox,
    y: u16,
    x0: u16,
    x1: u16,
    target: usize,
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

/// A line of styled chars as spans, one per run of the same style.
fn styled_line(prefix: &str, cells: crate::summary::Cells) -> Line<'static> {
    let mut spans = vec![Span::raw(prefix.to_string())];
    let mut run = String::new();
    let mut style = Style::default();
    for (c, cell_style) in cells {
        if cell_style != style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        style = cell_style;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

/// Drawn after the fold arrow of each summary section header.
const REFRESH_BUTTON: &str = "⟳ ";

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

fn wrap_ranges(chars: &[char], width: usize) -> Vec<(usize, usize)> {
    use unicode_width::UnicodeWidthChar;
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut used = 0;
        let mut end = start;
        while end < chars.len() {
            let w = chars[end].width().unwrap_or(0);
            if used + w > width {
                break;
            }
            used += w;
            end += 1;
        }
        if end == start {
            end = start + 1;
        }
        if end < chars.len()
            && let Some(space) = chars[start..end].iter().rposition(|c| *c == ' ')
            && space > 0
        {
            end = start + space + 1;
        }
        rows.push((start, end));
        start = end;
    }
    rows
}

fn move_visual(composer: &mut Composer, width: Option<usize>, key: &KeyEvent) -> bool {
    use unicode_width::UnicodeWidthChar;
    let up = match key.code {
        KeyCode::Up if key.modifiers.is_empty() => true,
        KeyCode::Down if key.modifiers.is_empty() => false,
        _ => return false,
    };
    let Some(width) = width else { return false };
    let rows = visual_rows(composer.text(), width);
    let (_, (row, x)) = layout_prompt(composer.text(), composer.cursor_line_col(), width);
    let target = if up {
        row.checked_sub(1)
    } else {
        Some(row + 1)
    };
    let Some((line, start, end, chars)) = target.and_then(|t| rows.get(t)) else {
        return false;
    };
    let mut col = *start;
    let mut used = 0;
    while col < *end && used + chars[col].width().unwrap_or(0) <= x {
        used += chars[col].width().unwrap_or(0);
        col += 1;
    }
    if col == *end && *end > *start && chars[*end - 1] == ' ' && *end < chars.len() {
        col = *end - 1;
    }
    composer.set_cursor_line_col(*line, col);
    true
}

fn visual_rows(text: &str, width: usize) -> Vec<(usize, usize, usize, Vec<char>)> {
    let mut rows = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        let chars: Vec<char> = line.chars().collect();
        for (start, end) in wrap_ranges(&chars, width) {
            rows.push((index, start, end, chars.clone()));
        }
    }
    rows
}

fn layout_prompt(
    text: &str,
    cursor: (usize, usize),
    width: usize,
) -> (Vec<String>, (usize, usize)) {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut position = (0, 0);
    for (index, line) in text.split('\n').enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let ranges = wrap_ranges(&chars, width);
        let last = ranges.len() - 1;
        for (r, &(start, end)) in ranges.iter().enumerate() {
            if index == cursor.0 {
                let col = cursor.1.min(chars.len());
                if col >= start && (col < end || r == last) {
                    let x: usize = chars[start..col]
                        .iter()
                        .map(|c| c.width().unwrap_or(0))
                        .sum();
                    position = (rows.len(), x);
                }
            }
            rows.push(chars[start..end].iter().collect::<String>());
        }
    }
    if position.1 >= width {
        position = (position.0 + 1, 0);
        if position.0 == rows.len() {
            rows.push(String::new());
        }
    }
    (rows, position)
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
            active: config.harnesses.len(),
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
            working: regex::RegexSet::empty(),
            summary_scroll: 0,
            summary_unseen: false,
            model_patterns: Vec::new(),
            target_hits: std::cell::RefCell::new(Vec::new()),
            prompt_widths: std::cell::RefCell::new(std::collections::HashMap::new()),
            exit_span: (0, 0),
            quit: false,
        };
        app.dialogs = app.config.dialog_regex()?;
        app.inputs = app.config.input_regex()?;
        app.working = app.config.working_regex()?;
        app.model_patterns = app
            .config
            .harnesses
            .iter()
            .map(|h| h.model_regexes())
            .collect::<Result<_>>()?;
        let harnesses = app.config.harnesses.clone();
        for harness in harnesses {
            let pane = if harness.is_enabled() {
                app.spawn_pane(harness)
            } else {
                Pane::stopped(harness, app.pane_size, app.config.scrollback)
            };
            app.panes.push(pane);
        }
        let defaults = app.default_targets();
        app.summary_input.open(&defaults);
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
        let harness = if self.config.yolo {
            harness.with_yolo()
        } else {
            harness
        };
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
                    self.send_to(i, &text, delay);
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

    fn shows_trust_dialog(&self, index: usize) -> bool {
        matches!(self.panes[index].status(), Status::Running)
            && self.dialogs.is_match(&self.panes[index].screen_text())
    }

    fn still_working(&self, index: usize) -> bool {
        matches!(self.panes[index].status(), Status::Running)
            && self.working.is_match(&self.panes[index].screen_tail(25))
    }

    fn needs_input(&self, index: usize) -> bool {
        matches!(self.panes[index].status(), Status::Running)
            && self.inputs.is_match(&self.panes[index].screen_tail(25))
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
        self.start_checked();
    }

    /// Starts the disabled harnesses checked in a prompt box, which also shows
    /// their tabs.
    fn start_checked(&mut self) {
        let mut checked = self.summary_input.selected();
        if self.mode == Mode::Compose {
            checked.extend(self.composer.selected());
        }
        let mut started = Vec::new();
        for i in checked {
            if self.start(i) {
                started.push(self.panes[i].name().to_string());
            }
        }
        if !started.is_empty() {
            self.flash(format!("started {}", started.join(", ")));
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

    /// The tabs in tab-bar order: the summary tab first, then the harnesses in
    /// config order, leaving out disabled ones that have not been started.
    fn visible_tabs(&self) -> Vec<usize> {
        std::iter::once(self.summary_tab())
            .chain((0..self.panes.len()).filter(|&i| self.panes[i].status() != Status::Stopped))
            .collect()
    }

    fn select_position(&mut self, position: usize) {
        if let Some(&index) = self.visible_tabs().get(position) {
            self.select(index);
        }
    }

    fn cycle(&mut self, delta: isize) {
        let tabs = self.visible_tabs();
        let current = tabs.iter().position(|&i| i == self.active).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(tabs.len() as isize) as usize;
        self.select(tabs[next]);
    }

    /// The harnesses a prompt to all of them targets until toggled: all but
    /// the disabled ones that have not been started.
    fn default_targets(&self) -> Vec<usize> {
        (0..self.panes.len())
            .filter(|&i| self.panes[i].status() != Status::Stopped)
            .collect()
    }

    fn open_composer(&mut self, all: bool) {
        let targets: Vec<usize> = if all || self.on_summary() {
            self.default_targets()
        } else {
            vec![self.active]
        };
        self.composer.open(&targets);
        self.mode = Mode::Compose;
    }

    fn restart_active(&mut self) {
        let name = self.panes[self.active].name().to_string();
        if self.start(self.active) {
            self.flash(format!("started {name}"));
            return;
        }
        let harness = self.panes[self.active].harness.clone();
        crate::pane::shutdown(
            std::slice::from_mut(&mut self.panes[self.active]),
            SHUTDOWN_GRACE,
        );
        self.panes[self.active] = self.spawn_pane(harness);
        self.flash(format!("restarted {name}"));
    }

    /// Starts a disabled harness that has not run yet. Returns whether it did.
    fn start(&mut self, index: usize) -> bool {
        if self.panes[index].status() != Status::Stopped {
            return false;
        }
        let harness = self.panes[index].harness.clone();
        self.panes[index] = self.spawn_pane(harness);
        true
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
            self.select_position(c as usize - '1' as usize);
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
            KeyCode::Char(c @ '1'..='9') => self.select_position(c as usize - '1' as usize),
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
        let width = self
            .prompt_widths
            .borrow()
            .get(&PromptBox::Composer)
            .copied();
        if move_visual(&mut self.composer, width, &key) {
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

    fn model_name(&self, pane: usize) -> Option<String> {
        let harness = &self.panes[pane].harness;
        if let Some(model) = &harness.model {
            return Some(model.clone());
        }
        crate::config::detect_model(
            self.model_patterns.get(pane)?,
            &self.panes[pane].screen_text(),
        )
        .map(|(model, _)| model)
    }

    fn send_to(&mut self, pane: usize, text: &str, delay: Duration) {
        let mut text = text.to_string();
        if self.config.model_id_prefix
            && !self.panes[pane].introduced
            && let Some(model) = self.model_name(pane)
        {
            let model_id = model.split_whitespace().collect::<Vec<_>>().join("-");
            text = format!("MODEL_ID={model_id}\n\n{text}");
            self.panes[pane].introduced = true;
        }
        self.mark_delivered(pane);
        self.panes[pane].send_prompt(&text, delay);
    }

    fn submit_prompt(&mut self, text: &str, targets: &[usize]) {
        let delay = Duration::from_millis(self.config.submit_delay_ms);
        let mut sent = Vec::new();
        let mut queued = Vec::new();
        for &i in targets {
            self.start(i);
        }
        if targets.len() >= 2 {
            self.start_round(text, targets);
        }
        for &i in targets {
            if self.readiness(i) == Readiness::Ready {
                self.send_to(i, text, delay);
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

    fn target_hit(&self, column: u16, row: u16) -> Option<TargetHit> {
        let kind = match self.mode {
            Mode::Compose => PromptBox::Composer,
            Mode::Normal if self.on_summary() => PromptBox::Summary,
            _ => return None,
        };
        self.target_hits
            .borrow()
            .iter()
            .rev()
            .find(|h| h.kind == kind && h.y == row && column >= h.x0 && column < h.x1)
            .copied()
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let left_down = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        if left_down
            && mouse.row == 0
            && mouse.column >= self.exit_span.0
            && mouse.column < self.exit_span.1
        {
            if self.mode == Mode::ConfirmQuit {
                self.quit = true;
            } else {
                self.mode = Mode::ConfirmQuit;
            }
            return;
        }
        if left_down && let Some(hit) = self.target_hit(mouse.column, mouse.row) {
            match hit.kind {
                PromptBox::Composer => self.composer.toggle(hit.target),
                PromptBox::Summary => self.summary_input.toggle(hit.target),
            }
            return;
        }
        if mouse.row == 0 {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
                && let Some(i) = self
                    .tab_spans
                    .iter()
                    .position(|&(a, b)| mouse.column >= a && mouse.column < b)
            {
                self.select_position(i);
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
            Status::Running if self.shows_trust_dialog(index) => ('⚑', Color::Yellow),
            Status::Running if self.has_pending(index) => ('⧗', Color::Yellow),
            Status::Running if self.needs_input(index) => ('?', Color::Magenta),
            Status::Running if pane.is_busy(BUSY_WINDOW) || self.still_working(index) => {
                ('●', Color::Green)
            }
            Status::Running => (' ', Color::Reset),
            Status::Stopped => ('○', Color::DarkGray),
            Status::Exited(_) => ('✗', Color::Red),
            Status::Failed(_) => ('!', Color::Red),
        };
        let text = format!("{marker}{}", pane.name());
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
        for (position, index) in self.visible_tabs().into_iter().enumerate() {
            let (text, style) = if index == self.summary_tab() {
                self.summary_tab_label()
            } else {
                self.tab_label(index)
            };
            if position > 0 {
                spans.push(Span::raw(" │ ").dark_gray());
                x += 3;
            }
            let width = text.width() as u16;
            self.tab_spans.push((x, x + width));
            x += width;
            spans.push(Span::styled(text, style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), tabs_area);
        let exit = " ✕ exit ";
        let exit_width = exit.width() as u16;
        let exit_x = tabs_area.right().saturating_sub(exit_width);
        if self.config.yolo {
            let badge = " YOLO ";
            let badge_width = badge.width() as u16;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    badge,
                    Style::default().white().on_red().bold(),
                )),
                Rect::new(
                    exit_x.saturating_sub(badge_width + 1),
                    tabs_area.y,
                    badge_width,
                    1,
                ),
            );
        }
        self.exit_span = (exit_x, tabs_area.right());
        let exit_style = if self.mode == Mode::ConfirmQuit {
            Style::default().white().on_red().bold()
        } else {
            Style::default().red()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(exit, exit_style)),
            Rect::new(exit_x, tabs_area.y, exit_width.min(tabs_area.width), 1),
        );

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
        } else if pane.status() == Status::Stopped {
            let text = format!(
                "{} is disabled (enabled = false), so it was not started.\n\n\
                 Ctrl+\\ r starts it, as does checking it in a prompt box.",
                pane.name()
            );
            frame.render_widget(
                Paragraph::new(text).dark_gray().wrap(Wrap { trim: false }),
                pane_area,
            );
            None
        } else {
            pane.restore_selection();
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
            let mut spans = vec![Span::raw(" summary ").black().on_magenta()];
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
                Span::raw(" Stop all harnesses and quit? y/N, or click ✕ exit again"),
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
        let width = area.width.saturating_sub(2).max(1) as usize;
        let (rows, _) = layout_prompt(composer.text(), composer.cursor_line_col(), width);
        let max_height = (area.height / 2).max(3);
        (rows.len() as u16 + 2).clamp(3, max_height)
    }

    fn draw_composer(&self, frame: &mut Frame, pane_area: Rect) -> Option<Position> {
        let height = Self::prompt_box_height(&self.composer, pane_area);
        let area = Rect::new(
            pane_area.x,
            pane_area.bottom().saturating_sub(height),
            pane_area.width,
            height,
        );
        self.draw_prompt_box(
            frame,
            area,
            &self.composer,
            " Prompt → ",
            PromptBox::Composer,
        )
    }

    fn draw_prompt_box(
        &self,
        frame: &mut Frame,
        area: Rect,
        composer: &Composer,
        heading: &'static str,
        kind: PromptBox,
    ) -> Option<Position> {
        let mut title = vec![Span::raw(heading)];
        let mut x = area.x + 1 + heading.width() as u16;
        let mut hits = self.target_hits.borrow_mut();
        hits.retain(|h| h.kind != kind);
        for (i, pane) in self.panes.iter().enumerate() {
            let on = composer.targets[i];
            let label = format!("[{}] {} ", if on { "x" } else { " " }, pane.name());
            let width = label.width() as u16;
            hits.push(TargetHit {
                kind,
                y: area.y,
                x0: x,
                x1: x + width,
                target: i,
            });
            x += width;
            title.push(if on {
                Span::raw(label).cyan().bold()
            } else {
                Span::raw(label).dark_gray()
            });
        }
        drop(hits);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().cyan())
            .title(Line::from(title));
        let inner = block.inner(area);
        let width = inner.width.max(1) as usize;
        self.prompt_widths.borrow_mut().insert(kind, width);
        let (rows, (cursor_row, cursor_x)) =
            layout_prompt(composer.text(), composer.cursor_line_col(), width);
        let visible = (inner.height as usize).max(1);
        let first = cursor_row.saturating_sub(visible - 1);
        let shown: Vec<Line> = rows
            .iter()
            .skip(first)
            .take(visible)
            .map(|row| Line::raw(row.clone()))
            .collect();

        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(shown).block(block), area);

        let x = (inner.x + cursor_x as u16).min(inner.right().saturating_sub(1));
        let y = inner.y + (cursor_row - first) as u16;
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
                "  r (summary)  summarize the latest answers now (also Ctrl+R in the summary tab)",
            ),
            Line::from("  q            quit (stops all harnesses)"),
            Line::from("  Ctrl+\\       send a literal Ctrl+\\ to the harness"),
            Line::from(""),
            Line::from("Mouse: drag with the left button to select text; it's copied on release."),
            Line::from("Click a [x] target in a prompt box to toggle it; click ✕ exit to quit."),
            Line::from(""),
            Line::from("Prompt composer:").bold(),
            Line::from("  Enter send · Alt+Enter / Ctrl+J newline · Alt+1-9 toggle target"),
            Line::from("  Ctrl+A toggle all · ↑/↓ history · Esc cancel"),
            Line::from(""),
            Line::from(
                "Tab markers: ● output · ⚑ trust the folder · ⧗ prompt queued · ? waiting for your answer · ✗ exited",
            ),
            Line::from("Summary tab: a prompt sent to 2+ harnesses is compared once all go quiet."),
            Line::from(
                "Type in the summary tab's prompt box and press Enter to send to all harnesses.",
            ),
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
        if let Some(slot) = round.slot(pane) {
            if round.delivered[slot].is_some() {
                round.followed_up[slot] = true;
            } else if round.state() == SummaryState::Waiting {
                round.delivered[slot] = Some((Instant::now(), mark));
            }
        }
    }

    fn capture(&self, pane: usize, mark: usize, finished: bool) -> Capture {
        let lines = self.panes[pane].styled_since(mark, self.config.summary.max_lines);
        let text = lines
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let text = if self.panes[pane].harness.strips_right_panel() {
            crate::summary::strip_right_panel(&text)
        } else {
            text
        };
        // Stripping a side panel keeps one line per line, so cut each line's
        // styles to its stripped text.
        let styles = text
            .lines()
            .zip(lines)
            .map(|(line, (_, mut styles))| {
                styles.truncate(line.chars().count());
                (line.to_string(), styles)
            })
            .collect();
        Capture {
            text,
            finished,
            model: self.model_of(pane),
            at: Instant::now(),
            styles,
        }
    }

    fn log_capture(&self, index: usize, slot: usize, capture: &Capture) {
        let Some(dir) = &self.log_dir else { return };
        let round = &self.rounds[index];
        let dir = dir.join("captures");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let state = if capture.finished {
            "finished"
        } else {
            "unfinished"
        };
        let file = dir.join(format!(
            "{}-{}-{}.txt",
            round.started_at, round.names[slot], state
        ));
        let _ = std::fs::write(file, &capture.text);
    }

    fn model_of(&self, pane: usize) -> Option<String> {
        let harness = &self.panes[pane].harness;
        let detected = match &harness.model {
            Some(_) => None,
            None => crate::config::detect_model(
                self.model_patterns.get(pane)?,
                &self.panes[pane].screen_text(),
            ),
        };
        harness.model_label(detected)
    }

    pub fn tick_summary(&mut self) -> bool {
        let Some(index) = self.rounds.len().checked_sub(1) else {
            return false;
        };
        let state = self.rounds[index].state();
        if state == SummaryState::Superseded {
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
                changed |= self.reopen_if_resumed(index, slot);
                continue;
            }
            let pane = self.rounds[index].targets[slot];
            let running = matches!(self.panes[pane].status(), Status::Running);
            let capture = match self.rounds[index].delivered[slot] {
                Some((at, mark)) => {
                    let quiet_now = at.elapsed() >= quiet
                        && !self.panes[pane].is_busy(quiet)
                        && !self.needs_input(pane)
                        && !self.still_working(pane);
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
            if let Some(capture) = capture {
                self.log_capture(index, slot, &capture);
                self.store_capture(index, slot, capture);
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

    /// Stores a capture. A reopened harness whose answer changed gets its
    /// round compared again once every answer is in.
    fn store_capture(&mut self, index: usize, slot: usize, capture: Capture) {
        let round = &mut self.rounds[index];
        let finished = capture.finished;
        round.captures[slot] = Some(capture);
        if !finished {
            return;
        }
        let Some(before) = round.reopened[slot].take() else {
            return;
        };
        let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let answer = round.answer(slot).unwrap_or_default();
        if words(&answer) != words(&before) && round.state() != SummaryState::Waiting {
            round.compared_unfinished = true;
        }
    }

    /// A harness counted as done that produces output again, without another
    /// prompt, was not done after all (agy, for one, goes quiet while a
    /// command runs). Watch it again; a changed answer is compared again.
    fn reopen_if_resumed(&mut self, index: usize, slot: usize) -> bool {
        let round = &self.rounds[index];
        let pane = &self.panes[round.targets[slot]];
        let Some(capture) = round.captures[slot].as_ref() else {
            return false;
        };
        let resumed = !round.followed_up[slot]
            && pane.status() == Status::Running
            && pane.last_output().is_some_and(|t| t > capture.at);
        if !resumed {
            return false;
        }
        let before = round.answer(slot).unwrap_or_default();
        let round = &mut self.rounds[index];
        round.reopened[slot] = Some(before);
        if let Some(capture) = round.captures[slot].as_mut() {
            capture.finished = false;
        }
        true
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
            KeyCode::Char('l') if ctrl && sections > 0 => {
                self.refresh_section(self.summary_focus.min(sections - 1));
            }
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
            KeyCode::Up | KeyCode::Down
                if move_visual(
                    &mut self.summary_input,
                    self.prompt_widths
                        .borrow()
                        .get(&PromptBox::Summary)
                        .copied(),
                    &key,
                ) => {}
            _ => match self.summary_input.handle_key(&key) {
                ComposerAction::None => {}
                ComposerAction::Cancel => self.summary_input.clear(),
                ComposerAction::Submit { text, targets } => {
                    self.submit_prompt(&text, &targets);
                    self.summary_scroll = 0;
                }
            },
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
            Some(SummaryState::Waiting) => ('⧗', Color::Yellow),
            Some(SummaryState::Running { .. }) => ('…', Color::Cyan),
            Some(SummaryState::Done { .. }) if self.summary_unseen => ('●', Color::Green),
            Some(SummaryState::Failed(_)) => ('!', Color::Red),
            _ => (' ', Color::Reset),
        };
        let text = format!("{marker}summary");
        let mut style = Style::default().fg(color);
        if self.on_summary() {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        (text, style)
    }

    fn agent_label(&self, round: &Round, slot: usize) -> String {
        let model = round.captures[slot]
            .as_ref()
            .and_then(|c| c.model.clone())
            .or_else(|| self.model_of(round.targets[slot]));
        match model {
            Some(model) => format!("{} · {model}", round.names[slot]),
            None => round.names[slot].clone(),
        }
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
            None if round.delivered[slot].is_none() && self.shows_trust_dialog(pane) => {
                ("waiting for you to trust the folder".into(), Color::Yellow)
            }
            None if round.delivered[slot].is_none() => ("prompt queued".into(), Color::Yellow),
            None if self.needs_input(pane) => ("waiting for your answer".into(), Color::Magenta),
            None if self.panes[pane]
                .is_busy(Duration::from_secs(self.config.summary.quiet_secs)) =>
            {
                ("working".into(), Color::Green)
            }
            None if self.still_working(pane) => ("working in the background".into(), Color::Green),
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
        let mut name = Span::raw(label).bold();
        if index == self.summary_focus {
            name = name.reversed();
        }
        // The arrow and the refresh button sit in fixed columns so a click can
        // tell them apart (see `summary_click`).
        let mut spans = vec![
            Span::raw(if folded { "▸ " } else { "▾ " }).bold(),
            Span::raw(REFRESH_BUTTON).cyan(),
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
                Span::raw(
                    "   click or Ctrl+O to fold · ⟳ or Ctrl+L refresh · Tab focus · Ctrl+E fold all",
                )
                .dark_gray(),
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
                    self.agent_label(round, slot),
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
                    round
                        .answer_lines(slot)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|cells| (styled_line("    ", cells), None)),
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
            self.section_header(overall, "comparison".into(), status, headline),
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
        let input = self.summary_input.clone();
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
        self.draw_prompt_box(frame, input_area, &input, " Prompt → ", PromptBox::Summary)
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
        let Some(Some(section)) = self.summary_hits.get(index).copied() else {
            return;
        };
        let first_row = index == 0 || self.summary_hits[index - 1] != Some(section);
        let x = (column - area.x) as usize;
        if first_row && (2..2 + REFRESH_BUTTON.trim_end().width()).contains(&x) {
            self.refresh_section(section);
        } else {
            self.toggle_section(section);
        }
    }

    /// Captures a harness's answer again now, as finished; for the comparison
    /// section, compares again. A changed answer is compared again.
    fn refresh_section(&mut self, section: usize) {
        let Some(index) = self.rounds.len().checked_sub(1) else {
            return;
        };
        let index = self.summary_view.min(index);
        let round = &self.rounds[index];
        if section >= round.targets.len() {
            self.regenerate_summary();
            return;
        }
        let pane = round.targets[section];
        let name = self.panes[pane].name().to_string();
        let Some((_, mark)) = round.delivered[section] else {
            self.flash(format!("the prompt has not reached {name} yet"));
            return;
        };
        let before = round.answer(section);
        let capture = self.capture(pane, mark, true);
        self.log_capture(index, section, &capture);
        self.rounds[index].reopened[section] = before;
        self.store_capture(index, section, capture);
        let round = &self.rounds[index];
        let rerun = round.compared_unfinished
            && round.all_finished()
            && matches!(
                round.state(),
                SummaryState::Done { .. } | SummaryState::Failed(_)
            );
        if rerun {
            self.start_summarizer(index);
            self.flash(format!("refreshed {name}; comparing again"));
        } else {
            self.flash(format!("refreshed {name}"));
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
            ..Harness::default()
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
            model_id_prefix: true,
            yolo: false,
            dialog_patterns: crate::config::default_dialog_patterns(),
            input_patterns: crate::config::default_input_patterns(),
            working_patterns: crate::config::default_working_patterns(),
            summary: test_summary(),
            harnesses,
        };
        let mut app = App::new(
            config,
            PathBuf::from("."),
            None,
            Size { cols: 60, rows: 12 },
        )
        .unwrap();
        app.select(0);
        app
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
        assert_eq!(app.tab_label(1).0, "?asker");
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
        assert!(text.contains("▸ ⟳ a  ✓ done  — • answer from a"), "{text}");
        assert!(!text.contains("    • answer from a"), "{text}");
        assert!(text.contains("▾ ⟳ comparison"), "{text}");
        assert_eq!(app.rounds[0].answer(0).unwrap(), "• answer from a");

        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        app.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let text = summary_text(&app);
        assert!(
            text.contains("▾ ⟳ a  ✓ done\n    • answer from a") && !text.contains("Ran ls"),
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
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        let buf = draw_app(&mut app, 60, 20);
        let rows = buffer_rows(&buf);
        assert!(
            rows[1].starts_with("Round 1 of 2") || rows[1].starts_with("Round 1 of 1"),
            "{rows:#?}"
        );
        let header = rows
            .iter()
            .position(|r| r.starts_with("▸ ⟳ b"))
            .expect("b header");
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: header as u16,
            modifiers: KeyModifiers::NONE,
        }));
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            5,
            header as u16,
        );
        assert!(
            summary_text(&app).contains("▾ ⟳ b"),
            "{}",
            summary_text(&app)
        );
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
    fn dragging_in_scrollback_survives_a_harness_redrawing_its_screen() {
        // An Ink app (cursor-agent) redraws its live area by erasing every line
        // and writing it again, which drops any selection that touches it.
        let script = "for i in $(seq 1 40); do printf 'line %s\\r\\n' $i; done; \
            while true; do for j in $(seq 1 9); do printf '\\033[2K\\033[1A'; done; \
            printf '\\033[2K\\033[G'; for j in $(seq 1 9); do printf 'live %s\\r\\n' $j; done; \
            printf 'footer %s' $RANDOM; sleep 0.02; done";
        let mut app = app_with(vec![script_harness("ink", script)]);
        wait_for(app.pane(0), "footer");
        for _ in 0..3 {
            mouse(&mut app, MouseEventKind::ScrollUp, 5, 5);
        }
        assert_eq!(app.mode(), Mode::Scroll);
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 2);
        thread::sleep(Duration::from_millis(100));
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 3, 11);
        thread::sleep(Duration::from_millis(100));
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 3, 11);
        let copied = app.clipboard.last.clone().unwrap_or_default();
        assert!(copied.starts_with("line "), "{copied:?}");
        // The last selected lines may be mid-redraw when the button comes up.
        assert!(copied.lines().count() >= 6, "{copied:?}");
    }

    #[test]
    fn dragging_in_summary_copies_and_click_still_folds() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "copy test");
        wait_summary(&mut app, "SUMMARY:2");
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
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
        let header = rows.iter().position(|r| r.starts_with("▸ ⟳ a")).unwrap() as u16;
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 1, header);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 1, header);
        assert!(summary_text(&app).contains("▾ ⟳ a"));
    }

    fn click(app: &mut App, column: u16, row: u16) {
        mouse(app, MouseEventKind::Down(MouseButton::Left), column, row);
        mouse(app, MouseEventKind::Up(MouseButton::Left), column, row);
    }

    fn find(rows: &[String], needle: &str) -> (u16, u16) {
        rows.iter()
            .enumerate()
            .find_map(|(y, r)| {
                r.find(needle)
                    .map(|byte| (r[..byte].chars().count() as u16, y as u16))
            })
            .unwrap_or_else(|| panic!("{needle:?} not on screen: {rows:#?}"))
    }

    #[test]
    fn clicking_a_target_in_the_summary_prompt_box_toggles_it() {
        let mut app = app(&["a", "b", "c"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        let (x, y) = find(&rows, "[x] b");
        click(&mut app, x + 1, y);
        assert_eq!(app.summary_input.selected(), [0, 2]);
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        assert!(rows.iter().any(|r| r.contains("[ ] b")), "{rows:#?}");
        type_text(&mut app, "only a and c");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        wait_for(app.pane(0), "got:only a and c");
        wait_for(app.pane(2), "got:only a and c");
        thread::sleep(Duration::from_millis(200));
        assert!(!screen(app.pane(1)).contains("only a and c"));
        assert_eq!(
            app.summary_input.selected(),
            [0, 2],
            "selection is kept for the next prompt"
        );
    }

    #[test]
    fn clicking_a_target_in_the_composer_toggles_it() {
        let mut app = app(&["a", "b"]);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE));
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        let (x, y) = find(&rows, "[x] a");
        click(&mut app, x, y);
        assert_eq!(app.composer.selected(), [1]);
        assert_eq!(app.mode(), Mode::Compose);
    }

    #[test]
    fn yolo_starts_harnesses_with_their_flag_and_shows_a_badge() {
        let mut config = Config {
            submit_delay_ms: 30,
            scrollback: 100,
            mouse: true,
            osc52: false,
            system_clipboard: false,
            model_id_prefix: true,
            yolo: true,
            dialog_patterns: crate::config::default_dialog_patterns(),
            input_patterns: crate::config::default_input_patterns(),
            working_patterns: crate::config::default_working_patterns(),
            summary: test_summary(),
            harnesses: vec![script_harness("p", "printf 'args:%s' \"$*\"; sleep 5")],
        };
        config.harnesses[0].yolo_args = Some(vec!["--approve".into()]);
        config.harnesses[0].args.push("--".into());
        let mut app = App::new(
            config,
            PathBuf::from("."),
            None,
            Size { cols: 60, rows: 12 },
        )
        .unwrap();
        wait_for(app.pane(0), "args:");
        assert!(
            screen(app.pane(0)).contains("--approve"),
            "{}",
            screen(app.pane(0))
        );
        let rows = buffer_rows(&draw_app(&mut app, 80, 10));
        assert!(
            rows[0].contains("YOLO") && rows[0].contains("✕ exit"),
            "{rows:#?}"
        );
    }

    #[test]
    fn exit_button_asks_first_then_quits() {
        let mut app = app(&["a"]);
        let rows = buffer_rows(&draw_app(&mut app, 80, 10));
        let (x, y) = find(&rows, "✕ exit");
        assert_eq!(y, 0);
        assert!(x > 60, "exit is right-aligned: {x}");
        click(&mut app, x + 2, 0);
        assert_eq!(app.mode(), Mode::ConfirmQuit);
        assert!(!app.quit);
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.mode(), Mode::Normal);
        click(&mut app, x + 2, 0);
        draw_app(&mut app, 80, 10);
        click(&mut app, x + 2, 0);
        assert!(app.quit);
    }

    #[test]
    fn tab_labels_keep_their_width_when_a_marker_appears() {
        let mut app = app_with(vec![
            script_harness("quiet", "sleep 30"),
            script_harness("busy", "while true; do echo x; sleep 0.2; done"),
        ]);
        wait_for(app.pane(1), "x");
        let quiet = app.tab_label(0).0;
        let busy = app.tab_label(1).0;
        assert_eq!(quiet, " quiet");
        assert_eq!(busy, "●busy");
        assert_eq!(quiet.width(), "quiet".width() + 1);
        assert_eq!(busy.width(), "busy".width() + 1);
        let summary = app.summary_tab_label().0;
        assert_eq!(summary, " summary");
        compose_all(&mut app, "q");
        assert_eq!(app.summary_tab_label().0.width(), summary.width());
    }

    #[test]
    fn long_prompts_wrap_and_keep_the_cursor_visible() {
        let text = "alpha beta gamma delta";
        let (rows, cursor) = layout_prompt(text, (0, text.chars().count()), 10);
        assert_eq!(rows, ["alpha ", "beta ", "gamma ", "delta"]);
        assert_eq!(cursor, (3, 5));
        assert_eq!(layout_prompt(text, (0, 7), 10).1, (1, 1));
        assert_eq!(layout_prompt(text, (0, 6), 10).1, (1, 0));
        let (rows, cursor) = layout_prompt("abcdefghij", (0, 10), 10);
        assert_eq!(rows, ["abcdefghij", ""]);
        assert_eq!(cursor, (1, 0));
        let (rows, cursor) = layout_prompt("one\ntwo three four", (1, 3), 6);
        assert_eq!(rows, ["one", "two ", "three ", "four"]);
        assert_eq!(cursor, (1, 3));
        assert_eq!(layout_prompt("", (0, 0), 5), (vec![String::new()], (0, 0)));
    }

    #[test]
    fn up_and_down_move_between_wrapped_rows() {
        let mut composer = Composer::new(1);
        composer.insert_str("alpha beta gamma delta");
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(move_visual(&mut composer, Some(10), &up));
        assert_eq!(composer.cursor_line_col(), (0, 16));
        assert!(move_visual(&mut composer, Some(10), &up));
        assert_eq!(composer.cursor_line_col(), (0, 10));
        assert!(move_visual(&mut composer, Some(10), &down));
        assert_eq!(composer.cursor_line_col(), (0, 15));
        composer.set_cursor_line_col(0, 0);
        assert!(!move_visual(&mut composer, Some(10), &up));
        assert!(!move_visual(&mut composer, None, &up));
        assert_eq!(composer.text(), "alpha beta gamma delta");
    }

    #[test]
    fn prompt_box_shows_wrapped_long_text() {
        let mut app = app(&["a", "b"]);
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        let long = "word ".repeat(40);
        app.handle_event(Event::Paste(long.trim_end().to_string()));
        let rows = buffer_rows(&draw_app(&mut app, 40, 30));
        let boxed: Vec<&String> = rows.iter().filter(|r| r.starts_with("│word")).collect();
        assert!(boxed.len() >= 5, "{rows:#?}");
        assert!(boxed.iter().all(|r| r.ends_with('│')), "{rows:#?}");
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
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
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
            summary_text(&app).contains("▸ ⟳ slow  ✓ done  — • final answer"),
            "{}",
            summary_text(&app)
        );
    }

    #[test]
    fn done_harness_that_resumes_is_recaptured_and_compared_again() {
        let pause = "printf '\\033[?2004h'; stty -echo; read l; printf '› %s\\r\\n• launched the tests\\r\\n' \"$l\"; sleep 2.5; printf '• final answer\\r\\n› Ask me\\r\\n'; sleep 30";
        let mut app = app_with(vec![
            replying_harness("fast"),
            script_harness("pause", pause),
        ]);
        app.config.summary.args = vec![
            "-c".into(),
            "printf 'FINAL:'; grep -c 'final answer'".into(),
        ];
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "run the tests");
        app.select(app.summary_tab());
        wait_summary(&mut app, "FINAL:0");
        assert!(
            app.rounds[0]
                .answer(1)
                .unwrap()
                .ends_with("launched the tests")
        );
        wait_summary(&mut app, "FINAL:1");
        assert!(app.rounds[0].all_finished());
        assert!(app.rounds[0].answer(1).unwrap().ends_with("• final answer"));
    }

    #[test]
    fn refresh_button_recaptures_one_harness_and_compares_again() {
        // `late` goes quiet long enough to count as done, then prints more
        // output that is too quick to be noticed before the refresh.
        let late = "printf '\\033[?2004h'; stty -echo; read l; printf '› %s\\r\\n• first take\\r\\n' \"$l\"; read x; printf '• better take\\r\\n'; sleep 30";
        let mut app = app_with(vec![replying_harness("fast"), script_harness("late", late)]);
        app.config.summary.args = vec![
            "-c".into(),
            "printf 'BETTER:'; grep -c 'better take'".into(),
        ];
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "go");
        app.select(app.summary_tab());
        wait_summary(&mut app, "BETTER:0");
        // Make the round's answer change without the harness being reopened.
        app.rounds[0].followed_up[1] = true;
        app.pane(1).write(b"\r");
        wait_for(app.pane(1), "better take");
        let rows = buffer_rows(&draw_app(&mut app, 80, 30));
        let header = rows.iter().position(|r| r.starts_with("▸ ⟳ late")).unwrap() as u16;
        click(&mut app, 2, header);
        assert!(app.rounds[0].answer(1).unwrap().ends_with("• better take"));
        wait_summary(&mut app, "BETTER:1");

        app.summary_focus = 0;
        app.handle_event(key(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(app.rounds[0].captures[0].as_ref().unwrap().finished);
    }

    #[test]
    fn summary_answer_keeps_the_harness_colors() {
        let colored = "printf '\\033[?2004h'; stty -echo; read l; printf '› %s\\r\\n• plain \\033[31mred\\033[0m\\r\\n' \"$l\"; sleep 30";
        let mut app = app_with(vec![replying_harness("a"), script_harness("c", colored)]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "colors");
        app.select(app.summary_tab());
        wait_summary(&mut app, "SUMMARY:2");
        let toggle = app.summary_folded.get(1).copied().unwrap_or(false);
        if toggle {
            app.toggle_section(1);
        }
        let buf = draw_app(&mut app, 80, 30);
        let rows = buffer_rows(&buf);
        let (x, y) = find(&rows, "• plain red");
        let red = (x + "• plain ".chars().count() as u16, y);
        assert_eq!(buf[red].fg, Color::Indexed(1), "{rows:#?}");
        assert_eq!(buf[(x + 2, y)].fg, Color::Reset);
    }

    #[test]
    fn a_follow_up_prompt_does_not_reopen_the_round() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "first");
        wait_summary(&mut app, "SUMMARY:2");
        app.select(0);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('s'), KeyModifiers::NONE));
        type_text(&mut app, "second");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        wait_for(app.pane(0), "› second");
        thread::sleep(Duration::from_millis(300));
        app.tick_summary();
        assert!(app.rounds[0].followed_up[0]);
        assert!(app.rounds[0].all_finished());
    }

    #[test]
    fn first_prompt_to_each_harness_starts_with_model_id() {
        let mut known = cat_harness("known");
        known.model = Some("Test Model high".into());
        let mut app = app_with(vec![known, cat_harness("unknown")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "first");
        wait_for(app.pane(0), "got:first");
        wait_for(app.pane(1), "got:first");
        assert!(
            screen(app.pane(0)).contains("got:MODEL_ID=Test-Model-high"),
            "{}",
            screen(app.pane(0))
        );
        assert!(
            !screen(app.pane(1)).contains("MODEL_ID"),
            "{}",
            screen(app.pane(1))
        );
        compose_all(&mut app, "second");
        wait_for(app.pane(0), "got:second");
        assert_eq!(screen(app.pane(0)).matches("MODEL_ID").count(), 1);
        assert_eq!(app.rounds.last().unwrap().prompt, "second");
    }

    #[test]
    fn background_work_keeps_a_harness_from_being_done() {
        let script = format!(
            "printf '\\033[?2004h'; read l; printf '• partial\\r\\n✻ done · 1 shell, 1 monitor still running\\r\\n'; sleep 2.5; printf '\\033[2J\\033[H› %s\\r\\n• final answer\\r\\n────────────────\\r\\n› Ask me\\r\\n' \"$l\"; {ECHO_LOOP}"
        );
        let mut app = app_with(vec![cat_harness("a"), script_harness("bg", &script)]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "go");
        wait_for(app.pane(1), "still running");
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(2000) {
            app.tick_summary();
            assert!(
                app.rounds[0].captures[1].is_none(),
                "captured while background work was running"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            summary_text(&app).contains("working in the background"),
            "{}",
            summary_text(&app)
        );
        assert_eq!(app.tab_label(1).0, "●bg");
        wait_summary(&mut app, "SUMMARY:2");
        assert_eq!(app.rounds[0].answer(1).as_deref(), Some("• final answer"));
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
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(app.on_summary());
        assert!(summary_text(&app).contains("No summaries yet"));
        app.handle_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        wait_summary(&mut app, "SUMMARY:2");
    }

    #[test]
    fn typing_in_summary_tab_sends_to_all_harnesses() {
        let mut app = app(&["a", "b", "c"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(app.on_summary());
        type_text(&mut app, "ready");
        app.handle_event(Event::Paste(" set".into()));
        assert_eq!(app.summary_input.text(), "ready set");
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        assert!(!app.on_summary(), "Alt+digit still switches tabs");
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
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
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        type_text(&mut app, "rpn");
        assert_eq!(app.summary_input.text(), "rpn");
        assert!(app.rounds.is_empty());
        app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.summary_input.text(), "");
        app.handle_event(key(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.summary_scroll > 0);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::NONE));
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
    fn trust_dialog_is_marked_on_the_tab_not_in_the_prompt_box() {
        let dialog = "printf 'Do you trust the contents of this project?'; sleep 5";
        let mut app = app_with(vec![script_harness("asks", dialog), cat_harness("ready")]);
        wait_for(app.pane(0), "Do you trust");
        assert_eq!(app.tab_label(0).0, "⚑asks");
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        assert!(
            rows.iter().any(|r| r.contains("[x] asks [x] ready")),
            "{rows:#?}"
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("dialog open") || r.contains("(starting)")),
            "{rows:#?}"
        );
        compose_all(&mut app, "hi");
        assert_eq!(app.tab_label(0).0, "⚑asks");
        assert!(
            summary_text(&app).contains("asks  waiting for you to trust the folder"),
            "{}",
            summary_text(&app)
        );
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
            model_id_prefix: true,
            yolo: false,
            dialog_patterns: crate::config::default_dialog_patterns(),
            input_patterns: crate::config::default_input_patterns(),
            working_patterns: crate::config::default_working_patterns(),
            summary: test_summary(),
            harnesses: names.iter().map(|n| cat_harness(n)).collect(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("."),
            None,
            Size { cols: 60, rows: 12 },
        )
        .unwrap();
        // Most tests type into the first harness, so start there rather than
        // on the summary tab.
        app.select(0);
        app
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
        assert_eq!(app.active(), 1, "tab 1 is the summary, tab 3 is harness b");
        app.handle_event(key(KeyCode::Char('4'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.active(), 2);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(
            app.on_summary(),
            "next after the last harness wraps to the summary"
        );
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.active(), 0);
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(app.on_summary());
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.active(), 0);
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn disabled_harnesses_are_hidden_until_checked_in_a_prompt_box() {
        let mut harnesses = vec![cat_harness("a"), cat_harness("b"), cat_harness("c")];
        harnesses[1].enabled = Some(false);
        harnesses[2].enabled = Some(false);
        let mut app = app_with(harnesses);
        assert_eq!(app.pane(1).status(), Status::Stopped);
        assert_eq!(app.visible_tabs(), [3, 0]);
        assert_eq!(app.summary_input.selected(), [0]);
        app.select(3);
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        assert!(
            !rows[0].contains(" b ") && !rows[0].contains('c'),
            "{:?}",
            rows[0]
        );
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        assert!(app.on_summary(), "Alt+3 is not a hidden tab");

        let (x, y) = find(&rows, "[ ] c");
        click(&mut app, x + 1, y);
        assert_eq!(
            app.pane(2).status(),
            Status::Running,
            "checking it starts it"
        );
        assert_eq!(app.visible_tabs(), [3, 0, 2]);
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        assert!(rows[0].contains('c'), "{:?}", rows[0]);

        app.select(0);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.composer.selected(), [0, 2]);
        assert_eq!(app.pane(1).status(), Status::Stopped);
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.pane(1).status(), Status::Running);
        type_text(&mut app, "hello");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        wait_until(&mut app, |a| !a.has_pending(1));
        wait_for(app.pane(1), "got:hello");
        assert_eq!(app.visible_tabs(), [3, 0, 1, 2]);
    }

    #[test]
    fn summary_is_the_first_tab_and_the_starting_one() {
        let mut app = app(&["a", "b"]);
        let rows = buffer_rows(&draw_app(&mut app, 80, 10));
        let summary = rows[0].find("summary").unwrap();
        assert!(summary < rows[0].find('a').unwrap(), "{:?}", rows[0]);
        click(&mut app, summary as u16 + 1, 0);
        assert!(app.on_summary());

        let config = Config {
            harnesses: vec![cat_harness("a")],
            ..app.config.clone()
        };
        let fresh = App::new(
            config,
            PathBuf::from("."),
            None,
            Size { cols: 60, rows: 12 },
        )
        .unwrap();
        assert!(fresh.on_summary());
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
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
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
            model_id_prefix: true,
            yolo: false,
            dialog_patterns: crate::config::default_dialog_patterns(),
            input_patterns: crate::config::default_input_patterns(),
            working_patterns: crate::config::default_working_patterns(),
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
