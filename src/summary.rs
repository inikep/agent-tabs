use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use ratatui::style::Style;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SummaryConfig {
    /// Whether to compare the answers of a round with `command`, shown as a
    /// last "comparison" section.
    pub compare: bool,
    pub command: String,
    pub args: Vec<String>,
    pub auto: bool,
    pub quiet_secs: u64,
    pub wait_timeout_secs: u64,
    pub run_timeout_secs: u64,
    pub max_lines: usize,
    pub env_remove: Vec<String>,
    /// Variables that name the file a prompt asks each harness to write, e.g.
    /// `OUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md`. A harness given such a
    /// prompt is done once it has written that file, which is shown instead
    /// of its screen.
    pub output_vars: Vec<String>,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            compare: false,
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
            output_vars: vec!["OUTPUT_FILE".into(), "REVIEW_COMBINED".into()],
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
    pub model: Option<String>,
    pub at: Instant,
    /// The harness's own style of each char, by line text.
    pub styles: HashMap<String, Vec<Style>>,
    /// The output file `text` was read from, and its modification time.
    pub report: Option<(PathBuf, SystemTime)>,
}

/// A line of chars, each with the style the harness drew it in.
pub type Cells = Vec<(char, Style)>;

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
    /// The harness got another prompt after this round's, so later output
    /// belongs to that prompt.
    pub followed_up: Vec<bool>,
    /// The answer of a finished harness that started working again, kept to
    /// tell whether its new answer differs.
    pub reopened: Vec<Option<String>>,
    /// The file the prompt asked each harness to write, and when the prompt
    /// was sent; only a file written after that counts.
    pub outputs: Vec<Option<(PathBuf, SystemTime)>>,
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
            followed_up: vec![false; n],
            reopened: vec![None; n],
            outputs: vec![None; n],
        }
    }

    pub fn state(&self) -> SummaryState {
        read_state(&self.state)
    }

    pub fn answer(&self, slot: usize) -> Option<String> {
        let capture = self.captures[slot].as_ref()?;
        if capture.report.is_some() {
            return Some(capture.text.trim_end().to_string());
        }
        Some(final_message(&extract_answer(&capture.text, &self.prompt)))
    }

    /// The answer reflowed for display, in the harness's own colors. A report
    /// file is shown line for line.
    pub fn answer_lines(&self, slot: usize) -> Option<Vec<Cells>> {
        let answer = self.answer(slot)?;
        let capture = self.captures[slot].as_ref()?;
        if capture.report.is_some() {
            return Some(report_lines(&answer));
        }
        Some(reflow_styled(&answer, &capture.styles))
    }

    /// One line shown on a folded section.
    pub fn headline(&self, slot: usize) -> Option<String> {
        let answer = self.answer(slot)?;
        let report = self.captures[slot].as_ref()?.report.is_some();
        report
            .then(|| report_headline(&answer))
            .flatten()
            .or_else(|| headline(&answer))
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

/// The file `prompt` asks to be written: the value of the first of `vars`
/// assigned in its `NAME=value` lines, with `$NAME` and `${NAME}` expanded from
/// the lines before it and from `model_id` as `MODEL_ID`. None if no such
/// line exists or it uses a variable that is not defined.
pub fn output_file(prompt: &str, vars: &[String], model_id: Option<&str>) -> Option<PathBuf> {
    let values = variables(prompt, model_id);
    vars.iter()
        .find_map(|var| values.get(var))
        .cloned()
        .flatten()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The `NAME` and value of a `NAME=value` line.
pub fn assignment(line: &str) -> Option<(&str, &str)> {
    static ASSIGNMENT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$").expect("valid regex")
    });
    let caps = ASSIGNMENT.captures(line)?;
    Some((caps.get(1)?.as_str(), caps.get(2)?.as_str()))
}

/// The variables a prompt's `NAME=value` lines define, each expanded from the
/// lines before it and from `model_id` as `MODEL_ID`; None for a value that
/// uses an undefined variable.
pub fn variables(prompt: &str, model_id: Option<&str>) -> HashMap<String, Option<String>> {
    let mut values: HashMap<String, Option<String>> = HashMap::new();
    if let Some(model_id) = model_id {
        values.insert("MODEL_ID".into(), Some(model_id.to_string()));
    }
    for line in prompt.lines() {
        let Some((name, value)) = assignment(line) else {
            continue;
        };
        let mut value = value.trim();
        if let Some((before, _)) = value.split_once(" #") {
            value = before.trim_end();
        }
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        let expanded = expand_vars(value, &values);
        values.insert(name.to_string(), expanded);
    }
    values
}

/// `value` with `$NAME` and `${NAME}` replaced; None if one is undefined.
pub fn expand_vars(value: &str, values: &HashMap<String, Option<String>>) -> Option<String> {
    static VAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$(?:\{([A-Za-z_][A-Za-z0-9_]*)\}|([A-Za-z_][A-Za-z0-9_]*))")
            .expect("valid regex")
    });
    let mut out = String::new();
    let mut last = 0;
    for caps in VAR.captures_iter(value) {
        let whole = caps.get(0).expect("match");
        let name = caps.get(1).or_else(|| caps.get(2)).expect("name").as_str();
        out.push_str(&value[last..whole.start()]);
        out.push_str(values.get(name)?.as_deref()?);
        last = whole.end();
    }
    out.push_str(&value[last..]);
    Some(out)
}

/// A report's verdict and findings for a folded section, e.g.
/// `changes-requested · 3 findings (1 major, 2 minor)`.
pub fn report_headline(report: &str) -> Option<String> {
    let field = |line: &str, name: &str| {
        line.trim()
            .trim_start_matches("- ")
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(|v| v.trim().to_string())
    };
    let verdict = report.lines().find_map(|l| field(l, "verdict"))?;
    let mut findings = 0;
    let mut severities: Vec<(String, usize)> = Vec::new();
    let mut in_findings = false;
    for line in report.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            in_findings = heading.trim().eq_ignore_ascii_case("findings");
        } else if in_findings && line.starts_with("### ") {
            findings += 1;
        } else if in_findings && let Some(severity) = field(line, "severity") {
            let severity = severity.split_whitespace().next().unwrap_or("").to_string();
            match severities.iter_mut().find(|(s, _)| *s == severity) {
                Some((_, n)) => *n += 1,
                None => severities.push((severity, 1)),
            }
        }
    }
    let mut text = format!(
        "{verdict} · {findings} finding{}",
        if findings == 1 { "" } else { "s" }
    );
    if !severities.is_empty() {
        let list: Vec<String> = severities.iter().map(|(s, n)| format!("{n} {s}")).collect();
        text.push_str(&format!(" ({})", list.join(", ")));
    }
    Some(text)
}

/// A Markdown report, one display line per line, with headings in bold and
/// blocker and major severities in red and yellow.
fn report_lines(report: &str) -> Vec<Cells> {
    use ratatui::style::{Color, Modifier};
    report
        .lines()
        .map(|line| {
            let text = line.trim_end();
            let lower = text.to_ascii_lowercase();
            let style = if text.starts_with('#') {
                Style::default().add_modifier(Modifier::BOLD)
            } else if lower.contains("severity: blocker") {
                Style::default().fg(Color::Red)
            } else if lower.contains("severity: major") {
                Style::default().fg(Color::Yellow)
            } else if text == "---" {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            text.chars().map(|c| (c, style)).collect()
        })
        .collect()
}

/// A row or border of a table drawn with box characters, as in
/// `┌──┬──┐`, `│a │b │` or `└──┴──┘`.
fn is_table_line(line: &str) -> bool {
    let text = line.trim();
    let (Some(first), Some(last)) = (text.chars().next(), text.chars().last()) else {
        return false;
    };
    text.chars().count() >= 3
        && matches!(
            (first, last),
            ('│', '│')
                | ('┌', '┐')
                | ('├', '┤')
                | ('└', '┘')
                | ('┃', '┃')
                | ('╭', '╮')
                | ('╰', '╯')
        )
        && !(first == '│' && text.chars().filter(|c| *c == '│').count() < 2)
}

fn is_separator(line: &str) -> bool {
    let text = line.trim();
    if is_table_line(line) {
        return false;
    }
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

const INPUT_MARKERS: [&str; 5] = ["❯", "›", ">", "→", "┃"];

fn is_input_line(line: &str) -> bool {
    if is_step_line(line) {
        return false;
    }
    let text = line.trim_start();
    INPUT_MARKERS.iter().any(|marker| {
        text.strip_prefix(marker)
            .is_some_and(|rest| rest.trim().is_empty() || rest.starts_with(char::is_whitespace))
    })
}

fn normalize(text: &str) -> String {
    let text = text.trim_start();
    let text = INPUT_MARKERS
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
            r"^\s*[●•○◦⏺✱→⚙◇]\s*(?:(Explored|Explore|Exploring|Read|Reading|Ran|Running|Searched|Searching|Search|Listed|List|Edited|Edit|Editing|Update|Updated|Write|Wrote|Thought|Thinking|Bash|Grep|Glob|Called|Fetched|Fetch|WebFetch|Task|Agent|Patch|Todo|Todos|Shell)\b|[A-Z]\w*\()",
        )
        .expect("valid regex")
    });
    STEP.is_match(line)
}

fn is_tool_line(line: &str) -> bool {
    let text = line.trim_start();
    is_step_line(line)
        || (text.starts_with(['⎿', '└', '├', '│']) && !is_table_line(line))
        || text.starts_with("$ ")
}

fn is_thinking_line(line: &str) -> bool {
    static THINKING: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^\s*┃?\s*[▸▾○●•◦⏺✻+]?\s*(Thought|Thinking)\b").expect("valid regex")
    });
    THINKING.is_match(line)
}

fn is_message_start(line: &str) -> bool {
    line.trim_start().starts_with(['●', '•', '⏺'])
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_right_aligned_notice(line: &str) -> bool {
    indent(line) >= 40
}

fn is_status_line(line: &str) -> bool {
    line.trim_start().starts_with('▣')
}

fn is_noise_line(line: &str) -> bool {
    is_timing_line(line) || is_status_line(line) || is_right_aligned_notice(line)
}

pub fn strip_right_panel(text: &str) -> String {
    let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
    let width = lines.iter().map(Vec::len).max().unwrap_or(0);
    if width < 80 {
        return text.to_string();
    }
    let gap = 3;
    for column in (width * 2 / 5).max(gap)..width.saturating_sub(8) {
        let mut starts = 0;
        let mut crossing = 0;
        for line in &lines {
            if line.len() <= column {
                continue;
            }
            if line[column - gap..column].iter().any(|c| *c != ' ') {
                crossing += 1;
            } else if line[column] != ' ' {
                starts += 1;
            }
        }
        if starts >= 4 && crossing == 0 {
            return lines
                .iter()
                .map(|line| {
                    line.iter()
                        .take(column)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    text.to_string()
}

pub fn final_message(answer: &str) -> String {
    let lines: Vec<&str> = answer.lines().collect();
    let mut start = None;
    let mut end = lines.len();
    let mut tool_indent: Option<usize> = None;
    // The indent of the harness's message markers (codex's `•` at column 0).
    // A deeper marker is a list item inside the message, not a new message.
    let mut marker_indent: Option<usize> = None;
    // The indent of the current message's plain text. A marker that is not
    // left of it is a list item too (agy indents both by two).
    let mut text_indent: Option<usize> = None;
    let mut thinking = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() || is_right_aligned_notice(line) {
            continue;
        }
        if is_input_line(line) && !is_thinking_line(line) {
            start = None;
            tool_indent = None;
            marker_indent = None;
            thinking = false;
            continue;
        }
        if start.is_some() && is_separator(line) {
            end = i;
            break;
        }
        if is_thinking_line(line) {
            thinking = true;
            tool_indent = None;
            continue;
        }
        if thinking && (line.trim_end().ends_with('…') || line.trim_end().ends_with("...")) {
            continue;
        }
        let after_thinking = std::mem::take(&mut thinking);
        if is_noise_line(line) {
            continue;
        }
        if is_tool_line(line) {
            if is_message_start(line) {
                marker_indent = Some(marker_indent.map_or(indent(line), |m| m.min(indent(line))));
            }
            tool_indent = Some(tool_indent.map_or(indent(line), |t| t.min(indent(line))));
            continue;
        }
        if tool_indent.is_some_and(|t| indent(line) > t) {
            continue;
        }
        let after_tool = tool_indent.take().is_some();
        let marker = is_message_start(line);
        let new_message = marker
            && marker_indent.is_none_or(|m| indent(line) <= m)
            && text_indent.is_none_or(|t| indent(line) < t);
        if new_message {
            marker_indent = Some(indent(line));
        }
        if after_thinking || new_message || after_tool || start.is_none() {
            start = Some(i);
            text_indent = None;
        }
        if !marker {
            text_indent = Some(text_indent.map_or(indent(line), |t| t.min(indent(line))));
        }
    }
    let Some(start) = start else {
        return answer.to_string();
    };
    let mut message: Vec<&str> = lines[start..end]
        .iter()
        .copied()
        .filter(|l| !is_noise_line(l) && !is_thinking_line(l))
        .collect();
    while message.last().is_some_and(|l| l.trim().is_empty()) {
        message.pop();
    }
    message.join("\n")
}

#[cfg(test)]
pub fn reflow(text: &str) -> Vec<String> {
    reflow_styled(text, &HashMap::new())
        .into_iter()
        .map(|line| line.into_iter().map(|(c, _)| c).collect())
        .collect()
}

/// Joins lines the harness wrapped back into paragraphs, keeping the style of
/// each char from `styles` (looked up by the line's text).
pub fn reflow_styled(text: &str, styles: &HashMap<String, Vec<Style>>) -> Vec<Cells> {
    let mut out: Vec<Cells> = Vec::new();
    let mut base_indent = 0;
    let mut after_table = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(Vec::new());
            }
            continue;
        }
        let table = is_table_line(line);
        let continues = out.last().is_some_and(|l| !l.is_empty())
            && !starts_item(line)
            && !table
            && !after_table
            && indent(line) <= base_indent + 2;
        after_table = table;
        let cells = trimmed_cells(line, styles);
        if continues {
            let last = out.last_mut().expect("checked non-empty");
            last.push((' ', Style::default()));
            last.extend(cells);
        } else {
            base_indent = indent(line);
            out.push(cells);
        }
    }
    while out.last().is_some_and(Vec::is_empty) {
        out.pop();
    }
    out
}

fn trimmed_cells(line: &str, styles: &HashMap<String, Vec<Style>>) -> Cells {
    let style = styles.get(line.trim_end());
    let lead = line.chars().take_while(|c| c.is_whitespace()).count();
    line.trim()
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let style = style.and_then(|s| s.get(lead + i)).copied();
            (c, style.unwrap_or_default())
        })
        .collect()
}

pub fn headline(answer: &str) -> Option<String> {
    let lines: Vec<&str> = answer.lines().collect();
    let last = lines
        .iter()
        .rposition(|l| !l.trim().is_empty() && !is_noise_line(l))?;
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
user input. Ignore the chrome and focus on what each agent said and did. An agent with a report \
attribute wrote that file as its answer, and its text is that file rather than a screen.\n\n\
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
        let model = capture
            .model
            .as_deref()
            .map(|m| format!(" model=\"{}\"", m.replace('"', "'")))
            .unwrap_or_default();
        let report = capture
            .report
            .as_ref()
            .map(|(path, _)| format!(" report=\"{}\"", path.display()))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n<agent name=\"{name}\"{model}{report} status=\"{status}\">\n"
        ));
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
            model: None,
            at: Instant::now(),
            styles: HashMap::new(),
            report: None,
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
    fn final_message_keeps_nested_bullets_inside_codex_message() {
        let answer = "• Yes—the reported issues are fixed in 26b523245570.\n\n  • The startup guard prevents pruning.\n  • The reproducer now passes.\n  • All nine targeted tests passed.\n\n  No new correctness issues found in the update.\n\n  Worked for 3m 1s";
        assert_eq!(
            final_message(answer),
            "• Yes—the reported issues are fixed in 26b523245570.\n\n  • The startup guard prevents pruning.\n  • The reproducer now passes.\n  • All nine targeted tests passed.\n\n  No new correctness issues found in the update."
        );
        let after_tool = "• Ran ls\n  └ a\n• Done:\n\n  • first\n  • second";
        assert_eq!(
            final_message(after_tool),
            "• Done:\n\n  • first\n  • second"
        );
    }

    #[test]
    fn final_message_keeps_agy_bullets_level_with_its_text() {
        let screen = "> what is your $MODEL_ID in Chinese ?\n▸ Thought for 8s, 210 tokens\n  I'm currently processing the request...\n  In Chinese, Gemini 4 Argon translates to:\n\n  • Literal translation: 双子座-4-氩-高\n  • Common tech usage: Gemini 4 氩 (高)\n\n  ### Breakdown:\n\n  • Gemini: 双子座\n  • High: 高";
        let answer = final_message(&extract_answer(
            screen,
            "what is your $MODEL_ID in Chinese ?",
        ));
        assert!(
            answer.starts_with("  In Chinese, Gemini 4 Argon translates to:"),
            "{answer}"
        );
        assert!(answer.ends_with("  • High: 高"), "{answer}");
        // Codex messages at column 0 with text indented under them still split.
        assert_eq!(
            final_message("• First message\n  wrapped on.\n• Second message"),
            "• Second message"
        );
    }

    #[test]
    fn final_message_keeps_a_cursor_review_with_bullets_and_code() {
        let answer = "  Verdict: ✅ Correct — no source changes needed\n\n  Correctness review\n\n  • Alphabet fixed: old alphabet had 60 chars, missing U/V; new one has all 62 unique\n    alphanumerics.\n\n   /work/x/audit_keyring.cc lines 59-67\n\n  bool get_random_string(std::string &str) {\n    static constexpr size_t rand_string_size = 126;\n  }\n\n  Validation (in the designated build dir)\n\n  • ninja component_audit_log_filter — builds/links cleanly.\n  • git diff --check clean; both changed .cc files are exact clang-format-15 matches;\n    /work/x left untouched (git status clean, HEAD unchanged).\n\n  One non-blocking observation\n\n  The added test asserts only the format.";
        assert_eq!(final_message(answer), answer);
    }

    #[test]
    fn final_message_keeps_a_table() {
        let answer = "     mtr results (component_audit_log_filter suite)\n\n     ┌──────────┬───────────────┐\n     │Result    │Tests          │\n     ├──────────┼───────────────┤\n     │PASS      │udf_get,       │\n     │          │log_format     │\n     └──────────┴───────────────┘\n\n     The failures were proven pre-existing.\n\n     Implemented by GLM-5.3 high";
        assert_eq!(final_message(answer), answer);
        let screen = format!("› run it\n{answer}\n────────────────\n› Ask me");
        let extracted = extract_answer(&screen, "run it");
        assert!(
            extracted.contains("│PASS      │udf_get,       │"),
            "{extracted}"
        );
        assert!(
            extracted.ends_with("Implemented by GLM-5.3 high"),
            "{extracted}"
        );
        let lines = reflow(answer);
        assert!(
            lines.contains(&"│Result    │Tests          │".to_string()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"│          │log_format     │".to_string()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"The failures were proven pre-existing.".to_string()),
            "{lines:#?}"
        );
        assert!(is_tool_line("  └ out.txt") && is_tool_line("  │ x"));
    }

    #[test]
    fn output_file_expands_the_prompts_variables() {
        let vars = SummaryConfig::default().output_vars;
        let prompt = "BASE=inikep/8.4\nTARGET=PS-11934-8.4-H08\nREVIEW_DIR=/data/reviews/$TARGET\nOUTPUT_FILE=$REVIEW_DIR/review-${MODEL_ID}.md\nRead instructions from review.md.";
        assert_eq!(
            output_file(prompt, &vars, Some("GPT-6-Astra-high")),
            Some(PathBuf::from(
                "/data/reviews/PS-11934-8.4-H08/review-GPT-6-Astra-high.md"
            ))
        );
        assert_eq!(output_file(prompt, &vars, None), None, "MODEL_ID unknown");
        assert_eq!(output_file("just a question", &vars, Some("M")), None);
        let merge = "TARGET=T\nREVIEW_FILES=/r/$TARGET/review-*.md\nREVIEW_COMBINED=\"/r/$TARGET/combined.md\"  # merged";
        assert_eq!(
            output_file(merge, &vars, None),
            Some(PathBuf::from("/r/T/combined.md"))
        );
        assert_eq!(
            output_file("OUTPUT_FILE=$NOPE/x.md", &vars, Some("M")),
            None
        );
        assert_eq!(
            output_file("OUTPUT_FILE=out/$MODEL_ID.md", &vars, Some("M")),
            Some(PathBuf::from("out/M.md"))
        );
    }

    #[test]
    fn report_headline_gives_verdict_and_findings() {
        let report = "---\nmodel: M\nverdict: changes-requested\n---\n\n## Findings\n\n### M-F1\n- title: a\n- severity: major\n\n### M-F2\n- severity: minor\n\n### M-F3\n- severity: minor\n\n## Checked and correct\n- x: ok\n";
        assert_eq!(
            report_headline(report).unwrap(),
            "changes-requested · 3 findings (1 major, 2 minor)"
        );
        assert_eq!(
            report_headline("verdict: approve\n## Findings\nnone\n").unwrap(),
            "approve · 0 findings"
        );
        assert!(report_headline("# Notes\nno front matter").is_none());
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
    fn agy_answer_after_tool_step_and_thinking() {
        let screen = "> In one sentence: what does get() return?\n● Explored 1 file (a.c) (ctrl+o to expand)\n▸ Thought for 7s, 196 tokens\n  I'm examining the `get` function. My current focus is on the conditional grea...\n  When i is out of range, get() returns -1 if i > n,\n  and reads a[i] if i == n.\n──────────────────────────\n>\n──────────────────────────\n? for shortcuts";
        let answer = final_message(&extract_answer(
            screen,
            "In one sentence: what does get() return?",
        ));
        assert_eq!(
            answer,
            "  When i is out of range, get() returns -1 if i > n,\n  and reads a[i] if i == n."
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
    fn claude_notice_above_the_prompt_is_not_the_answer() {
        let screen = " ▐▛███▛█   Claude Code v2.1.305\n▝▜██████▀  Opus 5.5 with high effort · Claude Team\n ▝▝   ▝▝   /tmp/e2e\n⚠ Your login expires in 1 day · run /login to renew\n❯ Reply with exactly: MODELS OK\n● MODELS OK\n✻ Cogitated for 3s · done 11:16 PM\n────────────────────────────────────────\n❯ \n────────────────────────────────────────\n  Opus 5.5 high | v2.1.305 | /tmp/e2e\n  ⏵⏵ auto mode on (shift+tab to cycle) · ← 1 agent";
        let answer = final_message(&extract_answer(screen, "Reply with exactly: MODELS OK"));
        assert_eq!(answer, "● MODELS OK");
        let with_toast = screen.replace(
            "✻ Cogitated for 3s · done 11:16 PM\n",
            "✻ Baked for 1s · done 11:17 PM\n\n                                                  Your login expires in 1 day · run /login to renew\n",
        );
        assert_eq!(
            final_message(&extract_answer(
                &with_toast,
                "Reply with exactly: MODELS OK"
            )),
            "● MODELS OK"
        );
        assert_eq!(headline(&answer).as_deref(), Some("● MODELS OK"));
    }

    const OPENCODE_SCREEN: &str = "  ┃                                                                                                           a.c get() out-of-range return
  ┃  In one sentence: what does get() in a.c return when i is out of range? Do not edit files.                behavior
  ┃
                                                                                                              Context
     + Thought: 490ms                                                                                         31,614 tokens
                                                                                                              3% used
     ✱ Glob \"**/a.c\" (1 match)                                                                                $0.13 spent
     → Read a.c
                                                                                                              ▼ MCP
     + Thought: 1.2s                                                                                          • github Connected
                                                                                                              • google-sheets MCP error -3200
     It returns -1 only when i > n; however, due to an off-by-one bug (i <= n instead of i < n), an out-        Connection closed
     of-range i == n (and any negative i) returns a[i] — an out-of-bounds read.                               • jenkins Needs auth
                                                                                                              • lad Connected

     ▣  Build · Kimi K3 (Global) · 12.8s                                                                      • serena Connected
                                                                                                              LSP

  ┃
  ┃                                                                                                           /tmp/claude-1000/-work-percona-
  ┃  Build · Kimi K3 (Global) Amazon Bedrock                                                                  4c26-8e72-766f9f1be90b/scratch
  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
   /tmp/e2e                                                                   31.6K (3%) · $0 ctrl+p";

    #[test]
    fn opencode_sidebar_is_stripped() {
        let stripped = strip_right_panel(OPENCODE_SCREEN);
        assert!(
            !stripped.contains("Connected") && !stripped.contains("Context"),
            "{stripped}"
        );
        assert!(
            stripped.contains("an out-\n     of-range i == n"),
            "{stripped}"
        );
        let plain = "a short screen\nwith no sidebar";
        assert_eq!(strip_right_panel(plain), plain);
    }

    #[test]
    fn opencode_final_message_and_headline() {
        let prompt = "In one sentence: what does get() in a.c return when i is out of range? Do not edit files.";
        let answer = final_message(&extract_answer(&strip_right_panel(OPENCODE_SCREEN), prompt));
        assert_eq!(
            answer,
            "     It returns -1 only when i > n; however, due to an off-by-one bug (i <= n instead of i < n), an out-\n     of-range i == n (and any negative i) returns a[i] — an out-of-bounds read."
        );
        assert!(
            headline(&answer)
                .unwrap()
                .starts_with("It returns -1 only when i > n")
        );
        assert!(is_step_line("     → Read a.c"));
        assert!(!is_input_line("     → Read a.c"));
        assert!(is_input_line("  → Add a follow-up"));
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
    fn prompt_includes_the_model_when_known() {
        let mut a = capture("x", true);
        a.model = Some("Opus 5.5 high".into());
        let text = build_prompt("q", &[("claude", &a)]);
        assert!(
            text.contains("<agent name=\"claude\" model=\"Opus 5.5 high\" status=\"finished\">"),
            "{text}"
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
