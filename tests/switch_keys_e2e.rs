//! Through a real client (an outer throwaway server's pane attached to the
//! inner one): the switch keys (`@home-switch-keys on`, C-M-S-F1 … F10 as
//! the terminal sends them for Cmd+Opt+1 … 0) from a pane, a focused
//! sidebar and the open popup; and `@home_sidebar_shown` in a status line.
mod common;
use common::*;
use std::time::{Duration, Instant};

/// The bytes a terminal sends for window `n`'s switch key (xterm's
/// modifier 8: C-M-S-F`n`, C-M-S-F12 for window 3), as `send-keys -H`
/// arguments.
fn switch_seq(n: u32) -> Vec<String> {
    let s = match n {
        1 | 2 | 4 => format!("\x1b[1;8{}", (b'P' + (n - 1) as u8) as char),
        3 => "\x1b[24;8~".into(),
        5 => "\x1b[15;8~".into(),
        6..=10 => format!("\x1b[{};8~", n + 11),
        _ => unreachable!(),
    };
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Writes window `n`'s switch key to the inner client's terminal, raw (`send-keys -H`
/// on the outer pane), so the inner server parses it as from a terminal.
fn press(o: &Outer, n: u32) {
    let hex = switch_seq(n);
    let mut args = vec!["send-keys", "-t", "outer", "-H"];
    args.extend(hex.iter().map(String::as_str));
    o.tmux(&args);
    std::thread::sleep(KEY_GAP);
}

/// Session alpha with windows 1 "one", 2 "two", 3 "three" (base-index 1),
/// the plugin installed with `@home-switch-keys on`, and a client on 1.
fn fixture() -> (TestEnv, TestServer, Outer) {
    let env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "base-index", "1"]);
    s.tmux(&["set-option", "-g", "escape-time", "50"]);
    s.tmux(&["move-window", "-r", "-t", "alpha"]);
    s.tmux(&["rename-window", "-t", "alpha:1", "one"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:2", "-n", "two"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:3", "-n", "three"]);
    s.tmux(&["set-option", "-g", "@home-switch-keys", "on"]);
    install_binding(&s);
    let o = Outer::attach(&s, "alpha:1", 160, 40);
    (env, s, o)
}

fn current(s: &TestServer) -> String {
    s.tmux(&["display-message", "-p", "-t", "alpha", "#{window_index}"])
        .trim()
        .to_string()
}

fn wait_current(s: &TestServer, want: &str) {
    wait_until(&format!("window {want} current"), || current(s) == want);
}

#[test]
fn off_by_default_binds_no_root_keys() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    install_binding(&s);
    let root = s.tmux(&["list-keys", "-T", "root"]);
    assert!(!root.contains("C-M-S-F1"), "{root}");
}

#[test]
fn switch_keys_select_windows_and_ignore_missing_ones() {
    let (_env, s, o) = fixture();
    let root = s.tmux(&["list-keys", "-T", "root"]);
    for f in [1, 2, 12, 4, 5, 6, 7, 8, 9, 10] {
        assert!(root.contains(&format!("C-M-S-F{f} ")), "F{f}: {root}");
    }
    assert!(!root.contains("C-M-S-F3 "), "{root}");
    press(&o, 2);
    wait_current(&s, "2");
    press(&o, 3);
    wait_current(&s, "3");
    press(&o, 1);
    wait_current(&s, "1");
    // no window 7 or 10: nothing happens, and no error message
    press(&o, 7);
    press(&o, 10);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(current(&s), "1");
    let screen = o.screen();
    assert!(!screen.contains("can't find"), "{screen}");
    // window 10 once there is one
    s.tmux(&["new-window", "-d", "-t", "alpha:10", "-n", "ten"]);
    press(&o, 10);
    wait_current(&s, "10");
}

#[test]
fn from_a_focused_sidebar_its_window_gets_its_last_pane_back() {
    let (_env, s, o) = fixture();
    let main = fmt(&s, "alpha:1", "#{pane_id}");
    let side = s
        .tmux(&[
            "split-window",
            "-d",
            "-h",
            "-t",
            "alpha:1",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .trim()
        .to_string();
    s.tmux(&["set-option", "-p", "-t", &side, "@home_role", "sidebar"]);
    s.tmux(&["select-pane", "-t", &side]);
    assert_eq!(fmt(&s, "alpha:1", "#{pane_id}"), side);
    press(&o, 2);
    wait_current(&s, "2");
    assert_eq!(
        fmt(&s, "alpha:1", "#{pane_id}"),
        main,
        "window 1's active pane is its main pane again"
    );
    // from an ordinary pane, nothing changes in the window left
    let two_a = fmt(&s, "alpha:2", "#{pane_id}");
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:2"]);
    press(&o, 3);
    wait_current(&s, "3");
    assert_eq!(fmt(&s, "alpha:2", "#{pane_id}"), two_a);
}

#[test]
fn switch_keys_work_with_the_popup_open() {
    let (_env, s, o) = fixture();
    o.open_popup();
    press(&o, 3);
    wait_current(&s, "3");
    wait_until("the popup closed", || !o.popup_open());
    // a missing window leaves the popup open
    o.open_popup();
    press(&o, 9);
    std::thread::sleep(Duration::from_millis(400));
    assert!(o.popup_open(), "{}", o.screen());
    assert_eq!(current(&s), "3");
    // ... and the next one still switches
    press(&o, 2);
    wait_current(&s, "2");
    wait_until("the popup closed", || !o.popup_open());
}

/// The status line's second row: the window list unless the current
/// window shows a sidebar.
fn status_fixture() -> (TestEnv, TestServer, Outer) {
    let (env, s, o) = fixture();
    s.tmux(&["set-option", "-g", "status-position", "top"]);
    s.tmux(&["set-option", "-g", "status", "2"]);
    // long enough that only a redraw from the change itself can show it
    s.tmux(&["set-option", "-g", "status-interval", "60"]);
    s.tmux(&["set-option", "-g", "status-format[0]", "TOPROW"]);
    s.tmux(&[
        "set-option",
        "-g",
        "status-format[1]",
        "LIST:#{?#{E:@home_sidebar_shown},,#{W:[#{window_name}]}}",
    ]);
    (env, s, o)
}

/// Waits (≤ 1 s) for the status row to read `want`.
fn wait_row(o: &Outer, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let screen = o.screen();
        let row = screen.lines().nth(1).unwrap_or("").trim_end().to_string();
        if row == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "status row {row:?}, want {want:?}:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sidebar_shown_hides_the_window_list_for_the_current_window_only() {
    let (_env, s, o) = status_fixture();
    let all = "LIST:[one][two][three]";
    wait_row(&o, all);
    // a sidebar in window 2: the list stays while window 1 is current
    let side = s
        .tmux(&[
            "split-window",
            "-d",
            "-h",
            "-t",
            "alpha:2",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .trim()
        .to_string();
    s.tmux(&["set-option", "-p", "-t", &side, "@home_role", "sidebar"]);
    wait_row(&o, all);
    press(&o, 2);
    wait_row(&o, "LIST:");
    press(&o, 1);
    wait_row(&o, all);
    // a sidebar added to the current window hides it at once
    let side1 = s
        .tmux(&[
            "split-window",
            "-d",
            "-h",
            "-t",
            "alpha:1",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .trim()
        .to_string();
    s.tmux(&["set-option", "-p", "-t", &side1, "@home_role", "sidebar"]);
    wait_row(&o, "LIST:");
    // zooming the other pane hides the sidebar: the list is back
    s.tmux(&["resize-pane", "-Z", "-t", "alpha:1.1"]);
    wait_row(&o, all);
    s.tmux(&["resize-pane", "-Z", "-t", "alpha:1.1"]);
    wait_row(&o, "LIST:");
    // closing it brings the list back at once
    s.tmux(&["kill-pane", "-t", &side1]);
    wait_row(&o, all);
    // a pane that is only tmux-agent-sidebar's (@pane_role) doesn't count
    s.tmux(&["kill-pane", "-t", &side]);
    let agent = s
        .tmux(&[
            "split-window",
            "-d",
            "-h",
            "-t",
            "alpha:1",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .trim()
        .to_string();
    s.tmux(&["set-option", "-p", "-t", &agent, "@pane_role", "sidebar"]);
    std::thread::sleep(Duration::from_millis(300));
    wait_row(&o, all);
}

#[test]
fn sidebar_shown_with_a_real_sidebar_toggle() {
    let (_env, s, o) = status_fixture();
    let all = "LIST:[one][two][three]";
    wait_row(&o, all);
    let bin = env!("CARGO_BIN_EXE_tmux-home");
    let socket = s.socket.display().to_string();
    let toggle = || {
        let out = std::process::Command::new(bin)
            .args(["sidebar-toggle", "--socket", &socket, "--window", "alpha:1"])
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    toggle();
    wait_row(&o, "LIST:");
    toggle();
    wait_row(&o, all);
}
