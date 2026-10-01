# R0: control-mode spike — results and source decision

Date: 2026-10-01 · tmux 3.7c · macOS (Darwin 25.5) · branch `rust-daemon`

**Decision: `Poll` is the default `--source` for `daemon`.** Control mode
passes size, latency and (after a fix) `detach-on-destroy`, but it fails the
`session_attached` criterion: the control client is a real, attached client to
tmux, and other tools in this setup read that. No attach mode hides it.
`--source control` stays available, with the fixes below.

## Criteria

| Criterion | Result | Evidence |
| --- | --- | --- |
| `session_attached` of the joined session unchanged, **or** corrected by tmux-home's snapshot and nothing else user-visible depends on it | **FAIL** | `main attached=1` becomes `attached=2` (run 1). tmux-home's own snapshot corrects it (the `control-mode` filter in `read_snapshot`), but other things read it: (a) `tmux ls` shows `(attached)` on a session with no real client (session `1` in run 2). (b) tmux-agent-sidebar's `cmd_auto_close` (`src/cli/toggle.rs`, `should_kill_window`) kills the last window only when `session_attached <= 1`, so the control client changes sidebar auto-close in whichever session it joins. (c) `client-attached` and `client-session-changed` hooks fire for it on every attach, and `client-detached`/`session-closed` fire too (hook log). (d) Until the user's next keystroke after an attach, the control client is tmux's "best client": `display -p '#{client_name}'` from outside, and from a pane inside the session, returns the control client, and `switch-client -t other` with no `-c` switches the control client, not the user's (probe 3). |
| Window size of that session unchanged (the 120x30 client still determines it) | PASS | `size=120x29` before and with the control client (status line takes 1 row). `ignore-size` works. |
| Other-session changes seen within 300 ms | PASS | Spike: 17.2 ms (run 1), 35.9 ms (run 2, which also checks that the snapshot contains the new window). Task 6 `control_sees_other_sessions_and_renames` passes (control client on `alpha`, add and rename in `beta`, each within 300 ms). Re-run for this spike: 3/3 passes. |
| `detach-on-destroy` doesn't silently kill the source | PASS **after fix** | Before the fix, killing the joined session (or `detach-client -s`) made the source report `Gone` while the server was still alive (RED tests below). After the fix, the source keeps running: on `kill-session` the same client moves to another session; on `detach-client` it re-attaches as a new client (run 2). |

## Fixes implemented in `control()` (src/tmux/source.rs)

1. Attach with `-f no-output,ignore-size,read-only,no-detach-on-destroy`. When
   the joined session is destroyed, tmux moves the control client to another
   session instead of detaching it.
2. Re-attach loop. When the control client ends (`%exit` or EOF), the source
   checks `has-session`. While the server is alive it re-attaches after 100 ms.
   It reports `Gone` only when the server (or its last session) is gone. This
   covers `detach-client`, and `attach -d` from another client, which detaches
   every other client on the session, control clients included.

Tests in `tests/source.rs`: `control_survives_its_session_being_killed` and
`control_survives_being_detached`.

### Fixes considered for `session_attached` and rejected

- **Sessionless control client.** I tried `tmux -C list-sessions` with stdin
  held open. The client prints the command's result and then `%exit`. It never
  shows in `list-clients` and gets no notifications, so it can't watch anything
  (probe 4).
- **A dedicated hidden session (e.g. `_tmux-home`).** This keeps
  `session_attached` correct for the user's own sessions. tmux has no hidden
  sessions, though, so the extra session would show up instead in `tmux ls`,
  `choose-tree`, `switch-client -n/-p`, tmux-fzf's session list and the bash
  popup's `list-panes -a`. tmux-resurrect would save and restore it, and the
  hooks and best-client problems above would remain. That swaps one visible
  side effect for several.
- **A `-f` flag that excludes the client from `session_attached`.** None
  exists. The full flag list in tmux 3.7c is `active-pane`, `ignore-size`,
  `no-detach-on-destroy`, `no-output`, `pause-after`, `read-only` and
  `wait-exit`.

What `Poll` costs: change latency is up to 500 ms instead of about 35 ms, and
each tick runs `read_snapshot`, which is two `tmux` invocations.

## Raw output

### Run 1: brief Step 2, as written (original flags `no-output,ignore-size,read-only`)

```
== before ==
sessions:
main attached=1 size=120x29
clients:
/dev/ttys007 attached,focused,UTF-8 main 120x30
detach-on-destroy: on

== with control client ==
sessions:
main attached=2 size=120x29
clients:
/dev/ttys007 attached,focused,UTF-8 main 120x30
client-32206 attached,focused,control-mode,ignore-size,no-output,read-only,UTF-8 main 80x
detach-on-destroy: on

other-session change seen after 17.173375ms
```

(The outer server ran `env -u TMUX tmux -L th-spike attach -t main`. The
`env -u TMUX` is needed because the outer pane sets `$TMUX` and a nested attach
refuses to run with it.)

### Run 2: extended spike after the fixes, with hook logging

The spike adds `tmux ls`, best-client resolution, a latency check that waits for
the snapshot to actually contain the new window, and kill and detach phases.
Driver script: a throwaway `th-spike` server with hooks
`client-attached client-detached client-session-changed session-closed` →
`run-shell 'echo <hook> #{client_name} #{client_flags} #{session_name} >> log'`,
and an outer `th-spike-outer` 120x30 client on `main`.

```
hooks after outer attach:
client-session-changed /dev/ttys007 attached,focused,UTF-8 main
client-attached /dev/ttys007 attached,focused,UTF-8 main
== before ==
sessions:
main attached=1 size=120x29
clients:
/dev/ttys007 attached,focused,UTF-8 main 120x30
detach-on-destroy: on
tmux ls:
main: 1 windows (created Thu Oct  1 12:11:10 2026) (attached)
display -p #{client_name} (no client context): /dev/ttys007

== with control client ==
sessions:
main attached=2 size=120x29
clients:
/dev/ttys007 attached,focused,UTF-8 main 120x30
client-44381 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 main 80x
detach-on-destroy: on
tmux ls:
main: 1 windows (created Thu Oct  1 12:11:10 2026) (attached)
display -p #{client_name} (no client context): client-44381

control client (name session): client-44381 main
other-session change seen after 35.850542ms

== kill-session -t main (control client: client-44381 main) ==
source still running
control client after: client-44381 1
sessions:
1 attached=1 size=80x24
clients:
client-44381 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 1 80x
detach-on-destroy: on
tmux ls:
1: 2 windows (created Thu Oct  1 12:11:13 2026) (attached)
display -p #{client_name} (no client context): client-44381

== detach-client -s 1 (control client: client-44381 1) ==
source still running
control client after: client-44934 1

== hooks fired during spike ==
client-session-changed client-44381 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 main
client-attached client-44381 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 main
client-session-changed client-44381 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 1
session-closed client-44381 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 1
client-detached client-44381 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 1
client-detached 1
client-session-changed client-44934 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 1
client-attached client-44934 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 1
client-detached 1
```

Session `1` (created by the spike, no real client) shows `(attached)` with
`attached=1` purely because of the control client. Killing `main` also detached
the real outer client. That is normal `detach-on-destroy` behaviour for it.

### RED: source behaviour before the `control()` fixes

```
test control_survives_being_detached ... FAILED
test control_survives_its_session_being_killed ... FAILED
thread 'control_survives_being_detached' panicked at tests/source.rs:124:47:
thread 'control_survives_its_session_being_killed' panicked at tests/source.rs:124:47:
```

`tests/source.rs:124` is the `"source reported Gone while the server is alive"`
panic. After the fixes, `cargo test --test source` reports 5 passed (3 repeat
runs).

### Probe 3: default ("best") client with a control client on the same session

```
clients:
  /dev/ttys011 attached,focused,UTF-8 main
  client-50016 attached,focused,control-mode,ignore-size,no-detach-on-destroy,no-output,read-only,UTF-8 main
from outside, display -p: client-50016
from a pane in main ($TMUX set), display -p: client-50016
after 'switch-client -t other' from outside:
  /dev/ttys011 main
  client-50016 other
```

After one keystroke in the real client:

```
before user input, display -p: client-51101
after one keystroke in the real client, display -p: /dev/ttys007
from a pane in main after keystroke: /dev/ttys007
```

So the hijack is transient, but it happens again on every (re-)attach. That
means daemon start, which happens at tmux startup, and every re-attach.

### Probe 4: control client without a session

```
$ (printf 'refresh-client -f …\nrefresh-client -B …\n'; sleep 2) | tmux -L th-spike-nosess -C list-sessions
main: 1 windows (created Thu Oct  1 12:11:53 2026)      <- list-clients (none listed) then ls
other: 1 windows (created Thu Oct  1 12:11:53 2026)
%begin 1790853114 290 0
main: 1 windows (created Thu Oct  1 12:11:53 2026)
other: 1 windows (created Thu Oct  1 12:11:53 2026)
%end 1790853114 290 0
%exit
```

No notifications arrived for the window add, rename or title change made while
it was "open".
