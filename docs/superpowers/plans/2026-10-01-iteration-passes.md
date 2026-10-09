# Iteration passes after the R1 MVP

Status: active plan, 2026-10-01. It supersedes `2026-10-01-r1-popup-parity.md`, which is on branch
`r1-popup` and is kept as a reference for its findings and test designs. It was written before the MVP
existed and would rebuild it.

Inputs: an independent review of R0, the MVP and that plan; and the user's verdict that the MVP "works
well". The priority is a tool used daily, improved in safe increments.

## Pass 1: MVP hot-fixes (single implementer, same day)

| # | Defect (verified on tmux 3.7c) | Fix | Test |
| --- | --- | --- | --- |
| H1 | The popup opens on the daemon's cached snapshot, which can be up to 500 ms stale. The cursor then lands on the previous window, and `^x` closes it without asking. | `popup/mod.rs` `run()`: always read tmux directly first, then call `select_current()`. The feed only applies updates. Also re-read fresh in the daemon on `Subscribe`. | Two windows. `select-window`, then open the popup at once. Assert the selected window ID equals the client's current window. |
| H2 | `#` is format-expanded in names, paths and session names (`#(…)` runs shell). | Add one `literal()` helper that doubles `#`. Use it at every expanded argument: `ops.rs` rename, `M-n -n/-c`, reopen `-c`, `-s`, `split-window -c`, rename on reopen. | Rename `fix #12` and `ab#` round-trips exactly. `M-n` from a cwd containing `#` lands there. Reopen restores a `#` name. |
| H3 | `M-r` unsets `automatic-rename`, so it inherits the global value, which may be off. | Set `automatic-rename on` instead. | With the global off, `M-r` re-enables auto-naming. |
| H4 | A reopen that fails after `new-window` has succeeded un-pops the stack, so the next `^t` creates a duplicate. | Un-pop only when the create itself failed. Later steps are best-effort. | Inject a failure after the create. Assert one window and the stack popped. |

Exit: tests pass, the branch is pushed, and the PR from `r1-mvp` to `main` is open. Merge it on the
user's direction, then point dotfiles back at `main`.

## Pass 2: safety net for destructive paths, then retire bash (subagent-driven with reviews)

The R1 plan supplies the designs:
- the `Outer` harness (Tasks 14–15);
- `tests/ops.rs` (Task 4);
- close/reopen integration tests (parity rows 13–27);
- manage e2e tests (rows 72–134, plus `reorder_and_new_window`);
- corrupt `closed.json` (Task 3), `tiny_terminals_do_not_panic` (Task 11), `daemon_death_falls_back_to_polling` (Task 13).

Port rows 1–4 and 36–70 as about 10 `app.rs` unit tests on the existing fixtures. The reviewer checks
that each test drives real tmux.

Exit: every parity row is green or listed as waived. Then delete `bin/tmux-home` and `tests/run`, and drop
the bash fallback binding.

## Pass 3: daemon/protocol hardening (single implementer)

- A build ID in the handshake `v` (git sha plus dirty flag), so a rebuild at the same version still restarts the daemon.
- A `refresh` op, or a fresh read on subscribe, plus a seq guard against stale pushes after a write.
- Change detection hashes the tmux section only, so volatile fields added later don't push every poll.
- One spawn/degraded path, shared by `client.rs` and the popup, and a single tmux wrapper.
- A size cap on `daemon.log`. The status chip documents that it shows ○ after a version restart until a client respawns the daemon.

Exit: a test that rebuilds at the same version proves the old daemon gets replaced.

## Pass 4: R2a agents, read-only (subagent-driven with reviews)

- Read tmux-agent-sidebar's existing `@pane_*` options in `PANE_FMT`.
- Derive `AgentState`, staleness and window status in pure code (SPEC.md §7).
- Popup UI: status column, NEEDS YOU pin, `^g`, filter tokens, agent card.
- No hooks are installed in this pass.

Exit: daily use shows agent status.

## Pass 5: R2b hooks, then R3 badges (single implementer each)

- `tmux-home hook claude <event>` behind the `AgentAdapter` trait, writing `@home_*`. It runs alongside the sidebar's hooks; no socket `agent_event` op is needed, because Poll picks the options up.
- Git badges only, from vendored stray plus spec §6 items 1–8:
  - git work runs in a separate daemon task, off the poll path;
  - results are keyed by repo root, use the refs memo, and are coalesced to at most one push per second.
- The Repos view comes later. Its scan results persist in the state dir, so a version restart doesn't rescan `~/dev`.
- *Done (5b, branch `passes-6`):* badges on window and agent rows, the git card and the F1 legend; items 1–8 (item 8 cheap tiers, also used by badges). Notes: design spec §6 "Implementation note".

## Pass 6: R4 sidebar, then the R5 cutover

- `tmux-home sidebar` is read-only on `e`/`E`.
- The sidebar process sets `@home_role=sidebar` on its own `$TMUX_PANE`. Dotfiles adds `"~tmux-home sidebar"` to `@resurrect-processes`, so resurrect restores it. No `sidebars.json`; this replaces spec §8's "replace restored shells" plan.
- Then the dotfiles cutover PR (spec §11).

## Pass 7: resume Claude sessions after a reboot (single implementer)

Scope widened by the user: SPEC §10 listed persistence/restore and launching agents as non-goals; agent
*resume* (never launching a new agent) is now an explicit, opt-in goal (`@home-restore-agents on`).

- **Record: pane options, not a sidecar file.** The hook already writes the pane's `@home_*` in one
  `set-option` batch; SessionStart, UserPromptSubmit and Stop (main context only) add
  `@home_transcript`, `@home_cwd` (launch directory: taken at SessionStart, or when the pane's session
  changed without one, since Claude's `cwd` follows `cd`s in Bash calls) and `@home_config_dir` (the
  hook's inherited `$CLAUDE_CONFIG_DIR`, the account; unset = default). Options are atomic (tmux
  serialises them), race-free across panes, die with the server (so no pruning, no stale pane IDs
  from a previous boot) and SessionEnd's teardown already wipes them. Not in the daemon's poll format.
- **Snapshot at save** (`tmux-home agents-snapshot`, `@resurrect-hook-post-save-all`): pane IDs don't
  survive a restore, so the record is saved under `session:window.pane` from the layout resurrect has
  just saved, to `<state dir>/agents.json` (temp + rename). Only panes whose `pane_current_command`
  is Claude (`AgentKind::Claude.looks_alive`) at that moment are saved. That, not SessionEnd's
  `reason`, decides "don't resume what the user exited": an exited, crashed or killed session leaves
  the pane at a shell (or running something else) by the next save, whether or not SessionEnd fired;
  SessionEnd (any reason) clears the options too. A pane `restore-agents` claimed whose claude hasn't
  recorded itself yet keeps its previous entry, so a save during a restore loses nothing.
- **Restore** (`tmux-home restore-agents`, `@resurrect-hook-post-restore-all`, before
  `@home_restore_done`): once per server (`@home_agents_restored`). Claims (`@home_restore`) each
  saved pane that is a fish/POSIX shell prompt, not in a mode, in the saved directory, with the
  transcript and launch directory on disk; logs every skip to `<state dir>/restore.log`. A detached
  worker types `cd <cwd> && env [-u] CLAUDE_CONFIG_DIR[=<dir>] claude --resume <id>` into each
  (re-checked first; marks `@home_restore_typed`), one per `@home-restore-agents-delay` (default 2 s).
  `env` bypasses a shell function named `claude`. Values are single-quoted so fish and POSIX shells
  read them alike (`'` and `\` escaped outside the quotes).
- `claude --help` (2.1.295) documents `--resume <session id>` only, so the id is used, not the
  transcript path; `cd` first anyway: project settings, CLAUDE.md and the transcript's project dir
  follow the cwd.

Exit: unit tests for record, snapshot, matching, quoting (through fish/bash/zsh/sh/dash) and skip
rules; `tests/restore.rs` end to end on a throwaway server with a fake `claude`.

## Spec amendments carried by this plan

- Spec §8, resurrect: use self-marking plus `@resurrect-processes`, as in pass 6.
- Spec §5, transition: read `@pane_*` first (pass 4); `@home_*` hooks come second (pass 5).
- Spec §6, refresh: git runs off the poll path and pushes are coalesced; scan results persist.
- Spec §3, handshake: compare a build ID, not only the package version.
- SPEC §10, non-goals: resuming Claude sessions after a resurrect restore is an opt-in goal (pass 7).

## Known caveat (shared with bash, not fixed)

A stopped job (`^z vim`) makes its pane look idle, so `^x` closes the window without asking. Revisit in pass 2.
