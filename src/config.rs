use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Harness {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_remove: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_submit_delay_ms")]
    pub submit_delay_ms: u64,
    #[serde(default = "default_scrollback")]
    pub scrollback: usize,
    #[serde(default = "default_mouse")]
    pub mouse: bool,
    #[serde(default = "default_dialog_patterns")]
    pub dialog_patterns: Vec<String>,
    #[serde(rename = "harness", default = "default_harnesses")]
    pub harnesses: Vec<Harness>,
}

fn default_submit_delay_ms() -> u64 {
    150
}

fn default_scrollback() -> usize {
    20_000
}

fn default_mouse() -> bool {
    true
}

pub fn default_dialog_patterns() -> Vec<String> {
    [
        r"(?i)quick safety check",
        r"(?i)is this a project you created or one you trust",
        r"(?i)trust this folder\?",
        r"(?i)do you trust the contents",
        r"(?i)workspace trust required",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn harness(name: &str, command: &str) -> Harness {
    Harness {
        name: name.to_string(),
        command: command.to_string(),
        args: Vec::new(),
        env: BTreeMap::new(),
        env_remove: vec![
            "CLAUDECODE".to_string(),
            "CLAUDE_CODE_ENTRYPOINT".to_string(),
            "CLAUDE_CODE_CHILD_SESSION".to_string(),
        ],
    }
}

fn default_harnesses() -> Vec<Harness> {
    vec![
        harness("claude", "claude"),
        harness("codex", "codex"),
        harness("agy", "agy"),
        harness("cursor", "cursor-agent"),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            submit_delay_ms: default_submit_delay_ms(),
            scrollback: default_scrollback(),
            mouse: default_mouse(),
            dialog_patterns: default_dialog_patterns(),
            harnesses: default_harnesses(),
        }
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("agent-tabs").join("config.toml"))
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => match default_config_path() {
                Some(path) if path.exists() => path,
                _ => return Ok(Self::default()),
            },
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.harnesses.is_empty(), "config defines no harnesses");
        self.dialog_regex()?;
        let mut seen = std::collections::HashSet::new();
        for h in &self.harnesses {
            anyhow::ensure!(!h.name.trim().is_empty(), "harness with empty name");
            anyhow::ensure!(
                !h.command.trim().is_empty(),
                "harness {} has empty command",
                h.name
            );
            anyhow::ensure!(
                seen.insert(h.name.clone()),
                "duplicate harness name {}",
                h.name
            );
        }
        Ok(())
    }

    pub fn select(&mut self, only: &[String]) -> Result<()> {
        if only.is_empty() {
            return Ok(());
        }
        for name in only {
            anyhow::ensure!(
                self.harnesses.iter().any(|h| &h.name == name),
                "unknown harness {name}; configured: {}",
                self.harnesses
                    .iter()
                    .map(|h| h.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        self.harnesses.retain(|h| only.contains(&h.name));
        Ok(())
    }

    pub fn dialog_regex(&self) -> Result<regex::RegexSet> {
        regex::RegexSet::new(&self.dialog_patterns).context("invalid dialog_patterns")
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_four_harnesses() {
        let names: Vec<_> = Config::default()
            .harnesses
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(names, ["claude", "codex", "agy", "cursor"]);
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let config = Config::default();
        let parsed: Config = toml::from_str(&config.to_toml()).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let parsed: Config = toml::from_str(
            "[[harness]]\nname = \"claude\"\ncommand = \"claude\"\nargs = [\"--model\", \"opus\"]\n",
        )
        .unwrap();
        assert_eq!(parsed.submit_delay_ms, 150);
        assert_eq!(parsed.harnesses.len(), 1);
        assert_eq!(parsed.harnesses[0].args, ["--model", "opus"]);
    }

    #[test]
    fn select_filters_and_rejects_unknown() {
        let mut config = Config::default();
        config.select(&["codex".into(), "agy".into()]).unwrap();
        assert_eq!(config.harnesses.len(), 2);
        assert!(Config::default().select(&["nope".into()]).is_err());
    }

    #[test]
    fn default_dialog_patterns_match_known_trust_dialogs() {
        let set = Config::default().dialog_regex().unwrap();
        for text in [
            "Quick safety check: Is this a project you created or one you trust?",
            "Trust this folder? Codex can read, edit, and run files here",
            "Do you trust the contents of this project?",
            "⚠ Workspace Trust Required",
        ] {
            assert!(set.is_match(text), "{text}");
        }
        assert!(!set.is_match("› Ask Codex to do anything"));
    }

    #[test]
    fn invalid_dialog_pattern_is_rejected() {
        let mut config = Config::default();
        config.dialog_patterns.push("(".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_duplicates() {
        let mut config = Config::default();
        config.harnesses.push(harness("claude", "claude"));
        assert!(config.validate().is_err());
    }
}
