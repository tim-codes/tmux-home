# tmux-home

A full-window home screen for tmux: every session, window and pane in one
popup, with the state of your coding agents alongside, and rename, reorder
and close without ever leaving it.

> **Status: M0 + close.** A bash + fzf prototype: grouped window list,
> type-first filter, preview, `⏎` switch, inline `^r` rename, `M-r`
> auto-name, `^x` close with an inline confirm. Agents, reorder and live
> refresh come in later milestones — see [`docs/SPEC.md`](docs/SPEC.md) §13.

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

## Install

With [TPM](https://github.com/tmux-plugins/tpm):

```tmux
set -g @plugin 'tim-codes/tmux-home'
# set -g @home-keys 'w f'   # prefix keys to bind (default); '' binds nothing
```

Or clone it and add `run-shell ~/path/to/tmux-home/tmux-home.tmux`.

Try it without installing (from a shell inside tmux, in a checkout):

```sh
tmux display-popup -E -B -w 100% -h 100% \
  -e "TMUX_HOME_CLIENT=$(tmux display -p '#{client_name}')" "$PWD/bin/tmux-home"
```

## Keys (M0)

| Key | Action |
| --- | --- |
| type | filter (session, name, command, path) |
| `↑` `↓` `^p` `^n` `^k` `^j` | move |
| `⏎` | switch to the window, close |
| `Esc` | clear the filter; close when empty |
| `^r` | rename inline (`⏎` save, `Esc` or empty cancels) |
| `M-r` | back to the automatic name |
| `^x` | close the window, staying in the popup (see below) |
| `^o` | toggle preview |
| `F1` `^/` | help |

`^x` closes at once when every pane in the window is idle at a shell
prompt (bash, zsh, fish, sh, …; sidebar panes don't count). Otherwise it
asks inline — `close "api"? running: nvim, node (y/N)` — and only `y`
closes; any other key, `⏎` or `Esc` cancels and restores the filter. The
session's last window always asks (`session "x" will end`); if that is the
session you are in, tmux-home first moves you to another session so the
popup isn't detached. The last window on the server is never closed.

## Tests

`tests/run` drives the real popup through a client attached to a throwaway
`tmux -L tmux-home-test` server; it never touches your tmux server.

## Planned

- `prefix w` / `prefix f` open a borderless full-window popup, cursor in the
  filter — just start typing.
- Windows grouped by session, with command, path, git branch and worktree.
- `^r` renames in place, `M-↑`/`M-↓` reorder, `^x` closes with an inline
  confirm (done), `M-n` adds a window. The popup stays open.
- Agents needing you (permission prompts, questions, errors) pinned at the
  top, read from the `@pane_*` options that
  [tmux-agent-sidebar](https://github.com/hiroppy/tmux-agent-sidebar)
  publishes.

## Requirements

tmux ≥ 3.3, a recent fzf (M0 uses `--id-nth`, `--footer` and `wait`;
tested with fzf 0.74.4 and tmux 3.7c), bash. Agent state (M2) will need
tmux-agent-sidebar.

## Licence

MIT
