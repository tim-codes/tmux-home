# R1 — Rust Popup Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the bash/fzf popup with a Rust `tmux-home popup` (ratatui) that has every M0/M1 behaviour and the persistent close/reopen stack. `prefix .` opens it. The bash popup and its suite are deleted once every parity row below is green.

**Architecture:** The popup is a thin client of the R0 daemon. It subscribes to the daemon's snapshot stream; if the daemon is down, it polls tmux directly and starts a daemon in the background. All popup logic sits in a pure state machine (`popup::app::App`): keys and snapshots go in, `Effect`s come out. `popup::exec` performs those effects through a small `tmux::ops::Ops` trait and reports results back. `popup::view` draws the `App` with ratatui (tested with `TestBackend` and `insta`). The closed-window stack is `store::ClosedStack` (`closed.json`, 10-deep LIFO, atomic writes under `state.lock`), shared by the popup and `tmux-home reopen`. End-to-end tests run the real popup in `display-popup` on a throwaway server and read it back with `capture-pane`, the same way the bash suite did.

**Tech Stack:** Rust stable (edition 2024), tokio, ratatui **0.30.2** (ratatui-core 0.1.2, ratatui-widgets 0.3.2), crossterm **0.29.0** (the version ratatui 0.30 re-exports, so one copy in the tree), nucleo-matcher **0.3.1**, ansi-to-tui **8.0.1** (depends on ratatui-core 0.1), insta **1.48.0**, regex (dev), tempfile **3.27.0** (dev), tmux 3.7c. The versions are what `cargo add` resolved on 2026-10-01. Every API this plan calls was checked against those versions' sources in `~/.cargo/registry/src/index.crates.io-*/`. If `cargo add` resolves a different minor version, re-check the calls named in that task against the new sources before writing code.

**Spec:** `docs/superpowers/specs/2026-09-29-rust-daemon-design.md` (§3 protocol, §4 degraded mode, §7 popup, §9 persistence, §10 install, §12 testing, §13 R1) and `docs/SPEC.md` §3–§6 (UX, layout, key map, management semantics). The bash reference implementation is `bin/tmux-home` + `tmux-home.tmux`, and its suite `tests/run` (143 checks) is the parity checklist (see the table below). Read the spec sections and `bin/tmux-home` before starting.

## Global Constraints

- tmux ≥ 3.3 (developed and tested on 3.7c). Every tmux call in code targets an explicit socket (`Tmux::new(socket)` runs `tmux -S <socket>`).
- Tests use only throwaway servers (`tmux -L th-test-*` / `th-outer-*` with `-f /dev/null`) and temp `TMUX_HOME_STATE_DIR` / `TMUX_HOME_RUNTIME_DIR` (`common::TestEnv`). Never the default server, never the real state dir.
- Launch: `display-popup -E -B -w 100% -h 100%` pinned to the invoking client (`-c #{client_name}`), which is passed in as `-e TMUX_HOME_CLIENT=#{client_name}`. Every switch, and every notion of "current", uses that client and never tmux's best-client guess.
- Every write targets IDs (`@n`, `%n`, `$n`), never indexes or names.
- Sidebar panes (`@pane_role=sidebar` from tmux-agent-sidebar, `@home_role=sidebar` from R4) are never listed, counted, previewed, close-checked or included in a reopen snapshot.
- Closed stack: `<state_dir>/closed.json`, where `state_dir = ${TMUX_HOME_STATE_DIR:-${XDG_STATE_HOME:-~/.local/state}/tmux-home}/<sha1(socket_path)[..12]>`. It is a 10-deep LIFO, written atomically (write + rename) under `<state_dir>/state.lock`.
- Degraded mode: the daemon gets a 150 ms connect budget. On failure the popup reads tmux itself (`read_snapshot`, every 500 ms) and starts a daemon in the background.
- Preview: right half at ≥ 160 columns, bottom third at ≥ 30 rows, otherwise hidden (`^o` then shows it at the bottom, half height). The layout is recomputed on resize.
- Colours: the terminal's default fg/bg plus the 16 named ANSI colours only (SPEC §9). Never RGB or indexed colours in our own styling.
- `@home-keys` defaults to `.` and `''` binds nothing. The popup binary is `${TMUX_HOME_BIN:-<plugin dir>/target/release/tmux-home}`.
- Add dependencies with `cargo add` only. Never hand-write a version.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass at every commit. `tests/run` keeps passing until Task 16 deletes it.
- Nothing may need an interactive prompt (the operator is remote). insta snapshots are accepted with `INSTA_UPDATE=always` and reviewed with `git diff`; never with `cargo insta review`.
- Commit with `timeout 60 git commit …` (signing goes through 1Password; a hung signer must not wedge the session). Commit after every task. Push only where a step says so.

## Review Focus

1. **Window names containing `#`** (`fix #12`, `#{session_name}`). tmux format-expands names given to `rename-window` and `new-window -n` (probed on 3.7c: `#{session_name}` becomes `a`, and `ab#` loses its `#`). The name should be stored exactly as typed. Tests: Task 4 `rename_keeps_hash_literally`, Task 5 `reopen_restores_a_hash_name`, Task 15 e2e `reorder_and_new_window` (`fresh #1`).
2. **A tiny terminal** (a 20×3 popup, a 1-row list, a zero-height preview) should draw without panicking, and PgUp/PgDn should still move by at least one row. Tests: Task 9 `page_keys_move_by_list_height` (page ≥ 1), Task 11 `tiny_terminals_do_not_panic`.
3. **The daemon dying while the popup is open** (killed, version-restarted). The popup should keep updating live by polling tmux itself, and its own writes should still show at once. Test: Task 13 `daemon_death_falls_back_to_polling`.
4. **Reopening a window whose directory is gone** (deleted worktree) should still succeed, with the pane opening in `$HOME` (tmux falls back, probed). Test: Task 5 `reopen_with_missing_cwd_falls_back_to_home`.
5. **A `closed.json` that is corrupt or from a newer version** (hand-edited, partial copy, extra fields). A corrupt file is moved aside and the stack restarts empty. Unknown fields are ignored. Neither case breaks `^x`/`^t`. Tests: Task 3 `corrupt_file_is_moved_aside`, `unknown_fields_are_ignored`.

## Decisions taken in this plan (rulings on spec gaps)

1. **Default key `.`**, not `w f`. Phase-2 spec §10/§15 supersede SPEC §4, and the user's dotfiles already set `@home-keys '.'`. Bash checks 32/33 become "binds `.`" and "leaves `w`/`f` alone".
2. **`M-r` sets `automatic-rename on` explicitly** instead of `set -u`. Unsetting falls back to the global value, which is `off` on test servers and could be `off` for a user. "Back to automatic name" has to work regardless.
3. **Names are escaped for tmux** (`#` → `##`, except before `[`, which tmux keeps literal). The bash popup didn't do this (see Review Focus 1).
4. **All close/reopen store writes happen in the client process** (popup or `tmux-home reopen`) under `state.lock`. R1 adds no daemon `reopen` op: the lock already serialises writers, and the stack layout doesn't change if one is added later.
5. **The daemon gains a `refresh` op.** It reads tmux now, broadcasts, and replies with that snapshot and its `seq`. The popup calls it after each write so its own change shows immediately instead of up to 500 ms later. The `App` ignores snapshots whose `seq` is ≤ the last one applied (`seq 0` = a direct read, always applied).
6. **The bash `closed` file is imported once.** The first time any server's store is opened and has no `closed.json`, the legacy file `<state root>/closed` is imported and renamed to `closed.imported`. The legacy file was server-agnostic, so the user's one real server is the natural owner.
7. **Session headers are non-selectable rows.** `^r` on a session header (session rename, SPEC §6) is deferred: the bash popup didn't have it, so it isn't a parity item.
8. **Inline editors render in the row** (SPEC §6: "row becomes an editor"). The close confirm replaces the filter line (SPEC §6: "the prompt becomes …").
9. **`M-↑`/`M-↓` swap with the adjacent window of the same session in index order**, whatever the filter. At a session's edge they do nothing.
10. **`M-n`:** `Esc` cancels, and an empty name creates the window with its automatic name. The filter is cleared so the new row is visible (same as `^t`). `M-↑↓` and `M-n` are in R1 per this milestone's brief, although spec §13 lists them under R3.
11. **Pushed snapshots keep applying while a confirm is open** (SPEC §8 said "pause"). Nothing in the prompt depends on the list, and if its target vanishes the prompt is cancelled with a notice.
12. **`^t` pops the stack only after a successful rebuild.** The bash popup popped first, so a failed rebuild lost the entry.
13. **After the bash popup is deleted (Task 16),** a missing release binary makes the key show `tmux-home: building …`. Loading the plugin starts `cargo build --release` in the background (spec §10).
14. **The filter matches** "session index name command short-path", the same columns the bash popup let fzf search.
15. **The popup starts from a read made at open** (a daemon `refresh`), not from the daemon's last push, which can be one poll (500 ms) old. The cursor starts on the client's *current* window, and with a stale snapshot `^x` closed the wrong one (found while checking this plan against real tmux; pinned by `tests/feed.rs::daemon_feed_starts_from_a_fresh_read`).

---

## File structure

```
Cargo.toml                    + nucleo-matcher, crossterm, ratatui, ansi-to-tui, clap/env;
                                dev: tempfile, insta, regex
tmux-home.tmux                default key `.`; Rust popup when built (bash fallback until Task 16)
src/lib.rs                    + pub mod model, popup, store
src/main.rs                   + `popup`, `reopen` subcommands
src/paths.rs                  + Paths.legacy_closed
src/model.rs          (new)   pure: is_shell, busy, client_location, ClosePlan/close_plan, confirm_prompt
src/store.rs          (new)   ClosedWindow, ClosedStack (closed.json, state.lock, atomic, legacy import)
src/tmux/snapshot.rs          + Pane.sidebar, Client.window_id
src/tmux/ops.rs       (new)   Ops trait + TmuxOps (switch, rename, reset, busy, swap, new window,
                                close, reopen), capture_pane, literal_name, reopen_cli
src/tmux/shape.rs     (new)   capture a window's shape; rebuild it (port of bash snapshot/reopen)
src/ipc.rs                    + Request::Refresh
src/daemon.rs                 + refresh op (read now, broadcast, reply)
src/client.rs                 + subscribe(), Subscription, refresh()
src/popup/mod.rs      (new)   module list + run(): terminal, event loop
src/popup/rows.rs     (new)   pure: WinRow, build(), short_path()
src/popup/filter.rs   (new)   nucleo fuzzy filter, row order kept
src/popup/edit.rs     (new)   LineEdit: one-line editor (filter, rename, new-window name)
src/popup/layout.rs   (new)   preview placement + screen regions
src/popup/app.rs      (new)   App state machine, Effect, Mode, ViewItem
src/popup/view.rs     (new)   ratatui drawing + HELP
src/popup/exec.rs     (new)   execute Effects through Ops, report back to App
src/popup/feed.rs     (new)   daemon subscription or degraded polling; refresh
src/popup/testutil.rs (new)   unit-test fixtures (cfg(test))
tests/common/mod.rs           + start_in, default-shell /bin/sh, Outer harness, popup_fixture, helpers
tests/snapshot.rs             + sidebar/client-window tests
tests/ops.rs          (new)   Ops against a real server
tests/close_reopen.rs (new)   close/reopen/stack/CLI against a real server
tests/daemon.rs, tests/client.rs   + refresh/subscribe tests
tests/feed.rs         (new)   Feed in daemon and degraded modes
tests/plugin.rs       (new)   tmux-home.tmux bindings
tests/popup_e2e.rs    (new)   real popup: open, filter, rename, M-r, ^o, F1, ⏎, Esc, layout
tests/popup_manage_e2e.rs (new) real popup: close, confirm, reopen, last window, reorder, new, live
bin/tmux-home, tests/run      deleted in Task 16
README.md                     rewritten for R1 in Task 16
```

---

## Parity table (bash `tests/run` → Rust)

Every row must be green before Task 16 deletes `tests/run`. "e2e" tests run the real popup on a throwaway server (`tests/popup_e2e.rs`, `tests/popup_manage_e2e.rs`). Unit tests live in `src/…` (`mod tests`). Module paths are relative to `src/`, and `tests/x.rs::f` is an integration test.

| # | bash check | Rust test(s) that replace it |
| --- | --- | --- |
| 1 | list: one row per window | `popup/rows.rs::one_row_per_window` |
| 2 | list: no client -> sessions by name, windows by index | `popup/rows.rs::no_client_sessions_by_name_windows_by_index` |
| 3 | list: rows carry session and window IDs | `popup/rows.rs::rows_carry_session_and_window_ids` |
| 4 | list: sidebar pane never chosen as the row pane | `popup/rows.rs::sidebar_pane_never_the_row_pane`, `tests/snapshot.rs::sidebar_panes_are_flagged` |
| 5 | preview: captures the non-sidebar pane | `tests/ops.rs::capture_shows_the_main_pane_not_the_sidebar`, e2e `opens_grouped_with_header_and_side_preview` |
| 6 | preview: does not capture the sidebar pane | same two tests |
| 7 | rename: by ID, name with - ( ) + , quote | `tests/ops.rs::rename_by_id_keeps_odd_characters` |
| 8 | rename: turns automatic-rename off | `tests/ops.rs::rename_turns_automatic_rename_off` |
| 9 | reset-name: automatic-rename back on | `tests/ops.rs::reset_turns_automatic_rename_back_on` |
| 10 | busy: idle shell -> nothing | `tests/ops.rs::busy_idle_shell_is_empty`, `model.rs::busy_*` |
| 11 | busy: lists each non-shell command | `tests/ops.rs::busy_lists_each_non_shell_command` |
| 12 | busy: sidebar panes do not count | `tests/ops.rs::busy_ignores_sidebar_panes`, `model.rs::busy_ignores_sidebar` |
| 13 | close: last window on the server refused (exit 3) | `tests/close_reopen.rs::close_refuses_last_window_on_server`, `popup/app.rs::last_window_on_server_is_refused` |
| 14 | close: ... and the window survives | `tests/close_reopen.rs::close_refuses_last_window_on_server` |
| 15 | close pushes a snapshot | `tests/close_reopen.rs::close_then_reopen_restores_shape` |
| 16 | ... and the window is gone | same |
| 17 | reopen prints the new window ID | same (`reopen()` returns it), `tests/close_reopen.rs::reopen_cli_prints_the_new_window_id` |
| 18 | reopen restores index, name, panes, layout, cwds, active pane | `tests/close_reopen.rs::close_then_reopen_restores_shape` |
| 19 | reopen pops the stack | same |
| 20 | reopen keeps automatic-rename on | `tests/close_reopen.rs::reopen_keeps_automatic_rename` |
| 21 | reopen with its index taken goes after its old neighbour | `tests/close_reopen.rs::reopen_after_old_neighbour_when_index_taken` |
| 22 | closing the last window ends the session | `tests/close_reopen.rs::reopen_recreates_ended_session` |
| 23 | reopen recreates the session with the window | same |
| 24 | stack keeps the last 10 closes | `tests/close_reopen.rs::stack_is_lifo_capped_at_ten`, `store.rs::keeps_the_last_ten` |
| 25 | reopen is LIFO | `tests/close_reopen.rs::stack_is_lifo_capped_at_ten` |
| 26 | reopen on an empty stack exits 4 | same (CLI) |
| 27 | ... and creates nothing | same |
| 28 | layout: >=160 cols -> right | `popup/layout.rs::preview_layout_by_size`, `popup/view.rs::side_preview_border_at_200_cols` |
| 29 | layout: <160 cols, >=30 rows -> bottom | `popup/layout.rs::preview_layout_by_size`, `popup/view.rs::bottom_preview_at_150x40` |
| 30 | layout: small -> hidden | `popup/layout.rs::preview_layout_by_size`, `popup/view.rs::small_terminal_hides_preview_until_ctrl_o` |
| 31 | @home-keys '' binds nothing | `tests/plugin.rs::empty_home_keys_binds_nothing` |
| 32 | default binds prefix w | `tests/plugin.rs::default_binds_prefix_dot_to_the_rust_popup` (ruling 1) |
| 33 | default binds prefix f | `tests/plugin.rs::default_binds_prefix_dot_to_the_rust_popup` (`w`,`f` left alone), `tests/plugin.rs::custom_keys_bind_each` |
| 34 | client attached to test server | `common::Outer::attach` (waits for it), asserted in e2e `opens_grouped_with_header_and_side_preview` |
| 35 | popup opens via prefix w | e2e `opens_grouped_with_header_and_side_preview` (`prefix .`) |
| 36 | header shows client location | same, `popup/app.rs::header_follows_client` |
| 37 | list grouped: client session (alpha) first, then beta | same, `popup/rows.rs::client_session_first`, `popup/app.rs::view_items_group_by_session` |
| 38 | current window marked | same, `popup/rows.rs::current_window_marked` |
| 39 | preview shown on the right at 200 cols | same, `popup/view.rs::side_preview_border_at_200_cols` |
| 40 | typing filters (1 of 4) | e2e `filter_esc_and_navigation`, `popup/app.rs::typing_filters_and_returns_to_top` |
| 41 | filter kept only beta build | e2e `filter_esc_and_navigation`, `popup/filter.rs::fuzzy_keeps_row_order` |
| 42 | Esc clears a non-empty filter | e2e `filter_esc_and_navigation`, `popup/app.rs::esc_clears_then_quits` |
| 43 | popup still open after clearing | e2e `filter_esc_and_navigation` |
| 44 | navigation keys accepted (^n ^j ↓ ^p ^k ↑) | e2e `filter_esc_and_navigation` (each key's effect asserted), `popup/app.rs::navigation_keys_cycle` |
| 45 | ^r opens editor pre-filled with current name | e2e `rename_inline`, `popup/app.rs::ctrl_r_prefills_and_enter_renames_by_id` |
| 46 | ^r ⏎ renames the target window by ID | same two |
| 47 | fzf still running after rename (list mode prompt) | e2e `rename_inline` |
| 48 | list shows the new name | e2e `rename_inline` |
| 49 | ^r pre-fills a name with ( ) + , | e2e `rename_inline` |
| 50 | Esc in editor returns to list mode | e2e `rename_inline`, `popup/app.rs::rename_esc_cancels` |
| 51 | Esc in editor leaves name unchanged | same two |
| 52 | editor reopened | e2e `rename_inline` |
| 53 | empty name returns to list mode | e2e `rename_inline`, `popup/app.rs::rename_empty_cancels` |
| 54 | empty name leaves name unchanged | same two |
| 55 | filter with ( ) + | e2e `rename_inline`, `popup/filter.rs::special_characters_are_literal` |
| 56 | editor opens from that filter | e2e `rename_inline` |
| 57 | filter with ( ) + restored after Esc | e2e `rename_inline`, `popup/app.rs::filter_survives_rename` |
| 58 | filter cleared again | e2e `rename_inline` |
| 59 | filter before rename | e2e `rename_inline` |
| 60 | editor on the filtered window | e2e `rename_inline`, `popup/app.rs::filter_survives_rename` |
| 61 | rename of filtered selection hits the right window | same two |
| 62 | filter restored after rename | same two |
| 63 | filter cleared | e2e `rename_inline` |
| 64 | before M-r: automatic-rename off | e2e `reset_preview_toggle_and_help` |
| 65 | M-r: automatic-rename back on (by ID) | e2e `reset_preview_toggle_and_help`, `popup/app.rs::alt_r_resets_selected` |
| 66 | popup still open after M-r | e2e `reset_preview_toggle_and_help` |
| 67 | ^o hides the preview | e2e `reset_preview_toggle_and_help`, `popup/app.rs::ctrl_o_toggles_preview` |
| 68 | ^o shows it again | same two |
| 69 | F1 shows help | e2e `reset_preview_toggle_and_help` (F1 and `^/`), `popup/app.rs::help_opens_and_any_key_returns`, `popup/view.rs::help_screen` |
| 70 | help returns to the list | same |
| 71 | popup closed before close tests | e2e `enter_switches_and_esc_closes` (each e2e test now opens its own popup on its own server) |
| 72 | popup opens on the client current window (qcur) | e2e `close_idle_current_and_cursor`, `popup/app.rs::list_starts_on_client_window` |
| 73 | header shows qcur | e2e `close_idle_current_and_cursor` |
| 74 | ^x on the client's current (idle) window closes it at once | e2e `close_idle_current_and_cursor`, `popup/app.rs::close_idle_is_immediate_unconfirmed`, `popup/exec.rs::idle_close_rechecks_then_closes` |
| 75 | ... qcur is gone | e2e `close_idle_current_and_cursor` |
| 76 | ... server still up, client still attached | same |
| 77 | ... client moved to another alpha window | same |
| 78 | ... header follows the client | same, `popup/app.rs::header_follows_client` |
| 79 | ... popup still open | e2e `close_idle_current_and_cursor` |
| 80 | filter to qone | same |
| 81 | ^x closes an idle window at once, filter kept | same, `popup/app.rs::filter_kept_after_close` |
| 82 | ... qone is gone | e2e `close_idle_current_and_cursor` |
| 83 | filter cleared after close | same |
| 84 | ^x leaves the cursor on the next row (preview shows qnext) | same, `popup/app.rs::cursor_moves_to_next_row_after_close` |
| 85 | ... qtwo closed | e2e `close_idle_current_and_cursor` |
| 86 | ... qtwo is gone | same |
| 87 | rename editor open on qnext | same |
| 88 | ^x ignored during rename: editor still open | same, `popup/app.rs::list_keys_ignored_while_renaming` |
| 89 | ^x ignored during rename: window kept | same two |
| 90 | back to the list | e2e `close_idle_current_and_cursor` |
| 91 | filter to qbusy | e2e `close_asks_when_busy` |
| 92 | ^x on a busy window asks, naming the command | same, `popup/app.rs::close_busy_asks_with_commands`, `model.rs::close_plan_busy_asks` |
| 93 | n cancels, filter restored | e2e `close_asks_when_busy`, `popup/app.rs::confirm_cancels_on_anything_but_y` |
| 94 | ... window kept after n | e2e `close_asks_when_busy` |
| 95 | confirm again | same |
| 96 | Esc cancels, filter restored | same, `popup/app.rs::confirm_cancels_on_anything_but_y` |
| 97 | ... window kept after Esc | e2e `close_asks_when_busy` |
| 98 | confirm again (⏎) | same |
| 99 | ⏎ cancels (default N) | same, `popup/app.rs::confirm_cancels_on_anything_but_y` |
| 100 | ... window kept after ⏎ | e2e `close_asks_when_busy` |
| 101 | ... popup still open | same |
| 102 | confirm again (y) | same |
| 103 | y closes it, filter restored | same, `popup/app.rs::confirm_y_closes` |
| 104 | ... qbusy is gone | e2e `close_asks_when_busy` |
| 105 | filter to qside | same |
| 106 | window whose only program is a sidebar closes at once | same, `popup/app.rs::sidebar_only_activity_closes_at_once`, `model.rs::close_plan_ignores_sidebar` |
| 107 | filter to qnext | e2e `reopen_restores_and_selects` (setup closes) |
| 108 | qnext closed | same |
| 109 | back to the four fixture windows | same |
| 110 | ^t reopens the last closed window | same, `popup/app.rs::reopened_window_selected_and_filter_cleared` |
| 111 | ... qnext is back | e2e `reopen_restores_and_selects` |
| 112 | ... client not switched | same |
| 113 | ... cursor is on it | same, `popup/app.rs::reopened_window_selected_and_filter_cleared` |
| 114 | back to list | e2e `reopen_restores_and_selects` |
| 115 | filter to build | same |
| 116 | ^t from a filter clears it and reopens qside | same, `popup/app.rs::reopened_window_selected_and_filter_cleared` |
| 117 | ... cursor is on qside | e2e `reopen_restores_and_selects` |
| 118 | re-close qnext | same |
| 119 | re-close qside | same |
| 120 | back to the four fixture windows again | same |
| 121 | ^t with nothing closed: footer notice | same, `popup/app.rs::nothing_to_reopen_notice_clears_on_key`, `popup/exec.rs::reopen_reports_back` |
| 122 | ... no window created | e2e `reopen_restores_and_selects` |
| 123 | ... notice goes once you type | same, `popup/app.rs::nothing_to_reopen_notice_clears_on_key` |
| 124 | list mode, 4/4 | e2e `reopen_restores_and_selects` |
| 125 | popup closed | e2e `enter_switches_and_esc_closes` (each e2e test opens its own popup) |
| 126 | popup opens in the one-window session qdelta | e2e `close_last_window_of_client_session` |
| 127 | header shows qdelta | same |
| 128 | ^x on the session's last window warns the session will end | same, `popup/app.rs::last_window_of_session_warns`, `model.rs::close_plan_last_in_session_warns` |
| 129 | y closes it; list back to the fixture | e2e `close_last_window_of_client_session` |
| 130 | ... session qdelta is gone | same |
| 131 | ... server still up, client still attached | same |
| 132 | ... client moved to another session | same |
| 133 | ... header follows the client | same |
| 134 | ... popup still open | same |
| 135 | filtered to beta build | e2e `enter_switches_and_esc_closes` |
| 136 | popup closes on ⏎ | same, `popup/app.rs::enter_switches_invoking_client_and_quits`, `popup/exec.rs::switch_then_quit` |
| 137 | ⏎ switched the invoking client to beta:build | e2e `enter_switches_and_esc_closes` |
| 138 | popup reopens | same |
| 139 | header follows the client (beta ▸ 1) | same |
| 140 | client session (beta) listed first | same, `popup/rows.rs::client_session_first` |
| 141 | Esc on empty filter closes the popup | same, `popup/app.rs::esc_clears_then_quits` |
| 142 | popup opens at 150x40 | e2e `layout_150x40_has_no_side_preview` |
| 143 | no side preview at 150 cols | same, `popup/view.rs::bottom_preview_at_150x40` |

These are R1 behaviours with no bash check. Each has its own test: `M-↑↓` and `M-n` (e2e `reorder_and_new_window`, `popup/app.rs::alt_*`), live redraw that leaves an open editor alone (e2e `live_redraw_keeps_the_editor`, `popup/app.rs::snapshot_never_touches_open_editor`), `closed.json` + legacy import (`store.rs`), daemon `refresh` (`tests/daemon.rs`, `tests/client.rs`), and degraded mode (`tests/feed.rs`).

---
### Task 1: Snapshot knows sidebar panes and each client's window

The popup has to ignore sidebar panes and know which window the invoking client is showing. Neither is in R0's snapshot. Test servers also get `/bin/sh` as their default shell (new windows start fast and report a shell; macOS's `/bin/sh` reports as `bash`), plus a `start_in(cwd)` constructor that the e2e fixture uses later. Probed on tmux 3.7c: `new-window` without `-c` uses the cwd of the *`tmux` command that creates it*, not the session's directory, so `start_in` also runs every `TestServer::tmux` command from `cwd`.

**Files:**
- Modify: `src/tmux/snapshot.rs` (structs `Pane`, `Client`; `PANE_FMT`, `CLIENT_FMT`; `pane_record`; `parse`)
- Modify: `tests/common/mod.rs` (`TestServer::start`)
- Test: `tests/snapshot.rs`

**Interfaces:**
- Produces: `Pane.sidebar: bool` (true when `@pane_role` or `@home_role` is `sidebar`) and `Client.window_id: String`. Both are `#[serde(default)]`. `TestServer::start_in(cwd: &str) -> TestServer` (its `tmux()` calls run from `cwd`, so windows they create start there). Test servers have `default-shell /bin/sh`.

- [ ] **Step 1: Write the failing tests** — append to `tests/snapshot.rs`, and replace the `good` record in `unparseable_record_is_skipped` (the pane format grows from 13 to 15 fields):

```rust
#[test]
fn unparseable_record_is_skipped() {
    // 15 fields: … pane_active, @pane_role, @home_role, command, path, session, title, window
    let good = "$0\x1f@0\x1f0\x1f1\x1f0\x1f%0\x1f0\x1f1\x1f\x1f\x1fsh\x1f/\x1falpha\x1ft\x1fw\x1e\n";
    let panes = format!("garbage\x1e\n{good}");
    let snap = tmux_home::tmux::snapshot::parse(&panes, "also garbage\x1e\n");
    assert_eq!(snap.panes.len(), 1);
    assert!(!snap.panes[0].sidebar);
    assert!(snap.clients.is_empty());
}

#[tokio::test]
async fn sidebar_panes_are_flagged() {
    let s = common::TestServer::start();
    s.tmux(&["split-window", "-d", "-t", "alpha:0"]);
    s.tmux(&["split-window", "-d", "-t", "alpha:0"]);
    s.tmux(&["set-option", "-p", "-t", "alpha:0.1", "@pane_role", "sidebar"]);
    s.tmux(&["set-option", "-p", "-t", "alpha:0.2", "@home_role", "sidebar"]);
    let (snap, _) = read_snapshot(&Tmux::new(s.socket.clone())).await.unwrap();
    let flags: Vec<bool> = snap.panes.iter().map(|p| p.sidebar).collect();
    assert_eq!(flags, [false, true, true]);
}

#[test]
fn clients_carry_their_window() {
    let clients = "/dev/ttys001\x1f/dev/ttys001\x1f$1\x1f@4\x1fattached,focused,UTF-8\x1e\n\
                   client-9\x1f\x1f$1\x1f@4\x1fattached,control-mode,read-only\x1e\n";
    let snap = tmux_home::tmux::snapshot::parse("", clients);
    assert_eq!(snap.clients.len(), 1, "control client filtered out");
    assert_eq!(snap.clients[0].session_id, "$1");
    assert_eq!(snap.clients[0].window_id, "@4");
}

#[tokio::test]
async fn new_windows_run_bin_sh() {
    let s = common::TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "fresh"]);
    s.wait_settled();
    let (snap, _) = read_snapshot(&Tmux::new(s.socket.clone())).await.unwrap();
    let w = snap.windows.iter().find(|w| w.name == "fresh").unwrap();
    let p = snap.panes.iter().find(|p| p.window_id == w.id).unwrap();
    // /bin/sh, never the developer's $SHELL (macOS's /bin/sh reports as bash)
    assert!(
        matches!(p.current_command.as_str(), "sh" | "bash" | "dash"),
        "{}",
        p.current_command
    );
}

#[tokio::test]
async fn start_in_sets_the_session_directory() {
    let s = common::TestServer::start_in("/");
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "rooted"]);
    s.wait_settled();
    let (snap, _) = read_snapshot(&Tmux::new(s.socket.clone())).await.unwrap();
    assert!(snap.panes.iter().all(|p| p.current_path == "/"), "{:?}", snap.panes);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --test snapshot`
Expected: compile error: no field `sidebar` on `Pane`, no field `window_id` on `Client`, no function `start_in`.

- [ ] **Step 3: Implement.** In `src/tmux/snapshot.rs`:

Replace the `Pane` and `Client` structs with:

```rust
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pane {
    pub id: String,
    pub window_id: String,
    pub session_id: String,
    pub index: u32,
    pub active: bool,
    /// `@pane_role` (tmux-agent-sidebar) or `@home_role` (tmux-home's own,
    /// R4) is `sidebar`: a monitor, never listed, counted, previewed,
    /// close-checked or included in a reopen snapshot.
    #[serde(default)]
    pub sidebar: bool,
    pub current_command: String,
    pub current_path: String,
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Client {
    pub name: String,
    pub tty: String,
    pub session_id: String,
    /// The window this client is showing.
    #[serde(default)]
    pub window_id: String,
}
```

Replace the two format constants with:

```rust
const PANE_FMT: &str = "#{session_id}\x1f#{window_id}\x1f#{window_index}\x1f#{window_active}\x1f#{automatic-rename}\x1f#{pane_id}\x1f#{pane_index}\x1f#{pane_active}\x1f#{@pane_role}\x1f#{@home_role}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{session_name}\x1f#{pane_title}\x1f#{window_name}\x1e";
const CLIENT_FMT: &str =
    "#{client_name}\x1f#{client_tty}\x1f#{session_id}\x1f#{window_id}\x1f#{client_flags}\x1e";
```

In `pane_record`, change `splitn(13, SEP)` to `splitn(15, SEP)` and `f.len() != 13` to `f.len() != 15` (`window_index` is still `f[2]`, `pane_index` still `f[6]`).

In `parse`, the pane loop's field indexes become: session name `f[12]`, window name `f[14]`, and the pane push becomes:

```rust
        s.panes.push(Pane {
            id: f[5].to_string(),
            window_id: wid,
            session_id: sid,
            index: pane_index,
            active: f[7] == "1",
            sidebar: f[8] == "sidebar" || f[9] == "sidebar",
            current_command: f[10].to_string(),
            current_path: f[11].to_string(),
            title: f[13].to_string(),
        });
```

(in the `Session` push, `name: f[12].to_string()`; in the `Window` push, `name: f[14].to_string()`). Replace the client loop body with:

```rust
        let f: Vec<&str> = rec.splitn(5, SEP).collect();
        if f.len() != 5 {
            eprintln!("tmux-home: skipping unparseable list-clients record: {rec:?}");
            continue;
        }
        if f[4].split(',').any(|x| x == "control-mode") {
            continue; // our own control client, never a user client
        }
        s.clients.push(Client {
            name: f[0].into(),
            tty: f[1].into(),
            session_id: f[2].into(),
            window_id: f[3].into(),
        });
        if let Some(sess) = s.sessions.iter_mut().find(|x| x.id == f[2]) {
            sess.attached += 1;
        }
```

In `tests/common/mod.rs`, replace `TestServer::start` with:

```rust
    /// A throwaway server with one detached 200x50 session "alpha" running /bin/sh.
    pub fn start() -> TestServer {
        TestServer::start_with(&[])
    }

    /// `start`, with session alpha in `cwd` and every `tmux()` call run from
    /// `cwd` (tmux gives a window created without `-c` the cwd of the tmux
    /// command that created it). e2e fixtures use "/": its short path has no
    /// letters the popup's fuzzy filter could match by accident.
    pub fn start_in(cwd: &str) -> TestServer {
        let mut s = TestServer::start_with(&["-c", cwd]);
        s.cwd = Some(PathBuf::from(cwd));
        s
    }

    fn start_with(extra: &[&str]) -> TestServer {
        let name = format!("th-test-{}-{}", std::process::id(), rand_suffix());
        let mut args = vec![
            "-L",
            name.as_str(),
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-x",
            "200",
            "-y",
            "50",
        ];
        args.extend_from_slice(extra);
        args.push("/bin/sh");
        let out = Command::new("tmux")
            .args(&args)
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Automatic window rename lags pane_current_command by a further,
        // separately-timed hook (pane_current_command flips to the real
        // shell within ~100ms, window_name can take several hundred ms
        // more), a second source of startup churn independent of the one
        // wait_settled rides out. Tests that need automatic-rename turn it on
        // themselves. default-shell /bin/sh: new windows start fast and
        // run /bin/sh whatever the developer's $SHELL is.
        for (opt, val) in [("automatic-rename", "off"), ("default-shell", "/bin/sh")] {
            let out = Command::new("tmux")
                .args(["-L", &name, "set-option", "-g", opt, val])
                .env_remove("TMUX")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "set-option {opt}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let socket = Command::new("tmux")
            .args(["-L", &name, "display", "-p", "#{socket_path}"])
            .env_remove("TMUX")
            .output()
            .unwrap();
        let socket = PathBuf::from(String::from_utf8(socket.stdout).unwrap().trim());
        TestServer {
            name,
            socket,
            cwd: None,
        }
    }
```

In the same file, give `TestServer` the field and make `tmux()` use it:

```rust
pub struct TestServer {
    pub name: String,
    pub socket: PathBuf,
    /// Set by `start_in`: `tmux()` runs from here.
    cwd: Option<PathBuf>,
}
```

```rust
    pub fn tmux(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("tmux");
        cmd.arg("-S").arg(&self.socket).args(args).env_remove("TMUX");
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
```

- [ ] **Step 4: Run the whole suite** (R0's tests must still pass with the new fields)

Run: `cargo test`
Expected: all pass, including the four new snapshot tests and the updated `unparseable_record_is_skipped`.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/tmux/snapshot.rs tests/snapshot.rs tests/common/mod.rs
timeout 60 git commit -m "snapshot: flag sidebar panes, record each client's window; sh on test servers"
```

---

### Task 2: `model` — busy panes and the close rules (pure)

These are SPEC §6's close rules as pure functions over a `Snapshot`, so the popup and its tests agree on them.

**Files:**
- Create: `src/model.rs`
- Modify: `src/lib.rs` (add `pub mod model;`)

**Interfaces:**
- Consumes: `tmux::snapshot::{Snapshot, Pane}` (Task 1: `Pane.sidebar`, `Client.window_id`).
- Produces:
  - `pub fn is_shell(cmd: &str) -> bool`
  - `pub fn busy<'a>(panes: impl IntoIterator<Item = (&'a str, bool)>) -> Vec<String>`: items are `(current_command, sidebar)`; returns the distinct non-shell, non-sidebar commands in pane order.
  - `pub fn client_location<'a>(snap: &'a Snapshot, client: Option<&str>) -> Option<(&'a str, &'a str)>`: `(session_id, window_id)`.
  - `pub enum ClosePlan { Refuse, Now, Confirm { prompt: String } }`
  - `pub fn close_plan(snap: &Snapshot, window_id: &str) -> Option<ClosePlan>`
  - `pub fn confirm_prompt(name: &str, busy: &[String], ending_session: Option<&str>) -> String`

- [ ] **Step 1: Write the failing tests** — create `src/model.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::snapshot::{Client, Pane, Session, Snapshot, Window};

    fn pane(wid: &str, sid: &str, cmd: &str, sidebar: bool) -> Pane {
        Pane {
            id: format!("%{wid}{cmd}"),
            window_id: wid.into(),
            session_id: sid.into(),
            index: 0,
            active: true,
            sidebar,
            current_command: cmd.into(),
            current_path: "/".into(),
            title: String::new(),
        }
    }

    fn window(id: &str, sid: &str, index: u32, name: &str) -> Window {
        Window {
            id: id.into(),
            session_id: sid.into(),
            index,
            name: name.into(),
            automatic_rename: false,
            active: index == 0,
        }
    }

    /// alpha: editor (nvim), idle (sh), side (sh + a sidebar running sleep);
    /// solo: one idle window.
    fn snap() -> Snapshot {
        Snapshot {
            sessions: vec![
                Session { id: "$0".into(), name: "alpha".into(), attached: 1 },
                Session { id: "$1".into(), name: "solo".into(), attached: 0 },
            ],
            windows: vec![
                window("@0", "$0", 0, "editor"),
                window("@1", "$0", 1, "idle"),
                window("@2", "$0", 2, "side"),
                window("@3", "$1", 0, "only"),
            ],
            panes: vec![
                pane("@0", "$0", "nvim", false),
                pane("@1", "$0", "-zsh", false),
                pane("@2", "$0", "sh", false),
                pane("@2", "$0", "sleep", true),
                pane("@3", "$1", "sh", false),
            ],
            clients: vec![Client {
                name: "/dev/ttys001".into(),
                tty: "/dev/ttys001".into(),
                session_id: "$0".into(),
                window_id: "@1".into(),
            }],
        }
    }

    #[test]
    fn is_shell_handles_login_shells_and_paths() {
        for c in ["sh", "-zsh", "bash", "/bin/bash", "fish", "nu", "dash", "ksh"] {
            assert!(is_shell(c), "{c}");
        }
        for c in ["nvim", "node", "2.1.3", "bashful", "sleep", ""] {
            assert!(!is_shell(c), "{c}");
        }
    }

    #[test]
    fn busy_lists_distinct_non_shell_commands_in_pane_order() {
        let got = busy([("sleep", false), ("tail", false), ("sleep", false), ("zsh", false)]);
        assert_eq!(got, ["sleep", "tail"]);
    }

    #[test]
    fn busy_ignores_sidebar() {
        assert!(busy([("sh", false), ("sleep", true)]).is_empty());
    }

    #[test]
    fn client_location_finds_the_named_client() {
        let s = snap();
        assert_eq!(client_location(&s, Some("/dev/ttys001")), Some(("$0", "@1")));
        assert_eq!(client_location(&s, Some("/dev/other")), None);
        assert_eq!(client_location(&s, None), None);
    }

    #[test]
    fn close_plan_idle_is_now() {
        assert_eq!(close_plan(&snap(), "@1"), Some(ClosePlan::Now));
    }

    #[test]
    fn close_plan_ignores_sidebar() {
        assert_eq!(close_plan(&snap(), "@2"), Some(ClosePlan::Now));
    }

    #[test]
    fn close_plan_busy_asks() {
        assert_eq!(
            close_plan(&snap(), "@0"),
            Some(ClosePlan::Confirm { prompt: "close \"editor\"? running: nvim (y/N) ".into() })
        );
    }

    #[test]
    fn close_plan_last_in_session_warns() {
        assert_eq!(
            close_plan(&snap(), "@3"),
            Some(ClosePlan::Confirm {
                prompt: "close \"only\"? — session \"solo\" will end (y/N) ".into()
            })
        );
    }

    #[test]
    fn close_plan_busy_and_last_says_both() {
        let mut s = snap();
        s.panes[4].current_command = "vim".into();
        assert_eq!(
            close_plan(&s, "@3"),
            Some(ClosePlan::Confirm {
                prompt: "close \"only\"? running: vim — session \"solo\" will end (y/N) ".into()
            })
        );
    }

    #[test]
    fn close_plan_refuses_the_last_window_on_the_server() {
        let mut s = snap();
        s.sessions.retain(|x| x.id == "$1");
        s.windows.retain(|w| w.session_id == "$1");
        s.panes.retain(|p| p.session_id == "$1");
        assert_eq!(close_plan(&s, "@3"), Some(ClosePlan::Refuse));
    }

    #[test]
    fn close_plan_unknown_window_is_none() {
        assert_eq!(close_plan(&snap(), "@99"), None);
    }
}
```

Add `pub mod model;` to `src/lib.rs` (keep the list alphabetical).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib model`
Expected: compile errors: `is_shell`, `busy`, `client_location`, `close_plan`, `ClosePlan` not found.

- [ ] **Step 3: Implement** — put this above the test module in `src/model.rs`:

```rust
//! Pure derivations over a `Snapshot` shared by the popup and its tests:
//! which panes are busy, where the invoking client is, and what closing a
//! window must do first (SPEC §6).

use crate::tmux::snapshot::Snapshot;

/// A pane is idle when it sits at a shell prompt. Login shells show as
/// "-zsh"; anything else (an editor, "node", Claude Code's version string)
/// is busy.
pub fn is_shell(cmd: &str) -> bool {
    let c = cmd.strip_prefix('-').unwrap_or(cmd);
    let c = c.rsplit('/').next().unwrap_or(c);
    matches!(
        c,
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "mksh" | "tcsh" | "csh" | "nu" | "ash"
            | "yash"
    )
}

/// The distinct non-shell commands among `(current_command, sidebar)` panes,
/// in pane order. Sidebar panes are views, not work: they never count.
pub fn busy<'a>(panes: impl IntoIterator<Item = (&'a str, bool)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (cmd, sidebar) in panes {
        if sidebar || is_shell(cmd) || out.iter().any(|c| c == cmd) {
            continue;
        }
        out.push(cmd.to_string());
    }
    out
}

/// `(session_id, window_id)` the named client is showing.
pub fn client_location<'a>(snap: &'a Snapshot, client: Option<&str>) -> Option<(&'a str, &'a str)> {
    let client = client?;
    let c = snap.clients.iter().find(|c| c.name == client)?;
    Some((c.session_id.as_str(), c.window_id.as_str()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClosePlan {
    /// The only window on the server: closing it would end the server, and
    /// the popup with it.
    Refuse,
    /// Every non-sidebar pane is at a shell prompt and other windows remain
    /// in its session: close at once.
    Now,
    /// Ask first. `prompt` is the whole inline question.
    Confirm { prompt: String },
}

/// What `^x` on `window_id` must do; `None` if the window isn't in `snap`.
pub fn close_plan(snap: &Snapshot, window_id: &str) -> Option<ClosePlan> {
    let w = snap.windows.iter().find(|w| w.id == window_id)?;
    let in_session = snap.windows.iter().filter(|x| x.session_id == w.session_id).count();
    let other_session = snap.sessions.iter().any(|s| s.id != w.session_id);
    if in_session <= 1 && !other_session {
        return Some(ClosePlan::Refuse);
    }
    let busy = busy(
        snap.panes
            .iter()
            .filter(|p| p.window_id == window_id)
            .map(|p| (p.current_command.as_str(), p.sidebar)),
    );
    if busy.is_empty() && in_session > 1 {
        return Some(ClosePlan::Now);
    }
    let ending = if in_session <= 1 {
        snap.sessions
            .iter()
            .find(|s| s.id == w.session_id)
            .map(|s| s.name.as_str())
    } else {
        None
    };
    Some(ClosePlan::Confirm {
        prompt: confirm_prompt(&w.name, &busy, ending),
    })
}

/// `close "<name>"? running: a, b — session "<s>" will end (y/N) `
pub fn confirm_prompt(name: &str, busy: &[String], ending_session: Option<&str>) -> String {
    let mut why = String::new();
    if !busy.is_empty() {
        why.push_str(&format!(" running: {}", busy.join(", ")));
    }
    if let Some(s) = ending_session {
        why.push_str(&format!(" — session \"{s}\" will end"));
    }
    format!("close \"{name}\"?{why} (y/N) ")
}
```

- [ ] **Step 4: Run them to see them pass**

Run: `cargo test --lib model`
Expected: 11 passed.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/model.rs src/lib.rs
timeout 60 git commit -m "model: busy panes, client location, close rules"
```

---

### Task 3: `store` — the closed-window stack on disk

**Files:**
- Create: `src/store.rs`
- Modify: `src/lib.rs` (add `pub mod store;`), `src/paths.rs` (add `legacy_closed`), `tests/paths.rs`, `Cargo.toml` (dev-dependency `tempfile`)

**Interfaces:**
- Consumes: `paths::Paths::for_socket`.
- Produces:
  - `Paths.legacy_closed: PathBuf` = `<state root>/closed` (the bash popup's file)
  - `pub const CLOSED_MAX: usize = 10`
  - `pub struct ClosedWindow { session_name: String, index: u32, prev: Option<String>, next: Option<String>, automatic_rename: bool, active_pane: usize, layout: Option<String>, name: String, paths: Vec<String> }` (all fields `pub`, serde)
  - `pub struct ClosedStack`, with `new(dir: PathBuf, legacy: PathBuf)`, `for_socket(&Path) -> Result<Self>`, `path() -> PathBuf`, `lock() -> Result<StoreLock<'_>>`, `push(ClosedWindow) -> Result<()>` and `list() -> Result<Vec<ClosedWindow>>` (oldest first)
  - `pub struct StoreLock<'a>`, with `load() -> Result<Vec<ClosedWindow>>` and `save(&[ClosedWindow]) -> Result<()>` (keeps the newest `CLOSED_MAX`). It holds `state.lock` until dropped.

- [ ] **Step 1: Add the dev-dependency**

```bash
cargo add --dev tempfile
```

- [ ] **Step 2: Write the failing tests.** In `tests/paths.rs`, add to `paths_honour_env_overrides`:

```rust
    assert_eq!(p.legacy_closed, env.state.join("closed"));
```

Create `src/store.rs` with only the test module, and add `pub mod store;` to `src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn stack(root: &Path) -> ClosedStack {
        ClosedStack::new(root.join("srv"), root.join("closed"))
    }

    fn w(name: &str) -> ClosedWindow {
        ClosedWindow {
            session_name: "s".into(),
            index: 1,
            prev: None,
            next: None,
            automatic_rename: false,
            active_pane: 0,
            layout: None,
            name: name.into(),
            paths: vec!["/".into()],
        }
    }

    fn names(v: &[ClosedWindow]) -> Vec<String> {
        v.iter().map(|w| w.name.clone()).collect()
    }

    #[test]
    fn push_then_list_newest_last() {
        let d = tempfile::tempdir().unwrap();
        let s = stack(d.path());
        assert!(s.list().unwrap().is_empty());
        s.push(w("a")).unwrap();
        s.push(w("b")).unwrap();
        assert_eq!(names(&s.list().unwrap()), ["a", "b"]);
    }

    #[test]
    fn keeps_the_last_ten() {
        let d = tempfile::tempdir().unwrap();
        let s = stack(d.path());
        for i in 1..=12 {
            s.push(w(&format!("w{i}"))).unwrap();
        }
        let got = names(&s.list().unwrap());
        assert_eq!(got.len(), CLOSED_MAX);
        assert_eq!(got.first().unwrap(), "w3");
        assert_eq!(got.last().unwrap(), "w12");
    }

    #[test]
    fn writes_versioned_json_atomically() {
        let d = tempfile::tempdir().unwrap();
        let s = stack(d.path());
        s.push(w("a")).unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(s.path()).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["windows"][0]["name"], "a");
        let mut files: Vec<String> = std::fs::read_dir(d.path().join("srv"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["closed.json", "state.lock"], "no temp file left behind");
    }

    #[test]
    fn save_through_the_lock_replaces_the_stack() {
        let d = tempfile::tempdir().unwrap();
        let s = stack(d.path());
        s.push(w("a")).unwrap();
        s.push(w("b")).unwrap();
        {
            let g = s.lock().unwrap();
            let mut v = g.load().unwrap();
            assert_eq!(v.pop().unwrap().name, "b");
            g.save(&v).unwrap();
        }
        assert_eq!(names(&s.list().unwrap()), ["a"]);
    }

    #[test]
    fn lock_serialises_writers() {
        let d = tempfile::tempdir().unwrap();
        let s = stack(d.path());
        s.push(w("a")).unwrap(); // creates the dir and lock file
        let held = s.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let _g = held.lock().unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
        });
        rx.recv().unwrap();
        let t0 = std::time::Instant::now();
        s.push(w("b")).unwrap();
        assert!(t0.elapsed() >= std::time::Duration::from_millis(200), "push waited for the lock");
        t.join().unwrap();
        assert_eq!(names(&s.list().unwrap()), ["a", "b"]);
    }

    #[test]
    fn imports_the_bash_stack_once() {
        let d = tempfile::tempdir().unwrap();
        let us = '\x1f';
        let line1 = ["work", "3", "@4", "-", "off", "1", "b25f,200x50,0,0,2", "api", "/a", "/b"].join(&us.to_string());
        let line2 = ["main", "0", "-", "@9", "on", "0", "", "shell", "/c"].join(&us.to_string());
        std::fs::write(d.path().join("closed"), format!("{line1}\n{line2}\n")).unwrap();
        let s = stack(d.path());
        let got = s.list().unwrap();
        assert_eq!(
            got[0],
            ClosedWindow {
                session_name: "work".into(),
                index: 3,
                prev: Some("@4".into()),
                next: None,
                automatic_rename: false,
                active_pane: 1,
                layout: Some("b25f,200x50,0,0,2".into()),
                name: "api".into(),
                paths: vec!["/a".into(), "/b".into()],
            }
        );
        assert_eq!(got[1].prev, None);
        assert_eq!(got[1].next.as_deref(), Some("@9"));
        assert!(got[1].automatic_rename);
        assert_eq!(got[1].layout, None);
        assert!(!d.path().join("closed").exists());
        assert!(d.path().join("closed.imported").exists());
        // a second store (another server) does not import again
        let other = ClosedStack::new(d.path().join("other"), d.path().join("closed"));
        assert!(other.list().unwrap().is_empty());
    }

    #[test]
    fn malformed_legacy_lines_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let good = ["s", "1", "-", "-", "off", "0", "", "ok", "/"].join("\x1f");
        std::fs::write(d.path().join("closed"), format!("garbage\n{good}\nx\x1fnotanumber\x1f-\x1f-\x1foff\x1f0\x1f\x1fbad\x1f/\n")).unwrap();
        assert_eq!(names(&stack(d.path()).list().unwrap()), ["ok"]);
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let d = tempfile::tempdir().unwrap();
        let s = stack(d.path());
        std::fs::create_dir_all(d.path().join("srv")).unwrap();
        std::fs::write(s.path(), b"{not json").unwrap();
        assert!(s.list().unwrap().is_empty());
        assert!(d.path().join("srv/closed.json.corrupt").exists());
        s.push(w("a")).unwrap();
        assert_eq!(names(&s.list().unwrap()), ["a"]);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let s = stack(d.path());
        std::fs::create_dir_all(d.path().join("srv")).unwrap();
        std::fs::write(
            s.path(),
            r#"{"version":2,"future":true,"windows":[{"session_name":"s","index":1,"prev":null,"next":null,"automatic_rename":false,"active_pane":0,"layout":null,"name":"a","paths":["/"],"closed_at":123}]}"#,
        )
        .unwrap();
        assert_eq!(names(&s.list().unwrap()), ["a"]);
    }
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test --lib store && cargo test --test paths`
Expected: compile errors: `ClosedStack`, `ClosedWindow`, `CLOSED_MAX` and `legacy_closed` not found.

- [ ] **Step 4: Implement.** In `src/paths.rs`, add the field and set it:

```rust
pub struct Paths {
    pub sock: PathBuf,
    pub lock: PathBuf,
    pub state_dir: PathBuf,
    /// The bash popup's server-agnostic closed-window stack, imported once
    /// into the first server's `closed.json` (see `store`).
    pub legacy_closed: PathBuf,
}
```

and in `for_socket`:

```rust
        let state = state_root();
        Ok(Paths {
            sock: rt.join(format!("{key}.sock")),
            lock: rt.join(format!("{key}.lock")),
            state_dir: state.join(&key),
            legacy_closed: state.join("closed"),
        })
```

Put this above the test module in `src/store.rs`:

```rust
//! The closed-window stack (spec §9): `<state_dir>/closed.json`, the shapes
//! of the last `CLOSED_MAX` closed windows, newest last. Every
//! read-modify-write holds `<state_dir>/state.lock` (separate from the
//! daemon's instance lock, so the popup, `tmux-home reopen` and a daemon can
//! all write safely), and writes are atomic (temp file + rename).

use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

pub const CLOSED_MAX: usize = 10;

/// A closed window's shape, not its processes: enough to rebuild it with
/// fresh shells in the same place.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClosedWindow {
    pub session_name: String,
    pub index: u32,
    /// IDs of its left/right neighbours when it was closed.
    pub prev: Option<String>,
    pub next: Option<String>,
    pub automatic_rename: bool,
    /// Position of the active pane in `paths`.
    pub active_pane: usize,
    /// `window_layout`; `None` when it can't be replayed (a sidebar pane
    /// was left out, so the pane count differs).
    pub layout: Option<String>,
    pub name: String,
    /// Each non-sidebar pane's cwd, in pane order. Never empty.
    pub paths: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct StackFile {
    version: u32,
    windows: Vec<ClosedWindow>,
}

#[derive(Clone, Debug)]
pub struct ClosedStack {
    dir: PathBuf,
    legacy: PathBuf,
}

/// Holds `state.lock` until dropped.
pub struct StoreLock<'a> {
    stack: &'a ClosedStack,
    _file: File,
}

impl ClosedStack {
    pub fn new(dir: PathBuf, legacy: PathBuf) -> ClosedStack {
        ClosedStack { dir, legacy }
    }

    pub fn for_socket(tmux_socket: &Path) -> anyhow::Result<ClosedStack> {
        let p = Paths::for_socket(tmux_socket)?;
        Ok(ClosedStack::new(p.state_dir, p.legacy_closed))
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("closed.json")
    }

    /// Takes the store lock (blocking), creating the state dir 0700.
    pub fn lock(&self) -> anyhow::Result<StoreLock<'_>> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("state.lock"))?;
        file.lock()?;
        Ok(StoreLock { stack: self, _file: file })
    }

    pub fn push(&self, w: ClosedWindow) -> anyhow::Result<()> {
        let g = self.lock()?;
        let mut v = g.load()?;
        v.push(w);
        g.save(&v)
    }

    /// The stack, oldest first.
    pub fn list(&self) -> anyhow::Result<Vec<ClosedWindow>> {
        self.lock()?.load()
    }
}

impl StoreLock<'_> {
    /// The stack, oldest first. With no `closed.json` yet, the bash popup's
    /// stack is imported (once: the legacy file is renamed). An unreadable
    /// file is moved aside to `closed.json.corrupt` and the stack restarts
    /// empty; unknown fields (a newer version's) are ignored.
    pub fn load(&self) -> anyhow::Result<Vec<ClosedWindow>> {
        let path = self.stack.path();
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StackFile>(&bytes) {
                Ok(f) => Ok(f.windows),
                Err(e) => {
                    eprintln!("tmux-home: {} unreadable ({e}); moved aside", path.display());
                    fs::rename(&path, path.with_extension("json.corrupt"))?;
                    Ok(Vec::new())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.import_legacy(),
            Err(e) => Err(e.into()),
        }
    }

    /// Replaces the stack with the newest `CLOSED_MAX` of `windows`.
    pub fn save(&self, windows: &[ClosedWindow]) -> anyhow::Result<()> {
        let keep = &windows[windows.len().saturating_sub(CLOSED_MAX)..];
        let path = self.stack.path();
        let tmp = path.with_extension("json.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&StackFile {
            version: 1,
            windows: keep.to_vec(),
        })?)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn import_legacy(&self) -> anyhow::Result<Vec<ClosedWindow>> {
        let legacy = &self.stack.legacy;
        let Ok(text) = fs::read_to_string(legacy) else {
            return Ok(Vec::new());
        };
        let windows: Vec<ClosedWindow> = text.lines().filter_map(parse_legacy).collect();
        self.save(&windows)?;
        fs::rename(legacy, legacy.with_extension("imported"))?;
        Ok(windows)
    }
}

/// One line of the bash popup's `closed` file: \x1f-separated
/// session_name, index, prev|-, next|-, on|off, active, layout, name, path…
fn parse_legacy(line: &str) -> Option<ClosedWindow> {
    let f: Vec<&str> = line.split('\x1f').collect();
    if f.len() < 9 {
        return None;
    }
    let id = |s: &str| (s != "-" && !s.is_empty()).then(|| s.to_string());
    Some(ClosedWindow {
        session_name: f[0].to_string(),
        index: f[1].parse().ok()?,
        prev: id(f[2]),
        next: id(f[3]),
        automatic_rename: f[4] == "on",
        active_pane: f[5].parse().ok()?,
        layout: (!f[6].is_empty()).then(|| f[6].to_string()),
        name: f[7].to_string(),
        paths: f[8..].iter().map(|s| s.to_string()).collect(),
    })
}
```

- [ ] **Step 5: Run them to see them pass**

Run: `cargo test --lib store && cargo test --test paths`
Expected: 9 store tests and 2 paths tests pass.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock src/store.rs src/lib.rs src/paths.rs tests/paths.rs
timeout 60 git commit -m "store: closed.json stack under state.lock, atomic writes, bash import"
```

---
### Task 4: `tmux::ops` — window writes behind the `Ops` trait

**Files:**
- Create: `src/tmux/ops.rs`
- Modify: `src/tmux/mod.rs` (add `pub mod ops;`)
- Test: `tests/ops.rs` (new)

**Interfaces:**
- Consumes: `Tmux::run` and `model::busy` (Task 2), `store::ClosedStack` (Task 3).
- Produces (`tmux_home::tmux::ops`):
  - `pub fn literal_name(name: &str) -> String`: escapes `#` for tmux
  - `pub fn trim_capture(s: &str) -> String`: drops trailing blank lines (blank after stripping SGR escapes)
  - `pub async fn capture_pane(t: &Tmux, pane: &str) -> anyhow::Result<String>`: `capture-pane -e -p`, trimmed
  - `#[allow(async_fn_in_trait)] pub trait Ops` with
    `switch_to(&self, session_id: &str, window_id: &str)`,
    `rename_window(&self, window_id: &str, name: &str)`,
    `reset_auto_name(&self, window_id: &str)`,
    `busy_commands(&self, window_id: &str) -> Vec<String>`,
    `swap_windows(&self, a: &str, b: &str)`,
    `new_window_after(&self, window_id: &str, cwd: &str, name: &str) -> String` (the new window's ID).
    Every method is `async fn … -> anyhow::Result<…>`. Task 5 adds `close_window` and `reopen`.
  - `pub struct TmuxOps { pub tmux: Tmux, pub store: ClosedStack, pub client: Option<String> }`, with `TmuxOps::new(socket: &Path, client: Option<String>) -> anyhow::Result<TmuxOps>`

- [ ] **Step 1: Write the failing integration tests** — create `tests/ops.rs`:

```rust
mod common;
use common::{TestEnv, TestServer};
use std::time::Duration;
use tmux_home::tmux::{
    Tmux,
    ops::{Ops, TmuxOps, capture_pane},
    snapshot::read_snapshot,
};

fn ops(s: &TestServer) -> TmuxOps {
    TmuxOps::new(&s.socket, None).unwrap()
}

fn fmt(s: &TestServer, target: &str, f: &str) -> String {
    s.tmux(&["display-message", "-p", "-t", target, f]).trim().to_string()
}

fn wid(s: &TestServer, target: &str) -> String {
    fmt(s, target, "#{window_id}")
}

#[tokio::test]
async fn rename_by_id_keeps_odd_characters() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let w = wid(&s, "alpha:0");
    ops(&s).rename_window(&w, "-dash (paren)+plus, comma 'q'").await.unwrap();
    assert_eq!(fmt(&s, &w, "#{window_name}"), "-dash (paren)+plus, comma 'q'");
}

#[tokio::test]
async fn rename_keeps_hash_literally() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let w = wid(&s, "alpha:0");
    let name = "#{session_name} #1 a#b #[x] end#";
    ops(&s).rename_window(&w, name).await.unwrap();
    assert_eq!(fmt(&s, &w, "#{window_name}"), name);
}

#[tokio::test]
async fn rename_turns_automatic_rename_off() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let w = wid(&s, "alpha:0");
    s.tmux(&["set-option", "-w", "-t", &w, "automatic-rename", "on"]);
    ops(&s).rename_window(&w, "named").await.unwrap();
    assert_eq!(fmt(&s, &w, "#{?automatic-rename,on,off}"), "off");
}

#[tokio::test]
async fn reset_turns_automatic_rename_back_on() {
    let _env = TestEnv::new();
    let s = TestServer::start(); // global automatic-rename is off here
    let w = wid(&s, "alpha:0");
    let o = ops(&s);
    o.rename_window(&w, "named").await.unwrap();
    o.reset_auto_name(&w).await.unwrap();
    assert_eq!(fmt(&s, &w, "#{?automatic-rename,on,off}"), "on");
}

#[tokio::test]
async fn busy_idle_shell_is_empty() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.wait_settled();
    assert!(ops(&s).busy_commands(&wid(&s, "alpha:0")).await.unwrap().is_empty());
}

#[tokio::test]
async fn busy_lists_each_non_shell_command() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qb-two", "sleep 1000"]);
    s.tmux(&["split-window", "-d", "-t", "alpha:qb-two", "tail -f /dev/null"]);
    s.wait_settled();
    let got = ops(&s).busy_commands(&wid(&s, "alpha:qb-two")).await.unwrap();
    assert_eq!(got, ["sleep", "tail"]);
}

#[tokio::test]
async fn busy_ignores_sidebar_panes() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qb-side"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:qb-side", "sleep 1000"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:qb-side.0", "sleep 1001"]);
    s.tmux(&["set-option", "-p", "-t", "alpha:qb-side.1", "@pane_role", "sidebar"]);
    s.tmux(&["set-option", "-p", "-t", "alpha:qb-side.2", "@home_role", "sidebar"]);
    s.wait_settled();
    assert!(ops(&s).busy_commands(&wid(&s, "alpha:qb-side")).await.unwrap().is_empty());
}

#[tokio::test]
async fn capture_shows_the_main_pane_not_the_sidebar() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "logs"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:logs"]);
    s.tmux(&["set-option", "-p", "-t", "alpha:logs.1", "@pane_role", "sidebar"]);
    s.tmux(&["select-pane", "-t", "alpha:logs.1"]);
    s.tmux(&["send-keys", "-t", "alpha:logs.0", "echo MAIN-PANE-MARKER", "Enter"]);
    s.tmux(&["send-keys", "-t", "alpha:logs.1", "echo SIDEBAR-MARKER", "Enter"]);
    let t = Tmux::new(s.socket.clone());
    let (snap, _) = read_snapshot(&t).await.unwrap();
    let logs = wid(&s, "alpha:logs");
    let main = snap
        .panes
        .iter()
        .find(|p| p.window_id == logs && !p.sidebar)
        .unwrap();
    let mut text = String::new();
    for _ in 0..30 {
        text = capture_pane(&t, &main.id).await.unwrap();
        if text.matches("MAIN-PANE-MARKER").count() >= 2 {
            break; // the typed command and its output
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(text.contains("MAIN-PANE-MARKER"), "{text:?}");
    assert!(!text.contains("SIDEBAR-MARKER"), "{text:?}");
    assert!(!text.ends_with('\n'), "trailing blank lines trimmed: {text:?}");
}

#[tokio::test]
async fn swap_exchanges_places_and_keeps_the_current_window() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "two"]);
    let (a, b) = (wid(&s, "alpha:0"), wid(&s, "alpha:1"));
    ops(&s).swap_windows(&a, &b).await.unwrap();
    assert_eq!(fmt(&s, &a, "#{window_index}"), "1");
    assert_eq!(fmt(&s, &b, "#{window_index}"), "0");
    assert_eq!(fmt(&s, &a, "#{window_active}"), "1", "-d: the current window stays current");
}

#[tokio::test]
async fn new_window_after_opens_in_cwd_with_a_literal_name() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["rename-window", "-t", "alpha:0", "first"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "last"]);
    let o = ops(&s);
    let new = o.new_window_after(&wid(&s, "alpha:first"), "/usr", "n #1").await.unwrap();
    assert!(new.starts_with('@'), "{new}");
    assert_eq!(fmt(&s, &new, "#{window_index}"), "1");
    let names = s.tmux(&["list-windows", "-t", "alpha", "-F", "#W"]);
    assert_eq!(names.lines().collect::<Vec<_>>(), ["first", "n #1", "last"]);
    s.wait_settled();
    assert_eq!(fmt(&s, &new, "#{pane_current_path}"), "/usr");
    let unnamed = o.new_window_after(&new, "/", "").await.unwrap();
    assert_eq!(fmt(&s, &unnamed, "#{window_index}"), "2");
}
```

Add `pub mod ops;` to `src/tmux/mod.rs` and create `src/tmux/ops.rs` with just the unit tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_name_doubles_hashes_except_before_a_bracket() {
        assert_eq!(literal_name("plain"), "plain");
        assert_eq!(literal_name("#{a} #1"), "##{a} ##1");
        assert_eq!(literal_name("#[x]"), "#[x]");
        assert_eq!(literal_name("ab#"), "ab##");
    }

    #[test]
    fn trim_capture_drops_trailing_blank_lines() {
        assert_eq!(trim_capture("a\nb\n\n  \n\x1b[0m\n"), "a\nb");
        assert_eq!(trim_capture("\x1b[31mred\x1b[0m\n\n"), "\x1b[31mred\x1b[0m");
        assert_eq!(trim_capture("\n\n"), "");
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --test ops; cargo test --lib tmux::ops`
Expected: compile errors: `TmuxOps`, `Ops`, `capture_pane`, `literal_name` and `trim_capture` not found.

- [ ] **Step 3: Implement** — above the test module in `src/tmux/ops.rs`:

```rust
//! tmux writes performed by the popup and `tmux-home reopen`, behind `Ops`
//! so the popup's effect runner can be tested without a server. Every
//! target is an ID (`@n`, `%n`, `$n`), never an index or a name.

use super::Tmux;
use crate::{model, store::ClosedStack};
use std::path::Path;

/// tmux format-expands window names given to `rename-window` and
/// `new-window -n` (`#{session_name}`, `#1`…). Doubling `#` stores the name
/// as typed; `#[` is kept as is, since tmux leaves style markers literal.
pub fn literal_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '#' && chars.peek() != Some(&'[') {
            out.push('#');
        }
    }
    out
}

/// Drops trailing lines that are blank once SGR escapes are removed, so a
/// preview that shows "the last N lines" lands on the last real one.
pub fn trim_capture(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    match lines.iter().rposition(|l| !strip_sgr(l).trim().is_empty()) {
        Some(last) => lines[..=last].join("\n"),
        None => String::new(),
    }
}

fn strip_sgr(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The visible content of `pane` with colours (`capture-pane -e`), trailing
/// blank lines trimmed.
pub async fn capture_pane(t: &Tmux, pane: &str) -> anyhow::Result<String> {
    Ok(trim_capture(&t.run(&["capture-pane", "-e", "-p", "-t", pane]).await?))
}

/// The tmux writes the popup performs. Used generically (never as `dyn`)
/// and on one task, so the `async fn`s' futures need no `Send` bound.
#[allow(async_fn_in_trait)]
pub trait Ops {
    /// Switch the invoking client (or tmux's current one) to the window.
    async fn switch_to(&self, session_id: &str, window_id: &str) -> anyhow::Result<()>;
    /// Rename; tmux turns automatic-rename off for the window.
    async fn rename_window(&self, window_id: &str, name: &str) -> anyhow::Result<()>;
    /// Back to the automatic name.
    async fn reset_auto_name(&self, window_id: &str) -> anyhow::Result<()>;
    /// Distinct non-shell commands in the window's non-sidebar panes, read now.
    async fn busy_commands(&self, window_id: &str) -> anyhow::Result<Vec<String>>;
    /// `swap-window -d`: the current window stays current.
    async fn swap_windows(&self, a: &str, b: &str) -> anyhow::Result<()>;
    /// New window right after `window_id`, in `cwd`; `name` empty = automatic.
    async fn new_window_after(&self, window_id: &str, cwd: &str, name: &str) -> anyhow::Result<String>;
}

pub struct TmuxOps {
    pub tmux: Tmux,
    pub store: ClosedStack,
    /// The invoking client (`TMUX_HOME_CLIENT`).
    pub client: Option<String>,
}

impl TmuxOps {
    pub fn new(socket: &Path, client: Option<String>) -> anyhow::Result<TmuxOps> {
        Ok(TmuxOps {
            tmux: Tmux::new(socket.to_path_buf()),
            store: ClosedStack::for_socket(socket)?,
            client,
        })
    }
}

impl Ops for TmuxOps {
    async fn switch_to(&self, session_id: &str, window_id: &str) -> anyhow::Result<()> {
        let target = format!("{session_id}:{window_id}");
        match &self.client {
            Some(c) => self.tmux.run(&["switch-client", "-c", c, "-t", &target]).await?,
            None => self.tmux.run(&["switch-client", "-t", &target]).await?,
        };
        Ok(())
    }

    async fn rename_window(&self, window_id: &str, name: &str) -> anyhow::Result<()> {
        self.tmux
            .run(&["rename-window", "-t", window_id, "--", &literal_name(name)])
            .await?;
        Ok(())
    }

    async fn reset_auto_name(&self, window_id: &str) -> anyhow::Result<()> {
        // `on`, not `-u`: unsetting falls back to the global value, which
        // may itself be off.
        self.tmux
            .run(&["set-option", "-w", "-t", window_id, "automatic-rename", "on"])
            .await?;
        Ok(())
    }

    async fn busy_commands(&self, window_id: &str) -> anyhow::Result<Vec<String>> {
        let out = self
            .tmux
            .run(&[
                "list-panes",
                "-t",
                window_id,
                "-F",
                "#{@pane_role}\x1f#{@home_role}\x1f#{pane_current_command}",
            ])
            .await?;
        Ok(model::busy(out.lines().filter_map(|l| {
            let f: Vec<&str> = l.splitn(3, '\x1f').collect();
            (f.len() == 3).then(|| (f[2], f[0] == "sidebar" || f[1] == "sidebar"))
        })))
    }

    async fn swap_windows(&self, a: &str, b: &str) -> anyhow::Result<()> {
        self.tmux.run(&["swap-window", "-d", "-s", a, "-t", b]).await?;
        Ok(())
    }

    async fn new_window_after(&self, window_id: &str, cwd: &str, name: &str) -> anyhow::Result<String> {
        let out = self
            .tmux
            .run(&["new-window", "-a", "-d", "-P", "-F", "#{window_id}", "-t", window_id, "-c", cwd])
            .await?;
        let new = out.trim().to_string();
        if !name.is_empty() {
            self.rename_window(&new, name).await?;
        }
        Ok(new)
    }
}
```

- [ ] **Step 4: Run them to see them pass**

Run: `cargo test --test ops && cargo test --lib tmux::ops`
Expected: 10 integration and 2 unit tests pass.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/tmux/ops.rs src/tmux/mod.rs tests/ops.rs
timeout 60 git commit -m "ops: window writes by ID behind an Ops trait; literal names"
```

---

### Task 5: Close and reopen — window shapes, `Ops::close_window`/`reopen`, `tmux-home reopen`

This ports the bash `snapshot_window`/`close_window`/`reopen_window` to Rust and stores shapes in `ClosedStack`.

**Files:**
- Create: `src/tmux/shape.rs`
- Modify: `src/tmux/mod.rs` (add `pub mod shape;`), `src/tmux/ops.rs` (trait + impl + `LastWindowOnServer` + `reopen_cli`), `src/main.rs`
- Test: `tests/close_reopen.rs` (new)

**Interfaces:**
- Consumes: `ClosedStack::{push, lock}`, `StoreLock::{load, save}` and `ClosedWindow` (Task 3); `literal_name` (Task 4).
- Produces:
  - `tmux::shape::capture(t: &Tmux, window_id: &str) -> anyhow::Result<ClosedWindow>`
  - `tmux::shape::rebuild(t: &Tmux, w: &ClosedWindow) -> anyhow::Result<String>` (the new window ID)
  - `tmux::shape::layout_size(layout: &str) -> Option<(u32, u32)>`
  - `tmux::ops::LastWindowOnServer` (error type; test it with `err.is::<LastWindowOnServer>()`)
  - `Ops::close_window(&self, window_id: &str) -> anyhow::Result<()>`. Refuses the server's last window. For the last window of the invoking client's session, it first switches that client to another session. Then it captures the shape, kills the window and pushes the shape.
  - `Ops::reopen(&self) -> anyhow::Result<Option<String>>`. Rebuilds the newest entry and pops it only after the rebuild succeeds. Returns `None` if the stack is empty.
  - `tmux::ops::reopen_cli(socket: Option<PathBuf>) -> anyhow::Result<i32>`: prints the ID and returns 0, or returns 4 if there's nothing to reopen
  - CLI: `tmux-home reopen [--socket PATH]`

- [ ] **Step 1: Write the failing tests** — create `tests/close_reopen.rs`:

```rust
mod common;
use common::{TestEnv, TestServer};
use tmux_home::{
    store::{ClosedStack, ClosedWindow},
    tmux::ops::{LastWindowOnServer, Ops, TmuxOps},
};

fn ops(s: &TestServer) -> TmuxOps {
    TmuxOps::new(&s.socket, None).unwrap()
}

fn stack(s: &TestServer) -> Vec<ClosedWindow> {
    ClosedStack::for_socket(&s.socket).unwrap().list().unwrap()
}

fn fmt(s: &TestServer, target: &str, f: &str) -> String {
    s.tmux(&["display-message", "-p", "-t", target, f]).trim().to_string()
}

fn wid(s: &TestServer, target: &str) -> String {
    fmt(s, target, "#{window_id}")
}

fn names(s: &TestServer, session: &str) -> Vec<String> {
    s.tmux(&["list-windows", "-t", session, "-F", "#W"]).lines().map(String::from).collect()
}

fn sessions(s: &TestServer) -> Vec<String> {
    s.tmux(&["list-sessions", "-F", "#S"]).lines().map(String::from).collect()
}

/// index|name|auto-rename|pane count, then index:active:geometry:cwd per pane
fn shape(s: &TestServer, w: &str) -> String {
    fmt(s, w, "#{window_index}|#{window_name}|#{?automatic-rename,on,off}|#{window_panes}")
        + "\n"
        + &s.tmux(&[
            "list-panes",
            "-t",
            w,
            "-F",
            "#{pane_index}:#{pane_active}:#{pane_width}x#{pane_height}+#{pane_left}+#{pane_top}:#{pane_current_path}",
        ])
}

fn reopen_cli(s: &TestServer) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tmux-home"))
        .args(["reopen", "--socket"])
        .arg(&s.socket)
        .output()
        .unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[tokio::test]
async fn close_refuses_last_window_on_server() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let w = wid(&s, "alpha:0");
    let err = ops(&s).close_window(&w).await.unwrap_err();
    assert!(err.is::<LastWindowOnServer>(), "{err:#}");
    assert_eq!(wid(&s, "alpha:0"), w);
    assert!(stack(&s).is_empty());
}

#[tokio::test]
async fn close_then_reopen_restores_shape() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:5", "-n", "qshape", "-c", "/usr"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:qshape", "-c", "/etc"]);
    s.tmux(&["split-window", "-d", "-v", "-t", "alpha:qshape.1", "-c", "/var"]);
    s.tmux(&["resize-pane", "-t", "alpha:qshape.0", "-x", "60"]);
    s.tmux(&["select-pane", "-t", "alpha:qshape.2"]);
    s.wait_settled();
    let want = shape(&s, "alpha:qshape");
    let o = ops(&s);
    o.close_window(&wid(&s, "alpha:qshape")).await.unwrap();
    assert_eq!(stack(&s).len(), 1, "close pushes a snapshot");
    assert!(!names(&s, "alpha").contains(&"qshape".to_string()));
    let new = o.reopen().await.unwrap().expect("something to reopen");
    s.wait_settled();
    assert_eq!(wid(&s, "alpha:qshape"), new);
    assert_eq!(shape(&s, &new), want);
    assert!(stack(&s).is_empty(), "reopen pops the stack");
}

#[tokio::test]
async fn reopen_keeps_automatic_rename() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:5", "-n", "qauto"]);
    s.tmux(&["set-option", "-w", "-t", "alpha:qauto", "automatic-rename", "on"]);
    let o = ops(&s);
    o.close_window(&wid(&s, "alpha:5")).await.unwrap();
    let new = o.reopen().await.unwrap().unwrap();
    assert_eq!(fmt(&s, &new, "#{?automatic-rename,on,off}"), "on");
}

#[tokio::test]
async fn reopen_restores_a_hash_name() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "tmp"]);
    let o = ops(&s);
    let w = wid(&s, "alpha:tmp");
    o.rename_window(&w, "fix #12 #{x}").await.unwrap();
    o.close_window(&w).await.unwrap();
    let new = o.reopen().await.unwrap().unwrap();
    assert_eq!(fmt(&s, &new, "#{window_name}"), "fix #12 #{x}");
}

#[tokio::test]
async fn reopen_after_old_neighbour_when_index_taken() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    for (i, n) in [(5, "qn5"), (6, "qn6"), (7, "qn7")] {
        s.tmux(&["new-window", "-d", "-t", &format!("alpha:{i}"), "-n", n]);
    }
    let o = ops(&s);
    o.close_window(&wid(&s, "alpha:qn6")).await.unwrap();
    s.tmux(&["move-window", "-s", "alpha:qn7", "-t", "alpha:6"]);
    o.reopen().await.unwrap().unwrap();
    let qn: Vec<String> = names(&s, "alpha").into_iter().filter(|n| n.starts_with("qn")).collect();
    assert_eq!(qn, ["qn5", "qn6", "qn7"]);
}

#[tokio::test]
async fn reopen_recreates_ended_session() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-session", "-d", "-s", "qgone", "-n", "qlast", "-c", "/usr"]);
    s.wait_settled();
    let o = ops(&s);
    o.close_window(&wid(&s, "qgone:qlast")).await.unwrap();
    assert!(!sessions(&s).contains(&"qgone".to_string()), "closing the last window ends the session");
    let new = o.reopen().await.unwrap().unwrap();
    s.wait_settled();
    assert_eq!(fmt(&s, &new, "#{session_name} #{window_name} #{pane_current_path}"), "qgone qlast /usr");
}

#[tokio::test]
async fn stack_is_lifo_capped_at_ten() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-session", "-d", "-s", "qstack"]);
    for i in 1..=12 {
        s.tmux(&["new-window", "-d", "-t", "qstack:", "-n", &format!("qs{i}")]);
    }
    let o = ops(&s);
    for i in 1..=12 {
        o.close_window(&wid(&s, &format!("qstack:qs{i}"))).await.unwrap();
    }
    assert_eq!(stack(&s).len(), 10);
    let mut order = Vec::new();
    for _ in 0..10 {
        let w = o.reopen().await.unwrap().unwrap();
        order.push(fmt(&s, &w, "#{window_name}"));
    }
    assert_eq!(order, ["qs12", "qs11", "qs10", "qs9", "qs8", "qs7", "qs6", "qs5", "qs4", "qs3"]);
    let before = s.tmux(&["list-windows", "-a"]).lines().count();
    assert_eq!(reopen_cli(&s).0, 4, "empty stack exits 4");
    assert_eq!(s.tmux(&["list-windows", "-a"]).lines().count(), before, "and creates nothing");
}

#[tokio::test]
async fn reopen_cli_prints_the_new_window_id() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qcli"]);
    ops(&s).close_window(&wid(&s, "alpha:qcli")).await.unwrap();
    let (code, out) = reopen_cli(&s);
    assert_eq!(code, 0);
    assert_eq!(out, wid(&s, "alpha:qcli"));
}

#[tokio::test]
async fn reopen_with_missing_cwd_falls_back_to_home() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    ClosedStack::for_socket(&s.socket)
        .unwrap()
        .push(ClosedWindow {
            session_name: "alpha".into(),
            index: 7,
            prev: None,
            next: None,
            automatic_rename: false,
            active_pane: 0,
            layout: None,
            name: "gone-dir".into(),
            paths: vec!["/nonexistent/th-gone".into()],
        })
        .unwrap();
    let new = ops(&s).reopen().await.unwrap().unwrap();
    assert_eq!(fmt(&s, &new, "#{window_index} #{window_name}"), "7 gone-dir");
}
```

Add unit tests at the bottom of a new `src/tmux/shape.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_size_reads_the_window_size() {
        assert_eq!(layout_size("b25f,200x50,0,0,2"), Some((200, 50)));
        assert_eq!(layout_size("c1a2,80x24,0,0{40x24,0,0,1,39x24,41,0,2}"), Some((80, 24)));
        assert_eq!(layout_size("junk"), None);
    }
}
```

and add `pub mod shape;` to `src/tmux/mod.rs`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --test close_reopen; cargo test --lib tmux::shape`
Expected: compile errors: `close_window`, `reopen`, `LastWindowOnServer` and `layout_size` not found, and no `reopen` subcommand.

- [ ] **Step 3: Implement `src/tmux/shape.rs`** (above its tests):

```rust
//! A window's shape — session, place, name, panes, layout, cwds — captured
//! before it is closed and rebuilt with fresh shells by reopen. Port of the
//! bash popup's `snapshot_window` / `reopen_window`.

use super::{Tmux, ops::literal_name};
use crate::store::ClosedWindow;

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

/// "b25f,200x50,0,0,2" → (200, 50): the window size the layout was taken at.
pub fn layout_size(layout: &str) -> Option<(u32, u32)> {
    let (x, y) = layout.split(',').nth(1)?.split_once('x')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

pub async fn capture(t: &Tmux, window_id: &str) -> anyhow::Result<ClosedWindow> {
    let info = t
        .run(&[
            "display-message",
            "-p",
            "-t",
            window_id,
            "#{session_id}\x1f#{session_name}\x1f#{window_index}\x1f#{automatic-rename}\x1f#{window_layout}\x1f#{window_panes}\x1f#{window_name}",
        ])
        .await?;
    let f: Vec<&str> = info.trim_end_matches('\n').splitn(7, '\x1f').collect();
    anyhow::ensure!(f.len() == 7, "unexpected display-message output: {info:?}");

    let ids = t.run(&["list-windows", "-t", f[0], "-F", "#{window_id}"]).await?;
    let ids: Vec<&str> = ids.lines().collect();
    let pos = ids.iter().position(|w| *w == window_id);
    let prev = pos.and_then(|i| i.checked_sub(1)).map(|i| ids[i].to_string());
    let next = pos.and_then(|i| ids.get(i + 1)).map(|w| w.to_string());

    let panes = t
        .run(&[
            "list-panes",
            "-t",
            window_id,
            "-F",
            "#{pane_active}\x1f#{@pane_role}\x1f#{@home_role}\x1f#{pane_current_path}\x1e",
        ])
        .await?;
    let mut paths = Vec::new();
    let mut active_pane = 0;
    for rec in panes.split("\x1e\n").map(|r| r.trim_end_matches('\x1e')) {
        let g: Vec<&str> = rec.splitn(4, '\x1f').collect();
        if g.len() != 4 || g[1] == "sidebar" || g[2] == "sidebar" {
            continue;
        }
        if g[0] == "1" {
            active_pane = paths.len();
        }
        paths.push(g[3].to_string());
    }
    let total: usize = f[5].parse()?;
    // a dropped sidebar pane makes the layout's pane count wrong
    let layout = (paths.len() == total && !f[4].is_empty()).then(|| f[4].to_string());
    if paths.is_empty() {
        paths.push(home());
    }
    Ok(ClosedWindow {
        session_name: f[1].to_string(),
        index: f[2].parse()?,
        prev,
        next,
        automatic_rename: f[3] == "1",
        active_pane,
        layout,
        name: f[6].replace('\n', " "),
        paths,
    })
}

async fn new_window(t: &Tmux, place: &[&str], cwd: &str) -> anyhow::Result<String> {
    let mut args = vec!["new-window", "-d", "-P", "-F", "#{window_id}", "-c", cwd];
    args.extend_from_slice(place);
    Ok(t.run(&args).await?.trim().to_string())
}

/// `window_id` if it still exists in the session named `session`.
async fn in_session(t: &Tmux, window_id: Option<&str>, session: &str) -> Option<String> {
    let w = window_id?;
    let name = t.run(&["display-message", "-p", "-t", w, "#{session_name}"]).await.ok()?;
    (name.trim_end_matches('\n') == session).then(|| w.to_string())
}

/// Rebuilds `w` with fresh shells in each pane's old cwd: at its old index
/// if free, else right after its old left neighbour, else right before its
/// old right neighbour, else at the end; its session is recreated if it
/// has gone. Returns the new window ID. Nothing is restarted.
pub async fn rebuild(t: &Tmux, w: &ClosedWindow) -> anyhow::Result<String> {
    let path0 = w.paths.first().cloned().unwrap_or_else(home);
    let exact = format!("={}", w.session_name);
    let new = if t.run(&["has-session", "-t", &exact]).await.is_ok() {
        let at = format!("={}:{}", w.session_name, w.index);
        if let Ok(id) = new_window(t, &["-t", &at], &path0).await {
            id
        } else if let Some(p) = in_session(t, w.prev.as_deref(), &w.session_name).await {
            new_window(t, &["-a", "-t", &p], &path0).await?
        } else if let Some(n) = in_session(t, w.next.as_deref(), &w.session_name).await {
            new_window(t, &["-b", "-t", &n], &path0).await?
        } else {
            new_window(t, &["-t", &format!("={}:", w.session_name)], &path0).await?
        }
    } else {
        // size the new session like the layout it is about to get
        let size = w
            .layout
            .as_deref()
            .and_then(layout_size)
            .map(|(x, y)| (x.to_string(), y.to_string()));
        let mut args = vec!["new-session", "-d", "-P", "-F", "#{window_id}", "-c", &path0, "-s", &w.session_name];
        if let Some((x, y)) = &size {
            args.extend(["-x", x.as_str(), "-y", y.as_str()]);
        }
        let id = t.run(&args).await?.trim().to_string();
        let _ = t
            .run(&["move-window", "-s", &id, "-t", &format!("={}:{}", w.session_name, w.index)])
            .await;
        id
    };

    // split the LAST pane each time so pane order matches `paths`; retile as
    // we go so small windows keep room for the next split
    let mut pane = t.run(&["display-message", "-p", "-t", &new, "#{pane_id}"]).await?.trim().to_string();
    for p in w.paths.iter().skip(1) {
        match t.run(&["split-window", "-d", "-P", "-F", "#{pane_id}", "-t", &pane, "-c", p]).await {
            Ok(id) => pane = id.trim().to_string(),
            Err(_) => break,
        }
        let _ = t.run(&["select-layout", "-t", &new, "tiled"]).await;
    }
    if let Some(layout) = &w.layout {
        let _ = t.run(&["select-layout", "-t", &new, layout]).await;
    }
    if w.automatic_rename {
        t.run(&["set-option", "-w", "-t", &new, "automatic-rename", "on"]).await?;
    } else {
        t.run(&["rename-window", "-t", &new, "--", &literal_name(&w.name)]).await?;
    }
    let ids = t.run(&["list-panes", "-t", &new, "-F", "#{pane_id}"]).await?;
    if let Some(p) = ids.lines().nth(w.active_pane) {
        let _ = t.run(&["select-pane", "-t", p]).await;
    }
    Ok(new)
}
```

- [ ] **Step 4: Extend `src/tmux/ops.rs`.** Change the `use` lines to:

```rust
use super::{Tmux, shape};
use crate::{model, store::ClosedStack};
use std::path::{Path, PathBuf};
```

Add after `trim_capture`/`capture_pane`:

```rust
/// `close_window` refuses the only window on the server: closing it would
/// end the server, and the popup with it.
#[derive(Debug)]
pub struct LastWindowOnServer;

impl std::fmt::Display for LastWindowOnServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("can't close the last window on the server")
    }
}

impl std::error::Error for LastWindowOnServer {}
```

Add to the `Ops` trait:

```rust
    /// Close the window without taking the popup down: refuses the last
    /// window on the server (`LastWindowOnServer`); for the last window of
    /// the invoking client's session, first moves that client to another
    /// session (else `detach-on-destroy` would detach it, and the popup with
    /// it). Its shape is pushed onto the closed stack.
    async fn close_window(&self, window_id: &str) -> anyhow::Result<()>;
    /// Rebuild the most recently closed window; `None` if there is none.
    /// The entry is popped only once the rebuild succeeded.
    async fn reopen(&self) -> anyhow::Result<Option<String>>;
```

Add to `impl Ops for TmuxOps`:

```rust
    async fn close_window(&self, window_id: &str) -> anyhow::Result<()> {
        let info = self
            .tmux
            .run(&["display-message", "-p", "-t", window_id, "#{session_id}\x1f#{session_windows}"])
            .await?;
        let (sid, windows) = info
            .trim_end()
            .split_once('\x1f')
            .ok_or_else(|| anyhow::anyhow!("unexpected display-message output: {info:?}"))?;
        if windows.parse::<u32>()? <= 1 {
            let sessions = self.tmux.run(&["list-sessions", "-F", "#{session_id}"]).await?;
            let Some(other) = sessions.lines().find(|s| *s != sid) else {
                return Err(LastWindowOnServer.into());
            };
            if let Some(c) = &self.client {
                let clients = self
                    .tmux
                    .run(&["list-clients", "-F", "#{client_name}\x1f#{session_id}"])
                    .await?;
                if clients.lines().any(|l| l.split_once('\x1f') == Some((c.as_str(), sid))) {
                    self.tmux.run(&["switch-client", "-c", c, "-t", other]).await?;
                }
            }
        }
        let shape = shape::capture(&self.tmux, window_id).await.ok();
        self.tmux.run(&["kill-window", "-t", window_id]).await?;
        if let Some(s) = shape {
            self.store.push(s)?;
        }
        Ok(())
    }

    async fn reopen(&self) -> anyhow::Result<Option<String>> {
        let lock = self.store.lock()?;
        let mut stack = lock.load()?;
        let Some(last) = stack.last().cloned() else {
            return Ok(None);
        };
        let window_id = shape::rebuild(&self.tmux, &last).await?;
        stack.pop();
        lock.save(&stack)?;
        Ok(Some(window_id))
    }
```

and at the end of the file (above the tests):

```rust
/// `tmux-home reopen`: prints the new window ID; exit status 4 when there
/// is nothing to reopen.
pub async fn reopen_cli(socket: Option<PathBuf>) -> anyhow::Result<i32> {
    let socket = crate::client::current_socket(socket).await?;
    match TmuxOps::new(&socket, None)?.reopen().await? {
        Some(w) => {
            println!("{w}");
            Ok(0)
        }
        None => Ok(4),
    }
}
```

- [ ] **Step 5: Add the subcommand.** Replace `src/main.rs` with:

```rust
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tmux-home", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon for one tmux server (normally started automatically).
    Daemon {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, value_enum, default_value = "poll")]
        source: tmux_home::tmux::source::SourceKind,
    },
    /// Print the current snapshot as JSON.
    Query {
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Print JSON (accepted for forward compatibility; JSON is
        /// currently the only output format).
        #[arg(long)]
        json: bool,
    },
    /// Recreate the most recently closed window — same place, name, panes,
    /// layout and directories, fresh shells — and print its ID. Exit
    /// status 4 when there is nothing to reopen.
    Reopen {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// R0 spike: measure control-mode side effects on a server and print a report.
    SpikeControl {
        #[arg(long)]
        socket: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.cmd {
        Cmd::Daemon { socket, source } => rt.block_on(tmux_home::daemon::run(socket, source)),
        Cmd::Query { socket, json: _ } => rt.block_on(tmux_home::client::query(socket)),
        Cmd::Reopen { socket } => {
            let code = rt.block_on(tmux_home::tmux::ops::reopen_cli(socket))?;
            drop(rt);
            std::process::exit(code)
        }
        Cmd::SpikeControl { socket } => rt.block_on(tmux_home::tmux::source::spike_control(socket)),
    }
}
```

- [ ] **Step 6: Run them to see them pass**

Run: `cargo test --test close_reopen && cargo test --lib tmux::shape`
Expected: 9 integration and 1 unit test pass.

- [ ] **Step 7: Full check, lint, commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/tmux/shape.rs src/tmux/mod.rs src/tmux/ops.rs src/main.rs tests/close_reopen.rs
timeout 60 git commit -m "close/reopen: window shapes on the closed stack; tmux-home reopen"
```

---

### Task 6: Daemon `refresh` op; client `subscribe` and `refresh`

So the popup sees its own writes at once (ruling 5) and can follow the pushed stream.

**Files:**
- Modify: `src/ipc.rs`, `src/daemon.rs`, `src/client.rs`
- Test: `tests/daemon.rs`, `tests/client.rs`, unit test in `src/ipc.rs`

**Interfaces:**
- Consumes: R0 `daemon::run`, `ipc::{Request, Reply, read_msg, write_msg}`, `snapshot::read_snapshot`.
- Produces:
  - `Request::Refresh { v: String }`. Wire form: `{"op":"refresh","v":"…"}`. The daemon re-reads tmux now, bumps `seq`, pushes to every subscriber and replies `Reply::Snapshot { seq, data }` (or `Reply::Error`).
  - `client::subscribe(tmux_socket: &Path, client: &str, budget: Duration) -> anyhow::Result<(u64, Snapshot, Subscription)>`. On no daemon, a timeout or a `restart` reply it starts a daemon in the background and returns `Err`.
  - `client::Subscription::next(&mut self) -> anyhow::Result<Option<(u64, Snapshot)>>` (`None` = the daemon closed the stream)
  - `client::refresh(tmux_socket: &Path, budget: Duration) -> anyhow::Result<(u64, Snapshot)>`

- [ ] **Step 1: Write the failing tests.** In `src/ipc.rs`, add to `wire_shape`:

```rust
        assert_eq!(
            serde_json::to_string(&Request::Refresh { v: "0.1.0".into() }).unwrap(),
            r#"{"op":"refresh","v":"0.1.0"}"#
        );
```

Append to `tests/daemon.rs`:

```rust
/// `refresh` reads tmux at once (no poll wait), replies with that snapshot
/// and pushes the same `seq` to subscribers.
#[tokio::test]
async fn refresh_reads_now_and_broadcasts() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let (r, mut w) = connect(&p).await.into_split();
    let mut r = BufReader::new(r);
    write_msg(&mut w, &Request::Subscribe { v: v(), client: "test".into() })
        .await
        .unwrap();
    let Some(Reply::Snapshot { seq: first, .. }) = read_msg(&mut r).await.unwrap() else {
        panic!("no initial snapshot")
    };
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "fresh"]);
    let Some(Reply::Snapshot { seq, data }) = ask(&p, Request::Refresh { v: v() }).await else {
        panic!("refresh did not answer with a snapshot")
    };
    assert!(seq > first);
    assert!(data.windows.iter().any(|w| w.name == "fresh"));
    let pushed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Reply::Snapshot { seq: got, .. }) = read_msg::<_, Reply>(&mut r).await.unwrap()
                && got >= seq
            {
                return got;
            }
        }
    })
    .await
    .expect("subscriber saw the refreshed snapshot");
    assert!(pushed >= seq);
    d.abort();
}
```

Append to `tests/client.rs`:

```rust
async fn daemon_up(socket: &std::path::Path) {
    let p = tmux_home::paths::Paths::for_socket(socket).unwrap();
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(&p.sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon never came up");
}

#[tokio::test]
async fn subscribe_streams_pushes() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let d = tokio::spawn(tmux_home::daemon::run(
        s.socket.clone(),
        tmux_home::tmux::source::SourceKind::Poll,
    ));
    daemon_up(&s.socket).await;
    let (seq, first, mut sub) =
        tmux_home::client::subscribe(&s.socket, "test", Duration::from_millis(500))
            .await
            .unwrap();
    assert!(seq > 0);
    assert_eq!(first.windows.len(), 1);
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "pushed"]);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, snap) = sub.next().await.unwrap().expect("stream open");
            if snap.windows.iter().any(|w| w.name == "pushed") {
                return;
            }
        }
    })
    .await
    .expect("push arrived");
    d.abort();
}

#[tokio::test]
async fn refresh_returns_the_current_state() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let d = tokio::spawn(tmux_home::daemon::run(
        s.socket.clone(),
        tmux_home::tmux::source::SourceKind::Poll,
    ));
    daemon_up(&s.socket).await;
    let (first, _, _sub) =
        tmux_home::client::subscribe(&s.socket, "test", Duration::from_millis(500))
            .await
            .unwrap();
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "now"]);
    let (seq, snap) = tmux_home::client::refresh(&s.socket, Duration::from_millis(500))
        .await
        .unwrap();
    assert!(seq > first);
    assert!(snap.windows.iter().any(|w| w.name == "now"));
    d.abort();
}

#[tokio::test]
async fn subscribe_without_daemon_fails_fast() {
    // SAFETY: see degraded_then_daemon above.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", "/nonexistent/tmux-home");
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let t0 = std::time::Instant::now();
    assert!(
        tmux_home::client::subscribe(&s.socket, "test", Duration::from_millis(150))
            .await
            .is_err()
    );
    assert!(t0.elapsed() < Duration::from_secs(1));
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib ipc; cargo test --test daemon --test client`
Expected: compile errors: `Request::Refresh`, `client::subscribe` and `client::refresh` not found.

- [ ] **Step 3: Implement the protocol** — in `src/ipc.rs`:

```rust
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Subscribe { v: String, client: String },
    Query { v: String },
    /// Re-read tmux now, push the result to every subscriber, and reply with
    /// it: a client that just wrote to tmux sees its change at once.
    Refresh { v: String },
}
```

and in `version()`:

```rust
            Request::Subscribe { v, .. } | Request::Query { v } | Request::Refresh { v } => v,
```

- [ ] **Step 4: Implement the daemon side** — in `src/daemon.rs`, change the imports to:

```rust
use crate::{
    VERSION,
    ipc::{Reply, Request, read_msg, write_msg},
    paths::Paths,
    tmux::{
        Tmux,
        snapshot::{Snapshot, read_snapshot},
        source::{self, SourceEvent, SourceKind},
    },
};
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    signal::unix::{SignalKind, signal},
    sync::{Notify, mpsc, oneshot, watch},
};

type Latest = watch::Receiver<Option<(u64, Snapshot)>>;
/// A `refresh` request waiting for the main loop's read.
type RefreshReply = oneshot::Sender<anyhow::Result<(u64, Snapshot)>>;
```

In `run_with_version`, replace

```rust
    let mut events = source::start(kind, Tmux::new(tmux_socket));
```

with

```rust
    let tmux = Tmux::new(tmux_socket);
    let mut events = source::start(kind, tmux.clone());
    let (refresh_tx, mut refresh_rx) = mpsc::channel::<RefreshReply>(8);
```

and in the `tokio::select!` loop change the accept arm and add a refresh arm:

```rust
            conn = listener.accept() => match conn {
                Ok((stream, _)) => {
                    tokio::spawn(serve(stream, latest.clone(), restart.clone(), refresh_tx.clone(), version));
                }
                Err(e) => {
                    eprintln!("tmux-home: accept error: {e:#}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
            },
            Some(reply) = refresh_rx.recv() => {
                // A poll read already in flight may still deliver an older
                // state after this; the next poll (≤ 500 ms) corrects it.
                let r = read_snapshot(&tmux).await.map(|(snap, _)| {
                    seq += 1;
                    let _ = tx.send(Some((seq, snap.clone())));
                    (seq, snap)
                });
                let _ = reply.send(r);
            }
```

Change `serve` to take the refresh channel and answer `Refresh` right after the version check:

```rust
async fn serve(
    stream: UnixStream,
    mut latest: Latest,
    restart: Arc<Notify>,
    refresh: mpsc::Sender<RefreshReply>,
    version: &'static str,
) {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let Ok(Ok(Some(req))) =
        tokio::time::timeout(INITIAL_REQUEST_TIMEOUT, read_msg::<_, Request>(&mut r)).await
    else {
        return;
    };
    if req.version() != version {
        let _ = write_msg(&mut w, &Reply::Restart).await;
        restart.notify_one();
        return;
    }
    if let Request::Refresh { .. } = req {
        let (otx, orx) = oneshot::channel();
        let reply = if refresh.send(otx).await.is_err() {
            Reply::Error { msg: "daemon shutting down".into() }
        } else {
            match orx.await {
                Ok(Ok((seq, data))) => Reply::Snapshot { seq, data },
                Ok(Err(e)) => Reply::Error { msg: format!("{e:#}") },
                Err(_) => Reply::Error { msg: "daemon shutting down".into() },
            }
        };
        let _ = write_msg(&mut w, &reply).await;
        return;
    }
    // wait for the first snapshot if the source hasn't produced one yet
    if latest.wait_for(|s| s.is_some()).await.is_err() {
        return;
    }
    let cur = latest.borrow_and_update().clone();
    if send(&mut w, cur).await.is_err() {
        return;
    }
    if let Request::Subscribe { .. } = req {
        while latest.changed().await.is_ok() {
            let cur = latest.borrow_and_update().clone();
            if send(&mut w, cur).await.is_err() {
                return;
            }
        }
    }
}
```

- [ ] **Step 5: Implement the client side** — append to `src/client.rs` (it already imports `Reply`, `Request`, `read_msg`, `write_msg`, `Paths`, `Snapshot`, `BufReader` and `UnixStream`):

```rust
/// A live snapshot stream from the daemon.
pub struct Subscription {
    r: BufReader<tokio::net::unix::OwnedReadHalf>,
    _w: tokio::net::unix::OwnedWriteHalf,
}

impl Subscription {
    /// The next pushed snapshot; `None` when the daemon closed the stream
    /// (it exited, or was replaced after a version mismatch).
    pub async fn next(&mut self) -> anyhow::Result<Option<(u64, Snapshot)>> {
        match read_msg::<_, Reply>(&mut self.r).await? {
            None => Ok(None),
            Some(Reply::Snapshot { seq, data }) => Ok(Some((seq, data))),
            Some(other) => anyhow::bail!("unexpected message on subscription: {other:?}"),
        }
    }
}

/// Subscribe within `budget`. Any failure (no daemon, timeout, a
/// version-mismatch `restart`) starts a daemon in the background and
/// returns `Err`, so the caller can fall back to reading tmux itself.
pub async fn subscribe(
    tmux_socket: &Path,
    client: &str,
    budget: Duration,
) -> anyhow::Result<(u64, Snapshot, Subscription)> {
    let p = Paths::for_socket(tmux_socket)?;
    let attempt = async {
        let s = UnixStream::connect(&p.sock).await?;
        let (r, mut w) = s.into_split();
        write_msg(
            &mut w,
            &Request::Subscribe {
                v: VERSION.into(),
                client: client.into(),
            },
        )
        .await?;
        let mut r = BufReader::new(r);
        let first = read_msg::<_, Reply>(&mut r).await?;
        anyhow::Ok((first, r, w))
    };
    match tokio::time::timeout(budget, attempt).await {
        Ok(Ok((Some(Reply::Snapshot { seq, data }), r, w))) => {
            Ok((seq, data, Subscription { r, _w: w }))
        }
        Ok(Ok((Some(Reply::Restart), _, _))) => {
            wait_for_socket_gone(&p.sock).await;
            try_spawn_daemon(tmux_socket);
            anyhow::bail!("daemon restarting (version mismatch)")
        }
        Ok(Ok((other, _, _))) => anyhow::bail!("daemon replied {other:?}"),
        Ok(Err(e)) => {
            try_spawn_daemon(tmux_socket);
            Err(e)
        }
        Err(_) => {
            try_spawn_daemon(tmux_socket);
            anyhow::bail!("daemon did not answer within {budget:?}")
        }
    }
}

/// Ask the daemon to re-read tmux now; returns that snapshot and its `seq`.
pub async fn refresh(tmux_socket: &Path, budget: Duration) -> anyhow::Result<(u64, Snapshot)> {
    let p = Paths::for_socket(tmux_socket)?;
    let attempt = async {
        let s = UnixStream::connect(&p.sock).await?;
        let (r, mut w) = s.into_split();
        write_msg(&mut w, &Request::Refresh { v: VERSION.into() }).await?;
        read_msg::<_, Reply>(&mut BufReader::new(r)).await
    };
    match tokio::time::timeout(budget, attempt).await {
        Ok(Ok(Some(Reply::Snapshot { seq, data }))) => Ok((seq, data)),
        Ok(Ok(Some(Reply::Error { msg }))) => anyhow::bail!("daemon: {msg}"),
        Ok(Ok(other)) => anyhow::bail!("unexpected refresh reply: {other:?}"),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!("refresh did not answer within {budget:?}"),
    }
}
```

- [ ] **Step 6: Run them to see them pass**

Run: `cargo test --lib ipc && cargo test --test daemon --test client`
Expected: all pass (R0's daemon and client tests included).

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/ipc.rs src/daemon.rs src/client.rs tests/daemon.rs tests/client.rs
timeout 60 git commit -m "daemon: refresh op; client: subscribe stream and refresh"
```

---
### Task 7: Popup rows and the fuzzy filter (pure)

**Files:**
- Create: `src/popup/mod.rs`, `src/popup/rows.rs`, `src/popup/filter.rs`, `src/popup/testutil.rs`
- Modify: `src/lib.rs` (add `pub mod popup;`), `Cargo.toml` (`nucleo-matcher`)

**Interfaces:**
- Consumes: `model::client_location` (Task 2), `Snapshot` (Task 1).
- Produces:
  - `popup::rows::WinRow { session_id, session_name, window_id, index: u32, name, automatic_rename: bool, pane_id: Option<String>, command, path, short_path, current: bool }` (all `pub`; Clone, Debug, PartialEq, Eq)
  - `popup::rows::build(snap: &Snapshot, client: Option<&str>, home: &str) -> Vec<WinRow>`
  - `popup::rows::short_path(path: &str, home: &str) -> String`
  - `popup::filter::Filter` (Default), with `matching(&mut self, query: &str, rows: &[WinRow]) -> Vec<usize>` and `Filter::haystack(&WinRow) -> String`
  - `popup::testutil` (cfg(test)): `HOME`, `CLIENT`, `window()`, `pane()`, `snap()`, `add_window()`, `add_session()`, `drop_window()`

- [ ] **Step 1: Add the dependency and check its API**

```bash
cargo add nucleo-matcher
grep -n "pub fn parse\|pub fn score" ~/.cargo/registry/src/*/nucleo-matcher-0.3.*/src/pattern.rs
```

Expected: `Pattern::parse(pattern: &str, case_matching: CaseMatching, normalize: Normalization) -> Pattern` and `Pattern::score(&self, haystack: Utf32Str<'_>, matcher: &mut Matcher) -> Option<u32>` (0.3.1).

- [ ] **Step 2: Create the module skeleton and fixtures.** `src/popup/mod.rs`:

```rust
//! `tmux-home popup` (spec §7): the window list TUI, a client of the daemon.

pub mod filter;
pub mod rows;
#[cfg(test)]
pub(crate) mod testutil;
```

Add `pub mod popup;` to `src/lib.rs`. Create `src/popup/testutil.rs`:

```rust
//! Fixtures for the popup's unit tests.

use crate::tmux::snapshot::{Client, Pane, Session, Snapshot, Window};

pub const HOME: &str = "/home/u";
pub const CLIENT: &str = "/dev/ttys001";

pub fn window(id: &str, session_id: &str, index: u32, name: &str) -> Window {
    Window {
        id: id.into(),
        session_id: session_id.into(),
        index,
        name: name.into(),
        automatic_rename: false,
        active: index == 0,
    }
}

pub fn pane(id: &str, window_id: &str, session_id: &str, active: bool, cmd: &str, path: &str) -> Pane {
    Pane {
        id: id.into(),
        window_id: window_id.into(),
        session_id: session_id.into(),
        index: 0,
        active,
        sidebar: false,
        current_command: cmd.into(),
        current_path: path.into(),
        title: String::new(),
    }
}

/// alpha ($0, attached): 0 editor (nvim in ~/dev/dotfiles), 1 "win two"
/// (sh in ~); beta ($1): 0 logs (sh in /var/log, plus an ACTIVE sidebar
/// pane running sleep), 1 build (cargo in ~/dev/api). The client is on
/// alpha:editor.
pub fn snap() -> Snapshot {
    let mut side = pane("%3", "@2", "$1", true, "sleep", "/var/log");
    side.sidebar = true;
    side.index = 1;
    Snapshot {
        sessions: vec![
            Session { id: "$0".into(), name: "alpha".into(), attached: 1 },
            Session { id: "$1".into(), name: "beta".into(), attached: 0 },
        ],
        windows: vec![
            window("@0", "$0", 0, "editor"),
            window("@1", "$0", 1, "win two"),
            window("@2", "$1", 0, "logs"),
            window("@3", "$1", 1, "build"),
        ],
        panes: vec![
            pane("%0", "@0", "$0", true, "nvim", "/home/u/dev/dotfiles"),
            pane("%1", "@1", "$0", true, "sh", "/home/u"),
            pane("%2", "@2", "$1", false, "sh", "/var/log"),
            side,
            pane("%4", "@3", "$1", true, "cargo", "/home/u/dev/api"),
        ],
        clients: vec![Client {
            name: CLIENT.into(),
            tty: CLIENT.into(),
            session_id: "$0".into(),
            window_id: "@0".into(),
        }],
    }
}

/// Adds a window with one pane (`%n` for `@n`) running `cmd` in HOME.
pub fn add_window(s: &mut Snapshot, id: &str, session_id: &str, index: u32, name: &str, cmd: &str) {
    s.windows.push(window(id, session_id, index, name));
    s.panes.push(pane(&id.replace('@', "%"), id, session_id, true, cmd, HOME));
}

/// Adds a detached session with one idle window.
pub fn add_session(s: &mut Snapshot, id: &str, name: &str, window_id: &str, window_name: &str) {
    s.sessions.push(Session { id: id.into(), name: name.into(), attached: 0 });
    add_window(s, window_id, id, 0, window_name, "sh");
}

pub fn drop_window(s: &mut Snapshot, id: &str) {
    s.windows.retain(|w| w.id != id);
    s.panes.retain(|p| p.window_id != id);
}
```

- [ ] **Step 3: Write the failing tests.** `src/popup/rows.rs` (tests only for now):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::testutil::*;

    #[test]
    fn one_row_per_window() {
        assert_eq!(build(&snap(), Some(CLIENT), HOME).len(), 4);
    }

    #[test]
    fn no_client_sessions_by_name_windows_by_index() {
        let mut s = snap();
        s.sessions.reverse();
        s.windows.reverse();
        let rows = build(&s, None, HOME);
        let got: Vec<(&str, u32, &str)> = rows
            .iter()
            .map(|r| (r.session_name.as_str(), r.index, r.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [("alpha", 0, "editor"), ("alpha", 1, "win two"), ("beta", 0, "logs"), ("beta", 1, "build")]
        );
        assert!(rows.iter().all(|r| !r.current));
    }

    #[test]
    fn rows_carry_session_and_window_ids() {
        let rows = build(&snap(), Some(CLIENT), HOME);
        let r = &rows[0];
        assert_eq!(
            (r.session_id.as_str(), r.window_id.as_str(), r.pane_id.as_deref()),
            ("$0", "@0", Some("%0"))
        );
    }

    #[test]
    fn sidebar_pane_never_the_row_pane() {
        let rows = build(&snap(), Some(CLIENT), HOME);
        let logs = rows.iter().find(|r| r.window_id == "@2").unwrap();
        assert_eq!(logs.pane_id.as_deref(), Some("%2"));
        assert_eq!((logs.command.as_str(), logs.short_path.as_str()), ("sh", "/v/log"));
    }

    #[test]
    fn window_of_only_sidebars_has_no_pane() {
        let mut s = snap();
        s.panes.retain(|p| p.id != "%2");
        let rows = build(&s, Some(CLIENT), HOME);
        let logs = rows.iter().find(|r| r.window_id == "@2").unwrap();
        assert_eq!(logs.pane_id, None);
        assert_eq!(logs.command, "");
    }

    #[test]
    fn client_session_first() {
        let mut s = snap();
        s.clients[0].session_id = "$1".into();
        s.clients[0].window_id = "@3".into();
        let got: Vec<String> = build(&s, Some(CLIENT), HOME)
            .iter()
            .map(|r| format!("{} {}", r.session_name, r.name))
            .collect();
        assert_eq!(got, ["beta logs", "beta build", "alpha editor", "alpha win two"]);
    }

    #[test]
    fn current_window_marked() {
        let rows = build(&snap(), Some(CLIENT), HOME);
        let cur: Vec<&str> = rows.iter().filter(|r| r.current).map(|r| r.window_id.as_str()).collect();
        assert_eq!(cur, ["@0"]);
    }

    #[test]
    fn short_paths() {
        assert_eq!(short_path("/home/u", "/home/u"), "~");
        assert_eq!(short_path("/home/u/dev/tmux-home", "/home/u"), "~/d/tmux-home");
        assert_eq!(short_path("/usr/local/bin", "/home/u"), "/u/l/bin");
        assert_eq!(short_path("/", "/home/u"), "/");
        assert_eq!(short_path("/home/user2/x", "/home/u"), "/h/u/x");
        assert_eq!(short_path("/home/u/éclair/x", "/home/u"), "~/é/x");
        assert_eq!(short_path("/a/b", ""), "/a/b");
    }
}
```

`src/popup/filter.rs` (tests only for now):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        popup::{rows::build, testutil::*},
        tmux::snapshot::Snapshot,
    };

    fn names(query: &str, s: &Snapshot) -> Vec<String> {
        let rows = build(s, Some(CLIENT), HOME);
        Filter::default()
            .matching(query, &rows)
            .into_iter()
            .map(|i| rows[i].name.clone())
            .collect()
    }

    #[test]
    fn empty_query_matches_all_in_order() {
        assert_eq!(names("", &snap()), ["editor", "win two", "logs", "build"]);
        assert_eq!(names("  ", &snap()).len(), 4);
    }

    #[test]
    fn fuzzy_keeps_row_order() {
        assert_eq!(names("bui", &snap()), ["build"]);
        assert_eq!(names("a", &snap()), ["editor", "win two", "logs", "build"]);
    }

    #[test]
    fn matches_session_command_and_path() {
        assert_eq!(names("beta", &snap()), ["logs", "build"]);
        assert_eq!(names("cargo", &snap()), ["build"]);
        assert_eq!(names("dotfiles", &snap()), ["editor"]);
    }

    #[test]
    fn special_characters_are_literal() {
        let mut s = snap();
        s.windows[1].name = "renamed (x)+y, z".into();
        assert_eq!(names("(x)+", &s), ["renamed (x)+y, z"]);
    }

    #[test]
    fn smart_case() {
        assert!(names("Editor", &snap()).is_empty());
        assert_eq!(names("editor", &snap()), ["editor"]);
    }

    #[test]
    fn unicode_names_match() {
        let mut s = snap();
        s.windows[1].name = "日本語 ✳".into();
        assert_eq!(names("日本", &s), ["日本語 ✳"]);
    }
}
```

- [ ] **Step 4: Run them to see them fail**

Run: `cargo test --lib popup`
Expected: compile errors: `build`, `short_path` and `Filter` not found.

- [ ] **Step 5: Implement `src/popup/rows.rs`** (above its tests):

```rust
//! The list's rows (pure): one per window, grouped by session — the
//! invoking client's session first, then by name — windows in index order.

use crate::{model, tmux::snapshot::Snapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinRow {
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub index: u32,
    pub name: String,
    pub automatic_rename: bool,
    /// The pane the row describes and the preview captures: the active pane
    /// unless it is a sidebar, else the first non-sidebar pane.
    pub pane_id: Option<String>,
    pub command: String,
    /// That pane's cwd, in full (new windows open here).
    pub path: String,
    pub short_path: String,
    /// The invoking client is showing this window.
    pub current: bool,
}

pub fn build(snap: &Snapshot, client: Option<&str>, home: &str) -> Vec<WinRow> {
    let loc = model::client_location(snap, client);
    let cur_sid = loc.map(|(sid, _)| sid);
    let mut rows: Vec<WinRow> = snap
        .windows
        .iter()
        .map(|w| {
            let panes: Vec<_> = snap
                .panes
                .iter()
                .filter(|p| p.window_id == w.id && p.session_id == w.session_id && !p.sidebar)
                .collect();
            let pane = panes.iter().find(|p| p.active).or(panes.first()).copied();
            WinRow {
                session_id: w.session_id.clone(),
                session_name: snap
                    .sessions
                    .iter()
                    .find(|s| s.id == w.session_id)
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
                window_id: w.id.clone(),
                index: w.index,
                name: w.name.clone(),
                automatic_rename: w.automatic_rename,
                pane_id: pane.map(|p| p.id.clone()),
                command: pane.map(|p| p.current_command.clone()).unwrap_or_default(),
                path: pane.map(|p| p.current_path.clone()).unwrap_or_default(),
                short_path: pane.map(|p| short_path(&p.current_path, home)).unwrap_or_default(),
                current: loc == Some((w.session_id.as_str(), w.id.as_str())),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let other = |r: &WinRow| Some(r.session_id.as_str()) != cur_sid;
        other(a)
            .cmp(&other(b))
            .then_with(|| a.session_name.cmp(&b.session_name))
            .then(a.index.cmp(&b.index))
    });
    rows
}

/// `~/dev/tmux-home` → `~/d/tmux-home`: home as `~`, every directory but the
/// last cut to its first character.
pub fn short_path(path: &str, home: &str) -> String {
    if !home.is_empty() && path == home {
        return "~".to_string();
    }
    let tilde;
    let path = match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && rest.starts_with('/') => {
            tilde = format!("~{rest}");
            tilde.as_str()
        }
        _ => path,
    };
    let parts: Vec<&str> = path.split('/').collect();
    let last = parts.len() - 1;
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| match p.chars().next() {
            Some(c) if i > 0 && i < last && p.chars().count() > 1 => c.to_string(),
            _ => p.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}
```

`src/popup/filter.rs` (above its tests):

```rust
//! Type-first fuzzy filter (nucleo-matcher; fzf-like syntax: space-separated
//! terms, `'exact`, `^prefix`, `suffix$`, `!not`). Matches keep the list's
//! order — the grouping is the order, not the score.

use super::rows::WinRow;
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

pub struct Filter {
    matcher: Matcher,
    buf: Vec<char>,
}

impl Default for Filter {
    fn default() -> Filter {
        Filter {
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
        }
    }
}

impl Filter {
    /// What a row is matched against: the columns the bash popup showed.
    pub fn haystack(r: &WinRow) -> String {
        format!("{} {} {} {} {}", r.session_name, r.index, r.name, r.command, r.short_path)
    }

    /// Indexes of the rows matching `query`, in row order.
    pub fn matching(&mut self, query: &str, rows: &[WinRow]) -> Vec<usize> {
        if query.trim().is_empty() {
            return (0..rows.len()).collect();
        }
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut out = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let h = Self::haystack(r);
            if pattern.score(Utf32Str::new(&h, &mut self.buf), &mut self.matcher).is_some() {
                out.push(i);
            }
        }
        out
    }
}
```

- [ ] **Step 6: Run them to see them pass**

Run: `cargo test --lib popup`
Expected: 8 rows tests and 6 filter tests pass.

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock src/lib.rs src/popup
timeout 60 git commit -m "popup: grouped rows and fuzzy filter"
```

---

### Task 8: `LineEdit` — the one-line editor

This one editor serves the filter, the rename editor and the new-window name editor.

**Files:**
- Create: `src/popup/edit.rs`
- Modify: `src/popup/mod.rs` (add `pub mod edit;`), `Cargo.toml` (`crossterm`)

**Interfaces:**
- Produces:
  - `popup::edit::EditResult { Ignored, Moved, Changed }`
  - `popup::edit::LineEdit` (Clone, Debug, Default, PartialEq, Eq), with `new(&str)` (cursor at the end), `text() -> &str`, `cursor() -> usize` (chars), `before_cursor() -> &str`, `is_empty()`, `clear()` and `handle(KeyEvent) -> EditResult`
  - Keys: printable chars (with or without Shift) insert. `Backspace`/`^h` delete back, `Delete` deletes forward, `←` `→` `Home` `End` `^a` `^e` move, `^u` kills to the start, `^w` kills the previous word. Every other key is `Ignored`, so callers can use it.

- [ ] **Step 1: Add the dependency and check it unifies with ratatui's**

```bash
cargo add crossterm
grep -n "x1A'\|x1F'" ~/.cargo/registry/src/*/crossterm-0.29.*/src/event/sys/unix/parse.rs
```

Expected: crossterm 0.29.x. In raw mode `0x01–0x1A` parse as `Char('a'..'z') + CONTROL` (so `^j` is `Ctrl+'j'` and `^h` is `Ctrl+'h'`), and `0x1C–0x1F` as `Char('4'..'7') + CONTROL`, so `^/` (0x1f) arrives as **`Ctrl+'7'`**. `Esc x` arrives as `Alt+x`. Task 9 relies on all of this.

- [ ] **Step 2: Write the failing tests** — create `src/popup/edit.rs` with the tests, and add `pub mod edit;` to `src/popup/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode::*, KeyEvent, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(Char(c), KeyModifiers::CONTROL)
    }
    fn typed(e: &mut LineEdit, s: &str) {
        for c in s.chars() {
            e.handle(k(Char(c)));
        }
    }

    #[test]
    fn new_puts_the_cursor_at_the_end() {
        let e = LineEdit::new("abc");
        assert_eq!((e.text(), e.cursor()), ("abc", 3));
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut e = LineEdit::new("ac");
        e.handle(k(Left));
        assert_eq!(e.handle(k(Char('b'))), EditResult::Changed);
        assert_eq!(e.text(), "abc");
        assert_eq!(e.before_cursor(), "ab");
    }

    #[test]
    fn shifted_characters_insert() {
        let mut e = LineEdit::default();
        e.handle(KeyEvent::new(Char('A'), KeyModifiers::SHIFT));
        typed(&mut e, "(x)+");
        assert_eq!(e.text(), "A(x)+");
    }

    #[test]
    fn backspace_delete_and_ctrl_h() {
        let mut e = LineEdit::new("abcd");
        e.handle(k(Backspace));
        assert_eq!(e.text(), "abc");
        e.handle(ctrl('h'));
        assert_eq!(e.text(), "ab");
        e.handle(k(Home));
        e.handle(k(Delete));
        assert_eq!((e.text(), e.cursor()), ("b", 0));
        assert_eq!(e.handle(k(Backspace)), EditResult::Ignored);
    }

    #[test]
    fn home_end_and_ctrl_a_e() {
        let mut e = LineEdit::new("abc");
        assert_eq!(e.handle(ctrl('a')), EditResult::Moved);
        assert_eq!(e.cursor(), 0);
        e.handle(k(End));
        assert_eq!(e.cursor(), 3);
        e.handle(k(Home));
        e.handle(ctrl('e'));
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn ctrl_u_kills_to_start() {
        let mut e = LineEdit::new("hello world");
        for _ in 0..5 {
            e.handle(k(Left));
        }
        e.handle(ctrl('u'));
        assert_eq!((e.text(), e.cursor()), ("world", 0));
    }

    #[test]
    fn ctrl_w_kills_the_previous_word() {
        let mut e = LineEdit::new("foo bar  ");
        e.handle(ctrl('w'));
        assert_eq!(e.text(), "foo ");
        e.handle(ctrl('w'));
        assert_eq!(e.text(), "");
    }

    #[test]
    fn multibyte_text_edits_by_character() {
        let mut e = LineEdit::new("日本語✳");
        e.handle(k(Backspace));
        assert_eq!(e.text(), "日本語");
        e.handle(k(Left));
        e.handle(k(Backspace));
        assert_eq!(e.text(), "日語");
        assert_eq!(e.before_cursor(), "日");
    }

    #[test]
    fn moves_report_moved_and_other_keys_are_ignored() {
        let mut e = LineEdit::new("ab");
        assert_eq!(e.handle(k(Right)), EditResult::Ignored);
        assert_eq!(e.handle(k(Left)), EditResult::Moved);
        assert_eq!(e.handle(ctrl('x')), EditResult::Ignored);
        assert_eq!(e.handle(KeyEvent::new(Char('r'), KeyModifiers::ALT)), EditResult::Ignored);
        assert_eq!(e.handle(k(Enter)), EditResult::Ignored);
        assert_eq!(e.text(), "ab");
    }
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test --lib popup::edit`
Expected: compile errors: `LineEdit` and `EditResult` not found.

- [ ] **Step 4: Implement** (above the tests):

```rust
//! A one-line text editor: the filter, the rename editor and the new-window
//! name. Keys it doesn't use come back `Ignored` for the caller.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditResult {
    Ignored,
    Moved,
    Changed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// In characters, 0..=len.
    cursor: usize,
}

impl LineEdit {
    pub fn new(text: &str) -> LineEdit {
        LineEdit {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn before_cursor(&self) -> &str {
        &self.text[..self.byte(self.cursor)]
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn handle(&mut self, k: KeyEvent) -> EditResult {
        let plain = k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT;
        let ctrl = k.modifiers == KeyModifiers::CONTROL;
        match k.code {
            KeyCode::Char(c) if plain => {
                let at = self.byte(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
                EditResult::Changed
            }
            KeyCode::Backspace if plain => self.delete_back(1),
            KeyCode::Char('h') if ctrl => self.delete_back(1),
            KeyCode::Delete if plain => {
                if self.cursor < self.len() {
                    let at = self.byte(self.cursor);
                    self.text.remove(at);
                    EditResult::Changed
                } else {
                    EditResult::Ignored
                }
            }
            KeyCode::Left if plain => self.move_to(self.cursor.saturating_sub(1)),
            KeyCode::Right if plain => self.move_to((self.cursor + 1).min(self.len())),
            KeyCode::Home if plain => self.move_to(0),
            KeyCode::Char('a') if ctrl => self.move_to(0),
            KeyCode::End if plain => self.move_to(self.len()),
            KeyCode::Char('e') if ctrl => self.move_to(self.len()),
            KeyCode::Char('u') if ctrl => self.delete_back(self.cursor),
            KeyCode::Char('w') if ctrl => self.delete_back(self.word_back()),
            _ => EditResult::Ignored,
        }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset of character `ci`.
    fn byte(&self, ci: usize) -> usize {
        self.text.char_indices().nth(ci).map_or(self.text.len(), |(b, _)| b)
    }

    fn move_to(&mut self, c: usize) -> EditResult {
        if c == self.cursor {
            EditResult::Ignored
        } else {
            self.cursor = c;
            EditResult::Moved
        }
    }

    fn delete_back(&mut self, n: usize) -> EditResult {
        let n = n.min(self.cursor);
        if n == 0 {
            return EditResult::Ignored;
        }
        let (from, to) = (self.byte(self.cursor - n), self.byte(self.cursor));
        self.text.replace_range(from..to, "");
        self.cursor -= n;
        EditResult::Changed
    }

    /// Characters from the cursor back to the start of the previous word
    /// (trailing spaces, then the word).
    fn word_back(&self) -> usize {
        let before: Vec<char> = self.before_cursor().chars().collect();
        let mut i = before.len();
        while i > 0 && before[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && before[i - 1] != ' ' {
            i -= 1;
        }
        before.len() - i
    }
}
```

- [ ] **Step 5: Run them to see them pass**

Run: `cargo test --lib popup::edit`
Expected: 9 passed.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock src/popup/edit.rs src/popup/mod.rs
timeout 60 git commit -m "popup: one-line editor"
```

---

### Task 9: `App` core — list mode, layout, live snapshots

This is the pure state machine for list mode: filter, navigation, `⏎`, `Esc`, help, `^o`, preview placement, and applying snapshots with the selection kept by ID. Editing and management keys come in Task 10.

**Files:**
- Create: `src/popup/layout.rs`, `src/popup/app.rs`
- Modify: `src/popup/mod.rs` (add `pub mod app; pub mod layout;`), `src/popup/testutil.rs` (key and app helpers), `Cargo.toml` (`ratatui`)

**Interfaces:**
- Consumes: `rows::{build, WinRow}`, `filter::Filter`, `edit::{LineEdit, EditResult}`, `model::client_location`.
- Produces:
  - `layout::PreviewPos { Right, Bottom { percent: u16 } }`, `layout::PreviewLayout { pos, hidden }`, `layout::preview_layout(cols, rows)`, `layout::Regions { header, filter, list, preview: Option<Rect>, footer }` and `layout::regions(area: Rect, p: PreviewLayout) -> Regions`
  - `app::Effect`: `Quit`, `Switch { session_id, window_id }`, `Rename { window_id, name }`, `ResetName { window_id }`, `Close { window_id, confirmed }`, `Reopen`, `Swap { a, b }`, `NewWindow { after, cwd, name }`
  - `app::Mode`: `List`, `Help`, `Rename { window_id, edit }`, `NewWindow { after, cwd, edit }`, `Confirm { window_id, prompt }`
  - `app::ViewItem`: `Header { name, windows, attached }`, `Window { row: Box<WinRow>, selected, editor: Option<(String, usize)> }`, `NewWindow { text, cursor }`
  - `app::App`:
    - `new(client: Option<String>, home: String, size: (u16, u16))`
    - inputs: `on_snapshot(seq: u64, Snapshot)` (seq 0 = direct read, always applied), `on_resize(cols, rows)`, `on_key(KeyEvent) -> Vec<Effect>`, `on_preview(pane: String, text: String)`, `on_error(String)`
    - reads: `header() -> String`, `counts() -> (usize, usize)`, `selected_row() -> Option<&WinRow>`, `capture_target() -> Option<String>`, `preview_text() -> Option<&str>`, `page_size() -> usize`, `view_items() -> (Vec<ViewItem>, Option<usize>)`
    - public fields: `filter: LineEdit`, `mode: Mode`, `notice: Option<String>`, `preview: PreviewLayout`, `list_state: ListState`
  - testutil additions: `key(KeyCode)`, `ch(char)`, `ctrl(char)`, `alt(KeyCode)`, `app()`, `app_with(Snapshot)`, `typed(&mut App, &str)`, `selected(&App) -> String`

- [ ] **Step 1: Add the dependency and check the APIs used**

```bash
cargo add ratatui
cargo tree -i crossterm --depth 0
grep -n "pub fn areas\|pub fn vertical\|pub fn horizontal" ~/.cargo/registry/src/*/ratatui-core-0.1.*/src/layout/layout.rs
grep -n "pub fn select\b\|pub fn select(\|pub const fn offset" ~/.cargo/registry/src/*/ratatui-widgets-0.3.*/src/list/state.rs
```

Expected: `cargo tree` shows exactly one crossterm (0.29.x). `Layout::vertical`/`horizontal(constraints)`, `Layout::areas::<N>(area) -> [Rect; N]`, `ListState::select(Option<usize>)` and `ListState::offset()` exist (ratatui 0.30.2: ratatui-core 0.1.2, ratatui-widgets 0.3.2).

- [ ] **Step 2: Write `src/popup/layout.rs` test-first.** Tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_layout_by_size() {
        assert_eq!(preview_layout(200, 50), PreviewLayout { pos: PreviewPos::Right, hidden: false });
        assert_eq!(
            preview_layout(150, 40),
            PreviewLayout { pos: PreviewPos::Bottom { percent: 33 }, hidden: false }
        );
        assert_eq!(
            preview_layout(120, 20),
            PreviewLayout { pos: PreviewPos::Bottom { percent: 50 }, hidden: true }
        );
    }

    #[test]
    fn regions_200x50_right_half() {
        let r = regions(Rect::new(0, 0, 200, 50), preview_layout(200, 50));
        assert_eq!(r.header, Rect::new(0, 0, 200, 1));
        assert_eq!(r.filter, Rect::new(0, 1, 200, 1));
        assert_eq!(r.list, Rect::new(0, 2, 100, 47));
        assert_eq!(r.preview, Some(Rect::new(100, 2, 100, 47)));
        assert_eq!(r.footer, Rect::new(0, 49, 200, 1));
    }

    #[test]
    fn regions_150x40_bottom_third() {
        let r = regions(Rect::new(0, 0, 150, 40), preview_layout(150, 40));
        let p = r.preview.unwrap();
        assert_eq!((r.list.y, r.list.width, p.width), (2, 150, 150));
        assert_eq!(p.y, r.list.y + r.list.height);
        assert_eq!(r.list.height + p.height, 37);
        assert!((11..=13).contains(&p.height), "{p:?}");
    }

    #[test]
    fn hidden_preview_gives_the_list_everything() {
        let r = regions(Rect::new(0, 0, 120, 20), preview_layout(120, 20));
        assert_eq!(r.preview, None);
        assert_eq!(r.list, Rect::new(0, 2, 120, 17));
    }

    #[test]
    fn tiny_areas_do_not_panic() {
        for (w, h) in [(20, 3), (10, 1), (1, 1), (0, 0)] {
            let _ = regions(Rect::new(0, 0, w, h), preview_layout(w, h));
        }
    }
}
```

Implementation (above the tests):

```rust
//! Where things go on screen (SPEC §5): header, filter line, list, preview
//! (right half ≥ 160 columns, bottom third ≥ 30 rows, else hidden), footer.

use ratatui::layout::{Constraint, Layout, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewPos {
    Right,
    Bottom { percent: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewLayout {
    pub pos: PreviewPos,
    /// `^o` toggles this; a resize recomputes the whole layout.
    pub hidden: bool,
}

/// Right half at ≥ 160 columns, bottom third at ≥ 30 rows, otherwise hidden
/// (`^o` then shows it at the bottom, half height).
pub fn preview_layout(cols: u16, rows: u16) -> PreviewLayout {
    if cols >= 160 {
        PreviewLayout { pos: PreviewPos::Right, hidden: false }
    } else if rows >= 30 {
        PreviewLayout { pos: PreviewPos::Bottom { percent: 33 }, hidden: false }
    } else {
        PreviewLayout { pos: PreviewPos::Bottom { percent: 50 }, hidden: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regions {
    pub header: Rect,
    pub filter: Rect,
    pub list: Rect,
    pub preview: Option<Rect>,
    pub footer: Rect,
}

pub fn regions(area: Rect, p: PreviewLayout) -> Regions {
    let [header, filter, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let (list, preview) = match (p.hidden, p.pos) {
        (true, _) => (body, None),
        (false, PreviewPos::Right) => {
            let [l, r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(body);
            (l, Some(r))
        }
        (false, PreviewPos::Bottom { percent }) => {
            let [l, b] = Layout::vertical([
                Constraint::Percentage(100 - percent),
                Constraint::Percentage(percent),
            ])
            .areas(body);
            (l, Some(b))
        }
    };
    Regions { header, filter, list, preview, footer }
}
```

Add `pub mod layout;` to `src/popup/mod.rs`. Run `cargo test --lib popup::layout`. If you wrote the tests alone first it fails (`preview_layout` not found). With the implementation in place it gives 5 passed.

- [ ] **Step 3: Extend the fixtures** — append to `src/popup/testutil.rs`:

```rust
use super::app::App;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
pub fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}
pub fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
pub fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

/// An App at 200x50 showing `snap()`, its client on alpha:editor.
pub fn app() -> App {
    app_with(snap())
}
pub fn app_with(s: Snapshot) -> App {
    let mut a = App::new(Some(CLIENT.into()), HOME.into(), (200, 50));
    a.on_snapshot(1, s);
    a
}
pub fn typed(a: &mut App, text: &str) {
    for c in text.chars() {
        a.on_key(ch(c));
    }
}
/// The selected window's ID ("" when nothing is selected).
pub fn selected(a: &App) -> String {
    a.selected_row().map(|r| r.window_id.clone()).unwrap_or_default()
}
```

(move the new `use` lines to the top of the file next to the existing one).

- [ ] **Step 4: Write the failing App tests** — add `pub mod app;` to `src/popup/mod.rs` and create `src/popup/app.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::testutil::*;
    use crossterm::event::KeyCode::*;

    #[test]
    fn list_starts_on_client_window() {
        let a = app();
        assert_eq!(selected(&a), "@0");
        assert!(a.selected_row().unwrap().current);
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn header_follows_client() {
        let mut a = app();
        assert_eq!(a.header(), " tmux-home   alpha ▸ 0");
        let mut s = snap();
        s.clients[0].session_id = "$1".into();
        s.clients[0].window_id = "@3".into();
        a.on_snapshot(0, s);
        assert_eq!(a.header(), " tmux-home   beta ▸ 1");
        let mut none = App::new(None, HOME.into(), (200, 50));
        none.on_snapshot(0, snap());
        assert_eq!(none.header(), " tmux-home");
    }

    #[test]
    fn typing_filters_and_returns_to_top() {
        let mut a = app();
        a.on_key(key(Down));
        typed(&mut a, "bui");
        assert_eq!(a.counts(), (1, 4));
        assert_eq!(selected(&a), "@3");
        for _ in 0..3 {
            a.on_key(key(Backspace));
        }
        assert_eq!(a.counts(), (4, 4));
        assert_eq!(selected(&a), "@0", "back to the top as the filter changes");
    }

    #[test]
    fn esc_clears_then_quits() {
        let mut a = app();
        typed(&mut a, "bui");
        assert!(a.on_key(key(Esc)).is_empty());
        assert_eq!((a.filter.text(), a.counts()), ("", (4, 4)));
        assert_eq!(selected(&a), "@0");
        assert_eq!(a.on_key(key(Esc)), vec![Effect::Quit]);
    }

    #[test]
    fn navigation_keys_cycle() {
        let mut a = app();
        let mut go = |k: KeyEvent| {
            a.on_key(k);
            selected(&a)
        };
        assert_eq!(go(ctrl('n')), "@1");
        assert_eq!(go(ctrl('j')), "@2");
        assert_eq!(go(key(Down)), "@3");
        assert_eq!(go(key(Down)), "@0", "wraps");
        assert_eq!(go(ctrl('p')), "@3");
        assert_eq!(go(ctrl('k')), "@2");
        assert_eq!(go(key(Up)), "@1");
    }

    #[test]
    fn page_keys_move_by_list_height() {
        let mut s = snap();
        for i in 2..60u32 {
            add_window(&mut s, &format!("@{}", 100 + i), "$0", i, &format!("w{i}"), "sh");
        }
        let mut a = app_with(s);
        a.on_resize(120, 20); // preview hidden: the list has 20 - 3 = 17 rows
        assert_eq!(a.page_size(), 17);
        a.on_key(key(PageDown));
        assert_eq!(a.selected_row().unwrap().index, 17);
        a.on_key(key(PageUp));
        assert_eq!(a.selected_row().unwrap().index, 0);
        a.on_resize(20, 3); // no room for list rows at all: still moves by one
        assert_eq!(a.page_size(), 1);
    }

    #[test]
    fn enter_switches_invoking_client_and_quits() {
        let mut a = app();
        typed(&mut a, "bui");
        assert_eq!(
            a.on_key(key(Enter)),
            vec![
                Effect::Switch { session_id: "$1".into(), window_id: "@3".into() },
                Effect::Quit
            ]
        );
    }

    #[test]
    fn enter_with_no_match_does_nothing() {
        let mut a = app();
        typed(&mut a, "zzz");
        assert_eq!(a.counts(), (0, 4));
        assert!(a.on_key(key(Enter)).is_empty());
    }

    #[test]
    fn ctrl_c_and_ctrl_q_quit() {
        assert_eq!(app().on_key(ctrl('c')), vec![Effect::Quit]);
        assert_eq!(app().on_key(ctrl('q')), vec![Effect::Quit]);
    }

    #[test]
    fn ctrl_o_toggles_preview() {
        let mut a = app();
        assert!(!a.preview.hidden);
        assert_eq!(a.capture_target().as_deref(), Some("%0"));
        a.on_key(ctrl('o'));
        assert!(a.preview.hidden);
        assert_eq!(a.capture_target(), None);
        a.on_key(ctrl('o'));
        assert!(!a.preview.hidden);
    }

    #[test]
    fn help_opens_and_any_key_returns() {
        for k in [key(F(1)), ctrl('/'), ctrl('7')] {
            let mut a = app();
            a.on_key(k);
            assert_eq!(a.mode, Mode::Help, "{k:?}");
            assert!(a.on_key(ch('q')).is_empty());
            assert_eq!(a.mode, Mode::List);
            assert_eq!(a.filter.text(), "", "the key that leaves help is not typed");
        }
    }

    #[test]
    fn resize_recomputes_preview() {
        let mut a = app();
        a.on_resize(150, 40);
        assert_eq!(a.preview.pos, crate::popup::layout::PreviewPos::Bottom { percent: 33 });
        a.on_key(ctrl('o'));
        a.on_resize(200, 50);
        assert_eq!(a.preview, crate::popup::layout::preview_layout(200, 50));
    }

    #[test]
    fn snapshot_keeps_selection_by_id() {
        let mut a = app();
        a.on_key(key(Down)); // @1 "win two"
        let mut s = snap();
        s.windows.iter_mut().find(|w| w.id == "@1").unwrap().index = 2;
        add_window(&mut s, "@7", "$0", 1, "inserted", "sh");
        a.on_snapshot(0, s);
        assert_eq!(selected(&a), "@1");
        assert_eq!(a.view_items().1, Some(3), "one row lower");
    }

    #[test]
    fn vanished_selection_takes_the_next_row() {
        let mut a = app();
        a.on_key(key(Down));
        let mut s = snap();
        drop_window(&mut s, "@1");
        a.on_snapshot(0, s);
        assert_eq!(selected(&a), "@2");
    }

    #[test]
    fn vanished_last_row_takes_the_new_last() {
        let mut a = app();
        a.on_key(key(Up)); // wraps to @3
        let mut s = snap();
        drop_window(&mut s, "@3");
        a.on_snapshot(0, s);
        assert_eq!(selected(&a), "@2");
    }

    #[test]
    fn stale_seq_is_ignored() {
        let mut a = app(); // applied seq 1
        let mut s = snap();
        add_window(&mut s, "@7", "$1", 2, "new", "sh");
        a.on_snapshot(5, s);
        assert_eq!(a.counts(), (5, 5));
        a.on_snapshot(4, snap());
        assert_eq!(a.counts(), (5, 5), "older seq ignored");
        a.on_snapshot(0, snap());
        assert_eq!(a.counts(), (4, 4), "seq 0 (a direct read) always applies");
    }

    #[test]
    fn view_items_group_by_session() {
        let a = app();
        let (items, sel) = a.view_items();
        assert_eq!(sel, Some(1));
        let names: Vec<String> = items
            .iter()
            .map(|i| match i {
                ViewItem::Header { name, .. } => format!("# {name}"),
                ViewItem::Window { row, .. } => row.name.clone(),
                ViewItem::NewWindow { .. } => "+".into(),
            })
            .collect();
        assert_eq!(names, ["# alpha", "editor", "win two", "# beta", "logs", "build"]);
        assert!(matches!(&items[0], ViewItem::Header { windows: 2, attached: true, .. }));
        assert!(matches!(&items[3], ViewItem::Header { windows: 2, attached: false, .. }));
    }

    #[test]
    fn preview_text_only_for_the_selected_pane() {
        let mut a = app();
        a.on_preview("%0".into(), "hello".into());
        assert_eq!(a.preview_text(), Some("hello"));
        a.on_key(key(Down));
        assert_eq!(a.preview_text(), None);
        assert_eq!(a.capture_target().as_deref(), Some("%1"));
    }

    #[test]
    fn errors_show_until_the_next_key() {
        let mut a = app();
        a.on_error("tmux: boom".into());
        assert_eq!(a.notice.as_deref(), Some("tmux: boom"));
        a.on_key(key(Down));
        assert_eq!(a.notice, None);
    }
}
```

- [ ] **Step 5: Run them to see them fail**

Run: `cargo test --lib popup::app`
Expected: compile errors: `App`, `Effect`, `Mode` and `ViewItem` not found.

- [ ] **Step 6: Implement `src/popup/app.rs`** (above the tests):

```rust
//! The popup's state machine: keys and snapshots in, [`Effect`]s out. No I/O
//! here — `popup::exec` performs the effects through `tmux::ops::Ops` and
//! reports back (`on_busy`, `on_reopened`, `on_created`, `on_error`).

use super::{
    edit::{EditResult, LineEdit},
    filter::Filter,
    layout::{PreviewLayout, preview_layout, regions},
    rows::{self, WinRow},
};
use crate::{model, tmux::snapshot::Snapshot};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{layout::Rect, widgets::ListState};

/// A tmux write (or exit) for the runner. Targets are IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Leave the popup (`display-popup -E` then closes it).
    Quit,
    /// Switch the invoking client to this window.
    Switch { session_id: String, window_id: String },
    Rename { window_id: String, name: String },
    /// Turn automatic-rename back on.
    ResetName { window_id: String },
    /// Close the window. `confirmed: false` means the snapshot showed every
    /// pane idle: the runner re-checks before killing and asks
    /// (`App::on_busy`) if something started since.
    Close { window_id: String, confirmed: bool },
    /// Reopen the most recently closed window.
    Reopen,
    /// `swap-window -d -s a -t b`.
    Swap { a: String, b: String },
    /// A new window right after `after`, in `cwd`, named `name` (empty: automatic).
    NewWindow { after: String, cwd: String, name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Type-first list: printable keys edit the filter.
    List,
    /// The key reference; any key returns.
    Help,
    /// Inline rename: the window's row is the editor.
    Rename { window_id: String, edit: LineEdit },
    /// Inline name for a new window: an extra row under `after`.
    NewWindow { after: String, cwd: String, edit: LineEdit },
    /// `close "…"? (y/N)` on the filter line; only `y` closes.
    Confirm { window_id: String, prompt: String },
}

/// One line of the list, as the view draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewItem {
    Header { name: String, windows: usize, attached: bool },
    /// `editor`: the rename buffer and its cursor, when this row is being
    /// renamed. Boxed: a row is far bigger than the other variants.
    Window { row: Box<WinRow>, selected: bool, editor: Option<(String, usize)> },
    NewWindow { text: String, cursor: usize },
}

pub struct App {
    client: Option<String>,
    home: String,
    snap: Snapshot,
    last_seq: u64,
    /// The first snapshot puts the cursor on the client's current window.
    initial: bool,
    rows: Vec<WinRow>,
    /// Indexes into `rows` that match the filter, in row order.
    visible: Vec<usize>,
    matcher: Filter,
    pub filter: LineEdit,
    /// (session_id, window_id) under the cursor.
    selected: Option<(String, String)>,
    /// Its position in `visible`: where the cursor stays when it vanishes.
    sel_pos: usize,
    pub mode: Mode,
    /// One-line footer notice, cleared by the next key.
    pub notice: Option<String>,
    pub preview: PreviewLayout,
    size: (u16, u16),
    /// A window (reopened / created) to select as soon as a snapshot lists it.
    pending_select: Option<String>,
    /// (pane id, capture) of the last preview capture.
    preview_text: Option<(String, String)>,
    /// The list's scroll position, kept by the view between frames.
    pub list_state: ListState,
}

fn plain(k: &KeyEvent, code: KeyCode) -> bool {
    k.modifiers.is_empty() && k.code == code
}

fn is_ctrl(k: &KeyEvent, c: char) -> bool {
    k.modifiers == KeyModifiers::CONTROL && k.code == KeyCode::Char(c)
}

fn is_alt(k: &KeyEvent, code: KeyCode) -> bool {
    k.modifiers == KeyModifiers::ALT && k.code == code
}

impl App {
    pub fn new(client: Option<String>, home: String, size: (u16, u16)) -> App {
        App {
            client,
            home,
            snap: Snapshot::default(),
            last_seq: 0,
            initial: true,
            rows: Vec::new(),
            visible: Vec::new(),
            matcher: Filter::default(),
            filter: LineEdit::default(),
            selected: None,
            sel_pos: 0,
            mode: Mode::List,
            notice: None,
            preview: preview_layout(size.0, size.1),
            size,
            pending_select: None,
            preview_text: None,
            list_state: ListState::default(),
        }
    }

    // ---- inputs

    /// A new picture of the server. `seq` orders daemon snapshots (older ones
    /// are dropped); 0 marks a direct read, which always applies. The filter,
    /// the cursor (by ID) and any open editor survive.
    pub fn on_snapshot(&mut self, seq: u64, snap: Snapshot) {
        if seq != 0 {
            if seq <= self.last_seq {
                return;
            }
            self.last_seq = seq;
        }
        self.snap = snap;
        self.rows = rows::build(&self.snap, self.client.as_deref(), &self.home);
        self.refilter();
        if std::mem::take(&mut self.initial) {
            let start = self
                .visible
                .iter()
                .position(|&i| self.rows[i].current)
                .unwrap_or(0);
            self.select_or_clear(start);
            return;
        }
        if self.apply_pending() {
            return;
        }
        self.reselect();
    }

    pub fn on_resize(&mut self, cols: u16, rows: u16) {
        self.size = (cols, rows);
        self.preview = preview_layout(cols, rows);
    }

    pub fn on_preview(&mut self, pane: String, text: String) {
        self.preview_text = Some((pane, text));
    }

    pub fn on_error(&mut self, msg: String) {
        self.notice = Some(msg);
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Vec<Effect> {
        self.notice = None;
        match self.mode {
            Mode::Help => {
                self.mode = Mode::List;
                Vec::new()
            }
            Mode::List => self.key_list(k),
            Mode::Rename { .. } | Mode::NewWindow { .. } | Mode::Confirm { .. } => Vec::new(),
        }
    }

    fn key_list(&mut self, k: KeyEvent) -> Vec<Effect> {
        if plain(&k, KeyCode::Enter) {
            return match self.selected_row() {
                Some(r) => vec![
                    Effect::Switch {
                        session_id: r.session_id.clone(),
                        window_id: r.window_id.clone(),
                    },
                    Effect::Quit,
                ],
                None => Vec::new(),
            };
        }
        if plain(&k, KeyCode::Esc) {
            if self.filter.is_empty() {
                return vec![Effect::Quit];
            }
            self.filter.clear();
            self.refilter_to_top();
            return Vec::new();
        }
        if is_ctrl(&k, 'c') || is_ctrl(&k, 'q') {
            return vec![Effect::Quit];
        }
        if plain(&k, KeyCode::Up) || is_ctrl(&k, 'p') || is_ctrl(&k, 'k') {
            self.step(-1);
        } else if plain(&k, KeyCode::Down) || is_ctrl(&k, 'n') || is_ctrl(&k, 'j') {
            self.step(1);
        } else if plain(&k, KeyCode::PageUp) {
            self.page(-1);
        } else if plain(&k, KeyCode::PageDown) {
            self.page(1);
        } else if plain(&k, KeyCode::F(1)) || is_ctrl(&k, '/') || is_ctrl(&k, '7') {
            // ^/ is byte 0x1f, which crossterm reports as Ctrl+'7'
            self.mode = Mode::Help;
        } else if is_ctrl(&k, 'o') {
            self.preview.hidden = !self.preview.hidden;
        } else if self.filter.handle(k) == EditResult::Changed {
            self.refilter_to_top();
        }
        Vec::new()
    }

    // ---- reads (view, runner)

    /// " tmux-home   <session> ▸ <index>" for the invoking client.
    pub fn header(&self) -> String {
        let mut h = String::from(" tmux-home");
        if let Some((sid, wid)) = model::client_location(&self.snap, self.client.as_deref())
            && let Some(s) = self.snap.sessions.iter().find(|s| s.id == sid)
            && let Some(w) = self
                .snap
                .windows
                .iter()
                .find(|w| w.id == wid && w.session_id == sid)
        {
            h.push_str(&format!("   {} ▸ {}", s.name, w.index));
        }
        h
    }

    /// (matching, total) windows.
    pub fn counts(&self) -> (usize, usize) {
        (self.visible.len(), self.rows.len())
    }

    pub fn selected_row(&self) -> Option<&WinRow> {
        let (sid, wid) = self.selected.as_ref()?;
        self.visible
            .iter()
            .map(|&i| &self.rows[i])
            .find(|r| r.session_id == *sid && r.window_id == *wid)
    }

    /// The pane the preview should show now, if it is visible.
    pub fn capture_target(&self) -> Option<String> {
        if self.preview.hidden || self.mode == Mode::Help {
            return None;
        }
        self.selected_row()?.pane_id.clone()
    }

    /// The last capture, if it is of the selected row's pane.
    pub fn preview_text(&self) -> Option<&str> {
        let (pane, text) = self.preview_text.as_ref()?;
        (self.selected_row()?.pane_id.as_ref() == Some(pane)).then_some(text.as_str())
    }

    /// Rows a PgUp/PgDn moves: the list's height (at least 1).
    pub fn page_size(&self) -> usize {
        let area = Rect::new(0, 0, self.size.0, self.size.1);
        (regions(area, self.preview).list.height as usize).max(1)
    }

    /// The list as drawn — a header per session, then its matching windows —
    /// and the index of the selected item.
    pub fn view_items(&self) -> (Vec<ViewItem>, Option<usize>) {
        let mut items = Vec::new();
        let mut selected_at = None;
        let mut group: Option<&str> = None;
        for &i in &self.visible {
            let r = &self.rows[i];
            if group != Some(r.session_id.as_str()) {
                group = Some(r.session_id.as_str());
                items.push(ViewItem::Header {
                    name: r.session_name.clone(),
                    windows: self.rows.iter().filter(|x| x.session_id == r.session_id).count(),
                    attached: self
                        .snap
                        .sessions
                        .iter()
                        .any(|s| s.id == r.session_id && s.attached > 0),
                });
            }
            let selected = self
                .selected
                .as_ref()
                .is_some_and(|(s, w)| *s == r.session_id && *w == r.window_id);
            let editor = match &self.mode {
                Mode::Rename { window_id, edit } if *window_id == r.window_id => {
                    Some((edit.text().to_string(), edit.cursor()))
                }
                _ => None,
            };
            if selected {
                selected_at = Some(items.len());
            }
            items.push(ViewItem::Window { row: Box::new(r.clone()), selected, editor });
            if let Mode::NewWindow { after, edit, .. } = &self.mode
                && *after == r.window_id
            {
                selected_at = Some(items.len());
                items.push(ViewItem::NewWindow {
                    text: edit.text().to_string(),
                    cursor: edit.cursor(),
                });
            }
        }
        (items, selected_at)
    }

    // ---- selection

    fn refilter(&mut self) {
        self.visible = self.matcher.matching(self.filter.text(), &self.rows);
    }

    fn refilter_to_top(&mut self) {
        self.refilter();
        self.select_or_clear(0);
    }

    fn select_pos(&mut self, pos: usize) {
        self.sel_pos = pos;
        let r = &self.rows[self.visible[pos]];
        self.selected = Some((r.session_id.clone(), r.window_id.clone()));
    }

    fn select_or_clear(&mut self, pos: usize) {
        if self.visible.is_empty() {
            self.selected = None;
            self.sel_pos = 0;
        } else {
            self.select_pos(pos.min(self.visible.len() - 1));
        }
    }

    fn step(&mut self, d: isize) {
        let n = self.visible.len() as isize;
        if n > 0 {
            self.select_pos((self.sel_pos as isize + d).rem_euclid(n) as usize);
        }
    }

    fn page(&mut self, d: isize) {
        let n = self.visible.len() as isize;
        if n > 0 {
            let pos = (self.sel_pos as isize + d * self.page_size() as isize).clamp(0, n - 1);
            self.select_pos(pos as usize);
        }
    }

    /// Keep the cursor on its window (by ID); if that went, stay at the same
    /// row position — the next window — clamped to the list.
    fn reselect(&mut self) {
        if let Some((sid, wid)) = &self.selected
            && let Some(pos) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].session_id == *sid && self.rows[i].window_id == *wid)
        {
            self.sel_pos = pos;
            return;
        }
        self.select_or_clear(self.sel_pos);
    }

    fn apply_pending(&mut self) -> bool {
        let Some(w) = self.pending_select.clone() else {
            return false;
        };
        match self.visible.iter().position(|&i| self.rows[i].window_id == w) {
            Some(pos) => {
                self.select_pos(pos);
                self.pending_select = None;
                true
            }
            None => false,
        }
    }
}
```

- [ ] **Step 7: Run them to see them pass**

Run: `cargo test --lib popup`
Expected: all popup tests pass (19 new app tests).

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock src/popup
timeout 60 git commit -m "popup: App state machine — list mode, layout, live snapshots"
```

---

### Task 10: `App` management — rename, auto-name, new window, close/confirm, reopen, reorder

**Files:**
- Modify: `src/popup/app.rs`

**Interfaces:**
- Consumes: `model::{close_plan, ClosePlan, confirm_prompt}` (Task 2).
- Produces (on `App`):
  - Keys in list mode: `^r` opens the rename editor, `M-r` emits `ResetName`, `M-n` opens the new-window editor, `^x` follows `close_plan` (notice / `Close{confirmed:false}` / `Confirm` mode), `^t` emits `Reopen`, and `M-↑`/`M-↓` emit `Swap` with the session neighbour.
  - In the editors, `⏎` commits and `Esc` cancels. An empty rename cancels; an empty new-window name means automatic. Other keys edit; list keys do nothing.
  - In confirm, `y`/`Y` gives `Close{confirmed:true}`; any other key cancels.
  - `on_busy(&mut self, window_id: &str, busy: Vec<String>)`: opens the confirm.
  - `on_reopened(&mut self, window_id: Option<String>)` and `on_created(&mut self, window_id: String)`: clear the filter and select that window once it's listed. `None` gives the notice `nothing to reopen`.
  - Snapshots never touch an open editor. If its window vanished, the editor closes with a notice: `window closed while renaming`, `window closed — new window cancelled` or `window already closed`.

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `src/popup/app.rs`:

```rust
    #[test]
    fn ctrl_r_prefills_and_enter_renames_by_id() {
        let mut a = app();
        a.on_key(ctrl('r'));
        assert_eq!(
            a.mode,
            Mode::Rename { window_id: "@0".into(), edit: LineEdit::new("editor") }
        );
        a.on_key(ctrl('u'));
        typed(&mut a, "renamed (x)+y, z");
        assert_eq!(
            a.on_key(key(Enter)),
            vec![Effect::Rename { window_id: "@0".into(), name: "renamed (x)+y, z".into() }]
        );
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn rename_esc_cancels() {
        let mut a = app();
        a.on_key(ctrl('r'));
        typed(&mut a, "zz");
        assert!(a.on_key(key(Esc)).is_empty());
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn rename_empty_cancels() {
        let mut a = app();
        a.on_key(ctrl('r'));
        a.on_key(ctrl('u'));
        assert!(a.on_key(key(Enter)).is_empty());
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn filter_survives_rename() {
        let mut a = app();
        typed(&mut a, "win");
        assert_eq!((a.counts(), selected(&a)), ((1, 4), "@1".to_string()));
        a.on_key(ctrl('r'));
        for _ in 0..3 {
            a.on_key(key(Backspace));
        }
        typed(&mut a, "2");
        assert_eq!(
            a.on_key(key(Enter)),
            vec![Effect::Rename { window_id: "@1".into(), name: "win 2".into() }]
        );
        assert_eq!((a.filter.text(), a.counts()), ("win", (1, 4)));
        a.on_key(ctrl('r'));
        a.on_key(key(Esc));
        assert_eq!(a.filter.text(), "win");
    }

    #[test]
    fn list_keys_ignored_while_renaming() {
        let mut a = app();
        a.on_key(ctrl('r'));
        for k in [
            ctrl('x'), ctrl('t'), alt(Char('r')), alt(Char('n')), key(Down), ctrl('n'), ctrl('o'),
            key(F(1)), alt(Up),
        ] {
            assert!(a.on_key(k).is_empty(), "{k:?}");
        }
        assert!(matches!(a.mode, Mode::Rename { .. }));
        assert_eq!(selected(&a), "@0");
        assert!(!a.preview.hidden);
    }

    #[test]
    fn alt_r_resets_selected() {
        let mut a = app();
        assert_eq!(a.on_key(alt(Char('r'))), vec![Effect::ResetName { window_id: "@0".into() }]);
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn close_idle_is_immediate_unconfirmed() {
        let mut a = app();
        a.on_key(key(Down));
        assert_eq!(
            a.on_key(ctrl('x')),
            vec![Effect::Close { window_id: "@1".into(), confirmed: false }]
        );
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn sidebar_only_activity_closes_at_once() {
        let mut a = app();
        a.on_key(key(Down));
        a.on_key(key(Down)); // @2 logs: its only program is the sidebar's
        assert_eq!(
            a.on_key(ctrl('x')),
            vec![Effect::Close { window_id: "@2".into(), confirmed: false }]
        );
    }

    #[test]
    fn close_busy_asks_with_commands() {
        let mut a = app();
        assert!(a.on_key(ctrl('x')).is_empty());
        assert_eq!(
            a.mode,
            Mode::Confirm {
                window_id: "@0".into(),
                prompt: "close \"editor\"? running: nvim (y/N) ".into()
            }
        );
    }

    #[test]
    fn confirm_cancels_on_anything_but_y() {
        for k in [ch('n'), key(Esc), key(Enter), ctrl('x'), ch('q')] {
            let mut a = app();
            typed(&mut a, "edi");
            a.on_key(ctrl('x'));
            assert!(matches!(a.mode, Mode::Confirm { .. }));
            assert!(a.on_key(k).is_empty(), "{k:?}");
            assert_eq!(a.mode, Mode::List);
            assert_eq!((a.filter.text(), selected(&a)), ("edi", "@0".to_string()));
        }
    }

    #[test]
    fn confirm_y_closes() {
        for c in ['y', 'Y'] {
            let mut a = app();
            a.on_key(ctrl('x'));
            assert_eq!(
                a.on_key(ch(c)),
                vec![Effect::Close { window_id: "@0".into(), confirmed: true }]
            );
            assert_eq!(a.mode, Mode::List);
        }
    }

    #[test]
    fn last_window_of_session_warns() {
        let mut s = snap();
        add_session(&mut s, "$2", "gamma", "@4", "solo");
        let mut a = app_with(s);
        typed(&mut a, "solo");
        a.on_key(ctrl('x'));
        assert_eq!(
            a.mode,
            Mode::Confirm {
                window_id: "@4".into(),
                prompt: "close \"solo\"? — session \"gamma\" will end (y/N) ".into()
            }
        );
    }

    #[test]
    fn last_window_on_server_is_refused() {
        use crate::tmux::snapshot::Session;
        let s = Snapshot {
            sessions: vec![Session { id: "$0".into(), name: "only".into(), attached: 1 }],
            windows: vec![window("@0", "$0", 0, "w")],
            panes: vec![pane("%0", "@0", "$0", true, "sh", "/")],
            clients: vec![],
        };
        let mut a = App::new(None, HOME.into(), (200, 50));
        a.on_snapshot(0, s);
        assert!(a.on_key(ctrl('x')).is_empty());
        assert_eq!(a.mode, Mode::List);
        assert_eq!(a.notice.as_deref(), Some("can't close the last window on the server"));
        a.on_key(ch('x'));
        assert_eq!(a.notice, None);
    }

    #[test]
    fn on_busy_opens_confirm() {
        let mut a = app();
        a.on_busy("@1", vec!["vim".into()]);
        assert_eq!(
            a.mode,
            Mode::Confirm { window_id: "@1".into(), prompt: "close \"win two\"? running: vim (y/N) ".into() }
        );
    }

    #[test]
    fn filter_kept_after_close() {
        let mut a = app();
        typed(&mut a, "win");
        a.on_key(ctrl('x'));
        let mut s = snap();
        drop_window(&mut s, "@1");
        a.on_snapshot(0, s);
        assert_eq!((a.filter.text(), a.counts()), ("win", (0, 3)));
        assert!(a.selected_row().is_none());
    }

    #[test]
    fn cursor_moves_to_next_row_after_close() {
        let mut a = app();
        a.on_key(key(Down));
        a.on_key(ctrl('x'));
        let mut s = snap();
        drop_window(&mut s, "@1");
        a.on_snapshot(0, s);
        assert_eq!(selected(&a), "@2");
    }

    #[test]
    fn reopened_window_selected_and_filter_cleared() {
        let mut a = app();
        typed(&mut a, "bui");
        assert_eq!(a.on_key(ctrl('t')), vec![Effect::Reopen]);
        a.on_reopened(Some("@7".into()));
        assert_eq!(a.filter.text(), "");
        let mut s = snap();
        add_window(&mut s, "@7", "$0", 2, "back", "sh");
        a.on_snapshot(0, s);
        assert_eq!(selected(&a), "@7");
    }

    #[test]
    fn nothing_to_reopen_notice_clears_on_key() {
        let mut a = app();
        a.on_reopened(None);
        assert_eq!(a.notice.as_deref(), Some("nothing to reopen"));
        a.on_key(ch('x'));
        assert_eq!(a.notice, None);
    }

    #[test]
    fn alt_down_swaps_with_session_neighbour() {
        let mut a = app();
        assert_eq!(
            a.on_key(alt(Down)),
            vec![Effect::Swap { a: "@0".into(), b: "@1".into() }]
        );
        assert!(a.on_key(alt(Up)).is_empty(), "top of its session");
    }

    #[test]
    fn alt_down_at_session_end_is_noop() {
        let mut a = app();
        a.on_key(key(Down)); // @1, last in alpha (beta follows in the list)
        assert!(a.on_key(alt(Down)).is_empty());
        assert_eq!(
            a.on_key(alt(Up)),
            vec![Effect::Swap { a: "@1".into(), b: "@0".into() }]
        );
    }

    #[test]
    fn selection_follows_swapped_window() {
        let mut a = app();
        a.on_key(alt(Down));
        let mut s = snap();
        s.windows[0].index = 1; // @0
        s.windows[1].index = 0; // @1
        a.on_snapshot(0, s);
        assert_eq!(selected(&a), "@0");
        assert_eq!(a.view_items().1, Some(2));
    }

    #[test]
    fn alt_n_creates_after_selection() {
        let mut a = app();
        typed(&mut a, "edi");
        a.on_key(alt(Char('n')));
        assert_eq!(
            a.mode,
            Mode::NewWindow { after: "@0".into(), cwd: "/home/u/dev/dotfiles".into(), edit: LineEdit::default() }
        );
        let (items, sel) = a.view_items();
        assert_eq!(sel, Some(2));
        assert!(matches!(items[2], ViewItem::NewWindow { .. }));
        typed(&mut a, "fresh");
        assert_eq!(
            a.on_key(key(Enter)),
            vec![Effect::NewWindow { after: "@0".into(), cwd: "/home/u/dev/dotfiles".into(), name: "fresh".into() }]
        );
        a.on_created("@7".into());
        assert_eq!(a.filter.text(), "");
        let mut s = snap();
        s.windows[1].index = 2;
        add_window(&mut s, "@7", "$0", 1, "fresh", "sh");
        a.on_snapshot(0, s);
        assert_eq!(selected(&a), "@7");
    }

    #[test]
    fn alt_n_esc_cancels_and_empty_name_is_automatic() {
        let mut a = app();
        a.on_key(alt(Char('n')));
        assert!(a.on_key(key(Esc)).is_empty());
        assert_eq!(a.mode, Mode::List);
        a.on_key(alt(Char('n')));
        assert_eq!(
            a.on_key(key(Enter)),
            vec![Effect::NewWindow { after: "@0".into(), cwd: "/home/u/dev/dotfiles".into(), name: String::new() }]
        );
    }

    #[test]
    fn snapshot_never_touches_open_editor() {
        let mut a = app();
        a.on_key(ctrl('r'));
        typed(&mut a, "-x");
        let mut s = snap();
        add_window(&mut s, "@7", "$1", 2, "pushed", "sh");
        s.windows[0].name = "changed elsewhere".into();
        a.on_snapshot(0, s);
        assert_eq!(a.mode, Mode::Rename { window_id: "@0".into(), edit: LineEdit::new("editor-x") });
        assert_eq!(a.counts(), (5, 5));
    }

    #[test]
    fn vanished_rename_target_closes_editor_with_notice() {
        let mut a = app();
        a.on_key(ctrl('r'));
        let mut s = snap();
        drop_window(&mut s, "@0");
        a.on_snapshot(0, s);
        assert_eq!(a.mode, Mode::List);
        assert_eq!(a.notice.as_deref(), Some("window closed while renaming"));
    }

    #[test]
    fn vanished_confirm_target_cancels() {
        let mut a = app();
        a.on_key(ctrl('x'));
        let mut s = snap();
        drop_window(&mut s, "@0");
        a.on_snapshot(0, s);
        assert_eq!(a.mode, Mode::List);
        assert_eq!(a.notice.as_deref(), Some("window already closed"));
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib popup::app`
Expected: compile errors (`on_busy`, `on_reopened` and `on_created` not found), then assertion failures on the key handling.

- [ ] **Step 3: Implement.** In `src/popup/app.rs`:

Change the `crate` import to:

```rust
use crate::{
    model::{self, ClosePlan},
    tmux::snapshot::Snapshot,
};
```

Replace `on_key` with:

```rust
    pub fn on_key(&mut self, k: KeyEvent) -> Vec<Effect> {
        self.notice = None;
        match self.mode {
            Mode::Help => {
                self.mode = Mode::List;
                Vec::new()
            }
            Mode::List => self.key_list(k),
            Mode::Rename { .. } => self.key_rename(k),
            Mode::NewWindow { .. } => self.key_new_window(k),
            Mode::Confirm { .. } => self.key_confirm(k),
        }
    }
```

Insert as the first statement of `key_list`:

```rust
        if let Some(effects) = self.key_manage(&k) {
            return effects;
        }
```

In `on_snapshot`, insert right after `self.refilter();`:

```rust
        self.close_vanished_editor();
```

Add these methods to `impl App` (after `key_list`):

```rust
    /// `^r` `M-r` `M-n` `^x` `^t` `M-↑` `M-↓` — the management keys.
    fn key_manage(&mut self, k: &KeyEvent) -> Option<Vec<Effect>> {
        if is_ctrl(k, 'r') {
            if let Some(mode) = self.selected_row().map(|r| Mode::Rename {
                window_id: r.window_id.clone(),
                edit: LineEdit::new(&r.name),
            }) {
                self.mode = mode;
            }
            return Some(Vec::new());
        }
        if is_alt(k, KeyCode::Char('r')) {
            return Some(
                self.selected_row()
                    .map(|r| vec![Effect::ResetName { window_id: r.window_id.clone() }])
                    .unwrap_or_default(),
            );
        }
        if is_alt(k, KeyCode::Char('n')) {
            if let Some(mode) = self.selected_row().map(|r| Mode::NewWindow {
                after: r.window_id.clone(),
                cwd: r.path.clone(),
                edit: LineEdit::default(),
            }) {
                self.mode = mode;
            }
            return Some(Vec::new());
        }
        if is_ctrl(k, 'x') {
            return Some(self.start_close());
        }
        if is_ctrl(k, 't') {
            return Some(vec![Effect::Reopen]);
        }
        if is_alt(k, KeyCode::Up) {
            return Some(self.reorder(-1));
        }
        if is_alt(k, KeyCode::Down) {
            return Some(self.reorder(1));
        }
        None
    }

    fn key_rename(&mut self, k: KeyEvent) -> Vec<Effect> {
        if plain(&k, KeyCode::Esc) {
            self.mode = Mode::List;
            return Vec::new();
        }
        if plain(&k, KeyCode::Enter) {
            let Mode::Rename { window_id, edit } = std::mem::replace(&mut self.mode, Mode::List) else {
                return Vec::new();
            };
            return if edit.is_empty() {
                Vec::new()
            } else {
                vec![Effect::Rename { window_id, name: edit.text().to_string() }]
            };
        }
        if let Mode::Rename { edit, .. } = &mut self.mode {
            edit.handle(k);
        }
        Vec::new()
    }

    fn key_new_window(&mut self, k: KeyEvent) -> Vec<Effect> {
        if plain(&k, KeyCode::Esc) {
            self.mode = Mode::List;
            return Vec::new();
        }
        if plain(&k, KeyCode::Enter) {
            let Mode::NewWindow { after, cwd, edit } = std::mem::replace(&mut self.mode, Mode::List)
            else {
                return Vec::new();
            };
            return vec![Effect::NewWindow { after, cwd, name: edit.text().to_string() }];
        }
        if let Mode::NewWindow { edit, .. } = &mut self.mode {
            edit.handle(k);
        }
        Vec::new()
    }

    /// Only `y` closes; any other key (`n`, `⏎`, `Esc`, …) cancels.
    fn key_confirm(&mut self, k: KeyEvent) -> Vec<Effect> {
        let Mode::Confirm { window_id, .. } = std::mem::replace(&mut self.mode, Mode::List) else {
            return Vec::new();
        };
        let yes = matches!(k.code, KeyCode::Char('y' | 'Y'))
            && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if yes {
            vec![Effect::Close { window_id, confirmed: true }]
        } else {
            Vec::new()
        }
    }

    fn start_close(&mut self) -> Vec<Effect> {
        let Some(window_id) = self.selected_row().map(|r| r.window_id.clone()) else {
            return Vec::new();
        };
        match model::close_plan(&self.snap, &window_id) {
            Some(ClosePlan::Refuse) => {
                self.notice = Some("can't close the last window on the server".into());
                Vec::new()
            }
            Some(ClosePlan::Now) => vec![Effect::Close { window_id, confirmed: false }],
            Some(ClosePlan::Confirm { prompt }) => {
                self.mode = Mode::Confirm { window_id, prompt };
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// Swap with the adjacent window of the same session, in index order
    /// (whatever the filter shows); nothing at the session's edge.
    fn reorder(&self, d: isize) -> Vec<Effect> {
        let Some(r) = self.selected_row() else {
            return Vec::new();
        };
        let same: Vec<&WinRow> = self.rows.iter().filter(|x| x.session_id == r.session_id).collect();
        let Some(i) = same.iter().position(|x| x.window_id == r.window_id) else {
            return Vec::new();
        };
        let j = i as isize + d;
        if j < 0 || j as usize >= same.len() {
            return Vec::new();
        }
        vec![Effect::Swap { a: r.window_id.clone(), b: same[j as usize].window_id.clone() }]
    }

    /// The runner's re-check found work the snapshot didn't show yet: ask.
    pub fn on_busy(&mut self, window_id: &str, busy: Vec<String>) {
        let name = self
            .rows
            .iter()
            .find(|r| r.window_id == window_id)
            .map(|r| r.name.clone())
            .unwrap_or_default();
        self.mode = Mode::Confirm {
            window_id: window_id.to_string(),
            prompt: model::confirm_prompt(&name, &busy, None),
        };
    }

    pub fn on_reopened(&mut self, window_id: Option<String>) {
        match window_id {
            Some(w) => self.select_when_listed(w),
            None => self.notice = Some("nothing to reopen".into()),
        }
    }

    pub fn on_created(&mut self, window_id: String) {
        self.select_when_listed(window_id);
    }

    /// Clear the filter (so the window can be seen) and put the cursor on it
    /// as soon as a snapshot lists it.
    fn select_when_listed(&mut self, window_id: String) {
        self.filter.clear();
        self.refilter();
        self.pending_select = Some(window_id);
        if !self.apply_pending() {
            self.reselect();
        }
    }

    /// An open editor or prompt is never disturbed by a snapshot — unless
    /// its window has gone, which closes it with a notice.
    fn close_vanished_editor(&mut self) {
        let exists = |w: &str| self.rows.iter().any(|r| r.window_id == w);
        let notice = match &self.mode {
            Mode::Rename { window_id, .. } if !exists(window_id) => "window closed while renaming",
            Mode::NewWindow { after, .. } if !exists(after) => "window closed — new window cancelled",
            Mode::Confirm { window_id, .. } if !exists(window_id) => "window already closed",
            _ => return,
        };
        self.mode = Mode::List;
        self.notice = Some(notice.to_string());
    }
```

- [ ] **Step 4: Run them to see them pass**

Run: `cargo test --lib popup`
Expected: all pass (26 new app tests).

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/popup/app.rs
timeout 60 git commit -m "popup: rename, auto-name, new window, close/confirm, reopen, reorder"
```

---
### Task 11: `view` — drawing the popup

**Files:**
- Create: `src/popup/view.rs`, `src/popup/snapshots/` (generated by insta)
- Modify: `src/popup/mod.rs` (add `pub mod view;`), `Cargo.toml` (`ansi-to-tui`, dev `insta`)

**Interfaces:**
- Consumes: `App` reads (`header`, `counts`, `filter`, `mode`, `notice`, `preview`, `view_items`, `selected_row`, `preview_text`, `list_state`), `layout::regions`.
- Produces: `view::render(f: &mut Frame, app: &mut App)`, `view::HELP: &str` and `view::ansi_tail(capture: &str, height: usize) -> Text<'static>`.
- Screen contract, which the e2e tests in Tasks 14–15 match on:
  - Line 0: ` tmux-home   <session> ▸ <index>`, with `F1 help` right-aligned.
  - Line 1 is the filter line, `> <filter>` with `<shown>/<total>` right-aligned. While a confirm is open, it shows the confirm prompt instead.
  - Session headers: `─ <name> ───… [attached · ]<n> window[s] ──`.
  - Window rows: `<▌ if selected, else space><▶ if the client's window, else space> <index:>3>  <name:<24>  <command:<10>  <short path>`.
  - The rename row reads `…<index:>3>  rename › <text>`, and the new-window row reads `▌    +  new window › <text>`.
  - The preview has a `│` left border when it's on the right and a `─` top border when it's at the bottom.
  - Footer: the mode's keys, or the notice.

- [ ] **Step 1: Add the dependencies and check their APIs**

```bash
cargo add ansi-to-tui
cargo add --dev insta
grep -n "fn into_text" ~/.cargo/registry/src/*/ansi-to-tui-8.*/src/lib.rs
grep -n "ratatui-core" ~/.cargo/registry/src/*/ansi-to-tui-8.*/Cargo.toml
cargo tree -i ratatui-core --depth 1
```

Expected: `IntoText::into_text(&self) -> Result<Text<'static>, Error>` for any `AsRef<[u8]>` (ansi-to-tui 8.0.1). It depends on `ratatui-core` 0.1, and `cargo tree` shows a single `ratatui-core` (0.1.2) used by both ratatui and ansi-to-tui.

- [ ] **Step 2: Write the failing tests** — create `src/popup/view.rs` with the tests, and add `pub mod view;` to `src/popup/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::{app::Effect, testutil::*};
    use crossterm::event::KeyCode::*;
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    fn draw(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        app.on_resize(w, h);
        t.draw(|f| render(f, app)).unwrap();
        t
    }

    fn lines(t: &Terminal<TestBackend>) -> Vec<String> {
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn header_and_filter_lines() {
        let l = lines(&draw(&mut app(), 200, 50));
        assert!(l[0].starts_with(" tmux-home   alpha ▸ 0"), "{:?}", l[0]);
        assert!(l[0].ends_with("F1 help"), "{:?}", l[0]);
        assert!(l[1].starts_with("> "), "{:?}", l[1]);
        assert!(l[1].ends_with("4/4"), "{:?}", l[1]);
    }

    #[test]
    fn groups_and_rows() {
        let l = lines(&draw(&mut app(), 200, 50));
        assert!(l[2].starts_with("─ alpha ─"), "{:?}", l[2]);
        assert!(l[2].contains(" attached · 2 windows ──"), "{:?}", l[2]);
        assert!(l[3].starts_with("▌▶   0  editor                    nvim        ~/d/dotfiles"), "{:?}", l[3]);
        assert!(l[4].starts_with("     1  win two"), "{:?}", l[4]);
        assert!(l[5].starts_with("─ beta ─"), "{:?}", l[5]);
        assert!(l[5].contains(" 2 windows ──") && !l[5].contains("attached"), "{:?}", l[5]);
        assert!(l[6].starts_with("     0  logs                      sh          /v/log"), "{:?}", l[6]);
    }

    #[test]
    fn side_preview_border_at_200_cols() {
        let l = lines(&draw(&mut app(), 200, 50));
        for (y, line) in l.iter().enumerate().take(49).skip(2) {
            assert_eq!(line.chars().nth(100), Some('│'), "row {y}: {line:?}");
        }
    }

    #[test]
    fn bottom_preview_at_150x40() {
        let l = lines(&draw(&mut app(), 150, 40));
        assert!(l.iter().all(|x| !x.contains('│')), "{l:#?}");
        assert!(
            l.iter().any(|x| x.chars().count() == 150 && x.chars().all(|c| c == '─')),
            "a full-width top border: {l:#?}"
        );
    }

    #[test]
    fn small_terminal_hides_preview_until_ctrl_o() {
        let mut a = app();
        let l = lines(&draw(&mut a, 120, 20));
        let border = |l: &[String]| l.iter().any(|x| x.chars().count() == 120 && x.chars().all(|c| c == '─'));
        assert!(!border(&l) && l.iter().all(|x| !x.contains('│')), "{l:#?}");
        a.on_key(ctrl('o'));
        let mut t = Terminal::new(TestBackend::new(120, 20)).unwrap();
        t.draw(|f| render(f, &mut a)).unwrap();
        assert!(border(&lines(&t)), "{:#?}", lines(&t));
    }

    #[test]
    fn preview_shows_the_tail_of_the_capture() {
        let mut a = app();
        let text: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        a.on_preview("%0".into(), text.join("\n"));
        let l = lines(&draw(&mut a, 200, 50));
        assert!(l[2].ends_with("│line 54"), "{:?}", l[2]);
        assert!(l[48].ends_with("│line 100"), "{:?}", l[48]);
    }

    #[test]
    fn preview_keeps_ansi_colours() {
        let mut a = app();
        a.on_preview("%0".into(), "\x1b[31mred\x1b[0m".into());
        let t = draw(&mut a, 200, 50);
        let cell = &t.backend().buffer()[(101, 2)];
        assert_eq!((cell.symbol(), cell.fg), ("r", Color::Red));
    }

    #[test]
    fn rename_row_is_an_editor_with_the_cursor_at_its_end() {
        let mut a = app();
        a.on_key(ctrl('r'));
        let mut t = draw(&mut a, 200, 50);
        let l = lines(&t);
        assert!(l[3].starts_with("▌▶   0  rename › editor"), "{:?}", l[3]);
        t.backend_mut().assert_cursor_position((23, 3));
        assert!(l[49].starts_with(" ⏎ save"), "{:?}", l[49]);
    }

    #[test]
    fn new_window_row_under_its_anchor() {
        let mut a = app();
        a.on_key(alt(Char('n')));
        typed(&mut a, "api");
        let l = lines(&draw(&mut a, 200, 50));
        assert!(l[4].starts_with("▌    +  new window › api"), "{:?}", l[4]);
        assert!(l[5].starts_with("     1  win two"), "{:?}", l[5]);
    }

    #[test]
    fn confirm_prompt_on_the_filter_line() {
        let mut a = app();
        a.on_key(ctrl('x'));
        let l = lines(&draw(&mut a, 200, 50));
        assert!(l[1].starts_with("close \"editor\"? running: nvim (y/N)"), "{:?}", l[1]);
        assert!(l[49].starts_with(" y close"), "{:?}", l[49]);
    }

    #[test]
    fn footer_shows_keys_or_the_notice() {
        let mut a = app();
        assert!(lines(&draw(&mut a, 200, 50))[49].starts_with(" ⏎ go"));
        a.on_error("boom".into());
        assert_eq!(lines(&draw(&mut a, 200, 50))[49], " boom");
    }

    #[test]
    fn no_matches_message() {
        let mut a = app();
        typed(&mut a, "zzz");
        let l = lines(&draw(&mut a, 200, 50));
        assert!(l[2].starts_with("no windows match \"zzz\"  (Esc clears)"), "{:?}", l[2]);
        assert!(l[1].ends_with("0/4"));
    }

    #[test]
    fn help_screen() {
        let mut a = app();
        a.on_key(key(F(1)));
        let l = lines(&draw(&mut a, 200, 50));
        assert_eq!(l[0], "tmux-home — keys");
        assert!(l.iter().any(|x| x == "press any key to return"));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (w, h) in [(20, 3), (10, 1), (1, 1)] {
            for keys in [vec![], vec![ctrl('r')], vec![ctrl('x')], vec![alt(Char('n'))]] {
                let mut a = app();
                for k in keys {
                    let _: Vec<Effect> = a.on_key(k);
                }
                draw(&mut a, w, h);
            }
        }
    }

    #[test]
    fn snapshot_120x24() {
        let mut a = app();
        insta::assert_snapshot!(draw(&mut a, 120, 24).backend());
    }
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test --lib popup::view`
Expected: compile errors: `render`, `HELP` and `App` items not in scope.

- [ ] **Step 4: Implement** (above the tests in `src/popup/view.rs`):

```rust
//! Drawing the popup with ratatui. Reads an `App`; the only state it
//! touches is the list's scroll offset. Colours: named ANSI colours and
//! modifiers only, so the terminal's theme applies.

use super::{
    app::{App, Mode, ViewItem},
    layout::{PreviewPos, regions},
    rows::WinRow,
};
use ansi_to_tui::IntoText;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

pub const HELP: &str = "\
tmux-home — keys

  type              filter windows (session, index, name, command, path)
  ↑ ↓  ^p ^n  ^k ^j  move selection
  PgUp PgDn         page
  ← →  Home End     edit the filter
  ⏎                 switch to the selected window and close
  Esc               clear the filter; close if it is already empty
  ^r                rename the window inline (⏎ save, Esc/empty cancel)
  M-r               reset the window to its automatic name
  M-n               new window after the selection (⏎ create, Esc cancel)
  M-↑ M-↓           move the window up/down within its session
  ^x                close the window; asks first (y/N) if anything but a
                    shell is running in it, or if it is its session's last
  ^t                reopen the last closed window (up to 10 back): same
                    place, name, panes, layout and directories — but fresh
                    shells; what was running in it is gone
  ^o                toggle the preview
  F1  ^/            this help

press any key to return";

const HEADER_RIGHT: &str = "F1 help ";
const FOOTER_LIST: &str = " ⏎ go   ^r rename   M-r auto-name   M-n new   M-↑↓ move   ^x close   ^t reopen   ^o preview   Esc clear/close   F1 keys";
const FOOTER_RENAME: &str = " ⏎ save   Esc cancel   (empty name cancels)";
const FOOTER_NEW: &str = " ⏎ create   Esc cancel   (empty name: automatic)";
const FOOTER_CONFIRM: &str = " y close   any other key or Esc cancels";

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if app.mode == Mode::Help {
        f.render_widget(Paragraph::new(HELP), area);
        return;
    }
    let r = regions(area, app.preview);

    let (left, right) = split_right(r.header, HEADER_RIGHT);
    f.render_widget(Paragraph::new(Line::from(app.header()).bold()), left);
    f.render_widget(Paragraph::new(HEADER_RIGHT), right);

    if let Mode::Confirm { prompt, .. } = &app.mode {
        f.render_widget(Paragraph::new(Line::from(prompt.as_str()).bold().yellow()), r.filter);
        f.set_cursor_position((r.filter.x.saturating_add(width(prompt)), r.filter.y));
    } else {
        let (shown, total) = app.counts();
        let count = format!("{shown}/{total} ");
        let (left, right) = split_right(r.filter, &count);
        f.render_widget(Paragraph::new(format!("> {}", app.filter.text())), left);
        f.render_widget(Paragraph::new(count.as_str()), right);
        if app.mode == Mode::List {
            let x = 2u16.saturating_add(width(app.filter.before_cursor()));
            f.set_cursor_position((r.filter.x.saturating_add(x), r.filter.y));
        }
    }

    draw_list(f, app, r.list);
    if let Some(p) = r.preview {
        draw_preview(f, app, p);
    }
    f.render_widget(Paragraph::new(footer(app)), r.footer);
}

fn width(s: &str) -> u16 {
    u16::try_from(Line::from(s).width()).unwrap_or(u16::MAX)
}

/// `area` split into what's left and a right-hand part exactly as wide as `right`.
fn split_right(area: Rect, right: &str) -> (Rect, Rect) {
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(width(right))]).areas(area);
    (l, r)
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let (items, selected) = app.view_items();
    if items.is_empty() {
        let msg = if app.filter.is_empty() {
            "no windows".to_string()
        } else {
            format!("no windows match \"{}\"  (Esc clears)", app.filter.text())
        };
        f.render_widget(Paragraph::new(msg).dim(), area);
        return;
    }
    let mut cursor = None;
    let list: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let (line, x) = match item {
                ViewItem::Header { name, windows, attached } => {
                    (header_line(name, *windows, *attached, area.width as usize), None)
                }
                ViewItem::Window { row, selected, editor } => window_line(row, *selected, editor.as_ref()),
                ViewItem::NewWindow { text, cursor } => {
                    editor_line("▌    +  new window › ", text, *cursor)
                }
            };
            if let Some(x) = x {
                cursor = Some((i, x));
            }
            ListItem::new(line)
        })
        .collect();
    app.list_state.select(selected);
    f.render_stateful_widget(
        List::new(list).highlight_style(Style::new().reversed()),
        area,
        &mut app.list_state,
    );
    if let Some((i, x)) = cursor {
        let offset = app.list_state.offset();
        if i >= offset && i - offset < area.height as usize {
            f.set_cursor_position((area.x.saturating_add(x), area.y + (i - offset) as u16));
        }
    }
}

fn header_line(name: &str, windows: usize, attached: bool, cols: usize) -> Line<'static> {
    let left = format!("─ {name} ");
    let right = format!(
        " {}{windows} window{} ──",
        if attached { "attached · " } else { "" },
        if windows == 1 { "" } else { "s" }
    );
    let used = width(&left) as usize + width(&right) as usize;
    let fill = cols.saturating_sub(used).max(1);
    Line::from(vec![
        Span::raw(left).bold().cyan(),
        Span::raw("─".repeat(fill)).dim(),
        Span::raw(right).dim(),
    ])
}

/// A window row, or its rename editor; the second value is the cursor
/// column when it is an editor.
fn window_line(row: &WinRow, selected: bool, editor: Option<&(String, usize)>) -> (Line<'static>, Option<u16>) {
    let gutter = if selected { "▌" } else { " " };
    let mark = if row.current { "▶" } else { " " };
    let index = format!(" {:>3}  ", row.index);
    if let Some((text, cursor)) = editor {
        return editor_line(&format!("{gutter}{mark}{index}rename › "), text, *cursor);
    }
    let line = Line::from(vec![
        Span::raw(gutter),
        Span::raw(mark).yellow(),
        Span::raw(index),
        Span::raw(format!("{:<24}  ", row.name)),
        Span::raw(format!("{:<10}  {}", row.command, row.short_path)).dim(),
    ]);
    (line, None)
}

fn editor_line(prefix: &str, text: &str, cursor: usize) -> (Line<'static>, Option<u16>) {
    let before: String = text.chars().take(cursor).collect();
    let x = width(prefix).saturating_add(width(&before));
    (
        Line::from(vec![Span::raw(prefix.to_string()), Span::raw(text.to_string()).bold()]),
        Some(x),
    )
}

fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    let block = match app.preview.pos {
        PreviewPos::Right => Block::new().borders(Borders::LEFT),
        PreviewPos::Bottom { .. } => Block::new().borders(Borders::TOP),
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let body = match app.selected_row() {
        Some(row) if row.pane_id.is_none() => Text::from("(no pane)"),
        Some(_) => app
            .preview_text()
            .map(|t| ansi_tail(t, inner.height as usize))
            .unwrap_or_default(),
        None => Text::default(),
    };
    f.render_widget(Paragraph::new(body), inner);
}

/// The last `height` lines of a pane capture, colours kept (`capture-pane -e`).
pub fn ansi_tail(capture: &str, height: usize) -> Text<'static> {
    let lines: Vec<&str> = capture.lines().collect();
    let tail = lines[lines.len().saturating_sub(height)..].join("\n");
    tail.into_text().unwrap_or_else(|_| Text::raw(tail.clone()))
}

fn footer(app: &App) -> Line<'static> {
    if let Some(n) = &app.notice {
        return Line::from(format!(" {n}")).yellow();
    }
    Line::from(match app.mode {
        Mode::Rename { .. } => FOOTER_RENAME,
        Mode::NewWindow { .. } => FOOTER_NEW,
        Mode::Confirm { .. } => FOOTER_CONFIRM,
        Mode::List | Mode::Help => FOOTER_LIST,
    })
    .dim()
}
```

- [ ] **Step 5: Run the tests; accept the snapshot non-interactively**

```bash
cargo test --lib popup::view                       # all but snapshot_120x24 pass
INSTA_UPDATE=always cargo test --lib popup::view   # writes src/popup/snapshots/*.snap
git status --short src/popup/snapshots && cat src/popup/snapshots/*.snap
cargo test --lib popup::view
```

Expected: the first run fails only `snapshot_120x24` (new snapshot). Check the `.snap` by eye: header, `> ` … `4/4`, two session headers, four rows, footer, and no preview (120×24 hides it). The last run gives 15 passed.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock src/popup/view.rs src/popup/mod.rs src/popup/snapshots
timeout 60 git commit -m "popup: ratatui view — header, filter, grouped rows, editors, preview"
```

---

### Task 12: `exec` — performing effects through `Ops`

**Files:**
- Create: `src/popup/exec.rs`
- Modify: `src/popup/mod.rs` (add `pub mod exec;`)

**Interfaces:**
- Consumes: `app::{App, Effect}` (Tasks 9–10), `tmux::ops::{Ops, LastWindowOnServer}` (Tasks 4–5).
- Produces: `exec::Outcome { pub quit: bool, pub refresh: bool }` (Debug, Default, PartialEq, Eq) and `exec::execute<O: Ops>(effects: Vec<Effect>, ops: &O, app: &mut App) -> Outcome`. Effects run in order. The first error becomes the footer notice and stops the rest (so a failed switch doesn't quit). `refresh` is set after any write.

- [ ] **Step 1: Write the failing tests** — create `src/popup/exec.rs` with the tests, and add `pub mod exec;` to `src/popup/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::{app::Mode, testutil::*};
    use std::cell::RefCell;

    /// Records calls; `busy`, `reopen`, `fail_switch` and `last_on_server`
    /// script its answers.
    #[derive(Default)]
    struct FakeOps {
        calls: RefCell<Vec<String>>,
        busy: Vec<String>,
        reopen: Option<String>,
        fail_switch: bool,
        last_on_server: bool,
    }

    impl FakeOps {
        fn log(&self, s: String) {
            self.calls.borrow_mut().push(s);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Ops for FakeOps {
        async fn switch_to(&self, session_id: &str, window_id: &str) -> anyhow::Result<()> {
            self.log(format!("switch {session_id} {window_id}"));
            if self.fail_switch {
                anyhow::bail!("no client");
            }
            Ok(())
        }
        async fn rename_window(&self, window_id: &str, name: &str) -> anyhow::Result<()> {
            self.log(format!("rename {window_id} {name}"));
            Ok(())
        }
        async fn reset_auto_name(&self, window_id: &str) -> anyhow::Result<()> {
            self.log(format!("reset {window_id}"));
            Ok(())
        }
        async fn busy_commands(&self, window_id: &str) -> anyhow::Result<Vec<String>> {
            self.log(format!("busy {window_id}"));
            Ok(self.busy.clone())
        }
        async fn swap_windows(&self, a: &str, b: &str) -> anyhow::Result<()> {
            self.log(format!("swap {a} {b}"));
            Ok(())
        }
        async fn new_window_after(&self, window_id: &str, cwd: &str, name: &str) -> anyhow::Result<String> {
            self.log(format!("new {window_id} {cwd} {name}"));
            Ok("@9".into())
        }
        async fn close_window(&self, window_id: &str) -> anyhow::Result<()> {
            self.log(format!("close {window_id}"));
            if self.last_on_server {
                return Err(LastWindowOnServer.into());
            }
            Ok(())
        }
        async fn reopen(&self) -> anyhow::Result<Option<String>> {
            self.log("reopen".into());
            Ok(self.reopen.clone())
        }
    }

    fn close(w: &str, confirmed: bool) -> Vec<Effect> {
        vec![Effect::Close { window_id: w.into(), confirmed }]
    }

    #[tokio::test]
    async fn idle_close_rechecks_then_closes() {
        let ops = FakeOps::default();
        let mut a = app();
        let out = execute(close("@1", false), &ops, &mut a).await;
        assert_eq!(ops.calls(), ["busy @1", "close @1"]);
        assert_eq!(out, Outcome { quit: false, refresh: true });
    }

    #[tokio::test]
    async fn close_that_became_busy_asks_instead() {
        let ops = FakeOps { busy: vec!["vim".into()], ..Default::default() };
        let mut a = app();
        let out = execute(close("@1", false), &ops, &mut a).await;
        assert_eq!(ops.calls(), ["busy @1"]);
        assert_eq!(
            a.mode,
            Mode::Confirm { window_id: "@1".into(), prompt: "close \"win two\"? running: vim (y/N) ".into() }
        );
        assert!(!out.refresh);
    }

    #[tokio::test]
    async fn confirmed_close_skips_the_recheck() {
        let ops = FakeOps { busy: vec!["nvim".into()], ..Default::default() };
        let mut a = app();
        execute(close("@0", true), &ops, &mut a).await;
        assert_eq!(ops.calls(), ["close @0"]);
    }

    #[tokio::test]
    async fn last_window_on_server_becomes_a_notice() {
        let ops = FakeOps { last_on_server: true, ..Default::default() };
        let mut a = app();
        let out = execute(close("@0", true), &ops, &mut a).await;
        assert_eq!(a.notice.as_deref(), Some("can't close the last window on the server"));
        assert!(!out.quit);
    }

    #[tokio::test]
    async fn switch_then_quit() {
        let ops = FakeOps::default();
        let mut a = app();
        let out = execute(
            vec![Effect::Switch { session_id: "$1".into(), window_id: "@3".into() }, Effect::Quit],
            &ops,
            &mut a,
        )
        .await;
        assert_eq!(ops.calls(), ["switch $1 @3"]);
        assert!(out.quit);
    }

    #[tokio::test]
    async fn failed_switch_keeps_the_popup_open() {
        let ops = FakeOps { fail_switch: true, ..Default::default() };
        let mut a = app();
        let out = execute(
            vec![Effect::Switch { session_id: "$1".into(), window_id: "@3".into() }, Effect::Quit],
            &ops,
            &mut a,
        )
        .await;
        assert!(!out.quit);
        assert_eq!(a.notice.as_deref(), Some("no client"));
    }

    #[tokio::test]
    async fn reopen_reports_back() {
        let mut a = app();
        execute(vec![Effect::Reopen], &FakeOps::default(), &mut a).await;
        assert_eq!(a.notice.as_deref(), Some("nothing to reopen"));
        typed(&mut a, "bui");
        let ops = FakeOps { reopen: Some("@7".into()), ..Default::default() };
        let out = execute(vec![Effect::Reopen], &ops, &mut a).await;
        assert_eq!(a.filter.text(), "", "filter cleared so the window shows");
        assert!(out.refresh);
    }

    #[tokio::test]
    async fn writes_ask_for_a_refresh() {
        for (e, call) in [
            (Effect::Rename { window_id: "@0".into(), name: "n".into() }, "rename @0 n"),
            (Effect::ResetName { window_id: "@0".into() }, "reset @0"),
            (Effect::Swap { a: "@0".into(), b: "@1".into() }, "swap @0 @1"),
            (
                Effect::NewWindow { after: "@0".into(), cwd: "/".into(), name: "x".into() },
                "new @0 / x",
            ),
        ] {
            let ops = FakeOps::default();
            let mut a = app();
            let out = execute(vec![e], &ops, &mut a).await;
            assert_eq!(ops.calls(), [call]);
            assert_eq!(out, Outcome { quit: false, refresh: true });
        }
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib popup::exec`
Expected: compile errors: `execute` and `Outcome` not found.

- [ ] **Step 3: Implement** (above the tests):

```rust
//! Performs the popup's effects through `Ops` and reports the results back
//! to the `App`. A failure never takes the popup down: it becomes the
//! footer notice and stops the remaining effects (so a failed switch does
//! not quit).

use super::app::{App, Effect};
use crate::tmux::ops::{LastWindowOnServer, Ops};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Leave the popup.
    pub quit: bool,
    /// tmux was written to: re-read it now rather than wait for a push.
    pub refresh: bool,
}

pub async fn execute<O: Ops>(effects: Vec<Effect>, ops: &O, app: &mut App) -> Outcome {
    let mut out = Outcome::default();
    for effect in effects {
        if let Err(e) = apply(effect, ops, app, &mut out).await {
            app.on_error(format!("{e:#}"));
            out.quit = false;
            out.refresh = true;
            break;
        }
    }
    out
}

async fn apply<O: Ops>(effect: Effect, ops: &O, app: &mut App, out: &mut Outcome) -> anyhow::Result<()> {
    match effect {
        Effect::Quit => out.quit = true,
        Effect::Switch { session_id, window_id } => ops.switch_to(&session_id, &window_id).await?,
        Effect::Rename { window_id, name } => {
            ops.rename_window(&window_id, &name).await?;
            out.refresh = true;
        }
        Effect::ResetName { window_id } => {
            ops.reset_auto_name(&window_id).await?;
            out.refresh = true;
        }
        Effect::Close { window_id, confirmed } => {
            if !confirmed {
                // the snapshot said idle; something may have started since
                let busy = ops.busy_commands(&window_id).await?;
                if !busy.is_empty() {
                    app.on_busy(&window_id, busy);
                    return Ok(());
                }
            }
            match ops.close_window(&window_id).await {
                Err(e) if e.is::<LastWindowOnServer>() => app.on_error(e.to_string()),
                r => r?,
            }
            out.refresh = true;
        }
        Effect::Reopen => {
            let w = ops.reopen().await?;
            app.on_reopened(w);
            out.refresh = true;
        }
        Effect::Swap { a, b } => {
            ops.swap_windows(&a, &b).await?;
            out.refresh = true;
        }
        Effect::NewWindow { after, cwd, name } => {
            let w = ops.new_window_after(&after, &cwd, &name).await?;
            app.on_created(w);
            out.refresh = true;
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run them to see them pass**

Run: `cargo test --lib popup::exec`
Expected: 8 passed.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/popup/exec.rs src/popup/mod.rs
timeout 60 git commit -m "popup: run effects through Ops; failures become notices"
```

---

### Task 13: `feed`, the event loop and `tmux-home popup`

**Files:**
- Create: `src/popup/feed.rs`
- Modify: `src/popup/mod.rs` (module list + `run`), `src/main.rs`, `Cargo.toml` (`clap` gains the `env` feature)
- Test: `tests/feed.rs` (new)

**Interfaces:**
- Consumes: `client::{subscribe, refresh, Subscription}` (Task 6), `read_snapshot`, `source::POLL_EVERY`, `App`, `view::render`, `exec::execute`, `TmuxOps`, `capture_pane`.
- Produces:
  - `popup::feed::Feed`:
    - `Feed::start(socket: PathBuf) -> anyhow::Result<(Feed, (u64, Snapshot))>`. With the daemon, the first snapshot comes from an immediate `refresh` (fresh), not the daemon's last poll.
    - `from_daemon() -> bool`
    - `next(&mut self) -> (u64, Snapshot)` (pending forever once the source stops)
    - `refresh(&self) -> anyhow::Result<(u64, Snapshot)>`
    - consts `CONNECT_BUDGET` (150 ms), `REFRESH_BUDGET` (500 ms)
  - `popup::run(socket: PathBuf, client: Option<String>) -> anyhow::Result<()>`
  - CLI: `tmux-home popup [--socket PATH] [--client NAME]`, where `--client` defaults to `$TMUX_HOME_CLIENT`, then to tmux's client for the pane it runs in.

- [ ] **Step 1: Enable clap's `env` feature**

```bash
cargo add clap --features env
```

- [ ] **Step 2: Write the failing tests** — create `tests/feed.rs`:

```rust
mod common;
use common::{TestEnv, TestServer};
use std::time::Duration;
use tmux_home::{popup::feed::Feed, tmux::source::SourceKind};

async fn daemon_up(socket: &std::path::Path) {
    let p = tmux_home::paths::Paths::for_socket(socket).unwrap();
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(&p.sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon never came up");
}

/// The seq of the first snapshot that lists a window called `name`.
async fn until_window(feed: &mut Feed, name: &str) -> u64 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (seq, snap) = feed.next().await;
            if snap.windows.iter().any(|w| w.name == name) {
                return seq;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} never arrived"))
}

fn no_daemon_spawns() {
    // SAFETY: tests run single-threaded per process (RUST_TEST_THREADS=1).
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", "/nonexistent/tmux-home");
    }
}

#[tokio::test]
async fn degraded_feed_polls_tmux() {
    no_daemon_spawns();
    let _env = TestEnv::new();
    let s = TestServer::start();
    let (mut feed, (seq, first)) = Feed::start(s.socket.clone()).await.unwrap();
    assert!(!feed.from_daemon());
    assert_eq!((seq, first.windows.len()), (0, 1));
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "later"]);
    assert_eq!(until_window(&mut feed, "later").await, 0);
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "now"]);
    let (seq, snap) = feed.refresh().await.unwrap();
    assert_eq!(seq, 0);
    assert!(snap.windows.iter().any(|w| w.name == "now"));
}

#[tokio::test]
async fn daemon_feed_subscribes_and_refreshes() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    daemon_up(&s.socket).await;
    let (mut feed, (first, _)) = Feed::start(s.socket.clone()).await.unwrap();
    assert!(feed.from_daemon());
    assert!(first > 0);
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "now"]);
    let (seq, snap) = feed.refresh().await.unwrap();
    assert!(seq > first);
    assert!(snap.windows.iter().any(|w| w.name == "now"));
    until_window(&mut feed, "now").await; // pushed as well
    d.abort();
}

/// The first snapshot is read at start, not the daemon's last poll: a
/// window made just before the popup opens is already there.
#[tokio::test]
async fn daemon_feed_starts_from_a_fresh_read() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    daemon_up(&s.socket).await;
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "just-made"]);
    let (feed, (_, first)) = Feed::start(s.socket.clone()).await.unwrap();
    assert!(feed.from_daemon());
    assert!(first.windows.iter().any(|w| w.name == "just-made"));
    d.abort();
}

#[tokio::test]
async fn daemon_death_falls_back_to_polling() {
    no_daemon_spawns();
    let _env = TestEnv::new();
    let s = TestServer::start();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    daemon_up(&s.socket).await;
    let (mut feed, _) = Feed::start(s.socket.clone()).await.unwrap();
    assert!(feed.from_daemon());
    d.abort();
    let _ = d.await;
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "after"]);
    assert_eq!(until_window(&mut feed, "after").await, 0, "read directly");
    assert!(!feed.from_daemon());
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "mine"]);
    let (seq, snap) = feed.refresh().await.unwrap();
    assert_eq!(seq, 0);
    assert!(snap.windows.iter().any(|w| w.name == "mine"));
}

#[test]
fn popup_cli_takes_the_client_from_the_environment() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tmux-home"))
        .args(["popup", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("TMUX_HOME_CLIENT"), "{help}");
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test --test feed`
Expected: compile error: `tmux_home::popup::feed` not found.

- [ ] **Step 4: Implement `src/popup/feed.rs`:**

```rust
//! Where the popup's snapshots come from: the daemon's push stream, or —
//! when the daemon can't be reached within 150 ms, or goes away — tmux read
//! directly every 500 ms (spec §4). Either way, the popup's own writes are
//! followed by `refresh()` so they show at once.

use crate::{
    client,
    tmux::{
        Tmux,
        snapshot::{Snapshot, read_snapshot},
        source::POLL_EVERY,
    },
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

pub const CONNECT_BUDGET: Duration = Duration::from_millis(150);
pub const REFRESH_BUDGET: Duration = Duration::from_millis(500);

pub struct Feed {
    socket: PathBuf,
    tmux: Tmux,
    /// Snapshots are coming from the daemon (false: read directly).
    live: Arc<AtomicBool>,
    rx: mpsc::Receiver<(u64, Snapshot)>,
}

impl Feed {
    /// Subscribes to the daemon (a failed connect starts one in the
    /// background) or falls back to polling; returns the first snapshot.
    pub async fn start(socket: PathBuf) -> anyhow::Result<(Feed, (u64, Snapshot))> {
        let tmux = Tmux::new(socket.clone());
        let live = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(8);
        let first = match client::subscribe(&socket, "popup", CONNECT_BUDGET).await {
            Ok((seq, snap, sub)) => {
                live.store(true, Ordering::Relaxed);
                tokio::spawn(pump(sub, tmux.clone(), live.clone(), tx));
                // The daemon's latest snapshot can be a poll (500 ms) old:
                // the cursor starts on the client's *current* window, so
                // start from a read made now.
                client::refresh(&socket, REFRESH_BUDGET)
                    .await
                    .unwrap_or((seq, snap))
            }
            Err(_) => {
                let (snap, hash) = read_snapshot(&tmux).await?;
                tokio::spawn(poll(tmux.clone(), Some(hash), tx));
                (0, snap)
            }
        };
        Ok((Feed { socket, tmux, live, rx }, first))
    }

    pub fn from_daemon(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    /// The next snapshot. Once the source has stopped (the server is gone,
    /// and the popup with it) this never resolves.
    pub async fn next(&mut self) -> (u64, Snapshot) {
        match self.rx.recv().await {
            Some(x) => x,
            None => std::future::pending().await,
        }
    }

    /// The server as it is now: through the daemon while it is live (which
    /// also pushes it to every other client), else read directly (seq 0).
    pub async fn refresh(&self) -> anyhow::Result<(u64, Snapshot)> {
        if self.from_daemon() {
            match client::refresh(&self.socket, REFRESH_BUDGET).await {
                Ok(x) => return Ok(x),
                Err(_) => self.live.store(false, Ordering::Relaxed),
            }
        }
        Ok((0, read_snapshot(&self.tmux).await?.0))
    }
}

async fn pump(
    mut sub: client::Subscription,
    tmux: Tmux,
    live: Arc<AtomicBool>,
    tx: mpsc::Sender<(u64, Snapshot)>,
) {
    while let Ok(Some(x)) = sub.next().await {
        if tx.send(x).await.is_err() {
            return;
        }
    }
    // the daemon went away: stay live by reading tmux ourselves
    live.store(false, Ordering::Relaxed);
    poll(tmux, None, tx).await;
}

async fn poll(tmux: Tmux, mut last: Option<u64>, tx: mpsc::Sender<(u64, Snapshot)>) {
    loop {
        tokio::time::sleep(POLL_EVERY).await;
        let Ok((snap, hash)) = read_snapshot(&tmux).await else {
            return; // server gone
        };
        if Some(hash) != last {
            last = Some(hash);
            if tx.send((0, snap)).await.is_err() {
                return;
            }
        }
    }
}
```

- [ ] **Step 5: Implement the runner** — replace `src/popup/mod.rs` with:

```rust
//! `tmux-home popup` (spec §7): the window list TUI, a client of the daemon.

pub mod app;
pub mod edit;
pub mod exec;
pub mod feed;
pub mod filter;
pub mod layout;
pub mod rows;
#[cfg(test)]
pub(crate) mod testutil;
pub mod view;

use crate::tmux::{
    Tmux,
    ops::{TmuxOps, capture_pane},
};
use app::App;
use crossterm::event::{self, Event, KeyEventKind};
use feed::Feed;
use std::{path::PathBuf, time::Duration};
use tokio::sync::mpsc;

/// How often a visible preview is re-captured.
const CAPTURE_EVERY: Duration = Duration::from_millis(500);

/// Runs the popup until `⏎` (switch) or `Esc` on an empty filter. `client`
/// is the invoking client (`TMUX_HOME_CLIENT`); without one, tmux's client
/// for the pane we run in.
pub async fn run(socket: PathBuf, client: Option<String>) -> anyhow::Result<()> {
    let tmux = Tmux::new(socket.clone());
    let client = match client.filter(|c| !c.is_empty()) {
        Some(c) => Some(c),
        None => tmux
            .run(&["display-message", "-p", "#{client_name}"])
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    let ops = TmuxOps::new(&socket, client.clone())?;
    let (mut feed, (seq, first)) = Feed::start(socket).await?;
    let home = std::env::var("HOME").unwrap_or_default();
    let mut terminal = ratatui::try_init()?;
    let result: anyhow::Result<()> = async {
        let size = terminal.size()?;
        let mut app = App::new(client, home, (size.width, size.height));
        app.on_snapshot(seq, first);
        event_loop(&mut terminal, &ops, &mut feed, &mut app).await
    }
    .await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    ops: &TmuxOps,
    feed: &mut Feed,
    app: &mut App,
) -> anyhow::Result<()> {
    let mut keys = spawn_input();
    let (cap_tx, mut cap_rx) = mpsc::unbounded_channel::<(String, String)>();
    let mut tick = tokio::time::interval(CAPTURE_EVERY);
    let mut captured: Option<String> = None;
    loop {
        terminal.draw(|f| view::render(f, app))?;
        match app.capture_target() {
            Some(p) if captured.as_ref() != Some(&p) => {
                spawn_capture(&ops.tmux, p.clone(), cap_tx.clone());
                captured = Some(p);
            }
            Some(_) => {}
            None => captured = None,
        }
        tokio::select! {
            ev = keys.recv() => match ev {
                Some(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let effects = app.on_key(k);
                    let out = exec::execute(effects, ops, app).await;
                    if out.quit {
                        return Ok(());
                    }
                    if out.refresh {
                        match feed.refresh().await {
                            Ok((seq, snap)) => app.on_snapshot(seq, snap),
                            Err(e) => app.on_error(format!("tmux-home: {e:#}")),
                        }
                    }
                }
                Some(Event::Resize(cols, rows)) => app.on_resize(cols, rows),
                Some(_) => {}
                None => return Ok(()), // input closed
            },
            (seq, snap) = feed.next() => app.on_snapshot(seq, snap),
            Some((pane, text)) = cap_rx.recv() => app.on_preview(pane, text),
            _ = tick.tick() => captured = None, // re-capture on the next pass
        }
    }
}

/// crossterm's blocking reader on its own thread, forwarding into the loop.
fn spawn_input() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });
    rx
}

fn spawn_capture(tmux: &Tmux, pane: String, tx: mpsc::UnboundedSender<(String, String)>) {
    let tmux = tmux.clone();
    tokio::spawn(async move {
        let text = capture_pane(&tmux, &pane).await.unwrap_or_default();
        let _ = tx.send((pane, text));
    });
}
```

- [ ] **Step 6: Add the subcommand** — in `src/main.rs`, add this variant to `Cmd` (before `Reopen`):

```rust
    /// The window-list popup. Run inside `display-popup -E` (tmux-home.tmux
    /// binds it); writes target the invoking client.
    Popup {
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Client to switch on ⏎ and to keep in place when closing windows.
        #[arg(long, env = "TMUX_HOME_CLIENT")]
        client: Option<String>,
    },
```

and this arm to the `match` in `main`:

```rust
        Cmd::Popup { socket, client } => rt.block_on(async {
            let socket = tmux_home::client::current_socket(socket).await?;
            tmux_home::popup::run(socket, client).await
        }),
```

- [ ] **Step 7: Run the tests, then try the popup by hand on a throwaway server**

Run: `cargo test --test feed`
Expected: 5 passed.

Smoke run (non-interactive: it only checks that the popup draws, then sends Escape):

```bash
cargo build
S=th-smoke-$$
tmux -L $S -f /dev/null new-session -d -s alpha -x 200 -y 50 /bin/sh
tmux -L $S new-window -d -t alpha: -n two
tmux -L th-smoke-outer-$$ -f /dev/null new-session -d -s o -x 200 -y 50 \
  "env -u TMUX tmux -L $S attach -t alpha"
sleep 1
C=$(tmux -L $S list-clients -F '#{client_name}')
tmux -L $S display-popup -c "$C" -E -B -w 100% -h 100% -e TMUX_HOME_CLIENT="$C" \
  "$PWD/target/debug/tmux-home popup" &
sleep 1.5
tmux -L th-smoke-outer-$$ capture-pane -p -t o | head -6
tmux -L th-smoke-outer-$$ send-keys -t o Escape; sleep 0.5
tmux -L $S kill-server; tmux -L th-smoke-outer-$$ kill-server 2>/dev/null; true
```

Expected: the capture shows ` tmux-home   alpha ▸ 0 … F1 help`, `> … 2/2`, `─ alpha ─…` and two rows.

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add Cargo.toml Cargo.lock src/popup src/main.rs tests/feed.rs
timeout 60 git commit -m "popup: snapshot feed (daemon or direct), event loop, tmux-home popup"
```

---
### Task 14: `prefix .` opens the Rust popup; e2e harness; e2e parity part 1

This switches the binding (the bash popup stays as the fallback while no binary is built) and ports the bash suite's outer/inner-server approach to Rust. It covers open, filter, navigation, rename, `M-r`, `^o`, help, `⏎`, `Esc` and layout.

**Files:**
- Modify: `tmux-home.tmux`, `tests/run` (keeps its 143 checks, adapted to the new default key and pinned to the bash popup), `tests/common/mod.rs`, `Cargo.toml` (dev `regex`)
- Test: `tests/plugin.rs` (new), `tests/popup_e2e.rs` (new)

**Interfaces:**
- Consumes: the `tmux-home popup` and `tmux-home daemon` CLIs.
- Produces:
  - `tmux-home.tmux`: `@home-keys` defaults to `.`. Each key runs `run-shell -b "if [ -x <bin> ]; then <tmux> display-popup -c #{q:client_name} -E -B -w 100% -h 100% -e TMUX_HOME_CLIENT=#{q:client_name} <bin> popup; else … <plugin>/bin/tmux-home; fi"`, where `<bin>` is `${TMUX_HOME_BIN:-<plugin>/target/release/tmux-home}`. The script starts the daemon if `<bin>` exists.
  - `tests/common` additions: `KEY_GAP`, `ESC_GAP`, `wait_until(what, f)`, `Outer { attach, tmux, screen, keys, typed, wait_for, wait_gone, popup_open, open_popup }`, `install_binding(&TestServer)`, `popup_fixture() -> (TestEnv, TestServer, Outer)`, `fmt`, `wid`, `window_ids`, `client_at`, `groups`, `window_rows`

- [ ] **Step 1: Add the dev-dependency**

```bash
cargo add --dev regex
```

- [ ] **Step 2: Write the failing plugin tests** — create `tests/plugin.rs`:

```rust
mod common;
use common::{TestEnv, TestServer, wait_until};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_tmux-home");

fn run_plugin(s: &TestServer, bin: &str) {
    let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tmux-home.tmux"))
        .env("TMUX_HOME_TMUX", format!("tmux -S {}", s.socket.display()))
        .env("TMUX_HOME_BIN", bin)
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// The prefix-table binding of `key`, as `list-keys` prints it.
fn binding(s: &TestServer, key: &str) -> Option<String> {
    s.tmux(&["list-keys", "-T", "prefix"])
        .lines()
        .find(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            t.len() > 3 && t[2] == "prefix" && t[3] == key
        })
        .map(String::from)
}

fn is_ours(line: &Option<String>) -> bool {
    line.as_deref().is_some_and(|l| l.contains("tmux-home"))
}

#[test]
fn empty_home_keys_binds_nothing() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-keys", ""]);
    run_plugin(&s, BIN);
    assert!(!s.tmux(&["list-keys", "-T", "prefix"]).contains("tmux-home"));
}

#[test]
fn default_binds_prefix_dot_to_the_rust_popup() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    run_plugin(&s, BIN);
    let b = binding(&s, ".").expect("prefix . bound");
    assert!(b.contains("run-shell -b"), "{b}");
    assert!(
        b.contains("display-popup -c #{q:client_name} -E -B -w 100% -h 100% -e TMUX_HOME_CLIENT=#{q:client_name}"),
        "{b}"
    );
    assert!(b.contains(&format!("{BIN} popup")), "{b}");
    assert!(!is_ours(&binding(&s, "w")) && !is_ours(&binding(&s, "f")), "w and f left alone");
}

#[test]
fn custom_keys_bind_each() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-keys", "w f"]);
    run_plugin(&s, BIN);
    assert!(is_ours(&binding(&s, "w")) && is_ours(&binding(&s, "f")));
    assert!(!is_ours(&binding(&s, ".")));
}

#[test]
fn missing_binary_falls_back_to_bash_popup() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    run_plugin(&s, "/nonexistent/tmux-home");
    let b = binding(&s, ".").expect("prefix . bound");
    assert!(b.contains("/bin/tmux-home"), "{b}");
}

#[test]
fn starts_the_daemon_when_the_binary_exists() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    run_plugin(&s, BIN);
    let sock = tmux_home::paths::Paths::for_socket(&s.socket).unwrap().sock;
    wait_until("daemon socket", || sock.exists());
}
```

- [ ] **Step 3: Extend the harness** — append to `tests/common/mod.rs` (add `use std::time::{Duration, Instant};` at the top):

```rust
/// Pause after each keystroke sent to the outer pane, so the popup has
/// drawn before the next one.
pub const KEY_GAP: Duration = Duration::from_millis(150);
/// Pause after an Escape. Through the nested client a lone Escape reaches
/// the popup ~400 ms late whatever either server's escape-time is (measured
/// on tmux 3.7c; `x` takes ~50 ms): a key sent sooner merges with it into
/// one Esc, or into Meta-<key>.
pub const ESC_GAP: Duration = Duration::from_millis(600);

/// Polls `f` every 50 ms for up to 5 s; panics naming `what` on timeout.
pub fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A second throwaway server whose one pane runs a real client attached to
/// the inner server — so tmux-home's key binding can be pressed
/// (`send-keys`) and the popup read back (`capture-pane`). Ported from the
/// bash suite's outer/inner servers.
pub struct Outer {
    pub name: String,
}

impl Outer {
    pub fn attach(inner: &TestServer, target: &str, cols: u16, rows: u16) -> Outer {
        let name = format!("th-outer-{}-{}", std::process::id(), rand_suffix());
        let attach = format!(
            "env -u TMUX tmux -S '{}' attach -t '{}'",
            inner.socket.display(),
            target
        );
        let (cols, rows) = (cols.to_string(), rows.to_string());
        let out = Command::new("tmux")
            .args(["-L", &name, "-f", "/dev/null", "new-session", "-d", "-s", "outer"])
            .args(["-x", &cols, "-y", &rows, &attach])
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        wait_until("a client attached to the inner server", || {
            !inner.tmux(&["list-clients", "-F", "#{client_name}"]).trim().is_empty()
        });
        Outer { name }
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-L", &self.name])
            .args(args)
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(out.status.success(), "tmux {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn screen(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "outer"])
    }

    /// `send-keys` with tmux key names (`C-r`, `M-Down`, `Escape`, `F1`).
    pub fn keys(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", "outer"];
        args.extend_from_slice(keys);
        self.tmux(&args);
        std::thread::sleep(if keys.contains(&"Escape") { ESC_GAP } else { KEY_GAP });
    }

    /// Literal text (`--`: text may start with `-`).
    pub fn typed(&self, text: &str) {
        self.tmux(&["send-keys", "-t", "outer", "-l", "--", text]);
        std::thread::sleep(KEY_GAP);
    }

    /// Waits up to 5 s for the screen to match `re` (multi-line: `^` is a
    /// line start); panics with the screen.
    pub fn wait_for(&self, re: &str) {
        let r = regex::Regex::new(&format!("(?m){re}")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let s = self.screen();
            if r.is_match(&s) {
                return;
            }
            assert!(Instant::now() < deadline, "screen never matched {re:?}:\n{s}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits up to 5 s for `re` to disappear from the screen.
    pub fn wait_gone(&self, re: &str) {
        let r = regex::Regex::new(&format!("(?m){re}")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let s = self.screen();
            if !r.is_match(&s) {
                return;
            }
            assert!(Instant::now() < deadline, "screen still matches {re:?}:\n{s}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn popup_open(&self) -> bool {
        self.screen().contains("F1 help")
    }

    /// Presses `prefix .` and waits for the filter line.
    pub fn open_popup(&self) {
        self.keys(&["C-b", "."]);
        self.wait_for(r"^> .*\d+/\d+");
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-L", &self.name, "kill-server"]).output();
    }
}

/// Runs tmux-home.tmux against `s`, using the test build of the binary.
pub fn install_binding(s: &TestServer) {
    let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tmux-home.tmux"))
        .env("TMUX_HOME_TMUX", format!("tmux -S {}", s.socket.display()))
        .env("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"))
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(out.status.success(), "tmux-home.tmux: {}", String::from_utf8_lossy(&out.stderr));
}

/// The bash suite's fixture: sessions alpha (editor, "win two") and beta
/// (logs — whose ACTIVE pane is a sidebar — and build), every pane in "/",
/// tmux-home's binding installed, and a 200x50 client on alpha:editor.
/// Bind as `let (_env, s, o) = popup_fixture();` so the drop order (outer,
/// server, temp dirs) is right.
pub fn popup_fixture() -> (TestEnv, TestServer, Outer) {
    let env = TestEnv::new();
    let s = TestServer::start_in("/");
    s.tmux(&["rename-window", "-t", "alpha:0", "editor"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "win two"]);
    s.tmux(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50", "-c", "/", "-n", "logs"]);
    s.tmux(&["new-window", "-d", "-t", "beta:", "-n", "build"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "beta:logs"]);
    s.tmux(&["set-option", "-p", "-t", "beta:logs.1", "@pane_role", "sidebar"]);
    s.tmux(&["select-pane", "-t", "beta:logs.1"]);
    s.tmux(&["send-keys", "-t", "beta:logs.0", "echo MAIN-PANE-MARKER", "Enter"]);
    s.tmux(&["send-keys", "-t", "beta:logs.1", "echo SIDEBAR-MARKER", "Enter"]);
    s.tmux(&["set-option", "-g", "escape-time", "50"]);
    install_binding(&s);
    let o = Outer::attach(&s, "alpha:editor", 200, 50);
    (env, s, o)
}

pub fn fmt(s: &TestServer, target: &str, f: &str) -> String {
    s.tmux(&["display-message", "-p", "-t", target, f]).trim().to_string()
}

pub fn wid(s: &TestServer, target: &str) -> String {
    fmt(s, target, "#{window_id}")
}

pub fn window_ids(s: &TestServer) -> Vec<String> {
    s.tmux(&["list-windows", "-a", "-F", "#{window_id}"]).lines().map(String::from).collect()
}

/// "<session>:<window name>" of the (first) attached client.
pub fn client_at(s: &TestServer) -> String {
    s.tmux(&["list-clients", "-F", "#{session_name}:#{window_name}"])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Session names of the popup's group headers, top to bottom.
pub fn groups(screen: &str) -> Vec<String> {
    screen
        .lines()
        .filter_map(|l| l.strip_prefix("─ "))
        .filter_map(|l| l.split(' ').next())
        .map(String::from)
        .collect()
}

/// Window rows on screen.
pub fn window_rows(screen: &str) -> usize {
    let r = regex::Regex::new(r"^[▌ ][▶ ] +\d+  ").unwrap();
    screen.lines().filter(|l| r.is_match(l)).count()
}
```

- [ ] **Step 4: Write the failing e2e tests** — create `tests/popup_e2e.rs`:

```rust
//! The real popup, opened with tmux-home's binding through a client on a
//! throwaway server, read back with capture-pane (bash suite port, part 1).
mod common;
use common::*;
use std::time::Duration;

#[test]
fn opens_grouped_with_header_and_side_preview() {
    let (_env, s, o) = popup_fixture();
    assert!(!s.tmux(&["list-clients", "-F", "#{client_name}"]).trim().is_empty());
    o.open_popup();
    o.wait_for("tmux-home +alpha ▸ 0");
    let screen = o.screen();
    assert_eq!(groups(&screen), ["alpha", "beta"]);
    assert!(regex::Regex::new(r"(?m)^▌▶ +0  editor").unwrap().is_match(&screen), "{screen}");
    assert!(screen.contains('│'), "side preview at 200 cols:\n{screen}");
    // beta:logs previews its main pane, never its (active) sidebar
    o.keys(&["Down", "Down"]);
    o.wait_for(r"^▌ +0  logs");
    o.wait_for("MAIN-PANE-MARKER");
    assert!(!o.screen().contains("SIDEBAR-MARKER"));
}

#[test]
fn filter_esc_and_navigation() {
    let (_env, _s, o) = popup_fixture();
    o.open_popup();
    o.typed("bui");
    o.wait_for(r"^> bui .*1/4");
    assert_eq!(window_rows(&o.screen()), 1, "only beta build left");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
    assert!(o.popup_open());
    for (k, row) in [
        ("C-n", r"^▌ +1  win two"),
        ("C-j", r"^▌ +0  logs"),
        ("Down", r"^▌ +1  build"),
        ("C-p", r"^▌ +0  logs"),
        ("C-k", r"^▌ +1  win two"),
        ("Up", r"^▌▶ +0  editor"),
    ] {
        o.keys(&[k]);
        o.wait_for(row);
    }
}

#[test]
fn rename_inline() {
    let (_env, s, o) = popup_fixture();
    let target = wid(&s, "alpha:editor");
    o.open_popup();
    o.keys(&["C-r"]);
    o.wait_for("rename › editor");
    o.keys(&["C-u"]);
    o.typed("renamed (x)+y, z");
    o.keys(&["Enter"]);
    wait_until("renamed by ID", || fmt(&s, &target, "#{window_name}") == "renamed (x)+y, z");
    o.wait_for(r"^> +.*4/4");
    o.wait_for(r"▶ +0  renamed \(x\)\+y, z");
    // the pre-fill round-trips ( ) + ,
    o.keys(&["C-r"]);
    o.wait_for(r"rename › renamed \(x\)\+y, z");
    o.keys(&["Escape"]);
    o.wait_gone("rename ›");
    assert_eq!(fmt(&s, &target, "#{window_name}"), "renamed (x)+y, z");
    // an empty name cancels
    o.keys(&["C-r"]);
    o.wait_for("rename › renamed");
    o.keys(&["C-u", "Enter"]);
    o.wait_gone("rename ›");
    assert_eq!(fmt(&s, &target, "#{window_name}"), "renamed (x)+y, z");
    // a filter with ( ) + survives an editor round-trip
    o.typed("(x)+");
    o.wait_for(r"^> \(x\)\+ .*1/4");
    o.keys(&["C-r"]);
    o.wait_for("rename › renamed");
    o.keys(&["Escape"]);
    o.wait_for(r"^> \(x\)\+ .*1/4");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
    // renaming a filtered selection hits that window; the filter stays
    o.typed("win");
    o.wait_for(r"^> win .*1/4");
    o.keys(&["C-r"]);
    o.wait_for("rename › win two");
    o.keys(&["BSpace", "BSpace", "BSpace"]);
    o.typed("2");
    o.keys(&["Enter"]);
    wait_until("win 2", || fmt(&s, "alpha:1", "#{window_name}") == "win 2");
    o.wait_for(r"^> win .*1/4");
}

#[test]
fn reset_preview_toggle_and_help() {
    let (_env, s, o) = popup_fixture();
    let target = wid(&s, "alpha:editor");
    o.open_popup();
    assert_eq!(fmt(&s, &target, "#{?automatic-rename,on,off}"), "off");
    o.keys(&["M-r"]);
    wait_until("automatic-rename on", || fmt(&s, &target, "#{?automatic-rename,on,off}") == "on");
    assert!(o.popup_open());
    o.keys(&["C-o"]);
    o.wait_gone("│");
    o.keys(&["C-o"]);
    o.wait_for("│");
    for k in ["F1", "C-/"] {
        o.keys(&[k]);
        o.wait_for("press any key to return");
        o.keys(&["q"]);
        o.wait_for(r"^> +.*4/4");
    }
}

#[test]
fn enter_switches_and_esc_closes() {
    let (_env, s, o) = popup_fixture();
    o.open_popup();
    o.typed("build");
    o.wait_for(r"^> build .*1/4");
    o.keys(&["Enter"]);
    o.wait_gone("F1 help");
    let want = fmt(&s, "beta:build", "#{session_id} #{window_id}");
    wait_until("client on beta:build", || {
        s.tmux(&["list-clients", "-F", "#{session_id} #{window_id}"]).trim() == want
    });
    o.open_popup();
    o.wait_for("tmux-home +beta ▸ 1");
    assert_eq!(groups(&o.screen()), ["beta", "alpha"]);
    o.keys(&["Escape"]);
    o.wait_gone("F1 help");
}

#[test]
fn layout_150x40_has_no_side_preview() {
    let (_env, _s, o) = popup_fixture();
    o.tmux(&["resize-window", "-t", "outer", "-x", "150", "-y", "40"]);
    std::thread::sleep(Duration::from_millis(500));
    o.open_popup();
    let screen = o.screen();
    assert!(!screen.contains('│'), "{screen}");
    assert!(
        screen.lines().any(|l| l.chars().count() >= 150 && l.chars().all(|c| c == '─')),
        "bottom preview border:\n{screen}"
    );
}
```

- [ ] **Step 5: Run them to see them fail**

Run: `cargo test --test plugin --test popup_e2e`
Expected: the plugin tests fail on the default key (`prefix .` not bound) and on `{BIN} popup` not appearing. The e2e tests time out in `open_popup` (`prefix .` isn't bound to tmux-home yet), each panic printing the screen.

- [ ] **Step 6: Implement `tmux-home.tmux`** — replace it with:

```bash
#!/usr/bin/env bash
# TPM entry point: binds the tmux-home popup and starts its daemon.
#
#   set -g @home-keys '.'   # prefix keys to bind (default "."; '' = none)
#
# Environment (tests): TMUX_HOME_TMUX  tmux command, word-split (default: tmux)
#                      TMUX_HOME_BIN   the Rust binary (default: target/release/tmux-home)

set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_BIN=${TMUX_HOME_BIN:-$CURRENT_DIR/target/release/tmux-home}
BASH_POPUP="$CURRENT_DIR/bin/tmux-home"

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
popup="${tmux_q}display-popup -c #{q:client_name} -E -B -w 100% -h 100% -e TMUX_HOME_CLIENT=#{q:client_name}"
rust_q=$(printf '%q' "$RUST_BIN")
# Which popup runs is decided when the key is pressed, so a release build
# that appears later (first build, TPM update) is used without a reload.
cmd="if [ -x $rust_q ]; then $popup $rust_q popup; else $popup $(printf '%q' "$BASH_POPUP"); fi"

for key in $keys; do
	t bind-key "$key" run-shell -b "$cmd"
done

# The daemon: one per server, started if the binary is built. A second start
# is a no-op (lock), and it exits with the server.
if [[ -x $RUST_BIN ]]; then
	t run-shell -b "$rust_q daemon --socket #{q:socket_path}"
fi
```

- [ ] **Step 7: Keep the bash suite green and pinned to the bash popup.** In `tests/run`:

After `export TMUX_HOME_TMUX="tmux -L tmux-home-test"` add:

```bash
# this suite tests the bash popup: never the Rust one the binding prefers
export TMUX_HOME_BIN=/nonexistent/tmux-home
```

Replace the three binding checks (the lines from `T set-option -g @home-keys ''` to `check 'default binds prefix f' …`) with:

```bash
T set-option -g @home-keys ''
"$ROOT/tmux-home.tmux"
check "@home-keys '' binds nothing" bash -c '! tmux -L tmux-home-test list-keys -T prefix | grep -q "tmux-home"'
T set-option -gu @home-keys
"$ROOT/tmux-home.tmux"
check 'default binds prefix .' bash -c 'tmux -L tmux-home-test list-keys -T prefix | grep -Eq "prefix +\. +run-shell -b .*display-popup .*TMUX_HOME_CLIENT=#\{q:client_name\}"'
check 'default leaves prefix w alone' bash -c '! tmux -L tmux-home-test list-keys -T prefix | grep -E "prefix +w .*tmux-home"'
```

and in `open_popup` change `keys C-b w` to `keys C-b .` and `popup opens via prefix w` to `popup opens via prefix .`.

- [ ] **Step 8: Run everything**

```bash
cargo test --test plugin --test popup_e2e
tests/run | tail -1
cargo test
```

Expected: plugin 5 passed and popup_e2e 6 passed. `tests/run` ends `passed: 143  failed: 0`. The full `cargo test` is green.

- [ ] **Step 9: Lint, commit, push the checkpoint**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock tmux-home.tmux tests/run tests/common/mod.rs tests/plugin.rs tests/popup_e2e.rs
timeout 60 git commit -m "prefix . opens the Rust popup; e2e harness and first parity tests"
git push -u origin r1-popup
gh pr create --draft --title "R1: Rust popup parity" --body "Implements docs/superpowers/plans/2026-10-01-r1-popup-parity.md (in progress)."
```

---

### Task 15: e2e parity part 2 — close, confirm, reopen, last window, reorder, new window, live

**Files:**
- Test: `tests/popup_manage_e2e.rs` (new)

**Interfaces:**
- Consumes: the `tests/common` harness (Task 14), `store::ClosedStack` (Task 3).
- Produces: no new code. These tests pin the remaining parity rows (72–134), plus `M-↑↓`, `M-n` and live redraw. If one fails, the fix goes in the module that owns the behaviour (`app`, `exec`, `ops`, `shape`), with a unit test there first.

- [ ] **Step 1: Write the tests** — create `tests/popup_manage_e2e.rs`:

```rust
//! The real popup managing windows (bash suite port, part 2).
mod common;
use common::*;
use std::time::Duration;
use tmux_home::store::ClosedStack;

/// Filters to `name`, closes it with ^x (it must close at once) and clears the filter.
fn close_idle(o: &Outer, name: &str) {
    o.typed(name);
    o.wait_for(&format!(r"^> {name} .*1/"));
    o.keys(&["C-x"]);
    o.wait_for(&format!(r"^> {name} .*0/"));
    o.keys(&["Escape"]);
}

#[test]
fn close_idle_current_and_cursor() {
    let (_env, s, o) = popup_fixture();
    for n in ["qone", "qtwo", "qnext"] {
        s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", n]);
    }
    s.tmux(&["send-keys", "-t", "alpha:qnext", "echo QNEXT-MARKER", "Enter"]);
    s.tmux(&["new-window", "-t", "alpha:", "-n", "qcur"]); // not -d: the client's current window
    let qcur = wid(&s, "alpha:qcur");
    o.open_popup();
    o.wait_for(&format!("tmux-home +alpha ▸ {}", fmt(&s, &qcur, "#{window_index}")));
    o.wait_for(r"^▌▶ +\d+  qcur");

    // the client's current, idle window closes at once; the client moves, the popup stays
    o.keys(&["C-x"]);
    o.wait_for(r"^> +.*7/7");
    assert!(!window_ids(&s).contains(&qcur));
    assert!(!s.tmux(&["list-clients", "-F", "#{client_name}"]).trim().is_empty());
    let at = client_at(&s);
    assert!(at.starts_with("alpha:") && at != "alpha:qcur", "{at}");
    let cur = s.tmux(&["list-clients", "-F", "#{window_id}"]).trim().to_string();
    o.wait_for(&format!("tmux-home +alpha ▸ {}", fmt(&s, &cur, "#{window_index}")));
    assert!(o.popup_open());

    // an idle window closes at once, the filter stays
    let qone = wid(&s, "alpha:qone");
    o.typed("qone");
    o.wait_for(r"^> qone .*1/7");
    o.keys(&["C-x"]);
    o.wait_for(r"^> qone .*0/6");
    assert!(!window_ids(&s).contains(&qone));
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*6/6");

    // closing a row leaves the cursor on the next one
    o.keys(&["Down", "Down"]);
    o.wait_for(r"^▌ +\d+  qtwo");
    o.keys(&["C-x"]);
    o.wait_for(r"^> +.*5/5");
    o.wait_for(r"^▌ +\d+  qnext");
    o.wait_for("QNEXT-MARKER");
    assert!(!s.tmux(&["list-windows", "-t", "alpha", "-F", "#W"]).lines().any(|l| l == "qtwo"));

    // ^x does nothing while the rename editor is open
    o.keys(&["C-r"]);
    o.wait_for("rename › qnext");
    o.keys(&["C-x"]);
    std::thread::sleep(Duration::from_millis(500));
    assert!(o.screen().contains("rename › qnext"));
    assert!(window_ids(&s).contains(&wid(&s, "alpha:qnext")));
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*5/5");
}

#[test]
fn close_asks_when_busy() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qbusy", "sleep 1000"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qside"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:qside", "sleep 1000"]);
    s.tmux(&["set-option", "-p", "-t", "alpha:qside.1", "@pane_role", "sidebar"]);
    s.wait_settled();
    let qbusy = wid(&s, "alpha:qbusy");
    o.open_popup();
    o.typed("qbusy");
    o.wait_for(r"^> qbusy .*1/6");
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "qbusy"\? running: sleep \(y/N\)"#);
    o.typed("n");
    o.wait_for(r"^> qbusy .*1/6");
    assert!(window_ids(&s).contains(&qbusy), "kept after n");
    for cancel in ["Escape", "Enter"] {
        o.keys(&["C-x"]);
        o.wait_for(r#"^close "qbusy"\?"#);
        o.keys(&[cancel]);
        o.wait_for(r"^> qbusy .*1/6");
        assert!(window_ids(&s).contains(&qbusy), "kept after {cancel}");
    }
    assert!(o.popup_open());
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "qbusy"\?"#);
    o.typed("y");
    o.wait_for(r"^> qbusy .*0/5");
    assert!(!window_ids(&s).contains(&qbusy));
    o.keys(&["Escape"]);
    // a sidebar pane running a program doesn't make the window busy
    o.typed("qside");
    o.wait_for(r"^> qside .*1/5");
    o.keys(&["C-x"]);
    o.wait_for(r"^> qside .*0/4");
}

#[test]
fn reopen_restores_and_selects() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qnext"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qside"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:qside", "sleep 1000"]);
    s.tmux(&["set-option", "-p", "-t", "alpha:qside.1", "@pane_role", "sidebar"]);
    o.open_popup();
    close_idle(&o, "qside");
    close_idle(&o, "qnext");
    o.wait_for(r"^> +.*4/4");
    let before = client_at(&s);

    o.keys(&["C-t"]);
    o.wait_for(r"^> +.*5/5");
    assert!(s.tmux(&["list-windows", "-t", "alpha", "-F", "#W"]).lines().any(|l| l == "qnext"));
    assert_eq!(client_at(&s), before, "client not switched");
    o.wait_for(r"^▌ +\d+  qnext");

    o.typed("build");
    o.wait_for(r"^> build .*1/5");
    o.keys(&["C-t"]);
    o.wait_for(r"^> +.*6/6");
    o.wait_for(r"^▌ +\d+  qside");

    close_idle(&o, "qnext");
    close_idle(&o, "qside");
    o.wait_for(r"^> +.*4/4");
    ClosedStack::for_socket(&s.socket).unwrap().lock().unwrap().save(&[]).unwrap();
    o.keys(&["C-t"]);
    o.wait_for("nothing to reopen");
    assert_eq!(window_ids(&s).len(), 4, "nothing created");
    o.typed("x");
    o.wait_gone("nothing to reopen");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
}

#[test]
fn close_last_window_of_client_session() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["new-session", "-d", "-s", "qdelta", "-c", "/", "-n", "qd"]);
    let client = s.tmux(&["list-clients", "-F", "#{client_name}"]).lines().next().unwrap().to_string();
    s.tmux(&["switch-client", "-c", &client, "-t", "qdelta"]);
    o.open_popup();
    o.wait_for("tmux-home +qdelta ▸ 0");
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "qd"\? — session "qdelta" will end \(y/N\)"#);
    o.typed("y");
    o.wait_for(r"^> +.*4/4");
    assert!(!s.tmux(&["list-sessions", "-F", "#S"]).lines().any(|l| l == "qdelta"));
    assert!(!s.tmux(&["list-clients", "-F", "#{client_name}"]).trim().is_empty(), "client still attached");
    let at = client_at(&s);
    assert!(at.starts_with("alpha:") || at.starts_with("beta:"), "{at}");
    o.wait_for(r"tmux-home +(alpha|beta) ▸ \d");
    assert!(o.popup_open());
}

#[test]
fn reorder_and_new_window() {
    let (_env, s, o) = popup_fixture();
    let editor = wid(&s, "alpha:editor");
    o.open_popup();
    o.keys(&["M-Down"]);
    wait_until("editor at index 1", || fmt(&s, &editor, "#{window_index}") == "1");
    o.wait_for(r"^▌▶ +1  editor");
    assert_eq!(
        s.tmux(&["list-clients", "-F", "#{window_id}"]).trim(),
        editor,
        "swap -d keeps the client's window"
    );
    o.keys(&["M-Up"]);
    wait_until("editor back at index 0", || fmt(&s, &editor, "#{window_index}") == "0");
    o.wait_for(r"^▌▶ +0  editor");

    o.keys(&["M-n"]);
    o.wait_for("new window › ");
    o.typed("fresh #1");
    o.keys(&["Enter"]);
    wait_until("fresh #1 after editor", || {
        s.tmux(&["list-windows", "-t", "alpha", "-F", "#W"]).lines().collect::<Vec<_>>()
            == ["editor", "fresh #1", "win two"]
    });
    o.wait_for(r"^▌ +1  fresh #1");
    o.wait_for(r"^> +.*5/5");
}

#[test]
fn live_redraw_keeps_the_editor() {
    let (_env, s, o) = popup_fixture();
    o.open_popup();
    o.keys(&["C-r"]);
    o.wait_for("rename › editor");
    o.typed("-x");
    s.tmux(&["new-window", "-d", "-t", "beta:", "-n", "pushed"]);
    o.wait_for(r"^> +.*5/5");
    o.wait_for("rename › editor-x");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*5/5");
    o.keys(&["Down"]);
    o.wait_for(r"^▌ +1  win two");
    o.keys(&["C-r"]);
    o.wait_for("rename › win two");
    s.tmux(&["kill-window", "-t", "alpha:1"]);
    o.wait_for("window closed while renaming");
    o.wait_for(r"^> +.*4/4");
}
```

- [ ] **Step 2: Run them**

Run: `cargo test --test popup_manage_e2e`
Expected: 6 passed. Tasks 9–13 already implement these behaviours, so a failure here is a real bug. Debug it with superpowers:systematic-debugging: reproduce the failing step on a throwaway server (the Task 13 smoke recipe), find the owning module, add a failing unit test there, fix it, and re-run both.

- [ ] **Step 3: Full checks and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test && tests/run | tail -1
git add tests/popup_manage_e2e.rs
timeout 60 git commit -m "e2e: close, confirm, reopen, last window, reorder, new window, live redraw"
```

---

### Task 16: Retire the bash popup

Do this only once every parity row is green. Step 1 is the gate.

**Files:**
- Delete: `bin/tmux-home`, `tests/run`
- Modify: `tmux-home.tmux` (fallback → "building" message + background build), `tests/plugin.rs`, `README.md`

**Interfaces:**
- Produces: with no binary, the key shows `tmux-home: building …`. When the plugin loads without a binary (and with `TMUX_HOME_BIN` unset), it runs `cargo build --release` in the plugin dir in the background.

- [ ] **Step 1: Parity gate — every Rust test named in the parity table exists and passes**

```bash
cargo test 2>&1 | grep -E '^test result:|FAILED|^error' | sort | uniq -c
cargo test -- --list 2>/dev/null | sed 's/: test$//' > /tmp/th-r1-tests.txt
for t in one_row_per_window no_client_sessions_by_name_windows_by_index rows_carry_session_and_window_ids \
  sidebar_pane_never_the_row_pane sidebar_panes_are_flagged capture_shows_the_main_pane_not_the_sidebar \
  rename_by_id_keeps_odd_characters rename_turns_automatic_rename_off reset_turns_automatic_rename_back_on \
  busy_idle_shell_is_empty busy_lists_each_non_shell_command busy_ignores_sidebar_panes busy_ignores_sidebar \
  close_refuses_last_window_on_server last_window_on_server_is_refused close_then_reopen_restores_shape \
  reopen_cli_prints_the_new_window_id reopen_keeps_automatic_rename reopen_after_old_neighbour_when_index_taken \
  reopen_recreates_ended_session stack_is_lifo_capped_at_ten keeps_the_last_ten preview_layout_by_size \
  side_preview_border_at_200_cols bottom_preview_at_150x40 small_terminal_hides_preview_until_ctrl_o \
  empty_home_keys_binds_nothing default_binds_prefix_dot_to_the_rust_popup custom_keys_bind_each \
  opens_grouped_with_header_and_side_preview header_follows_client client_session_first \
  view_items_group_by_session current_window_marked filter_esc_and_navigation typing_filters_and_returns_to_top \
  fuzzy_keeps_row_order esc_clears_then_quits navigation_keys_cycle rename_inline \
  ctrl_r_prefills_and_enter_renames_by_id rename_esc_cancels rename_empty_cancels special_characters_are_literal \
  filter_survives_rename reset_preview_toggle_and_help alt_r_resets_selected ctrl_o_toggles_preview \
  help_opens_and_any_key_returns help_screen enter_switches_and_esc_closes close_idle_current_and_cursor \
  list_starts_on_client_window close_idle_is_immediate_unconfirmed idle_close_rechecks_then_closes \
  filter_kept_after_close cursor_moves_to_next_row_after_close list_keys_ignored_while_renaming \
  close_asks_when_busy close_busy_asks_with_commands close_plan_busy_asks confirm_cancels_on_anything_but_y \
  confirm_y_closes sidebar_only_activity_closes_at_once close_plan_ignores_sidebar reopen_restores_and_selects \
  reopened_window_selected_and_filter_cleared nothing_to_reopen_notice_clears_on_key reopen_reports_back \
  close_last_window_of_client_session last_window_of_session_warns close_plan_last_in_session_warns \
  enter_switches_invoking_client_and_quits switch_then_quit layout_150x40_has_no_side_preview \
  reorder_and_new_window live_redraw_keeps_the_editor snapshot_never_touches_open_editor; do
  grep -Eq "(^|::)$t\$" /tmp/th-r1-tests.txt || echo "MISSING: $t"
done; echo gate-done
tests/run | tail -1
```

Expected: every `test result:` line is `ok`, with no `FAILED` or `error` lines. No `MISSING:` line is printed before `gate-done`, and `tests/run` passes 143. Any MISSING or failing row stops the task until it's fixed. `tests/run` uses fixed server names (`tmux-home-test*`), so a run elsewhere on the machine at the same time makes it fail in a burst of `no server running on …tmux-home-test-outer`. If that happens, rerun it alone.

- [ ] **Step 2: Write the failing plugin test** — in `tests/plugin.rs`, replace `missing_binary_falls_back_to_bash_popup` with:

```rust
#[test]
fn missing_binary_shows_building_message() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    run_plugin(&s, "/nonexistent/tmux-home");
    let b = binding(&s, ".").expect("prefix . bound");
    assert!(b.contains("display-message"), "{b}");
    assert!(b.contains("building"), "{b}");
    assert!(!b.contains("bin/tmux-home"), "no bash fallback: {b}");
}
```

Run: `cargo test --test plugin missing_binary`
Expected: FAIL (the binding still falls back to `bin/tmux-home`).

- [ ] **Step 3: Update `tmux-home.tmux`** — replace the lines from `rust_q=$(printf '%q' "$RUST_BIN")` to the end of the file with:

```bash
rust_q=$(printf '%q' "$RUST_BIN")
# ASCII only: printf %q would $'…'-quote anything else, which run-shell's
# /bin/sh (dash on Linux) can't read
building=$(printf '%q' "tmux-home: building (cargo build --release in $CURRENT_DIR), try again in a minute")
# Which binary runs is decided when the key is pressed, so a build that
# finishes later (first install, TPM update) is used without a reload.
cmd="if [ -x $rust_q ]; then $popup $rust_q popup; else ${tmux_q}display-message -c #{q:client_name} $building; fi"

for key in $keys; do
	t bind-key "$key" run-shell -b "$cmd"
done

if [[ -x $RUST_BIN ]]; then
	# The daemon: one per server; a second start is a no-op (lock), and it
	# exits with the server.
	t run-shell -b "$rust_q daemon --socket #{q:socket_path}"
elif [[ -z ${TMUX_HOME_BIN:-} ]] && cargo=$(command -v cargo); then
	# First load without a build (spec §10): build in the background.
	t run-shell -b "cd $(printf '%q' "$CURRENT_DIR") && $(printf '%q' "$cargo") build --release >/dev/null 2>&1"
fi
```

Also delete the now-unused `BASH_POPUP=…` line, and in the header comment change `TMUX_HOME_BIN   the Rust binary` to `TMUX_HOME_BIN   the Rust binary; when set, a missing one is not built`.

- [ ] **Step 4: Delete the bash popup and its suite**

```bash
git rm bin/tmux-home tests/run
rmdir bin 2>/dev/null || true
grep -rln "bin/tmux-home\|tests/run" src tests tmux-home.tmux README.md Cargo.toml \
  | grep -v '^tests/plugin.rs$' || echo "no references left"
```

Expected: only `README.md`, which Step 5 rewrites. `tests/plugin.rs` only mentions it in the negative assertion of Step 2. Historical mentions under `docs/superpowers/` (spec, plans, notes) stay as written. After Step 5 the same command prints `no references left`.

- [ ] **Step 5: Rewrite `README.md`** — replace the whole file with:

````markdown
# tmux-home

A full-window home screen for tmux: every session and window in one popup,
with rename, reorder, close and reopen — and you never leave it until you
choose where to go.

> **Status: R1.** A Rust popup over a per-server daemon: grouped window
> list, type-first fuzzy filter, live preview, `⏎` switch, inline rename,
> reorder, new window, close with an inline confirm, and reopen of closed
> windows (kept across tmux restarts). Agents, git badges and the sidebar
> are next — see
> [`docs/superpowers/specs/2026-09-29-rust-daemon-design.md`](docs/superpowers/specs/2026-09-29-rust-daemon-design.md) §13.

## Why

- `choose-tree` can't rename and can't take custom keys.
- `find-window` asks for a search term in the status line before showing
  anything.
- The fzf-based pickers close the popup to rename a window, or only rename
  sessions.

tmux-home puts the overview and the management in one place, and keeps you
in it.

## Install

With [TPM](https://github.com/tmux-plugins/tpm):

```tmux
set -g @plugin 'tim-codes/tmux-home'
# set -g @home-keys '.'   # prefix keys to bind (default); '' binds nothing
```

tmux-home is a Rust binary. TPM doesn't build it, so the plugin starts
`cargo build --release` in its directory the first time it loads without
one; until that finishes, the key says so. To build it yourself:

```sh
cd ~/.tmux/plugins/tmux-home && cargo build --release
```

Or clone it, build, and add `run-shell ~/path/to/tmux-home/tmux-home.tmux`.

## Keys

| Key | Action |
| --- | --- |
| type | filter (session, index, name, command, path; fzf-like syntax) |
| `↑` `↓` `^p` `^n` `^k` `^j` | move |
| `PgUp` `PgDn` | page |
| `⏎` | switch to the window, close the popup |
| `Esc` | clear the filter; close when it's empty |
| `^r` | rename inline (`⏎` save, `Esc` or empty cancels) |
| `M-r` | back to the automatic name |
| `M-n` | new window after the selection, named inline (empty: automatic) |
| `M-↑` `M-↓` | move the window up/down within its session |
| `^x` | close the window, staying in the popup (see below) |
| `^t` | reopen the last closed window |
| `^o` | toggle the preview |
| `F1` `^/` | help |

`^x` closes the window at once when every pane in it is idle at a shell
prompt (bash, zsh, fish, sh, …). Sidebar panes don't count. Otherwise it
asks inline, for example `close "api"? running: nvim, node (y/N)`. Only
`y` closes; any other key, `⏎` or `Esc` cancels. A session's last window
always asks (`session "x" will end`). If that's the session you're in,
tmux-home first moves you to another session so the popup isn't detached.
The last window on the server is never closed.

`^t` (or `tmux-home reopen` from a shell or your own binding) rebuilds the
most recently closed window, like reopening a browser tab, up to 10 back.
You get the same session (recreated if it ended), place, name, pane count,
layout, directories and active pane, but **fresh shells**: whatever was
running in it is gone. The popup stays open with the cursor on it. The stack
is `${XDG_STATE_HOME:-~/.local/state}/tmux-home/<server>/closed.json`
(`TMUX_HOME_STATE_DIR` overrides the root). A stack left by the old bash
popup is imported once.

## How it works

One daemon per tmux server keeps a live snapshot of every session, window
and pane, and pushes it to the popup over a unix socket. `tmux-home.tmux`
starts the daemon, and the popup starts one if it can't reach it. Without a
daemon the popup reads tmux itself, so nothing breaks. The daemon polls tmux
every 500 ms by default; `--source control` (a tmux control-mode client) is
available but isn't the default — see
[`docs/superpowers/notes/r0-control-mode.md`](docs/superpowers/notes/r0-control-mode.md).
`tmux-home query --json` prints the current snapshot.

## Tests

`cargo test`. Every test uses throwaway `tmux -L th-test-*` servers and temp
state dirs; none touch your tmux server. The end-to-end tests open the real
popup through a client attached to a throwaway server and read it back with
`capture-pane`.

## Requirements

tmux ≥ 3.3 (tested with 3.7c) and a stable Rust toolchain to build.

## Licence

MIT
````

- [ ] **Step 6: Run everything**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: all green, with no `tests/run` left to run.

- [ ] **Step 7: Commit, push, mark the PR ready for review**

```bash
git add -A tmux-home.tmux tests/plugin.rs README.md
timeout 60 git commit -m "Retire the bash popup: the Rust popup is tmux-home"
git push
gh pr edit --body "$(cat <<'EOF'
R1 — Rust popup parity (docs/superpowers/plans/2026-10-01-r1-popup-parity.md).

- `tmux-home popup`: ratatui popup over the daemon's snapshot stream (direct reads when the daemon is down)
- grouped list, fuzzy filter, preview, ⏎ via the invoking client, ^r / M-r, M-n, M-↑↓, ^x with the confirm rules, ^t
- closed.json store (10-deep LIFO, atomic, state.lock), `tmux-home reopen`, one-time import of the bash stack
- daemon `refresh` op
- `prefix .` → Rust popup; bash popup and tests/run deleted after all 143 parity rows went green
EOF
)"
gh pr ready
```

(The repo is a personal one, so merging waits for the operator's go-ahead per the global workflow. Don't merge here.)

---

## Self-review notes (for the plan's reviewer)

- **Spec coverage:** §7 popup → Tasks 7–13. §4 degraded mode → Task 13 (`Feed`), Task 6 (`subscribe` starts a daemon). §9 persistence → Task 3 (+ import), Task 5. §10 install → Tasks 14 and 16. §12 testing → unit tests per module, `TestBackend` + `insta` (Task 11), e2e (Tasks 14–15). §13 R1 → all tasks, with the bash deletion last (Task 16, gated). SPEC §3–§6 key map → Tasks 9–10, help text in Task 11.
- **Not in R1, on purpose:** agents/NEEDS YOU/`^g`/filter tokens (R2), git badges/Repos view/density (R3), sidebar (R4), session rename from a header (ruling 7), the cross-client resize hint (SPEC M3), and `M-→`/`M-←` pane expansion (SPEC §5; the bash popup never had it, so it isn't a parity item).
- **Checked against real tmux before handoff:** every code block was applied to a scratch copy of `main` and run on tmux 3.7c, macOS, 2026-10-01. Results: `cargo clippy --all-targets -D warnings` clean, every Rust test passing (unit, integration, plugin and the 12 e2e), and `tests/run` at 143/143 with the new binding. That run found and fixed four things now in the plan: macOS `/bin/sh` reports as `bash`; `new-window` without `-c` takes the creating command's cwd (hence `start_in` runs `tmux()` from `cwd`); a lone Escape through nested tmux arrives ~400 ms late (`ESC_GAP`); and the stale first snapshot (ruling 15).
