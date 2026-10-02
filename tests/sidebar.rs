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
/// tmux-resurrect restored runs it (`--managed`): unmarked, it marks itself.
fn unmarked_sidebar(s: &TestServer, window: &str) -> String {
    s.tmux(&[
        "split-window",
        "-d",
        "-h",
        "-t",
        window,
        "-P",
        "-F",
        "#{pane_id}",
        "/bin/sh",
        "-c",
        "\"$0\" sidebar --managed; exit",
        BIN,
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

/// Auto-create's start-up gate, shortened: no minimum age, windows and
/// panes unchanged for 300 ms. Set before the daemon starts (it reads
/// the server's global environment).
fn quick_gate(s: &TestServer) {
    s.tmux(&["set-environment", "-g", "TMUX_HOME_AUTO_MIN_AGE", "0"]);
    s.tmux(&["set-environment", "-g", "TMUX_HOME_AUTO_STABLE_MS", "300"]);
}

#[test]
fn auto_create_adds_to_new_windows_except_excluded_sessions_once() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-sidebar-auto", "on"]);
    s.tmux(&["set-option", "-g", "@home-sidebar-exclude", "scratch other"]);
    quick_gate(&s);
    install_binding(&s);
    std::thread::sleep(Duration::from_millis(1500)); // allowed by now
    assert!(sidebars(&s, "alpha:0").is_empty(), "pre-existing");
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "two"]);
    wait_until("a sidebar in alpha:1", || {
        sidebars(&s, "alpha:1").len() == 1
    });
    let side = sidebars(&s, "alpha:1")[0].clone();
    assert_eq!(fmt(&s, &side, "#{pane_active}"), "0", "never focused");
    s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "scratch",
        "-x",
        "200",
        "-y",
        "50",
    ]);
    s.tmux(&["new-window", "-d", "-t", "scratch:"]);
    // a toggled-off sidebar stays off
    toggle(&s, &["--window", "alpha:1"]);
    std::thread::sleep(Duration::from_millis(1500)); // three polls
    assert!(sidebars(&s, "scratch:0").is_empty(), "excluded");
    assert!(sidebars(&s, "scratch:1").is_empty(), "excluded");
    assert!(sidebars(&s, "alpha:0").is_empty());
    assert!(sidebars(&s, "alpha:1").is_empty(), "decided once");
    assert_eq!(panes(&s, "alpha:1").len(), 1);
}

#[test]
fn auto_create_is_off_by_default_and_decides_each_window_once() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    quick_gate(&s);
    install_binding(&s);
    std::thread::sleep(Duration::from_millis(1500));
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    std::thread::sleep(Duration::from_millis(1200));
    assert!(sidebars(&s, "alpha:1").is_empty(), "off by default");
    // turned on later: only windows made from then on
    s.tmux(&["set-option", "-g", "@home-sidebar-auto", "on"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    wait_until("a sidebar in alpha:2", || {
        sidebars(&s, "alpha:2").len() == 1
    });
    assert!(sidebars(&s, "alpha:0").is_empty());
    assert!(sidebars(&s, "alpha:1").is_empty());
}

/// A young server gets no auto-created sidebars, and with continuum's
/// restore on none until resurrect's post-restore hook says it's done.
#[test]
fn auto_create_waits_for_server_age_and_the_restore() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-sidebar-auto", "on"]);
    s.tmux(&["set-environment", "-g", "TMUX_HOME_AUTO_STABLE_MS", "300"]);
    // default minimum age (30 s): this server is seconds old
    install_binding(&s);
    std::thread::sleep(Duration::from_millis(1000));
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(sidebars(&s, "alpha:1").is_empty(), "too young");
    drop(s);

    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-sidebar-auto", "on"]);
    s.tmux(&["set-option", "-g", "@continuum-restore", "on"]);
    quick_gate(&s);
    install_binding(&s);
    std::thread::sleep(Duration::from_millis(1000));
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(sidebars(&s, "alpha:1").is_empty(), "restoring");
    s.tmux(&["set-option", "-g", "@home_restore_done", "1"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(sidebars(&s, "alpha:1").is_empty(), "made while restoring");
    s.tmux(&["new-window", "-d", "-t", "alpha:"]);
    wait_until("a sidebar in alpha:2", || {
        sidebars(&s, "alpha:2").len() == 1
    });
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
    assert_eq!(
        saved,
        [format!("{BIN} sidebar --managed")],
        "what resurrect saves"
    );
    assert!(
        regex::Regex::new("(tmux-home sidebar)")
            .unwrap()
            .is_match(&saved[0])
    );
}

#[test]
fn a_restored_sidebar_runs_in_a_shell_and_q_closes_its_pane() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    // what resurrect does on restore: a shell, the saved command typed in
    let pane = s
        .tmux(&[
            "split-window",
            "-d",
            "-h",
            "-t",
            "alpha:0",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .trim()
        .to_string();
    s.tmux(&[
        "send-keys",
        "-t",
        &pane,
        &format!("{BIN} sidebar --managed"),
        "Enter",
    ]);
    wait_until("self-marked", || {
        fmt(&s, &pane, "#{@home_role}") == "sidebar"
    });
    wait_screen(&s, &pane, r"^─ alpha ─");
    s.tmux(&["send-keys", "-t", &pane, "x", "Enter", "j"]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        panes(&s, "alpha:0").contains(&pane),
        "other keys are ignored"
    );
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
    assert_eq!(
        (sp.role.as_str(), sp.home_role.as_str()),
        ("sidebar", "sidebar")
    );
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
    wait_until("a sidebar in alpha:0", || {
        sidebars(&s, "alpha:0").len() == 1
    });
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
        "new-window",
        "-d",
        "-t",
        "alpha:",
        "-n",
        "app",
        "-c",
        repo.to_str().unwrap(),
    ]);
    s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-n",
        "agent",
        &fake_claude(),
    ]);
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
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "beta:agent.0",
        "@pane_status",
        "running",
    ]);
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "beta:agent.0",
        "@pane_attention",
        "clear",
    ]);
    wait_screen_gone(&s, &side, r"NEEDS YOU");
    wait_screen(&s, &side, r"^ 2s 3w · 1 running$");
}

/// Run by hand in a working pane, a sidebar marks nothing, and `q` exits
/// it, leaving the pane and its shell.
#[test]
fn a_sidebar_run_by_hand_is_not_managed() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let pane = panes(&s, "alpha:0")[0].clone();
    s.tmux(&["send-keys", "-t", &pane, &format!("{BIN} sidebar"), "Enter"]);
    wait_screen(&s, &pane, r"^─ alpha ─");
    assert_eq!(fmt(&s, &pane, "#{@home_role}"), "");
    s.tmux(&["send-keys", "-t", &pane, "q"]);
    s.tmux(&["send-keys", "-t", &pane, "echo STILL-HERE", "Enter"]);
    wait_screen(&s, &pane, r"^STILL-HERE");
    assert_eq!(panes(&s, "alpha:0"), [pane]);
}

/// A sidebar in a server that is starting up doesn't start the daemon
/// (the plugin does that later on purpose); it reads tmux directly.
#[test]
fn a_sidebar_in_a_young_server_does_not_start_the_daemon() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    toggle(&s, &["--window", "alpha:0"]);
    let side = sidebars(&s, "alpha:0")[0].clone();
    wait_screen(&s, &side, r"^ ○ 1s 1w");
    std::thread::sleep(Duration::from_millis(2500));
    let sock = tmux_home::paths::Paths::for_socket(&s.socket).unwrap().sock;
    assert!(!sock.exists(), "no daemon yet");
    // once the daemon exists, it subscribes
    install_binding(&s);
    wait_screen(&s, &side, r"^ 1s 1w");
    drop(s);
    // past that age it starts one itself
    let s = TestServer::start();
    s.tmux(&["set-environment", "-g", "TMUX_HOME_SIDEBAR_REVIVE_AGE", "0"]);
    toggle(&s, &["--window", "alpha:0"]);
    let side = sidebars(&s, "alpha:0")[0].clone();
    wait_screen(&s, &side, r"^ 1s 1w");
}

/// A restored sidebar keeps the width it was saved with; the daemon sets
/// it to `@home-sidebar-width` when it first sees it.
#[test]
fn a_sidebar_first_seen_is_resized_to_the_configured_width() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    install_binding(&s);
    let pane = s
        .tmux(&[
            "split-window",
            "-d",
            "-h",
            "-b",
            "-l",
            "50",
            "-t",
            "alpha:0",
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
            "-c",
            "\"$0\" sidebar --managed; exit",
            BIN,
        ])
        .trim()
        .to_string();
    wait_until("resized to 32", || fmt(&s, &pane, "#{pane_width}") == "32");
}

/// tmux-resurrect's scripts, if installed (the test is skipped without).
fn resurrect_scripts() -> Option<std::path::PathBuf> {
    let d = std::path::PathBuf::from(std::env::var("HOME").ok()?)
        .join(".tmux/plugins/tmux-resurrect/scripts");
    d.join("restore.sh").exists().then_some(d)
}

/// Per window `session:index`: (pane count, sidebar count, the active
/// pane is a sidebar, the sidebars' left edge and width).
fn layout(s: &TestServer) -> Vec<(String, usize, usize, bool, Vec<String>)> {
    let mut out = Vec::new();
    for w in s
        .tmux(&[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index}",
        ])
        .lines()
    {
        let rows = s.tmux(&[
            "list-panes",
            "-t",
            w,
            "-F",
            "#{@home_role}|#{pane_active}|#{pane_left}x#{pane_width}",
        ]);
        let rows: Vec<Vec<&str>> = rows.lines().map(|l| l.split('|').collect()).collect();
        let side: Vec<&Vec<&str>> = rows.iter().filter(|r| r[0] == "sidebar").collect();
        out.push((
            w.to_string(),
            rows.len(),
            side.len(),
            rows.iter().any(|r| r[0] == "sidebar" && r[1] == "1"),
            side.iter().map(|r| r[2].to_string()).collect(),
        ));
    }
    out
}

/// A cold start that restores a resurrect save with sidebars in it, the
/// daemon up from the first second and auto-create on: every window comes
/// back as saved (no orphan or extra panes), one sidebar each at the left
/// at its width, none focused; only windows made afterwards get a new one.
/// HOME, the resurrect dir and tmux-home's dirs are all temporary.
#[test]
fn a_cold_start_restore_with_sidebars_comes_back_as_saved() {
    let Some(scripts) = resurrect_scripts() else {
        eprintln!("skipped: tmux-resurrect isn't installed");
        return;
    };
    let _env = TestEnv::new();
    let home = TempDir::new("home");
    let dir = home.0.join("resurrect");
    std::fs::create_dir_all(&dir).unwrap();
    let envs: Vec<(&str, std::ffi::OsString)> = vec![
        ("HOME", home.0.clone().into()),
        ("XDG_DATA_HOME", home.0.join("share").into()),
        ("XDG_STATE_HOME", home.0.join("state").into()),
        ("XDG_CONFIG_HOME", home.0.join("config").into()),
    ];
    let envs: Vec<(&str, &std::ffi::OsStr)> =
        envs.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
    let resurrect_opts = |s: &TestServer| {
        s.tmux(&["set-option", "-g", "@resurrect-dir", dir.to_str().unwrap()]);
        s.tmux(&[
            "set-option",
            "-g",
            "@resurrect-processes",
            "\"~tmux-home sidebar\"",
        ]);
    };

    // server A: main (3 windows, one split) with sidebars, scratch without
    let a = TestServer::start_with(&envs);
    a.tmux(&["rename-session", "-t", "alpha", "main"]);
    a.tmux(&["new-window", "-d", "-t", "main:"]);
    a.tmux(&["split-window", "-d", "-t", "main:1"]);
    a.tmux(&["new-window", "-d", "-t", "main:"]);
    a.tmux(&[
        "new-session",
        "-d",
        "-s",
        "scratch",
        "-x",
        "200",
        "-y",
        "50",
    ]);
    toggle(&a, &["--session", "--window", "main:0"]);
    for w in ["main:0", "main:1", "main:2"] {
        let side = sidebars(&a, w)[0].clone();
        wait_screen(&a, &side, r"^─ main ─");
    }
    resurrect_opts(&a);
    let saved = layout(&a);
    let o = Outer::attach(&a, "main:0", 200, 50);
    let r = a.tmux(&[
        "run-shell",
        &format!("{}/save.sh quiet 2>&1; echo rc=$?", scripts.display()),
    ]);
    assert!(r.contains("rc=0"), "{r}");
    drop(o);
    let last = std::fs::read_to_string(dir.join("last")).unwrap();
    assert_eq!(
        last.matches("tmux-home sidebar --managed").count(),
        3,
        "{last}"
    );
    drop(a);

    // server B: a cold start, the daemon at once, auto-create on, restore
    let b = TestServer::start_with(&envs);
    b.tmux(&["rename-session", "-t", "alpha", "0"]);
    resurrect_opts(&b);
    b.tmux(&["set-option", "-g", "@home-sidebar-auto", "on"]);
    b.tmux(&["set-option", "-g", "@continuum-restore", "on"]);
    b.tmux(&[
        "set-option",
        "-g",
        "@resurrect-hook-post-restore-all",
        "tmux set -g @home_restore_done 1",
    ]);
    b.tmux(&["set-environment", "-g", "TMUX_HOME_AUTO_MIN_AGE", "0"]);
    b.tmux(&["set-environment", "-g", "TMUX_HOME_AUTO_STABLE_MS", "300"]);
    b.tmux(&["set-environment", "-g", "TMUX_HOME_SIDEBAR_REVIVE_AGE", "0"]);
    install_binding(&b);
    let o = Outer::attach(&b, "0", 200, 50);
    let r = b.tmux(&[
        "run-shell",
        &format!("{}/restore.sh 2>&1; echo rc=$?", scripts.display()),
    ]);
    assert!(r.contains("rc=0"), "{r}");
    assert_eq!(fmt(&b, "main:0", "#{@home_restore_done}"), "1");
    wait_until("the restored sidebars started", || {
        ["main:0", "main:1", "main:2"]
            .iter()
            .all(|w| sidebars(&b, w).len() == 1)
    });
    std::thread::sleep(Duration::from_millis(2000)); // daemon passes
    assert_eq!(layout(&b), saved, "restored as saved");
    for (w, _, n, focused, geo) in &saved {
        if w.starts_with("main") {
            assert_eq!(*n, 1, "{w}");
            assert!(!focused, "{w}");
            assert_eq!(geo, &["0x32".to_string()], "{w}");
        }
    }
    // from now on new windows get one
    b.tmux(&["new-window", "-d", "-t", "main:"]);
    wait_until("a sidebar in main:3", || sidebars(&b, "main:3").len() == 1);
    drop(o);
}
