# R1 parity: bash `tests/run` → Rust tests

The bash popup (`bin/tmux-home`) and its suite (`tests/run`) were deleted in
pass 2 once every row below was covered. Row numbers follow the parity table
in the R1 plan (`docs/superpowers/plans/2026-10-01-r1-popup-parity.md`, 143
rows). The last bash suite ran 142 checks: it had one "default binds prefix
." check where the plan's table has rows 32 and 33 (`w` and `f`), and its
"re-close $n" check ran twice (rows 118 and 119).

Every test listed here drives a real tmux server (`tmux -L th-test-*`, with
`th-outer-*` hosting the client for e2e) except the `src/` unit tests, which
cover the pure state the bash suite checked through the screen.

Where the tests live:

| Short | File | Kind |
| --- | --- | --- |
| `app::` | `src/popup/app.rs` (`mod tests`) | unit: rows, filter, keys, modes |
| `popup::` | `src/popup/mod.rs` (`mod tests`) | unit: layout, drawing (ratatui `TestBackend`) |
| `ops::` | `tests/ops.rs` | real tmux: `ops::Tx` writes, busy, capture |
| `cr::` | `tests/close_reopen.rs` | real tmux: close, reopen, stack, `tmux-home reopen` |
| `store::` | `tests/store.rs` | closed.json on disk |
| `plugin::` | `tests/plugin.rs` | real tmux: `tmux-home.tmux` bindings |
| `e2e::` | `tests/popup_e2e.rs` | real popup via `prefix .`, read back with capture-pane |
| `e2e2::` | `tests/popup_manage_e2e.rs` | real popup: close, reopen, reorder, new window, daemon |

| # | bash check | Rust test(s) |
| --- | --- | --- |
| 1 | list: one row per window | `app::one_row_per_window_with_ids_and_main_pane` |
| 2 | list: no client -> sessions by name, windows by index | `app::no_client_sessions_by_name_windows_by_index` |
| 3 | list: rows carry session and window IDs | `app::one_row_per_window_with_ids_and_main_pane` |
| 4 | list: sidebar pane never chosen as the row pane | `app::one_row_per_window_with_ids_and_main_pane`, `ops::capture_shows_the_main_pane_not_the_sidebar` |
| 5 | preview: captures the non-sidebar pane | `ops::capture_shows_the_main_pane_not_the_sidebar`, `e2e::opens_grouped_with_header_and_side_preview` |
| 6 | preview: does not capture the sidebar pane | same two |
| 7 | rename: by ID, name with - ( ) + , quote | `ops::rename_by_id_keeps_odd_characters`, `ops::rename_keeps_hash_literally` |
| 8 | rename: turns automatic-rename off | `ops::rename_turns_automatic_rename_off` |
| 9 | reset-name: automatic-rename back on | `ops::reset_name_reenables_automatic_rename_when_global_is_off` |
| 10 | busy: idle shell -> nothing | `ops::busy_idle_shell_is_empty` |
| 11 | busy: lists each non-shell command | `ops::busy_lists_each_non_shell_command` |
| 12 | busy: sidebar panes do not count | `ops::busy_ignores_sidebar_panes` |
| 13 | close: last window on the server refused (exit 3) | `cr::close_refuses_last_window_on_server` (`CloseOutcome::Refused`; there is no `close` CLI, so no exit code), `e2e2::close_refuses_the_last_window_on_the_server` |
| 14 | close: ... and the window survives | same two |
| 15 | close pushes a snapshot | `cr::close_then_reopen_restores_shape` |
| 16 | ... and the window is gone | same |
| 17 | reopen prints the new window ID | `cr::close_then_reopen_restores_shape` (return value), `cr::reopen_cli_prints_the_new_window_id` |
| 18 | reopen restores index, name, panes, layout, cwds, active pane | `cr::close_then_reopen_restores_shape` |
| 19 | reopen pops the stack | same |
| 20 | reopen keeps automatic-rename on | `cr::reopen_keeps_automatic_rename` |
| 21 | reopen with its index taken goes after its old neighbour | `cr::reopen_after_old_neighbour_when_index_taken`, `cr::reopen_before_old_right_neighbour_when_left_is_gone` |
| 22 | closing the last window ends the session | `cr::reopen_recreates_ended_session` |
| 23 | reopen recreates the session with the window | same, `ops::reopen_restores_hash_name_session_and_cwds` |
| 24 | stack keeps the last 10 closes | `cr::stack_is_lifo_capped_at_ten`, `store::round_trip_lifo_capped` |
| 25 | reopen is LIFO | `cr::stack_is_lifo_capped_at_ten` |
| 26 | reopen on an empty stack exits 4 | same (runs `tmux-home reopen`) |
| 27 | ... and creates nothing | same |
| 28 | layout: >=160 cols -> right | `popup::preview_layout_by_size`, `popup::side_and_bottom_borders` |
| 29 | layout: <160 cols, >=30 rows -> bottom | same two, `e2e::layout_150x40_has_no_side_preview` |
| 30 | layout: small -> hidden | `popup::preview_layout_by_size`, `popup::side_and_bottom_borders` |
| 31 | @home-keys '' binds nothing | `plugin::empty_home_keys_binds_nothing` |
| 32 | default binds prefix w (MVP suite: "default binds prefix .") | `plugin::default_binds_prefix_dot_to_the_rust_popup` (default key is `.`, R1 ruling 1) |
| 33 | default binds prefix f | `plugin::default_binds_prefix_dot_to_the_rust_popup` (`w`, `f` left alone), `plugin::custom_keys_bind_each` |
| 34 | client attached to test server | `common::Outer::attach` waits for it; asserted in `e2e::opens_grouped_with_header_and_side_preview` |
| 35 | popup opens via prefix . | every e2e test (`Outer::open_popup` presses `prefix .`) |
| 36 | header shows client location | `e2e::opens_grouped_with_header_and_side_preview`, `app::header_follows_client_and_current_is_marked` |
| 37 | list grouped: client session (alpha) first, then beta | `e2e::opens_grouped_with_header_and_side_preview`, `app::rows_grouped_client_session_first` |
| 38 | current window marked | same, `app::header_follows_client_and_current_is_marked` |
| 39 | preview shown on the right at 200 cols | `e2e::opens_grouped_with_header_and_side_preview`, `popup::side_and_bottom_borders` |
| 40 | typing filters (1 of 4) | `e2e::filter_esc_and_navigation`, `app::typing_filters_and_returns_to_top` |
| 41 | filter kept only beta build | same two |
| 42 | Esc clears a non-empty filter | `e2e::filter_esc_and_navigation`, `app::filter_and_move` |
| 43 | popup still open after clearing | `e2e::filter_esc_and_navigation` |
| 44 | navigation keys accepted (^n ^j ↓ ^p ^k ↑) | `e2e::filter_esc_and_navigation` (each key's cursor row asserted, and the wrap), `app::filter_and_move` |
| 45 | ^r opens editor pre-filled with current name | `e2e::rename_inline`, `app::rename_mode` |
| 46 | ^r ⏎ renames the target window by ID | same two |
| 47 | fzf still running after rename (list mode prompt) | `e2e::rename_inline` |
| 48 | list shows the new name | same |
| 49 | ^r pre-fills a name with ( ) + , | same |
| 50 | Esc in editor returns to list mode | `e2e::rename_inline`, `app::rename_mode` |
| 51 | Esc in editor leaves name unchanged | same two |
| 52 | editor reopened | `e2e::rename_inline` |
| 53 | empty name returns to list mode | `e2e::rename_inline`, `app::rename_mode` |
| 54 | empty name leaves name unchanged | same two |
| 55 | filter with ( ) + | `e2e::rename_inline`, `app::typing_filters_and_returns_to_top` |
| 56 | editor opens from that filter | `e2e::rename_inline` |
| 57 | filter with ( ) + restored after Esc | `e2e::rename_inline`, `app::rename_mode` |
| 58 | filter cleared again | `e2e::rename_inline` |
| 59 | filter before rename | same |
| 60 | editor on the filtered window | `e2e::rename_inline`, `app::rename_mode` |
| 61 | rename of filtered selection hits the right window | same two |
| 62 | filter restored after rename | same two |
| 63 | filter cleared | `e2e::rename_inline` |
| 64 | before M-r: automatic-rename off | `e2e::reset_preview_toggle_and_help` |
| 65 | M-r: automatic-rename back on (by ID) | `e2e::reset_preview_toggle_and_help`, `app::alt_r_resets_selected` |
| 66 | popup still open after M-r | same two |
| 67 | ^o hides the preview | `e2e::reset_preview_toggle_and_help`, `app::ctrl_o_toggles_preview_and_help_returns_on_any_key` |
| 68 | ^o shows it again | same two |
| 69 | F1 shows help | same two (F1 and `^/`) |
| 70 | help returns to the list | same two |
| 71 | popup closed before close tests | `e2e::enter_switches_and_esc_closes` (each e2e test opens its own popup on its own server) |
| 72 | popup opens on the client current window (qcur) | `e2e2::close_idle_current_and_cursor`, `e2e::cursor_opens_on_the_window_selected_just_before` |
| 73 | header shows qcur | `e2e2::close_idle_current_and_cursor` |
| 74 | ^x on the client's current (idle) window closes it at once | same, `app::close_verdicts` |
| 75 | ... qcur is gone | `e2e2::close_idle_current_and_cursor` |
| 76 | ... server still up, client still attached | same |
| 77 | ... client moved to another alpha window | same |
| 78 | ... header follows the client | same, `app::header_follows_client_and_current_is_marked` |
| 79 | ... popup still open | `e2e2::close_idle_current_and_cursor` |
| 80 | filter to qone | same |
| 81 | ^x closes an idle window at once, filter kept | same |
| 82 | ... qone is gone | same |
| 83 | filter cleared after close | same |
| 84 | ^x leaves the cursor on the next row (preview shows qnext) | same |
| 85 | ... qtwo closed | same |
| 86 | ... qtwo is gone | same |
| 87 | rename editor open on qnext | same |
| 88 | ^x ignored during rename: editor still open | same, `app::rename_mode` |
| 89 | ^x ignored during rename: window kept | `e2e2::close_idle_current_and_cursor` |
| 90 | back to the list | same |
| 91 | filter to qbusy | `e2e2::close_asks_when_busy` |
| 92 | ^x on a busy window asks, naming the command | same, `cr::close_plan_by_the_bash_rules`, `app::close_verdicts` |
| 93 | n cancels, filter restored | `e2e2::close_asks_when_busy`, `app::close_verdicts` |
| 94 | ... window kept after n | `e2e2::close_asks_when_busy` |
| 95 | confirm again | same |
| 96 | Esc cancels, filter restored | same, `app::close_verdicts` |
| 97 | ... window kept after Esc | `e2e2::close_asks_when_busy` |
| 98 | confirm again (⏎) | same |
| 99 | ⏎ cancels (default N) | same, `app::close_verdicts` |
| 100 | ... window kept after ⏎ | `e2e2::close_asks_when_busy` |
| 101 | ... popup still open | same |
| 102 | confirm again (y) | same |
| 103 | y closes it, filter restored | same, `app::close_verdicts` |
| 104 | ... qbusy is gone | `e2e2::close_asks_when_busy` |
| 105 | filter to qside | same |
| 106 | window whose only program is a sidebar closes at once | same, `cr::close_skips_sidebar_panes_in_the_shape` |
| 107 | filter to qnext | `e2e2::reopen_restores_and_selects` (setup closes) |
| 108 | qnext closed | same |
| 109 | back to the four fixture windows | same |
| 110 | ^t reopens the last closed window | same |
| 111 | ... qnext is back | same |
| 112 | ... client not switched | same |
| 113 | ... cursor is on it | same, `app::selection_survives_a_stale_snapshot`, `e2e2::degraded_mode_without_a_daemon` |
| 114 | back to list | `e2e2::reopen_restores_and_selects` |
| 115 | filter to build | same |
| 116 | ^t from a filter clears it and reopens qside | same |
| 117 | ... cursor is on qside | same |
| 118 | re-close qnext | same |
| 119 | re-close qside | same |
| 120 | back to the four fixture windows again | same |
| 121 | ^t with nothing closed: footer notice | same |
| 122 | ... no window created | same |
| 123 | ... notice goes once you type | same, `app::notice_clears_once_you_type` |
| 124 | list mode, 4/4 | `e2e2::reopen_restores_and_selects` |
| 125 | popup closed | `e2e::enter_switches_and_esc_closes` (each e2e test opens its own popup) |
| 126 | popup opens in the one-window session qdelta | `e2e2::close_last_window_of_client_session` |
| 127 | header shows qdelta | same |
| 128 | ^x on the session's last window warns the session will end | same, `cr::close_plan_by_the_bash_rules` |
| 129 | y closes it; list back to the fixture | `e2e2::close_last_window_of_client_session` |
| 130 | ... session qdelta is gone | same |
| 131 | ... server still up, client still attached | same |
| 132 | ... client moved to another session | same |
| 133 | ... header follows the client | same |
| 134 | ... popup still open | same (then `^t` recreates qdelta without switching) |
| 135 | filtered to beta build | `e2e::enter_switches_and_esc_closes` |
| 136 | popup closes on ⏎ | same, `app::filter_and_move` |
| 137 | ⏎ switched the invoking client to beta:build | `e2e::enter_switches_and_esc_closes` |
| 138 | popup reopens | same |
| 139 | header follows the client (beta ▸ 1) | same |
| 140 | client session (beta) listed first | same, `app::rows_grouped_client_session_first` |
| 141 | Esc on empty filter closes the popup | same, `app::filter_and_move` |
| 142 | popup opens at 150x40 | `e2e::layout_150x40_has_no_side_preview` |
| 143 | no side preview at 150 cols | same, `popup::side_and_bottom_borders` |

Covered: 143 of 143. Waived: none.

## Beyond the bash suite

| Behaviour | Test(s) |
| --- | --- |
| A job stopped with ^z counts as busy (`sleep (stopped)`) | `ops::busy_reports_a_stopped_job`, `e2e2::close_asks_when_a_job_is_stopped` |
| A corrupt `closed.json` is moved aside; ^x/^t keep working; unknown fields ignored | `store::corrupt_file_is_moved_aside`, `store::unknown_fields_are_ignored`, `e2e2::corrupt_store_does_not_break_close_and_reopen` |
| Reopen with a missing cwd still succeeds | `cr::reopen_with_missing_cwd_falls_back_to_home` |
| A failed rebuild after create keeps the entry popped | `ops::reopen_failing_after_create_keeps_the_stack_popped` |
| `#` in names and paths stays literal | `ops::rename_keeps_hash_literally`, `ops::new_window_lands_in_a_hash_cwd_with_a_hash_name`, `ops::reopen_restores_hash_name_session_and_cwds`, `e2e2::reorder_and_new_window` (`fresh #1`) |
| M-↑/M-↓ reorder (and stop at the session edge), M-n new window (Esc cancels) | `e2e2::reorder_and_new_window`, `ops::swap_exchanges_places_and_keeps_the_current_window`, `ops::new_window_after_lands_next_with_its_name_and_cwd`, `app::swap_stays_in_session`, `app::alt_n_names_a_new_window_after_the_selection` |
| Live redraw leaves an open editor alone; a vanished target closes it | `e2e2::live_redraw_keeps_the_editor`, `app::rename_mode` |
| Daemon killed under an open popup: direct reads, own writes show at once | `e2e2::daemon_death_falls_back_to_polling` |
| No daemon at all | `e2e2::degraded_mode_without_a_daemon` |
| A snapshot read before the popup's own write does not move the cursor | `app::selection_survives_a_stale_snapshot`, `e2e2::degraded_mode_without_a_daemon` |
| Tiny terminals | `popup::tiny_terminals_do_not_panic` (1x1 to 20x3, every mode), `popup::page_keys_move_at_least_one_row`, `e2e::tiny_terminal_keeps_running` (24x4 popup) |
| Missing binary: one-line message, no bash fallback | `plugin::missing_binary_shows_a_message` |
| Plugin starts the daemon | `plugin::starts_the_daemon_when_the_binary_exists` |
