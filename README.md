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
| `Ctrl+\` `q` | Quit, stopping every harness |
| `Ctrl+\` `?` | Help |
| `Ctrl+\` `Ctrl+\` | Send a literal `Ctrl+\` to the harness |

In the prompt composer:

| Keys | Action |
|---|---|
| `Enter` | Send to every checked target |
| `Alt+Enter`, `Shift+Enter`, `Ctrl+J` | New line |
| `Alt+1`…`Alt+9` | Toggle target N (`Alt+0` toggles all) |
| `Ctrl+A` | Toggle all targets |
| `↑` / `↓` | Move between lines; past the first or last line, step through prompt history |
| `Ctrl+U` / `Ctrl+K` / `Ctrl+W` | Delete to line start / line end / previous word |
| `Esc` | Cancel |

A composed prompt is sent to each target as a bracketed paste, so a multi-line
prompt arrives as one message. After `submit_delay_ms` agent-tabs presses
`Enter` for you.

Tab markers: `●` the harness produced output in the last 1.5 s (it's working),
`✗` it exited, `!` it failed to start.

Harnesses that run full-screen (claude, codex and agy at the time of writing)
handle their own scrolling, so `PgUp`/`PgDn` and the mouse wheel are passed
through to them. For inline harnesses such as cursor-agent, agent-tabs keeps up
to `scrollback` lines of history.

## Configuration

Optional: `~/.config/agent-tabs/config.toml`, or pass `--config FILE`.
`agent-tabs --print-config` prints the effective configuration.

```toml
submit_delay_ms = 150
scrollback = 20000
mouse = true

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
args = ["--model", "gemini-4-argon", "--effort", "high"]

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

## Notes

- Mouse capture is on by default so the wheel and clicks on tabs work. Most
  terminals still select text with `Shift`+drag; set `mouse = false` to turn
  capture off.
- Colour queries (OSC 10/11) are answered with a dark-background palette.
- On quit or restart, each harness's process group receives `SIGHUP`, as when a
  terminal window closes. Whatever is still running after 1.5 s gets `SIGKILL`.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The tests spawn real `sh`/`bash` processes in pseudo-terminals.
