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

/// A live agent pane in `beta:<window>` (`fake_claude`) with these
/// `@pane_*` options, faked the way tmux-agent-sidebar's hooks write them.
fn fake_agent(s: &TestServer, window: &str, opts: &[(&str, &str)]) {
    // its first pane: the agent, whichever pane is active
    let target = format!("beta:{window}.0");
    if s.tmux(&["list-windows", "-t", "beta", "-F", "#W"])
        .lines()
        .all(|w| w != window)
    {
        s.tmux(&[
            "new-window",
            "-d",
            "-t",
            "beta:",
            "-n",
            window,
            &fake_claude(),
        ]);
    }
    for (k, v) in opts {
        s.tmux(&["set-option", "-p", "-t", &target, k, v]);
    }
}

const WAITING: &[(&str, &str)] = &[
    ("@pane_agent", "claude"),
    ("@pane_status", "waiting"),
    ("@pane_attention", "notification"),
    ("@pane_wait_reason", "permission_prompt"),
    ("@pane_prompt", "tidy the changelog"),
];

/// Lines between the NEEDS YOU header and the next header.
fn needs_you_rows(screen: &str) -> Vec<String> {
    screen
        .lines()
        .skip_while(|l| !l.contains("─ NEEDS YOU ─"))
        .skip(1)
        .take_while(|l| !l.contains("─ sessions ─"))
        .map(str::to_string)
        .collect()
}

/// An agent starting to wait while the popup is open is pinned in NEEDS
/// YOU (the poll picks the option up), counted in the header, and ^g
/// jumps to it; ⏎ there switches to its window.
#[test]
fn waiting_agent_is_pinned_and_ctrl_g_jumps_to_it() {
    let (_env, s, o) = popup_fixture();
    fake_agent(&s, "agent", &[]);
    // a second pane, active: ⏎ must still focus the agent's
    let agent_pane = fmt(&s, "beta:agent.0", "#{pane_id}");
    s.tmux(&["split-window", "-t", "beta:agent"]);
    assert_ne!(fmt(&s, "beta:agent", "#{pane_id}"), agent_pane);
    o.open_popup();
    o.wait_for(r"^> .*5/5");
    assert!(!o.screen().contains("NEEDS YOU"), "{}", o.screen());
    fake_agent(&s, "agent", WAITING);
    o.wait_for("─ NEEDS YOU ─");
    let screen = o.screen();
    let pinned = needs_you_rows(&screen);
    assert_eq!(pinned.len(), 1, "{screen}");
    assert!(
        pinned[0].contains("agent") && pinned[0].contains("◐ waiting"),
        "{screen}"
    );
    assert!(pinned[0].contains("permission"), "{screen}");
    assert!(screen.contains("agents: 1 waiting"), "{screen}");
    // the agent card previews it, with the prompt
    o.wait_cursor_on("editor");
    o.keys(&["C-g"]);
    o.wait_cursor_on("agent");
    let screen = o.screen();
    assert_eq!(
        needs_you_rows(&screen).first().map(|l| l.starts_with('▌')),
        Some(true),
        "^g lands on the pinned row:\n{screen}"
    );
    o.wait_for("needs you  permission");
    o.wait_for("prompt     tidy the changelog");
    o.keys(&["Enter"]);
    o.wait_gone("F1 help");
    wait_until("client on beta:agent", || client_at(&s) == "beta:agent");
    wait_until("the agent's pane focused", || {
        fmt(&s, "beta:agent", "#{pane_id}") == agent_pane
    });
}

/// ^x from a pinned copy asks with the agent wording, and once closed the
/// cursor lands on the next window (both copies are gone).
#[test]
fn close_from_a_pinned_copy() {
    let (_env, s, o) = popup_fixture();
    fake_agent(&s, "agent", WAITING);
    let target = wid(&s, "beta:agent");
    o.open_popup();
    o.wait_for("─ NEEDS YOU ─");
    o.keys(&["C-g"]);
    o.wait_cursor_on("agent");
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "agent"\? agent still working — running: claude \(y/N\)"#);
    o.keys(&["y"]);
    wait_until("window closed", || !window_ids(&s).contains(&target));
    o.wait_for(r"^> .*4/4");
    o.wait_gone("NEEDS YOU");
    o.wait_cursor_on("editor");
}

/// The agent is answered while its pinned copy is selected: the group
/// goes and the cursor stays on that window, in its session.
#[test]
fn answered_while_its_pinned_copy_is_selected() {
    let (_env, s, o) = popup_fixture();
    fake_agent(&s, "agent", WAITING);
    o.open_popup();
    o.wait_for("─ NEEDS YOU ─");
    o.keys(&["C-g"]);
    o.wait_cursor_on("agent");
    fake_agent(
        &s,
        "agent",
        &[("@pane_status", "running"), ("@pane_attention", "clear")],
    );
    o.wait_gone("NEEDS YOU");
    o.wait_cursor_on("agent");
    let row = o.cursor_row().unwrap();
    assert!(row.contains("● running"), "{row}");
    assert!(row.starts_with("▌beta"), "in its session: {row}");
}

/// `@waiting` keeps only the waiting agent's window; an agent whose pane
/// is back at its shell is shown dimmed with "(ended?)", and is neither
/// pinned nor counted.
#[test]
fn waiting_filter_and_stale_agent() {
    let (_env, s, o) = popup_fixture();
    fake_agent(&s, "agent", WAITING);
    for (k, v) in [("@pane_agent", "claude"), ("@pane_status", "waiting")] {
        s.tmux(&["set-option", "-p", "-t", "alpha:win two", k, v]);
    }
    o.open_popup();
    o.wait_for("─ NEEDS YOU ─");
    o.wait_for(r"win two .*\(ended\?\)");
    let screen = o.screen();
    assert_eq!(needs_you_rows(&screen).len(), 1, "{screen}");
    assert!(
        regex::Regex::new(r"(?m)agents: 1 waiting( |$)")
            .unwrap()
            .is_match(&screen),
        "the stale agent isn't counted:\n{screen}"
    );
    // dimmed: SGR 2 on the stale cell
    let raw = o.tmux(&["capture-pane", "-p", "-e", "-t", "outer"]);
    let stale = raw.lines().find(|l| l.contains("(ended?)")).unwrap();
    assert!(
        regex::Regex::new(r"\x1b\[(?:[0-9;]*;)?2(?:;[0-9;]*)?m[^\x1b]*◐ waiting \(ended\?\)")
            .unwrap()
            .is_match(stale),
        "{stale:?}"
    );
    o.typed("@waiting");
    o.wait_for(r"^> @waiting .*1/5");
    let screen = o.screen();
    assert!(!screen.contains("win two"), "{screen}");
    assert_eq!(
        screen.lines().filter(|l| l.contains("  agent ")).count(),
        2,
        "pinned and in its session:\n{screen}"
    );
    o.typed(" s:alpha");
    o.wait_for(r"^> @waiting s:alpha .*0/5");
}
