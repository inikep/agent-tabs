//! Rounds and the script, saved so that they survive a restart of
//! agent-tabs. One state file per working directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use ratatui::style::Style;
use serde::{Deserialize, Serialize};

use crate::app::StepState;
use crate::script::Step;

const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    pub rounds: Vec<SavedRound>,
    pub run: Option<SavedRun>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedRound {
    pub prompt: String,
    pub names: Vec<String>,
    /// Each section's prompt and script step; empty in older files.
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub steps: Vec<usize>,
    /// When the prompt reached each harness.
    pub delivered: Vec<Option<SystemTime>>,
    pub captures: Vec<Option<SavedCapture>>,
    pub state: SavedState,
    pub started_at: u64,
    pub compared_unfinished: bool,
    pub followed_up: Vec<bool>,
    pub reopened: Vec<Option<String>>,
    pub outputs: Vec<Option<(PathBuf, SystemTime)>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedCapture {
    pub text: String,
    pub finished: bool,
    pub model: Option<String>,
    /// Each line's styles as runs of (count, style).
    pub styles: HashMap<String, Vec<(usize, Style)>>,
    pub report: Option<(PathBuf, SystemTime)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SavedState {
    Waiting,
    Done { text: String, took_secs: u64 },
    Failed(String),
    Superseded,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedRun {
    pub steps: Vec<Step>,
    pub states: Vec<StepState>,
    pub cancelled: bool,
    pub last_send: Option<(Vec<String>, SystemTime)>,
    pub started: SystemTime,
    pub seen: bool,
    /// Harnesses the current wait already counted, and those it still waits for.
    pub satisfied: Vec<String>,
    pub pending: Vec<String>,
    /// The script's round, as an index into the saved rounds.
    #[serde(default)]
    pub round: Option<usize>,
}

pub fn compress_styles(styles: &[Style]) -> Vec<(usize, Style)> {
    let mut runs: Vec<(usize, Style)> = Vec::new();
    for style in styles {
        match runs.last_mut() {
            Some((n, last)) if last == style => *n += 1,
            _ => runs.push((1, *style)),
        }
    }
    runs
}

pub fn expand_styles(runs: &[(usize, Style)]) -> Vec<Style> {
    runs.iter()
        .flat_map(|(n, style)| std::iter::repeat_n(*style, *n))
        .collect()
}

/// `$XDG_STATE_HOME/agent-tabs/<cwd>.json`, with `~/.local/state` as the
/// default state home and the working directory's slashes turned into `%`.
pub fn path_for(cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let name = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "%");
    Some(home.join("agent-tabs").join(format!("{name}.json")))
}

pub fn load(path: &Path) -> Result<Option<Saved>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let saved: Saved =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok((saved.version == VERSION).then_some(saved))
}

/// Writes the state to a temporary file and renames it into place, so a
/// crash never leaves half a file.
pub fn save(path: &Path, rounds: Vec<SavedRound>, run: Option<SavedRun>) -> Result<()> {
    let saved = Saved {
        version: VERSION,
        rounds,
        run,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&saved)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn styles_round_trip_as_runs() {
        let red = Style::default().fg(Color::Red);
        let plain = Style::default();
        let styles = vec![plain, plain, red, red, red, plain];
        let runs = compress_styles(&styles);
        assert_eq!(runs, vec![(2, plain), (3, red), (1, plain)]);
        assert_eq!(expand_styles(&runs), styles);
    }

    #[test]
    fn state_file_is_named_after_the_directory() {
        let path = path_for(Path::new("/no/such/dir")).unwrap();
        assert!(
            path.ends_with("agent-tabs/no%such%dir.json"),
            "{}",
            path.display()
        );
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-state-{}", std::process::id()));
        let path = dir.join("state.json");
        assert!(load(&path).unwrap().is_none());
        save(&path, Vec::new(), None).unwrap();
        let saved = load(&path).unwrap().unwrap();
        assert!(saved.rounds.is_empty() && saved.run.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
