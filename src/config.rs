use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::commands::CommandsConfig;
use crate::rounds::MainConfig;

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
    /// Models to try in order, e.g. `["mythos", "fable"]`: the harness starts
    /// with the first one it can use, checked when agent-tabs starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_right_panel: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_args: Option<Vec<String>>,
    /// A disabled harness is not started with agent-tabs and is left out of
    /// prompts to all harnesses until it is started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

pub fn default_yolo_args(command: &str) -> &'static [&'static str] {
    match program_name(command) {
        "claude" => &["--dangerously-skip-permissions"],
        "codex" => &["--dangerously-bypass-approvals-and-sandbox"],
        "agy" => &["--dangerously-skip-permissions"],
        "cursor-agent" => &["--yolo"],
        "opencode" => &["--auto"],
        _ => &[],
    }
}

fn yolo_conflicts(command: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match program_name(command) {
        "claude" => (&["--permission-mode"], &[]),
        "codex" => (
            &["--sandbox", "-s", "--ask-for-approval", "-a"],
            &["--full-auto"],
        ),
        _ => (&[], &[]),
    }
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

/// The arguments that ask `command` for one short answer from `model`, to
/// check that the harness can use it, or None for a harness agent-tabs can't
/// check.
fn model_check_args(command: &str, model: &str) -> Option<Vec<String>> {
    let args: &[&str] = match program_name(command) {
        "claude" => &[
            "-p",
            "--model",
            model,
            "--output-format",
            "json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--system-prompt",
            "Reply with OK.",
            "OK",
        ],
        _ => return None,
    };
    Some(args.iter().map(|a| a.to_string()).collect())
}

/// The name of a harness that starts with `model`, in the style of the
/// other harness names: `claude-mythos-5-2` is `mythos-5.2`, and
/// `claude-opus-4-8-20251001` is `opus-4.8`.
fn harness_name_for(model: &str) -> String {
    let model = model.strip_prefix("claude-").unwrap_or(model);
    let version = regex::Regex::new(r"-(\d+)-(\d+)(?:-\d{8})?$").expect("valid regex");
    version.replace(model, "-$1.$2").into_owned()
}

const MODEL_CHECK_TIMEOUT: Duration = Duration::from_secs(60);

impl Harness {
    /// The harness with `--model model` in place of any `--model` in its args.
    pub fn with_model(&self, model: &str) -> Harness {
        let mut args = vec!["--model".to_string(), model.to_string()];
        let mut rest = self.args.iter();
        while let Some(arg) = rest.next() {
            if arg == "--model" {
                rest.next();
            } else if !arg.starts_with("--model=") {
                args.push(arg.clone());
            }
        }
        Harness {
            args,
            ..self.clone()
        }
    }

    /// Whether the harness answers a short prompt with `model` in `cwd`.
    fn can_use_model(&self, model: &str, cwd: &Path) -> bool {
        let Some(args) = model_check_args(&self.command, model) else {
            return false;
        };
        let mut command = Command::new(&self.command);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for name in &self.env_remove {
            command.env_remove(name);
        }
        command.envs(&self.env);
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if started.elapsed() < MODEL_CHECK_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }

    /// The first of `models` the harness can use; the last one when none of
    /// the others works, without checking it.
    fn choose_model(&self, models: &[String], cwd: &Path) -> String {
        let (last, others) = models.split_last().expect("validated as non-empty");
        others
            .iter()
            .find(|model| self.can_use_model(model, cwd))
            .unwrap_or(last)
            .clone()
    }

    pub fn with_yolo(&self) -> Harness {
        let extra: Vec<String> = match &self.yolo_args {
            Some(args) => args.clone(),
            None => default_yolo_args(&self.command)
                .iter()
                .map(|a| a.to_string())
                .collect(),
        };
        let (with_value, plain) = yolo_conflicts(&self.command);
        let mut args = Vec::new();
        let mut skip_value = false;
        for arg in &self.args {
            if skip_value {
                skip_value = false;
                continue;
            }
            let flag = arg.split('=').next().unwrap_or(arg);
            if with_value.contains(&flag) {
                skip_value = !arg.contains('=');
                continue;
            }
            if plain.contains(&arg.as_str()) {
                continue;
            }
            args.push(arg.clone());
        }
        for arg in extra {
            if !args.contains(&arg) {
                args.push(arg);
            }
        }
        Harness {
            args,
            ..self.clone()
        }
    }

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

    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
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
    #[serde(default = "default_osc52")]
    pub model_id_prefix: bool,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default = "default_dialog_patterns")]
    pub dialog_patterns: Vec<String>,
    #[serde(default = "default_input_patterns")]
    pub input_patterns: Vec<String>,
    #[serde(default = "default_working_patterns")]
    pub working_patterns: Vec<String>,
    /// `[[auto_reply]]`: what to send a harness that stops on a screen
    /// matching `pattern`, such as codex's "This content can't be shown".
    #[serde(rename = "auto_reply", default = "default_auto_replies")]
    pub auto_replies: Vec<AutoReply>,
    /// The main tab's settings, `[main]`; `[summary]`, its old name, is
    /// still accepted.
    #[serde(default, alias = "summary")]
    pub main: MainConfig,
    /// `[commands]`: where the `/NAME` templates are, and the variables
    /// they get.
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(rename = "harness", default = "default_harnesses")]
    pub harnesses: Vec<Harness>,
}

/// A reply agent-tabs sends by itself when a harness, done working, shows
/// `pattern` in the last lines of its screen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoReply {
    pub pattern: String,
    pub reply: String,
}

pub fn default_auto_replies() -> Vec<AutoReply> {
    vec![AutoReply {
        // codex, when it refuses a request it takes for a cybersecurity one.
        pattern: r"(?i)this content can.t be shown".into(),
        reply: "continue".into(),
    }]
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

pub fn default_working_patterns() -> Vec<String> {
    [
        // claude's status line: `… · 1 shell, 1 monitor still running`, not
        // a model's own words, such as "the test is still running".
        r"(?im)·[^·\n]*\bstill running\s*$",
        r"(?i)\besc to interrupt\b",
        // agy's step for a command it is still running (`● Ran` once done).
        r"(?m)^\s*○ Running\b",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
        r"(?i)△?\s*permission required",
        r"(?i)allow once\s+allow always\s+reject",
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
            model_id_prefix: default_osc52(),
            yolo: false,
            dialog_patterns: default_dialog_patterns(),
            input_patterns: default_input_patterns(),
            working_patterns: default_working_patterns(),
            auto_replies: default_auto_replies(),
            main: MainConfig::default(),
            commands: CommandsConfig::default(),
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
        self.working_regex()?;
        self.auto_reply_regexes()?;
        self.commands.validate()?;
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
            if let Some(models) = &h.models {
                anyhow::ensure!(!models.is_empty(), "harness {} has empty models", h.name);
                anyhow::ensure!(
                    models.len() == 1 || model_check_args(&h.command, "m").is_some(),
                    "harness {}: agent-tabs can check models only for claude, not {}",
                    h.name,
                    h.command
                );
            }
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
        // Naming a harness in --only asks for it, even when it is disabled.
        for harness in &mut self.harnesses {
            harness.enabled = None;
        }
        Ok(())
    }

    /// Gives each harness with `models` the first one it can use, checking
    /// them all at once. `report` is told what is checked and what is chosen.
    pub fn choose_models(&mut self, cwd: &Path, report: impl Fn(&str)) {
        let choices: Vec<(usize, String)> = std::thread::scope(|scope| {
            let checks: Vec<_> = self
                .harnesses
                .iter()
                .enumerate()
                .filter_map(|(i, harness)| {
                    let models = harness.models.as_ref()?;
                    if models.len() > 1 {
                        report(&format!(
                            "{}: checking which model it can use: {}",
                            harness.name,
                            models.join(", ")
                        ));
                    }
                    Some((i, scope.spawn(move || harness.choose_model(models, cwd))))
                })
                .collect();
            checks
                .into_iter()
                .map(|(i, check)| (i, check.join().expect("model check panicked")))
                .collect()
        });
        for (i, model) in choices {
            let name = harness_name_for(&model);
            let taken = self
                .harnesses
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && other.name == name);
            let harness = &mut self.harnesses[i];
            if harness.models.as_ref().is_some_and(|m| m.len() > 1) {
                report(&format!("{}: using {model}", harness.name));
            }
            *harness = harness.with_model(&model);
            if !taken {
                harness.name = name;
            }
        }
    }

    pub fn dialog_regex(&self) -> Result<regex::RegexSet> {
        regex::RegexSet::new(&self.dialog_patterns).context("invalid dialog_patterns")
    }

    pub fn working_regex(&self) -> Result<regex::RegexSet> {
        regex::RegexSet::new(&self.working_patterns).context("invalid working_patterns")
    }

    pub fn auto_reply_regexes(&self) -> Result<Vec<(regex::Regex, String)>> {
        self.auto_replies
            .iter()
            .map(|a| {
                let re = regex::Regex::new(&a.pattern)
                    .with_context(|| format!("invalid auto_reply pattern {:?}", a.pattern))?;
                Ok((re, a.reply.clone()))
            })
            .collect()
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
    fn main_section_overrides_defaults() {
        let parsed: Config =
            toml::from_str("[main]\ncommand = \"codex\"\nargs = [\"exec\", \"-\"]\nauto = false\n")
                .unwrap();
        assert_eq!(parsed.main.command, "codex");
        assert_eq!(parsed.main.args, ["exec", "-"]);
        assert!(!parsed.main.auto);
        assert_eq!(parsed.main.quiet_secs, 5);
        assert_eq!(parsed.harnesses.len(), 4);
    }

    #[test]
    fn old_summary_section_name_still_works() {
        let parsed: Config = toml::from_str("[summary]\nquiet_secs = 9\n").unwrap();
        assert_eq!(parsed.main.quiet_secs, 9);
    }

    #[test]
    fn select_filters_and_rejects_unknown() {
        let mut config = Config::default();
        config.select(&["codex".into(), "agy".into()]).unwrap();
        assert_eq!(config.harnesses.len(), 2);
        assert!(Config::default().select(&["nope".into()]).is_err());
    }

    #[test]
    fn select_starts_disabled_harnesses_it_names() {
        let mut config = Config::default();
        config.harnesses[1].enabled = Some(false);
        let name = config.harnesses[1].name.clone();
        config.select(&[name]).unwrap();
        assert!(config.harnesses[0].is_enabled());
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
    fn auto_replies_continue_after_codex_refuses() {
        let replies = Config::default().auto_reply_regexes().unwrap();
        let screen = " This content can’t be shown\n  Daybreak isn’t available for Astra.";
        let (_, reply) = replies.iter().find(|(re, _)| re.is_match(screen)).unwrap();
        assert_eq!(reply, "continue");
        let config: Config = toml::from_str(
            "[[auto_reply]]\npattern = \"Retry\\\\?\"\nreply = \"yes\"\n[[harness]]\nname = \"a\"\ncommand = \"a\"",
        )
        .unwrap();
        assert_eq!(
            config.auto_replies.len(),
            1,
            "the config's list replaces the default"
        );
        assert!(config.auto_reply_regexes().unwrap()[0].0.is_match("Retry?"));
        let bad: Config = toml::from_str(
            "[[auto_reply]]\npattern = \"(\"\nreply = \"x\"\n[[harness]]\nname = \"a\"\ncommand = \"a\"",
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn default_input_patterns_match_known_permission_prompts() {
        let set = Config::default().input_regex().unwrap();
        for text in [
            " Do you want to proceed?\n ❯ 1. Yes\n   4. No\n Esc to cancel · Tab to amend",
            "Requesting permission for:\n   mkdir -p x\nRun this command?\n> 1. Yes, run command",
            "Would you like to run the following command?",
            "Overwrite file? (y/n)",
            "  ┃  △ Permission required\n  ┃    ← Access external directory /work/x",
            "  ┃   Allow once   Allow always   Reject                     ctrl+f fullscreen",
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
    fn yolo_adds_each_harness_flag_and_drops_conflicts() {
        let mut claude = harness("opus", "claude");
        claude.args = vec![
            "--permission-mode".into(),
            "auto".into(),
            "--model".into(),
            "opus".into(),
        ];
        assert_eq!(
            claude.with_yolo().args,
            ["--model", "opus", "--dangerously-skip-permissions"]
        );
        let mut codex = harness("gpt", "codex");
        codex.args = vec![
            "--full-auto".into(),
            "--sandbox=workspace-write".into(),
            "-c".into(),
            "x=1".into(),
        ];
        assert_eq!(
            codex.with_yolo().args,
            ["-c", "x=1", "--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            harness("g", "agy").with_yolo().args,
            ["--dangerously-skip-permissions"]
        );
        let mut cursor = harness("c", "cursor-agent");
        cursor.args = vec!["--model".into(), "grok-4.7-high".into()];
        assert_eq!(
            cursor.with_yolo().args,
            ["--model", "grok-4.7-high", "--yolo"]
        );
        assert_eq!(
            harness("o", "/usr/bin/opencode").with_yolo().args,
            ["--auto"]
        );
        assert_eq!(harness("x", "other").with_yolo().args, Vec::<String>::new());
        let twice = claude.with_yolo().with_yolo();
        assert_eq!(
            twice.args,
            ["--model", "opus", "--dangerously-skip-permissions"]
        );
        let mut custom = harness("c", "claude");
        custom.yolo_args = Some(vec!["--permission-mode".into(), "bypassPermissions".into()]);
        assert_eq!(
            custom.with_yolo().args,
            ["--permission-mode", "bypassPermissions"]
        );
    }

    #[test]
    fn with_model_replaces_the_model_arg() {
        let mut claude = harness("c", "claude");
        claude.args = vec![
            "--model".into(),
            "fable".into(),
            "--effort".into(),
            "high".into(),
            "--model=opus".into(),
        ];
        assert_eq!(
            claude.with_model("mythos").args,
            ["--model", "mythos", "--effort", "high"]
        );
        assert_eq!(
            harness("c", "claude").with_model("fable").args,
            ["--model", "fable"]
        );
    }

    #[test]
    fn choose_models_falls_back_to_the_last_model() {
        let mut config = Config::default();
        let mut missing = harness("missing", "/nonexistent/claude");
        missing.models = Some(vec!["mythos".into(), "fable".into()]);
        let mut single = harness("single", "claude");
        single.models = Some(vec!["opus".into()]);
        config.harnesses = vec![missing, single];
        let reports = std::sync::Mutex::new(Vec::new());
        config.choose_models(Path::new("/"), |r| {
            reports.lock().unwrap().push(r.to_string())
        });
        assert_eq!(config.harnesses[0].args, ["--model", "fable"]);
        assert_eq!(config.harnesses[0].name, "fable");
        assert_eq!(config.harnesses[1].args, ["--model", "opus"]);
        assert_eq!(config.harnesses[1].name, "opus");
        assert_eq!(
            reports.into_inner().unwrap(),
            [
                "missing: checking which model it can use: mythos, fable",
                "missing: using fable"
            ]
        );
    }

    #[test]
    fn harnesses_are_named_after_their_models() {
        assert_eq!(harness_name_for("claude-mythos-5-2"), "mythos-5.2");
        assert_eq!(harness_name_for("claude-opus-4-8-20251001"), "opus-4.8");
        assert_eq!(harness_name_for("fable"), "fable");
        assert_eq!(harness_name_for("gpt-6.1-sol"), "gpt-6.1-sol");
    }

    #[test]
    fn a_model_name_another_harness_has_keeps_the_configured_name() {
        let mut config = Config::default();
        let mut top = harness("top", "/nonexistent/claude");
        top.models = Some(vec!["claude-mythos-5-2".into(), "opus".into()]);
        config.harnesses = vec![top, harness("opus", "claude")];
        config.choose_models(Path::new("/"), |_| {});
        assert_eq!(config.harnesses[0].name, "top");
        assert_eq!(config.harnesses[0].args, ["--model", "opus"]);
    }

    #[test]
    fn models_are_validated() {
        let mut config = Config::default();
        config.harnesses[0].models = Some(Vec::new());
        assert!(config.validate().is_err());
        let mut config = Config::default();
        config.harnesses[1].models = Some(vec!["a".into(), "b".into()]);
        assert!(config.validate().is_err(), "codex models can't be checked");
        config.harnesses[1].models = Some(vec!["a".into()]);
        config.validate().unwrap();
        config.harnesses[0].models = Some(vec!["mythos".into(), "fable".into()]);
        config.validate().unwrap();
    }

    #[test]
    fn invalid_model_pattern_is_rejected() {
        let mut config = Config::default();
        config.harnesses[0].model_patterns = Some(vec!["(".into()]);
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_working_patterns_match_background_work() {
        let set = Config::default().working_regex().unwrap();
        for text in [
            "✻ Sautéed for 2m 12s · done 1:35 PM · 1 shell, 1 monitor still running",
            "• Working (5s • esc to interrupt)",
            "● Explored 4 files\n○ Running (ninja -C build) (ctrl+o to expand)\n\n  waiting",
        ] {
            assert!(set.is_match(text), "{text}");
        }
        assert!(!set.is_match("✻ Churned for 3s · done 6:38 PM"));
        assert!(
            !set.is_match(
                "• The last Valgrind test is still running; I’m waiting for its shutdown summary.\n\n■ This content can’t be shown"
            ),
            "a model saying something is still running"
        );
        assert!(!set.is_match("  the build is still running"));
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
