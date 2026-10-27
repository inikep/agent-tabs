//! Prompts with `::` commands that run as a sequence of steps, e.g.
//!
//! ```text
//! TARGET=PS-1
//! ::send all
//! OUTPUT_FILE=/reviews/$TARGET/review-$MODEL_ID.md
//! Review $TARGET.
//! ::wait-model-files $OUTPUT_FILE
//! ::send opus
//! Merge the reviews.
//! ```

use serde::{Deserialize, Serialize};

use crate::rounds::{assignment, expand_vars, variables};

const COMMANDS: [&str; 4] = ["send", "wait-model-files", "wait-file", "wait-done"];

/// Who a command is for: `all` (every harness that is not disabled) or
/// harnesses by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Targets {
    All,
    Named(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// Send `text` to the targets as one round.
    Send { targets: Targets, text: String },
    /// Wait for one file per target, `path` expanded with each one's
    /// MODEL_ID. Without targets, the targets of the last `::send`.
    WaitModelFiles {
        path: String,
        targets: Option<Targets>,
        vars: String,
    },
    /// Wait for one file.
    WaitFile { path: String, vars: String },
    /// Wait until the targets are done. Without targets, the targets of the
    /// last `::send`.
    WaitDone { targets: Option<Targets> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The `::` line, as written.
    pub line: String,
    pub command: Command,
}

impl Command {
    /// The path a wait command waits for, for a harness with `model_id`.
    pub fn path(&self, model_id: Option<&str>) -> Option<String> {
        let (path, vars) = match self {
            Command::WaitModelFiles { path, vars, .. } | Command::WaitFile { path, vars } => {
                (path, vars)
            }
            _ => return None,
        };
        expand_vars(path, &variables(vars, model_id)).filter(|p| !p.is_empty())
    }
}

/// Whether `line` is a `::` command line, and the command's name.
fn command_name(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("::")?;
    if rest.starts_with('#') {
        return Some("#");
    }
    let name = rest.split_whitespace().next().unwrap_or("");
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-')).then_some(name)
}

/// Splits command arguments at whitespace, keeping quoted parts together.
fn split_args(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unterminated quote".into());
    }
    if started {
        args.push(current);
    }
    Ok(args)
}

fn text_lines<'a>(lines: &[(usize, &'a str)]) -> Vec<&'a str> {
    lines.iter().map(|(_, l)| *l).collect()
}

fn is_variables_only(lines: &[&str]) -> bool {
    lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| assignment(l).is_some())
}

/// Parses a prompt. `Ok(None)` is an ordinary prompt; `Ok(Some(steps))` a
/// script; `Err` says what is wrong, with its line number. `harnesses` are
/// the names a target may use.
pub fn parse(text: &str, harnesses: &[&str]) -> Result<Option<Vec<Step>>, String> {
    static MID_LINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\S\s*::(send|wait-model-files|wait-file|wait-done)\b")
            .expect("valid regex")
    });
    // Line numbers count from 1; `::#` comment lines are dropped.
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .filter(|(_, l)| command_name(l) != Some("#"))
        .collect();
    for (no, line) in &lines {
        if let Some(m) = MID_LINE.captures(line) {
            return Err(format!("line {no}: \"::{}\" must start a line", &m[1]));
        }
    }
    let is_command = |line: &str| command_name(line).is_some_and(|n| COMMANDS.contains(&n));
    let Some(first) = lines.iter().position(|(_, l)| is_command(l)) else {
        if is_variables_only(&text_lines(&lines)) {
            return Err(
                "the prompt only sets variables, so there is nothing to do; add an instruction or a ::send"
                    .into(),
            );
        }
        return Ok(None);
    };
    // In a script, a `::name` line that is no command is likely a typo.
    for (no, line) in &lines {
        if let Some(name) = command_name(line)
            && !COMMANDS.contains(&name)
        {
            return Err(format!(
                "line {no}: unknown command ::{name}; commands are ::{}",
                COMMANDS.join(", ::")
            ));
        }
    }
    let header: Vec<&str> = text_lines(&lines[..first])
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if let Some((no, _)) = lines[..first]
        .iter()
        .find(|(_, l)| !l.trim().is_empty() && assignment(l).is_none())
    {
        return Err(format!(
            "line {no}: text before the first ::send is sent nowhere; only NAME=value lines may come first"
        ));
    }
    let targets = |args: &[String], no: usize| -> Result<Targets, String> {
        if args.iter().any(|a| a == "all") {
            if args.len() > 1 {
                return Err(format!("line {no}: all can't be combined with names"));
            }
            return Ok(Targets::All);
        }
        args.iter()
            .map(|name| {
                if harnesses.contains(&name.as_str()) {
                    Ok(name.clone())
                } else {
                    Err(format!(
                        "line {no}: unknown target {name}; use all or one of: {}",
                        harnesses.join(", ")
                    ))
                }
            })
            .collect::<Result<_, _>>()
            .map(Targets::Named)
    };

    let mut steps: Vec<Step> = Vec::new();
    let mut last_send: Option<String> = None;
    let mut i = first;
    while i < lines.len() {
        let (no, line) = lines[i];
        let name = command_name(line).expect("the loop stops at command lines");
        let mut end = i + 1;
        while end < lines.len() && !is_command(lines[end].1) {
            end += 1;
        }
        let body_lines = &lines[i + 1..end];
        i = end;
        let first_text = body_lines.iter().find(|(_, l)| !l.trim().is_empty());
        let mut body = text_lines(body_lines);
        while body.first().is_some_and(|l| l.trim().is_empty()) {
            body.remove(0);
        }
        while body.last().is_some_and(|l| l.trim().is_empty()) {
            body.pop();
        }
        if name != "send"
            && let Some((at, _)) = first_text
        {
            return Err(format!(
                "line {at}: this text follows ::{name}, so it is sent nowhere; put a ::send above it"
            ));
        }
        let rest = line.trim_start()["::".len() + name.len()..].trim();
        let args = split_args(rest).map_err(|e| format!("line {no}: {e}"))?;
        let vars = || {
            let mut text = header.join("\n");
            if let Some(send) = &last_send {
                text.push('\n');
                text.push_str(send);
            }
            text
        };
        let command = match name {
            "send" => {
                if args.is_empty() {
                    return Err(format!("line {no}: ::send needs a target, e.g. ::send all"));
                }
                let targets = targets(&args, no)?;
                if body.is_empty() {
                    return Err(format!(
                        "line {no}: ::send {} has no prompt text",
                        args.join(" ")
                    ));
                }
                if is_variables_only(&body) {
                    return Err(format!(
                        "line {no}: ::send {} sends only variables, no instruction",
                        args.join(" ")
                    ));
                }
                let text = header
                    .iter()
                    .chain(&body)
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                last_send = Some(body.join("\n"));
                Command::Send { targets, text }
            }
            "wait-model-files" => {
                let Some((path, names)) = args.split_first() else {
                    return Err(format!("line {no}: ::wait-model-files needs a path"));
                };
                let targets = if names.is_empty() {
                    if last_send.is_none() {
                        return Err(format!(
                            "line {no}: ::wait-model-files without targets needs a ::send above it"
                        ));
                    }
                    None
                } else {
                    Some(targets(names, no)?)
                };
                let vars = vars();
                if expand_vars(path, &variables(&vars, Some("MODEL"))).is_none() {
                    return Err(format!(
                        "line {no}: {path} uses a variable that is not set above"
                    ));
                }
                Command::WaitModelFiles {
                    path: path.clone(),
                    targets,
                    vars,
                }
            }
            "wait-file" => {
                let [path] = args.as_slice() else {
                    return Err(format!("line {no}: ::wait-file needs exactly one path"));
                };
                let vars = vars();
                if expand_vars(path, &variables(&vars, None)).is_none() {
                    let per_model = expand_vars(path, &variables(&vars, Some("MODEL"))).is_some();
                    let hint = if per_model {
                        "; use ::wait-model-files for one file per model"
                    } else {
                        ""
                    };
                    return Err(format!(
                        "line {no}: {path} uses a variable that is not set above{hint}"
                    ));
                }
                Command::WaitFile {
                    path: path.clone(),
                    vars,
                }
            }
            "wait-done" => {
                let targets = if args.is_empty() {
                    if last_send.is_none() {
                        return Err(format!(
                            "line {no}: ::wait-done without targets needs a ::send above it"
                        ));
                    }
                    None
                } else {
                    Some(targets(&args, no)?)
                };
                Command::WaitDone { targets }
            }
            _ => unreachable!("only known commands start a step"),
        };
        steps.push(Step {
            line: line.trim().to_string(),
            command,
        });
    }
    Ok(Some(steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 3] = ["opus-5.5", "gpt-6", "glm-5.3"];

    fn parse_ok(text: &str) -> Vec<Step> {
        parse(text, &NAMES).unwrap().expect("a script")
    }

    fn error(text: &str) -> String {
        parse(text, &NAMES).unwrap_err()
    }

    const REVIEW: &str = "TARGET=PS-1
REVIEW_DIR=/r/$TARGET

::# 1. Every model reviews
::send all
BASE=inikep/8.4
OUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md
Read instructions from review.md

::wait-model-files $OUTPUT_FILE

::send opus-5.5
REVIEW_COMBINED=$REVIEW_DIR/review-combined.md
Merge them.

::wait-file $REVIEW_COMBINED

::send opus-5.5
Fix all issues

::wait-done
";

    #[test]
    fn parses_the_review_script() {
        let steps = parse_ok(REVIEW);
        assert_eq!(steps.len(), 6);
        assert_eq!(
            steps[0].command,
            Command::Send {
                targets: Targets::All,
                text: "TARGET=PS-1\nREVIEW_DIR=/r/$TARGET\nBASE=inikep/8.4\nOUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md\nRead instructions from review.md".into()
            }
        );
        assert_eq!(steps[1].line, "::wait-model-files $OUTPUT_FILE");
        assert_eq!(
            steps[1].command.path(Some("GPT-6-high")).unwrap(),
            "/r/PS-1/review-GPT-6-high.md"
        );
        assert_eq!(
            steps[3].command.path(None).unwrap(),
            "/r/PS-1/review-combined.md"
        );
        assert_eq!(
            steps[4].command,
            Command::Send {
                targets: Targets::Named(vec!["opus-5.5".into()]),
                text: "TARGET=PS-1\nREVIEW_DIR=/r/$TARGET\nFix all issues".into()
            }
        );
        assert_eq!(steps[5].command, Command::WaitDone { targets: None });
    }

    #[test]
    fn ordinary_prompts_are_not_scripts() {
        assert_eq!(parse("What is 2+2?", &NAMES), Ok(None));
        assert_eq!(
            parse("::std::move(x) is fine\nexplain it", &NAMES),
            Ok(None)
        );
        assert_eq!(parse("OUTPUT_FILE=/x.md\nReview it", &NAMES), Ok(None));
    }

    #[test]
    fn variables_only_is_an_error() {
        assert!(error("TARGET=PS-1\nREVIEW_DIR=/r/$TARGET").contains("only sets variables"));
        assert!(
            error("TARGET=PS-1\n::send all\nREVIEW_DIR=/r")
                .contains("line 2: ::send all sends only variables")
        );
        assert!(
            error("TARGET=PS-1\n::send all\n").contains("line 2: ::send all has no prompt text")
        );
    }

    #[test]
    fn commands_must_start_a_line() {
        assert!(
            error("TARGET=PS-1 ::send all REVIEW_DIR=/r")
                .contains("line 1: \"::send\" must start a line")
        );
        assert!(error("REVIEW_DIR=/r  ::send all").contains("must start a line"));
    }

    #[test]
    fn reports_mistakes_with_line_numbers() {
        assert!(error("Hello\n::send all\nhi").contains("line 1: text before the first ::send"));
        assert!(error("::send nobody\nhi").contains("line 1: unknown target nobody"));
        assert!(error("::send all gpt-6\nhi").contains("can't be combined"));
        assert!(
            error("::send all\nhi\n::wait-done\nmore text")
                .contains("line 4: this text follows ::wait-done")
        );
        assert!(error("::wait-done\n").contains("needs a ::send above it"));
        assert!(error("::send all\nhi\n::wait-file $NOPE").contains("$NOPE uses a variable"));
        assert!(
            error("::send all\nOUT=/x/$MODEL_ID\nhi\n::wait-file $OUT")
                .contains("use ::wait-model-files")
        );
        assert!(error("::send all\nhi\n::wait-files x").contains("unknown command ::wait-files"));
        assert!(error("::send all \"x\nhi").contains("unterminated quote"));
    }

    #[test]
    fn explicit_targets_and_quoted_paths() {
        let steps = parse_ok(
            "::send gpt-6 glm-5.3\nhi\n::wait-model-files \"/a b/$MODEL_ID.md\" gpt-6\n::wait-done glm-5.3",
        );
        assert_eq!(
            steps[0].command,
            Command::Send {
                targets: Targets::Named(vec!["gpt-6".into(), "glm-5.3".into()]),
                text: "hi".into()
            }
        );
        assert_eq!(steps[1].command.path(Some("M")).unwrap(), "/a b/M.md");
        assert!(
            matches!(&steps[1].command, Command::WaitModelFiles { targets: Some(Targets::Named(n)), .. } if n == &["gpt-6"])
        );
    }
}
