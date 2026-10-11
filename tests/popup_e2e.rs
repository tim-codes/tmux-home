//! The real popup, opened with tmux-home's binding through a client on a
//! throwaway server and read back with capture-pane: list, filter, rename,
//! auto-name, preview, help, switching, session pages and pane rows
//! (bash suite port, part 1).
mod common;
use common::*;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};
use tmux_home::paths::Paths;

/// A full-width rule with the layout's frame right under it: the
/// preview, below the list.
fn bottom_preview(screen: &str, cols: usize) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    lines.windows(2).any(|w| {
        w[0].chars().count() == cols && w[0].chars().all(|c| c == '─') && w[1].starts_with('┌')
    })
}

#[test]
fn opens_on_the_client_session_with_tabs_and_bottom_preview() {
    let (_env, s, o) = popup_fixture();
    assert!(
        !s.tmux(&["list-clients", "-F", "#{client_name}"])
            .trim()
            .is_empty()
    );
    o.open_popup();
    o.wait_for(r"^  ▶alpha   beta  ");
    o.wait_for(r"^ +# +NM ");
    let screen = o.screen();
    assert_eq!(row_windows(&screen), ["editor", "win two"], "{screen}");
    assert!(
        regex::Regex::new(r"(?m)^▌▶ 0 +editor")
            .unwrap()
            .is_match(&screen),
        "current window marked and selected:\n{screen}"
    );
    assert!(
        regex::Regex::new(r"(?m)^   └ \.0 ")
            .unwrap()
            .is_match(&screen),
        "a pane row under each window:\n{screen}"
    );
    assert!(bottom_preview(&screen, 200), "{screen}");
    // beta:logs previews its main pane, never its (active) sidebar
    o.keys(&["C-b", ")"]);
    o.wait_cursor_on("logs");
    o.wait_for(r"^> .*2/2");
    assert_eq!(row_windows(&o.screen()), ["logs", "build"]);
    o.wait_for("MAIN-PANE-MARKER");
    assert!(!o.screen().contains("SIDEBAR-MARKER"), "{}", o.screen());
}

#[test]
fn filter_esc_and_navigation() {
    let (_env, _s, o) = popup_fixture();
    o.open_popup();
    o.typed("win");
    o.wait_for(r"^> win .*1/2");
    assert_eq!(row_windows(&o.screen()), ["win two"]);
    o.wait_cursor_on("win two");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
    assert!(o.popup_open(), "Esc on a filter only clears it");
    o.wait_cursor_on("editor");
    // windows and their pane rows, wrapping within the page
    for (k, name) in [
        ("C-n", None),
        ("C-j", Some("win two")),
        ("Down", None),
        ("Down", Some("editor")), // cycles, never to beta
        ("C-p", None),
        ("C-k", Some("win two")),
        ("Up", None),
        ("Up", Some("editor")),
    ] {
        o.keys(&[k]);
        match name {
            Some(n) => o.wait_cursor_on(n),
            None => o.wait_cursor_on_pane(""),
        }
    }
}

/// `prefix )` / `prefix (` page through the sessions, keeping the filter;
/// a bare paren is just text.
#[test]
fn prefix_parens_page_the_sessions() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["new-session", "-d", "-s", "gamma", "-n", "gwin"]);
    o.open_popup();
    o.wait_for(r"^  ▶alpha   beta   gamma ");
    o.keys(&["C-b", ")"]);
    o.wait_cursor_on("logs");
    o.keys(&["C-b", ")"]);
    o.wait_cursor_on("gwin");
    o.wait_for(r"^> .*1/1");
    o.keys(&["C-b", ")"]);
    o.wait_cursor_on("editor");
    o.keys(&["C-b", "("]);
    o.wait_cursor_on("gwin");
    o.keys(&["C-b", "("]);
    o.wait_cursor_on("logs");
    // the filter stays, applied to each page
    o.typed("ui");
    o.wait_for(r"^> ui .*1/2");
    o.wait_cursor_on("build");
    o.keys(&["C-b", "("]);
    o.wait_for(r"^> ui .*0/2");
    o.keys(&["C-b", ")"]);
    o.wait_for(r"^> ui .*1/2");
    o.wait_cursor_on("build");
    o.typed(")");
    o.wait_for(r"^> ui\) .*0/2");
    assert!(o.popup_open());
}

/// ⏎ on a pane row switches to its window and selects that pane.
#[test]
fn enter_on_a_pane_row_selects_that_pane() {
    let (_env, s, o) = popup_fixture();
    let first = fmt(&s, "alpha:editor", "#{pane_id}");
    let second = s
        .tmux(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "alpha:editor",
        ])
        .trim()
        .to_string();
    s.tmux(&["select-window", "-t", "alpha:win two"]);
    o.open_popup();
    o.wait_cursor_on("win two");
    o.wait_for(r"^   ├ \.0 ");
    o.keys(&["Up"]);
    o.wait_cursor_on_pane("");
    o.wait_for(r"^▌  └ \.1 ");
    o.keys(&["Enter"]);
    o.wait_gone("F1 help");
    wait_until("client on alpha:editor", || client_at(&s) == "alpha:editor");
    wait_until("the second pane focused", || {
        fmt(&s, "alpha:editor", "#{pane_id}") == second
    });
    assert_ne!(first, second);
}

#[test]
fn the_open_key_toggles_the_popup() {
    let (_env, _s, o) = popup_fixture();
    o.open_popup();
    // inside the popup tmux passes the prefix on: `C-b .` reaches it as two
    // keys, and closes it rather than typing '.' into the filter
    o.typed("bu");
    o.wait_for(r"^> bu .*/2");
    o.keys(&["C-b", "."]);
    o.wait_gone(r"^> bu ");
    assert!(!o.popup_open(), "{}", o.screen());
    // opens again on the same key
    o.open_popup();
    // the prefix then another key acts as before: C-a moves to the start
    o.typed("x");
    o.keys(&["C-b", "C-a"]);
    o.typed(".");
    o.wait_for(r"^> \.x ");
    assert!(o.popup_open());
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
    o.wait_for(r"^> +.*2/2");
    o.wait_for(r"▶ 0 +renamed \(x\)\+y, z");
    // the pre-fill round-trips ( ) + ,
    o.keys(&["C-r"]);
    o.wait_for(r"^rename › renamed \(x\)\+y, z");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
    assert_eq!(fmt(&s, &target, "#{window_name}"), "renamed (x)+y, z");
    // an empty name cancels
    o.keys(&["C-r"]);
    o.wait_for("^rename › renamed");
    o.keys(&["C-u"]);
    o.keys(&["Enter"]);
    o.wait_for(r"^> +.*2/2");
    assert_eq!(fmt(&s, &target, "#{window_name}"), "renamed (x)+y, z");
    // a filter with ( ) + survives an editor round-trip
    o.typed("(x)+");
    o.wait_for(r"^> \(x\)\+ .*1/2");
    o.keys(&["C-r"]);
    o.wait_for("^rename › renamed");
    o.keys(&["Escape"]);
    o.wait_for(r"^> \(x\)\+ .*1/2");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
    // renaming a filtered selection hits that window; the filter stays
    o.typed("win");
    o.wait_for(r"^> win .*1/2");
    o.keys(&["C-r"]);
    o.wait_for("^rename › win two");
    o.keys(&["BSpace", "BSpace", "BSpace"]);
    o.typed("2");
    o.keys(&["Enter"]);
    wait_until("win 2", || fmt(&s, "alpha:1", "#{window_name}") == "win 2");
    o.wait_for(r"^> win .*1/2");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
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
    o.wait_gone("┌");
    o.keys(&["C-o"]);
    o.wait_for("┌");
    for k in ["F1", "C-/"] {
        o.keys(&[k]);
        o.wait_for("press any key to return");
        o.keys(&["q"]);
        o.wait_for(r"^> +.*2/2");
    }
}

#[test]
fn enter_switches_and_esc_closes() {
    let (_env, s, o) = popup_fixture();
    o.open_popup();
    o.keys(&["C-b", ")"]);
    o.typed("build");
    o.wait_for(r"^> build .*1/2");
    o.keys(&["Enter"]);
    o.wait_gone("F1 help");
    let want = fmt(&s, "beta:build", "#{session_id} #{window_id}");
    wait_until("client on beta:build", || {
        s.tmux(&["list-clients", "-F", "#{session_id} #{window_id}"])
            .trim()
            == want
    });
    o.open_popup();
    o.wait_for(r"^  alpha   ▶beta ");
    assert_eq!(row_windows(&o.screen()), ["logs", "build"]);
    o.wait_cursor_on("build");
    o.keys(&["Escape"]);
    o.wait_gone("F1 help");
}

#[test]
fn the_preview_is_at_the_bottom_when_narrow_too() {
    let (_env, _s, o) = popup_fixture_sized(150, 40);
    o.open_popup();
    o.wait_for("┌");
    let screen = o.screen();
    assert!(bottom_preview(&screen, 150), "{screen}");
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
        let screen = o.screen();
        assert!(
            screen.contains("F1 help") || screen.contains("tmux-home"),
            "{screen}"
        );
    }
    o.wait_for(r"^> ");
    o.keys(&["Escape"]);
    o.wait_gone("F1 help");
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

/// The window rows between the NEEDS YOU header and the next header.
fn needs_you_rows(screen: &str) -> Vec<String> {
    screen
        .lines()
        .skip_while(|l| !l.contains("─ NEEDS YOU ─"))
        .skip(1)
        .take_while(|l| !l.starts_with(" ─"))
        .filter(|l| !l.contains(" ├ .") && !l.contains(" └ ."))
        .map(str::to_string)
        .collect()
}

/// An agent starting to wait while the popup is open is counted on its
/// session's tab (the poll picks the option up), and ^g pages there and
/// jumps to it, pinned in NEEDS YOU; ⏎ there switches to its window.
#[test]
fn waiting_agent_is_pinned_and_ctrl_g_jumps_to_it() {
    let (_env, s, o) = popup_fixture();
    fake_agent(&s, "agent", &[]);
    // a second pane, active: ⏎ must still focus the agent's
    let agent_pane = fmt(&s, "beta:agent.0", "#{pane_id}");
    s.tmux(&["split-window", "-t", "beta:agent"]);
    assert_ne!(fmt(&s, "beta:agent", "#{pane_id}"), agent_pane);
    o.open_popup();
    o.wait_for(r"^> .*2/2");
    assert!(!o.screen().contains('●'), "{}", o.screen());
    fake_agent(&s, "agent", WAITING);
    o.wait_for(r"^  ▶alpha   beta ●1 ");
    assert!(
        !o.screen().contains("NEEDS YOU"),
        "another page's: {}",
        o.screen()
    );
    // ^g pages to beta, onto the pinned copy; the preview's pane band
    // shows the agent's state
    o.wait_cursor_on("editor");
    o.keys(&["C-g"]);
    o.wait_cursor_on("agent");
    o.wait_for("─ NEEDS YOU ─");
    let screen = o.screen();
    let pinned = needs_you_rows(&screen);
    assert_eq!(pinned.len(), 1, "{screen}");
    assert!(
        pinned[0].contains("agent") && pinned[0].contains("◐ waiting"),
        "{screen}"
    );
    assert!(
        screen
            .lines()
            .any(|l| l.contains(" .0") && l.contains("permission")),
        "the reason on its pane row:\n{screen}"
    );
    assert!(
        pinned[0].starts_with('▌'),
        "^g lands on the pinned row:\n{screen}"
    );
    o.wait_for(r"│\.0 \S+ +◐ waiting");
    assert!(
        !o.screen().contains("prompt     tidy"),
        "no agent card:\n{}",
        o.screen()
    );
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
    o.wait_for("beta ●1");
    o.keys(&["C-g"]);
    o.wait_cursor_on("agent");
    o.wait_for("─ NEEDS YOU ─");
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "agent"\? agent still working — running: claude \(y/N\)"#);
    o.keys(&["y"]);
    wait_until("window closed", || !window_ids(&s).contains(&target));
    o.wait_for(r"^> .*2/2");
    o.wait_gone("NEEDS YOU");
    o.wait_gone("●");
    o.wait_cursor_on("logs");
}

/// The agent is answered while its pinned copy is selected: the group
/// goes and the cursor stays on that window, in its session.
#[test]
fn answered_while_its_pinned_copy_is_selected() {
    let (_env, s, o) = popup_fixture();
    fake_agent(&s, "agent", WAITING);
    o.open_popup();
    o.wait_for("beta ●1");
    o.keys(&["C-g"]);
    o.wait_cursor_on("agent");
    o.wait_for("─ NEEDS YOU ─");
    fake_agent(
        &s,
        "agent",
        &[("@pane_status", "running"), ("@pane_attention", "clear")],
    );
    o.wait_gone("NEEDS YOU");
    o.wait_cursor_on("agent");
    let row = o.cursor_row().unwrap();
    assert!(row.contains("● running"), "{row}");
    assert_eq!(
        row_windows(&o.screen()),
        ["logs", "build", "agent"],
        "in its session"
    );
}

/// `@waiting` keeps only the waiting agent's window; an agent whose pane
/// is back at its shell is shown dimmed with "(ended?)", and is neither
/// pinned nor counted on its tab.
#[test]
fn waiting_filter_and_stale_agent() {
    let (_env, s, o) = popup_fixture();
    fake_agent(&s, "agent", WAITING);
    for (k, v) in [("@pane_agent", "claude"), ("@pane_status", "waiting")] {
        s.tmux(&["set-option", "-p", "-t", "alpha:win two", k, v]);
    }
    o.open_popup();
    o.wait_for("beta ●1");
    o.wait_for(r"win two .*\(ended\?\)");
    let screen = o.screen();
    assert!(!screen.contains("NEEDS YOU"), "{screen}");
    assert!(
        screen.contains("▶alpha  "),
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
    o.wait_for(r"^> @waiting .*0/2");
    o.keys(&["C-b", ")"]);
    o.wait_for(r"^> @waiting .*1/3");
    let screen = o.screen();
    assert!(!screen.contains("win two"), "{screen}");
    assert_eq!(
        row_windows(&screen),
        ["agent", "agent"],
        "pinned and in its session:\n{screen}"
    );
    o.typed(" zzz");
    o.wait_for(r"^> @waiting zzz .*0/3");
}
