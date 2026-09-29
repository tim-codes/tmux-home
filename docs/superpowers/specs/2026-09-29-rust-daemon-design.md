# tmux-home phase 2 — Rust daemon, agents, git, sidebar

Status: draft for review · 2026-09-29 · supersedes SPEC.md §7, §8, §10
("replacing the sidebar"), §12 and §13 where they conflict. The UX in
SPEC.md §3–§6 (type-first, never leave the popup, target by ID, key map)
still holds.

## 1. Intent

**What the user asked for**

- A Rust daemon, one per tmux server, that owns tmux-home's state; the popup
  and a new sidebar are thin clients of it.
- It replaces the tmux-agent-sidebar plugin, including a sidebar on
  `prefix e` / `prefix E`.
- Context it tracks: running coding agents (Claude for the MVP, behind an
  adapter layer so Codex / OpenCode / others are cheap to add), git worktrees
  (borrowing from worktrunk), and stray's repo-hygiene view.
- The popup is rewritten in Rust (ratatui); the bash/fzf version is the
  reference implementation until the Rust one reaches parity, then retired.
- The closed-window history survives tmux server restarts.
- The sidebar is a read-only monitor, never interactive.
- stray: git-hygiene badges on window rows *and* a full repos view, with
  keys to cycle leaner/fuller presentations.
- Worktrees are shown, never created or removed (that stays with `wt`).
- Bootstrap by taking the best source from the reference projects.

**Out of scope for phase 2:** status-line touchpoints; desktop notifications;
typing into or approving an agent from tmux-home (you switch to the pane
first); worktree create/remove; Codex/OpenCode adapters (the trait exists,
the implementations don't); mouse beyond what comes free.

**Success looks like:** tmux-agent-sidebar is uninstalled; `prefix .`
opens a popup at least as capable as today's bash one, with agent status
and git badges on every row, in under 100 ms; `prefix e` shows a live,
consistent monitor; nothing regresses when the daemon is not running.

## 2. Architecture

One Rust crate, `tmux-home` (lib + bin), one binary with subcommands:

| Subcommand | Role |
| --- | --- |
| `tmux-home daemon` | Long-lived; one per tmux server. Owns the model, pushes snapshots. |
| `tmux-home popup` | Full-window TUI, run inside `display-popup -E`. Client. |
| `tmux-home sidebar` | Narrow read-only TUI in its own pane. Client. |
| `tmux-home hook <agent> <event>` | Called by agent hooks (Claude Code). Short-lived. |
| `tmux-home reopen` / `query --json` / `sidebar-toggle [--session]` | CLI entry points for bindings and scripts. |

```
 tmux server ──control mode (%window-add, %window-renamed, subscriptions…)──┐
      ▲                                                                     ▼
      │ commands (switch, rename, kill…)                          ┌────────────────┐
      │                                                           │ tmux-home      │
 popup / sidebar ◀──── snapshot stream (unix socket, NDJSON) ─────│ daemon         │
      │                                                           │  model         │
      └──── requests (subscribe, reopen, rescan) ────────────────▶│  agents        │
                                                                  │  git / repos   │
 Claude Code hooks ─▶ tmux-home hook ─▶ socket event ────────────▶│  closed stack  │
                          └─▶ @home_* pane options (always)       └────────────────┘
```

Modules (each testable on its own, talking through plain data types):

- `tmux::` — `TmuxSource` trait with two implementations: `ControlMode`
  (a `tmux -C` client) and `Poll` (one `list-panes -a -F …` per tick). Plus
  `TmuxCmd` for writes (always by `@id`/`%id`/`$id`).
- `model::` — `Snapshot { sessions, windows, panes, agents, git, clients }`,
  pure functions deriving window status, NEEDS YOU, staleness, sort order.
- `agents::` — `AgentAdapter` trait + `claude` adapter.
- `git::` — vendored from stray (`git.rs`, `model.rs`, `scan.rs`,
  `ignore.rs`), adapted to be called per-repo from the daemon.
- `store::` — closed-window stack and UI prefs on disk.
- `ipc::` — socket path, framing, protocol types, version handshake.
- `ui::` — shared ratatui widgets (rows, badges, agent card); `popup` and
  `sidebar` are thin apps over them.

Runtime: tokio (control-mode reader, socket server, timers); git work on
`spawn_blocking`. Fuzzy matching: `nucleo-matcher`. TUI: ratatui +
crossterm.

## 3. The daemon

**Identity and lifecycle.** Keyed by the tmux server's `#{socket_path}`, so
`-L tmux-home-test` servers get their own daemon. Socket and lock at
`${XDG_RUNTIME_DIR:-$TMPDIR}/tmux-home-$UID/<sha1(socket_path)[..12]>.{sock,lock}`.

- Started by `tmux-home.tmux` (`run-shell -b`) and lazily by any client that
  can't connect. A `flock` on the lock file makes a second start a no-op.
- Exits when its control-mode connection closes (server gone) — no stale
  daemons after `kill-server`.
- Every client request carries the client's build version; on mismatch the
  daemon replies `restart`, exits, and the client starts the new binary. TPM
  updates therefore take effect on the next popup open.

**Watching tmux (approach A).** One control-mode client, attached with
`refresh-client -f no-output,ignore-size,read-only` and a
`refresh-client -B` subscription for the pane fields the model needs
(`pane_current_command`, `pane_current_path`, `pane_title`, `@home_*`).
Structural notifications (`%window-add/close/renamed`, `%sessions-changed`,
`%session-window-changed`, `%layout-change`, `%unlinked-window-*`) trigger a
targeted re-read. A full resync runs every 5 s as a backstop.

The control client is filtered out everywhere tmux-home reasons about
clients (it's marked via `client_flags` `control-mode`).

*Risk, validated first (R0):* control mode attaches to a session, so it can
change `session_attached`, interact with `detach-on-destroy`, and may not
report other sessions' window events. R0 measures this; if control mode
can't be made invisible and complete, `TmuxSource::Poll` (500 ms, change
detection by hash) becomes the default and nothing above the trait changes.

**Model and push.** The daemon keeps the current `Snapshot`, recomputes
derived fields on each change, debounces 30 ms, and pushes the **whole
snapshot** to subscribers (tens of windows — a few KB; no diffing until
measured to matter).

**Protocol.** Newline-delimited JSON over the unix socket:

```
→ {"v":"0.3.0","op":"subscribe","client":"popup","tty":"/dev/ttys004"}
← {"type":"snapshot","seq":41,"data":{…}}
→ {"op":"reopen"} | {"op":"rescan","scope":"repos"} | {"op":"agent_event",…}
← {"type":"ok"} | {"type":"error","msg":"…"} | {"type":"restart"}
```

Writes that are plain tmux commands (switch, rename, kill, swap) are issued
by the client directly — the daemon observes the result like any other
change. The daemon only handles operations that need its state (reopen,
closed-stack push, rescans).

## 4. Degraded mode (daemon down)

- **Popup:** connects with a 150 ms budget; on failure it builds a snapshot
  itself through `TmuxSource::Poll` (windows, panes, agent `@home_*` options)
  — everything except git badges and the repos view, which show
  `(daemon starting…)`. It kicks off a daemon start in the background.
- **Hooks:** always write `@home_*` pane options first, then try the socket
  (50 ms budget, errors ignored). Agent state therefore survives a daemon
  restart and is readable without it.
- **Sidebar:** shows `tmux-home daemon not running — restarting` and retries
  with backoff.
- **Close/reopen without daemon:** the popup writes the closed stack itself
  (same file, same store lock).

## 5. Agents

```rust
trait AgentAdapter {
    fn kind(&self) -> AgentKind;                           // Claude, Codex, …
    fn on_hook(&self, event: &str, payload: &Value) -> Vec<StateChange>;
    fn looks_alive(&self, pane: &Pane) -> bool;            // staleness check
}
```

Adding an agent = one module implementing this, plus its hook wiring. The
model, UI and storage only see `AgentState`:

```rust
struct AgentState {
    kind: AgentKind, status: Status,     // Running, Background, Waiting, Idle, Error
    wait_reason: Option<WaitReason>,     // Permission, Question, TeammateIdle, Error(String)
    run_started: Option<Timestamp>,      // current run only
    prompt: Option<String>, subagents: u32, bg_cmd: Option<String>,
    permission_mode: Option<String>, session_id: Option<String>,
}
```

**Claude adapter** — ported from tmux-agent-sidebar's hook handlers
(`src/cli/hook/handlers/*.rs`, `status_priority.rs`), keeping its precedence
(`running > permission > background > waiting > idle`) and the subagent guard
(a subagent's SessionEnd must not wipe its parent). Events: SessionStart,
UserPromptSubmit, Stop, StopFailure, Notification, PermissionDenied,
SessionEnd, SubagentStart, SubagentStop. Not PostToolUse / Task* — kept off
the tool-call path. Payload on stdin, pane from `$TMUX_PANE`.

**Storage:** pane options in tmux-home's own namespace (`@home_agent`,
`@home_status`, `@home_wait_reason`, `@home_run_started`, `@home_prompt`, …)
so the old and new hooks can run side by side during migration without
fighting over the same options.

**Staleness:** agent options on a pane whose `pane_current_command` is a
shell ⇒ stale: dimmed `(ended?)`, not in NEEDS YOU or tallies. The daemon
(unlike the bash version) clears stale `@home_*` options after 10 minutes,
since they are its own.

## 6. Git and repos

**Source:** stray's git layer, vendored (MIT, same author): porcelain-v2
status, unpushed commits against *all* remotes, no-upstream/gone branches,
stashes, `worktree list --porcelain`, the `.git` file/dir/bare classification.
`GIT_OPTIONAL_LOCKS=0` everywhere; never writes to a repo; never fetches.

**From worktrunk** we borrow vocabulary, not code: its status symbols and
worktree-path conventions, so badges read the same as `wt list`. Worktrunk's
library API is unpublished and unstable, and stray's layer already yields
the same facts in-process; `wt list --format=json` is not a dependency.

**Per window:** active pane's `pane_current_path` → repo root → a
`RepoStatus` shared by every window in that repo. Refreshed on cwd change,
on window focus, and every 10 s for repos that have a window; git runs off
the async runtime, at most 4 in parallel.

**Repos view:** stray's scan of `@home-repos-root` (default `~/dev`, depth
8, honouring `~/.strayignore`), run at daemon start, every 10 min, and on
`r`. Repos that have a tmux window are marked; `⏎` on one jumps to its
window if there is one (otherwise does nothing — no window creation in
phase 2).

**Badge** (compact form): branch, `(wt)` for a linked worktree, then
worktrunk-style symbols — `+` staged, `!` modified, `?` untracked, `↑n`
unpushed, `↓n` behind, `$` stash, `⚠` stray branches.

## 7. Popup (Rust)

Launched as today (`display-popup -E -B -w 100% -h 100%`, invoking client
passed in). Everything in SPEC.md §4–§6 applies, ported from the bash
version and its 143-check test suite, which is the parity checklist.

- **Views** (`Tab` cycles): **Windows** (default; grouped by session, NEEDS
  YOU pinned first) and **Repos** (stray-style tree, attention first).
- **Density** (`M-d` cycles, remembered per view): *compact* (name + status
  icon + badge), *normal* (today's columns), *full* (two-line rows: prompt /
  wait reason, worktree path, ahead/behind detail).
- **Keys** carried over: filter, `⏎`, `^r`, `M-r`, `^x` (+ confirm), `^t`,
  `^o`, `F1`, `Esc`. Added from SPEC M1–M2: `M-↑↓` reorder, `M-n` new window,
  `^g` next needing attention, filter tokens (`@attn`, `@agent`, `@running`,
  `@waiting`, `@idle`, `@error`, `s:<session>`).
- **Preview:** pane capture; for agent windows the agent card (SPEC §7).
- **Live:** redraws on every pushed snapshot; selection kept by ID; open
  inline editors are never disturbed (a vanished target closes the editor
  with a notice).

## 8. Sidebar

- `prefix e` → `tmux-home sidebar-toggle`: add/remove a sidebar pane in the
  current window. `prefix E` → `sidebar-toggle --session`: same for every
  window of the current session (on if any window lacks one, else off).
- Pane: `split-window -d -f -h -l <width>` (`@home-sidebar-width`, default
  32; `@home-sidebar-side`, default `left`), marked `@home_role=sidebar`,
  running `tmux-home sidebar`. `-d` so it never takes focus.
- **Read-only:** it renders and ignores input (a click that focuses it does
  nothing harmful; `q` closes that sidebar). It is excluded from lists,
  counts, close checks and reopen snapshots.
- **Content:** NEEDS YOU across the server, then the current session's
  windows (status icon, name, compact badge), highlighting the window it
  lives in; a one-line server tally at the bottom. Same widgets as the
  popup's compact density.
- **Auto-create:** off by default; `@home-sidebar-auto on` adds one to new
  windows, except sessions listed in `@home-sidebar-exclude` (the user's
  config sets `scratch`). Implemented by the daemon reacting to
  `%window-add`, not by global tmux hooks.
- When the last non-sidebar pane in a window exits, the daemon closes the
  sidebar so no window is left holding only a monitor.
- *Risk:* tmux-resurrect restores sidebar panes as plain shells. The daemon
  records which windows had a sidebar (in the store) and, on start, replaces
  restored shells in those positions with fresh sidebars — validated in R4.

## 9. Persistence

`${XDG_STATE_HOME:-~/.local/state}/tmux-home/` (dir per `sha1(socket_path)`
so test servers never touch the real one; `TMUX_HOME_STATE_DIR` overrides):

- `closed.json` — 10-deep LIFO of closed-window snapshots (as in the bash
  M1, now JSON). Survives server restarts. Reopen into a session that no
  longer exists recreates it.
- `prefs.json` — last view, density per view.
- `sidebars.json` — windows with a sidebar (for the resurrect case).

Writes are atomic (write + rename) under a store lock (`state.lock`), separate from the daemon's instance lock, so a degraded-mode popup can write safely.

## 10. Install and build

- TPM plugin as now. `tmux-home.tmux` binds keys and starts the daemon; it
  uses `target/release/tmux-home` when present, and during the transition
  falls back to the bash `bin/tmux-home` for the popup.
- Built with `cargo build --release`: by dotfiles' `scripts/tmux-plugins`
  (as it does for tmux-agent-sidebar today) and by `tmux-home.tmux` on first
  load if the binary is missing (in the background, bash popup meanwhile).
- Bindings: `@home-keys` (default `.`), plus `e`/`E` for the sidebar
  (`@home-sidebar-keys`, default `e E`).

## 11. Migration from tmux-agent-sidebar (dotfiles PR, R5)

1. Add tmux-home's Claude hook entries to `files/claude/settings.shared.json`
   **alongside** the sidebar's; run `claude-sync`. Both run; no conflict
   (separate option namespaces).
2. Rebind `prefix e/E` to tmux-home; remove the two post-tpm auto-create
   hooks and `bind E`; set `@home-sidebar-auto on`,
   `@home-sidebar-exclude scratch`.
3. After every running Claude session has restarted: remove the sidebar's
   hook entries, the `@plugin` line, `@sidebar_*` options,
   `src/bin/tmux-sidebar-session`, the cargo step in `scripts/tmux-plugins`,
   and mentions in `deps-up`, `setup-macbook`, `CLAUDE.md`; run
   `scripts/tmux-plugins` to delete the plugin; kill leftover
   `@pane_role=sidebar` panes.

## 12. Testing

- **Unit:** model derivations, status precedence, staleness, filter tokens,
  badge rendering, closed-stack store, protocol round-trips, Claude adapter
  against recorded hook payloads in `tests/fixtures/claude/`.
- **Git:** stray's existing temp-repo integration tests, carried over.
- **Integration (real tmux):** each test starts `tmux -L th-test-<rand>`,
  a daemon for it, and drives it through the socket and `TmuxCmd`; asserts on
  snapshots. Includes control-mode invisibility (no `session_attached`
  change, no resize) and daemon exit on `kill-server`.
- **TUI:** ratatui `TestBackend` + `insta` snapshots per view × density;
  a handful of end-to-end runs in a real `display-popup` on the test server,
  read back with `capture-pane`, ported from the bash suite.
- The bash suite keeps running until the bash popup is deleted.

## 13. Milestones (each a reviewable PR on its own branch)

- **R0 — foundations + control-mode spike.** Crate, socket/lock/lifecycle,
  version handshake, `TmuxSource` with both implementations, `query --json`.
  Exit criteria: control client invisible to the user and complete across
  sessions — or `Poll` chosen as default, with the numbers.
- **R1 — popup parity.** Rust popup with every M0/M1 behaviour and the
  close/reopen store; `prefix .` switches to it; bash popup deleted.
- **R2 — agents.** Adapter trait, Claude adapter + `tmux-home hook`, rows,
  NEEDS YOU, staleness, agent card, `^g`, tokens.
- **R3 — git + repos.** Vendored stray layer, window badges, Repos view,
  density cycling, `M-↑↓`, `M-n`.
- **R4 — sidebar.** `e`/`E`, auto-create, exclude list, resurrect handling.
- **R5 — cutover.** Dotfiles PR per §11.

## 14. Attribution

Vendored code keeps its origin in a header comment and in `NOTICE`:
stray (MIT, tim-codes), tmux-agent-sidebar (hook handlers and status
precedence; licence checked and recorded before copying), worktrunk
(symbol vocabulary only; MIT OR Apache-2.0).

## 15. Decisions taken for you (say if any is wrong)

1. Git data comes from stray's vendored layer, not `wt list --format=json`.
2. Agent state is written to tmux pane options *and* sent to the daemon, so
   it survives daemon restarts and works in degraded mode.
3. New `@home_*` option namespace rather than reusing the sidebar's
   `@pane_*` names.
4. The daemon pushes whole snapshots, not diffs.
5. Sidebar defaults: left, 32 columns, auto-create off (your config turns it
   on with `scratch` excluded).
6. `prefix .` stays the popup key; `prefix f` stays tmux-fzf until you say.
