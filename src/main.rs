mod app;
mod clipboard;
mod commands;
mod composer;
mod config;
mod keys;
mod pane;
mod render;
mod rounds;
mod script;
mod state;

use std::io::{Write, stdout};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

use crate::app::App;
use crate::config::Config;
use crate::pane::Size;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Run claude, codex, agy and cursor-agent side by side in tabs and broadcast prompts"
)]
struct Cli {
    #[arg(
        long,
        help = "Config file (default: ~/.config/agent-tabs/config.toml if present)"
    )]
    config: Option<PathBuf>,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Only start these harnesses, e.g. --only claude,codex"
    )]
    only: Vec<String>,

    #[arg(
        long,
        help = "Working directory for all harnesses (default: current directory)"
    )]
    cwd: Option<PathBuf>,

    #[arg(
        long,
        help = "Write each harness's raw output and every composed prompt to this directory"
    )]
    log_dir: Option<PathBuf>,

    #[arg(long, help = "Print the effective configuration as TOML and exit")]
    print_config: bool,

    #[arg(
        long,
        help = "Start every harness in its approve-everything mode (claude/agy --dangerously-skip-permissions, codex --dangerously-bypass-approvals-and-sandbox, cursor-agent --yolo, opencode --auto)"
    )]
    yolo: bool,

    #[arg(
        long,
        help = "Start without the rounds and script saved by the last run in this directory (they are still saved from now on)"
    )]
    no_restore: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref())?;
    config.select(&cli.only)?;
    if cli.yolo {
        config.yolo = true;
    }
    if cli.print_config {
        print!("{}", config.to_toml());
        return Ok(());
    }
    let cwd = match cli.cwd {
        Some(dir) => dir
            .canonicalize()
            .with_context(|| format!("working directory {}", dir.display()))?,
        None => std::env::current_dir()?,
    };
    let state_path = state::path_for(&cwd);

    let mut terminal = ratatui::init();
    let keyboard_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    let mut out = stdout();
    let _ = execute!(out, EnableBracketedPaste);
    if config.mouse {
        let _ = execute!(out, EnableMouseCapture);
    }
    if keyboard_enhanced {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let mouse = config.mouse;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(keyboard_enhanced, mouse);
        previous_hook(info);
    }));

    let area = match terminal.size() {
        Ok(area) => area,
        Err(err) => {
            restore_terminal(keyboard_enhanced, mouse);
            return Err(err.into());
        }
    };
    let mut app = match App::new(
        config,
        cwd,
        cli.log_dir,
        Size {
            cols: area.width,
            rows: area.height,
        },
    ) {
        Ok(app) => app,
        Err(err) => {
            restore_terminal(keyboard_enhanced, mouse);
            return Err(err);
        }
    };
    if let Some(path) = state_path {
        app.use_state_file(path, !cli.no_restore);
    }
    let result = app.run(&mut terminal);
    app.save_state();
    restore_terminal(keyboard_enhanced, mouse);
    app.shutdown();
    result
}

fn restore_terminal(keyboard_enhanced: bool, mouse: bool) {
    let mut out = stdout();
    if mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, DisableBracketedPaste);
    if keyboard_enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = out.flush();
    std::thread::sleep(std::time::Duration::from_millis(30));
    discard_pending_input();
    ratatui::restore();
}

fn discard_pending_input() {
    while ratatui::crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        if ratatui::crossterm::event::read().is_err() {
            break;
        }
    }
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}
