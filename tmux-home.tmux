#!/usr/bin/env bash
# TPM entry point: binds the tmux-home popup and nothing else.
#
#   set -g @home-keys 'w f'   # prefix keys to bind (default "w f"; '' = none)

set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOME_BIN="$CURRENT_DIR/bin/tmux-home"

read -r -a TMUX_CMD <<<"${TMUX_HOME_TMUX:-tmux}"
t() { "${TMUX_CMD[@]}" "$@"; }

# Unset → default; explicitly set to '' → bind nothing.
if [[ -n $(t show-options -gq @home-keys) ]]; then
	keys=$(t show-options -gqv @home-keys)
else
	keys='w f'
fi

# display-popup does not expand formats in -e, so the binding goes through
# run-shell, which expands #{client_name} for the client that pressed the key.
# The popup is pinned to that client (-c) and told its name.
[[ ${TMUX_CMD[0]} == */* ]] || TMUX_CMD[0]=$(command -v "${TMUX_CMD[0]}")
tmux_q=$(printf '%q ' "${TMUX_CMD[@]}")
popup="${tmux_q}display-popup -c #{q:client_name} -E -B -w 100% -h 100%"
popup+=" -e TMUX_HOME_CLIENT=#{q:client_name} $(printf '%q' "$HOME_BIN")"

for key in $keys; do
	t bind-key "$key" run-shell -b "$popup"
done
