# tmux-home

A full-window home screen for tmux: every session, window and pane in one
popup, with the state of your coding agents alongside, and rename, reorder
and close without ever leaving it.

> **Status: design.** Nothing to install yet. The UX spec is in
> [`docs/SPEC.md`](docs/SPEC.md); the first milestone (M0) is a bash + fzf
> script.

## Why

- `choose-tree` can't rename and can't take custom keys.
- `find-window` asks for a search term in the status line before showing
  anything.
- The fzf-based pickers (tmux-fzf, tmux-sessionx, …) close the popup to
  rename a window, or only rename sessions.
- Agent sidebars show what Claude Code / Codex are doing, but not alongside
  window management.

tmux-home puts the overview, the management and the agent state in one
place, and keeps you in it until you choose where to go.

## Planned

- `prefix w` / `prefix f` open a borderless full-window popup, cursor in the
  filter — just start typing.
- Windows grouped by session, with command, path, git branch and worktree.
- `^r` renames in place, `M-↑`/`M-↓` reorder, `^x` closes with an inline
  confirm, `M-n` adds a window. The popup stays open.
- Agents needing you (permission prompts, questions, errors) pinned at the
  top, read from the `@pane_*` options that
  [tmux-agent-sidebar](https://github.com/hiroppy/tmux-agent-sidebar)
  publishes.

## Requirements (planned)

tmux ≥ 3.3, fzf ≥ 0.54, bash. Agent state needs tmux-agent-sidebar.

## Licence

MIT
