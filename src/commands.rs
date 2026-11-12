//! `/NAME ARGS…` prompts, which send the template `NAME.txt` from the
//! commands directory, by default the repository's `commands/`. A template
//! is a prompt or a script; its `::args` line names the variables its
//! arguments set, e.g.
//!
//! ```text
//! ::args BASE..TARGET
//! ::send all
//! Review the commits in $BASE..$TARGET into $REVIEW_DIR.
//! ```
//!
//! `/review percona/8.4..PS-2` then sends it with `BASE=percona/8.4`,
//! `TARGET=PS-2` and the config's `[commands.variables]` at the top.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::rounds::{assignment, expand_vars, variables};

static NAME: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("valid regex"));

static VAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\$(?:\{([A-Za-z_][A-Za-z0-9_]*)\}|([A-Za-z_][A-Za-z0-9_]*))")
        .expect("valid regex")
});

/// `[commands]` in the config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CommandsConfig {
    /// Where the templates are, `NAME.txt` each; by default the
    /// repository's `commands/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// `[commands.variables]`: `NAME = "value"` lines put at the top of
    /// every template, in this order, after its arguments. A value may use
    /// the arguments and the variables above it, e.g. `$TARGET`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,
    /// The harness that coordinates the others, e.g. merges their reviews:
    /// `$COORDINATOR` in a template, unless an argument or a variable sets
    /// it.
    pub coordinator: String,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            dir: None,
            variables: IndexMap::new(),
            coordinator: "opus-5.5".into(),
        }
    }
}

impl CommandsConfig {
    pub fn dir(&self) -> PathBuf {
        self.dir
            .clone()
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("commands"))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.coordinator.trim().is_empty(),
            "[commands]: coordinator is empty"
        );
        for (name, value) in &self.variables {
            anyhow::ensure!(
                NAME.find(name).is_some_and(|m| m.len() == name.len()),
                "[commands.variables]: {name} is not a variable name"
            );
            anyhow::ensure!(
                !value.contains('\n'),
                "[commands.variables]: {name} has more than one line"
            );
        }
        Ok(())
    }
}

/// A `/NAME` prompt with its template filled in.
#[derive(Debug)]
pub struct Expanded {
    pub path: PathBuf,
    pub text: String,
    /// The variable lines put above the template.
    added: usize,
}

impl Expanded {
    /// `err`, from parsing `text`, with the template's file and line.
    pub fn error(&self, err: &str) -> String {
        static LINE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"^line (\d+):").expect("valid regex"));
        let line = LINE
            .captures(err)
            .and_then(|c| Some((c.get(0)?.end(), c[1].parse::<usize>().ok()?)));
        match line {
            Some((end, no)) if no > self.added => format!(
                "{}: line {}:{}",
                self.path.display(),
                no - self.added,
                &err[end..]
            ),
            _ => format!("{}: {err}", self.path.display()),
        }
    }
}

/// The template a `/NAME ARGS…` prompt sends, read from the commands
/// directory each time. `Ok(None)` for any other prompt: one that is no
/// `/NAME`, has no `NAME.txt`, or has no arguments while its template needs
/// some; those go to the harnesses as they are, so their own `/NAME`
/// commands still work.
pub fn expand(text: &str, config: &CommandsConfig) -> Result<Option<Expanded>, String> {
    let mut words = text.split_whitespace();
    let Some(name) = words.next().and_then(|w| w.strip_prefix('/')) else {
        return Ok(None);
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Ok(None);
    }
    let path = config.dir().join(format!("{name}.txt"));
    let template = match std::fs::read_to_string(&path) {
        Ok(template) => template,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let args: Vec<&str> = words.collect();
    match fill(&template, name, &args, config) {
        Ok(Some((text, added))) => Ok(Some(Expanded { path, text, added })),
        Ok(None) => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// Whether `line` is the `::` command `name`.
fn is_command(line: &str, name: &str) -> bool {
    line.trim_start()
        .strip_prefix("::")
        .and_then(|rest| rest.strip_prefix(name))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// A template as the main tab lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    /// `/NAME` and its `::args`, e.g. `/review BASE..TARGET [NOTES...]`.
    pub usage: String,
    /// The `::#` lines it starts with, without the `::#`.
    pub description: Vec<String>,
}

/// The templates in the commands directory, by name; empty if there is
/// none or the directory can't be read.
pub fn list(config: &CommandsConfig) -> Vec<Template> {
    let Ok(entries) = std::fs::read_dir(config.dir()) else {
        return Vec::new();
    };
    let mut templates: Vec<(String, Template)> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_stem()?.to_str()?.to_string();
            if path.extension()? != "txt" {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            Some((name.clone(), describe(&name, &text)))
        })
        .collect();
    templates.sort_by(|a, b| a.0.cmp(&b.0));
    templates.into_iter().map(|(_, t)| t).collect()
}

fn describe(name: &str, template: &str) -> Template {
    let args = template
        .lines()
        .find(|l| is_args_line(l))
        .map(|l| l.trim_start()["::args".len()..].trim())
        .unwrap_or("");
    let usage = format!("/{name} {args}").trim_end().to_string();
    let description = template
        .lines()
        .map_while(|l| l.trim_start().strip_prefix("::#"))
        .map(|l| l.strip_prefix(' ').unwrap_or(l).to_string())
        .collect();
    Template { usage, description }
}

fn is_args_line(line: &str) -> bool {
    is_command(line, "args")
}

/// A regex matching one argument for an `::args` word such as
/// `BASE..TARGET`, and the variables its groups set.
fn arg_pattern(word: &str) -> Result<(Vec<String>, regex::Regex), String> {
    let found: Vec<regex::Match> = NAME.find_iter(word).collect();
    if found.is_empty() {
        return Err(format!("::args {word} names no variable"));
    }
    let mut pattern = String::from("^");
    let mut at = 0;
    for (i, m) in found.iter().enumerate() {
        pattern.push_str(&regex::escape(&word[at..m.start()]));
        // Every part but the last is as short as it can be, so `a..b..c`
        // splits at its first `..`.
        pattern.push_str(if i + 1 < found.len() { "(.+?)" } else { "(.+)" });
        at = m.end();
    }
    pattern.push_str(&regex::escape(&word[at..]));
    pattern.push('$');
    let names = found.iter().map(|m| m.as_str().to_string()).collect();
    Ok((names, regex::Regex::new(&pattern).expect("escaped pattern")))
}

fn var_of<'a>(caps: &regex::Captures<'a>) -> &'a str {
    caps.get(1)
        .or_else(|| caps.get(2))
        .expect("a name")
        .as_str()
}

/// The first variable `text` uses that is neither in `defined` nor in the
/// environment.
fn unset<'a>(text: &'a str, defined: &HashSet<&str>) -> Option<&'a str> {
    VAR.captures_iter(text)
        .map(|c| var_of(&c))
        .find(|v| !defined.contains(v) && std::env::var_os(v).is_none())
}

/// `template` with `NAME=value` lines on top: those its `::args` line gets
/// from `args`, then the `NAME=value` arguments, then the `variables` that
/// neither sets. Its `::args` line becomes a comment, and variables in the
/// targets of `::send` and `::wait-done` are replaced. Also returns how many
/// lines were put on top.
fn fill(
    template: &str,
    name: &str,
    args: &[&str],
    config: &CommandsConfig,
) -> Result<Option<(String, usize)>, String> {
    let mut lines: Vec<String> = template.lines().map(str::to_string).collect();
    let specs: Vec<usize> = (0..lines.len())
        .filter(|&i| is_args_line(&lines[i]))
        .collect();
    let spec_at = match specs.as_slice() {
        [] => None,
        [at] => Some(*at),
        _ => return Err("it has more than one ::args line".into()),
    };
    let words: Vec<String> = spec_at
        .map(|at| {
            lines[at].trim_start()["::args".len()..]
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if args.is_empty() && !words.is_empty() {
        return Ok(None);
    }
    // A last word `NAME...` takes the rest of the arguments, e.g. a task
    // in words: `::args TARGET TASK...`; in brackets, `[NOTES...]`, it may
    // be left out.
    let last = words.last().map(String::as_str).unwrap_or("");
    let optional = last.starts_with('[') && last.ends_with(']');
    let rest = last
        .trim_start_matches('[')
        .trim_end_matches(']')
        .strip_suffix("...")
        .filter(|w| NAME.find(w).is_some_and(|m| m.len() == w.len()))
        .map(str::to_string);
    // A plain `NAME` word may also be given as `NAME=value`.
    let fixed: Vec<&String> = words[..words.len() - usize::from(rest.is_some())]
        .iter()
        .filter(|word| {
            !args
                .iter()
                .any(|arg| assignment(arg).is_some_and(|(v, _)| v == *word))
        })
        .collect();
    let (mut named, mut positional, mut rest_words) = (Vec::new(), Vec::new(), Vec::new());
    for &arg in args {
        let is_named = assignment(arg).is_some();
        if rest.is_some()
            && positional.len() == fixed.len()
            && (!is_named || !rest_words.is_empty())
        {
            rest_words.push(arg);
        } else if is_named {
            named.push(arg);
        } else {
            positional.push(arg);
        }
    }
    let usage = || {
        if words.is_empty() {
            format!("/{name} takes no arguments other than NAME=value")
        } else {
            format!("usage: /{name} {} [NAME=value…]", words.join(" "))
        }
    };
    if positional.len() != fixed.len() || (rest.is_some() && !optional && rest_words.is_empty()) {
        return Err(usage());
    }
    let mut set: Vec<(String, String)> = Vec::new();
    for (word, arg) in fixed.iter().zip(&positional) {
        let (names, pattern) = arg_pattern(word)?;
        let captures = pattern
            .captures(arg)
            .ok_or_else(|| format!("{arg} isn't {word}; {}", usage()))?;
        for (i, var) in names.into_iter().enumerate() {
            set.push((var, captures[i + 1].to_string()));
        }
    }
    if let Some(rest) = rest {
        let value = rest_words.join(" ");
        // Quotes around the whole text are only there to group it.
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .map_or(value.clone(), str::to_string);
        set.push((rest, value));
    }
    for arg in named {
        let (var, value) = assignment(arg).expect("partitioned by assignment");
        set.push((var.to_string(), value.to_string()));
    }
    // Only the config variables the template needs, directly or through
    // another one, so `PLAN_DIR = "…/$TARGET"` doesn't break a template
    // without TARGET.
    let mut needed: HashSet<String> = lines
        .iter()
        .filter(|l| !l.trim_start().starts_with("::#"))
        .map(String::as_str)
        .chain(set.iter().map(|(_, value)| value.as_str()))
        .flat_map(|text| VAR.captures_iter(text).map(|c| var_of(&c).to_string()))
        .collect();
    loop {
        let more: Vec<String> = config
            .variables
            .iter()
            .filter(|(var, _)| needed.contains(var.as_str()))
            .flat_map(|(_, value)| VAR.captures_iter(value).map(|c| var_of(&c).to_string()))
            .filter(|var| !needed.contains(var))
            .collect();
        if more.is_empty() {
            break;
        }
        needed.extend(more);
    }
    for (var, value) in &config.variables {
        if needed.contains(var.as_str()) && !set.iter().any(|(v, _)| v == var) {
            set.push((var.clone(), value.clone()));
        }
    }
    // The prompts the templates point to, such as review.md, sit next to them.
    let uses = |var: &str| {
        lines
            .iter()
            .filter(|l| !l.trim_start().starts_with("::#"))
            .any(|l| VAR.captures_iter(l).any(|c| var_of(&c) == var))
    };
    if !set.iter().any(|(v, _)| v == "PROMPT_DIR") && uses("PROMPT_DIR") {
        set.push(("PROMPT_DIR".into(), config.dir().display().to_string()));
    }
    if let Some(at) = spec_at {
        lines[at] = format!("::# {}", lines[at].trim());
    }

    // A variable nothing sets would reach the models as `$NAME`.
    let mut defined: HashSet<&str> = HashSet::from(["MODEL_ID"]);
    let hint = |var: &str| {
        format!("set it under [commands.variables] in the config, or add {var}=… after /{name}")
    };
    for (var, value) in &set {
        if let Some(missing) = unset(value, &defined) {
            return Err(format!(
                "{var}={value} uses ${missing}, which is not set; {}",
                hint(missing)
            ));
        }
        defined.insert(var);
    }
    defined.insert("COORDINATOR");
    defined.extend(lines.iter().filter_map(|l| assignment(l).map(|(n, _)| n)));
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("::#") {
            continue;
        }
        if let Some(missing) = unset(line, &defined) {
            return Err(format!(
                "line {}: ${missing} is not set; {}",
                i + 1,
                hint(missing)
            ));
        }
    }

    // The script takes harness names as they are written, so `::send
    // $COORDINATOR` is filled in here.
    let header: String = set.iter().map(|(v, x)| format!("{v}={x}\n")).collect();
    let mut values = variables(&header, None);
    values
        .entry("COORDINATOR".into())
        .or_insert_with(|| Some(config.coordinator.clone()));
    for (i, line) in lines.iter_mut().enumerate() {
        if is_command(line, "send") || is_command(line, "wait-done") {
            *line = expand_vars(line, &values).ok_or_else(|| {
                format!(
                    "line {}: {} can only use variables set by arguments or the config",
                    i + 1,
                    line.trim()
                )
            })?;
        }
    }

    // A line that is only `$NAME`, for a variable set above, becomes its
    // value, or an empty line; one used only so isn't put on top.
    let mut inlined = HashSet::new();
    for line in &mut lines {
        let whole = VAR
            .captures(line.trim())
            .filter(|c| c[0].len() == line.trim().len())
            .map(|c| var_of(&c).to_string());
        if let Some((var, value)) = whole.and_then(|w| set.iter().find(|(v, _)| *v == w)) {
            inlined.insert(var.clone());
            *line = value.clone();
        }
    }
    let used = |var: &str| {
        lines
            .iter()
            .filter(|l| !l.trim_start().starts_with("::#"))
            .chain(set.iter().map(|(_, value)| value))
            .any(|text| VAR.captures_iter(text).any(|c| var_of(&c) == var))
    };
    let set: Vec<(String, String)> = set
        .iter()
        .filter(|(var, _)| !inlined.contains(var) || used(var))
        .cloned()
        .collect();

    let added = set.len();
    let text = set
        .into_iter()
        .map(|(var, value)| format!("{var}={value}"))
        .chain(lines)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some((text, added)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::{Command, Targets, parse};

    const REVIEW: &str = include_str!("../commands/review.txt");

    fn config() -> CommandsConfig {
        with_variables(&[("REVIEW_DIR", "/r/$TARGET"), ("PROMPT_DIR", "/p")])
    }

    fn with_variables(variables: &[(&str, &str)]) -> CommandsConfig {
        CommandsConfig {
            variables: variables
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            ..CommandsConfig::default()
        }
    }

    fn fill_review(args: &[&str]) -> Result<Option<String>, String> {
        fill(REVIEW, "review", args, &config()).map(|t| t.map(|(text, _)| text))
    }

    #[test]
    fn the_prompts_are_next_to_the_templates_unless_the_config_says_otherwise() {
        let config = CommandsConfig {
            dir: Some("/t".into()),
            ..with_variables(&[("REVIEW_DIR", "/r")])
        };
        let (text, _) = fill(REVIEW, "review", &["a..b"], &config).unwrap().unwrap();
        assert!(
            text.starts_with("BASE=a\nTARGET=b\nREVIEW_DIR=/r\nPROMPT_DIR=/t\n::#"),
            "{text}"
        );
        assert!(CommandsConfig::default().dir().join("review.md").exists());
        assert!(
            CommandsConfig::default()
                .dir()
                .join("review-merge.md")
                .exists()
        );
        let (text, _) = fill("::send all\nhi", "hi", &[], &config).unwrap().unwrap();
        assert!(!text.contains("PROMPT_DIR"), "only when used: {text}");
    }

    #[test]
    fn the_review_template_takes_a_range_and_the_config_variables() {
        let (text, added) = fill(REVIEW, "review", &["inikep/8.4..PS-2-8.4"], &config())
            .unwrap()
            .unwrap();
        assert_eq!(added, 4);
        assert_eq!(text.lines().count(), REVIEW.lines().count() + 4);
        let steps = parse(&text, &["opus-5.5", "gpt-6"]).unwrap().unwrap();
        assert_eq!(steps.len(), 4);
        let Command::Send { targets, text } = &steps[0].command else {
            panic!("{steps:?}");
        };
        assert_eq!(targets, &Targets::All);
        assert_eq!(
            text,
            "BASE=inikep/8.4\nTARGET=PS-2-8.4\nREVIEW_DIR=/r/$TARGET\nPROMPT_DIR=/p\nOUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md\nRead instructions from $PROMPT_DIR/review.md"
        );
        assert_eq!(
            steps[1].command.path(Some("GPT-6-high")).unwrap(),
            "/r/PS-2-8.4/review-GPT-6-high.md"
        );
        assert_eq!(steps[2].line, "::send opus-5.5");
        assert_eq!(
            steps[3].command.path(None).unwrap(),
            "/r/PS-2-8.4/review-combined.md"
        );
    }

    #[test]
    fn named_arguments_override_the_config() {
        let text = fill_review(&["a..b", "REVIEW_DIR=/tmp/x"])
            .unwrap()
            .unwrap();
        assert!(
            text.starts_with("BASE=a\nTARGET=b\nREVIEW_DIR=/tmp/x\nPROMPT_DIR=/p\n::#"),
            "{text}"
        );
        assert_eq!(
            fill(
                "::send all\nhi $WHO",
                "hi",
                &["WHO=you"],
                &with_variables(&[])
            ),
            Ok(Some(("WHO=you\n::send all\nhi $WHO".into(), 1)))
        );
    }

    #[test]
    fn the_coordinator_comes_from_the_config_or_an_argument() {
        let script = |text: &str| {
            let steps = parse(text, &["opus-5.5", "gpt-6"]).unwrap().unwrap();
            steps[2].line.clone()
        };
        assert_eq!(
            script(&fill_review(&["a..b"]).unwrap().unwrap()),
            "::send opus-5.5"
        );
        let config = CommandsConfig {
            coordinator: "gpt-6".into(),
            ..config()
        };
        let (text, _) = fill(REVIEW, "review", &["a..b"], &config).unwrap().unwrap();
        assert_eq!(script(&text), "::send gpt-6");
        assert!(!text.contains("COORDINATOR="), "{text}");
        let text = fill_review(&["a..b", "COORDINATOR=gpt-6"])
            .unwrap()
            .unwrap();
        assert_eq!(script(&text), "::send gpt-6");
        assert!(
            fill(
                "::send all\nhi\n::wait-done $NO_OUT_X",
                "x",
                &[],
                &with_variables(&[])
            )
            .unwrap_err()
            .starts_with("line 3: $NO_OUT_X is not set")
        );
        assert!(
            fill(
                "::send all\nOUT=x\nhi\n::wait-done $OUT",
                "x",
                &[],
                &with_variables(&[])
            )
            .unwrap_err()
            .starts_with("line 4: ::wait-done $OUT can only use variables set by arguments")
        );
    }

    #[test]
    fn a_last_word_with_dots_takes_the_rest() {
        const PLAN: &str = include_str!("../commands/plan-full.txt");
        let config = with_variables(&[("PLAN_DIR", "/p/$TARGET")]);
        let fill_plan = |args: &[&str]| fill(PLAN, "plan-full", args, &config);
        let (text, added) = fill_plan(&["PS-1", "PLAN_DIR=/x", "implement", "option", "B"])
            .unwrap()
            .unwrap();
        assert!(
            text.starts_with("TARGET=PS-1\nTASK=implement option B\nPLAN_DIR=/x\n"),
            "{text}"
        );
        assert_eq!(added, 3);
        let steps = parse(&text, &["opus-5.5"]).unwrap().unwrap();
        assert_eq!(steps.len(), 10);
        let Command::Send { text, .. } = &steps[0].command else {
            panic!("{steps:?}");
        };
        assert!(
            text.ends_with("In $OUTPUT_FILE create a plan to $TASK"),
            "{text}"
        );
        assert_eq!(steps[9].command.path(Some("M")).unwrap(), "/x/plan4-M.md");
        let (text, _) = fill_plan(&["PS-1", "use", "X=1"]).unwrap().unwrap();
        assert!(text.contains("\nTASK=use X=1\n"), "{text}");
        let usage = "usage: /plan-full TARGET TASK... [NAME=value…]";
        assert_eq!(fill_plan(&["PS-1"]), Err(usage.into()));
        assert_eq!(fill_plan(&["PS-1", "A=1"]), Err(usage.into()));
        assert_eq!(fill_plan(&[]), Ok(None));
    }

    #[test]
    fn plan_has_the_coordinator_improve_the_best_plan() {
        const PLAN: &str = include_str!("../commands/plan.txt");
        let config = with_variables(&[("PLAN_DIR", "/p/$TARGET")]);
        let (text, _) = fill(PLAN, "plan", &["PS-1", "do", "it"], &config)
            .unwrap()
            .unwrap();
        let steps = parse(&text, &["opus-5.5", "gpt-6"]).unwrap().unwrap();
        assert_eq!(steps.len(), 4);
        let Command::Send { targets, text } = &steps[2].command else {
            panic!("{steps:?}");
        };
        assert_eq!(targets, &Targets::Named(vec!["opus-5.5".into()]));
        assert!(text.contains("PLAN_FILES=$PLAN_DIR/plan-*.md\n"), "{text}");
        assert_eq!(
            steps[3].command.path(Some("M")).unwrap(),
            "/p/PS-1/plan2-final-M.md"
        );
    }

    #[test]
    fn implement_takes_its_arguments_by_position_or_by_name() {
        const IMPLEMENT: &str = include_str!("../commands/implement.txt");
        let config = with_variables(&[("IMPLEMENT_DIR", "/i/$BASE_BRANCH"), ("PROMPT_DIR", "/p")]);
        let fill_implement = |args: &[&str]| {
            fill(IMPLEMENT, "implement", args, &config).map(|t| t.map(|(text, _)| text))
        };
        let by_position = fill_implement(&["PS-1-trunk", "/plans/plan5.md"])
            .unwrap()
            .unwrap();
        let by_name = fill_implement(&["BASE_BRANCH=PS-1-trunk", "PLAN=/plans/plan5.md"])
            .unwrap()
            .unwrap();
        assert_eq!(by_position, by_name);
        let mixed = fill_implement(&["PS-1-trunk", "PLAN=/plans/plan5.md"])
            .unwrap()
            .unwrap();
        assert_eq!(mixed, by_name);
        assert!(
            by_name.starts_with(
                "BASE_BRANCH=PS-1-trunk\nPLAN=/plans/plan5.md\nIMPLEMENT_DIR=/i/$BASE_BRANCH\nPROMPT_DIR=/p\n::#"
            ),
            "{by_name}"
        );
        let steps = parse(&by_name, &["opus-5.5"]).unwrap().unwrap();
        assert_eq!(steps.len(), 6);
        let sends: Vec<(&Targets, &String)> = steps
            .iter()
            .filter_map(|s| match &s.command {
                Command::Send { targets, text } => Some((targets, text)),
                _ => None,
            })
            .collect();
        let coordinator = Targets::Named(vec!["opus-5.5".into()]);
        let expected = [
            (&Targets::All, "implement.md"),
            (&coordinator, "implement-compare.md"),
            (&coordinator, "implement-improve.md"),
        ];
        assert_eq!(sends.len(), expected.len());
        for ((targets, text), (want, prompt)) in sends.iter().zip(expected) {
            assert_eq!(*targets, want, "{text}");
            let read = format!("Read instructions from $PROMPT_DIR/{prompt}");
            assert!(text.lines().any(|l| l == read), "{text}");
            assert!(
                CommandsConfig::default().dir().join(prompt).exists(),
                "{prompt}"
            );
        }
        let model = "Opus-5.5-high";
        let waits: Vec<String> = [1, 3, 5]
            .iter()
            .map(|&i| steps[i].command.path(Some(model)).unwrap())
            .collect();
        assert_eq!(
            waits,
            [
                "/i/PS-1-trunk/implement-Opus-5.5-high.md",
                "/i/PS-1-trunk/compare.md",
                "/i/PS-1-trunk/final.md"
            ]
        );

        let text = fill_implement(&["PLAN=/x.md", "PS-1-trunk", "Don't", "build"])
            .unwrap()
            .unwrap();
        assert!(
            text.starts_with("BASE_BRANCH=PS-1-trunk\nPLAN=/x.md\n"),
            "{text}"
        );
        assert!(
            text.contains("implement.md\nDon't build\n::wait-model-files"),
            "{text}"
        );
        assert_eq!(
            fill_implement(&["PLAN=/x.md"]),
            Err("usage: /implement BASE_BRANCH PLAN [NOTES...] [NAME=value…]".into())
        );
    }

    #[test]
    fn config_variables_a_template_doesnt_use_are_left_out() {
        const IMPLEMENT: &str = include_str!("../commands/implement.txt");
        let config = with_variables(&[
            ("REVIEW_DIR", "/r/$TARGET"),
            ("PLAN_DIR", "/p/$TARGET"),
            ("SUB", "$PLAN_DIR/sub"),
            ("IMPLEMENT_DIR", "/i/$BASE_BRANCH"),
        ]);
        let (text, _) = fill(IMPLEMENT, "implement", &["PS-1-trunk", "/x.md"], &config)
            .unwrap()
            .unwrap();
        assert!(
            !text.contains("REVIEW_DIR") && !text.contains("PLAN_DIR"),
            "{text}"
        );
        let (text, _) = fill("::send all\nsee $SUB", "t", &["TARGET=PS-1"], &config)
            .unwrap()
            .unwrap();
        assert!(
            text.starts_with("TARGET=PS-1\nPLAN_DIR=/p/$TARGET\nSUB=$PLAN_DIR/sub\n::send"),
            "only what $SUB needs: {text}"
        );
    }

    #[test]
    fn review_notes_follow_the_instructions_or_are_left_out() {
        let steps = |text: &str| -> Vec<String> {
            parse(text, &["opus-5.5"])
                .unwrap()
                .unwrap()
                .into_iter()
                .filter_map(|s| match s.command {
                    Command::Send { text, .. } => Some(text),
                    _ => None,
                })
                .collect()
        };
        let text = fill_review(&["a..b", "Don't", "build.", "extra/libkmip", "is", "checked"])
            .unwrap()
            .unwrap();
        let sends = steps(&text);
        assert!(
            sends[0].ends_with(
                "Read instructions from $PROMPT_DIR/review.md\nDon't build. extra/libkmip is checked"
            ),
            "{}",
            sends[0]
        );
        assert!(
            sends[1].contains("review-merge.md\nDon't build. extra/libkmip is checked\n"),
            "{}",
            sends[1]
        );
        assert!(!text.contains("NOTES="), "the notes are inlined: {text}");

        let text = fill_review(&["a..b"]).unwrap().unwrap();
        let sends = steps(&text);
        assert!(sends[0].ends_with("$PROMPT_DIR/review.md"), "{}", sends[0]);
        assert!(
            !text.contains("NOTES=") && !text.contains("$NOTES"),
            "{text}"
        );
        assert_eq!(text.lines().count(), REVIEW.lines().count() + 4);

        let text = fill_review(&["a..b", "\"Don't", "build\""])
            .unwrap()
            .unwrap();
        assert!(text.contains("\nDon't build\n"), "{text}");
        assert_eq!(
            fill_review(&["a"]),
            Err(
                "a isn't BASE..TARGET; usage: /review BASE..TARGET [NOTES...] [NAME=value…]".into()
            )
        );
    }

    #[test]
    fn a_range_splits_at_its_first_dots() {
        let text = fill_review(&["a..b..c"]).unwrap().unwrap();
        assert!(text.starts_with("BASE=a\nTARGET=b..c\n"), "{text}");
    }

    #[test]
    fn without_arguments_the_harnesses_get_the_command() {
        assert_eq!(fill_review(&[]), Ok(None));
        let plain = "Say hi";
        assert_eq!(
            fill(plain, "hi", &[], &with_variables(&[])),
            Ok(Some((plain.into(), 0)))
        );
    }

    #[test]
    fn reports_unset_variables() {
        let err = fill(REVIEW, "review", &["a..b"], &with_variables(&[])).unwrap_err();
        assert!(
            err.starts_with("line 16: $REVIEW_DIR is not set; set it under [commands.variables]"),
            "{err}"
        );
        let vars = with_variables(&[("REVIEW_DIR", "$NO_ROOT_X/$TARGET")]);
        let err = fill(REVIEW, "review", &["a..b", "PROMPT_DIR=/p"], &vars).unwrap_err();
        assert!(
            err.starts_with("REVIEW_DIR=$NO_ROOT_X/$TARGET uses $NO_ROOT_X, which is not set"),
            "{err}"
        );
    }

    #[test]
    fn reports_wrong_arguments_and_templates() {
        let usage = "usage: /review BASE..TARGET [NOTES...] [NAME=value…]";
        assert_eq!(fill_review(&["REVIEW_DIR=/x"]), Err(usage.into()));
        assert_eq!(
            fill(
                "::args A\n::send all\nhi $A",
                "hi",
                &["x", "y"],
                &with_variables(&[])
            ),
            Err("usage: /hi A [NAME=value…]".into())
        );
        assert_eq!(
            fill_review(&["..b"]),
            Err(format!("..b isn't BASE..TARGET; {usage}"))
        );
        assert!(
            fill("hi", "hi", &["x"], &with_variables(&[]))
                .unwrap_err()
                .contains("takes no arguments")
        );
        assert!(
            fill("::args A\n::args B", "x", &["y"], &with_variables(&[]))
                .unwrap_err()
                .contains("more than one")
        );
    }

    #[test]
    fn errors_name_the_template_line() {
        let expanded = Expanded {
            path: "/c/review.txt".into(),
            text: String::new(),
            added: 4,
        };
        assert_eq!(
            expanded.error("line 16: unknown target x"),
            "/c/review.txt: line 12: unknown target x"
        );
        assert_eq!(
            expanded.error("the prompt only sets variables"),
            "/c/review.txt: the prompt only sets variables"
        );
    }

    #[test]
    fn expand_reads_the_template_named_by_the_command() {
        let dir = std::env::temp_dir().join(format!("agent-tabs-commands-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hi.txt"), "::args WHO\nSay hi to $WHO").unwrap();
        let config = CommandsConfig {
            dir: Some(dir.clone()),
            ..CommandsConfig::default()
        };
        let expanded = expand("/hi world\n", &config).unwrap().unwrap();
        assert_eq!(expanded.path, dir.join("hi.txt"));
        assert_eq!(expanded.text, "WHO=world\n::# ::args WHO\nSay hi to $WHO");
        assert!(expand("/hi", &config).unwrap().is_none());
        assert!(expand("/bye world", &config).unwrap().is_none());
        assert!(expand("say /hi world", &config).unwrap().is_none());
        assert!(expand("/../hi world", &config).unwrap().is_none());
        let err = expand("/hi a b", &config).unwrap_err();
        assert!(
            err.ends_with("hi.txt: usage: /hi WHO [NAME=value…]"),
            "{err}"
        );
        assert!(CommandsConfig::default().dir().join("review.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lists_the_templates_with_their_usage_and_comments() {
        let names: Vec<String> = list(&CommandsConfig::default())
            .into_iter()
            .map(|t| t.usage)
            .collect();
        assert_eq!(
            names,
            [
                "/implement BASE_BRANCH PLAN [NOTES...]",
                "/plan TARGET TASK...",
                "/plan-full TARGET TASK...",
                "/review BASE..TARGET [NOTES...]"
            ]
        );
        let review = describe("review", REVIEW);
        assert!(
            review.description[0]
                .starts_with("/review BASE..TARGET [NOTES...]: every checked model"),
            "{review:?}"
        );
        assert_eq!(review.description.len(), 11);
        assert_eq!(
            describe("hi", "Say hi"),
            Template {
                usage: "/hi".into(),
                description: Vec::new()
            }
        );
    }

    #[test]
    fn config_variables_keep_their_order() {
        let config: crate::config::Config = toml::from_str(
            "[commands.variables]\nZ = \"1\"\nA = \"$Z\"\n[[harness]]\nname = \"a\"\ncommand = \"a\"",
        )
        .unwrap();
        let names: Vec<&String> = config.commands.variables.keys().collect();
        assert_eq!(names, ["Z", "A"]);
        config.validate().unwrap();
        assert_eq!(config.commands.coordinator, "opus-5.5");
        let bad: crate::config::Config =
            toml::from_str("[commands.variables]\n\"A-B\" = \"1\"").unwrap();
        assert!(bad.validate().is_err());
    }
}
