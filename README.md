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
> alongside the sidebar's, and (pass 5b) git badges on window rows from the
> daemon, and (pass 6) a read-only sidebar on `prefix e`/`E`. The repos
> view comes in a later milestone; see
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
# set -g @home-preview-history 2000   # scrollback lines the preview captures
# set -g @home-sidebar-keys 'e E'     # sidebar: window, session ('' = none)
# set -g @home-sidebar-width 32       # columns, or a percentage (20%)
# set -g @home-sidebar-side left      # or right
# set -g @home-sidebar-auto off       # on: a sidebar in every new window
# set -g @home-sidebar-exclude ''     # sessions auto-create skips (space-separated)
```

The plugin finds tmux by absolute path (the running server's own binary,
then `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, then `PATH`), so it
works from a login with a minimal `PATH`.

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

Loading the plugin also starts the daemon, one per tmux server, 10 seconds
later (`TMUX_HOME_DAEMON_DELAY`); it exits with the server. The delay is for
tmux-continuum, which skips its auto-restore if, as the server starts, it
sees any process whose command line starts with `tmux`; tmux-home also runs
tmux by its absolute path, so its commands never look like that. A popup or
sidebar opened sooner starts the daemon itself. The popup subscribes to it for live updates. If the daemon
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
| type | filter (session, index, name, command, path, agent kind, git branch; agent prompts word by word) and tokens, below |
| `↑` `↓` `^p` `^n` `^k` `^j` | move (wraps) |
| `PgUp` `PgDn` | page |
| `S-↑` `S-↓` | scroll the preview a line (the list stays put) |
| `S-PgUp` `S-PgDn` | scroll the preview half a page |
| mouse wheel | scroll the preview, 3 lines a notch, wherever the pointer is; never the list |
| click | select the row clicked |
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

### Preview

The preview shows the selected window's pane in its own colours and
attributes (bold, dim, italic, underline, reverse), captured with
`capture-pane -e` together with up to `@home-preview-history` lines of its
scrollback (default 2000; `0` captures only the visible screen). It opens at
the latest output; the wheel, `S-↑`/`S-↓` and `S-PgUp`/`S-PgDn` scroll back
through the history, a `↑n` tag in its top-right corner says how far, and
moving the selection returns to the latest output. Lines are clipped at the
preview's edge, not wrapped, so one pane line is one preview row and a TUI's
columns stay lined up. The selected pane is captured again every second, and
only once the selection has rested on it for 60 ms, so holding `↓` doesn't
capture every pane passed; a 2000-line capture and parse takes about 16 ms.

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

### Git badges

Each window row ends in a compact badge for the git repo its pane is in
(an agent window: the repo, or linked worktree, its lead agent works in, in
place of the hook's `(wt) <branch>`), e.g. `main +!? ⇡2 ⇣1 $1 ⚠2 ↻`. The
preview of a window in a repo gets a git card spelling it out: branch →
upstream, ahead/behind, changes, operation in progress, stashes, stray
branches by name, worktree (linked, main, locked…) and the default branch.

| Symbol | Meaning |
| --- | --- |
| `main` | branch (the short commit when detached) |
| `(wt)` | a linked worktree (`git worktree add`) |
| `+` `!` `?` | staged, modified, untracked files |
| `✘` | conflicts |
| `⇡n` `⇣n` | commits ahead of / behind the upstream |
| `\|` | in sync with the upstream |
| `$n` | stashes |
| `⚠n` | stray branches: commits on no remote, no upstream (or a gone one), not merged into the default branch; in a repo with no remote, every branch other than the default one that isn't merged into it |
| `↻` | merge, rebase, cherry-pick, revert or bisect in progress |
| `⊟` `⊞` `⊘` `⚑` | worktree prunable, locked, detached, branch/path mismatch |
| `⊗` | limited: the repo's config names a command under a key that can't be switched off; only HEAD is shown, no status runs |
| `~` | stale: the last check timed out or failed (the values are older) |

`F1` shows the same legend. The symbols follow worktrunk's `wt list`;
`↑↓` are reserved for "vs default branch" (later).

The daemon computes badges in a task of its own, off the tmux poll's path,
keyed by repo root (windows in one repo share one status). A repo is
re-checked when a window's directory changes, when one of its windows gains
focus, when git itself changes it (the index, HEAD, reflogs, packed refs,
`FETCH_HEAD`: watched by `stat` once a second), and otherwise every
`max(10 s, 20 × its last status time)`. Each check is fast-first: the branch
from `.git/HEAD` (no git process), then one `git status`, then three
small reads (`for-each-ref`, a few config keys, `worktree list`) whose
result is memoised — unpushed counts, stray detection and the default
branch are recomputed only when a ref or that config changed.
At most 4 git processes run at once, each with a 10 s timeout
(`TMUX_HOME_GIT_TIMEOUT_MS` overrides it); badges reach the popup at most
once a second. Git runs read-only and runs nothing a repo or your config
names: `--no-optional-locks`, no fetch or any transport, no config
written, fsmonitor off (no hook, no `fsmonitor--daemon`), no hooks, no
clean/process filters, textconv or external diff (every such key in the
repo's config, whatever its name, is overridden to nothing, and attributes
are read from the empty tree), inherited `GIT_DIR`-style variables removed,
`LC_ALL=C`, each call in its own process group, and never into a
submodule (`--ignore-submodules=all`: a submodule's own config and
attributes could name a filter), so **changes inside a submodule don't
show** — the superproject's badge counts it as unchanged. One consequence: a
stat-dirty LFS or `text=auto` file can show as modified (`!`) until your
own git refreshes the index.
Badges need the daemon: a popup reading tmux directly (`(direct)`) shows
none.

### Claude Code hooks

tmux-home records Claude Code's state itself with `tmux-home hook claude
<Event>`: it reads the hook's JSON on stdin, finds its pane from
`$TMUX_PANE` (and the server from `$TMUX`), and writes that pane's
`@home_*` options (`@home_agent`, `@home_status`, `@home_wait_reason`,
`@home_run_started`, `@home_prompt`, …). It talks to no daemon (the daemon's
poll picks the options up), and it always exits 0, silently and in about
10 ms (release build, p50 of 100 runs; p95 11.7 ms): a problem (no
`$TMUX_PANE`, tmux unreachable, bad JSON) is a no-op, noted in
`<state root>/<server key>/hook.log` (rotated to `hook.log.1` at 64 KiB).

Wire it in Claude Code's settings (`~/.claude/settings.json` or a
project's `.claude/settings.json`), merged into any `hooks` you already
have. The event names are Claude Code's, and the command's event must
match the key it sits under. Each command ends in `>/dev/null 2>&1 ||
true` and has a 5-second timeout, so a missing or out-of-date binary can
never get in Claude's way: Claude Code treats a hook's exit 2 as "block"
(an unknown subcommand exits 2, which would block every prompt and keep
every Stop going), and a UserPromptSubmit hook's stdout becomes context.

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SessionStart >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude UserPromptSubmit >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude Stop >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "StopFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude StopFailure >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude Notification >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "PermissionDenied": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude PermissionDenied >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SessionEnd >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "SubagentStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SubagentStart >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "SubagentStop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude SubagentStop >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude PostToolUse >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ],
    "PostToolUseFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.tmux/plugins/tmux-home/target/release/tmux-home hook claude PostToolUseFailure >/dev/null 2>&1 || true",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

Claude Code fires no hook when you answer a permission prompt (or an MCP
elicitation): `PermissionRequest` fires as the dialog opens and
`Notification permission_prompt` six seconds later, but nothing on the
answer. The first sign is the approved tool's `PostToolUse` (or
`PostToolUseFailure`) as it finishes, so those two take a waiting pane
back to `running` (attention and reason cleared, run time kept); without
them the pane would read `waiting`, pinned under NEEDS YOU, until the
turn's Stop. They have no `matcher`, since any tool can ask for
permission. They run on every tool call, so they take a fast path: stdin
is drained unparsed, the pane's state is read in one tmux call, and on a
pane that isn't waiting on a prompt nothing else happens (about 6 ms,
p50 of 100 release runs with a 256 KiB tool output on stdin; p95 6.7 ms;
the transition itself 10 ms, p95 10.9 ms). The wait is the pane's, not
one agent's: Claude Code's notification doesn't say whether the parent
or a subagent asked, so any tool use on the pane, a subagent's included,
ends it, and a sibling finishing a tool while another agent's dialog is
still open ends it early. No `Task*` hooks. tmux-home's options
live in their own namespace, so these hooks run side by side with
tmux-agent-sidebar's. For a pane with both, tmux-home's win while they are
current (every write stamps `@home_updated`); if they go quiet and the
sidebar's are clearly newer, or name another session, the sidebar's are
shown. Panes without `@home_*` fall back to the sidebar's `@pane_*`.
Background work comes from Stop's `background_tasks` (and, alongside the
sidebar, its `@pane_bg_cmd`): a Stop with any still running reads
`◎ background`, with the first shell's command.

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

## Sidebar

`prefix e` toggles a sidebar in the current window; `prefix E` toggles one
in every window of the current session (on in every window lacking one if
any does, else off everywhere; other sessions are never touched). From a
shell or your own binding: `tmux-home sidebar-toggle [--session] [--window
<target>]`.

The sidebar is a narrow, read-only pane (`@home-sidebar-width`, default 32
columns, at the `@home-sidebar-side` edge, full height):

```
─ NEEDS YOU ───────────────────
 ◐ main:1 api
   permission
─ main ────────────────────────
 ◐ 1 api
▌  2 web  main ! ⇡1
 ● 3 editor  feat/x +?
 2s 7w · 1 waiting · 1 running
```

Windows that need you, server-wide, with the reason; then the windows of
its own session, each with its agent status icon, index, name and git
badge (whole pieces dropped as the width shrinks), the window it lives in
highlighted; a tally at the bottom (sessions, windows, live agents by
status; `○` while it reads tmux directly, without the daemon). It updates
live from the daemon. It never takes focus and never captures the mouse;
it ignores every key except `q`, which closes it. Sidebar panes are marked
`@home_role=sidebar` and left out of the popup's rows, `^x`'s close check
and `^t`'s reopen snapshot (as are tmux-agent-sidebar's `@pane_role`
panes).

With `@home-sidebar-auto on`, the daemon adds a sidebar to each new window,
except in sessions named in `@home-sidebar-exclude` (e.g. `scratch`). Each
window is decided once, when the daemon first sees it, so closing one with
`prefix e` sticks; windows that were there before the daemon started don't
count as new unless the server itself has just started. When a window's last
other pane exits, the daemon closes its sidebar, so no window is left holding
only a sidebar; a window with two tmux-home sidebars keeps the older one.

### tmux-resurrect

Add the sidebar to the processes resurrect restores:

```tmux
set -g @resurrect-processes '"~tmux-home sidebar"'
```

(append it to any list you already have). The sidebar pane runs
`/bin/sh -c '"$0" sidebar; exit' <path>/tmux-home`; resurrect's default
`ps` strategy saves the command lines of a pane process's children, so it
saves `<path>/tmux-home sidebar`, which `~tmux-home sidebar` (a substring
match) restores by typing it into the restored shell. The sidebar marks its
own pane at start, so nothing else is needed; `q` on a restored one closes
the whole pane. With auto-create on, a restored window can briefly get a
second, new sidebar before the restored one starts; the daemon then removes
the newer one.

## Tests

```sh
cargo test
```

The integration and end-to-end tests run against throwaway servers only
(`tmux -L th-test-*`, plus `th-outer-*`, whose pane runs a real client so the
popup's binding can be pressed and the popup read back with `capture-pane`),
with temporary state and runtime dirs, and start every window in the temp
dir. They never touch your tmux server. Git tests use throwaway repos and
run git with `GIT_CONFIG_GLOBAL=/dev/null` (no signing, no hooks).
[`docs/superpowers/notes/r1-parity.md`](docs/superpowers/notes/r1-parity.md)
maps each check of the retired bash suite to the test that replaced it.

## Requirements

tmux ≥ 3.3 (developed on 3.7c), git for the badges (developed on 2.50), and a
Rust toolchain to build (`rust-toolchain.toml`).

## Licence

MIT. The git layer is vendored from
[stray](https://github.com/tim-codes/stray) (MIT) and borrows ideas from
[worktrunk](https://github.com/max-sixty/worktrunk) (MIT OR Apache-2.0); see
[`NOTICE`](NOTICE).
