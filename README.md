# tmux-home

A full-window home screen for tmux: every session and window in one popup,
with rename, reorder, close and reopen, and you stay in it until you choose
where to go.

> **Status: R1.** A Rust popup (ratatui) on top of a per-server daemon:
> grouped window list, type-first fuzzy filter, preview, `⏎` switch, inline
> `^r` rename, `M-r` auto-name, `^x` close with an inline confirm, `^t`
> reopen, `M-↑`/`M-↓` reorder and `M-n` new window, plus read-only agent
> awareness (pass 4: status, NEEDS YOU, `^g`, filter tokens, agent card) from
> [tmux-agent-sidebar](https://github.com/hiroppy/tmux-agent-sidebar)'s pane
> options, and (pass 5a) from tmux-home's own Claude Code hooks, which run
> alongside the sidebar's. Git badges and the sidebar come in later milestones; see
> [`docs/superpowers/specs/2026-09-29-rust-daemon-design.md`](docs/superpowers/specs/2026-09-29-rust-daemon-design.md) §13.

## Why

- `choose-tree` can't rename and can't take custom keys.
- `find-window` asks for a search term in the status line before showing
  anything.
- The fzf-based pickers (tmux-fzf, tmux-sessionx, …) close the popup to
  rename a window, or only rename sessions.
- Agent sidebars show what Claude Code / Codex are doing, but not alongside
  window management.

tmux-home puts the overview, the management and (soon) the agent state in
one place.

## Install

With [TPM](https://github.com/tmux-plugins/tpm):

```tmux
set -g @plugin 'tim-codes/tmux-home'
# set -g @home-keys '.'   # prefix keys to bind (default); '' binds nothing
```

Or clone it and add `run-shell ~/path/to/tmux-home/tmux-home.tmux`.

## Build

The popup and daemon are one Rust binary, `target/release/tmux-home`:

```sh
cargo build --release
```

If the plugin loads before it is built, it starts that build in the
background, writing its output to `target/build.log`. Only one build runs at
a time: reloading the config while it runs starts no other. Until the build
finishes, the key shows a one-line `tmux-home: not built yet …` message
naming that log instead of the popup; without `cargo` on `PATH` no build is
started and the message says so. A build that finishes later is picked up on
the next key press, with no reload. Rebuild after pulling an update.

Loading the plugin also starts the daemon, one per tmux server; it exits
with the server. The popup subscribes to it for live updates. If the daemon
is down, the popup reads tmux itself (the header shows `(direct)`) and starts
a new daemon in the background. The daemon's errors go to
`<state root>/<server key>/daemon.log` (see below), moved to `daemon.log.1`
once it reaches 1 MB.

Each client tells the daemon its build (the version plus the git commit, and
a hash of the sources when the tree is dirty or there is no git), so after a
rebuild — even at the same version — the old daemon exits on the first
request and the client starts the new one.

`tmux-home status` prints `●` when the daemon answers and `○` when it
doesn't, for a status line. It never starts a daemon that is simply down,
so after the server's daemon dies the chip shows `○` until something (the
popup, `tmux-home query`) starts a new one. After a rebuild, the old
daemon's `Restart` reply makes `status` itself start the new build's daemon:
the chip shows `○` once, then `●` from its next refresh. Only the plugin's
own binary (`target/release/tmux-home`) does this, unless
`TMUX_HOME_STATUS_RESPAWN` says otherwise (`0` never, anything else always).

Don't point a development build at your live tmux server: each build
restarts the other's daemon on every request (one per server, one build at
a time). Use a throwaway server (`tmux -L scratch`) as the tests do.

## Keys

| Key | Action |
| --- | --- |
| type | filter (session, index, name, command, path, agent kind; agent prompts word by word) and tokens, below |
| `↑` `↓` `^p` `^n` `^k` `^j` | move (wraps) |
| `PgUp` `PgDn` | page |
| `←` `→` `Home` `End` `^a` `^e` `^u` `^w` | edit the filter |
| `⏎` | switch to the window and close (an agent window: also focus the agent's pane) |
| `^g` | jump to the next window that needs you (cycles through NEEDS YOU) |
| `Esc` | clear the filter; close when it is already empty |
| `^r` | rename inline (`⏎` saves; `Esc` or an empty name cancels) |
| `M-r` | back to the automatic name |
| `^x` | close the window, staying in the popup (see below) |
| `^t` | reopen the last closed window |
| `M-↑` `M-↓` | move the window up / down within its session |
| `M-n` | new window after the selection, named inline (empty: automatic name) |
| `^o` | toggle the preview |
| `F1` `^/` | help |

Filter tokens combine with text: `@attn` (needs you), `@agent` (any agent
window, stale ones included), `@running`, `@waiting`, `@background`,
`@idle`, `@error` (either of the statuses given), `s:<session>` (session name prefix, any
case). For example `@waiting s:main api`.

### Agents

Agent state comes from tmux-home's own Claude Code hooks (`@home_*`, below)
or, for panes they haven't touched, from the `@pane_*` options
[tmux-agent-sidebar](https://github.com/hiroppy/tmux-agent-sidebar)'s hooks
publish (read-only: tmux-home never writes or clears those). An agent window's row shows
status icon **and** word, the current run's elapsed time and the agent kind
(`● running  12m  claude ×2` for two agents), in place of command and path.
Windows whose agent is waiting, errored or has a pending notification are
also pinned in a **NEEDS YOU** group above the sessions, with the reason;
the header carries a tally (`agents: 1 waiting · 2 running`). The preview of
an agent window starts with an agent card: status, run time, wait reason,
subagents, background command, worktree, permission mode and the prompt (or
last reply), then the pane's last lines.

An agent whose pane has moved on (the agent crashed and left its options
behind: Claude counts as alive only while its pane runs its versioned binary,
`claude` or `node`; other agents while it isn't a shell) is **stale**: dimmed with `(ended?)`, never pinned or
counted. With neither set of hooks, windows are listed as before, with no
agent column.

### Claude Code hooks

tmux-home records Claude Code's state itself with `tmux-home hook claude
<Event>`: it reads the hook's JSON on stdin, finds its pane from
`$TMUX_PANE` (and the server from `$TMUX`), and writes that pane's
`@home_*` options (`@home_agent`, `@home_status`, `@home_wait_reason`,
`@home_run_started`, `@home_prompt`, …). It talks to no daemon (the daemon's
poll picks the options up), and it always exits 0, silently and in about
11 ms (release build, p50 of 100 runs; p95 12.5 ms): a problem (no
`$TMUX_PANE`, tmux unreachable, bad JSON) is a no-op, noted in
`<state root>/<server key>/hook.log` (rotated to `hook.log.1` at 64 KiB).

Wire it in Claude Code's settings (`~/.claude/settings.json` or a
project's `.claude/settings.json`), merged into any `hooks` you already
have. The event names are Claude Code's, and the command's last word must
match the key it sits under:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SessionStart"
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude UserPromptSubmit"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude Stop"
          }
        ]
      }
    ],
    "StopFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude StopFailure"
          }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude Notification"
          }
        ]
      }
    ],
    "PermissionDenied": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude PermissionDenied"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SessionEnd"
          }
        ]
      }
    ],
    "SubagentStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SubagentStart"
          }
        ]
      }
    ],
    "SubagentStop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SubagentStop"
          }
        ]
      }
    ]
  }
}
```

Only these nine events: none on the tool-call path (`PostToolUse`,
`Task*`), so the hook never slows a tool call down. tmux-home's options
live in their own namespace, so these hooks run side by side with
tmux-agent-sidebar's; for a pane with `@home_*` options they take
precedence, and panes without them fall back to the sidebar's `@pane_*`.
Background shells are only seen through the sidebar (its `PostToolUse`
hook sets `@pane_bg_cmd`): with it, a Stop while one is live reads
`◎ background`; without it, `○ idle`.

`^x` closes at once when every pane in the window is idle at a shell prompt
(bash, zsh, fish, sh, …; sidebar panes don't count). Otherwise it asks
inline, for example `close "api"? running: nvim, node (y/N)`, and only `y`
closes; any other key, `⏎` or `Esc` cancels and keeps the filter. A job
stopped with `^z` counts as running (`vim (stopped)`). A running or waiting
agent says so: `close "api"? agent still working — running: claude (y/N)`. A job running in the
background (`make &`) does not: the pane sits at its prompt, so `^x` closes it
without asking and the job dies with the window. (Shells and prompt helpers
keep their own background processes on the terminal, so tmux-home can't tell
your jobs from theirs reliably.) The session's last
window always asks (`session "x" will end`); if that is the session you are
in, tmux-home first moves you to another session so the popup isn't
detached. The last window on the server is never closed.

`^t` (or `tmux-home reopen` from a shell or your own binding) rebuilds the
most recently closed window, like reopening a browser tab, up to 10 back:
same session (recreated if it ended), same place, name, pane count, layout,
directories and active pane, but **fresh shells**: whatever was running in it
is gone. The popup stays open with the cursor on it. The stack lives in
`<state root>/<server key>/closed.json`, where the state root is
`${XDG_STATE_HOME:-~/.local/state}/tmux-home` (override with
`TMUX_HOME_STATE_DIR`). A `closed.json` that can't be read is renamed to
`closed.json.corrupt.<timestamp>` and the stack starts empty.

## Tests

```sh
cargo test
```

The integration and end-to-end tests run against throwaway servers only
(`tmux -L th-test-*`, plus `th-outer-*`, whose pane runs a real client so the
popup's binding can be pressed and the popup read back with `capture-pane`),
with temporary state and runtime dirs. They never touch your tmux server.
[`docs/superpowers/notes/r1-parity.md`](docs/superpowers/notes/r1-parity.md)
maps each check of the retired bash suite to the test that replaced it.

## Requirements

tmux ≥ 3.3 (developed on 3.7c) and a Rust toolchain to build
(`rust-toolchain.toml`).

## Licence

MIT
