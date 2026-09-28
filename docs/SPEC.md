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
  switch to something, or press `Esc` on an empty filter.
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

```
 tmux-home   main ▸ 2  ·  agents: 1 waiting · 1 running · 6 idle      F1 help
 > dotf▏                                                        (12/31 windows)
 ─ NEEDS YOU ───────────────────────────────────────────────────────────────
 ◐ scratch 4  dotfiles-fix      claude  waiting      permission: Bash(rm -rf…)
 ─ main ─────────────────────────────────────────── attached · 3 windows ──
   1  lighthouse            fish     ~/d/lighthouse        main
 ▶ 2  dotfiles              nvim     ~/d/dotfiles          main ✚
   3  api-refactor          ● running 12m  claude ×2       feat/api  (wt)
 ─ scratch ────────────────────────────────────────────────── 11 windows ──
   1  scratch               fish     ~
   4  dotfiles-fix          ◐ waiting      claude          fix/x     (wt)
   …
 ───────────────────────────────────────────────────────────────────────────
 │ preview: capture of the selected window's active pane                  │
 │ (agent windows: agent card — see §7)                                   │
 ───────────────────────────────────────────────────────────────────────────
 ⏎ go   ^r rename   ^x close   M-↑↓ move   ^o preview   F1 all keys
```

- **Header:** current location, agent tally by status, match count.
- **Filter line:** always focused unless an inline editor is open.
- **List:** grouped by session (the invoking client's session first, then by
  name), windows in index order. Row: index, name, then either the active
  pane's command + short path + git branch, or — for agent windows — status
  icon + word + run elapsed + agent count. `(wt)` marks a worktree, `✚` dirty.
- **Panes** hidden by default; `M-→` / `M-←` expand / collapse a window.
  Windows with more than one agent pane show a `×N` count. Sidebar panes
  (`@pane_role=sidebar`) are never listed or counted.
- **Preview:** right half at ≥ 160 columns, bottom third at ≥ 30 rows,
  otherwise hidden (the half-height dropdown gets the list only). `^o`
  toggles. Layout recomputes on terminal resize.
- **Footer:** the handful of keys valid in the current mode.

## 6. Interaction

### Navigation and switching

| Key | Action |
| --- | --- |
| printable keys | Filter (fuzzy over session, name, path, branch, command, agent prompt) |
| `↑` `↓`, `^p` `^n`, `^k` `^j` | Move selection |
| `PgUp` `PgDn` | Page |
| `←` `→`, `Home` `End` | Edit the filter (Cmd+←/→ send Home/End) |
| `M-→` / `M-←` | Expand / collapse window panes |
| `⏎` | Switch the invoking client to the selection and close |
| `^g` | Jump to the next item needing attention |
| `Esc` | Clear filter; if already empty, close |
| `F1` or `^/` | All keys |

Filter tokens (combine with text): `@attn`, `@agent`, `@running`,
`@waiting`, `@idle`, `@error`, `s:<session>`.

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
| `^x` | Close window / pane | Row shows `close "dotfiles-fix"? y/n`. Stronger wording if it hosts a running/waiting agent (`agent still working`), and if it is the session's last window (`session "scratch" will end`). `kill-window -t @id` / `kill-pane -t %id`. |
| `M-↑` `M-↓` | Move window up/down within its session | `swap-window -d -s @a -t @b` with the adjacent window **in the list** (not index ± 1, so index gaps are fine); `-d` so the current window doesn't change. Selection follows the window. |
| `M-n` | New window after the selection | Inline name editor; `new-window -a -d -t @id -c <cwd>`; selection moves to it; popup stays. |

After any close, tmux-home re-resolves the invoking client's current
session/window (closing the current window or ending the current session
moves the client; `detach-on-destroy off` switches it to another session).

## 7. Agent awareness

State comes from the pane options the tmux-agent-sidebar hooks publish
(event-driven, sub-second). tmux-home adds no hooks of its own.

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

Colours: terminal's default foreground/background plus the 16 ANSI colours,
so it follows whatever theme the terminal has.

## 10. Non-goals

- Sending prompts or keystrokes to agents; approving permissions.
- Launching agents (claude-hatch `prefix y`).
- Session persistence/restore (resurrect/continuum).
- Replacing the sidebar: the sidebar is the always-on glance, tmux-home the
  on-demand full view.
- Mouse support beyond what comes free.
- Undo.

Deferred until asked for: marks + bulk actions, move-to-session, peek
(switch but stay open), prebuilt release binaries, more config options.

## 11. Open questions

1. Once tmux-home is in daily use, does `prefix u` move to it (`@attn`
   filter) and claude-hatch's picker retire?
2. Is the cross-client resize hint enough, or should `⏎` from the dropdown
   offer "open in the main client instead"?

## 12. Implementation

**M0–M1: bash + a recent fzf, one script.** fzf 0.74 covers every v1
interaction without leaving the popup:

- inline rename: `^r` → `transform` into rename mode (`change-prompt`,
  `change-query` pre-filled with the name, `disable-search`, `rebind` of
  `enter`), commit via `execute-silent(tmux rename-window -t {id} {q})` then
  back to list mode + `reload`;
- close confirm: same mode switch with a `y/n` prompt;
- live refresh: `--listen` + a 1 s background ticker posting `reload`;
- grouping: session header lines as non-selectable rows (`--header-lines`
  per session is not enough; headers are rows skipped by a `transform` on
  movement), agent card via `--preview`.

Rows carry hidden ID fields (`--with-nth` / `--nth`) so every action targets
`@id`/`%id`, never the visible index.

**Rust + ratatui later, only if the prototype hits fzf's limits** (e.g.
editors disturbed by reload, header rows fighting navigation, preview
latency). Same stack as tmux-agent-sidebar.

**Install:** a TPM plugin (`tmux-home.tmux` sets the bindings and nothing
else). Dependencies: bash, a recent fzf (M0 uses `--id-nth`, `--footer` and `wait`; tested with 0.74.4), tmux ≥ 3.3.

**Testing:** all tmux I/O in one function set, exercised against a
throwaway `tmux -L tmux-home-test` server.

## 13. Milestones

- **M0 — usable this week:** grouped list, type-first filter, preview,
  `⏎` switch via the invoking client, inline `^r` rename, `M-r` reset.
- **M1 — tidy up:** `^x` close with confirm + consequence wording,
  `M-↑↓` reorder, `M-n` new window, live 1 s refresh.
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
