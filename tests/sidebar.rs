//! The sidebar on throwaway servers: `sidebar-toggle` (window and
//! session), the daemon's auto-create, dedupe and cleanup, self-marking
//! (and what tmux-resurrect would save), the popup's and `^x`'s rules
//! skipping sidebar panes, the bindings, and live rendering.
mod common;
use common::*;
use std::process::Command;
use std::time::Duration;
use tmux_home::ops::{ClosePlan, Tx};

const BIN: &str = env!("CARGO_BIN_EXE_tmux-home");

/// `tmux-home sidebar-toggle --socket <s> <args>`, as a key binding runs it.
fn toggle(s: &TestServer, args: &[&str]) {
    let out = Command::new(BIN)
        .arg("sidebar-toggle")
        .arg("--socket")
        .arg(&s.socket)
        .args(args)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sidebar-toggle {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The tmux-home sidebar panes of `window`.
fn sidebars(s: &TestServer, window: &str) -> Vec<String> {
    s.tmux(&["list-panes", "-t", window, "-F", "#{pane_id} #{@home_role}"])
        .lines()
        .filter_map(|l| l.strip_suffix(" sidebar").map(str::to_string))
        .collect()
}

fn panes(s: &TestServer, window: &str) -> Vec<String> {
    s.tmux(&["list-panes", "-t", window, "-F", "#{pane_id}"])
        .lines()
        .map(str::to_string)
        .collect()
}

fn capture(s: &TestServer, pane: &str) -> String {
    s.tmux(&["capture-pane", "-p", "-t", pane])
}

/// Waits for sidebar `pane`'s screen to match `re` (multi-line).
fn wait_screen(s: &TestServer, pane: &str, re: &str) {
    let r = regex::Regex::new(&format!("(?m){re}")).unwrap();
    let mut last = String::new();
    let ok = (0..160).any(|_| {
        last = capture(s, pane);
        let hit = r.is_match(&last);
        if !hit {
            std::thread::sleep(Duration::from_millis(50));
        }
        hit
    });
    assert!(ok, "sidebar {pane} never matched {re:?}:\n{last}");
}

fn wait_screen_gone(s: &TestServer, pane: &str, re: &str) {
    let r = regex::Regex::new(&format!("(?m){re}")).unwrap();
    wait_until(&format!("{re:?} gone from {pane}"), || {
        !r.is_match(&capture(s, pane))
    });
}

/// A second sidebar process in `window`, started the way a pane
/// tmux-resurrect restored runs it: unmarked, it marks itself.
fn unmarked_sidebar(s: &TestServer, window: &str) -> String {
    s.tmux(&[
        "split-window", "-d", "-h", "-t", window, "-P", "-F", "#{pane_id}", "/bin/sh", "-c",
        "\"$0\" sidebar; exit", BIN,
    ])
    .trim()
    .to_string()
}

#[test]
fn toggle_adds_and_removes_a_sidebar_in_the_window() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "two"]);
    let main = panes(&s, "alpha:0")[0].clone();
    toggle(&s, &["--window", "alpha:0"]);
    let side = sidebars(&s, "alpha:0");
    assert_eq!(side.len(), 1, "one sidebar");
    assert_eq!(
        fmt(&s, &side[0], "#{pane_left} #{pane_width} #{pane_active}"),
        "0 32 0",
        "full height at the left, 32 wide, never focused"
    );
    assert_eq!(fmt(&s, "alpha:0", "#{pane_id}"), main, "focus stays put");
    assert!(sidebars(&s, "alpha:1").is_empty(), "only that window");
    wait_screen(&s, &side[0], r"^─ alpha ─");
    wait_screen(&s, &side[0], r"^▌  0 ");
    toggle(&s, &["--window", "alpha:0"]);
    assert!(sidebars(&s, "alpha:0").is_empty());
    assert_eq!(panes(&s, "alpha:0"), [main]);
}

#[test]
fn width_and_side_come_from_options() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-sidebar-width", "40"]);
    s.tmux(&["set-option", "-g", "@home-sidebar-side", "right"]);
    toggle(&s, &["--window", "alpha:0"]);
    let side = sidebars(&s, "alpha:0");
    assert_eq!(
        fmt(&s, &side[0], "#{pane_width} #{pane_right}"),
        "40 199",
        "40 wide at the right edge"
    );
}

#[test]
fn session_toggle_turns_on_where_lacking_then_off_and_leaves_other_sessions() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    s.tmux(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"]);
    s.tmux(&["new-window", "-d", "-t", "beta:"]);
    toggle(&s, &["--window", "beta:0"]); // beta:0 has one of its own
    toggle(&s, &["--window", "alpha:1"]);
    let before = sidebars(&s, "alpha:1");
    toggle(&s, &["--session", "--window", "alpha:0"]);
    for w in ["alpha:0", "alpha:1", "alpha:2"] {
        assert_eq!(sidebars(&s, w).len(), 1, "{w}: on");
    }
    assert_eq!(sidebars(&s, "alpha:1"), before, "an existing one is kept");
    assert_eq!(sidebars(&s, "beta:0").len(), 1, "beta untouched");
    assert!(sidebars(&s, "beta:1").is_empty(), "beta untouched");
    toggle(&s, &["--session", "--window", "alpha:2"]);
    for w in ["alpha:0", "alpha:1", "alpha:2"] {
        assert!(sidebars(&s, w).is_empty(), "{w}: off");
    }
    assert_eq!(sidebars(&s, "beta:0").len(), 1, "beta's still there");
}

#[test]
fn auto_create_adds_to_new_windows_except_excluded_sessions_once() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-sidebar-auto", "on"]);
    s.tmux(&["set-option", "-g", "@home-sidebar-exclude", "scratch other"]);
    install_binding(&s); // the daemon; the server is fresh, so alpha:0 is new
    wait_until("a sidebar in alpha:0", || sidebars(&s, "alpha:0").len() == 1);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "two"]);
    wait_until("a sidebar in alpha:1", || sidebars(&s, "alpha:1").len() == 1);
    s.tmux(&["new-session", "-d", "-s", "scratch", "-x", "200", "-y", "50"]);
    s.tmux(&["new-window", "-d", "-t", "scratch:"]);
    // a toggled-off sidebar stays off
    toggle(&s, &["--window", "alpha:1"]);
    std::thread::sleep(Duration::from_millis(1500)); // three polls
    assert!(sidebars(&s, "scratch:0").is_empty(), "excluded");
    assert!(sidebars(&s, "scratch:1").is_empty(), "excluded");
    assert_eq!(sidebars(&s, "alpha:0").len(), 1, "never two");
    assert!(sidebars(&s, "alpha:1").is_empty(), "decided once");
    assert_eq!(panes(&s, "alpha:1").len(), 1);
}

#[test]
fn auto_create_is_off_by_default_and_ignores_windows_older_than_the_daemon() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    install_binding(&s);
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    std::thread::sleep(Duration::from_millis(1200));
    assert!(sidebars(&s, "alpha:1").is_empty(), "off by default");
    // turned on later: only windows made from then on
    s.tmux(&["set-option", "-g", "@home-sidebar-auto", "on"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    wait_until("a sidebar in alpha:2", || sidebars(&s, "alpha:2").len() == 1);
    assert!(sidebars(&s, "alpha:0").is_empty());
    assert!(sidebars(&s, "alpha:1").is_empty());
}

#[test]
fn a_second_sidebar_in_a_window_is_removed_keeping_the_first() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    install_binding(&s);
    toggle(&s, &["--window", "alpha:0"]);
    let first = sidebars(&s, "alpha:0");
    let second = unmarked_sidebar(&s, "alpha:0");
    wait_until("the second sidebar removed", || {
        !panes(&s, "alpha:0").contains(&second)
    });
    assert_eq!(sidebars(&s, "alpha:0"), first);
    assert_eq!(panes(&s, "alpha:0").len(), 2);
    assert_eq!(
        fmt(&s, &first[0], "#{pane_width}"),
        "32",
        "back to its width"
    );
}

#[test]
fn the_sidebar_goes_when_the_last_real_pane_exits() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    install_binding(&s);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "work"]);
    s.tmux(&["split-window", "-d", "-t", "alpha:work"]);
    toggle(&s, &["--window", "alpha:work"]);
    let wid = wid(&s, "alpha:work");
    let real: Vec<String> = panes(&s, &wid)
        .into_iter()
        .filter(|p| !sidebars(&s, &wid).contains(p))
        .collect();
    assert_eq!(real.len(), 2);
    s.tmux(&["send-keys", "-t", &real[0], "exit", "Enter"]);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(sidebars(&s, &wid).len(), 1, "one real pane left: kept");
    s.tmux(&["send-keys", "-t", &real[1], "exit", "Enter"]);
    wait_until("the window closed", || !window_ids(&s).contains(&wid));
    assert_eq!(sessions(&s), ["alpha"], "the session lives on");
}

#[test]
fn a_sidebar_marks_itself_and_resurrect_would_save_it() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let pane = unmarked_sidebar(&s, "alpha:0");
    wait_until("self-marked", || {
        fmt(&s, &pane, "#{@home_role}") == "sidebar"
    });
    // tmux-resurrect's default `ps` strategy saves the command lines of the
    // pane process's children; `~tmux-home sidebar` is a regex search in it
    let pid = fmt(&s, &pane, "#{pane_pid}");
    let ps = Command::new("ps")
        .args(["-ao", "ppid,args"])
        .output()
        .unwrap();
    let saved: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .map(|l| l.trim_start())
        .filter_map(|l| l.strip_prefix(&format!("{pid} ")))
        .map(str::to_string)
        .collect();
    assert_eq!(saved, [format!("{BIN} sidebar")], "what resurrect saves");
    assert!(regex::Regex::new("(tmux-home sidebar)").unwrap().is_match(&saved[0]));
}

#[test]
fn a_restored_sidebar_runs_in_a_shell_and_q_closes_its_pane() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    // what resurrect does on restore: a shell, the saved command typed in
    let pane = s
        .tmux(&["split-window", "-d", "-h", "-t", "alpha:0", "-P", "-F", "#{pane_id}"])
        .trim()
        .to_string();
    s.tmux(&["send-keys", "-t", &pane, &format!("{BIN} sidebar"), "Enter"]);
    wait_until("self-marked", || {
        fmt(&s, &pane, "#{@home_role}") == "sidebar"
    });
    wait_screen(&s, &pane, r"^─ alpha ─");
    s.tmux(&["send-keys", "-t", &pane, "x", "Enter", "j"]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(panes(&s, "alpha:0").contains(&pane), "other keys are ignored");
    s.tmux(&["send-keys", "-t", &pane, "q"]);
    wait_until("the pane closed, shell and all", || {
        !panes(&s, "alpha:0").contains(&pane)
    });
}

#[test]
fn the_popup_and_close_rules_ignore_sidebar_panes() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    let wid = wid(&s, "alpha:0");
    let main = panes(&s, &wid)[0].clone();
    toggle(&s, &["--window", &wid]);
    let side = sidebars(&s, &wid)[0].clone();
    wait_until("self-marked and running", || {
        capture(&s, &side).contains("─ alpha ─")
    });
    s.tmux(&["select-pane", "-t", &side]); // even when focused
    let tx = Tx::new(s.socket.clone());
    assert_eq!(tx.busy_commands(&wid), Vec::<String>::new());
    assert_eq!(tx.close_plan(&wid).unwrap(), ClosePlan::Now);
    let closed = tx.snapshot_window(&wid).unwrap();
    assert_eq!(closed.paths.len(), 1, "the sidebar isn't reopened");
    assert_eq!(closed.layout, "", "nor is its layout");
    let snap = tmux_home::tmux::snapshot::read_snapshot(&tx.tmux).unwrap();
    let rows = tmux_home::popup::app::build_rows(&snap, None, "");
    let row = rows.iter().find(|r| r.wid == wid).unwrap();
    assert_eq!(row.pane.as_deref(), Some(main.as_str()));
    assert_eq!(rows.len(), 2, "one row per window");
    let sp = snap.panes.iter().find(|p| p.id == side).unwrap();
    assert_eq!((sp.role.as_str(), sp.home_role.as_str()), ("sidebar", "sidebar"));
    // tmux-agent-sidebar's marker still counts
    s.tmux(&["set-option", "-p", "-t", &side, "-u", "@home_role"]);
    s.tmux(&["set-option", "-p", "-t", &side, "@pane_role", "sidebar"]);
    let snap = tmux_home::tmux::snapshot::read_snapshot(&tx.tmux).unwrap();
    let sp = snap.panes.iter().find(|p| p.id == side).unwrap();
    assert_eq!((sp.role.as_str(), sp.home_role.as_str()), ("sidebar", ""));
    assert_eq!(tx.snapshot_window(&wid).unwrap().paths.len(), 1);
}

#[test]
fn prefix_e_and_shift_e_toggle_through_the_binding() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    s.tmux(&["new-session", "-d", "-s", "beta"]);
    install_binding(&s);
    let o = Outer::attach(&s, "alpha:0", 120, 30);
    o.keys(&["C-b", "e"]);
    wait_until("a sidebar in alpha:0", || sidebars(&s, "alpha:0").len() == 1);
    assert!(sidebars(&s, "alpha:1").is_empty());
    o.keys(&["C-b", "E"]);
    wait_until("one in alpha:1", || sidebars(&s, "alpha:1").len() == 1);
    assert_eq!(sidebars(&s, "alpha:0").len(), 1);
    assert!(sidebars(&s, "beta:0").is_empty(), "the session only");
    o.keys(&["C-b", "E"]);
    wait_until("all off", || {
        sidebars(&s, "alpha:0").is_empty() && sidebars(&s, "alpha:1").is_empty()
    });
}

/// The sidebar follows agent and git changes as the daemon pushes them.
#[test]
fn the_sidebar_renders_live_agent_and_badge_changes() {
    git_env(); // before the server starts: its daemon inherits it
    let t = TempDir::new("sidebar");
    let repo = t.repo("app");
    with_origin(&repo);
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["rename-window", "-t", "alpha:0", "home"]);
    s.tmux(&[
        "new-window", "-d", "-t", "alpha:", "-n", "app", "-c", repo.to_str().unwrap(),
    ]);
    s.tmux(&["new-session", "-d", "-s", "beta", "-n", "agent", &fake_claude()]);
    install_binding(&s);
    toggle(&s, &["--window", "alpha:0"]);
    let side = sidebars(&s, "alpha:0")[0].clone();
    wait_screen(&s, &side, r"^▌  0 home");
    // the badge, then a change to it
    wait_screen(&s, &side, r"^   1 app +main \|$");
    write(&repo, "README", "edited\n");
    wait_screen(&s, &side, r"^   1 app +main ! \|$");
    // an agent elsewhere on the server starts waiting: NEEDS YOU
    for (k, v) in [
        ("@pane_agent", "claude"),
        ("@pane_status", "waiting"),
        ("@pane_attention", "notification"),
        ("@pane_wait_reason", "permission_prompt"),
    ] {
        s.tmux(&["set-option", "-p", "-t", "beta:agent.0", k, v]);
    }
    wait_screen(&s, &side, r"^─ NEEDS YOU ─");
    wait_screen(&s, &side, r"^ ◐ beta:0 agent");
    wait_screen(&s, &side, r"^   permission");
    wait_screen(&s, &side, r"^ .*· 1 waiting$");
    // answered: running, and NEEDS YOU goes
    s.tmux(&["set-option", "-p", "-t", "beta:agent.0", "@pane_status", "running"]);
    s.tmux(&["set-option", "-p", "-t", "beta:agent.0", "@pane_attention", "clear"]);
    wait_screen_gone(&s, &side, r"NEEDS YOU");
    wait_screen(&s, &side, r"^ 2s 3w · 1 running$");
}
