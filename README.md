# tmux-home

A full-window home screen for tmux: every session and window in one popup,
with rename, reorder, close and reopen, and you stay in it until you choose
where to go.

## Features

- **Popup home screen** (`prefix .`): one session per page (a tab strip
  across the top, `prefix )` / `prefix (` to page), every window with its
  panes beneath it in an aligned table, a type-first fuzzy filter and a
  live, coloured preview of the selected window's whole pane layout. Switch, rename inline, reorder, create, close (with a
  confirm when something is still running) and reopen closed windows, all
  without leaving the popup.
- **Coding-agent awareness**: Claude Code sessions show their status
  (running, waiting, idle, error, background) and run time on their pane's
  row; windows waiting on you are pinned under **NEEDS YOU** and counted on
  their session's tab, and `^g` jumps to the next one, on any page. Fed by tmux-home's own Claude Code hooks, or read-only
  from [tmux-agent-sidebar](https://github.com/hiroppy/tmux-agent-sidebar)'s
  pane options.
- **Git badges**: branch, dirty state, ahead/behind, stashes, stray branches
  and operations in progress for each window's repo, computed read-only in
  the background.
- **Sidebar** (`prefix e` / `prefix E`): a narrow, always-on view of the
  same state, pinned to a whole session (new windows included, kept across
  restarts) or toggled in one window; focus it to move through its windows
  and switch.
- **tmux-resurrect integration**: sidebars come back after a restore, and
  (opt-in) Claude Code sessions are resumed in the panes they ran in.

A small per-server daemon (one Rust binary) keeps the state; the popup and
sidebar subscribe to it, and fall back to reading tmux directly if it is
down.

## Why

- `choose-tree` can't rename and can't take custom keys.
- `find-window` asks for a search term in the status line before showing
  anything.
- The fzf-based pickers (tmux-fzf, tmux-sessionx, …) close the popup to
  rename a window, or only rename sessions.
- Agent sidebars show what Claude Code / Codex are doing, but not alongside
  window management.

tmux-home puts the overview, the management and the agent state in one
place.

## Requirements

- **tmux 3.3 or newer** (developed on 3.7c).
- **macOS**: developed and used daily on macOS. **Linux is untested**: the
  code has Linux paths (it reads `/proc`, and keeps the commands tmux
  runs through `/bin/sh` working under dash), but it has not been run
  there yet. Reports welcome.
- **A Rust toolchain** (`cargo`) to build it: the plugin builds itself
  from source on first load. `rust-toolchain.toml` pins the `stable`
  channel; the crate uses edition 2024.
- **git** for the badges (developed on 2.50); optional.
- Optional: [Claude Code](https://github.com/anthropics/claude-code) for
  agent status, and [tmux-resurrect](https://github.com/tmux-plugins/tmux-resurrect)
  / [tmux-continuum](https://github.com/tmux-plugins/tmux-continuum).

## Install

With [TPM](https://github.com/tmux-plugins/tpm), in `~/.tmux.conf`:

```tmux
set -g @plugin 'tim-codes/tmux-home'
```

Then `prefix I` to install. On its first load the plugin runs
`cargo build --release` in the background (output in
`~/.tmux/plugins/tmux-home/target/build.log`); until that finishes, the key
shows a `tmux-home: not built yet` message instead of the popup. No reload is
needed once the build is done. After a TPM update (`prefix U`), rebuild with
`cargo build --release` in the plugin directory.

Without TPM: clone the repo, run `cargo build --release` in it, and add
`run-shell ~/path/to/tmux-home/tmux-home.tmux` to your config.

## Quick start

1. Install as above and wait for the build (a minute or so).
2. `prefix .` opens the popup. Type to filter, `↑`/`↓` to move, `⏎` to
   switch, `F1` for help, `Esc` (or `prefix .` again) to close.
3. `prefix e` pins a sidebar to every window of the current session;
   `prefix E` toggles one in the current window.
4. For agent status, add the [Claude Code hooks](#claude-code-hooks).

## Configuration

All options are global (`set -g`) and read when the plugin loads or the
feature runs. Shown with their defaults:

```tmux
set -g @home-keys '.'               # prefix keys for the popup; '' binds nothing
set -g @home-preview-history 2000   # scrollback lines the preview captures (0: screen only)
set -g @home-preview-pct ''         # preview size, % of list+preview; unset: auto (M-j/M-k/M-= set it)
set -g @home-sidebar-keys 'e E'     # sidebar: pin the session, toggle this window ('' = none, '-' skips one)
set -g @home-sidebar-width 32       # columns, or a percentage (20%); until you resize one by hand
set -g @home-sidebar-side left      # or right
set -g @home-sidebar-auto off       # on: a sidebar in every new window
set -g @home-sidebar-exclude ''     # sessions auto-create skips (space-separated)
set -g @home-restore-agents off     # on: resume Claude Code sessions after a restore
set -g @home-restore-agents-delay 2 # seconds between resumed sessions
```

State (the closed-window stack, the pinned sessions, the sidebar width set
by hand, logs) lives in
`${XDG_STATE_HOME:-~/.local/state}/tmux-home/<server key>/`, one directory
per tmux server (the key is its socket path, so a restarted server finds
it again); `TMUX_HOME_STATE_DIR` overrides the root.

## How it runs

The plugin finds tmux by absolute path (the running server's own binary,
then `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, then `PATH`), so it
works from a login with a minimal `PATH`.

The popup and daemon are one Rust binary, `target/release/tmux-home`.
Only one background build runs at a time: reloading the config while it
runs starts no other. Without `cargo` on `PATH` no build is started and the
key's message says so.

Loading the plugin also starts the daemon, one per tmux server, 10 seconds
later (`TMUX_HOME_DAEMON_DELAY`); it exits with the server. The delay is for
tmux-continuum, which skips its auto-restore if, as the server starts, it
sees any process whose command line starts with `tmux`; tmux-home also runs
tmux by its absolute path, so its commands never look like that. A popup or
sidebar opened sooner starts the daemon itself. The popup subscribes to it for live updates. If the daemon
is down, the popup reads tmux itself (the header shows `(direct)`) and starts
a new daemon in the background. The daemon's errors go to
`<state root>/<server key>/daemon.log` (see [Configuration](#configuration)), moved to `daemon.log.1`
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

## Layout

```
  ▶dev ●1   scratch                                                       F1 help
>                                                                             4/4
   #     NM     CM      ST                  T    PA             GT
 ─ NEEDS YOU ─────────────────────────────────────────────────────────────────────
   0     fix
   └ .0         claude  ◐ waiting           12m  ~/d/app.fix-x  fix/x (wt) ! ≡
 ─ dev ───────────────────────────────────────────────────────────────────────────
   1     api
▌  ├ .0         claude  ● running           12m  ~/d/api        main ! ≡
   └ .1         fish                             ~/d/api        main ! ≡
 ▶ 3     shell
   └ .0         fish                             ~/d/a/src      main +!? ⇕⇡2⇣1
┌──────────────────────────────────────┬─────────────────────────────────────────┐
│.0 claude  ● running  12m  opus 5.5   │.1 fish                                  │
│main → origin/main ≡  !1              │main → origin/main ≡  !1                 │
│~/dev/api                             │~/dev/api                                │
│┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄│┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄│
│> add the tests                       │$ cargo test                             │
└──────────────────────────────────────┴─────────────────────────────────────────┘
```

- **Tab strip**: every session by name; the page shown is reversed and
  bold, the client's session marked `▶`, `●N` counts its windows that need
  you. The popup opens on the client's session (no client: the first).
  `prefix )` / `prefix (` page — tmux's own next/previous-session keys,
  deliberately a prefixed chord rather than a bare key; list navigation
  never pages.
- **Rows**: every window, and under it every pane but sidebars (single-pane
  windows too). A window row shows only its index and name (per-pane
  facts are on the pane rows); a pane row shows `├ .N` / `└ .N`, its title
  (blank when it is just the host name), command, agent state, run time,
  directory and git badge. Claude Code's executable is named after its
  version; a pane tagged as an agent (`@home_agent`, else the legacy
  `@pane_agent`) shows the agent's name there instead.
- **Columns** (`# NM CM ST T PA GT`, dim, under the filter) are sized
  per page from what they hold, so values line up. Narrow terminals drop
  `GT`, then `PA`, `T`, `CM`, `ST`; `#` and the name stay.
  `F1` lists the legend.

## Keys

| Key | Action |
| --- | --- |
| type | filter the page (index, name, command, path, title, agent kind, git branch; agent prompts word by word) and tokens, below. A window is shown when it or one of its panes matches, then with all its panes |
| `prefix )` `prefix (` | next / previous session page (wraps); the filter is kept and applies to the new page |
| `↑` `↓` `^p` `^n` `^k` `^j` | move over windows and panes (wraps within the page) |
| `PgUp` `PgDn` | page |
| `S-↑` `S-↓` | scroll the selected pane in the preview a line (on a pane row; the list stays put) |
| `S-PgUp` `S-PgDn` | scroll it half a page |
| mouse wheel | scroll the selected pane, 3 lines a notch, wherever the pointer is; never the list; nothing on a window row |
| click | select the row clicked |
| `←` `→` `Home` `End` `^a` `^e` `^u` `^w` | edit the filter |
| `⏎` | switch to the window and close: on a pane row also select that pane, on an agent window's row focus the agent's pane |
| `^g` | jump to the next window that needs you, paging to its session if needed |
| `Esc` | clear the filter; close when it is already empty |
| `prefix` + home key | close, from any mode: the key that opened the popup (`prefix .` by default) toggles it. Anything but a home key after the prefix acts as usual (`^a` still moves to the start); an unsaved rename or new-window name is dropped, a pending close is not done |
| `^r` | rename inline (`⏎` saves; `Esc` or an empty name cancels) |
| `M-r` | back to the automatic name |
| `^x` | close the window, staying in the popup (see below) |
| `^t` | reopen the last closed window |
| `M-↑` `M-↓` | move the window up / down within its session |
| `M-n` | new window after the selection, named inline (empty: automatic name) |
| `^o` | toggle the preview |
| `M-k` `M-j` | preview larger / smaller: the list/preview divider up / down 5% (the list and the preview keep 5 rows each). `M-k` shows a hidden preview; `M-j` never hides it. Remembered across opens |
| `M-=` | preview back to its automatic size |
| `F1` `^/` | help |

Window keys (`^r`, `M-r`, `^x`, `M-↑↓`, `M-n`) act on a pane row's window.
The preview keys work in the list only; the rename and new-window editors
ignore them.

### Preview

The preview sits below the list, always. It shows when the popup is at
least 30 rows tall, and is hidden below that until `^o` (which also hides a
shown one; `M-k` shows it too). Its size is automatic: the list gets the
page's lines and the preview the rest, recomputed on every
draw, so paging to a smaller session or filtering grows it; when that would
leave the preview under a third of the area (or 5 rows), the two split it in
half and the list scrolls. `M-k` / `M-j` move the divider 5% up / down from
wherever it is, keeping 5 rows each side; the size is then fixed and kept in
the server option `@home-preview-pct` for the next popup, until `M-=` unsets
it (automatic again). It draws the selected window's whole pane layout:
each pane's visible screen, captured with `capture-pane -e` in its own
colours and attributes (bold, dim, italic, underline, reverse), placed in
its rectangle scaled to the preview, inside a frame with tmux-style
separators (sidebar panes are left out); its top edge is the divider, right
under the list. Every pane's rectangle starts with a banner of up to three
dim lines and a dotted rule:

1. `.N` and its command; an agent's state (icon and word), run time and
   model (`opus 5.5`)
2. git: branch `→` upstream and sync (`≡`, `⇡n⇣n`, `gone`), changes
   (`+n` staged, `!n` modified, `?n` untracked, `=n` conflicts), `$n`
   stashes, an operation in progress (`↻ rebase`), `⊗ limited`, `~ stale`
3. where: the cwd (shortened when tight), `(wt) ← main worktree` for a
   linked worktree or the main one's linked count, `⚠n stray`,
   `⚑ mismatch`, `⊟ prunable`, `⊞ locked`, and the default branch when
   it isn't the one checked out

A pane outside a repo shows its cwd on the second line. Every banner of a
layout is the same height whatever its pane, with missing lines left blank;
when the panes are short they all shed the rule, then lines, before the
capture loses its last rows. A narrow pane drops whole pieces, the least
important first. Everything in a banner comes from the daemon's snapshot
(the hooks' agent state, the git task's status). On a window row no
pane is highlighted; on a pane row that pane is outlined in magenta and
its banner drawn brighter. Nothing else changes with the selection, so
moving between a window's panes never shifts the preview.

The selected pane is captured with up to `@home-preview-history` lines of
scrollback (default 2000; `0`: the visible screen only). It opens at the
latest output; the wheel, `S-↑`/`S-↓` and `S-PgUp`/`S-PgDn` scroll back
through the history, a `↑n` tag in its top-right corner says how far, and
moving the selection returns to the latest output. Lines are clipped at a
pane's edge, not wrapped, so a TUI's columns stay lined up. The window is
captured again every second, and only once the selection has rested on it
for 60 ms, so holding `↓` doesn't capture every window passed; a 2000-line
capture and parse takes about 16 ms.

Filter tokens combine with text: `@attn` (needs you), `@agent` (any agent
window, stale ones included), `@running`, `@waiting`, `@background`,
`@idle`, `@error` (either of the statuses given). For example
`@waiting api`. The filter applies to the session page shown; paging picks
the session.

### Agents

Agent state comes from tmux-home's own Claude Code hooks (`@home_*`, below)
or, for panes they haven't touched, from the `@pane_*` options
[tmux-agent-sidebar](https://github.com/hiroppy/tmux-agent-sidebar)'s hooks
publish (read-only: tmux-home never writes or clears those). An agent
pane's row shows status icon **and** word, the current run's elapsed time
and, as its command, the agent kind. Windows whose
agent is waiting, errored or has a pending notification are also pinned in
a **NEEDS YOU** group at the top of their session's page, and counted on its
tab (`●1`). In the preview an agent pane's banner shows its state, run time
and model; its prompt and reply are on screen in the pane itself.

An agent whose pane has moved on (the agent crashed and left its options
behind: Claude counts as alive only while its pane runs its versioned binary,
`claude` or `node`; other agents while it isn't a shell) is **stale**: dimmed with `(ended?)`, never pinned or
counted. With neither set of hooks, windows are listed as before, with no
agent column.

### Git badges

Each pane row ends in a compact badge for the git repo (or linked
worktree) its directory is in, e.g. `main +!? ⇕⇡2⇣1 $1 ⚠2 ↻`; the pane's
banner in the preview spells it out over two lines.

| Symbol | Meaning |
| --- | --- |
| `main` | branch (the short commit when detached) |
| `(wt)` | a linked worktree (`git worktree add`) |
| `+` `!` `?` | staged, modified, untracked files |
| `=` | conflicts |
| `⇡n` `⇣n` | commits ahead of / behind the upstream |
| `⇕⇡n⇣n` | diverged: both ahead of and behind the upstream |
| `≡` | in sync with the upstream |
| `$n` | stashes |
| `⚠n` | stray branches: commits on no remote, no upstream (or a gone one), not merged into the default branch; in a repo with no remote, every branch other than the default one that isn't merged into it |
| `↻` | merge, rebase, cherry-pick, revert or bisect in progress |
| `⊟` `⊞` `⊘` `⚑` | worktree prunable, locked, detached, branch/path mismatch |
| `⊗` | limited: the repo's config names a command under a key that can't be switched off; only HEAD is shown, no status runs |
| `~` | stale: the last check timed out or failed (the values are older) |

`F1` shows the same legend. Where a meaning overlaps starship's
`git_status`, the symbol is starship's (`+ ! ? = $ ⇡ ⇣ ⇕`, and `≡` from its
common preset), so a prompt and the badge read alike; the rest are
tmux-home's own. `↑↓` are reserved for "vs default branch" (later).

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
have. The snippet assumes TPM's default plugin directory
(`~/.tmux/plugins/tmux-home`); adjust the path if yours differs. Claude
Code is the only agent with hooks built in; other agents (Codex, …) show up
only through tmux-agent-sidebar's pane options, if you run it. The event names are Claude Code's, and the command's event must
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

`prefix e` pins the current session's sidebars: every window of it gets
one, and so does every window made in it from then on (at once, as the
daemon sees it). `prefix e` again unpins it and removes all its sidebars.
`prefix E` toggles the sidebar in the current window only. Other sessions
are never touched. From a shell or your own binding: `tmux-home
sidebar-toggle [--pin] [--window <target>]` (`--session` is the old name of
`--pin`).

In a pinned session, a sidebar closed by hand (`prefix E`, `q`, or killed
any other way) stays closed in that window — it is marked
`@home_sidebar_off` — until the session is pinned again, which gives every
window one. A window linked into several sessions has a sidebar while any
of them is pinned: unpinning one session leaves the sidebar of a window
another pinned session holds.

Pins are kept by session name, in the session option `@home_sidebar_pin`
and in the state dir's `pins` file (one name per line). After a restart,
once the restore is done (the same point auto-create waits for, below), the
daemon pins the sessions named there again, and gives their windows that
lack one a sidebar (none is added while a restore may be running). A pinned
session's rename is written to the file; one killed while the server lives
on is dropped from it. (The per-window "closed by hand" marks don't survive
a server restart: tmux-resurrect doesn't save window options.)

The sidebar is a narrow pane, full height at the `@home-sidebar-side` edge,
32 columns wide or as `@home-sidebar-width` says — until you resize one by
hand (mouse drag, `resize-pane`). Once its width has held for a moment
(0.7 s, so a drag isn't fought), it becomes the width of every sidebar, as a
percentage of the window: the others are resized to it, new ones are made
at it, and a window that changes size gets its sidebar set back to it. The
percentage is stored in the state dir's `sidebar-width` (`25%`) and wins
over `@home-sidebar-width` from then on; delete the file to go back to the
option. A change of one column, a resize of the whole window, and the space
a closing pane hands the sidebar don't count as resizing it by hand (the
last two set it back to the width), nor does a layout applied over it
(`select-layout`: no longer full height at the edge, or over half the
window):

```
─ NEEDS YOU ───────────────────
 ◐ main:1 api
   permission
─ main ────────────────────────
 ◐ 1 api waiting
▌  2 web
     main ! ⇡1
 ● 3 editor running
     feat/x +? ⇣2 $1
 2s 7w · 1 waiting · 1 running
```

Windows that need you, server-wide, with the reason; then the windows of
its own session, the window it lives in highlighted as one band. Each window
is stacked, at every width: the first line is the agent status icon, the
index (a fixed-width field, so names line up), the name, and the agent's
state word (`running`, `waiting`, ...) at the end; when the line is too
narrow the name is clipped with `…` first and the word stays visible (only
if not even a one-cell name fits beside it does the word drop to the second
line, ahead of the git pieces). The git badge's pieces follow below,
indented to the name. What doesn't fit on a line wraps, whole pieces at a time, onto
further lines with the same indent (a single piece wider than the line is
clipped), and a window with no agent and no repo is one line. A tally sits at the bottom (sessions, windows,
live agents by status; `○` while it reads tmux directly, without the
daemon). It updates live from the daemon. It never takes focus by itself
and never captures the mouse.

Move into it (`prefix ←`/`→`, or however you change panes) and it takes the
popup's movement keys:

| Key | Action |
|---|---|
| `↑` / `↓`, `^p` / `^n`, `k` / `j` | Move the selection (it starts on the sidebar's own window) over NEEDS YOU and the session's windows |
| `⏎` | Switch to the selected window; the sidebar's window gets its last pane back, so returning there doesn't land in the sidebar |
| `Esc` | Hand the focus back to the window's last pane |
| `q` | Close the sidebar |

Sidebar panes are marked `@home_role=sidebar` and left out of the popup's
rows, `^x`'s close check and `^t`'s reopen snapshot (as are
tmux-agent-sidebar's `@pane_role` panes).

With `@home-sidebar-auto on`, the daemon adds a sidebar to each window
created from then on, except in sessions named in `@home-sidebar-exclude`
(e.g. `scratch`). Each window is decided once, when the daemon first sees
it, so closing one with `prefix E` sticks. Auto-create starts only once the
server is past its start-up: at least 30 seconds old, its windows and panes
unchanged for 5 seconds, and, with `@continuum-restore on`, the restore done
(see below; at most 60 seconds' wait). Windows that exist at that moment
never get one. (A restore creates windows and later selects panes and types
commands into them by index; a sidebar added in between would shift them.)
Pinned sessions don't wait for any of this (only for a restore to finish),
and `@home-sidebar-exclude` doesn't apply to them: pinning is explicit.
A sidebar first seen by the daemon (a restored one keeps its saved width) is
set to the sidebar width. When a window's last other pane exits, the
daemon closes its sidebar, so no window is left holding only a sidebar; a
window with two tmux-home sidebars keeps the older one.

A sidebar never starts the daemon in the server's first 15 seconds (the
plugin starts it at 10): until the daemon is up it reads tmux itself, shown
by `○`, then subscribes. `tmux-home sidebar` run by hand in a pane of yours
marks nothing, and `q` just exits it; the panes tmux-home makes run
`tmux-home sidebar --managed`.

### tmux-resurrect

Add the sidebar to the processes resurrect restores, and tell tmux-home
when a restore has finished:

```tmux
set -g @resurrect-processes '"~tmux-home sidebar"'
set -g @resurrect-hook-post-restore-all 'tmux set -g @home_restore_done 1'
```

(append the first to any list you already have). The sidebar pane runs
`/bin/sh -c '"$0" sidebar --managed; exit' <path>/tmux-home`; resurrect's
default `ps` strategy saves the command lines of a pane process's children,
so it saves `<path>/tmux-home sidebar --managed`, which `~tmux-home sidebar`
(a substring match) restores by typing it into the restored shell. The
sidebar then marks its own pane, so nothing else is needed; `q` on a
restored one closes the whole pane. Pinned sessions are pinned again after
the restore (see [Sidebar](#sidebar)); their restored sidebars count, so a
window gets a new one only if it came back without.

#### Resuming Claude Code sessions

Opt-in: after a reboot, resurrect restores every pane as a plain shell;
with this, a pane that was running a Claude Code session when resurrect
last saved comes back running `claude --resume <that session>`, in the
session's directory and account. It only resumes: it never starts a new
session, and never touches a pane that wasn't running one.

```tmux
set -g @home-restore-agents on
set -g @resurrect-hook-post-save-all '~/.tmux/plugins/tmux-home/target/release/tmux-home agents-snapshot'
set -g @resurrect-hook-post-restore-all '~/.tmux/plugins/tmux-home/target/release/tmux-home restore-agents; tmux set -g @home_restore_done 1'
```

(the second line replaces the post-restore hook above; `restore-agents`
returns at once). It needs tmux-home's Claude Code hooks (above) in every
account whose sessions should come back: SessionStart, UserPromptSubmit
and Stop record the session's id, transcript, directory and account
(`$CLAUDE_CONFIG_DIR`) in the pane's `@home_*` options.

- **Save.** After each resurrect save, `agents-snapshot` writes the panes
  running Claude at that moment, by `session:window.pane`, to
  `<state root>/<server key>/agents.json`. A session you exited (or that
  crashed) has left its pane by then, so it isn't saved.
- **Restore.** After a restore, once per tmux server, `restore-agents`
  takes each saved pane that is back at a shell prompt (fish, bash, zsh,
  sh, …) in the saved directory, and whose transcript still exists
  (Claude Code deletes old ones, `cleanupPeriodDays`), and types

  ```sh
  cd '<dir>' && env CLAUDE_CONFIG_DIR='<account>' claude --resume <id>
  ```

  (`env -u CLAUDE_CONFIG_DIR` for the default account) into it, one every
  `@home-restore-agents-delay` seconds (default 2) from a background
  process, so a dozen sessions don't start their MCP servers at once.
  `env` runs the `claude` on `PATH`, not a shell function of that name.
  Exiting claude leaves you at the shell. Every skip, and every resume, is
  logged to `restore.log` beside `agents.json`; a second restore on the
  same server (`prefix C-r`) types nothing.

Don't add claude to `@resurrect-processes` as well. A long idle session
may open at Claude Code's "resume from summary?" question instead of its
prompt; nothing is sent to the API until you answer it.

### Moving from tmux-agent-sidebar

Its keys and hooks override tmux-home's: remove its `@plugin` line, any
`bind E …` of your own that runs its toggle (e.g. a session-scoped
`tmux-sidebar-session` script), and any `after-new-window` /
`after-new-session` hooks that run `toggle --create-only`; those bound after
TPM win over the `e`/`E` tmux-home binds. Then pin the sessions you want
sidebars in with `prefix e`, or set `@home-sidebar-auto on` and
`@home-sidebar-exclude` for every new window.

Coming from an earlier tmux-home: `prefix e` and `prefix E` swapped roles.
`e` (the first of `@home-sidebar-keys`) used to toggle the window and now
pins the session; `E` (the second) used to toggle every window of the
session once and now toggles the window. A custom `@home-sidebar-keys`
keeps its keys with the new meanings, so swap them to keep the old feel.

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

## License

[MIT](LICENSE). The git layer is vendored from
[stray](https://github.com/tim-codes/stray) (MIT), the Claude Code hook
handling is adapted from
[tmux-agent-sidebar](https://github.com/hiroppy/tmux-agent-sidebar) (MIT),
and some ideas come from [worktrunk](https://github.com/max-sixty/worktrunk)
(MIT OR Apache-2.0); see [`NOTICE`](NOTICE).
