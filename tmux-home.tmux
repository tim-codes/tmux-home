#!/usr/bin/env bash
# TPM entry point: binds the tmux-home popup and starts the daemon.
#
#   set -g @home-keys '.'     # prefix keys to bind (default "."; '' = none)
#
# The popup and daemon are the Rust binary, target/release/tmux-home. If it
# isn't built yet, the key says so and loading the plugin starts
# `cargo build --release` in the background.
#
# Environment (tests): TMUX_HOME_TMUX  tmux command, word-split (default: tmux)
#                      TMUX_HOME_BIN   the Rust binary (default:
#                                      target/release/tmux-home); when set, a
#                                      missing one is not built

set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_BIN="${TMUX_HOME_BIN:-$CURRENT_DIR/target/release/tmux-home}"

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
rust_q=$(printf '%q' "$RUST_BIN")
# ASCII only: printf %q would $'...'-quote anything else, which run-shell's
# /bin/sh (dash on Linux) can't read
building=$(printf '%q' "tmux-home: not built yet (cargo build --release in $CURRENT_DIR); try again in a minute")
# Which binary runs is decided when the key is pressed, so a build that
# finishes later (first install, TPM update) is used without a reload.
cmd="if [ -x $rust_q ]; then $popup$rust_q popup; else ${tmux_q}display-message -c #{q:client_name} $building; fi"

for key in $keys; do
	t bind-key "$key" run-shell -b "$cmd"
done

if [[ -x $RUST_BIN ]]; then
	# The daemon: one per server; a second start is a no-op (lock), and it
	# exits with the server.
	t run-shell -b "$rust_q daemon --socket #{q:socket_path}"
elif [[ -z ${TMUX_HOME_BIN:-} ]] && cargo=$(command -v cargo); then
	# First load without a build (spec section 10): build in the background.
	t run-shell -b "cd $(printf '%q' "$CURRENT_DIR") && $(printf '%q' "$cargo") build --release >/dev/null 2>&1"
fi
