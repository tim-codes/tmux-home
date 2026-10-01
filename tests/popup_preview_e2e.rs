//! The preview in the real popup: colours from `capture-pane -e`, and the
//! mouse wheel scrolling it (never the list), through a client on a
//! throwaway server; plus the cost of a 2000-line capture.
mod common;
use common::*;
use std::time::Instant;
use tmux_home::{ops::Tx, popup::preview};

/// `n` numbered lines, `ROW<i>` in red, printed in `target`.
fn print_rows(s: &TestServer, target: &str, n: usize) {
    s.tmux(&[
        "send-keys",
        "-t",
        target,
        &format!(
            "clear; i=1; while [ $i -le {n} ]; do printf '\\033[31mROW%04d\\033[0m plain\\n' $i; i=$((i+1)); done"
        ),
        "Enter",
    ]);
}

/// The cursor row's list part (the side preview shares the screen row).
fn cursor_row(o: &Outer) -> Option<String> {
    o.cursor_row()
        .map(|l| l.split('│').next().unwrap_or_default().to_string())
}

/// The outer screen with escape sequences (`capture-pane -e`).
fn styled_screen(o: &Outer) -> String {
    o.tmux(&["capture-pane", "-p", "-e", "-t", "outer"])
}

#[test]
fn the_preview_is_coloured_and_the_wheel_scrolls_only_it() {
    let (_env, s, o) = popup_fixture();
    // the popup asks for mouse reporting and tmux passes the wheel on to
    // it (also with `mouse off`; `on` is the usual config)
    s.tmux(&["set-option", "-g", "mouse", "on"]);
    print_rows(&s, "alpha:editor", 300);
    wait_until("the rows printed", || {
        s.tmux(&["capture-pane", "-p", "-t", "alpha:editor"])
            .contains("ROW0300 plain")
    });
    o.open_popup();
    o.wait_cursor_on("editor");
    o.wait_for(r"│ROW0300 plain");
    // the preview's cells carry the pane's red: the outer capture has the
    // SGR right before the text
    let red = regex::Regex::new(r"\x1b\[31m(\x1b\[[0-9;]*m)*ROW0300").unwrap();
    let screen = styled_screen(&o);
    assert!(red.is_match(&screen), "no red ROW0300 in\n{screen:?}");
    let plain = regex::Regex::new(r"ROW0300(\x1b\[[0-9;]*m)+ plain").unwrap();
    assert!(
        plain.is_match(&screen),
        "colour reset before ' plain':\n{screen:?}"
    );

    // a wheel-up notch over the list (raw SGR mouse: button 64, 1-based
    // column/row), then one over the preview
    let before = cursor_row(&o);
    o.typed("\x1b[<64;20;10M");
    o.wait_for(r"│ROW0297 plain");
    o.wait_for(r"↑3$");
    o.typed("\x1b[<64;150;20M");
    o.wait_for(r"│ROW0294 plain");
    o.wait_for(r"↑6$");
    assert!(!o.screen().contains("ROW0300"), "{}", o.screen());
    assert_eq!(cursor_row(&o), before, "the list didn't move");
    // wheel down: back towards the end
    o.typed("\x1b[<65;150;20M");
    o.wait_for(r"│ROW0297 plain");
    assert_eq!(cursor_row(&o), before);
    // Shift-↑ a line; then a selection change returns to the latest
    o.keys(&["S-Up"]);
    o.wait_for(r"│ROW0296 plain");
    o.wait_for(r"↑4$");
    o.keys(&["Down"]);
    o.wait_cursor_on("win two");
    o.keys(&["Up"]);
    o.wait_cursor_on("editor");
    o.wait_for(r"│ROW0300 plain");
    o.wait_gone(r"↑\d+$");
    // a click on a row selects it
    o.typed("\x1b[<0;20;4M\x1b[<0;20;4m");
    o.wait_cursor_on("win two");
    assert!(o.popup_open());
}

/// Capturing and parsing 2000 lines of coloured history stays cheap (it
/// runs once a second for the selected pane); the measurement is printed.
#[test]
fn a_2000_line_capture_is_cheap() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "history-limit", "5000"]);
    print_rows(&s, "alpha", 2500);
    wait_until("the rows printed", || {
        s.tmux(&["capture-pane", "-p", "-t", "alpha"])
            .contains("ROW2500 plain")
    });
    let pane = fmt(&s, "alpha", "#{pane_id}");
    let tx = Tx::new(s.socket.clone());
    let mut best = u128::MAX;
    let mut lines = 0;
    for _ in 0..5 {
        let t = Instant::now();
        let text = tx.capture_styled(&pane, 2000);
        let cap = t.elapsed();
        let parsed = preview::parse(&text);
        let all = t.elapsed();
        eprintln!(
            "capture {} lines: {:?}, + parse: {:?}",
            parsed.len(),
            cap,
            all
        );
        best = best.min(all.as_micros());
        lines = parsed.len();
    }
    assert!(lines >= 2000, "{lines}");
    assert!(best < 200_000, "2000-line capture+parse took {best}µs");
}
