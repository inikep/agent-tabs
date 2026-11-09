# agent-tabs

Run several coding-agent CLIs side by side, one per tab, each in its own real
interactive UI: `claude`, `codex`, `agy` and `cursor-agent` by default. Switch
between tabs, type into one harness directly, or compose a prompt once and send
it to any subset of them, or to all of them.

Each tab is a pseudo-terminal running the unmodified CLI. Its output goes
through [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal),
which also answers the terminal queries the CLIs send (cursor position, colours,
size), and is drawn with [ratatui](https://ratatui.rs). Prompts, replies,
approvals, slash commands and history all stay exactly as each harness shows
them.

```
 1 claude ● │ 2 codex │ 3 agy │ 4 cursor ✗ │
╭──────────────────────────────────────────────╮
│  …the active harness's own UI…               │
╰──────────────────────────────────────────────╯
 claude  ✳ Fix the flaky test   Ctrl+\ a prompt all · Ctrl+\ ? help
```

## Build and run

```sh
cargo build --release
cp target/release/agent-tabs ~/.local/bin/

cd /path/to/repo
agent-tabs
agent-tabs --only claude,codex
agent-tabs --log-dir ~/agent-tabs-logs
agent-tabs --no-restore   # start without the last run's rounds and script
```

All harnesses start in the current directory, or in `--cwd DIR`.

### `--yolo`

`agent-tabs --yolo` (or `yolo = true` in the config) starts every harness in its
own "approve everything" mode, and shows a red `YOLO` badge in the tab bar:

| Harness | Added flag | Removed from its args |
|---|---|---|
| claude | `--dangerously-skip-permissions` | `--permission-mode …` |
| codex | `--dangerously-bypass-approvals-and-sandbox` | `--full-auto`, `--sandbox …`, `--ask-for-approval …` |
| agy | `--dangerously-skip-permissions` | |
| cursor-agent | `--yolo` | |
| opencode | `--auto` | |

Harnesses then run commands and edit files without asking, and codex also
drops its sandbox, so use it only in a disposable checkout or VM. Set
`yolo_args = [...]` on a harness to use different flags, or `yolo_args = []`
to keep that harness asking.

## Keys

Everything you type goes to the harness in the active tab. agent-tabs only
intercepts its prefix key, `Ctrl+\`, and `Alt+1`…`Alt+9`.

| Keys | Action |
|---|---|
| `Alt+1`…`Alt+9`, or `Ctrl+\` `1`…`9` | Switch to tab N |
| `Ctrl+\` `n` / `p` (or `Tab` / `Shift+Tab`, `→` / `←`) | Next / previous tab |
| `Ctrl+\` `a` | Compose a prompt for **all** harnesses |
| `Ctrl+\` `s` (or `Enter`) | Compose a prompt for the **current** harness |
| `Ctrl+\` `[` (or `PgUp`), `Shift+PgUp`, mouse wheel | Scroll back through a tab's history |
| `Ctrl+\` `r` | Restart the current harness |
| `Ctrl+\` `x` | Cancel prompts queued for the current harness |
| `Ctrl+\` `q`, or click `✕ exit` (top right) | Quit, stopping every harness. It asks first; press `y` or click `✕ exit` again |
| `Ctrl+\` `?` | Help |
| `Ctrl+\` `Ctrl+\` | Send a literal `Ctrl+\` to the harness |

In the prompt composer:

| Keys | Action |
|---|---|
| `Enter` | Send to every checked target |
| `Alt+Enter`, `Shift+Enter`, `Ctrl+J` | New line |
| `Alt+1`…`Alt+9`, or click `[x] name` in the title | Toggle target N (`Alt+0` toggles all) |
| `Ctrl+A` | Toggle all targets |
| `↑` / `↓` | Move between lines; past the first or last line, step through prompt history |
| `Ctrl+U` / `Ctrl+K` / `Ctrl+W` | Delete to line start / line end / previous word |
| `Esc` | Cancel |

A harness with `enabled = false` in the config is not started with
agent-tabs and has no tab. It is still listed, unchecked, in the prompt boxes
(the main tab's and `Ctrl+\` `a`). Check it there, by clicking `[ ] name` or
with `Alt+N` in the composer, and agent-tabs starts it and shows its tab right
away. From then on it is treated like any other harness. `--only` starts the
harnesses it names even when they are disabled.

```toml
[[harness]]
name = "grok-4.7"
command = "cursor-agent"
enabled = false
```

A composed prompt is sent to each target as a bracketed paste, so a multi-line
prompt arrives as one message. After `submit_delay_ms` agent-tabs presses
`Enter` for you. A prompt whose last word starts with `$` or `@` gets a
trailing space: with the cursor in such a word, codex (and claude, for `@`)
opens a popup that would take that `Enter`.

The first prompt agent-tabs sends to each harness starts with a line naming
its model, so the model doesn't have to guess what it is:

```text
MODEL_ID=Gemini-4-Argon-high
<your prompt>
```

The name is the one the main tab shows, without the provider and with
spaces replaced by `-`. It's added
once per harness session (a restarted tab gets it again), and left off until
the model is known. Text you type directly into a tab is never changed. Set
`model_id_prefix = false` to turn it off.

A prompt is only sent to a harness that is at its input box. Until then it
waits in a queue, and the tab shows `⧗`:

- **Starting:** the harness hasn't enabled bracketed paste yet. Prompts are
  never typed as individual keystrokes, because single keys are actions in some
  dialogs (`q` quits cursor-agent's trust dialog).
- **Dialog open:** the screen matches one of `dialog_patterns`, by default the
  folder-trust dialogs of claude, codex, agy and cursor-agent. Without this, the
  final `Enter` would answer the dialog; for claude that can mean "No, exit".

Answer the dialog in its tab and the queued prompt follows automatically. The
harness's tab shows `⚑` while its folder-trust dialog is open.

Tab markers: `●` the harness produced output in the last 1.5 s (it's working),
`⚑` it's waiting for you to trust the folder (`dialog_patterns`), `⧗` a prompt
is queued, `?` it's waiting for your answer to a permission prompt
(`input_patterns`), `✗` it exited, `!` it failed to start. Tabs of disabled
harnesses that have not been started are hidden.

Harnesses that run full-screen (claude, codex and agy at the time of writing)
handle their own scrolling, so `PgUp`/`PgDn` and the mouse wheel are passed
through to them. For inline harnesses such as cursor-agent, agent-tabs keeps up
to `scrollback` lines of history.

## Main tab

The first tab is the `main` tab (`Alt+1`), and agent-tabs starts on it,
showing what to do and every [command template](#command-templates) with
its description. That intro is also where `Ctrl+N` goes past the last
round, and `Ctrl+P` goes from it to the last round.
The harness tabs follow in config order (`Alt+2` is the first harness).
Whenever you send a prompt to two or more harnesses, agent-tabs starts a
*round*:

1. A harness counts as done once it has received the prompt, been quiet for
   `quiet_secs`, and isn't asking you anything: no line near the bottom of its
   screen matches `input_patterns` (permission prompts such as claude's "Do
   you want to proceed?" or agy's "Run this command?"). A harness that exits
   also counts as done. A harness whose screen matches `working_patterns` is
   still working in the background, even when quiet, and is not done yet. By
   default these are `still running` (claude's "1 shell, 1 monitor still
   running"), `esc to interrupt`, and agy's `○ Running (…)` step for a command
   it hasn't finished. If a round takes longer than `wait_timeout_secs`, or you
   press `Ctrl+R` before every harness is done, whatever is on screen is used
   and marked as unfinished. agent-tabs keeps watching those harnesses: when
   one finishes, its section is updated, and once every answer is complete a
   comparison that used unfinished answers runs again. A harness counted as
   done that prints more without getting another prompt is watched again the
   same way, and if its answer changed, the comparison runs again.
2. When a harness is done, agent-tabs captures its text: the screen, plus for
   inline harnesses the scrollback since the prompt was sent, up to
   `max_lines` lines. From that it takes the harness's final message: the
   text after its echo of your prompt and before its input box, minus tool
   steps (`● Bash(…)`, `• Ran …`, `⎿`/`└` output, `$ cmd`), thinking blocks and
   timing lines. That is claude's last `●` paragraph, codex's last `•` message,
   and agy's or cursor's closing text. It's shown straight away as that
   harness's section, rewrapped to the tab's width.
3. With `compare = true` under `[main]` (off by default; `[summary]`, the
   section's old name, still works), when every
   harness is done agent-tabs pipes the prompt and all captures to the
   summarizer, which writes a `comparison` section: consensus, differences,
   the best answer and concerns. Without it, a round is just the answers; a
   script's merge step does the comparing better.

The summarizer command is `claude --print` by default, run in a temporary
directory so it can't read your repo. It is called once per round, for the
comparison. The rest of this section describes `compare = true` where it
mentions the comparison.

Each section header names the harness, its model and the provider, e.g.
`muse-wonder · Muse Wonder 300K High (Cursor)`. The model is read from the
harness's own status line or banner when its answer is captured, and the label
is passed to the comparison too. The provider defaults to the harness (Claude
Code, Codex, Antigravity, Cursor, OpenCode). Per harness you can set
`model = "..."` and `provider = "..."`, or teach agent-tabs a new status line
with `model_patterns = ["regex with (capture groups)"]`; a group named
`provider` is used as the provider.

Each harness gets a section with its status (`working`, `waiting for your
answer`, `✓ done`) and its own answer, folded by default. A folded section
shows the answer's last paragraph, which is usually the conclusion. The
`comparison` section follows, unfolded. Click a section header to fold or
unfold it. The text has no border, so it copies cleanly. Answers keep the
colors and text styles the harness drew them in.

Click the `⟳` in a harness's header (or press `Ctrl+L` on the focused
section) to capture that harness's answer again right now and count it as
done, for when agent-tabs got it wrong. If the answer changed, the comparison
runs again. The `⟳` on the `comparison` header does the same as `Ctrl+R`.

While a script runs, the main tab's marker shows it: `▶` sending or waiting
for harnesses to finish, `⏸` waiting for files, then `✓` (finished) or `✗`
(cancelled) until you look at the tab. Otherwise the marker shows the latest
round's state: `⧗` waiting for answers, `●`
all answers are in and you haven't looked yet; with `compare = true`, `…`
comparing, `●` a new comparison you haven't looked at, `!` failed.

The main tab has its own prompt box at the bottom, always active: type a prompt
and press `Enter` to send it to every ticked harness. All are ticked at first,
except disabled harnesses that haven't been started;
click a `[x] name` label in the box's title to untick or tick one, and the
choice is kept for later prompts. `[x]` only means "send to this harness"; the
tab markers show whether a harness is working. You can then watch the round
progress and read the answers without leaving the tab. The box edits like the
composer (`Alt+Enter` for a new line, `↑`/`↓` for history, `Esc` to clear), so
the tab's own commands use control keys:

| Key (in the main tab) | Action |
|---|---|
| typing, `Enter` | Write a prompt and send it to all harnesses |
| `Ctrl+R` | Take every answer of the round being viewed as it is now (with `compare = true`: summarize now, or re-run the comparison). With no rounds yet, uses what every harness currently shows |
| `⟳` in a header, `Ctrl+L` | Capture that harness's answer again now (`Ctrl+L`: the focused section) |
| `Ctrl+P` / `Ctrl+N` | Previous / next round (past the last: the intro with the commands) |
| `Ctrl+S` | Skip the wait a script is on |
| `Ctrl+X` | Cancel the running script |
| `Ctrl+G` | Resume a script paused by a restart |
| `Tab` / `Shift+Tab` | Move focus between sections |
| left click on a header, `Ctrl+O` | Fold or unfold that section (`Ctrl+O`: the focused one) |
| `Ctrl+E` | Fold or unfold all sections |
| `PgUp` / `PgDn`, `Ctrl+↑` / `Ctrl+↓`, mouse wheel | Scroll the main tab |

A new broadcast sent before the previous round finished marks that round as
superseded; press `Ctrl+R` on it to summarize it anyway.

Full-screen harnesses only expose what's on their screen, so a very long answer
may be summarized from its last screenful.

### Report files

A prompt can ask each harness to write its answer to a file instead, with a
`NAME=value` line for one of `output_vars` (`OUTPUT_FILE` or
`REVIEW_COMBINED` by default):

```text
BASE=inikep/8.4
TARGET=PS-11934-8.4-H08
REVIEW_DIR=/data/sh/LLM/reviews/$TARGET
OUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md
Read instructions from /work/review/agent-tabs/commands/review.md; the values above replace its placeholders.
```

agent-tabs works out each harness's file the way a shell would: `$NAME` and
`${NAME}` are expanded from the lines above it and from the harness's
`MODEL_ID`, so gpt-6 gets
`/data/sh/LLM/reviews/PS-11934-8.4-H08/review-GPT-6-Astra-high.md`. A relative
path is taken from agent-tabs' working directory. If the path can't be worked
out (the model isn't known yet, or a variable isn't defined in the prompt),
that harness is followed by its screen as usual.

A harness with a report file is done only once it has written the file after
the prompt was sent, the file hasn't changed for `quiet_secs`, and the harness
itself is done by the usual rules. A report left over from an earlier run
doesn't count. Once the harness has been sent another prompt (a script's
next step, say), its screen belongs to that prompt, so the settled file alone
is enough. Until then its section says `finishing its report`, and the
unfolded section shows the path, with `(not written yet)` while it's missing.
When it's done, the section shows the file line for line, with headings in
bold and `severity: blocker` / `major` in red and yellow. Folded, it shows the
report's verdict and findings, e.g. `changes-requested · 3 findings (1 major,
2 minor)`. A report rewritten later is read again, and the comparison runs
again if it changed. `⟳` reads the file again right away. The comparison gets
the report files instead of the screens.

### Scripts

A prompt with `::` command lines runs as a script: a sequence of prompts to
different harnesses, each started when the previous step's files are written
or its harnesses are done. Type or paste it into either prompt box; the
targets checked in the box don't matter, because the script names its own.

```text
TARGET=PS-11934-8.4-H10
REVIEW_DIR=/data/sh/LLM/reviews/$TARGET

::# 1. Every checked model reviews the branch into its own file
::send all
BASE=inikep/8.4
OUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md
Read instructions from /work/review/agent-tabs/commands/review.md

::wait-model-files $OUTPUT_FILE

::# 2. Opus merges the reviews
::send opus-5.5
REVIEW_FILES=$REVIEW_DIR/review-*.md
REVIEW_COMBINED=$REVIEW_DIR/review-combined.md
Read instructions from /work/review/agent-tabs/commands/review-merge.md

::wait-file $REVIEW_COMBINED

::# 3. Opus fixes the issues
::send opus-5.5
Fix all issues in current branch in /work/mysql-server/work

::wait-done

::# 4. Every model checks the fixes
::send all
REVIEW_COMBINED=$REVIEW_DIR/review-combined.md
Check if all issues from $REVIEW_COMBINED were fixed in $TARGET
```

| Command | Meaning |
|---|---|
| `::send TARGET…` | Send the text up to the next command to the targets; their answers join the script's round in the main tab |
| `::wait-model-files PATH [TARGET…]` | Wait for one file per target, with `$MODEL_ID` in `PATH` set to each one's model |
| `::wait-file PATH` | Wait for one file |
| `::wait-done [TARGET…]` | Wait until the targets are done, by the same rules as a round |
| `::# text` | A comment; never sent |

- A target is a tab name, or `all` for every harness checked in the prompt
  box the script was sent from, so `/plan` with only one tab checked runs on
  that one alone. A disabled harness named in `::send` is started. Without targets, a wait is
  for the targets of the last `::send`. Quote an argument with spaces.
- The `NAME=value` lines before the first command are shared: they're put at
  the top of every prompt the script sends. Prompts are sent as written, with
  `$NAME` left for the model; agent-tabs expands `$NAME` and `${NAME}` only to
  find the files to wait for, from the shared lines and the last `::send`'s
  lines.
- A file counts once it is written after its `::send` and hasn't changed for
  `quiet_secs`. A model whose `MODEL_ID` isn't known yet is waited for until
  it is done instead. Waits have no timeout: `Ctrl+S` in the main tab skips
  the wait the script is on (for a model that never writes its file), and
  `Ctrl+X` cancels the script; prompts already sent keep running.
- A script is one round in the main tab. Its first `::send` starts the
  round, and each later one adds a section per target, after the earlier
  ones, so `/review` shows every model's review and then the merge. The
  sections are named by step, e.g. `opus-5.5 · step 2`, and each is taken
  from its own step's prompt and report file. A prompt you send while a
  script runs gets its own round and doesn't supersede the script's. The
  script's steps are listed above its round only (and on the intro while it
  runs): `✓` done, `●` the step it's on
  and what it waits for, `⤼` skipped, `✗` where it was cancelled.
- The whole script is checked when you press Enter, and nothing is sent if
  something is wrong; the error names the line and the text stays in the box.
  A command has to start its line; text before the first `::send` other than
  `NAME=value` lines, text after a wait, a `::send` with no text or only
  `NAME=value` lines, an unknown target and an undefined variable in a wait's
  path are all errors. So is any prompt, script or not, made only of
  `NAME=value` lines, since it gives the models nothing to do.
- One script runs at a time.

### Command templates

`/NAME ARGS…` in either prompt box sends the template `NAME.txt` from this
repository's [`commands/`](commands/), found through the path agent-tabs
was built from (`dir` under `[commands]` in the config overrides it). A
template is an ordinary prompt or a script whose values come from the
command's arguments and from the config. There are three so far,
[`commands/plan.txt`](commands/plan.txt) and
[`commands/implement.txt`](commands/implement.txt) (below), and
[`commands/review.txt`](commands/review.txt):

```text
::# /review BASE..TARGET [NOTES...]: every checked model reviews BASE..TARGET, the coordinator
::# merges the reviews. NOTES, e.g. "Don't build. extra/libkmip is already checked", go to every
::# model after the instructions. REVIEW_DIR comes from [commands.variables] in the config,
::# COORDINATOR from [commands] coordinator, and review.md and review-merge.md sit next to this file.
::#
::# Example: /review percona/8.4..PS-11925-8.4-bump_libkmip
::#
::# Example: /review percona/8.4..PS-11925-8.4-bump_libkmip Don't build. extra/libkmip is already checked
::args BASE..TARGET [NOTES...]

::# Every checked model reviews the branch into its own file
::send all
OUTPUT_FILE=$REVIEW_DIR/review-$MODEL_ID.md
Read instructions from $PROMPT_DIR/review.md
$NOTES
::wait-model-files $OUTPUT_FILE

::# The coordinator merges the reviews
::send $COORDINATOR
REVIEW_FILES=$REVIEW_DIR/review-*.md
REVIEW_COMBINED=$REVIEW_DIR/review-combined.md
Read instructions from $PROMPT_DIR/review-merge.md
$NOTES
::wait-file $REVIEW_COMBINED
```

```toml
[commands]
coordinator = "opus-5.5"   # the default

[commands.variables]
REVIEW_DIR = "/work/LLM-reviews/$TARGET"
```

`/review percona/8.4..PS-11925-8.4-bump_libkmip` then runs the template
with these lines on top, which the script shares with every prompt it
sends:

```text
BASE=percona/8.4
TARGET=PS-11925-8.4-bump_libkmip
REVIEW_DIR=/work/LLM-reviews/$TARGET
PROMPT_DIR=/work/review/agent-tabs/commands
```

The prompts the templates point to, `review.md`, `review-merge.md` and
`implement.md`, are in `commands/` too: `$PROMPT_DIR` is that directory
unless the config or an argument sets it.

Words after the range are notes for the models, put on the `$NOTES` lines
after the instructions, e.g. `/review percona/8.4..PS-1 Don't build.
extra/libkmip is already checked` (quotes around the notes are optional).
Without notes, those lines are left out.

- `::args` lists one word per argument. In a word, each variable name
  matches part of the argument and the rest is matched literally:
  `BASE..TARGET` splits `a..b` at its first `..`.
- A last word ending in `...` takes the rest of the arguments, so a
  value can be several words: `::args TARGET TASK...`. In brackets,
  `[NOTES...]`, it may be left out, and is empty then.
- A line that is only `$NAME`, for a variable from the arguments or the
  config, is replaced by its value, or by nothing when it's empty, and a
  variable used only that way isn't put on top.
- Arguments written as `NAME=value` set or override any variable, e.g.
  `/review percona/8.4..PS-1 REVIEW_DIR=/tmp/reviews`. An argument whose
  word is a plain name can be given that way too, in any order:
  `/implement PLAN=/x.md BASE_BRANCH=PS-1-trunk`.
- `$COORDINATOR` is the harness that coordinates the others, here the one
  that merges the reviews: `coordinator` under `[commands]`, `opus-5.5` by
  default, or `COORDINATOR=gpt-6` after `/review` for one run. Since the
  script takes harness names as written, agent-tabs fills in variables in
  `::send` and `::wait-done` lines itself.
- `[commands.variables]` come after the arguments, in the order written,
  so a value can use `$TARGET` or a variable above it. A template gets only
  those it uses, directly or through another variable, so `PLAN_DIR =
  "/work/LLM-plans/$TARGET"` doesn't get in the way of `/implement`, which
  has no `TARGET`.
- A `$NAME` that the template uses and nothing sets (not an argument, the
  config, a `NAME=value` line in the template, `MODEL_ID` or the
  environment) is an error, so a missing `REVIEW_DIR` isn't sent to the
  models as `$REVIEW_DIR`.
- The template is read each time you use it, so an edit, or a new file in
  `commands/`, takes effect right away without a rebuild. A mistake in a
  template, such as a `::send` to a harness your config doesn't have, is
  reported with the file and line when you use it, and nothing is sent.
- `/NAME` without arguments, when its template has an `::args` line, is
  sent to the harnesses as it is, so their own commands (claude's and
  codex's `/review`) still work. So is `/NAME` without a template.

`/plan TARGET TASK...` has every checked model plan the task in
`$PLAN_DIR` (`PLAN_DIR = "/work/LLM-plans/$TARGET"` under
`[commands.variables]`), then improve its plan from all plans, compare the
improved plans, improve its plan from the comparisons, and finally pick the
best plan as a base and improve it, each step into its own files:
`plan-`, `plan2-`, `compare-`, `plan3-` and `plan4-$MODEL_ID.md`.
`/plan PS-11940-merge-26.10 implement option B` sends, besides any other
`[commands.variables]`,

```text
TARGET=PS-11940-merge-26.10
TASK=implement option B
PLAN_DIR=/work/LLM-plans/$TARGET
OUTPUT_FILE=$PLAN_DIR/plan-$MODEL_ID.md
In $OUTPUT_FILE create a plan to $TASK
```

and so on.

`/implement BASE_BRANCH PLAN [NOTES...]` has every checked model create its
own branch `$BASE_BRANCH-$MODEL_ID` on top of `BASE_BRANCH` and implement the
plan file there, following [`commands/implement.md`](commands/implement.md).
That prompt takes the worktree, build dir, cmake options and MTR rules from
[`commands/review.md`](commands/review.md), with the worktree on the new
branch instead of detached, and asks for commits with an `Implemented by`
trailer and a summary (branch and SHA, build, tests, deviations from the
plan, what's not done) as the final answer. The round is done when every
model is done, by the usual rules. `/implement PS-11299-trunk
/work/LLM-plans/PS-11299-port-vector-to-26.07/plan5-Opus-5.5-high.md`, or the
same with `BASE_BRANCH=` and `PLAN=`, sends

```text
BASE_BRANCH=PS-11299-trunk
PLAN=/work/LLM-plans/PS-11299-port-vector-to-26.07/plan5-Opus-5.5-high.md
PROMPT_DIR=/work/review/agent-tabs/commands
Read instructions from $PROMPT_DIR/implement.md
```

### Restarting

agent-tabs saves its rounds and the script in
`~/.local/state/agent-tabs/<working directory>.json` (under
`$XDG_STATE_HOME` if set), at most once a second while something changes and
when it exits, and restores them when it is started again in the same
directory. `--no-restore` starts without them; the state is still saved from
then on.

The harnesses come back as new sessions, so what they were doing is gone:

- The main tab starts on its intro; `Ctrl+P` shows the last round.
- Answers already captured come back as they were, colors included. A report
  file a round was still waiting for is still watched, and fills its section
  when it's written. An answer that was neither captured nor written to a
  file says so.
- A script that was running comes back paused, and the main tab's marker
  shows `?`. Its step list says which harnesses hadn't finished the step it
  was on. `Ctrl+G` sends that step's prompt again to just those harnesses,
  whose sections in the script's round wait for the new answer (the others'
  files or answers still count), and carries on; `Ctrl+X`
  cancels it. A comparison that was running is marked failed; `Ctrl+R` runs
  it again.

## Selecting and copying text

Drag with the left mouse button in any tab to select text; it's highlighted,
and copied when you release the button. A plain click clears the selection; in
the main tab a click on a section header still folds it.

- In harness tabs, the selection comes from the emulated terminal, so lines
  the harness wrapped are joined back together.
- The text goes to the system clipboard and the X11 primary selection (paste
  with `Ctrl+Shift+V` or the middle button). It's also sent as an OSC 52
  escape sequence, so copying works over SSH and inside tmux (with
  `set -g set-clipboard on`) when the terminal supports it.
- On X11 without a clipboard manager, the copied text stays available only
  while agent-tabs is running.
- Typing into a tab clears its selection.

Set `system_clipboard = false` or `osc52 = false` to turn either target off.
Your terminal's own selection still works with `Shift`+drag.

## Configuration

Optional: `~/.config/agent-tabs/config.toml`, or pass `--config FILE`.
`agent-tabs --print-config` prints the effective configuration.

```toml
submit_delay_ms = 150
scrollback = 20000
mouse = true
model_id_prefix = true
dialog_patterns = [
  "(?i)quick safety check",
  "(?i)trust this folder\\?",
  "(?i)do you trust the contents",
  "(?i)workspace trust required",
]

input_patterns = [
  "(?i)do you want to proceed\\?",
  "(?i)requesting permission for",
  "(?i)run this command\\?",
  "(?i)would you like to (run|make|apply|allow)",
]

[main]
compare = false
command = "claude"
args = ["--print", "--model", "claude-opus-5-5"]
auto = true
quiet_secs = 5
wait_timeout_secs = 900
run_timeout_secs = 300
max_lines = 400
output_vars = ["OUTPUT_FILE", "REVIEW_COMBINED"]

[commands]
dir = "/path/to/agent-tabs/commands"
coordinator = "opus-5.5"

[commands.variables]
REVIEW_DIR = "/work/LLM-reviews/$TARGET"
PLAN_DIR = "/work/LLM-plans/$TARGET"

[[harness]]
name = "claude"
command = "claude"
args = ["--model", "claude-opus-5-5"]

[[harness]]
name = "codex"
command = "codex"
args = ["-c", "model_reasoning_effort=\"high\""]

[[harness]]
name = "agy"
command = "agy"
args = ["--model", "gemini-4-argon", "--effort", "high", "--mode", "auto"]

[[harness]]
name = "cursor"
command = "cursor-agent"
env = { EXAMPLE_VAR = "value" }
env_remove = ["CLAUDECODE"]
```

Each harness runs with `TERM=xterm-256color`, `COLORTERM=truecolor` and
`TERM_PROGRAM=agent-tabs`. Variables describing the outer terminal (`TMUX`,
`TERM_PROGRAM`, `KITTY_WINDOW_ID`, `WEZTERM_PANE`, …) are removed, because the
harness's real terminal is agent-tabs.

## Logs

With `--log-dir DIR`, agent-tabs writes:

- `DIR/<harness>.log`: the raw terminal output of each harness. `cat` it in a
  terminal of the same size to replay it.
- `DIR/prompts.log`: every prompt sent from the composer, with a timestamp and
  its targets.
- `DIR/summaries.log`: every comparison (with `compare = true`), with the prompt it compares.
- `DIR/captures/`: the screen text captured from each harness for each round,
  which is what its section and the comparison are built from.

## Notes

- Mouse capture is on by default so the wheel, clicks on tabs and text
  selection work. Set `mouse = false` to turn capture off.
- Colour queries (OSC 10/11) are answered with a dark-background palette.
- On quit or restart, each harness's process group receives `SIGHUP`, as when a
  terminal window closes. Whatever is still running after 1.5 s gets `SIGKILL`.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The tests spawn real `sh`/`bash` processes in pseudo-terminals.
