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

/// Runs the popup in a new window of `s` under `script`, which records
/// everything it writes to its terminal into the returned file, with
/// `TMUX_HOME_TEST_PANIC=<at>`. Returns the file and the window.
fn recorded_popup(s: &TestServer, at: &str) -> (std::path::PathBuf, String) {
    recorded_popup_for(s, at, "")
}

/// `recorded_popup` for `client` (`TMUX_HOME_CLIENT`; "" = none).
fn recorded_popup_for(s: &TestServer, at: &str, client: &str) -> (std::path::PathBuf, String) {
    let out = std::env::temp_dir().join(format!("th-tty-{}-{}", std::process::id(), rand_suffix()));
    let bin = env!("CARGO_BIN_EXE_tmux-home");
    let sock = s.socket.display().to_string();
    let popup = format!("'{bin}' popup --socket '{sock}'");
    let script = if cfg!(target_os = "macos") {
        format!("script -q '{}' {popup}", out.display())
    } else {
        format!("script -q -c \"{popup}\" '{}'", out.display())
    };
    s.tmux(&["set-option", "-g", "remain-on-exit", "on"]);
    let w = s.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id}",
        "-t",
        "alpha:",
        "-e",
        &format!("TMUX_HOME_TEST_PANIC={at}"),
        "-e",
        &format!("TMUX_HOME_CLIENT={client}"),
        &script,
    ]);
    (out, w.trim().to_string())
}

/// Mouse reporting was turned on, then every mode crossterm enabled
/// (1000, 1002, 1003, 1015, 1006) off again before the alternate screen
/// was left.
fn assert_mouse_off_before_leaving(tty: &str) {
    let on = tty
        .find("\x1b[?1000h")
        .unwrap_or_else(|| panic!("mouse never on: {tty:?}"));
    let leave = tty[on..]
        .find("\x1b[?1049l")
        .map(|i| i + on)
        .unwrap_or_else(|| panic!("never left the alt screen: {tty:?}"));
    for m in ["1000", "1002", "1003", "1015", "1006"] {
        assert!(
            tty[on..leave].contains(&format!("\x1b[?{m}l")),
            "?{m}l not before ?1049l: {tty:?}"
        );
    }
}

fn pane_dead(s: &TestServer, w: &str) -> bool {
    fmt(s, w, "#{pane_dead}") == "1"
}

/// A panic in the event loop turns mouse reporting off first (then
/// ratatui leaves the alternate screen and the panic is printed): the
/// shell must not be left receiving mouse sequences.
#[test]
fn a_panic_turns_the_mouse_off() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let (out, w) = recorded_popup(&s, "loop");
    wait_until("the popup to panic and exit", || pane_dead(&s, &w));
    let tty = String::from_utf8_lossy(&std::fs::read(&out).unwrap()).into_owned();
    let _ = std::fs::remove_file(&out);
    assert_mouse_off_before_leaving(&tty);
    assert!(
        tty.contains("TMUX_HOME_TEST_PANIC=loop"),
        "the panic is still reported: {tty:?}"
    );
}

/// A preview parse that panics falls back to plain text without printing
/// anything over the TUI; the popup keeps running.
#[test]
fn a_parse_panic_is_silent() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["send-keys", "-t", "alpha:0", "echo PREVIEW-TEXT", "Enter"]);
    let (out, w) = recorded_popup(&s, "parse");
    // a few one-second recaptures, each one panicking
    let read = || String::from_utf8_lossy(&std::fs::read(&out).unwrap_or_default()).into_owned();
    // (script writes its file as it exits: the screen is read from tmux)
    wait_until("the preview's plain fallback", || {
        s.tmux(&["capture-pane", "-p", "-t", &w])
            .contains("│PREVIEW-TEXT")
    });
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(!pane_dead(&s, &w), "the popup kept running");
    s.tmux(&["send-keys", "-t", &w, "Escape"]);
    wait_until("the popup to exit", || pane_dead(&s, &w));
    let tty = read();
    let _ = std::fs::remove_file(&out);
    assert!(!tty.contains("panicked"), "{tty:?}");
    assert!(!tty.contains("TMUX_HOME_TEST_PANIC"), "{tty:?}");
    let on = tty.find("\x1b[?1000h").expect("mouse on");
    assert!(
        tty[on..].contains("\x1b[?1000l"),
        "mouse off on Esc: {tty:?}"
    );
}

/// ⏎ (switch) turns mouse reporting off before leaving the alternate
/// screen (and before tmux switches), and the switch happens.
#[test]
fn switch_turns_the_mouse_off_first() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:5", "-n", "target"]);
    let o = Outer::attach(&s, "alpha:0", 200, 50);
    let client = s.tmux(&["list-clients", "-F", "#{client_name}"]);
    let (out, w) = recorded_popup_for(&s, "none", client.trim());
    let screen = || s.tmux(&["capture-pane", "-p", "-t", &w]);
    wait_until("the popup", || screen().contains("F1 help"));
    s.tmux(&["send-keys", "-t", &w, "-l", "target"]);
    wait_until("the target selected", || {
        screen()
            .lines()
            .any(|l| l.starts_with('▌') && l.contains("target"))
    });
    s.tmux(&["send-keys", "-t", &w, "Enter"]);
    wait_until("the popup to exit", || pane_dead(&s, &w));
    let tty = String::from_utf8_lossy(&std::fs::read(&out).unwrap()).into_owned();
    let _ = std::fs::remove_file(&out);
    assert_mouse_off_before_leaving(&tty);
    // the switch: the client shows the target now
    wait_until("the client on target", || client_at(&s) == "alpha:target");
    drop(o);
}
