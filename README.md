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
(the summary tab's and `Ctrl+\` `a`). Check it there, by clicking `[ ] name` or
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
`Enter` for you.

The first prompt agent-tabs sends to each harness starts with a line naming
its model, so the model doesn't have to guess what it is:

```text
MODEL_ID=Gemini-4-Argon-high

<your prompt>
```

The name is the one the summary tab shows, without the provider and with
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

## Summary tab

The first tab is the `summary` tab (`Alt+1`), and agent-tabs starts on it.
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
3. When every harness is done, agent-tabs pipes the prompt and all captures to
   the summarizer, which writes the comparison: consensus, differences, the
   best answer and concerns.

The summarizer command is `claude --print` by default, run in a temporary
directory so it can't read your repo. It is called once per round, for the
comparison.

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

The tab marker shows the round's state: `⧗` waiting for answers, `…`
summarizing, `●` a new summary you haven't looked at, `!` failed.

The summary tab has its own prompt box at the bottom, always active: type a prompt
and press `Enter` to send it to every ticked harness. All are ticked at first,
except disabled harnesses that haven't been started;
click a `[x] name` label in the box's title to untick or tick one, and the
choice is kept for later prompts. `[x]` only means "send to this harness"; the
tab markers show whether a harness is working. You can then watch the round
progress and read the summary without leaving the tab. The box edits like the
composer (`Alt+Enter` for a new line, `↑`/`↓` for history, `Esc` to clear), so
the tab's own commands use control keys:

| Key (in the summary tab) | Action |
|---|---|
| typing, `Enter` | Write a prompt and send it to all harnesses |
| `Ctrl+R` | Summarize now, or re-run the round being viewed. With no rounds yet, summarizes what every harness currently shows |
| `⟳` in a header, `Ctrl+L` | Capture that harness's answer again now (`Ctrl+L`: the focused section) |
| `Ctrl+P` / `Ctrl+N` | Previous / next round |
| `Tab` / `Shift+Tab` | Move focus between sections |
| left click on a header, `Ctrl+O` | Fold or unfold that section (`Ctrl+O`: the focused one) |
| `Ctrl+E` | Fold or unfold all sections |
| `PgUp` / `PgDn`, `Ctrl+↑` / `Ctrl+↓`, mouse wheel | Scroll the summary |

A new broadcast sent before the previous round finished marks that round as
superseded; press `Ctrl+R` on it to summarize it anyway.

Full-screen harnesses only expose what's on their screen, so a very long answer
may be summarized from its last screenful.

## Selecting and copying text

Drag with the left mouse button in any tab to select text; it's highlighted,
and copied when you release the button. A plain click clears the selection; in
the summary tab a click on a section header still folds it.

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

[summary]
command = "claude"
args = ["--print", "--model", "claude-opus-5-5"]
auto = true
quiet_secs = 5
wait_timeout_secs = 900
run_timeout_secs = 300
max_lines = 400

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
- `DIR/summaries.log`: every summary, with the prompt it summarizes.
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
