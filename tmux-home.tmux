#!/usr/bin/env bash
# TPM entry point: binds the tmux-home popup and starts the daemon.
#
#   set -g @home-keys '.'     # prefix keys to bind (default "."; '' = none)
#
# The popup is the Rust one when target/release/tmux-home is built, else the
# bash + fzf fallback in bin/. TMUX_HOME_POPUP=bash forces the fallback.

set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOME_BIN="$CURRENT_DIR/bin/tmux-home"
RUST_BIN="$CURRENT_DIR/target/release/tmux-home"

read -r -a TMUX_CMD <<<"${TMUX_HOME_TMUX:-tmux}"
t() { "${TMUX_CMD[@]}" "$@"; }

# Unset → default; explicitly set to '' → bind nothing.
if [[ -n $(t show-options -gq @home-keys) ]]; then
	keys=$(t show-options -gqv @home-keys)
else
	keys='.'
fi

# display-popup does not expand formats in -e, so the binding goes through
# run-shell, which expands #{client_name} for the client that pressed the key.
# The popup is pinned to that client (-c) and told its name.
[[ ${TMUX_CMD[0]} == */* ]] || TMUX_CMD[0]=$(command -v "${TMUX_CMD[0]}")
tmux_q=$(printf '%q ' "${TMUX_CMD[@]}")
popup="${tmux_q}display-popup -c #{q:client_name} -E -B -w 100% -h 100%"
popup+=" -e TMUX_HOME_CLIENT=#{q:client_name} "
if [[ -x $RUST_BIN && ${TMUX_HOME_POPUP:-} != bash ]]; then
	popup+="$(printf '%q' "$RUST_BIN") popup"
else
	popup+=$(printf '%q' "$HOME_BIN")
fi

for key in $keys; do
	t bind-key "$key" run-shell -b "$popup"
done

# Phase 2 daemon: start one for this server if the Rust binary is built.
# A second start is a no-op (lock), and it exits with the server.
if [[ -x $RUST_BIN ]]; then
	t run-shell -b "$(printf '%q' "$RUST_BIN") daemon --socket #{q:socket_path}"
fi
