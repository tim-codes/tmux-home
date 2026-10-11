# tmux-home — UX spec (v0.2)

A full-window "home screen" for tmux: one popup that shows every session,
window and pane, the state of the coding agents running in them, and lets you
switch to, rename, reorder and close things without ever leaving it.

v0.2 incorporates an adversarial review of v0.1 — see §14.

## 1. Why

tmux's built-ins and the existing plugins each cover a slice, and each breaks
flow somewhere:

| Tool | Breaks where |
| --- | --- |
| `choose-tree` (`prefix w`) | No rename. Can't bind custom keys inside it. Actions go through a `:` template (`rename-window -t %% name`). |
| `find-window` (`prefix f`) | Asks for the search term in the status line *before* showing anything. |
| `command-prompt` (`prefix ,`) | Drawn over the status line; with a top status bar and a transparent theme the window tabs show through the typed text. |
| tmux-fzf | Rename closes the popup and asks in a split pane. Flat lists; no agent state. |
| tmux-sessionx | Rename is session-only. |
| tmux-menus | Rename falls back to `command-prompt`. |
| tmux-zoxide-session | In-popup window rename (via fzf's query line), but unmaintained, zoxide-bound, no agent state. |
| tmux-agent-sidebar | Great agent state, but one narrow column per window; read/navigate only; no window management. |
| claude-hatch picker (`prefix u`) | Agents only; no windows/sessions; no management. |

Nothing combines **overview + in-place management + agent awareness**.

## 2. Who and how

The primary user runs tmux as the whole desktop: a full-screen session
(`main`, Alacritty) plus a half-height dropdown session (`scratch`, Ghostty
quick terminal), each holding many windows, a good share of which host Claude
Code agents (often several per project, often in git worktrees). The
recurring jobs:

1. **"Where is it?"** — jump to a window by typing part of its name, path,
   branch or agent prompt.
2. **"Who needs me?"** — see which agents are waiting (permission, question,
   finished) and go to them.
3. **"Tidy up."** — rename windows to something meaningful, put them in a
   sensible order, close the dead ones — several in a row, without
   reopening anything.

## 3. Principles

- **Type first.** The popup opens with the cursor in the filter. Every
  printable key goes to the filter; no pre-prompt. Actions live on `Ctrl` /
  left-`Option` (Meta) chords so they never collide with filter text, and
  navigation keys match fzf.
- **Never leave the popup to finish an action.** Every edit — rename,
  confirm close — happens inline in the list. The popup only closes when you
  switch to something, press `Esc` on an empty filter, or press the key
  that opened it again.
- **Attention floats.** Anything that needs a human is pinned at the top.
- **tmux is the database.** Read state from tmux formats and the `@pane_*`
  options agent hooks already publish; write through plain tmux commands.
  tmux-home persists nothing.
- **Target by ID, never by position.** Every write uses `@window_id`,
  `%pane_id`, `$session_id` (indexes shift under `renumber-windows`; names
  repeat).
- **Legible everywhere.** Full window, opaque background (this also covers
  the status line, fixing text-over-tabs), no reliance on colour alone
  (icon + word).

## 4. Launch

| Binding (default) | Opens |
| --- | --- |
| `prefix w` | tmux-home, selection on the current window |
| `prefix f` | same (muscle memory for "find") |

Launched as `display-popup -E -B -w 100% -h 100%` with the invoking client
passed in (`-e TMUX_HOME_CLIENT=#{client_name}`); every switch and every
notion of "current" uses that client, never tmux's "most recent client"
guess — the user has two clients attached (Alacritty + Ghostty dropdown).

Keys typed into the popup go to tmux-home before any tmux key table, so root
bindings (vim-tmux-navigator `C-h/j/k/l`, `M-h/j/k/l` pane resize) do not
fire inside it; tmux-home receives those keys.

`prefix u` (claude-hatch's agent picker) is left alone; tmux-home offers
`@attn` filtering instead (§6) and the user can rebind later.

## 5. Layout

*Revised 2026-10-11 (docs/superpowers/specs/2026-10-11-paged-tree-table.md):
session pages, a window/pane tree, a column table, a bottom layout preview.*

```
  ▶dev ●1   scratch                                                       F1 help
>                                                                             4/4
   #     NM     CM       ST                  T    AG         PA             GT
 ─ NEEDS YOU ─────────────────────────────────────────────────────────────────────
   0     fix             ◐ waiting                claude
   └ .0         claude   ◐ waiting           12m  claude     ~/d/app.fix-x  fix/x (wt) ! ≡
 ─ dev ───────────────────────────────────────────────────────────────────────────
   1     api             ● running                claude ×2
   ├ .0         claude   ● running           12m  claude     ~/d/api
▌  └ .1         claude   ● running           12m  claude     ~/d/api
 ▶ 3     shell
   └ .0         fish                                         ~/d/a/src      main +!? ⇡2
──────────────────────────────────────────────────────────────────────────────────
 ● running  ·  claude  ·  run 12m
┌──────────────────────────────────────┬─────────────────────────────────────────┐
│$ cargo test                          │> add the tests                          │
└──────────────────────────────────────┴─────────────────────────────────────────┘
 ⏎ go   ^r rename   ^x close   M-↑↓ move   ^o preview   pfx ( ) session   F1 keys
```

- **Tab strip** (the header): every session by name; the page shown
  reversed and bold, the client's session `▶`, `●N` (yellow) its windows
  that need you; `(direct)` in degraded mode, `F1 help` at the right. Tabs
  that don't fit around the page shown give way (`‹` `›`).
- **Session pages:** the list shows one session. It opens on the client's
  session (no client: the first by name). `prefix )` / `prefix (` page,
  wrapping — tmux's own next/previous-session keys, recognised the way the
  open-key chord is (§6), deliberately not a bare key; list navigation never
  pages. The filter is scoped to the page; paging keeps and re-applies it.
- **Filter line:** always focused unless an inline editor is open; the
  count at its right is the page's windows matching / all.
- **Column header:** dim, htop-style: `# NM CM ST T AG PA GT MS`.
- **List:** a NEEDS YOU group (the page's windows that need you) when there
  is one, then the page's windows in index order. Every window is a row and
  every non-sidebar pane a row beneath it, single-pane windows included.

  | col | meaning | window row | pane row |
  | --- | --- | --- | --- |
  | `#` | index | window index (`▶` before it: the client's window) | `├ .N` / `└ .N` |
  | `NM` | name | window name | pane title, blank when it is the host name |
  | `CM` | command | — | current command (a tagged agent's version-named binary shows as the agent) |
  | `ST` | agent state, icon and word | most urgent pane's | the pane's; stale: dim `(ended?)` |
  | `T` | run time | — | the agent's |
  | `AG` | agent kind | `kind ×N` | kind |
  | `PA` | path (short, `~`) | — | cwd |
  | `GT` | git badge | — | the pane cwd's repo |
  | `MS` | message, rest of the width | — | wait reason (yellow) / prompt (dim) |

  Widths are computed per page from the content (capped) so values align;
  columns nothing on the page fills are left out. Narrow widths drop `MS`,
  then `GT`, `PA`, `T`, `AG`, `CM`, `ST`; `NM` shrinks no further than its
  minimum. `F1` lists the legend. Sidebar panes (`@pane_role=sidebar`) are
  never listed or counted.
- **Preview:** always at the bottom, shown when the popup is ≥ 30 rows,
  else hidden unless `^o` (which also hides a shown one) or `M-k`. Size,
  auto (no `@home-preview-pct`): the list gets its page's lines (window and
  pane rows, group headers) + 2 blank rows, the preview the rest,
  recomputed every draw; if that leaves the preview under max(⅓ of the
  list+preview area, 5 rows) it is a 50/50 split and the list scrolls.
  `M-k`/`M-j` move the divider 5 points up/down, starting from the split
  shown (auto converted to its percentage), clamped so list and preview
  keep 5 rows each (no-op at the limit; `M-j` never hides the preview), and
  write `set -g @home-preview-pct N`; `M-=` unsets it. A set size is
  clamped the same way on a smaller popup. It
  draws the selected window's pane layout — each pane's visible capture
  (`capture-pane -e`) in its rectangle scaled to the preview (the bounding
  box of the non-sidebar panes), inside a frame with tmux-style separators.
  On a pane row that pane is outlined in magenta and is the one the wheel
  and `S-↑`/`S-↓` scroll (its capture includes history); on a window row
  nothing is highlighted or scrolls. The agent and git cards sit above the
  layout: the lead pane's for a window row, the selected pane's for a pane
  row. Pane geometry comes from the snapshot (`pane_left/top/width/height`,
  `window_width/height`). Layout recomputes on terminal resize.
- **Footer:** the handful of keys valid in the current mode.

## 6. Interaction

### Navigation and switching

| Key | Action |
| --- | --- |
| printable keys | Filter (fuzzy over session, name, path, branch, command, agent prompt) |
| `↑` `↓`, `^p` `^n`, `^k` `^j` | Move selection over windows and panes (wraps within the page) |
| `prefix )` / `prefix (` | Next / previous session page (wraps; §5) |
| `PgUp` `PgDn` | Page |
| `←` `→`, `Home` `End` | Edit the filter (Cmd+←/→ send Home/End) |
| `⏎` | Switch the invoking client to the selection and close (a pane row: also select that pane) |
| `^g` | Jump to the next window needing attention, paging to its session |
| `Esc` | Clear filter; if already empty, close |
| `prefix` + home key | Close, from any mode (the key that opened the popup toggles it). Inside the popup tmux passes the prefix on, so the chord arrives as two keys; the popup learns its prefix and `@home-keys` from `TMUX_HOME_PREFIX` / `TMUX_HOME_KEYS`, set by the binding. The prefix keeps its own meaning (`^a`: cursor to the start; cancels a close prompt; leaves help), so only a home key straight after it closes. An unsaved rename / new-window name is dropped and a pending close is not done. Only single-character home keys count; `prefix2` is not recognised. |
| `M-k` / `M-j` | Preview larger / smaller: divider up / down 5 points (§5); `M-k` shows a hidden preview. List only — the editors ignore them (no text inserted). tmux delivers them to the popup even with root-table `M-j`/`M-k` bindings (`resize-pane`): while a popup has focus its keys go to the popup's program |
| `M-=` | Preview back to auto size (unsets `@home-preview-pct`) |
| `F1` or `^/` | All keys |

Filter tokens (combine with text): `@attn`, `@agent`, `@running`,
`@waiting`, `@background`, `@idle`, `@error`. (`s:<session>` is gone:
paging replaces it.) Window actions (rename, close, move, new) act on a
pane row's window.

**Switching across clients.** `⏎` onto a window in a different session than
the invoking client's (e.g. from the `scratch` dropdown to a `main` window)
moves *this* client there. With `aggressive-resize` / `window-size latest`
that can shrink the target window to the dropdown's size; tmux-home shows a
one-line hint `(opens in this dropdown — resizes main:2)` on the selected
row before you commit. No attempt to steer the other client in v1.

### Management (inline; popup stays open; list refreshes in place)

| Key | Action | Inline UX / tmux |
| --- | --- | --- |
| `^r` | Rename window (session if a session header is selected) | Row becomes an editor pre-filled with the current name, cursor at end. `⏎` commit, `Esc` cancel, empty = cancel. `rename-window -t @id` / `rename-session -t $id`. |
| `M-r` | Reset to automatic name | `set -w -t @id -u automatic-rename` (rename turns it off; this turns it back on). |
| `^x` | Close window / pane | Closes at once if every (non-sidebar) pane sits at a shell prompt. Otherwise the prompt becomes `close "dotfiles-fix"? running: nvim, node (y/N)`: `y` closes, any other key / `⏎` / `Esc` cancels. Stronger wording if it hosts a running/waiting agent (`agent still working`, M2); the session's last window always asks (`session "scratch" will end`). The last window on the server is never closed. `kill-window -t @id` / `kill-pane -t %id`. |
| `M-↑` `M-↓` | Move window up/down within its session | `swap-window -d -s @a -t @b` with the adjacent window **in the list** (not index ± 1, so index gaps are fine); `-d` so the current window doesn't change. Selection follows the window. |
| `^t` | Reopen the last closed window | Every close snapshots the window's shape (session, index, neighbours, name / auto-name, layout, each pane's cwd, active pane) onto a 10-deep LIFO stack in `${XDG_STATE_HOME:-~/.local/state}/tmux-home/closed`. Reopen rebuilds it with fresh shells — no processes are restarted — at its old index (else after its old neighbour, else at the end), recreating the session if it ended; cursor moves to it, filter cleared, no switch. Also `tmux-home reopen`. |
| `M-n` | New window after the selection | Inline name editor; `new-window -a -d -t @id -c <cwd>`; selection moves to it; popup stays. |

After any close, tmux-home re-resolves the invoking client's current
session/window (closing the current window or ending the current session
moves the client). Before ending the invoking client's own session,
tmux-home switches that client to another session itself — otherwise
`detach-on-destroy on` (the default) would detach it and take the popup
down mid-action. The cursor stays at the same row position (the next
window) and the filter is kept.

## 7. Agent awareness

State comes from pane options that agent hooks publish (event-driven,
sub-second): tmux-home's own (`tmux-home hook claude <Event>`, writing
`@home_*`, pass 5a) and tmux-agent-sidebar's (`@pane_*`), per pane in that
order.

> **Pass 5a.** `tmux-home hook claude <Event>` handles SessionStart,
> UserPromptSubmit, Stop, StopFailure, Notification, PermissionDenied,
> SessionEnd, SubagentStart and SubagentStop (not PostToolUse or Task*),
> ported from the sidebar's handlers through an `AgentAdapter`
> (`src/agent/adapter.rs`, `claude.rs`). It writes `@home_agent`,
> `@home_status`, `@home_attention`, `@home_wait_reason`,
> `@home_run_started`, `@home_prompt` / `_prompt_source`,
> `@home_subagents`, `@home_bg_cmd`, `@home_permission_mode`,
> `@home_worktree_name` / `_branch`, `@home_session_id`, `@home_updated`, with the
> sidebar's value vocabulary, so the table below applies with `@home_`
> for `@pane_` (and `@home_run_started` for `@pane_started_at`). The
> `HomeSource` reads them first and wins, unless the sidebar's are
> clearly newer (`@home_updated`, stamped on every write, against the
> sidebar's `@pane_started_at`) or name another session after `@home_*`
> has been quiet 30 s; a pane without them falls back to the sidebar's. Differences from the
> sidebar, from Claude Code 2.1.284's payloads: no activity log, cwd or
> desktop notifications; no TaskCompleted, so a running agent never gets
> attention; only `permission_prompt`, `elicitation_dialog`,
> `elicitation_url_dialog` and `agent_needs_input` notifications wait
> (the rest are metadata); PermissionDenied (auto mode) records
> `permission_denied:<reason>` with attention but leaves the status, as
> the model carries on; background work comes from Stop's
> `background_tasks` (or the sidebar's `@pane_bg_cmd`); subagent events are
> recognised by their payload's `agent_id`, not the subagent list, so
> SessionEnd (main context only) always clears, and SessionStart
> `startup`/`clear` drops the list; UserPromptSubmit records the prompt
> only for `source` `user`/`sdk` (or none).

> **Until pass 5** (implemented in pass 4), this state comes only from
> tmux-agent-sidebar's `@pane_*` options, read in the same `list-panes` call
> as everything else. The model (`src/agent/`) reads them through an
> `AgentSource`; pass 5's own hooks write `@home_*` and add a second source
> ahead of it, without UI changes. Pass-4 details: there is also an
> `@background` token; staleness is a positive check per kind (Claude is
> alive only as its version, `claude` or `node`; other kinds as any
> non-shell); a notification on a *running* agent doesn't need you (the
> sidebar flags TaskCompleted mid-run); the lead pane (card, preview, `⏎`
> focus) is the one that needs you; a status token
> (`@running`, …) matches a window with a live agent in that status, and
> several are alternatives; `@agent` includes stale agents; `s:` is a
> case-insensitive prefix; free text matches prompts word by word as
> substrings rather than fuzzily (a long prompt matches almost any short
> fuzzy query). A NEEDS YOU window is listed twice, pinned and in its
> session; `^g` visits the pinned copies. Claude's `pane_current_command` is
> its version (`2.1.283`), so agents are named by `@pane_agent`, also in the
> `^x` prompt.

| Option | Shown as |
| --- | --- |
| `@pane_agent` | agent kind (claude / codex / opencode) |
| `@pane_status` | `●` running · `◎` background · `◐` waiting (also `notification`) · `○` idle · `✕` error · `·` unknown — always icon **and** word |
| `@pane_attention` = `notification`, `@pane_wait_reason` | NEEDS YOU section + reason: permission, question (`elicitation_dialog`), teammate idle (`teammate_idle:<name>`), error text |
| `@pane_started_at` | elapsed time of the **current run** (set at run start, cleared at run end) — not time-in-state |
| `@pane_prompt` | last prompt (filterable; truncated in list, fuller in card) |
| `@pane_subagents` | `+N subagents` |
| `@pane_bg_cmd` | background command hint |
| `@pane_worktree_name` / `_branch` | `(wt)` + branch |
| `@pane_permission_mode` | `plan` badge |

**Window status** = most urgent of its agent panes: error > waiting >
running > background > idle.

**Staleness.** The sidebar only cleans up Claude panes via Claude's
SessionEnd hook; a crash or `kill -9` leaves e.g. `@pane_status=waiting`
behind. If a pane's `pane_current_command` is a shell (fish, zsh, bash, sh)
while it carries agent options, tmux-home treats them as **stale**: shown
dimmed with `(ended?)`, excluded from NEEDS YOU and the tally. Read-only —
tmux-home never clears another tool's options.

**Agent card** (preview for agent windows): status, run elapsed, wait
reason, subagents, background command, worktree/branch, permission mode,
the prompt, then the last lines of the pane.

No fallback when the sidebar isn't installed: windows are listed without an
agent column.

## 8. Live behaviour

- Refresh every 1 s with a single `tmux list-panes -a -F …` call; keep the
  selection by ID, never by row position.
- Filter text, expansion and scroll survive refreshes.
- An open inline editor is never disturbed by a refresh; if its target
  disappears, the editor closes with a one-line notice. Refresh pauses while
  a close confirmation is showing.
- Empty states: no matches → `no windows match "…"  (Esc clears)`;
  no agents → the tally and NEEDS YOU section are omitted.

## 9. Configuration

Only what the primary user needs; more when someone asks.

| tmux option | Default |
| --- | --- |
| `@home-keys` | `w f` (space-separated prefix keys; empty = bind nothing) |
| `@home-preview` | `auto` (`right`, `bottom`, `off`) |
| `@home-preview-pct` | unset = auto size (§5); 1–99 = the preview's % of the list+preview area, written by `M-j`/`M-k`, unset by `M-=` |
| `@home-restore-agents` | `off` (`on`: resume Claude Code sessions after a resurrect restore, §12) |
| `@home-restore-agents-delay` | `2` (seconds between resumed launches) |

Colours: terminal's default foreground/background plus the 16 ANSI colours,
so it follows whatever theme the terminal has.

## 10. Non-goals

- Sending prompts or keystrokes to agents; approving permissions.
- Launching agents (claude-hatch `prefix y`). Resuming one after a reboot
  is the single exception (below); a new agent is never started.
- Session persistence/restore (resurrect/continuum): tmux-resurrect saves
  and restores the layout. tmux-home only cooperates with it: sidebars
  restore themselves, and, as an explicit opt-in goal (pass 7,
  `@home-restore-agents on`), a pane that was running a Claude Code session
  when resurrect last saved comes back running `claude --resume <that
  session>` in its directory and account. Resume only: never a new
  session, never a pane that wasn't running one, never one the user
  exited.
- Replacing the sidebar: the sidebar is the always-on glance, tmux-home the
  on-demand full view.
- Mouse support beyond what comes free.
- Undo (beyond `^t` rebuilding a closed window's shape).

Deferred until asked for: marks + bulk actions, move-to-session, peek
(switch but stay open), prebuilt release binaries, more config options.

## 11. Open questions

1. Once tmux-home is in daily use, does `prefix u` move to it (`@attn`
   filter) and claude-hatch's picker retire?
2. Is the cross-client resize hint enough, or should `⏎` from the dropdown
   offer "open in the main client instead"?

## 12. Implementation

**Now (R1): Rust + ratatui, one binary, with a daemon.** The bash + fzf
prototype (M0–M1, `bin/tmux-home`) was the reference implementation until
the Rust popup reached parity with it, then it was deleted. The design is
`docs/superpowers/specs/2026-09-29-rust-daemon-design.md`, which supersedes
this section, and §7, §8, §10 and §13 where they conflict.

- `tmux-home popup` runs inside `display-popup -E -B -w 100% -h 100%`, pinned
  to the invoking client (`-c`, and `TMUX_HOME_CLIENT` in its environment):
  every switch and every notion of "current" use that client.
- `tmux-home daemon`, one per tmux server, reads tmux (polling every 500 ms
  by default) and pushes snapshots to subscribers. The popup subscribes; if
  the daemon is down it reads tmux itself, shows `(direct)` in the header,
  and starts a new daemon in the background. After each of its own writes
  the popup re-reads tmux at once.
- The popup's state is a pure state machine (`src/popup/app.rs`: keys and
  snapshots in, actions out); `src/ops.rs` performs the writes. Every write
  targets IDs (`@n`, `%n`, `$n`), never indexes or names. Names and paths
  are escaped for tmux (`#` → `##`) so they are stored exactly as typed.
- Closed windows go to `<state root>/<server key>/closed.json`: a 10-deep
  LIFO of window shapes, written atomically under `state.lock`, shared by
  the popup and `tmux-home reopen`. The bash prototype's `closed` file is
  imported once.
- Sidebar panes (`@pane_role=sidebar`) are never the row's pane, never
  previewed, never close-checked and never part of a reopen shape. A job
  stopped with `^z` makes its pane busy.
- Git badges (pass 5b) come from the daemon's git task (`src/daemon/git.rs`)
  over a vendored copy of stray's git layer (`src/git/`), published as the
  snapshot's `git` section: repo root → status, pane cwd → root. Git runs
  read-only, with a timeout and at most 4 at once; nothing git-related runs
  on the tmux poll's path.
- Agent restore (pass 7, `src/restore.rs`; opt-in). The Claude hook keeps
  each pane's session in its options (`@home_session_id`,
  `@home_transcript`, `@home_cwd`, `@home_config_dir`). `tmux-home
  agents-snapshot`, run by resurrect after each save, writes the panes
  running Claude at that moment to `<state dir>/agents.json` by
  `session:window.pane`. `tmux-home restore-agents`, run by resurrect after
  a restore, once per server, types `cd <cwd> && env [-u]
  CLAUDE_CONFIG_DIR[=<dir>] claude --resume <id>` into each restored pane
  that is at a shell prompt in the saved directory and whose transcript
  still exists, one every `@home-restore-agents-delay` seconds from a
  detached process.

**Install:** a TPM plugin. `tmux-home.tmux` binds `@home-keys` (default `.`)
and starts the daemon; without a built binary the key shows a one-line
message and the plugin starts `cargo build --release` in the background.
Dependencies: tmux ≥ 3.3 and a Rust toolchain.

**Testing:** `cargo test`. Unit tests cover the pure popup state and drawing;
integration and end-to-end tests run against throwaway servers only
(`tmux -L th-test-*`, plus a `th-outer-*` server whose pane hosts a real
client, so the popup's binding is pressed and the popup is read back with
`capture-pane`). `docs/superpowers/notes/r1-parity.md` maps the retired bash
suite's checks to these tests.

## 13. Milestones

- **M0 — usable this week:** grouped list, type-first filter, preview,
  `⏎` switch via the invoking client, inline `^r` rename, `M-r` reset.
- **M1 — tidy up:** `^x` close with confirm + consequence wording (done:
  windows only; plus `^t` reopen), `M-↑↓` reorder, `M-n` new window, live 1 s refresh.
- **M2 — agents:** status column, NEEDS YOU, staleness, agent card, `^g`,
  filter tokens.
- **M3 — polish:** narrow/half-height layout rules, cross-client hint,
  `@home-*` options, README screenshots.

## 14. Review log

Adversarial review of v0.1 (14 findings, all accepted). Main changes:

- Rename moved into M0 (it's the reason the project exists); M0–M1 built as
  an fzf script instead of Rust.
- Key map: `^n`/`^p`/`^j`/`^k` navigate (fzf parity); preview `^o`; new
  window `M-n`; help `F1`/`^/` (no `?` — it's filter text); expand on
  `M-→`/`M-←` so `←`/`→` edit the filter.
- Every write targets IDs; reorder uses `swap-window -d` with the list
  neighbour.
- Invoking client passed into the popup; cross-client resize hint.
- `@pane_started_at` is run elapsed, not time-in-state; `notification` is a
  waiting status; stale-agent detection via shell `pane_current_command`.
- Close wording covers "session will end"; client location re-resolved
  after kills.
- Cut from v1: `claude agents --json` fallback, marks/bulk, move-to-session,
  peek, undo, theme option, prebuilt binaries.
- Preview hidden below 30 rows / 100 columns; root bindings don't fire inside
  the popup (documented).
