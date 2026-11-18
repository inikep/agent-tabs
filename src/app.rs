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
use crate::rounds::{Capture, Round, SummaryState, run_summarizer};
use crate::script::{Command, Step, Targets};

const BUSY_WINDOW: Duration = Duration::from_millis(1500);
/// How long a harness must be quiet before it gets an `[[auto_reply]]`.
const AUTO_REPLY_QUIET: Duration = Duration::from_secs(2);
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
    main_input: Composer,
    /// Harnesses unchecked in the main prompt box: their tabs are hidden,
    /// though they keep running, until checked again or sent a prompt.
    hidden: Vec<bool>,
    config: Config,
    cwd: PathBuf,
    log_dir: Option<PathBuf>,
    dirty: Arc<AtomicBool>,
    pane_size: Size,
    tab_spans: Vec<(u16, u16)>,
    /// A status-line message, when it was shown, and whether it is an error
    /// (shown longer, in red).
    message: Option<(String, Instant, bool)>,
    run: Option<Run>,
    /// Where rounds and the script are saved, whether that is due, and when
    /// it last happened.
    state_path: Option<PathBuf>,
    state_dirty: bool,
    last_save: Instant,
    pending: Vec<(usize, String)>,
    dialogs: regex::RegexSet,
    rounds: Vec<Round>,
    /// The round the main tab shows; `rounds.len()` shows the intro, with
    /// the command templates.
    main_view: usize,
    /// The command templates the intro lists, and when they were read.
    templates: std::cell::RefCell<Option<(Instant, Vec<crate::commands::Template>)>>,
    main_focus: usize,
    main_folded: Vec<bool>,
    main_hits: Vec<Option<usize>>,
    main_area: Rect,
    main_rows: Vec<String>,
    main_selection: Option<((usize, u16), (usize, u16))>,
    drag: Option<Drag>,
    clipboard: Clipboard,
    inputs: regex::RegexSet,
    working: regex::RegexSet,
    auto_replies: Vec<(regex::Regex, String)>,
    /// Per harness: whether it got an auto reply for the screen it shows,
    /// so it gets one reply until the match leaves its screen.
    auto_replied: Vec<bool>,
    last_auto_check: Instant,
    main_scroll: u16,
    main_unseen: bool,
    model_patterns: Vec<Vec<regex::Regex>>,
    target_hits: std::cell::RefCell<Vec<TargetHit>>,
    prompt_widths: std::cell::RefCell<std::collections::HashMap<PromptBox, usize>>,
    exit_span: (u16, u16),
    quit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PromptBox {
    Composer,
    Main,
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
    Main { anchor: (usize, u16), moved: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StepState {
    Pending,
    Active,
    Done,
    Skipped,
}

/// A script being run: its steps, and what the waits need to know.
struct Run {
    steps: Vec<Step>,
    states: Vec<StepState>,
    cancelled: bool,
    /// The targets of the last `::send`, and when it was sent.
    last_send: Option<(Vec<usize>, Instant, SystemTime)>,
    started: (Instant, SystemTime),
    /// What the current wait is waiting for.
    detail: String,
    /// Whether the main tab has been looked at since the script ended.
    seen: bool,
    /// Harnesses the current wait has counted, and those it still waits for.
    satisfied: Vec<usize>,
    pending: Vec<usize>,
    /// Set for a script restored after a restart: it waits for Ctrl+G, which
    /// sends the interrupted step's prompt again to these harnesses.
    interrupted: Option<Vec<usize>>,
    /// The script's round in `rounds`, which each `::send` adds to.
    round: Option<usize>,
    /// What `all` means: the harnesses checked in the prompt box the script
    /// was sent from.
    all: Vec<usize>,
}

impl Run {
    fn running(&self) -> bool {
        !self.cancelled && self.current().is_some()
    }

    fn current(&self) -> Option<usize> {
        self.states
            .iter()
            .position(|s| matches!(s, StepState::Pending | StepState::Active))
    }
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
fn styled_line(prefix: &str, cells: crate::rounds::Cells) -> Line<'static> {
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

/// Whether a file modified at `modified` was written after `since`. File times
/// come from the kernel's coarse clock, which can lag the system time by a
/// few milliseconds, so a file written right after `since` may look older.
fn written_since(modified: SystemTime, since: SystemTime) -> bool {
    modified + Duration::from_millis(500) >= since
}

/// `names` without repeats, in order.
fn unique(names: &[String]) -> Vec<&str> {
    let mut seen = Vec::new();
    for name in names {
        if !seen.contains(&name.as_str()) {
            seen.push(name.as_str());
        }
    }
    seen
}

/// The prompt boxes' labels for `names`: each without its version, the
/// dash-separated parts with a digit (`gpt-6-astra` is `gpt-astra`,
/// `kimi-k3` is `kimi`), unless that leaves nothing or two the same.
fn target_labels(names: &[&str]) -> Vec<String> {
    let short: Vec<String> = names
        .iter()
        .map(|name| {
            let parts: Vec<&str> = name
                .split('-')
                .filter(|part| !part.chars().any(|c| c.is_ascii_digit()))
                .collect();
            if parts.is_empty() {
                name.to_string()
            } else {
                parts.join("-")
            }
        })
        .collect();
    short
        .iter()
        .zip(names)
        .map(|(label, name)| {
            if short.iter().filter(|l| *l == label).count() > 1 {
                name.to_string()
            } else {
                label.clone()
            }
        })
        .collect()
}

/// Drawn after the fold arrow of each main tab section header.
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
            main_input: Composer::new(config.harnesses.len()),
            hidden: vec![false; config.harnesses.len()],
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
            main_view: 0,
            main_focus: 0,
            main_folded: Vec::new(),
            main_hits: Vec::new(),
            main_area: Rect::default(),
            main_rows: Vec::new(),
            main_selection: None,
            drag: None,
            clipboard,
            inputs: regex::RegexSet::empty(),
            working: regex::RegexSet::empty(),
            auto_replies: Vec::new(),
            auto_replied: Vec::new(),
            last_auto_check: Instant::now(),
            main_scroll: 0,
            main_unseen: false,
            model_patterns: Vec::new(),
            target_hits: std::cell::RefCell::new(Vec::new()),
            prompt_widths: std::cell::RefCell::new(std::collections::HashMap::new()),
            templates: std::cell::RefCell::new(None),
            exit_span: (0, 0),
            quit: false,
            run: None,
            state_path: None,
            state_dirty: false,
            last_save: Instant::now(),
        };
        app.dialogs = app.config.dialog_regex()?;
        app.inputs = app.config.input_regex()?;
        app.working = app.config.working_regex()?;
        app.auto_replies = app.config.auto_reply_regexes()?;
        app.auto_replied = vec![false; app.config.harnesses.len()];
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
        app.main_input.open(&defaults);
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
        self.message = Some((text.into(), Instant::now(), false));
    }

    fn flash_error(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now(), true));
    }

    fn flash_span(&self) -> Option<Span<'static>> {
        let (text, at, error) = self.message.as_ref()?;
        let shown = if *error { 15 } else { 4 };
        if at.elapsed() >= Duration::from_secs(shown) {
            return None;
        }
        let span = Span::raw(format!(" {text}"));
        Some(if *error { span.red() } else { span.yellow() })
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

    /// Sends the `[[auto_reply]]` reply to each harness that is done
    /// working and shows its pattern near the bottom of its screen, once
    /// per match. Returns whether it sent any.
    fn auto_reply(&mut self) -> bool {
        let mut sent = Vec::new();
        for i in 0..self.panes.len() {
            if !matches!(self.panes[i].status(), Status::Running) {
                continue;
            }
            let tail = self.panes[i].screen_tail(15);
            let Some(reply) = self
                .auto_replies
                .iter()
                .find(|(re, _)| re.is_match(&tail))
                .map(|(_, reply)| reply.clone())
            else {
                self.auto_replied[i] = false;
                continue;
            };
            if self.auto_replied[i]
                || !self.panes[i].ready_for_prompt()
                || self.panes[i].is_busy(AUTO_REPLY_QUIET)
                || self.still_working(i)
            {
                continue;
            }
            self.auto_replied[i] = true;
            let delay = Duration::from_millis(self.config.submit_delay_ms);
            self.panes[i].send_prompt(&reply, delay);
            sent.push(format!("{} \"{reply}\"", self.panes[i].name()));
        }
        if sent.is_empty() {
            return false;
        }
        self.flash(format!("auto reply: {}", sent.join(", ")));
        true
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
            if self.tick_run() {
                redraw = true;
                self.state_dirty = true;
            }
            if self.tick_rounds() {
                redraw = true;
                self.state_dirty = true;
            }
            if self.last_auto_check.elapsed() >= Duration::from_millis(500) {
                self.last_auto_check = Instant::now();
                redraw |= self.auto_reply();
            }
            if self.state_dirty && self.last_save.elapsed() >= Duration::from_secs(1) {
                self.save_state();
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
        let checked = self.main_input.targets.clone();
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => self.on_paste(&text),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Resize(cols, rows) => self.resize(Size { cols, rows }),
            _ => {}
        }
        self.hide_unchecked(&checked);
        self.start_checked();
    }

    /// Hides the tabs of the harnesses just unchecked in the main prompt box
    /// and shows those just checked.
    fn hide_unchecked(&mut self, before: &[bool]) {
        for (i, (&was, &now)) in before.iter().zip(&self.main_input.targets).enumerate() {
            if was != now {
                self.hidden[i] = !now;
            }
        }
        if self.hidden.get(self.active) == Some(&true) {
            self.select(self.main_tab());
        }
    }

    /// Starts the disabled harnesses checked in a prompt box, which also shows
    /// their tabs.
    fn start_checked(&mut self) {
        let mut checked = self.main_input.selected();
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

    fn main_tab(&self) -> usize {
        self.panes.len()
    }

    fn on_main(&self) -> bool {
        self.active == self.main_tab()
    }

    fn select(&mut self, index: usize) {
        if index <= self.main_tab() {
            // Scrolling belongs to the tab being left, and the main tab has
            // no pane to scroll (a click on a tab switches in any mode).
            if index != self.active && self.mode == Mode::Scroll {
                if let Some(pane) = self.panes.get(self.active) {
                    pane.scroll(Scroll::Bottom);
                }
                self.mode = Mode::Normal;
            }
            self.active = index;
            if self.on_main() {
                self.main_unseen = false;
                if let Some(run) = self.run.as_mut().filter(|r| !r.running()) {
                    run.seen = true;
                }
            }
        }
    }

    /// The tabs in tab-bar order: the main tab first, then the harnesses in
    /// config order, leaving out disabled ones that have not been started
    /// and those unchecked in the main prompt box.
    fn visible_tabs(&self) -> Vec<usize> {
        std::iter::once(self.main_tab())
            .chain((0..self.panes.len()).filter(|&i| self.shown(i)))
            .collect()
    }

    /// Whether harness `i` has a tab: it runs and isn't hidden.
    fn shown(&self, i: usize) -> bool {
        self.panes[i].status() != Status::Stopped && !self.hidden[i]
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

    /// The harnesses a prompt to all of them targets until toggled: those
    /// with a tab.
    fn default_targets(&self) -> Vec<usize> {
        (0..self.panes.len()).filter(|&i| self.shown(i)).collect()
    }

    fn open_composer(&mut self, all: bool) {
        let targets: Vec<usize> = if all || self.on_main() {
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
        if self.on_main() {
            self.on_main_key(key);
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
            KeyCode::Char('[') | KeyCode::PageUp if !self.on_main() => {
                self.panes[self.active].scroll(Scroll::PageUp);
                self.mode = Mode::Scroll;
            }
            KeyCode::Char('r') if self.on_main() => self.refresh_round(),
            KeyCode::Char('r') => match self.panes[self.active].status() {
                Status::Running => self.mode = Mode::ConfirmRestart,
                _ => self.restart_active(),
            },
            KeyCode::Char('q') => self.mode = Mode::ConfirmQuit,
            KeyCode::Char('x') if !self.on_main() => {
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
                if self.submit(&text, &targets) {
                    self.mode = Mode::Normal;
                } else {
                    self.composer.insert_str(&text);
                }
            }
        }
    }

    /// The model name sent as MODEL_ID, e.g. `Gemini-4-Argon-high`.
    fn model_id(&self, pane: usize) -> Option<String> {
        let model = self.model_name(pane)?;
        Some(model.split_whitespace().collect::<Vec<_>>().join("-"))
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
            && let Some(model_id) = self.model_id(pane)
        {
            text = format!("MODEL_ID={model_id}\n{text}");
            self.panes[pane].introduced = true;
        }
        self.mark_delivered(pane);
        self.panes[pane].send_prompt(&text, delay);
    }

    /// Sends a prompt from a prompt box, or starts it as a script if it has
    /// `::` commands. Returns false, with the reason shown, if nothing was
    /// sent.
    fn submit(&mut self, text: &str, targets: &[usize]) -> bool {
        let command = match crate::commands::expand(text, &self.config.commands) {
            Ok(command) => command,
            Err(err) => {
                self.flash_error(format!("not sent: {err}"));
                return false;
            }
        };
        let text = command.as_ref().map_or(text, |c| c.text.as_str());
        let names: Vec<&str> = self.panes.iter().map(Pane::name).collect();
        match crate::script::parse(text, &names) {
            Ok(None) => {
                self.submit_prompt(text, targets, false);
                self.state_dirty = true;
                true
            }
            Ok(Some(_)) if self.run.as_ref().is_some_and(Run::running) => {
                self.flash_error(
                    "not sent: a script is already running; Ctrl+X in the main tab cancels it",
                );
                false
            }
            Ok(Some(steps)) => {
                self.start_run(steps, targets.to_vec());
                self.state_dirty = true;
                true
            }
            Err(err) => {
                let err = match &command {
                    Some(command) => command.error(&err),
                    None => err,
                };
                self.flash_error(format!("not sent: {err}"));
                false
            }
        }
    }

    /// Sends `text` to `targets`. A prompt to several harnesses is followed
    /// as a round in the main tab; a script's prompts all go to its round.
    fn submit_prompt(&mut self, text: &str, targets: &[usize], from_script: bool) {
        for &i in targets {
            self.start(i);
            self.hidden[i] = false;
        }
        if from_script {
            self.add_to_script_round(text, targets);
        } else if targets.len() >= 2 {
            self.start_round(text, targets, true);
        }
        self.send_or_queue(text, targets);
    }

    /// Sends `text` to each of `targets` that is ready, and queues it for
    /// the others.
    fn send_or_queue(&mut self, text: &str, targets: &[usize]) {
        let delay = Duration::from_millis(self.config.submit_delay_ms);
        let mut sent = Vec::new();
        let mut queued = Vec::new();
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
        let mut parts = Vec::new();
        if !sent.is_empty() {
            parts.push(format!("sent to {}", sent.join(", ")));
        }
        if !queued.is_empty() {
            parts.push(format!("queued for {} until ready", queued.join(", ")));
        }
        self.flash(parts.join("; "));
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
            Mode::Normal if self.on_main() => self.main_input.insert_str(text),
            Mode::Normal | Mode::Scroll if !self.on_main() => {
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
            Mode::Normal if self.on_main() => PromptBox::Main,
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
                PromptBox::Main => self.main_input.toggle(hit.target),
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
        if self.on_main() {
            self.main_scroll = if up {
                self.main_scroll.saturating_sub(3)
            } else {
                self.main_scroll.saturating_add(3)
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
            let (text, style) = if index == self.main_tab() {
                self.main_tab_label()
            } else {
                self.tab_label(index)
            };
            if position > 0 {
                spans.push(Span::raw("│").dark_gray());
                x += 1;
            }
            let width = text.width() as u16;
            self.tab_spans.push((x, x + width));
            x += width;
            spans.push(Span::styled(text, style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), tabs_area);
        let exit = "✕exit";
        let exit_width = exit.width() as u16;
        let exit_x = tabs_area.right().saturating_sub(exit_width);
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

        if self.on_main() {
            let input_cursor = self.draw_main(frame, pane_area);
            self.draw_status(frame, status_area);
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

        self.draw_status(frame, status_area);

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

    /// The status line, and above it, an error too long for that line in
    /// full, so it isn't cut off.
    fn draw_status(&self, frame: &mut Frame, status_area: Rect) {
        let mut line = self.status_line();
        if self.config.yolo {
            // Right after the model (or mode) badge, in plain red so it
            // doesn't outshine the badge.
            let at = line.spans.len().min(1);
            line.spans.insert(at, Span::raw(" YOLO").red());
        }
        let cut = line.width() > usize::from(status_area.width);
        frame.render_widget(Paragraph::new(line), status_area);
        let Some((text, _, true)) = &self.message else {
            return;
        };
        if !cut || self.flash_span().is_none() {
            return;
        }
        let width = usize::from(status_area.width.max(1));
        let mut lines = vec![String::new()];
        for c in text.chars() {
            let last = lines.last_mut().expect("never empty");
            if last.width() + c.to_string().width() > width {
                lines.push(String::new());
            }
            lines.last_mut().expect("never empty").push(c);
        }
        let height = (lines.len() as u16).min(status_area.y);
        let error = Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()).red();
        let area = Rect::new(
            status_area.x,
            status_area.y - height,
            status_area.width,
            height,
        );
        frame.render_widget(Clear, area);
        frame.render_widget(error, area);
    }

    fn status_line(&self) -> Line<'static> {
        let hint = |s: &str| Span::raw(s.to_string()).dark_gray();
        if self.on_main() && matches!(self.mode, Mode::Normal | Mode::Help) {
            let mut spans = vec![Span::raw(" main ").black().on_magenta()];
            spans.extend(self.flash_span());
            spans.push(hint(if self.config.main.compare {
                "  Enter send to all · Alt+Enter newline · Ctrl+R summarize · Ctrl+P/N round · PgUp/PgDn scroll"
            } else {
                "  Enter send to all · Alt+Enter newline · Ctrl+R capture all · Ctrl+P/N round · PgUp/PgDn scroll"
            }));
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
            Mode::Compose => match self.flash_span() {
                Some(message) => Line::from(vec![Span::raw(" PROMPT ").black().on_cyan(), message]),
                None => Line::from(vec![
                    Span::raw(" PROMPT ").black().on_cyan(),
                    hint(
                        " Enter send · Alt+Enter/Ctrl+J newline · Alt+1-9 toggle target · Ctrl+A all · ↑↓ history · Esc cancel",
                    ),
                ]),
            },
            Mode::Scroll => Line::from(vec![
                Span::raw(" SCROLL ").black().on_magenta(),
                hint(&format!(
                    " offset {} · ↑↓ j/k line · PgUp/PgDn page · g/G top/bottom · Esc/q back",
                    pane.display_offset()
                )),
            ]),
            Mode::ConfirmQuit => Line::from(vec![
                Span::raw(" QUIT ").white().on_red(),
                Span::raw(" Stop all harnesses and quit? y/N, or click ✕exit again"),
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
                spans.extend(self.flash_span());
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
        let names: Vec<&str> = self.panes.iter().map(Pane::name).collect();
        for (i, name) in target_labels(&names).into_iter().enumerate() {
            let on = composer.targets[i];
            let label = format!("{}{name} ", if on { "✓" } else { "" });
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
            Line::from("  r (main)  capture the latest answers now (also Ctrl+R in the main tab)"),
            Line::from("  q            quit (stops all harnesses)"),
            Line::from("  Ctrl+\\       send a literal Ctrl+\\ to the harness"),
            Line::from(""),
            Line::from("Mouse: drag with the left button to select text; it's copied on release."),
            Line::from("Click a ✓target in a prompt box to toggle it; click ✕exit to quit."),
            Line::from(""),
            Line::from("Prompt composer:").bold(),
            Line::from("  Enter send · Alt+Enter / Ctrl+J newline · Alt+1-9 toggle target"),
            Line::from("  Ctrl+A toggle all · ↑/↓ history · Esc cancel"),
            Line::from(""),
            Line::from(
                "Tab markers: ● output · ⚑ trust the folder · ⧗ prompt queued · ? waiting for your answer · ✗ exited",
            ),
            Line::from(
                "Main tab: each harness's answer to a prompt sent to 2+ harnesses, once it's done.",
            ),
            Line::from(
                "Type in the main tab's prompt box and press Enter to send to all harnesses.",
            ),
            Line::from(
                "A prompt with ::send, ::wait-model-files, ::wait-file or ::wait-done lines runs as a script;",
            ),
            Line::from("in the main tab Ctrl+S skips the wait it is on and Ctrl+X cancels it."),
            Line::from(
                "/review BASE..TARGET: every model reviews BASE..TARGET by reading it, then one merges the reviews;",
            ),
            Line::from("/review-full also builds and runs tests."),
            Line::from(
                "Rounds and the script are saved; after a restart Ctrl+G resumes a paused script.",
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

    fn start_round(&mut self, text: &str, targets: &[usize], supersede: bool) {
        let script = self.run.as_ref().and_then(|r| r.round);
        if supersede
            && let Some(last) = self.rounds.last()
            && last.state() == SummaryState::Waiting
            && script != Some(self.rounds.len() - 1)
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
        self.main_folded.clear();
        self.view_round(self.rounds.len() - 1);
    }

    /// Adds a script's `::send` to the script's round, which its first
    /// `::send` starts.
    fn add_to_script_round(&mut self, text: &str, targets: &[usize]) {
        let names: Vec<String> = targets
            .iter()
            .map(|&i| self.panes[i].name().to_string())
            .collect();
        let index = self
            .run
            .as_ref()
            .and_then(|r| r.round)
            .filter(|&i| i < self.rounds.len());
        let Some(index) = index else {
            self.start_round(text, targets, false);
            if let Some(run) = self.run.as_mut() {
                run.round = Some(self.rounds.len() - 1);
            }
            return;
        };
        let before = self.rounds[index].targets.len();
        self.rounds[index].add_step(text, targets, names);
        // The new sections start folded, before the comparison.
        if self.main_view == index && self.main_folded.len() >= before {
            for _ in targets {
                self.main_folded.insert(before, true);
            }
        }
    }

    fn mark_delivered(&mut self, pane: usize) {
        let mark = self.panes[pane].history_mark();
        let model_id = self.model_id(pane);
        // The prompt is for the pane's first section not yet delivered, in
        // the latest round or the script's.
        let last = self.rounds.len().checked_sub(1);
        let script = self.run.as_ref().and_then(|r| r.round);
        let mut delivered = false;
        for (index, round) in self.rounds.iter_mut().enumerate() {
            let open = Some(index) == last || Some(index) == script;
            for slot in 0..round.targets.len() {
                if round.targets[slot] != pane {
                    continue;
                }
                if round.delivered[slot].is_some() {
                    round.followed_up[slot] = true;
                } else if open && !delivered && round.state() == SummaryState::Waiting {
                    round.delivered[slot] = Some((Instant::now(), mark));
                    round.outputs[slot] = crate::rounds::output_file(
                        &round.prompts[slot],
                        &self.config.main.output_vars,
                        model_id.as_deref(),
                    )
                    .map(|path| (self.cwd.join(path), SystemTime::now()));
                    delivered = true;
                }
            }
        }
    }

    /// The modification time of the file a round's harness was asked to
    /// write, if it has written it since the prompt was sent.
    fn report_written(round: &Round, slot: usize) -> Option<SystemTime> {
        let (path, sent) = round.outputs[slot].as_ref()?;
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta.modified().ok()?;
        (meta.len() > 0 && written_since(modified, *sent)).then_some(modified)
    }

    /// Whether the harness has finished its report: None if the prompt names
    /// no output file, otherwise whether the file is written and unchanged
    /// for `quiet`.
    fn report_ready(round: &Round, slot: usize, quiet: Duration) -> Option<bool> {
        round.outputs[slot].as_ref()?;
        Some(
            Self::report_written(round, slot)
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= quiet),
        )
    }

    /// Captures a round's harness: its report file once written, otherwise
    /// its screen.
    fn capture_slot(&self, index: usize, slot: usize, mark: usize, finished: bool) -> Capture {
        let pane = self.rounds[index].targets[slot];
        let mut capture = self.capture(pane, mark, finished);
        if let Some(modified) = Self::report_written(&self.rounds[index], slot)
            && let Some((path, _)) = &self.rounds[index].outputs[slot]
            && let Ok(text) = std::fs::read_to_string(path)
        {
            capture.text = text;
            capture.styles.clear();
            capture.report = Some((path.clone(), modified));
        }
        capture
    }

    fn capture(&self, pane: usize, mark: usize, finished: bool) -> Capture {
        let lines = self.panes[pane].styled_since(mark, self.config.main.max_lines);
        let text = lines
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let text = if self.panes[pane].harness.strips_right_panel() {
            crate::rounds::strip_right_panel(&text)
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
            report: None,
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
        let name = round
            .section_name(slot)
            .replace(" · ", "-")
            .replace(' ', "-");
        let file = dir.join(format!("{}-{name}-{state}.txt", round.started_at));
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

    fn start_run(&mut self, steps: Vec<Step>, all: Vec<usize>) {
        let n = steps.len();
        self.run = Some(Run {
            steps,
            states: vec![StepState::Pending; n],
            cancelled: false,
            last_send: None,
            started: (Instant::now(), SystemTime::now()),
            detail: String::new(),
            seen: false,
            satisfied: Vec::new(),
            pending: Vec::new(),
            interrupted: None,
            round: None,
            all,
        });
        self.flash(format!("script started: {n} steps"));
        self.tick_run();
    }

    fn cancel_run(&mut self) {
        let on_main = self.on_main();
        match self.run.as_mut() {
            Some(run) if run.running() => {
                run.cancelled = true;
                run.seen = on_main;
                self.state_dirty = true;
                self.flash("script cancelled; prompts already sent keep running");
            }
            _ => self.flash("no script is running"),
        }
    }

    fn skip_wait(&mut self) {
        let Some(run) = self.run.as_mut().filter(|r| r.running()) else {
            self.flash("no script is running");
            return;
        };
        if run.interrupted.is_some() {
            self.flash("the script is paused; Ctrl+G resumes it");
            return;
        }
        let Some(i) = run.current() else { return };
        if matches!(run.steps[i].command, Command::Send { .. }) {
            return;
        }
        run.states[i] = StepState::Skipped;
        run.satisfied.clear();
        run.pending.clear();
        let line = run.steps[i].line.clone();
        self.state_dirty = true;
        self.flash(format!("skipped {line}"));
        self.tick_run();
    }

    /// The harnesses a script's targets mean: `all` is those checked in the
    /// prompt box the script was sent from.
    fn resolve(&self, targets: &Targets) -> Vec<usize> {
        match targets {
            Targets::All => self.run.as_ref().map_or_else(Vec::new, |r| r.all.clone()),
            Targets::Named(names) => names
                .iter()
                .filter_map(|n| self.panes.iter().position(|p| p.name() == n))
                .collect(),
        }
    }

    /// Whether a harness is done with what it was sent at `since`, by the
    /// same rules as a round: quiet, not asking, not working in the background.
    fn pane_done(&self, pane: usize, since: Instant) -> bool {
        let quiet = Duration::from_secs(self.config.main.quiet_secs);
        match self.panes[pane].status() {
            Status::Running => {
                !self.has_pending(pane)
                    && since.elapsed() >= quiet
                    && !self.panes[pane].is_busy(quiet)
                    && !self.needs_input(pane)
                    && !self.still_working(pane)
            }
            _ => true,
        }
    }

    /// Whether `path` was written after `since` and has not changed for
    /// `quiet_secs`.
    fn file_settled(&self, path: &std::path::Path, since: SystemTime) -> bool {
        let quiet = Duration::from_secs(self.config.main.quiet_secs);
        std::fs::metadata(self.cwd.join(path)).is_ok_and(|meta| {
            meta.len() > 0
                && meta.modified().is_ok_and(|m| {
                    written_since(m, since) && m.elapsed().is_ok_and(|age| age >= quiet)
                })
        })
    }

    /// Advances the script: sends, and checks the current wait. Returns
    /// whether anything changed.
    pub fn tick_run(&mut self) -> bool {
        let mut changed = false;
        loop {
            let Some(run) = self
                .run
                .as_ref()
                .filter(|r| r.running() && r.interrupted.is_none())
            else {
                return changed;
            };
            let i = run.current().expect("a running script has a current step");
            let step = run.steps[i].clone();
            let last_send = run.last_send.clone();
            let started = run.started;
            let already = run.satisfied.clone();
            let waiting_for = |targets: &Option<Targets>| -> (Vec<usize>, Instant, SystemTime) {
                let (sent, at, time) =
                    last_send
                        .clone()
                        .unwrap_or((Vec::new(), started.0, started.1));
                match targets {
                    Some(targets) => (self.resolve(targets), at, time),
                    None => (sent, at, time),
                }
            };
            // Splits the harnesses a wait is for into those it counts now
            // (once counted, always counted) and those it still waits for.
            let split =
                |panes: &[usize], ready: &dyn Fn(usize) -> bool| -> (Vec<usize>, Vec<usize>) {
                    panes
                        .iter()
                        .partition(|&&p| already.contains(&p) || ready(p))
                };
            let names = |panes: &[usize]| -> String {
                panes
                    .iter()
                    .map(|&p| self.panes[p].name())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (done, satisfied, pending, detail) = match &step.command {
                Command::Send { targets, text } => {
                    let panes = self.resolve(targets);
                    if panes.is_empty() {
                        let on_main = self.on_main();
                        if let Some(run) = self.run.as_mut() {
                            run.cancelled = true;
                            run.seen = on_main;
                        }
                        self.flash_error(format!(
                            "script stopped: {} has no harness to send to",
                            step.line
                        ));
                        return true;
                    }
                    // Before sending: a quick harness may write its file at once.
                    let sent = (Instant::now(), SystemTime::now());
                    self.submit_prompt(text, &panes, true);
                    if let Some(run) = self.run.as_mut() {
                        run.last_send = Some((panes, sent.0, sent.1));
                    }
                    (true, Vec::new(), Vec::new(), String::new())
                }
                Command::WaitModelFiles { targets, .. } => {
                    let (panes, at, time) = waiting_for(targets);
                    let (satisfied, pending) = split(&panes, &|p| {
                        match step.command.path(self.model_id(p).as_deref()) {
                            Some(path) => self.file_settled(std::path::Path::new(&path), time),
                            // No MODEL_ID to name its file: wait until it is done.
                            None => self.pane_done(p, at),
                        }
                    });
                    let detail = format!(
                        "{}/{} written; waiting for {}",
                        satisfied.len(),
                        panes.len(),
                        names(&pending)
                    );
                    (pending.is_empty(), satisfied, pending, detail)
                }
                Command::WaitFile { .. } => {
                    let (sent, _, time) = waiting_for(&None);
                    let path = step.command.path(None).unwrap_or_default();
                    let done = self.file_settled(std::path::Path::new(&path), time);
                    // Pending: the harnesses that were asked to write it.
                    let pending = if done { Vec::new() } else { sent };
                    (done, Vec::new(), pending, format!("waiting for {path}"))
                }
                Command::WaitDone { targets } => {
                    let (panes, at, _) = waiting_for(targets);
                    let (satisfied, pending) = split(&panes, &|p| self.pane_done(p, at));
                    let detail = format!("waiting for {}", names(&pending));
                    (pending.is_empty(), satisfied, pending, detail)
                }
            };
            let on_main = self.on_main();
            let run = self.run.as_mut().expect("checked above");
            if !done {
                if run.states[i] != StepState::Active
                    || run.detail != detail
                    || run.satisfied != satisfied
                    || run.pending != pending
                {
                    run.states[i] = StepState::Active;
                    run.detail = detail;
                    run.satisfied = satisfied;
                    run.pending = pending;
                    changed = true;
                }
                return changed;
            }
            run.states[i] = StepState::Done;
            run.detail.clear();
            run.satisfied.clear();
            run.pending.clear();
            changed = true;
            if run.current().is_none() {
                run.seen = on_main;
                self.flash("script finished");
                return true;
            }
        }
    }

    /// The script's steps, shown above the rounds in the main tab.
    fn run_lines(&self) -> Vec<Line<'static>> {
        let Some(run) = &self.run else {
            return Vec::new();
        };
        let resend = run.interrupted.as_ref().map(|panes| {
            panes
                .iter()
                .map(|&p| self.panes[p].name())
                .collect::<Vec<_>>()
                .join(", ")
        });
        let (title, hint) = if run.cancelled {
            ("Script cancelled".to_string(), String::new())
        } else if let Some(resend) = resend {
            let what = if resend.is_empty() {
                "Ctrl+G resumes".to_string()
            } else {
                format!("Ctrl+G sends the current step's prompt again to {resend} and resumes")
            };
            (
                "Script paused by a restart".to_string(),
                format!("   {what} · Ctrl+X cancel"),
            )
        } else if run.running() {
            (
                "Script".to_string(),
                "   Ctrl+S skip a wait · Ctrl+X cancel".to_string(),
            )
        } else {
            ("Script finished".to_string(), String::new())
        };
        let mut lines = vec![Line::from(vec![
            Span::raw(title).bold(),
            Span::raw(hint).dark_gray(),
        ])];
        for (step, state) in run.steps.iter().zip(&run.states) {
            let (marker, style) = match state {
                StepState::Done => ("✓", Style::default()),
                StepState::Skipped => ("⤼", Style::default().dark_gray()),
                StepState::Active if run.cancelled => ("✗", Style::default().red()),
                StepState::Active => ("●", Style::default().green()),
                StepState::Pending => (" ", Style::default().dark_gray()),
            };
            let mut spans = vec![Span::styled(format!("  {marker} {}", step.line), style)];
            if *state == StepState::Active && !run.cancelled && !run.detail.is_empty() {
                spans.push(Span::raw(format!("  — {}", run.detail)).dark_gray());
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines
    }

    /// Resumes a script restored after a restart, sending the step it was on
    /// again to the harnesses that had not finished it.
    fn resume_run(&mut self) {
        let Some(run) = self.run.as_mut().filter(|r| r.running()) else {
            self.flash("no script is running");
            return;
        };
        let Some(resend) = run.interrupted.take() else {
            self.flash("the script is not paused");
            return;
        };
        let current = run.current().unwrap_or(0);
        let text = run.steps[..current]
            .iter()
            .rev()
            .find_map(|s| match &s.command {
                Command::Send { text, .. } => Some(text.clone()),
                _ => None,
            });
        let round = run.round.filter(|&i| i < self.rounds.len());
        self.state_dirty = true;
        if let Some(text) = text
            && !resend.is_empty()
        {
            // The resent prompt is for the sections the restart interrupted.
            if let Some(round) = round.map(|i| &mut self.rounds[i]) {
                for &pane in &resend {
                    if let Some(slot) = round.last_slot(pane) {
                        round.delivered[slot] = None;
                        round.followed_up[slot] = false;
                        round.captures[slot] = None;
                        round.outputs[slot] = None;
                        round.added[slot] = Instant::now();
                    }
                }
            }
            for &i in &resend {
                self.start(i);
            }
            let sent = (Instant::now(), SystemTime::now());
            self.send_or_queue(&text, &resend);
            if let Some(run) = self.run.as_mut()
                && let Some(last) = run.last_send.as_mut()
            {
                last.1 = sent.0;
                last.2 = sent.1;
            }
        }
        self.flash("script resumed");
        self.tick_run();
    }

    /// Saves to `path` from now on, and first restores from it unless
    /// `restore` is false.
    pub fn use_state_file(&mut self, path: PathBuf, restore: bool) {
        if restore {
            match crate::state::load(&path) {
                Ok(Some(saved)) => self.restore(saved),
                Ok(None) => {}
                Err(err) => self.flash_error(format!("not restored: {err:#}")),
            }
        }
        self.state_path = Some(path);
        self.state_dirty = true;
    }

    pub fn save_state(&mut self) {
        self.state_dirty = false;
        self.last_save = Instant::now();
        let Some(path) = self.state_path.clone() else {
            return;
        };
        let rounds = self.rounds.iter().map(|r| self.saved_round(r)).collect();
        let run = self.run.as_ref().map(|r| self.saved_run(r));
        if let Err(err) = crate::state::save(&path, rounds, run) {
            self.flash_error(format!("could not save state: {err:#}"));
        }
    }

    fn saved_round(&self, round: &Round) -> crate::state::SavedRound {
        use crate::state::{SavedCapture, SavedRound, SavedState, compress_styles};
        let now = (Instant::now(), SystemTime::now());
        SavedRound {
            prompt: round.prompt.clone(),
            names: round.names.clone(),
            prompts: round.prompts.clone(),
            steps: round.steps.clone(),
            delivered: round
                .delivered
                .iter()
                .map(|d| d.map(|(at, _)| now.1 - now.0.duration_since(at)))
                .collect(),
            captures: round
                .captures
                .iter()
                .map(|c| {
                    c.as_ref().map(|c| SavedCapture {
                        text: c.text.clone(),
                        finished: c.finished,
                        model: c.model.clone(),
                        styles: c
                            .styles
                            .iter()
                            .map(|(line, styles)| (line.clone(), compress_styles(styles)))
                            .collect(),
                        report: c.report.clone(),
                    })
                })
                .collect(),
            state: match round.state() {
                SummaryState::Waiting => SavedState::Waiting,
                SummaryState::Running { .. } => {
                    SavedState::Failed("interrupted by a restart; Ctrl+R compares again".into())
                }
                SummaryState::Done { text, took } => SavedState::Done {
                    text,
                    took_secs: took.as_secs(),
                },
                SummaryState::Failed(err) => SavedState::Failed(err),
                SummaryState::Superseded => SavedState::Superseded,
            },
            started_at: round.started_at,
            compared_unfinished: round.compared_unfinished,
            followed_up: round.followed_up.clone(),
            reopened: round.reopened.clone(),
            outputs: round.outputs.clone(),
        }
    }

    fn saved_run(&self, run: &Run) -> crate::state::SavedRun {
        let names = |panes: &[usize]| -> Vec<String> {
            panes
                .iter()
                .map(|&p| self.panes[p].name().to_string())
                .collect()
        };
        let now = (Instant::now(), SystemTime::now());
        crate::state::SavedRun {
            steps: run.steps.clone(),
            states: run.states.clone(),
            cancelled: run.cancelled,
            last_send: run
                .last_send
                .as_ref()
                .map(|(panes, _, time)| (names(panes), *time)),
            started: now.1 - now.0.duration_since(run.started.0),
            seen: run.seen,
            satisfied: names(&run.satisfied),
            // A paused script still waits for the same harnesses.
            pending: names(run.interrupted.as_deref().unwrap_or(&run.pending)),
            round: run.round,
            all: Some(names(&run.all)),
        }
    }

    fn restore(&mut self, saved: crate::state::Saved) {
        use crate::state::{SavedState, expand_styles};
        let names: Vec<String> = self.panes.iter().map(|p| p.name().to_string()).collect();
        let index_of = |name: &str| names.iter().position(|n| n == name);
        let instant_of = |time: SystemTime| {
            let ago = SystemTime::now().duration_since(time).unwrap_or_default();
            Instant::now().checked_sub(ago).unwrap_or_else(Instant::now)
        };
        // Where each saved round ends up in `rounds`, if anywhere.
        let mut restored: Vec<Option<usize>> = Vec::new();
        for saved in saved.rounds {
            // A round for a harness no longer configured can't be shown.
            let Some(targets) = saved
                .names
                .iter()
                .map(|n| index_of(n))
                .collect::<Option<Vec<_>>>()
            else {
                restored.push(None);
                continue;
            };
            restored.push(Some(self.rounds.len()));
            let n = targets.len();
            let mut round = Round::new(saved.prompt, targets, saved.names, saved.started_at);
            if saved.prompts.len() == n && saved.steps.len() == n {
                round.prompts = saved.prompts;
                round.steps = saved.steps;
            }
            round.delivered = saved
                .delivered
                .iter()
                .map(|d| d.map(|time| (instant_of(time), 0)))
                .collect();
            round.captures = saved
                .captures
                .into_iter()
                .map(|c| {
                    c.map(|c| Capture {
                        text: c.text,
                        finished: c.finished,
                        model: c.model,
                        at: Instant::now(),
                        styles: c
                            .styles
                            .iter()
                            .map(|(line, runs)| (line.clone(), expand_styles(runs)))
                            .collect(),
                        report: c.report,
                    })
                })
                .collect();
            round.set_state(match saved.state {
                SavedState::Waiting => SummaryState::Waiting,
                SavedState::Done { text, took_secs } => SummaryState::Done {
                    text,
                    took: Duration::from_secs(took_secs),
                },
                SavedState::Failed(err) => SummaryState::Failed(err),
                SavedState::Superseded => SummaryState::Superseded,
            });
            round.compared_unfinished = saved.compared_unfinished;
            round.reopened = saved.reopened;
            round.outputs = saved.outputs;
            // The harnesses are new sessions: nothing they show belongs to
            // the round, but a report file still can.
            round.followed_up = vec![true; round.targets.len()];
            for slot in 0..round.targets.len() {
                if round.captures[slot].is_none()
                    && round.delivered[slot].is_some()
                    && round.outputs[slot].is_none()
                {
                    round.captures[slot] = Some(Capture {
                        text: "(not captured: agent-tabs was restarted first)".into(),
                        finished: true,
                        model: None,
                        at: Instant::now(),
                        styles: Default::default(),
                        report: None,
                    });
                }
            }
            self.rounds.push(round);
        }
        let mut paused = false;
        if let Some(saved) = saved.run {
            let indices = |names: &[String]| -> Vec<usize> {
                names.iter().filter_map(|n| index_of(n)).collect()
            };
            let running = !saved.cancelled
                && saved
                    .states
                    .iter()
                    .any(|s| matches!(s, StepState::Pending | StepState::Active));
            paused = running;
            self.run = Some(Run {
                steps: saved.steps,
                states: saved.states,
                cancelled: saved.cancelled,
                last_send: saved
                    .last_send
                    .map(|(names, time)| (indices(&names), instant_of(time), time)),
                started: (instant_of(saved.started), saved.started),
                detail: String::new(),
                seen: saved.seen,
                satisfied: indices(&saved.satisfied),
                pending: Vec::new(),
                interrupted: running.then(|| indices(&saved.pending)),
                round: saved.round.and_then(|i| restored.get(i).copied().flatten()),
                // Scripts saved before `all` was kept meant every harness that
                // isn't disabled.
                all: match saved.all {
                    Some(names) => indices(&names),
                    None => self.default_targets(),
                },
            });
        }
        // The main tab starts on the intro; Ctrl+P shows the last round.
        self.main_folded.clear();
        self.view_round(self.rounds.len());
        let rounds = self.rounds.len();
        if paused {
            self.flash(format!(
                "restored {rounds} rounds; the script is paused: Ctrl+G in the main tab resumes it"
            ));
        } else if rounds > 0 {
            self.flash(format!("restored {rounds} rounds"));
        }
    }

    pub fn tick_rounds(&mut self) -> bool {
        let mut changed = false;
        for index in 0..self.rounds.len() {
            changed |= self.tick_round(index);
        }
        changed
    }

    fn tick_round(&mut self, index: usize) -> bool {
        let state = self.rounds[index].state();
        if state == SummaryState::Superseded {
            return false;
        }
        let quiet = Duration::from_secs(self.config.main.quiet_secs);
        let timeout = Duration::from_secs(self.config.main.wait_timeout_secs);
        let mut changed = false;
        for slot in 0..self.rounds[index].targets.len() {
            let timed_out = self.rounds[index].added[slot].elapsed() >= timeout;
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
                    let report = Self::report_ready(&self.rounds[index], slot, quiet);
                    let quiet_now = at.elapsed() >= quiet
                        && !self.panes[pane].is_busy(quiet)
                        && !self.needs_input(pane)
                        && !self.still_working(pane)
                        && report != Some(false);
                    // A harness sent another prompt (a script's next step) is
                    // busy with that; its finished report is this round's answer.
                    let moved_on = self.rounds[index].followed_up[slot] && report == Some(true);
                    if !running || quiet_now || moved_on {
                        Some(self.capture_slot(index, slot, mark, true))
                    } else if timed_out && existing.is_none() {
                        Some(self.capture_slot(index, slot, mark, false))
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
        if changed && self.rounds[index].all_finished() && !self.on_main() {
            self.main_unseen = true;
        }
        if !self.config.main.compare
            || !self.config.main.auto
            || !self.rounds[index].all_captured()
            || self.rounds[index].targets.len() < 2
        {
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
        let rewritten = capture.report.as_ref().is_some_and(|(_, modified)| {
            Self::report_written(round, slot).is_some_and(|now| now > *modified)
        });
        let resumed = !round.followed_up[slot]
            && pane.status() == Status::Running
            && (pane.last_output().is_some_and(|t| t > capture.at) || rewritten);
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
        let config = self.config.main.clone();
        let dirty = self.dirty.clone();
        let log_dir = self.log_dir.clone();
        std::thread::spawn(move || {
            let cwd = std::env::temp_dir().join("agent-tabs-compare");
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
                self.rounds[index].captures[slot] =
                    Some(self.capture_slot(index, slot, mark, false));
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
        self.main_unseen = !self.on_main();
    }

    fn refresh_round(&mut self) {
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
            self.main_folded.clear();
            self.view_round(0);
        }
        let index = self.main_view.min(self.rounds.len() - 1);
        if !self.config.main.compare {
            // Without a comparison, Ctrl+R takes every answer as it is now.
            for slot in 0..self.rounds[index].targets.len() {
                if self.rounds[index].delivered[slot].is_some() {
                    self.refresh_section(slot);
                }
            }
            self.flash("captured every answer as it is now");
            return;
        }
        if matches!(self.rounds[index].state(), SummaryState::Running { .. }) {
            self.flash("comparison already running");
            return;
        }
        self.main_scroll = 0;
        self.start_summarizer(index);
    }

    fn on_main_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.pane_size.rows / 2;
        let sections = self.main_sections();
        match key.code {
            KeyCode::Char('r') if ctrl => self.refresh_round(),
            KeyCode::Char('p') if ctrl => self.view_round(self.main_view.saturating_sub(1)),
            // Past the last round is the intro.
            KeyCode::Char('n') if ctrl => {
                self.view_round((self.main_view + 1).min(self.rounds.len()))
            }
            KeyCode::Char('x') if ctrl => self.cancel_run(),
            KeyCode::Char('g') if ctrl => self.resume_run(),
            KeyCode::Char('s') if ctrl => self.skip_wait(),
            KeyCode::Char('l') if ctrl && sections > 0 => {
                self.refresh_section(self.main_focus.min(sections - 1));
            }
            KeyCode::Char('o') if ctrl && sections > 0 => {
                self.toggle_section(self.main_focus.min(sections - 1));
            }
            KeyCode::Char('e') if ctrl && sections > 0 => {
                let fold = !self.main_folded.iter().all(|f| *f);
                self.main_folded.iter_mut().for_each(|f| *f = fold);
            }
            KeyCode::Tab if sections > 0 => self.main_focus = (self.main_focus + 1) % sections,
            KeyCode::BackTab if sections > 0 => {
                self.main_focus = (self.main_focus + sections - 1) % sections
            }
            KeyCode::Up if ctrl => self.main_scroll = self.main_scroll.saturating_sub(1),
            KeyCode::Down if ctrl => self.main_scroll = self.main_scroll.saturating_add(1),
            KeyCode::PageUp => self.main_scroll = self.main_scroll.saturating_sub(page),
            KeyCode::PageDown => self.main_scroll = self.main_scroll.saturating_add(page),
            KeyCode::Up | KeyCode::Down
                if move_visual(
                    &mut self.main_input,
                    self.prompt_widths.borrow().get(&PromptBox::Main).copied(),
                    &key,
                ) => {}
            _ => match self.main_input.handle_key(&key) {
                ComposerAction::None => {}
                ComposerAction::Cancel => self.main_input.clear(),
                ComposerAction::Submit { text, targets } => {
                    if self.submit(&text, &targets) {
                        self.main_scroll = 0;
                    } else {
                        self.main_input.insert_str(&text);
                    }
                }
            },
        }
    }

    fn main_sections(&self) -> usize {
        self.rounds
            .get(self.main_view)
            .map(|r| r.targets.len() + usize::from(self.config.main.compare))
            .unwrap_or(0)
    }

    fn view_round(&mut self, index: usize) {
        if index != self.main_view || self.main_folded.len() != self.main_sections() {
            self.main_view = index;
            self.main_scroll = 0;
            self.main_focus = 0;
            // Answers start folded; the comparison, last, unfolded.
            let compare = self.config.main.compare;
            let agents = self.main_sections().saturating_sub(usize::from(compare));
            self.main_folded = std::iter::repeat_n(true, agents)
                .chain(compare.then_some(false))
                .collect();
            if self.main_sections() == 0 {
                self.main_folded.clear();
            }
        }
    }

    fn toggle_section(&mut self, section: usize) {
        if let Some(folded) = self.main_folded.get_mut(section) {
            *folded = !*folded;
            self.main_focus = section;
        }
    }

    fn main_tab_label(&self) -> (String, Style) {
        let compare = self.config.main.compare;
        // A script's state comes first: ▶ sending or waiting for harnesses,
        // ⏸ waiting for files, then ✓ or ✗ until the tab is looked at.
        let script = self.run.as_ref().and_then(|run| {
            if run.running() && run.interrupted.is_some() {
                Some(('?', Color::Magenta))
            } else if run.running() {
                let files = run.current().is_some_and(|i| {
                    matches!(
                        run.steps[i].command,
                        Command::WaitModelFiles { .. } | Command::WaitFile { .. }
                    )
                });
                Some(if files {
                    ('⏸', Color::Yellow)
                } else {
                    ('▶', Color::Green)
                })
            } else if !run.seen {
                Some(if run.cancelled {
                    ('✗', Color::Red)
                } else {
                    ('✓', Color::Green)
                })
            } else {
                None
            }
        });
        let (marker, color) = match self.rounds.last().map(Round::state) {
            _ if script.is_some() => script.expect("checked"),
            _ if !compare => match self.rounds.last() {
                Some(round) if !round.all_finished() => ('⧗', Color::Yellow),
                Some(_) if self.main_unseen => ('●', Color::Green),
                _ => (' ', Color::Reset),
            },
            Some(SummaryState::Waiting) => ('⧗', Color::Yellow),
            Some(SummaryState::Running { .. }) => ('…', Color::Cyan),
            Some(SummaryState::Done { .. }) if self.main_unseen => ('●', Color::Green),
            Some(SummaryState::Failed(_)) => ('!', Color::Red),
            _ => (' ', Color::Reset),
        };
        let text = format!("{marker}main");
        let mut style = Style::default().fg(color);
        if self.on_main() {
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
            Some(model) => format!("{} · {model}", round.section_name(slot)),
            None => round.section_name(slot),
        }
    }

    fn agent_status(&self, round: &Round, slot: usize) -> (String, Color) {
        let pane = round.targets[slot];
        match &round.captures[slot] {
            Some(capture) if capture.finished => ("✓ done".into(), Color::Reset),
            Some(_)
                if self.panes[pane].is_busy(Duration::from_secs(self.config.main.quiet_secs)) =>
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
            None if self.panes[pane].is_busy(Duration::from_secs(self.config.main.quiet_secs)) => {
                ("working".into(), Color::Green)
            }
            None if self.still_working(pane) => ("working in the background".into(), Color::Green),
            None if self.waiting_for_report(round, slot) => {
                ("finishing its report".into(), Color::Green)
            }
            None => ("settling".into(), Color::Green),
        }
    }

    /// Whether the round's harness has an output file it has not finished.
    fn waiting_for_report(&self, round: &Round, slot: usize) -> bool {
        let quiet = Duration::from_secs(self.config.main.quiet_secs);
        Self::report_ready(round, slot, quiet) == Some(false)
    }

    fn section_header(
        &self,
        index: usize,
        label: String,
        status: (String, Color),
        headline: Option<&str>,
    ) -> Line<'static> {
        let folded = self.main_folded.get(index).copied().unwrap_or(false);
        let mut name = Span::raw(label).bold();
        if index == self.main_focus {
            name = name.reversed();
        }
        // The arrow and the refresh button sit in fixed columns so a click can
        // tell them apart (see `main_click`).
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

    fn main_lines(&self) -> Vec<(Line<'static>, Option<usize>)> {
        let mut lines: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        if self.shows_run() {
            lines.extend(self.run_lines().into_iter().map(|l| (l, None)));
        }
        lines.extend(self.round_lines());
        lines
    }

    /// Whether the main tab shows the script's steps: above the script's
    /// round, and on the intro while it runs (or is paused by a restart) or
    /// when its round is unknown.
    fn shows_run(&self) -> bool {
        let Some(run) = &self.run else {
            return false;
        };
        let on_intro = self.main_view >= self.rounds.len();
        match run.round {
            Some(round) => round == self.main_view || (on_intro && run.running()),
            None => on_intro || run.running(),
        }
    }

    /// The main tab without a round: what to do, and the command templates.
    fn intro_lines(&self) -> Vec<Line<'static>> {
        let rounds = self.rounds.len();
        let mut lines = vec![
            Line::from(match rounds {
                0 => "No rounds yet.".to_string(),
                1 => "1 round so far; Ctrl+P shows it.".to_string(),
                n => format!(
                    "{n} rounds so far; Ctrl+P shows the last, Ctrl+P / Ctrl+N step through them."
                ),
            })
            .bold(),
            Line::from(""),
            Line::from(
                "Type a prompt in the box below and press Enter to send it to every harness.",
            ),
            Line::from(if self.config.main.compare {
                format!(
                    "Each harness's own answer appears here as soon as it's done, and all answers are compared by `{}` once every harness has been quiet for {}s.",
                    self.config.main.command_line(),
                    self.config.main.quiet_secs
                )
            } else {
                "Each harness's own answer appears here as soon as it's done.".to_string()
            }),
            Line::from(if self.config.main.compare {
                "Press Ctrl+R to compare what the harnesses show right now."
            } else {
                "Press Ctrl+R to capture what the harnesses show right now."
            }),
            Line::from(
                "A prompt with ::send, ::wait-model-files, ::wait-file or ::wait-done lines runs as a script.",
            ),
            Line::from("Ctrl+\\ ? shows every key.").dark_gray(),
            Line::from(""),
            Line::from("Commands").bold(),
        ];
        let mut cache = self.templates.borrow_mut();
        // Read again now and then, so a new or edited template shows up.
        if cache
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(2))
        {
            *cache = Some((Instant::now(), crate::commands::list(&self.config.commands)));
        }
        let templates = &cache.as_ref().expect("filled above").1;
        if templates.is_empty() {
            lines.push(Line::from(format!(
                "  No templates in {}.",
                self.config.commands.dir().display()
            )));
        }
        for template in templates {
            lines.push(Line::from(""));
            lines.push(Line::from(format!("  {}", template.usage)).bold());
            // One paragraph per run of comment lines, wrapped to the tab;
            // the first usually repeats the usage before its ": ".
            let text = template.description.join("\n");
            let text = text
                .strip_prefix(&template.usage)
                .and_then(|rest| rest.strip_prefix(':'))
                .unwrap_or(&text);
            for paragraph in text.split("\n\n") {
                let paragraph = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
                let mut chars = paragraph.chars();
                if let Some(first) = chars.next() {
                    let rest: String = chars.collect();
                    lines.push(Line::from(format!("    {}{rest}", first.to_uppercase())));
                }
            }
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from(format!(
                "Templates are read from {}; /NAME without its arguments goes to the harnesses as their own command.",
                self.config.commands.dir().display()
            ))
            .dark_gray(),
        );
        lines
    }

    fn round_lines(&self) -> Vec<(Line<'static>, Option<usize>)> {
        let plain = |lines: Vec<Line<'static>>| lines.into_iter().map(|l| (l, None)).collect();
        let Some(round) = self.rounds.get(self.main_view) else {
            return plain(self.intro_lines());
        };
        let mut lines: Vec<(Line<'static>, Option<usize>)> = plain(vec![
            Line::from(vec![
                Span::raw(format!(
                    "Round {} of {}",
                    self.main_view + 1,
                    self.rounds.len()
                ))
                .bold(),
                Span::raw(format!("  ·  {}", unique(&round.names).join(", "))).dark_gray(),
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
            let headline = round.headline(slot);
            lines.push((
                self.section_header(
                    slot,
                    self.agent_label(round, slot),
                    self.agent_status(round, slot),
                    headline.as_deref(),
                ),
                Some(slot),
            ));
            let unfolded = !self.main_folded.get(slot).copied().unwrap_or(false);
            if unfolded && let Some((path, _)) = &round.outputs[slot] {
                let written = Self::report_written(round, slot).is_some();
                let note = if written { "" } else { " (not written yet)" };
                lines.push((
                    Line::from(format!("    report: {}{note}", path.display())).dark_gray(),
                    None,
                ));
            }
            if unfolded && let Some(answer) = &answer {
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

        if !self.config.main.compare {
            return lines;
        }
        let overall = round.targets.len();
        let state = round.state();
        let status = match &state {
            SummaryState::Waiting if round.targets.len() < 2 => (
                "one harness, nothing to compare (Ctrl+R compares anyway)".to_string(),
                Color::DarkGray,
            ),
            SummaryState::Waiting if round.all_captured() && !self.config.main.auto => (
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
                    self.config.main.command_line(),
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
        if !self.main_folded.get(overall).copied().unwrap_or(false) {
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

    fn draw_main(&mut self, frame: &mut Frame, area: Rect) -> Option<Position> {
        let input = self.main_input.clone();
        let input_height = Self::prompt_box_height(&input, area);
        let [view_area, input_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(input_height)]).areas(area);
        let mut rows = Vec::new();
        let mut hits = Vec::new();
        for (line, section) in self.main_lines() {
            for row in wrap_line(line, view_area.width.max(1) as usize) {
                rows.push(row);
                hits.push(section);
            }
        }
        let max_scroll = rows.len().saturating_sub(view_area.height as usize) as u16;
        self.main_scroll = self.main_scroll.min(max_scroll);
        self.main_hits = hits;
        self.main_area = view_area;
        self.main_rows = rows.iter().map(|l| l.to_string()).collect();
        frame.render_widget(
            Paragraph::new(rows).scroll((self.main_scroll, 0)),
            view_area,
        );
        if let Some((a, b)) = self.main_selection {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            let buf = frame.buffer_mut();
            for y in 0..view_area.height {
                let row = y as usize + self.main_scroll as usize;
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
        self.draw_prompt_box(frame, input_area, &input, " Prompt → ", PromptBox::Main)
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
        if self.on_main() {
            let area = self.main_area;
            if !Self::inside(area, column, row) {
                return false;
            }
            let anchor = (
                (row - area.y) as usize + self.main_scroll as usize,
                column - area.x,
            );
            self.main_selection = None;
            self.drag = Some(Drag::Main {
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
            Some(Drag::Main { anchor, .. }) => {
                let area = self.main_area;
                let (col, row) = Self::clamp_to(area, column, row);
                let head = (row as usize + self.main_scroll as usize, col);
                self.main_selection = Some((anchor, head));
                self.drag = Some(Drag::Main {
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
            Some(Drag::Main { moved: false, .. }) => {
                self.main_selection = None;
                self.main_click(column, row);
            }
            Some(Drag::Main { moved: true, .. }) => {
                if let Some(text) = self.main_selected_text() {
                    let message = self.clipboard.copy(&text);
                    self.flash(message);
                }
            }
            None => {}
        }
    }

    fn main_selected_text(&self) -> Option<String> {
        let (a, b) = self.main_selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let mut out = Vec::new();
        for row in start.0..=end.0.min(self.main_rows.len().saturating_sub(1)) {
            let text = self.main_rows.get(row)?;
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 { end.1 } else { u16::MAX };
            out.push(columns_of(text, from, to).trim_end().to_string());
        }
        let text = out.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    fn main_click(&mut self, column: u16, row: u16) {
        let area = self.main_area;
        if column < area.x || column >= area.right() || row < area.y || row >= area.bottom() {
            return;
        }
        let index = (row - area.y) as usize + self.main_scroll as usize;
        let Some(Some(section)) = self.main_hits.get(index).copied() else {
            return;
        };
        let first_row = index == 0 || self.main_hits[index - 1] != Some(section);
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
        let index = self.main_view.min(index);
        let round = &self.rounds[index];
        if section >= round.targets.len() {
            if self.config.main.compare {
                self.refresh_round();
            }
            return;
        }
        let pane = round.targets[section];
        let name = self.panes[pane].name().to_string();
        let Some((_, mark)) = round.delivered[section] else {
            self.flash(format!("the prompt has not reached {name} yet"));
            return;
        };
        self.state_dirty = true;
        let before = round.answer(section);
        let capture = self.capture_slot(index, section, mark, true);
        self.log_capture(index, section, &capture);
        self.rounds[index].reopened[section] = before;
        self.store_capture(index, section, capture);
        let round = &self.rounds[index];
        let rerun = self.config.main.compare
            && round.compared_unfinished
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

    fn test_main_config() -> crate::rounds::MainConfig {
        crate::rounds::MainConfig {
            compare: true,
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
            auto_replies: crate::config::default_auto_replies(),
            main: test_main_config(),
            commands: Default::default(),
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

    fn main_text(app: &App) -> String {
        app.main_lines()
            .iter()
            .map(|(l, _)| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_main(app: &mut App, needle: &str) {
        let start = Instant::now();
        loop {
            app.deliver_pending();
            app.tick_rounds();
            if main_text(app).contains(needle) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "missing {needle:?} in:\n{}",
                main_text(app)
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
        wait_main(&mut app, "SUMMARY:2");
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
            app.tick_rounds();
            assert!(
                app.rounds[0].captures[1].is_none(),
                "captured while waiting for permission"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(app.needs_input(1));
        assert_eq!(app.tab_label(1).0, "?asker");
        assert!(main_text(&app).contains("waiting for your answer"));
        wait_main(&mut app, "SUMMARY:2");
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
        wait_main(&mut app, "SUMMARY:2");
        let text = main_text(&app);
        assert!(text.contains("▸ ⟳ a  ✓ done  — • answer from a"), "{text}");
        assert!(!text.contains("    • answer from a"), "{text}");
        assert!(text.contains("▾ ⟳ comparison"), "{text}");
        assert_eq!(app.rounds[0].answer(0).unwrap(), "• answer from a");

        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        app.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let text = main_text(&app);
        assert!(
            text.contains("▾ ⟳ a  ✓ done\n    • answer from a") && !text.contains("Ran ls"),
            "{text}"
        );
        app.handle_event(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(!main_text(&app).contains("▾ "));
        app.handle_event(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(!main_text(&app).contains("▸ "));
    }

    #[test]
    fn clicking_a_section_header_toggles_it_and_view_has_no_border() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "click test");
        wait_main(&mut app, "SUMMARY:2");
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
        assert!(main_text(&app).contains("▾ ⟳ b"), "{}", main_text(&app));
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
    fn dragging_in_main_tab_copies_and_click_still_folds() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "copy test");
        wait_main(&mut app, "SUMMARY:2");
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
        assert!(main_text(&app).contains("▾ ⟳ a"));
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
    fn clicking_a_target_in_the_main_prompt_box_toggles_it() {
        let mut app = app(&["a", "b", "c"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        let (x, y) = find(&rows, "✓b");
        click(&mut app, x + 1, y);
        assert_eq!(app.main_input.selected(), [0, 2]);
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        assert!(rows.iter().any(|r| r.contains("✓a b ✓c")), "{rows:#?}");
        type_text(&mut app, "only a and c");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        wait_for(app.pane(0), "got:only a and c");
        wait_for(app.pane(2), "got:only a and c");
        thread::sleep(Duration::from_millis(200));
        assert!(!screen(app.pane(1)).contains("only a and c"));
        assert_eq!(
            app.main_input.selected(),
            [0, 2],
            "selection is kept for the next prompt"
        );
    }

    #[test]
    fn unchecking_a_harness_in_the_main_prompt_box_hides_its_tab() {
        let mut app = app(&["a", "b", "c"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.visible_tabs(), [3, 0, 1, 2]);
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        let (x, y) = find(&rows, "✓a ✓b ✓c");
        click(&mut app, x + 3, y);
        assert_eq!(app.visible_tabs(), [3, 0, 2]);
        assert_eq!(app.pane(1).status(), Status::Running, "it keeps running");
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        assert!(!rows[0].contains(" b"), "{:?}", rows[0]);
        click(&mut app, x + 3, y);
        assert_eq!(app.visible_tabs(), [3, 0, 1, 2], "checking it shows it");

        click(&mut app, x + 3, y);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.composer.selected(), [0, 2], "prompt all leaves it out");
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        type_text(&mut app, "hello");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.visible_tabs(), [3, 0, 1, 2], "a prompt shows it again");
    }

    #[test]
    fn a_matching_screen_gets_its_auto_reply_once() {
        let script = format!("printf '\\033[?2004hThis content can’t be shown\\r\\n'; {ECHO_LOOP}");
        let mut app = app_with(vec![script_harness("codex", &script), cat_harness("b")]);
        wait_for(app.pane(0), "be shown");
        assert!(!app.auto_reply(), "not while it may still be working");
        thread::sleep(AUTO_REPLY_QUIET);
        assert!(app.auto_reply());
        wait_for(app.pane(0), "got:continue");
        thread::sleep(AUTO_REPLY_QUIET);
        assert!(!app.auto_reply(), "one reply while the match is on screen");
        assert!(!screen(app.pane(1)).contains("continue"));
    }

    #[test]
    fn clicking_a_target_in_the_composer_toggles_it() {
        let mut app = app(&["a", "b"]);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE));
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        let (x, y) = find(&rows, "✓a");
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
            auto_replies: crate::config::default_auto_replies(),
            main: test_main_config(),
            commands: Default::default(),
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
            !rows[0].contains("YOLO") && rows[0].ends_with("✕exit"),
            "{rows:#?}"
        );
        assert!(rows[9].starts_with(" main  YOLO "), "{rows:#?}");
    }

    #[test]
    fn exit_button_asks_first_then_quits() {
        let mut app = app(&["a"]);
        let rows = buffer_rows(&draw_app(&mut app, 80, 10));
        let (x, y) = find(&rows, "✕exit");
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
        let summary = app.main_tab_label().0;
        assert_eq!(summary, " main");
        compose_all(&mut app, "q");
        assert_eq!(app.main_tab_label().0.width(), summary.width());
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
        wait_main(&mut app, "SUMMARY:");
        wait_main(&mut app, "captured unfinished, still working");
        wait_main(&mut app, "SUMMARY:2");
        assert!(app.rounds[0].all_finished());
        assert!(!app.rounds[0].compared_unfinished);
        assert!(app.rounds[0].answer(1).unwrap().ends_with("• final answer"));
        assert!(
            main_text(&app).contains("▸ ⟳ slow  ✓ done  — • final answer"),
            "{}",
            main_text(&app)
        );
    }

    #[test]
    fn done_harness_that_resumes_is_recaptured_and_compared_again() {
        let pause = "printf '\\033[?2004h'; stty -echo; read l; printf '› %s\\r\\n• launched the tests\\r\\n' \"$l\"; sleep 2.5; printf '• final answer\\r\\n› Ask me\\r\\n'; sleep 30";
        let mut app = app_with(vec![
            replying_harness("fast"),
            script_harness("pause", pause),
        ]);
        app.config.main.args = vec![
            "-c".into(),
            "printf 'FINAL:'; grep -c 'final answer'".into(),
        ];
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "run the tests");
        app.select(app.main_tab());
        wait_main(&mut app, "FINAL:0");
        assert!(
            app.rounds[0]
                .answer(1)
                .unwrap()
                .ends_with("launched the tests")
        );
        wait_main(&mut app, "FINAL:1");
        assert!(app.rounds[0].all_finished());
        assert!(app.rounds[0].answer(1).unwrap().ends_with("• final answer"));
    }

    #[test]
    fn refresh_button_recaptures_one_harness_and_compares_again() {
        // `late` goes quiet long enough to count as done, then prints more
        // output that is too quick to be noticed before the refresh.
        let late = "printf '\\033[?2004h'; stty -echo; read l; printf '› %s\\r\\n• first take\\r\\n' \"$l\"; read x; printf '• better take\\r\\n'; sleep 30";
        let mut app = app_with(vec![replying_harness("fast"), script_harness("late", late)]);
        app.config.main.args = vec![
            "-c".into(),
            "printf 'BETTER:'; grep -c 'better take'".into(),
        ];
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "go");
        app.select(app.main_tab());
        wait_main(&mut app, "BETTER:0");
        // Make the round's answer change without the harness being reopened.
        app.rounds[0].followed_up[1] = true;
        app.pane(1).write(b"\r");
        wait_for(app.pane(1), "better take");
        let rows = buffer_rows(&draw_app(&mut app, 80, 30));
        let header = rows.iter().position(|r| r.starts_with("▸ ⟳ late")).unwrap() as u16;
        click(&mut app, 2, header);
        assert!(app.rounds[0].answer(1).unwrap().ends_with("• better take"));
        wait_main(&mut app, "BETTER:1");

        app.main_focus = 0;
        app.handle_event(key(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(app.rounds[0].captures[0].as_ref().unwrap().finished);
    }

    #[test]
    fn main_tab_answer_keeps_the_harness_colors() {
        let colored = "printf '\\033[?2004h'; stty -echo; read l; printf '› %s\\r\\n• plain \\033[31mred\\033[0m\\r\\n' \"$l\"; sleep 30";
        let mut app = app_with(vec![replying_harness("a"), script_harness("c", colored)]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "colors");
        app.select(app.main_tab());
        wait_main(&mut app, "SUMMARY:2");
        let toggle = app.main_folded.get(1).copied().unwrap_or(false);
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
    fn harness_with_an_output_file_is_done_once_the_file_is_written() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("review-Rev-Model.md");
        std::fs::write(&file, "verdict: stale report from an older run\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        let report =
            "---\nverdict: approve\n---\n## Findings\n### Rev-Model-F1\n- severity: minor\n";
        let script = format!(
            "printf '\\033[?2004h'; read l; printf 'started\\r\\n'; sleep 2.2; printf '%b' '{}' > '{}'; printf 'wrote it\\r\\n'; sleep 30",
            report.replace('\n', "\\n"),
            file.display()
        );
        let mut reviewer = script_harness("rev", &script);
        reviewer.model = Some("Rev Model".into());
        let mut app = app_with(vec![reviewer, replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        let prompt = format!(
            "REVIEW_DIR={}\nOUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md\nReview it.",
            dir.display()
        );
        compose_all(&mut app, &prompt);
        assert_eq!(app.rounds[0].outputs[0].as_ref().unwrap().0, file);
        assert!(app.rounds[0].outputs[1].is_none(), "b has no MODEL_ID");

        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1800) {
            app.deliver_pending();
            app.tick_rounds();
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            app.rounds[0].captures[0].is_none(),
            "quiet, but no new report yet"
        );
        let text = main_text(&app);
        assert!(text.contains("finishing its report"), "{text}");
        app.toggle_section(0);
        let text = main_text(&app);
        assert!(text.contains("(not written yet)"), "{text}");

        let start = Instant::now();
        while app.rounds[0].captures[0].is_none() {
            assert!(start.elapsed() < Duration::from_secs(10), "no capture");
            app.tick_rounds();
            thread::sleep(Duration::from_millis(50));
        }
        let capture = app.rounds[0].captures[0].as_ref().unwrap();
        assert!(capture.finished);
        assert_eq!(capture.report.as_ref().unwrap().0, file);
        assert_eq!(app.rounds[0].answer(0).unwrap(), report.trim_end());
        assert_eq!(
            app.rounds[0].headline(0).unwrap(),
            "approve · 1 finding (1 minor)"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn tick_until(app: &mut App, what: &str, mut done: impl FnMut(&App) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "timed out waiting for {what}:\n{}",
                main_text(app)
            );
            app.deliver_pending();
            app.tick_run();
            app.tick_rounds();
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn submit_in_main(app: &mut App, text: &str) {
        app.select(app.main_tab());
        app.handle_event(Event::Paste(text.into()));
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn script_sends_waits_for_model_files_then_sends_on() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let writer = |name: &str, model: &str, delay: &str| {
            let file = dir.join(format!("r-{}.md", model.replace(' ', "-")));
            let mut h = script_harness(
                name,
                &format!(
                    "printf '\\033[?2004h'; read l; sleep {delay}; printf 'report\\n' > '{}'; sleep 30",
                    file.display()
                ),
            );
            h.model = Some(model.into());
            h
        };
        let mut app = app_with(vec![
            writer("a", "A M", "0.3"),
            writer("b", "B M", "1.5"),
            cat_harness("c"),
        ]);
        thread::sleep(Duration::from_millis(200));
        let script = format!(
            "DIR={}\n::# reviews\n::send a b\nOUT=$DIR/r-$MODEL_ID.md\nwrite a report\n\n::wait-model-files $OUT\n\n::send c\nmerge them\n::wait-done",
            dir.display()
        );
        submit_in_main(&mut app, &script);
        assert_eq!(app.main_input.text(), "", "a valid script leaves the box");
        let text = main_text(&app);
        assert!(text.contains("✓ ::send a b"), "{text}");
        assert!(text.contains("● ::wait-model-files $OUT"), "{text}");
        assert!(text.contains("0/2 written; waiting for a, b"), "{text}");

        tick_until(&mut app, "a's report", |a| {
            main_text(a).contains("1/2 written; waiting for b")
        });
        assert!(
            !screen(app.pane(2)).contains("merge them"),
            "c waits for both files"
        );
        tick_until(&mut app, "the send to c", |a| {
            screen(a.pane(2)).contains("got:merge them")
        });
        let sent = app.pane(2).text_since(0, 50);
        assert!(
            sent.contains(&format!("got:DIR={}", dir.display())),
            "{sent}"
        );
        assert!(
            !sent.contains("OUT="),
            "only the header and c's own text: {sent}"
        );
        tick_until(&mut app, "the end", |a| !a.run.as_ref().unwrap().running());
        assert!(main_text(&app).contains("Script finished"));
        assert_eq!(app.rounds.len(), 1, "a script is one round");
        let round = &app.rounds[0];
        assert_eq!(round.names, ["a", "b", "c"]);
        assert_eq!(
            round.prompts[2],
            format!("DIR={}\nmerge them", dir.display())
        );
        let text = main_text(&app);
        assert!(text.contains("Round 1 of 1  ·  a, b, c"), "{text}");
        assert!(text.contains("c · step 2"), "{text}");
        assert!(text.contains("a · step 1"), "{text}");
        assert_ne!(round.state(), SummaryState::Superseded);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn script_waits_can_be_skipped_and_scripts_cancelled() {
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        submit_in_main(
            &mut app,
            "::send a\nhello\n::wait-file /nonexistent/never.md\n::send b\nafter the wait\n::wait-file /nonexistent/again.md",
        );
        assert!(main_text(&app).contains("● ::wait-file /nonexistent/never.md"));
        app.handle_event(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(main_text(&app).contains("⤼ ::wait-file /nonexistent/never.md"));
        tick_until(&mut app, "the second send", |a| {
            screen(a.pane(1)).contains("got:after the wait")
        });
        app.handle_event(key(KeyCode::Char('x'), KeyModifiers::CONTROL));
        let text = main_text(&app);
        assert!(text.contains("Script cancelled"), "{text}");
        assert!(
            text.contains("✗ ::wait-file /nonexistent/again.md"),
            "{text}"
        );
    }

    #[test]
    fn prompt_box_labels_leave_out_the_versions() {
        let names = [
            "fable-5.2",
            "opus-5.5",
            "gpt-6-astra",
            "gemini-4-carbon",
            "kimi-k3",
            "muse-wonder",
            "grok-4.7",
            "4o",
        ];
        assert_eq!(
            target_labels(&names),
            [
                "fable",
                "opus",
                "gpt-astra",
                "gemini-carbon",
                "kimi",
                "muse-wonder",
                "grok",
                "4o"
            ]
        );
        assert_eq!(
            target_labels(&["opus-5.5", "opus-4.8", "glm-5.3"]),
            ["opus-5.5", "opus-4.8", "glm"],
            "the same without versions keep them"
        );
    }

    #[test]
    fn script_all_means_the_checked_harnesses() {
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        app.main_input.toggle(1);
        assert_eq!(app.main_input.selected(), [0]);
        submit_in_main(&mut app, "::send all\nhello\n::wait-done");
        assert_eq!(app.run.as_ref().unwrap().all, [0]);
        tick_until(&mut app, "the send to a", |a| {
            screen(a.pane(0)).contains("got:hello")
        });
        tick_until(&mut app, "the end", |a| !a.run.as_ref().unwrap().running());
        assert!(!screen(app.pane(1)).contains("hello"), "b is not checked");
        assert_eq!(app.rounds[0].names, ["a"]);
    }

    #[test]
    fn main_tab_marker_follows_the_script() {
        use unicode_width::UnicodeWidthStr;
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        submit_in_main(
            &mut app,
            "::send a\nhello\n::wait-file /nonexistent/x.md\n::send b\nhi\n::wait-done",
        );
        assert_eq!(app.main_tab_label().0, "⏸main", "waiting for a file");
        app.select(0);
        app.skip_wait();
        assert_eq!(app.main_tab_label().0, "▶main", "waiting for b to finish");
        tick_until(&mut app, "the end", |a| !a.run.as_ref().unwrap().running());
        assert_eq!(app.main_tab_label().0, "✓main", "finished, not looked at");
        app.select(app.main_tab());
        assert_ne!(app.main_tab_label().0, "✓main", "looked at");
        for marker in ['▶', '⏸', '✓', '✗'] {
            assert_eq!(
                format!("{marker}main").width(),
                "main".width() + 1,
                "{marker}"
            );
        }
    }

    #[test]
    fn rounds_and_a_paused_script_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state.json");
        let writer = |name: &str, model: &str| {
            let file = dir.join(format!("r-{}.md", model.replace(' ', "-")));
            let mut h = script_harness(
                name,
                &format!(
                    "printf '\\033[?2004h'; read l; printf 'verdict: approve\\n' > '{}'; sleep 30",
                    file.display()
                ),
            );
            h.model = Some(model.into());
            h
        };
        let mut silent = cat_harness("b");
        silent.model = Some("B M".into());
        let script = format!(
            "DIR={}\n::send a b\nOUTPUT_FILE=$DIR/r-$MODEL_ID.md\nreview it\n::wait-model-files $OUTPUT_FILE\n::send a\nall done",
            dir.display()
        );

        let mut first = app_with(vec![writer("a", "A M"), silent]);
        first.use_state_file(state.clone(), false);
        thread::sleep(Duration::from_millis(200));
        submit_in_main(&mut first, &script);
        tick_until(&mut first, "a's report", |a| {
            a.rounds[0].captures[0].is_some() && a.run.as_ref().unwrap().satisfied == [0]
        });
        first.save_state();
        drop(first);

        // Same names, new sessions; this time b writes its report.
        let mut second = app_with(vec![writer("a", "A M"), writer("b", "B M")]);
        second.use_state_file(state.clone(), true);
        assert_eq!(second.rounds.len(), 1);
        let capture = second.rounds[0].captures[0].as_ref().unwrap();
        assert_eq!(capture.text, "verdict: approve\n");
        assert!(
            second.rounds[0].captures[1].is_none(),
            "b's report may still come"
        );
        assert_eq!(second.run.as_ref().unwrap().interrupted, Some(vec![1]));
        second.select(second.main_tab());
        let text = main_text(&second);
        assert!(text.contains("Script paused by a restart"), "{text}");
        assert!(text.contains("again to b and resumes"), "{text}");
        assert_eq!(second.main_tab_label().0, "?main");
        assert!(text.contains("1 round so far; Ctrl+P shows it"), "{text}");
        second.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(main_text(&second).contains("Round 1 of 1"));
        second.handle_event(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(main_text(&second).contains("1 round so far"));
        thread::sleep(Duration::from_millis(200));
        assert!(!second.tick_run(), "paused until Ctrl+G");

        second.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(second.rounds.len(), 1, "the script's round goes on");
        assert!(
            second.rounds[0].delivered[0].is_some(),
            "a isn't asked again"
        );
        tick_until(&mut second, "b's resent prompt", |a| {
            a.rounds[0].delivered[1].is_some()
        });
        tick_until(&mut second, "the end", |a| {
            !a.run.as_ref().unwrap().running()
        });
        tick_until(&mut second, "b's report", |a| {
            a.rounds[0].captures[1].is_some()
        });
        let round = &second.rounds[0];
        assert_eq!(round.names, ["a", "b", "a"], "then the next step");
        assert_eq!(
            round.captures[1].as_ref().unwrap().text,
            "verdict: approve\n",
            "b's resent prompt counts for its section"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn review_runs_the_review_template() {
        let mut app = app_with(vec![cat_harness("opus-5.5"), cat_harness("b")]);
        app.config.commands.variables = test_review_variables();
        thread::sleep(Duration::from_millis(200));
        submit_in_main(&mut app, "/review-full percona/8.4..PS-1");
        assert_eq!(app.main_input.text(), "");
        let text = main_text(&app);
        assert!(text.contains("✓ ::send all"), "{text}");
        assert!(text.contains("● ::wait-model-files $OUTPUT_FILE"), "{text}");
        assert!(text.contains("::send opus-5.5"), "{text}");
        tick_until(&mut app, "the prompt", |a| {
            let screen = screen(a.pane(1));
            screen.contains("TARGET=PS-1") && screen.contains("$PROMPT_DIR/review-full.md")
        });
    }

    #[test]
    fn a_template_mistake_names_the_template() {
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        app.config.commands.variables = test_review_variables();
        submit_in_main(&mut app, "/review-full percona/8.4..PS-1");
        assert_eq!(app.main_input.text(), "/review-full percona/8.4..PS-1");
        let message = &app.message.as_ref().unwrap().0;
        assert!(
            message.contains("commands/review-full.txt: line 23: unknown target opus-5.5"),
            "{message}"
        );

        app.config.commands.variables.clear();
        app.main_input.clear();
        submit_in_main(&mut app, "/review-full percona/8.4..PS-1");
        let message = &app.message.as_ref().unwrap().0;
        assert!(
            message.contains("commands/review-full.txt: line 16: $REVIEW_DIR is not set"),
            "{message}"
        );
    }

    #[test]
    fn a_long_error_is_shown_in_full() {
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        let error = format!("{}: the end", "x".repeat(150));
        app.flash_error(error.clone());
        let rows = buffer_rows(&draw_app(&mut app, 60, 20));
        let shown: String = rows[16..19].concat();
        assert_eq!(shown.trim_end(), error, "{rows:#?}");
    }

    fn test_review_variables() -> indexmap::IndexMap<String, String> {
        indexmap::IndexMap::from([
            ("REVIEW_DIR".into(), "/nonexistent/reviews/$TARGET".into()),
            ("PROMPT_DIR".into(), "/nonexistent/prompts".into()),
        ])
    }

    #[test]
    fn a_script_with_a_mistake_is_not_sent_and_stays_in_the_box() {
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        let script = "TARGET=PS-1\n::send all\nREVIEW_DIR=/r/$TARGET";
        submit_in_main(&mut app, script);
        assert_eq!(app.main_input.text(), script);
        assert!(app.run.is_none() && app.rounds.is_empty());
        let message = &app.message.as_ref().unwrap().0;
        assert!(
            message.contains("line 2: ::send all sends only variables"),
            "{message}"
        );
        thread::sleep(Duration::from_millis(300));
        assert!(!screen(app.pane(0)).contains("got:"));
    }

    #[test]
    fn without_compare_there_is_no_comparison() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        app.config.main.compare = false;
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "hello there");
        assert_eq!(app.main_tab_label().0, "⧗main");
        tick_until(&mut app, "both answers", |a| a.rounds[0].all_finished());
        assert_eq!(app.main_tab_label().0, "●main", "new answers not seen yet");
        app.select(app.main_tab());
        let text = main_text(&app);
        assert!(!text.contains("⟳ comparison"), "{text}");
        assert_eq!(
            app.rounds[0].state(),
            SummaryState::Waiting,
            "no summarizer ran"
        );
        assert_eq!(app.main_sections(), 2);
        app.handle_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.rounds[0].all_finished());
        assert_eq!(app.rounds[0].state(), SummaryState::Waiting);
    }

    #[test]
    fn a_prompt_sent_during_a_script_leaves_the_scripts_round_alone() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-during-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("go");
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        submit_in_main(
            &mut app,
            &format!(
                "::send a b\nreview\n::wait-file {}\n::send a\nmerge",
                file.display()
            ),
        );
        tick_until(&mut app, "the reviews", |a| {
            a.rounds[0].delivered.iter().all(Option::is_some)
        });
        submit_in_main(&mut app, "meanwhile");
        assert_eq!(app.rounds.len(), 2);
        assert_ne!(app.rounds[0].state(), SummaryState::Superseded);
        std::fs::write(&file, "x").unwrap();
        tick_until(&mut app, "the merge", |a| {
            a.rounds[0].delivered.get(2).is_some_and(Option::is_some)
        });
        assert_eq!(app.rounds[0].names, ["a", "b", "a"]);
        assert_eq!(
            app.rounds[1].names,
            ["a", "b"],
            "meanwhile keeps its own round"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn script_steps_show_only_above_the_scripts_round() {
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        submit_in_main(&mut app, "::send a b\nhello");
        tick_until(&mut app, "the script", |a| {
            !a.run.as_ref().unwrap().running()
        });
        assert!(main_text(&app).contains("Script finished"));
        submit_in_main(&mut app, "a prompt of my own");
        assert_eq!(app.main_view, 1);
        assert!(!main_text(&app).contains("Script"), "{}", main_text(&app));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(
            !main_text(&app).contains("Script"),
            "not on the intro once finished"
        );
        app.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.main_view, 0);
        assert!(main_text(&app).contains("Script finished"));
    }

    #[test]
    fn report_is_taken_when_its_harness_moved_on_to_the_next_prompt() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-moved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("combined.md");
        // Writes its report, then keeps printing as if working on more.
        let script = format!(
            "printf '\\033[?2004h'; read l; printf 'verdict: approve\\n' > '{}'; while true; do printf 'working\\r\\n'; sleep 0.2; done",
            file.display()
        );
        let mut app = app_with(vec![script_harness("opus", &script), cat_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        submit_in_main(
            &mut app,
            &format!(
                "::send opus\nREVIEW_COMBINED={}/combined.md\nmerge\n::wait-file $REVIEW_COMBINED\n::send opus\nfix it",
                dir.display()
            ),
        );
        tick_until(&mut app, "the second send", |a| {
            a.rounds[0].targets.len() == 2
        });
        tick_until(&mut app, "the merge round's report", |a| {
            a.rounds[0].captures[0].is_some()
        });
        let capture = app.rounds[0].captures[0].as_ref().unwrap();
        assert!(capture.finished && capture.report.is_some());
        assert_eq!(app.rounds[0].answer(0).unwrap(), "verdict: approve");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_follow_up_prompt_does_not_reopen_the_round() {
        let mut app = app_with(vec![replying_harness("a"), replying_harness("b")]);
        thread::sleep(Duration::from_millis(200));
        compose_all(&mut app, "first");
        wait_main(&mut app, "SUMMARY:2");
        app.select(0);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('s'), KeyModifiers::NONE));
        type_text(&mut app, "second");
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        wait_for(app.pane(0), "› second");
        thread::sleep(Duration::from_millis(300));
        app.tick_rounds();
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
        let lines = app.pane(0).text_since(0, 100);
        assert!(
            lines.contains("got:MODEL_ID=Test-Model-high\ngot:first"),
            "no blank line after MODEL_ID: {lines}"
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
            app.tick_rounds();
            assert!(
                app.rounds[0].captures[1].is_none(),
                "captured while background work was running"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            main_text(&app).contains("working in the background"),
            "{}",
            main_text(&app)
        );
        assert_eq!(app.tab_label(1).0, "●bg");
        wait_main(&mut app, "SUMMARY:2");
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
    fn main_tab_ctrl_r_takes_current_screens() {
        let mut app = app(&["a", "b"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(app.on_main());
        let text = main_text(&app);
        assert!(text.contains("No rounds yet"), "{text}");
        assert!(text.contains("  /review-full BASE..TARGET [NOTES...]"), "{text}");
        assert!(text.contains("  /plan TARGET TASK..."), "{text}");
        assert!(text.contains("  /plan-full TARGET TASK..."), "{text}");
        assert!(
            text.contains("    Every checked model reviews BASE..TARGET"),
            "the usage isn't repeated: {text}"
        );
        app.handle_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        wait_main(&mut app, "SUMMARY:2");
    }

    #[test]
    fn typing_in_main_tab_sends_to_all_harnesses() {
        let mut app = app(&["a", "b", "c"]);
        thread::sleep(Duration::from_millis(200));
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(app.on_main());
        type_text(&mut app, "ready");
        app.handle_event(Event::Paste(" set".into()));
        assert_eq!(app.main_input.text(), "ready set");
        app.handle_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
        assert!(!app.on_main(), "Alt+digit still switches tabs");
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.on_main());
        assert_eq!(app.main_input.text(), "");
        for i in 0..3 {
            wait_for(app.pane(i), "got:ready set");
        }
        assert_eq!(app.rounds.len(), 1);
        assert_eq!(app.rounds[0].targets, [0, 1, 2]);
    }

    #[test]
    fn main_tab_keeps_commands_on_control_keys() {
        let mut app = app(&["a", "b"]);
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        type_text(&mut app, "rpn");
        assert_eq!(app.main_input.text(), "rpn");
        assert!(app.rounds.is_empty());
        app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.main_input.text(), "");
        app.handle_event(key(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.main_scroll > 0);
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
        assert!(rows.iter().any(|r| r.contains("✓asks ✓ready")), "{rows:#?}");
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("dialog open") || r.contains("(starting)")),
            "{rows:#?}"
        );
        compose_all(&mut app, "hi");
        assert_eq!(app.tab_label(0).0, "⚑asks");
        assert!(
            main_text(&app).contains("asks  waiting for you to trust the folder"),
            "{}",
            main_text(&app)
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
            auto_replies: crate::config::default_auto_replies(),
            main: test_main_config(),
            commands: Default::default(),
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
        // on the main tab.
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
        assert_eq!(app.active(), 1, "tab 1 is the main tab, tab 3 is harness b");
        app.handle_event(key(KeyCode::Char('4'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.active(), 2);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(
            app.on_main(),
            "next after the last harness wraps to the main tab"
        );
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.active(), 0);
        app.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(app.on_main());
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
        assert_eq!(app.main_input.selected(), [0]);
        app.select(3);
        let rows = buffer_rows(&draw_app(&mut app, 100, 20));
        assert!(
            !rows[0].contains(" b ") && !rows[0].contains('c'),
            "{:?}",
            rows[0]
        );
        app.handle_event(key(KeyCode::Char('3'), KeyModifiers::ALT));
        assert!(app.on_main(), "Alt+3 is not a hidden tab");

        let (x, y) = find(&rows, "✓a b c");
        click(&mut app, x + 5, y);
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
    fn main_is_the_first_tab_and_the_starting_one() {
        let mut app = app(&["a", "b"]);
        let rows = buffer_rows(&draw_app(&mut app, 80, 10));
        let summary = rows[0].find("main").unwrap();
        assert!(summary < rows[0].find('a').unwrap(), "{:?}", rows[0]);
        click(&mut app, summary as u16 + 1, 0);
        assert!(app.on_main());

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
        assert!(fresh.on_main());
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
            auto_replies: crate::config::default_auto_replies(),
            main: test_main_config(),
            commands: Default::default(),
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

    #[test]
    fn switching_tabs_leaves_scroll_mode() {
        let mut app = app_with(vec![cat_harness("a"), cat_harness("b")]);
        thread::sleep(Duration::from_millis(300));
        app.select(0);
        app.handle_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('['), KeyModifiers::NONE));
        assert_eq!(app.mode(), Mode::Scroll);
        // As a click on the main tab's label in the tab bar does.
        app.select_position(0);
        assert!(app.on_main());
        assert_eq!(app.mode(), Mode::Normal);
        app.handle_event(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.main_input.text(), "x");
    }
}
