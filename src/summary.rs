use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SummaryConfig {
    pub command: String,
    pub args: Vec<String>,
    pub auto: bool,
    pub quiet_secs: u64,
    pub wait_timeout_secs: u64,
    pub run_timeout_secs: u64,
    pub max_lines: usize,
    pub env_remove: Vec<String>,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            args: vec!["--print".into()],
            auto: true,
            quiet_secs: 5,
            wait_timeout_secs: 900,
            run_timeout_secs: 300,
            max_lines: 400,
            env_remove: vec![
                "CLAUDECODE".into(),
                "CLAUDE_CODE_ENTRYPOINT".into(),
                "CLAUDE_CODE_CHILD_SESSION".into(),
            ],
        }
    }
}

impl SummaryConfig {
    pub fn command_line(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SummaryState {
    Waiting,
    Running { since: Instant },
    Done { text: String, took: Duration },
    Failed(String),
    Superseded,
}

#[derive(Debug, Clone)]
pub struct Capture {
    pub text: String,
    pub finished: bool,
}

#[derive(Debug)]
pub struct Round {
    pub prompt: String,
    pub targets: Vec<usize>,
    pub names: Vec<String>,
    pub delivered: Vec<Option<(Instant, usize)>>,
    pub captures: Vec<Option<Capture>>,
    pub state: Arc<Mutex<SummaryState>>,
    pub started: Instant,
    pub started_at: u64,
    pub compared_unfinished: bool,
}

pub fn read_state(state: &Arc<Mutex<SummaryState>>) -> SummaryState {
    state
        .lock()
        .map(|s| s.clone())
        .unwrap_or(SummaryState::Failed("state lock poisoned".into()))
}

impl Round {
    pub fn new(prompt: String, targets: Vec<usize>, names: Vec<String>, started_at: u64) -> Self {
        let n = targets.len();
        Self {
            prompt,
            targets,
            names,
            delivered: vec![None; n],
            captures: vec![None; n],
            state: Arc::new(Mutex::new(SummaryState::Waiting)),
            started: Instant::now(),
            started_at,
            compared_unfinished: false,
        }
    }

    pub fn state(&self) -> SummaryState {
        read_state(&self.state)
    }

    pub fn answer(&self, slot: usize) -> Option<String> {
        let capture = self.captures[slot].as_ref()?;
        Some(final_message(&extract_answer(&capture.text, &self.prompt)))
    }

    pub fn set_state(&self, state: SummaryState) {
        if let Ok(mut s) = self.state.lock() {
            *s = state;
        }
    }

    pub fn slot(&self, pane: usize) -> Option<usize> {
        self.targets.iter().position(|&t| t == pane)
    }

    pub fn all_finished(&self) -> bool {
        self.captures
            .iter()
            .all(|c| c.as_ref().is_some_and(|c| c.finished))
    }

    pub fn all_captured(&self) -> bool {
        self.captures.iter().all(Option::is_some)
    }

    pub fn input(&self) -> String {
        let entries: Vec<(&str, &Capture)> = self
            .names
            .iter()
            .zip(&self.captures)
            .filter_map(|(name, capture)| capture.as_ref().map(|c| (name.as_str(), c)))
            .collect();
        build_prompt(&self.prompt, &entries)
    }
}

fn is_separator(line: &str) -> bool {
    let text = line.trim();
    let total = text.chars().count();
    let boxed = text
        .chars()
        .filter(|c| matches!(c, '─' | '━' | '═' | '╌' | '┄' | '▄' | '▀'))
        .count();
    let ascii = text
        .chars()
        .filter(|c| matches!(c, '-' | '=' | '_'))
        .count();
    (total >= 3 && boxed == total) || (total >= 8 && (boxed + ascii) * 10 >= total * 8)
}

fn is_input_line(line: &str) -> bool {
    let text = line.trim_start();
    ["❯", "›", ">", "→"].iter().any(|marker| {
        text.strip_prefix(marker)
            .is_some_and(|rest| rest.trim().is_empty() || rest.starts_with(char::is_whitespace))
    })
}

fn normalize(text: &str) -> String {
    let text = text.trim_start();
    let text = ["❯", "›", ">", "→"]
        .iter()
        .find_map(|marker| text.strip_prefix(marker))
        .unwrap_or(text);
    text.split_whitespace()
        .map(|word| word.replace(['`', '*'], ""))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_timing_line(line: &str) -> bool {
    static TIMING: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(for|in) \d+(\.\d+)?\s*(ms|s|m|sec|secs|seconds)\b")
            .expect("valid regex")
    });
    line.trim().chars().count() < 60 && TIMING.is_match(line)
}

fn starts_item(line: &str) -> bool {
    let text = line.trim_start();
    text.starts_with([
        '●', '•', '○', '◦', '⏺', '-', '*', '⎿', '$', '>', '✓', '✗', '#',
    ]) || text
        .split_once(". ")
        .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

fn is_step_line(line: &str) -> bool {
    static STEP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^\s*[●•○◦⏺]\s*(?:(Explored|Explore|Exploring|Read|Reading|Ran|Running|Searched|Searching|Search|Listed|List|Edited|Edit|Editing|Update|Updated|Write|Wrote|Thought|Thinking|Bash|Grep|Glob|Called|Fetched|Fetch|Task|Agent)\b|[A-Z]\w*\()",
        )
        .expect("valid regex")
    });
    STEP.is_match(line)
}

fn is_tool_line(line: &str) -> bool {
    let text = line.trim_start();
    is_step_line(line) || text.starts_with(['⎿', '└', '├', '│']) || text.starts_with("$ ")
}

fn is_thinking_line(line: &str) -> bool {
    static THINKING: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^\s*[▸▾○●•◦⏺✻]?\s*(Thought|Thinking)\b").expect("valid regex")
    });
    THINKING.is_match(line)
}

fn is_message_start(line: &str) -> bool {
    line.trim_start().starts_with(['●', '•', '⏺'])
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

pub fn final_message(answer: &str) -> String {
    let lines: Vec<&str> = answer.lines().collect();
    let mut start = None;
    let mut end = lines.len();
    let mut tool_indent: Option<usize> = None;
    let mut thinking = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if is_input_line(line) {
            start = None;
            tool_indent = None;
            thinking = false;
            continue;
        }
        if start.is_some() && is_separator(line) {
            end = i;
            break;
        }
        if is_thinking_line(line) {
            thinking = true;
            continue;
        }
        if thinking && (line.trim_end().ends_with('…') || line.trim_end().ends_with("...")) {
            continue;
        }
        let after_thinking = std::mem::take(&mut thinking);
        if is_timing_line(line) {
            continue;
        }
        if after_thinking {
            tool_indent = None;
            start = Some(i);
            continue;
        }
        if is_tool_line(line) {
            tool_indent = Some(tool_indent.map_or(indent(line), |t| t.min(indent(line))));
            continue;
        }
        if tool_indent.is_some_and(|t| indent(line) > t) {
            continue;
        }
        let after_tool = tool_indent.take().is_some();
        if is_message_start(line) || after_tool || start.is_none() {
            start = Some(i);
        }
    }
    let Some(start) = start else {
        return answer.to_string();
    };
    let mut message: Vec<&str> = lines[start..end]
        .iter()
        .copied()
        .filter(|l| !is_timing_line(l) && !is_thinking_line(l))
        .collect();
    while message.last().is_some_and(|l| l.trim().is_empty()) {
        message.pop();
    }
    message.join("\n")
}

pub fn reflow(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut base_indent = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            continue;
        }
        let continues = out.last().is_some_and(|l| !l.is_empty())
            && !starts_item(line)
            && indent(line) <= base_indent + 2;
        if continues {
            let last = out.last_mut().expect("checked non-empty");
            last.push(' ');
            last.push_str(line.trim());
        } else {
            base_indent = indent(line);
            out.push(line.trim().to_string());
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

pub fn headline(answer: &str) -> Option<String> {
    let lines: Vec<&str> = answer.lines().collect();
    let last = lines
        .iter()
        .rposition(|l| !l.trim().is_empty() && !is_timing_line(l))?;
    let mut first = last;
    while first > 0 && !starts_item(lines[first]) && !lines[first - 1].trim().is_empty() {
        first -= 1;
    }
    if first < last && is_step_line(lines[first]) {
        first += 1;
    }
    let text = lines[first..=last]
        .iter()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join(" ");
    Some(text)
}

pub fn extract_answer(capture: &str, prompt: &str) -> String {
    let lines: Vec<&str> = capture.lines().collect();
    let needle: String = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(normalize)
        .unwrap_or_default()
        .chars()
        .take(40)
        .collect();
    let echoed = (!needle.is_empty())
        .then(|| lines.iter().rposition(|l| normalize(l).contains(&needle)))
        .flatten();
    let Some(echoed) = echoed else {
        let cut = lines
            .iter()
            .rposition(|l| is_input_line(l))
            .map(|i| {
                if i > 0 && is_separator(lines[i - 1]) {
                    i - 1
                } else {
                    i
                }
            })
            .unwrap_or(lines.len());
        return lines[..cut].join("\n").trim().to_string();
    };
    let flat_prompt = normalize(prompt);
    let mut start = echoed + 1;
    while start < lines.len() {
        let line = normalize(lines[start]);
        if line.is_empty() || !flat_prompt.contains(&line) {
            break;
        }
        start += 1;
    }
    let mut answer: Vec<&str> = Vec::new();
    for line in &lines[start..] {
        let chrome = is_separator(line) || is_input_line(line);
        if chrome && answer.iter().any(|l| !l.trim().is_empty()) {
            break;
        }
        if chrome || (answer.is_empty() && line.trim().is_empty()) {
            continue;
        }
        answer.push(line.trim_end());
    }
    while answer.last().is_some_and(|l| l.trim().is_empty()) {
        answer.pop();
    }
    answer.join("\n")
}

pub fn build_prompt(prompt: &str, entries: &[(&str, &Capture)]) -> String {
    let mut out = String::from(
        "You are given the answers of several AI coding agents to the same prompt. Each answer is a \
capture of that agent's terminal screen, so it can contain UI chrome such as input boxes, status \
bars, spinners, tips and placeholder suggestions shown in an empty input box. Placeholders are not \
user input. Ignore the chrome and focus on what each agent said and did.\n\n\
Write a concise comparison for a software engineer with these sections:\n\
Consensus: what all or most agents agree on.\n\
Differences: notable disagreements or unique points, attributed to agents by name.\n\
Best answer: which agent's answer is strongest and why; if answers combine well, say how.\n\
Concerns: mistakes, unsupported claims, or work an agent left unfinished.\n\n\
Keep it under 300 words, as plain text with short bullet points. Do not use any tools.\n\n",
    );
    out.push_str("<prompt>\n");
    out.push_str(prompt.trim());
    out.push_str("\n</prompt>\n");
    for (name, capture) in entries {
        let status = if capture.finished {
            "finished"
        } else {
            "still working when captured"
        };
        out.push_str(&format!("\n<agent name=\"{name}\" status=\"{status}\">\n"));
        let text = capture.text.trim();
        out.push_str(if text.is_empty() {
            "(no output captured)"
        } else {
            text
        });
        out.push_str("\n</agent>\n");
    }
    out
}

pub fn run_summarizer(config: &SummaryConfig, input: &str, cwd: &Path) -> Result<String> {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in &config.env_remove {
        cmd.env_remove(key);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting `{}`", config.command_line()))?;
    let mut stdin = child.stdin.take().context("summarizer stdin")?;
    let input = input.to_string();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let read_all = |mut stream: Box<dyn Read + Send>| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            let _ = tx.send(text);
        });
        rx
    };
    let out_rx = read_all(Box::new(child.stdout.take().context("summarizer stdout")?));
    let err_rx = read_all(Box::new(child.stderr.take().context("summarizer stderr")?));

    let timeout = Duration::from_secs(config.run_timeout_secs.max(1));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{}` did not finish within {}s",
                config.command_line(),
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = writer.join();
    let out = out_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_default();
    let err = err_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_default();
    if !status.success() {
        let detail = err.trim();
        let detail = if detail.is_empty() {
            out.trim()
        } else {
            detail
        };
        let tail: String = detail
            .chars()
            .rev()
            .take(500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        bail!("`{}` failed ({status}): {tail}", config.command_line());
    }
    let text = out.trim().to_string();
    if text.is_empty() {
        bail!("`{}` returned no output", config.command_line());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(text: &str, finished: bool) -> Capture {
        Capture {
            text: text.into(),
            finished,
        }
    }

    fn shell(script: &str) -> SummaryConfig {
        SummaryConfig {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            run_timeout_secs: 5,
            ..SummaryConfig::default()
        }
    }

    #[test]
    fn prompt_lists_every_agent_with_status() {
        let a = capture("  answer A  ", true);
        let b = capture("", false);
        let text = build_prompt("Why?", &[("claude", &a), ("agy", &b)]);
        assert!(text.contains("<prompt>\nWhy?\n</prompt>"));
        assert!(text.contains("<agent name=\"claude\" status=\"finished\">\nanswer A\n</agent>"));
        assert!(text.contains("<agent name=\"agy\" status=\"still working when captured\">\n(no output captured)\n</agent>"));
    }

    const PROMPT: &str =
        "Run this exact shell command: mkdir -p /tmp/x && date > /tmp/x/now.txt   Then reply DONE.";

    #[test]
    fn extracts_claude_answer() {
        let screen = " ▐▛███▜▌   Claude Code v2.1.304\n\n❯ Run this exact shell command: mkdir -p /tmp/x && date > /tmp/x/now.txt   Then\n  reply DONE.\n\n● Bash(mkdir -p /tmp/x && date > /tmp/x/now.txt)\n  ⎿  Done\n\n● DONE\n\n✻ Churned for 3s\n────────────────────────────────────────\n❯ \n────────────────────────────────────────\n  ⏵⏵ auto mode on";
        assert_eq!(
            extract_answer(screen, PROMPT),
            "● Bash(mkdir -p /tmp/x && date > /tmp/x/now.txt)\n  ⎿  Done\n\n● DONE\n\n✻ Churned for 3s"
        );
    }

    #[test]
    fn extracts_codex_answer() {
        let screen = "■ No active thread is available.\n  >_ OpenAI Codex (v0.163.0)\n› Run this exact shell command: mkdir -p /tmp/x && date > /tmp/x/now.txt   Then reply DONE.\n• I’ll run the command now.\n• Ran mkdir -p /tmp/x && date > /tmp/x/now.txt\n  └ (no output)\n• DONE.\n  Worked for 6s • 7:35 PM\n› Ask Codex to do anything\n  GPT-6-Astra high · Context 94% left";
        assert_eq!(
            extract_answer(screen, PROMPT),
            "• I’ll run the command now.\n• Ran mkdir -p /tmp/x && date > /tmp/x/now.txt\n  └ (no output)\n• DONE.\n  Worked for 6s • 7:35 PM"
        );
    }

    #[test]
    fn extracts_agy_answer() {
        let screen = "> Run this exact shell command: mkdir -p /tmp/x && date > /tmp/x/now.txt   Then reply\n  DONE.\n○ Thought for 4.9s\n● Ran (mkdir -p /tmp/x && date > /tmp/x/now.txt)\n  DONE\n──────────────────────────────────────────────\n>\n──────────────────────────────────────────────\n? for shortcuts                       Gemini 4 Argon · high";
        assert_eq!(
            extract_answer(screen, PROMPT),
            "○ Thought for 4.9s\n● Ran (mkdir -p /tmp/x && date > /tmp/x/now.txt)\n  DONE"
        );
    }

    #[test]
    fn extracts_cursor_answer() {
        let screen = "  Cursor Agent\n  Run this exact shell command: mkdir -p /tmp/x && date > /tmp/x/now.txt Then reply DONE.\n  I'll run that command and then reply.\n  $ mkdir -p /tmp/x && date > /tmp/x/now.txt 658ms\n  DONE\n ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄\n  → Add a follow-up\n ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n  Grok 4.7 256K High";
        assert_eq!(
            extract_answer(screen, PROMPT),
            "  I'll run that command and then reply.\n  $ mkdir -p /tmp/x && date > /tmp/x/now.txt 658ms\n  DONE"
        );
    }

    #[test]
    fn headline_is_the_last_paragraph_without_timing_lines() {
        let claude = "  Read 1 file\n● It returns -1 only when i > n; when i == n the check\n  still passes (UB).\n✻ Churned for 5s · done 8:11 PM";
        assert_eq!(
            headline(claude).unwrap(),
            "● It returns -1 only when i > n; when i == n the check still passes (UB)."
        );
        let codex = "• I’ll read a.c.\n• get() returns -1 for i > n.\n  Worked for 6s • 7:35 PM";
        assert_eq!(headline(codex).unwrap(), "• get() returns -1 for i > n.");
        assert_eq!(headline("only line").unwrap(), "only line");
        let agy = "● Explored 1 file\n  get() returns -1 for i > n, but the bounds check\nonly checks i <= n.";
        assert_eq!(
            headline(agy).unwrap(),
            "get() returns -1 for i > n, but the bounds check only checks i <= n."
        );
        let numbered = "1. first\n2. second item\n   continues";
        assert_eq!(headline(numbered).unwrap(), "2. second item continues");
        assert!(headline("  \n").is_none());
    }

    #[test]
    fn final_message_is_claudes_last_big_dot_paragraph() {
        let answer = "● Bash(gh pr review 47 --comment)\n  ⎿  Posted\n\n● I posted the review as a comment, so it\n  neither approves nor requests changes.\n\n  The summary lists what I checked.\n\n✻ Worked for 2m 3s";
        assert_eq!(
            final_message(answer),
            "● I posted the review as a comment, so it\n  neither approves nor requests changes.\n\n  The summary lists what I checked."
        );
    }

    #[test]
    fn final_message_is_codex_text_after_the_last_command() {
        let answer = "• I'll check the branch.\n• Ran git show branch:file.cc | sed -n '1,9p'; git status\n  └ */\n    static int recover(handlerton *hton\n     M storage/rocksdb/rocksdb\nThen issue 2 does not apply and should be dropped.\nThe new code already handles missing markers.\n  Worked for 1m 12s • 9:01 PM";
        assert_eq!(
            final_message(answer),
            "Then issue 2 does not apply and should be dropped.\nThe new code already handles missing markers."
        );
        assert_eq!(final_message("• Ran ls\n  └ a\n• All good."), "• All good.");
    }

    #[test]
    fn final_message_skips_agy_thinking_block() {
        let screen = "> Is the upgrade backward compatible?\n▸ Thought for 32s, 1.9k tokens\n  I'm now focused on system variables. My current analysis centers on removed var...\n  Yes — the server is backward-compatible on upgrade, and you can downgrade safely\n  if you follow a few precautions.\n  ──────\n>\n──────────────────────────\n? for shortcuts";
        let answer = final_message(&extract_answer(
            screen,
            "Is the upgrade backward compatible?",
        ));
        assert_eq!(
            answer,
            "  Yes — the server is backward-compatible on upgrade, and you can downgrade safely\n  if you follow a few precautions."
        );
        assert_eq!(
            headline(&answer).unwrap(),
            "Yes — the server is backward-compatible on upgrade, and you can downgrade safely if you follow a few precautions."
        );
    }

    #[test]
    fn short_box_rule_is_a_separator_but_markdown_dashes_are_not() {
        assert!(is_separator("  ──────"));
        assert!(!is_separator("---"));
        assert!(!is_separator("a -- b"));
    }

    #[test]
    fn claude_screen_with_rendered_markdown_prompt() {
        let screen = " ▐▛███▜▌   Claude Code v2.1.304\n❯ Run cat a.c and git log --oneline -1, then tell me what get() does\n  Ran 1 shell command\n● get() returns a[i] when i <= n and -1 otherwise,\n  but the check is off by one.\n✻ Crunched for 9s · done 9:25 PM\n──────────────────────────\n❯\u{a0}fix the off-by-one in get()\n──────────────────────────\n  Opus 5.5 high | v2.1.304\n  ⏵⏵ auto mode on";
        let prompt = "Run `cat a.c` and `git log --oneline -1`, then tell me what get() does";
        let answer = final_message(&extract_answer(screen, prompt));
        assert_eq!(
            answer,
            "● get() returns a[i] when i <= n and -1 otherwise,\n  but the check is off by one."
        );
        let fallback = final_message(&extract_answer(screen, "a prompt that is not on screen"));
        assert_eq!(fallback, answer);
        assert!(is_input_line("❯\u{a0}fix the off-by-one"));
    }

    #[test]
    fn lowercase_call_in_an_answer_is_not_a_tool_step() {
        let answer = "• Ran cat a.c\n  └ int get(int *a)\n• get() returns -1 for i > n. The last commit,\n  f82811c, added the check.";
        assert_eq!(
            final_message(answer),
            "• get() returns -1 for i > n. The last commit,\n  f82811c, added the check."
        );
        assert!(is_step_line("● Bash(cat a.c)"));
        assert!(!is_step_line("• get() returns -1"));
    }

    #[test]
    fn reflow_joins_wrapped_lines_but_keeps_items_and_code() {
        let text = "● get() returns a[i] when i <= n,\n  but the check is off by one.\n\n  Second paragraph\n  continues.\n  - item one\n  - item two\n        int x = 1;";
        assert_eq!(
            reflow(text),
            [
                "● get() returns a[i] when i <= n, but the check is off by one.",
                "",
                "Second paragraph continues.",
                "- item one",
                "- item two",
                "int x = 1;",
            ]
        );
    }

    #[test]
    fn final_message_for_cursor_and_plain_answers() {
        let cursor = "  I'll run that command and then reply.\n  $ mkdir -p /tmp/x 658ms\n  DONE";
        assert_eq!(final_message(cursor), "  DONE");
        assert_eq!(final_message("plain\nanswer"), "plain\nanswer");
        assert_eq!(final_message("• Ran ls\n  └ out"), "• Ran ls\n  └ out");
    }

    #[test]
    fn uses_the_last_echo_of_the_prompt() {
        let screen = "› Q\n• old answer\n› Q\n• new answer\n› Ask Codex to do anything";
        assert_eq!(extract_answer(screen, "Q"), "• new answer");
    }

    #[test]
    fn falls_back_to_the_whole_capture_without_an_echo() {
        assert_eq!(
            extract_answer("  just output  \n", "missing prompt"),
            "just output"
        );
    }

    #[test]
    fn round_tracks_slots_and_captures() {
        let mut round = Round::new("p".into(), vec![0, 2], vec!["a".into(), "c".into()], 0);
        assert_eq!(round.slot(2), Some(1));
        assert_eq!(round.slot(1), None);
        assert!(!round.all_captured());
        round.captures = vec![Some(capture("x", true)), Some(capture("y", true))];
        assert!(round.all_captured());
        assert!(round.input().contains("<agent name=\"c\""));
        assert_eq!(round.state(), SummaryState::Waiting);
    }

    #[test]
    fn summarizer_receives_input_on_stdin() {
        let text = run_summarizer(
            &shell("printf 'summary of: '; cat"),
            "hello",
            Path::new("."),
        )
        .unwrap();
        assert_eq!(text, "summary of: hello");
    }

    #[test]
    fn summarizer_failure_reports_stderr() {
        let err = run_summarizer(&shell("echo boom >&2; exit 4"), "x", Path::new(".")).unwrap_err();
        assert!(format!("{err}").contains("boom"), "{err}");
    }

    #[test]
    fn summarizer_empty_output_is_an_error() {
        assert!(run_summarizer(&shell("cat >/dev/null"), "x", Path::new(".")).is_err());
    }

    #[test]
    fn summarizer_timeout_kills_the_command() {
        let mut config = shell("sleep 30");
        config.run_timeout_secs = 1;
        let start = Instant::now();
        let err = run_summarizer(&config, "x", Path::new(".")).unwrap_err();
        assert!(format!("{err}").contains("did not finish"));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn missing_command_is_an_error() {
        let config = SummaryConfig {
            command: "no-such-summarizer-xyz".into(),
            ..SummaryConfig::default()
        };
        assert!(run_summarizer(&config, "x", Path::new(".")).is_err());
    }
}
