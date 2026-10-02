#!/usr/bin/env bash
# TPM entry point: binds the tmux-home popup and starts the daemon.
#
#   set -g @home-keys '.'     # prefix keys to bind (default "."; '' = none)
#   set -g @home-sidebar-keys 'e E'  # prefix keys: toggle the sidebar in the
#                             # window, then in every window of the session
#                             # (default "e E"; '' = none; '-' skips one)
#
# The popup and daemon are the Rust binary, target/release/tmux-home. If it
# isn't built yet, the key says so and loading the plugin starts
# `cargo build --release` in the background (one at a time: a reload while
# it runs starts nothing; output in target/build.log).
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

# Shell-quotes $1 for a run-shell command. run-shell expands formats in its
# command before /bin/sh sees it, so a literal `#` (legal in a path) is
# doubled to survive that expansion.
q() {
	local s
	s=$(printf '%q' "$1")
	printf '%s' "${s//\#/##}"
}

# A missing binary is built in the background unless TMUX_HOME_BIN names it
# (tests) or there is no cargo. BUILD_LOCK is held (a directory, created
# atomically) while a build runs, so reloading the config meanwhile doesn't
# start another; it records the build's pid and start time, and a lock whose
# build died (e.g. killed with the server) is taken over.
BUILD_LOG="$CURRENT_DIR/target/build.log"
BUILD_LOCK="$CURRENT_DIR/target/.building"
cargo=
if [[ ! -x $RUST_BIN && -z ${TMUX_HOME_BIN:-} ]]; then
	cargo=$(command -v cargo || true)
fi
# ASCII only: printf %q would $'...'-quote anything else, which run-shell's
# /bin/sh (dash on Linux) can't read
if [[ -n ${TMUX_HOME_BIN:-} ]]; then
	missing="tmux-home: not built yet ($RUST_BIN is not executable)"
elif [[ -n $cargo ]]; then
	missing="tmux-home: not built yet; building in the background (log: $BUILD_LOG), try again in a minute"
else
	missing="tmux-home: not built yet and cargo is not on PATH; install Rust, then run cargo build --release in $CURRENT_DIR"
fi

# display-popup does not expand formats in -e, so the binding goes through
# run-shell, which expands #{client_name} for the client that pressed the key.
# The popup is pinned to that client (-c) and told its name.
[[ ${TMUX_CMD[0]} == */* ]] || TMUX_CMD[0]=$(command -v "${TMUX_CMD[0]}")
tmux_q=
for w in "${TMUX_CMD[@]}"; do tmux_q+="$(q "$w") "; done
popup="${tmux_q}display-popup -c #{q:client_name} -E -B -w 100% -h 100%"
popup+=" -e TMUX_HOME_CLIENT=#{q:client_name} "
rust_q=$(q "$RUST_BIN")
# Which binary runs is decided when the key is pressed, so a build that
# finishes later (first install, TPM update) is used without a reload.
cmd="if [ -x $rust_q ]; then $popup$rust_q popup; else ${tmux_q}display-message -c #{q:client_name} $(q "$missing"); fi"

for key in $keys; do
	t bind-key "$key" run-shell -b "$cmd"
done

# Sidebar toggles: the first key toggles the window's sidebar, the second
# every window of the session. The window is named by session and window
# ID, as run-shell expands them for the client that pressed the key.
if [[ -n $(t show-options -gq @home-sidebar-keys) ]]; then
	sidebar_keys=$(t show-options -gqv @home-sidebar-keys)
else
	sidebar_keys='e E'
fi
read -r -a sk <<<"$sidebar_keys"
toggle="$rust_q sidebar-toggle --socket #{q:socket_path} --window #{q:session_id}:#{q:window_id}"
sidebar_cmd() {
	printf '%s' "if [ -x $rust_q ]; then $toggle$1; else ${tmux_q}display-message -c #{q:client_name} $(q "$missing"); fi"
}
if [[ -n ${sk[0]:-} && ${sk[0]} != - ]]; then
	t bind-key "${sk[0]}" run-shell -b "$(sidebar_cmd '')"
fi
if [[ -n ${sk[1]:-} && ${sk[1]} != - ]]; then
	t bind-key "${sk[1]}" run-shell -b "$(sidebar_cmd ' --session')"
fi

if [[ -x $RUST_BIN ]]; then
	# The daemon: one per server; a second start is a no-op (lock), and it
	# exits with the server.
	t run-shell -b "$rust_q daemon --socket #{q:socket_path}"
elif [[ -n $cargo ]]; then
	# First load without a build (spec section 10): build in the background.
	mkdir -p "$CURRENT_DIR/target"
	if ! mkdir "$BUILD_LOCK" 2>/dev/null; then
		# Held by a live build: its pid runs and started when the build's did
		# (a pid alone can be reused). Or by one just starting (no start time
		# yet) unless that start is over a minute old (its job never ran).
		pid=$(cat "$BUILD_LOCK/pid" 2>/dev/null || true)
		start=$(cat "$BUILD_LOCK/start" 2>/dev/null || true)
		if [[ -n $pid && -n $start && $(ps -o lstart= -p "$pid" 2>/dev/null) == "$start" ]]; then
			exit 0
		elif [[ -z $start && -z $(find "$BUILD_LOCK" -maxdepth 0 -mmin +1) ]]; then
			exit 0
		fi
		rm -rf "$BUILD_LOCK"
		mkdir "$BUILD_LOCK" 2>/dev/null || exit 0
	fi
	lock_q=$(q "$BUILD_LOCK")
	t run-shell -b "echo \$\$ >$lock_q/pid; ps -o lstart= -p \$\$ >$lock_q/start; cd $(q "$CURRENT_DIR") && $(q "$cargo") build --release >$(q "$BUILD_LOG") 2>&1; rm -rf $lock_q"
fi
