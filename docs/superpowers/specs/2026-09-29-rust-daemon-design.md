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

*R0 decision (2026-10-01): `Poll` is the default* — the control client counts in `session_attached` (read by `tmux ls` and tmux-agent-sidebar), fires `client-attached` hooks and is briefly the default client, and no attach mode hides it; `--source control` remains (`docs/superpowers/notes/r0-control-mode.md`).

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

> **Amended (pass 5b).** PostToolUse and PostToolUseFailure are handled,
> only to end a permission (or elicitation) wait: Claude Code fires no hook
> on the user's answer, and the approved tool finishing is the first sign.
> They take a fast path (stdin drained unparsed, one `display-message`
> read, nothing written unless the pane is waiting on a prompt), about
> 6 ms p50 in release. The wait is ended pane-wide, from any context: the
> Notification that starts it carries no `agent_id`. Still no Task*.

> **As built (pass 5a).** The trait is `agent::adapter::AgentAdapter`:
> `kind()`, `events()` and `on_hook(event, payload, prior, now) ->
> Vec<Change>`, pure; `prior` is the pane state the rules need (subagent
> list, live background shell), read in one `display-message` call, and
> the changes are written in one chained `set-option` call, stamped with
> `@home_updated`. Liveness stays on `AgentSource::looks_alive` (by
> kind); `AgentSource::updated` lets a fresher later source win a pane.
> Background work comes from Stop's `background_tasks`, so no sidebar is
> needed for it. The subagent guard keys on the payload's `agent_id`
> (present only inside a subagent), not the subagent list. The README's
> hook commands end in `>/dev/null 2>&1 || true` with `timeout: 5`: exit 2
> from any hook blocks UserPromptSubmit and forces Stop to continue. The
> 10-minute stale clean-up is not built yet.

**Storage:** pane options in tmux-home's own namespace (`@home_agent`,
`@home_status`, `@home_wait_reason`, `@home_run_started`, `@home_prompt`, …)
so the old and new hooks can run side by side during migration without
fighting over the same options.

**Staleness:** agent options on a pane whose `pane_current_command` is a
shell ⇒ stale: dimmed `(ended?)`, not in NEEDS YOU or tallies. The daemon
(unlike the bash version) clears stale `@home_*` options after 10 minutes,
since they are its own.

## 6. Git and repos

**Base:** stray's git layer, vendored (MIT, same author): porcelain-v2
status (whose `branch.ab` gives upstream ahead/behind in the same fork),
unpushed commits against *all* remotes, no-upstream/gone branches, stashes,
numeric staged/modified/untracked/conflict counts, `worktree list
--porcelain`, the `.git` file/dir/bare classification and `.strayignore`.
Never writes to a repo (no git config, no cache in `.git`); never fetches.

**Folded in from worktrunk** (ideas reimplemented; any verbatim code is
recorded in `NOTICE` per §14). Source paths refer to worktrunk v0.79.0.

| # | Change | worktrunk source | When |
| --- | --- | --- | --- |
| 1 | Scrub inherited `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY` on every git call (a daemon started from a hook would otherwise query the wrong repo); set `LC_ALL=C` (stray parses translated `upstream:track` text); keep `GIT_OPTIONAL_LOCKS=0`. | `shell_exec.rs` | R3 |
| 2 | `status --untracked-files=normal` always, so a repo's `showUntrackedFiles=no` can't make it look clean. | `working_tree.rs::status_porcelain_cached` | R3 |
| 3 | One fork per branch for unpushed commits: `log --no-show-signature -n1001 <branch> --not --remotes`, count lines, keep 20 (was `rev-list --count` + `log`). | `collect/mod.rs` | R3 |
| 4 | **Refs snapshot memo:** one `for-each-ref refs/heads refs/remotes refs/stash` per tick; branch-level results (unpushed, ahead/behind, stray, stash) recomputed only when a relevant SHA changed. Memo lives in daemon memory, never in `.git`. | `sha_cache.rs`, `ref_snapshot.rs` (concept) | R3 |
| 5 | Operation in progress (`↻`: merge, rebase, cherry-pick, revert, bisect) and conflicts (`✘`) from file checks — no fork. | `working_tree.rs::operation_in_progress` | R3 |
| 6 | Parse `locked`, `prunable`, `detached`, `bare` from `worktree list`; flag duplicate-branch / path mismatch (`⚑`). | `git/parse.rs`, `model/state.rs` | R3 |
| 7 | Default branch, read-only: `worktrunk.default-branch` config → `<remote>/HEAD` → local inference (only branch, `init.defaultBranch`, main/master/develop/trunk). No `ls-remote`, no write-back. | `config.rs::default_branch` | R3 |
| 8 | "Integrated" (`⊂`) for stray branches, cheap tiers only (same commit, `merge-base --is-ancestor`, empty three-dot diff, equal trees) against an upstream-aware base (compare with `origin/main` when local main lags). Squash-merged branches stop looking unpushed forever. Runs in the repos scan only. | `git/mod.rs::check_integration`, `integration.rs::integration_targets` | R3 |
| 9 | Ahead/behind vs default branch (`↑↓`) via one `for-each-ref %(ahead-behind:BASE)`. | `ref_snapshot.rs::capture_ahead_behind` | later |
| 10 | Expensive integration tiers (`merge-tree`, patch-id), would-conflict `✗`, line stats, `worktrunk.state.*.marker` interop, `taskpolicy -b` for the scan. | various | later |

Deliberate departure from worktrunk: every git call has a timeout
(`kill_on_drop`, 10 s badge / 30 s scan) and per-repo in-flight coalescing —
a daemon can't let a hung git hold a permit forever. A timed-out badge shows
its last value marked stale, not an error.

**Per window:** active pane's `pane_current_path` → repo root → one
`RepoStatus` shared by every window in that repo. Work is ordered fast-first
and each field publishes as it lands: branch/HEAD from `.git/HEAD` (no
fork) → `git status` → ref-derived fields (only when the refs memo says
something changed). `git status` runs on cwd change, on window focus, and on
an adaptive interval: `max(10 s, 20 × last status duration)`.

**Repos view:** stray's scan of `@home-repos-root` (default `~/dev`, depth
8, honouring `~/.strayignore`), at daemon start, every 10 min, and on `r`.
Scan-only status runs with `-c core.fsmonitor=false` so the scan never spawns
fsmonitor daemons across `~/dev`. Repos with a tmux window are marked; `⏎`
on one jumps to its window if there is one (no window creation in phase 2).

**Concurrency:** one global git semaphore of 4 permits shared by badges and
the scan; the scan may hold at most 2, so it never delays a focus refresh.

**Badge symbols** (worktrunk's vocabulary where it has one, so badges read
like `wt list`): branch, `(wt)` for a linked worktree, then
`+` staged · `!` modified · `?` untracked · `✘` conflicts · `↻` operation in
progress · `⊟ ⊞ ⊘ ⚑` prunable / locked / detached / mismatch ·
`⇡n ⇣n` ahead/behind upstream · `|` in sync · plus two tmux-home symbols
defined in the help legend: `$n` stashes, `⚠n` stray branches (unpushed,
no upstream or gone, not integrated). `↑↓` stay reserved for "vs default
branch" (later).

**Implementation note (pass 5b, 2026-10-01).** Badges are in; the repos
view, density cycling and items 9–10 are not. Where the code settled
details this section leaves open, or departs from it:

- *Layout:* `src/git/` (vendored `model`/`status`/`scan`/`ignore` plus
  `exec`, `repo`, `refs`, `badge`) and `src/daemon/git.rs` (the task). The
  snapshot gains `git: { paths: cwd → root, repos: root → RepoStatus }`,
  hashed as its own section (`Sections.git`), so git never forces a tmux
  push and a tmux push always carries the latest git section.
- *Targets:* every window's row pane cwd and every agent pane's cwd (the
  agent row's badge is the lead agent's repo — its worktree when it runs in
  one). cwd → root is a walk up for `.git` (no fork), re-checked every 10 s.
- *Triggers:* cwd change, focus (a client's session's active window), the
  adaptive interval, **plus a change stamp** not in the spec: once a second
  the task `stat`s the index, HEAD and its reflog, `packed-refs`,
  `FETCH_HEAD`, the stash reflog, the operation state files and the
  worktree registry, and refreshes a repo whose stamp moved. Git commands
  run in a pane show within ~1 s; plain file edits wait for the interval or
  a focus change (no fsmonitor).
- *Publishing:* each stage lands in the task's state as it completes; the
  task publishes the section when it changed, at most once per second.
  A timed-out status keeps its values with `stale` (badge `~`); a failure
  also records `error` (shown on the card). A timeout counts as the
  status's duration, so a hung repo backs off to `20 × 10 s`.
- *Item 1:* the scrub removes every inherited `GIT_*` variable except
  `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`/`GIT_CONFIG_NOSYSTEM` (they pick
  user config files, not a repo). Review fix: no call runs a command a
  repo or user config names — `core.fsmonitor=false` always (badges
  included: a hook script would run, `true` starts a resident daemon),
  `core.hooksPath=/dev/null`, `protocol.allow=never`,
  `GIT_NO_LAZY_FETCH=1`, empty `credential.helper` and `diff.external`,
  `--no-ext-diff --no-textconv` on diffs, and attributes from the empty
  tree (`--attr-source`, sha1 or sha256) with global/system attributes
  off. Re-review fix: the primary layer doesn't parse attributes at all —
  before a repo's calls, one `config -z --get-regexp` (every scope, the
  same scrubbed env) lists every `filter|diff|merge.<any>.<command
  key>` and `credential.<url>.helper`, and each is overridden by its exact
  name (`-c <key>=`, `filter.<x>.required=false`); a key `-c` can't carry
  (an `=` in it, an unparseable record) fails closed: HEAD-only badge
  `⊗` (limited), no status or diff checks, logged once. That read also
  carries item 7's keys, so it replaces the refs stage's config fork (no
  extra fork) and is part of the memo key. Driver names in
  `info/attributes` are overridden too, as a second layer. So no
  clean/process filter or textconv runs (trade-off: a stat-dirty LFS or
  `text=auto` file can read as modified). Each call runs in its own
  process group, killed as a whole on timeout. Also `color.ui=false`,
  `log.showSignature=false`, `GIT_TERMINAL_PROMPT=0`. `$TMUX_HOME_GIT`
  replaces the binary (tests).
- *Item 4:* the probe is `for-each-ref
  %(refname)%00%(objectname)%00%(symref)%00%(upstream)%00%(upstream:track)`;
  its key also covers the config keys item 7 reads (read every refresh),
  and `worktree list` runs every refresh outside the memo (a lock or a
  switch inside a linked worktree moves no ref). A branch whose tip is
  a remote ref's commit has 0 unpushed without a fork. Stashes are the
  lines of `logs/refs/stash` (no fork; also right after `stash drop
  stash@{1}`, which leaves `refs/stash` alone).
- *Item 6:* `⚑` is a branch checked out in two worktrees, or a linked
  worktree whose directory isn't the branch and doesn't end in
  `.<branch>`/`-<branch>` (the branch sanitised as worktrunk does: `/`,
  `\` → `-`) — a stand-in for worktrunk's path template, which tmux-home
  doesn't know.
- *Item 7:* one `config -z --get-regexp` fork reads
  `worktrunk.default-branch`, `init.defaultBranch` and the remotes;
  `<remote>/HEAD` comes from the probe's `%(symref)`.
- *Item 8 in badges, not only the scan:* stray (`⚠`) means unpushed > 0,
  no upstream or a gone one, and not integrated (in a repo with no remote,
  where every commit is on none, any non-default branch not merged into
  the default branch); the cheap tiers run only
  for those candidates, memoised by (branch SHA, target SHA), so they are
  cheap enough for badges. Targets: the default branch and its upstream.
- *Badge order* (as the pass brief's example): `branch (wt) +!?✘ ⇡n ⇣n |
  $n ⚠n ↻⊟⊞⊘⚑ ~`; `|` only once a status has landed, no arrows for a gone
  upstream.
- *Measured* (release build, fsmonitor off, 8 repos under `~/dev`,
  warm cache, after the review fixes): HEAD stage 20–40 µs; `git status`
  8–15 ms; the refs stage 23–32 ms when nothing changed (3 forks: probe,
  config, worktree list). The first refs pass is 3–7 forks / 23–125 ms for
  most repos, and 49–67 forks / 0.6–1.5 s for the two with 20–30
  stray-candidate branches (one `log` and up to a few integration forks
  each, memoised afterwards). Steady state per repo: ~35–45 ms of git per
  refresh, every 10 s.
- *Generations:* each tracking of a root is a generation; a dropped
  root's refresh is aborted and late messages from an older generation
  are ignored. A refresh that panics resets its root (stale); a panicking
  task marks every badge stale and is restarted by a supervisor.

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
(MIT OR Apache-2.0; taken under MIT — ideas reimplemented per §6, and any
function copied verbatim, e.g. `operation_in_progress`, keeps worktrunk's
copyright line in `NOTICE`).

## 15. Decisions taken for you (say if any is wrong)

1. Git data comes from stray's vendored layer improved with worktrunk's
   techniques (§6), not from `wt list --format=json` or the worktrunk crate.
2. Agent state is written to tmux pane options *and* sent to the daemon, so
   it survives daemon restarts and works in degraded mode.
3. New `@home_*` option namespace rather than reusing the sidebar's
   `@pane_*` names.
4. The daemon pushes whole snapshots, not diffs.
5. Sidebar defaults: left, 32 columns, auto-create off (your config turns it
   on with `scratch` excluded).
6. `prefix .` stays the popup key; `prefix f` stays tmux-fzf until you say.
