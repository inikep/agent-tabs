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

/// The first variable `text` uses that is neither in `defined` nor in the
/// environment.
fn unset<'a>(text: &'a str, defined: &HashSet<&str>) -> Option<&'a str> {
    VAR.captures_iter(text)
        .map(|c| c.get(1).or_else(|| c.get(2)).expect("a name").as_str())
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
    let (named, positional): (Vec<&str>, Vec<&str>) =
        args.iter().partition(|a| assignment(a).is_some());
    let usage = || {
        if words.is_empty() {
            format!("/{name} takes no arguments other than NAME=value")
        } else {
            format!("usage: /{name} {} [NAME=value…]", words.join(" "))
        }
    };
    if positional.len() != words.len() {
        return Err(usage());
    }
    let mut set: Vec<(String, String)> = Vec::new();
    for (word, arg) in words.iter().zip(&positional) {
        let (names, pattern) = arg_pattern(word)?;
        let captures = pattern
            .captures(arg)
            .ok_or_else(|| format!("{arg} isn't {word}; {}", usage()))?;
        for (i, var) in names.into_iter().enumerate() {
            set.push((var, captures[i + 1].to_string()));
        }
    }
    for arg in named {
        let (var, value) = assignment(arg).expect("partitioned by assignment");
        set.push((var.to_string(), value.to_string()));
    }
    for (var, value) in &config.variables {
        if !set.iter().any(|(v, _)| v == var) {
            set.push((var.clone(), value.clone()));
        }
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
        let vars = with_variables(&[("REVIEW_DIR", "/r/$TARGET")]);
        let err = fill(REVIEW, "review", &["a..b"], &vars).unwrap_err();
        assert!(
            err.starts_with("line 9: $PROMPT_DIR is not set; set it under [commands.variables]"),
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
        let usage = "usage: /review BASE..TARGET [NAME=value…]";
        assert_eq!(fill_review(&["a..b", "c"]), Err(usage.into()));
        assert_eq!(fill_review(&["REVIEW_DIR=/x"]), Err(usage.into()));
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
