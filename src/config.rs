use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::summary::SummaryConfig;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Harness {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_patterns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_right_panel: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

fn program_name(command: &str) -> &str {
    std::path::Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(command)
}

pub fn default_model_patterns(command: &str) -> Vec<String> {
    let patterns: &[&str] = match program_name(command) {
        "claude" => &[
            r"(?m)^\s*((?:Opus|Sonnet|Haiku|Fable|Mythos) [\d.]+(?: (?:low|medium|high|xhigh|max))?)\s*\|",
            r"((?:Opus|Sonnet|Haiku|Fable|Mythos) [\d.]+) with (\w+) effort",
        ],
        "codex" => &[r"(?m)^\s*(\S[^·\n]*?)\s+·\s+Context \d+% left"],
        "agy" => &[
            r"(?m)((?:Gemini|Claude|GPT)[\w .-]*?)\s+·\s+(minimal|low|medium|high|xhigh|max)\s*$",
            r"((?:Gemini|Claude|GPT)[\w .-]*?) \((Low|Medium|High|Max)\)",
        ],
        "cursor-agent" => &[
            r"(?m)^\s{1,4}((?:Gemini|Grok|GPT|Claude|Sonnet|Opus|Fable|Composer|Codex|Auto|Kimi|DeepSeek|Muse|Qwen|GLM|MiniMax|o\d)[^·\n]*?)\s*(?:·\s*\d+(?:\.\d+)?%|\s{3,}\S|$)",
        ],
        "opencode" => &[
            r"(?m)^\s*┃\s+\w+ · (?P<model>\S.*?\))\s+(?P<provider>\S.*?)(?:\s{2,}|$)",
            r"(?m)^\s*┃\s+\w+ · (\S.*?)(?:\s{2,}|$)",
            r"▣\s+\w+ · (.+?) · ",
        ],
        _ => &[],
    };
    patterns.iter().map(|p| p.to_string()).collect()
}

impl Harness {
    pub fn model_label(&self, detected: Option<(String, Option<String>)>) -> Option<String> {
        let (model, detected_provider) = match &self.model {
            Some(model) => (model.clone(), None),
            None => detected?,
        };
        let provider = self
            .provider
            .clone()
            .or(detected_provider)
            .or_else(|| default_provider(&self.command).map(str::to_string));
        Some(match provider {
            Some(provider) => format!("{model} ({provider})"),
            None => model,
        })
    }

    pub fn strips_right_panel(&self) -> bool {
        self.strip_right_panel
            .unwrap_or_else(|| program_name(&self.command) == "opencode")
    }

    pub fn model_regexes(&self) -> Result<Vec<regex::Regex>> {
        let patterns = self
            .model_patterns
            .clone()
            .unwrap_or_else(|| default_model_patterns(&self.command));
        patterns
            .iter()
            .map(|p| {
                regex::Regex::new(p)
                    .with_context(|| format!("invalid model_patterns entry for {}: {p}", self.name))
            })
            .collect()
    }
}

pub fn detect_model(patterns: &[regex::Regex], screen: &str) -> Option<(String, Option<String>)> {
    patterns.iter().find_map(|pattern| {
        let captures = pattern.captures_iter(screen).last()?;
        let provider_index = pattern
            .capture_names()
            .position(|name| name == Some("provider"));
        let parts: Vec<&str> = captures
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(i, _)| Some(*i) != provider_index)
            .filter_map(|(_, m)| m)
            .map(|m| m.as_str().trim())
            .filter(|s| !s.is_empty())
            .collect();
        let provider = captures
            .name("provider")
            .map(|m| m.as_str().trim().to_string())
            .filter(|s| !s.is_empty());
        (!parts.is_empty()).then(|| (parts.join(" "), provider))
    })
}

pub fn default_provider(command: &str) -> Option<&'static str> {
    match program_name(command) {
        "claude" => Some("Claude Code"),
        "codex" => Some("Codex"),
        "agy" => Some("Antigravity"),
        "cursor-agent" => Some("Cursor"),
        "opencode" => Some("OpenCode"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_submit_delay_ms")]
    pub submit_delay_ms: u64,
    #[serde(default = "default_scrollback")]
    pub scrollback: usize,
    #[serde(default = "default_mouse")]
    pub mouse: bool,
    #[serde(default = "default_osc52")]
    pub osc52: bool,
    #[serde(default = "default_osc52")]
    pub system_clipboard: bool,
    #[serde(default = "default_dialog_patterns")]
    pub dialog_patterns: Vec<String>,
    #[serde(default = "default_input_patterns")]
    pub input_patterns: Vec<String>,
    #[serde(default)]
    pub summary: SummaryConfig,
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

fn default_osc52() -> bool {
    true
}

pub fn default_input_patterns() -> Vec<String> {
    [
        r"(?i)do you want to proceed\?",
        r"(?i)do you want to (make this edit|create|allow|run)",
        r"(?i)esc to cancel · tab to amend",
        r"(?i)requesting permission for",
        r"(?i)run this command\?",
        r"(?i)would you like to (run|make|apply|allow)",
        r"(?i)allow (this )?(command|tool|edit)\?",
        r"(?i)\(y/n\)",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
        ..Harness::default()
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
            osc52: default_osc52(),
            system_clipboard: default_osc52(),
            dialog_patterns: default_dialog_patterns(),
            input_patterns: default_input_patterns(),
            summary: SummaryConfig::default(),
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
        self.input_regex()?;
        let mut seen = std::collections::HashSet::new();
        for h in &self.harnesses {
            anyhow::ensure!(!h.name.trim().is_empty(), "harness with empty name");
            h.model_regexes()?;
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

    pub fn input_regex(&self) -> Result<regex::RegexSet> {
        regex::RegexSet::new(&self.input_patterns).context("invalid input_patterns")
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
    fn summary_section_overrides_defaults() {
        let parsed: Config = toml::from_str(
            "[summary]\ncommand = \"codex\"\nargs = [\"exec\", \"-\"]\nauto = false\n",
        )
        .unwrap();
        assert_eq!(parsed.summary.command, "codex");
        assert_eq!(parsed.summary.args, ["exec", "-"]);
        assert!(!parsed.summary.auto);
        assert_eq!(parsed.summary.quiet_secs, 5);
        assert_eq!(parsed.harnesses.len(), 4);
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
    fn default_input_patterns_match_known_permission_prompts() {
        let set = Config::default().input_regex().unwrap();
        for text in [
            " Do you want to proceed?\n ❯ 1. Yes\n   4. No\n Esc to cancel · Tab to amend",
            "Requesting permission for:\n   mkdir -p x\nRun this command?\n> 1. Yes, run command",
            "Would you like to run the following command?",
            "Overwrite file? (y/n)",
        ] {
            assert!(set.is_match(text), "{text}");
        }
        for text in ["› Ask Codex to do anything", "• DONE.", "→ Add a follow-up"] {
            assert!(!set.is_match(text), "{text}");
        }
    }

    fn model_of(command: &str, screen: &str) -> Option<String> {
        let harness = harness(command, command);
        detect_model(&harness.model_regexes().unwrap(), screen).map(|(model, _)| model)
    }

    #[test]
    fn model_label_adds_the_provider() {
        let cursor = harness("muse", "cursor-agent");
        let screen =
            " ▀▀▀▀▀▀▀▀\n  Muse Wonder 300K High                                Run Everything";
        let detected = detect_model(&cursor.model_regexes().unwrap(), screen);
        assert_eq!(
            cursor.model_label(detected).as_deref(),
            Some("Muse Wonder 300K High (Cursor)")
        );

        let opencode = harness("kimi", "opencode");
        let screen = "  ┃  Build · Kimi K3 (Global) Amazon Bedrock          4c26-8e72";
        let detected = detect_model(&opencode.model_regexes().unwrap(), screen);
        assert_eq!(
            opencode.model_label(detected).as_deref(),
            Some("Kimi K3 (Global) (Amazon Bedrock)")
        );

        let mut fixed = harness("glm", "opencode");
        fixed.model = Some("GLM 5.3".into());
        fixed.provider = Some("Bedrock".into());
        assert_eq!(
            fixed.model_label(None).as_deref(),
            Some("GLM 5.3 (Bedrock)")
        );

        let unknown = harness("x", "some-cli");
        assert_eq!(
            unknown.model_label(Some(("M".into(), None))).as_deref(),
            Some("M")
        );
        assert_eq!(unknown.model_label(None), None);
    }

    #[test]
    fn detects_models_from_real_status_lines() {
        assert_eq!(
            model_of(
                "claude",
                "x\n  Opus 5.5 high | v2.1.304 | /tmp/e2e | tokens\n  ⏵⏵ auto mode on"
            )
            .as_deref(),
            Some("Opus 5.5 high")
        );
        assert_eq!(
            model_of(
                "claude",
                "▝▜██████▀  Opus 5.5 with high effort · Claude Team"
            )
            .as_deref(),
            Some("Opus 5.5 high")
        );
        assert_eq!(
            model_of(
                "codex",
                "› Ask Codex\n  GPT-6-Astra high · Context 94% left · /tmp/e2e"
            )
            .as_deref(),
            Some("GPT-6-Astra high")
        );
        assert_eq!(
            model_of(
                "agy",
                "? for shortcuts                                  Gemini 4 Argon · high"
            )
            .as_deref(),
            Some("Gemini 4 Argon high")
        );
        assert_eq!(
            model_of("agy", "  ▀▀▀▀  Gemini 4 Argon (High)").as_deref(),
            Some("Gemini 4 Argon High")
        );
        assert_eq!(
            model_of(
                "cursor-agent",
                " ▀▀▀▀▀▀▀▀\n  Grok 4.7 256K High · 5.4%\n  /tmp/e2e · feature"
            )
            .as_deref(),
            Some("Grok 4.7 256K High")
        );
        assert_eq!(
            model_of(
                "cursor-agent",
                "  Gemini 4 Argon High                                   Run Everything"
            )
            .as_deref(),
            Some("Gemini 4 Argon High")
        );
        assert_eq!(
            model_of(
                "opencode",
                "  ┃  Build · Kimi K3 (Global) Amazon Bedrock          4c26-8e72"
            )
            .as_deref(),
            Some("Kimi K3 (Global)")
        );
        assert_eq!(
            model_of(
                "cursor-agent",
                " ▀▀▀▀▀▀▀▀\n  Muse Wonder 300K High                                Run Everything"
            )
            .as_deref(),
            Some("Muse Wonder 300K High")
        );
        assert_eq!(model_of("codex", "no status here"), None);
        assert_eq!(model_of("unknown-cli", "Opus 5.5 high |"), None);
    }

    #[test]
    fn invalid_model_pattern_is_rejected() {
        let mut config = Config::default();
        config.harnesses[0].model_patterns = Some(vec!["(".into()]);
        assert!(config.validate().is_err());
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
