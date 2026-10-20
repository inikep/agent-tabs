use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::{Config as TermConfig, TermMode};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::config::Harness;
use crate::keys::bracketed_paste;

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
    size: Size,
}

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
                size,
            },
            Err(err) => {
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
                    failure: Some(format!("{err:#}")),
                    size,
                }
            }
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

    pub fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.write(&bracketed_paste(text, bracketed));
    }

    pub fn send_prompt(&self, text: &str, submit_delay: Duration) {
        self.scroll(Scroll::Bottom);
        self.paste(text);
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
