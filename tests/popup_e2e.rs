//! The real popup, opened with tmux-home's binding through a client on a
//! throwaway server and read back with capture-pane: list, filter, rename,
//! auto-name, preview, help, switching and layout (bash suite port, part 1).
mod common;
use common::*;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};
use tmux_home::paths::Paths;

#[test]
fn opens_grouped_with_header_and_side_preview() {
    let (_env, s, o) = popup_fixture();
    assert!(
        !s.tmux(&["list-clients", "-F", "#{client_name}"])
            .trim()
            .is_empty()
    );
    o.open_popup();
    o.wait_for("tmux-home +alpha ▸ 0");
    let screen = o.screen();
    assert_eq!(row_sessions(&screen), ["alpha", "alpha", "beta", "beta"]);
    assert!(
        regex::Regex::new(r"(?m)^▌alpha ▶ +0  editor")
            .unwrap()
            .is_match(&screen),
        "current window marked and selected:\n{screen}"
    );
    assert!(screen.contains('│'), "side preview at 200 cols:\n{screen}");
    // beta:logs previews its main pane, never its (active) sidebar
    o.keys(&["Down", "Down"]);
    o.wait_cursor_on("logs");
    o.wait_for("MAIN-PANE-MARKER");
    assert!(!o.screen().contains("SIDEBAR-MARKER"), "{}", o.screen());
}

#[test]
fn filter_esc_and_navigation() {
    let (_env, _s, o) = popup_fixture();
    o.open_popup();
    o.typed("bui");
    o.wait_for(r"^> bui .*1/4");
    assert_eq!(row_sessions(&o.screen()), ["beta"], "only beta build left");
    o.wait_cursor_on("build");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
    assert!(o.popup_open(), "Esc on a filter only clears it");
    o.wait_cursor_on("editor");
    for (k, name) in [
        ("C-n", "win two"),
        ("C-j", "logs"),
        ("Down", "build"),
        ("C-p", "logs"),
        ("C-k", "win two"),
        ("Up", "editor"),
        ("Up", "build"), // cycles
        ("Down", "editor"),
    ] {
        o.keys(&[k]);
        o.wait_cursor_on(name);
    }
}

#[test]
fn rename_inline() {
    let (_env, s, o) = popup_fixture();
    let target = wid(&s, "alpha:editor");
    o.open_popup();
    o.keys(&["C-r"]);
    o.wait_for("^rename › editor");
    o.keys(&["C-u"]);
    o.typed("renamed (x)+y, z");
    o.keys(&["Enter"]);
    wait_until("renamed by ID", || {
        fmt(&s, &target, "#{window_name}") == "renamed (x)+y, z"
    });
    o.wait_for(r"^> +.*4/4");
    o.wait_for(r"▶ +0  renamed \(x\)\+y, z");
    // the pre-fill round-trips ( ) + ,
    o.keys(&["C-r"]);
    o.wait_for(r"^rename › renamed \(x\)\+y, z");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
    assert_eq!(fmt(&s, &target, "#{window_name}"), "renamed (x)+y, z");
    // an empty name cancels
    o.keys(&["C-r"]);
    o.wait_for("^rename › renamed");
    o.keys(&["C-u"]);
    o.keys(&["Enter"]);
    o.wait_for(r"^> +.*4/4");
    assert_eq!(fmt(&s, &target, "#{window_name}"), "renamed (x)+y, z");
    // a filter with ( ) + survives an editor round-trip
    o.typed("(x)+");
    o.wait_for(r"^> \(x\)\+ .*1/4");
    o.keys(&["C-r"]);
    o.wait_for("^rename › renamed");
    o.keys(&["Escape"]);
    o.wait_for(r"^> \(x\)\+ .*1/4");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
    // renaming a filtered selection hits that window; the filter stays
    o.typed("win");
    o.wait_for(r"^> win .*1/4");
    o.keys(&["C-r"]);
    o.wait_for("^rename › win two");
    o.keys(&["BSpace", "BSpace", "BSpace"]);
    o.typed("2");
    o.keys(&["Enter"]);
    wait_until("win 2", || fmt(&s, "alpha:1", "#{window_name}") == "win 2");
    o.wait_for(r"^> win .*1/4");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
}

#[test]
fn reset_preview_toggle_and_help() {
    let (_env, s, o) = popup_fixture();
    let target = wid(&s, "alpha:editor");
    o.open_popup();
    assert_eq!(fmt(&s, &target, "#{?automatic-rename,on,off}"), "off");
    o.keys(&["M-r"]);
    wait_until("automatic-rename on", || {
        fmt(&s, &target, "#{?automatic-rename,on,off}") == "on"
    });
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
        s.tmux(&["list-clients", "-F", "#{session_id} #{window_id}"])
            .trim()
            == want
    });
    o.open_popup();
    o.wait_for("tmux-home +beta ▸ 1");
    assert_eq!(
        row_sessions(&o.screen()),
        ["beta", "beta", "alpha", "alpha"]
    );
    o.wait_cursor_on("build");
    o.keys(&["Escape"]);
    o.wait_gone("F1 help");
}

#[test]
fn layout_150x40_has_no_side_preview() {
    let (_env, _s, o) = popup_fixture_sized(150, 40);
    o.open_popup();
    let screen = o.screen();
    assert!(!screen.contains('│'), "{screen}");
    assert!(
        screen
            .lines()
            .any(|l| l.chars().count() >= 150 && l.chars().all(|c| c == '─')),
        "bottom preview border:\n{screen}"
    );
}

/// A tiny terminal still draws the popup, and every mode's keys work
/// without the popup dying.
#[test]
fn tiny_terminal_keeps_running() {
    let (_env, s, o) = popup_fixture_sized(24, 4);
    s.tmux(&["set-option", "-g", "status", "off"]);
    o.open_popup();
    for keys in [
        &["Down", "PPage", "NPage", "C-o"][..],
        &["C-r", "x"],
        &["Escape"],
        &["M-n"],
        &["Escape"],
        &["F1"],
        &["q"],
    ] {
        o.keys(keys);
        assert!(o.screen().contains("tmux-home"), "{}", o.screen());
    }
    o.wait_for(r"^> ");
    o.keys(&["Escape"]);
    o.wait_gone("tmux-home");
}

/// The cursor opens on the client's current window even when the daemon's
/// cached snapshot predates a `select-window` made just before the popup.
#[test]
fn cursor_opens_on_the_window_selected_just_before() {
    let (_env, s, o) = popup_fixture();
    let paths = Paths::for_socket(&s.socket).unwrap();
    // ride the poll: once the daemon has seen this rename, its next read is
    // ~500 ms away, so its cache misses the select-window below
    s.tmux(&["rename-window", "-t", "=alpha:win two", "tick"]);
    wait_until("daemon saw the rename", || {
        let Ok(mut c) = UnixStream::connect(&paths.sock) else {
            return false;
        };
        let q = format!("{{\"op\":\"query\",\"v\":\"{}\"}}\n", tmux_home::BUILD_ID);
        let mut line = String::new();
        c.write_all(q.as_bytes()).is_ok()
            && BufReader::new(c).read_line(&mut line).is_ok()
            && line.contains("\"tick\"")
    });
    s.tmux(&["select-window", "-t", "=alpha:tick"]);
    o.open_popup();
    o.wait_cursor_on("tick");
    std::thread::sleep(Duration::from_millis(100));
    o.keys(&["Escape"]);
    o.wait_gone("F1 help");
}
