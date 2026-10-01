//! Close and reopen against a throwaway server: the bash suite's
//! closed-window stack checks (rows 13-27), through `ops::Tx` and the
//! `tmux-home reopen` CLI.
mod common;
use common::*;
use tmux_home::{
    ops::{CloseOutcome, ClosePlan, Tx},
    store::{ClosedWindow, Store},
};

fn tx(s: &TestServer) -> Tx {
    Tx::new(s.socket.clone())
}

fn store(s: &TestServer) -> Store {
    Store::for_socket(&s.socket).unwrap()
}

/// index|name|auto-rename|pane count, then index:active:geometry:cwd per pane
fn shape(s: &TestServer, w: &str) -> String {
    fmt(
        s,
        w,
        "#{window_index}|#{window_name}|#{?automatic-rename,on,off}|#{window_panes}",
    ) + "\n"
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
        .env_remove("TMUX")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

/// /usr, /etc and /var resolved (macOS: /var -> /private/var).
fn real(p: &str) -> String {
    std::fs::canonicalize(p)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn close_refuses_last_window_on_server() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let w = wid(&s, "alpha:");
    let t = tx(&s);
    assert_eq!(t.close_plan(&w).unwrap(), ClosePlan::Refuse);
    assert_eq!(
        t.close_window(&w, None, &store(&s)).unwrap(),
        CloseOutcome::Refused
    );
    assert_eq!(window_ids(&s), [w], "the window survives");
    assert!(store(&s).is_empty().unwrap());
}

#[test]
fn close_plan_by_the_bash_rules() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "idle"]);
    s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "alpha:",
        "-n",
        "busy",
        "sleep 1000",
    ]);
    s.tmux(&["new-session", "-d", "-s", "solo", "-n", "only"]);
    s.wait_settled();
    let t = tx(&s);
    assert_eq!(
        t.close_plan(&wid(&s, "alpha:idle")).unwrap(),
        ClosePlan::Now
    );
    assert_eq!(
        t.close_plan(&wid(&s, "alpha:busy")).unwrap(),
        ClosePlan::Ask("close \"busy\"? running: sleep (y/N) ".into())
    );
    assert_eq!(
        t.close_plan(&wid(&s, "solo:only")).unwrap(),
        ClosePlan::Ask("close \"only\"? — session \"solo\" will end (y/N) ".into())
    );
}

#[test]
fn close_then_reopen_restores_shape() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "alpha:5",
        "-n",
        "qshape",
        "-c",
        "/usr",
    ]);
    s.tmux(&[
        "split-window",
        "-d",
        "-h",
        "-t",
        "alpha:qshape",
        "-c",
        "/etc",
    ]);
    s.tmux(&[
        "split-window",
        "-d",
        "-v",
        "-t",
        "alpha:qshape.1",
        "-c",
        "/var",
    ]);
    s.tmux(&["resize-pane", "-t", "alpha:qshape.0", "-x", "60"]);
    s.tmux(&["select-pane", "-t", "alpha:qshape.2"]);
    s.wait_settled();
    let want = shape(&s, "alpha:qshape");
    assert!(want.contains(&real("/var")), "{want}");
    let (t, st) = (tx(&s), store(&s));
    assert_eq!(
        t.close_window(&wid(&s, "alpha:qshape"), None, &st).unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(st.len().unwrap(), 1, "close pushes a snapshot");
    assert!(!window_names(&s, "alpha").contains(&"qshape".to_string()));
    let new = t.reopen(&st).unwrap().expect("something to reopen");
    s.wait_settled();
    assert_eq!(wid(&s, "alpha:qshape"), new, "reopen returns the new ID");
    assert_eq!(shape(&s, &new), want);
    assert!(st.is_empty().unwrap(), "reopen pops the stack");
}

#[test]
fn reopen_keeps_automatic_rename() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:5", "-n", "qauto"]);
    s.tmux(&[
        "set-option",
        "-w",
        "-t",
        "alpha:qauto",
        "automatic-rename",
        "on",
    ]);
    let (t, st) = (tx(&s), store(&s));
    t.close_window(&wid(&s, "alpha:5"), None, &st).unwrap();
    let new = t.reopen(&st).unwrap().unwrap();
    assert_eq!(fmt(&s, &new, "#{?automatic-rename,on,off}"), "on");
}

#[test]
fn reopen_after_old_neighbour_when_index_taken() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    for (i, n) in [(5, "qn5"), (6, "qn6"), (7, "qn7")] {
        s.tmux(&["new-window", "-d", "-t", &format!("alpha:{i}"), "-n", n]);
    }
    let (t, st) = (tx(&s), store(&s));
    t.close_window(&wid(&s, "alpha:qn6"), None, &st).unwrap();
    s.tmux(&["move-window", "-s", "alpha:qn7", "-t", "alpha:6"]);
    t.reopen(&st).unwrap().unwrap();
    let qn: Vec<String> = window_names(&s, "alpha")
        .into_iter()
        .filter(|n| n.starts_with("qn"))
        .collect();
    assert_eq!(qn, ["qn5", "qn6", "qn7"]);
}

/// Its old left neighbour gone too: it goes before its old right one.
#[test]
fn reopen_before_old_right_neighbour_when_left_is_gone() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    for (i, n) in [(4, "qa"), (5, "qb"), (6, "qc")] {
        s.tmux(&["new-window", "-d", "-t", &format!("alpha:{i}"), "-n", n]);
    }
    s.tmux(&["kill-window", "-t", "alpha:0"]);
    let (t, st) = (tx(&s), store(&s));
    t.close_window(&wid(&s, "alpha:qb"), None, &st).unwrap();
    s.tmux(&["kill-window", "-t", "alpha:qa"]);
    s.tmux(&["move-window", "-s", "alpha:qc", "-t", "alpha:5"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:9", "-n", "qz"]);
    t.reopen(&st).unwrap().unwrap();
    assert_eq!(window_names(&s, "alpha"), ["qb", "qc", "qz"]);
}

#[test]
fn reopen_recreates_ended_session() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "qgone",
        "-n",
        "qlast",
        "-c",
        "/usr",
    ]);
    s.wait_settled();
    let (t, st) = (tx(&s), store(&s));
    t.close_window(&wid(&s, "qgone:qlast"), None, &st).unwrap();
    assert!(
        !sessions(&s).contains(&"qgone".to_string()),
        "closing the last window ends the session"
    );
    let new = t.reopen(&st).unwrap().unwrap();
    s.wait_settled();
    assert_eq!(
        fmt(
            &s,
            &new,
            "#{session_name} #{window_name} #{pane_current_path}"
        ),
        "qgone qlast /usr"
    );
}

#[test]
fn stack_is_lifo_capped_at_ten() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-session", "-d", "-s", "qstack"]);
    for i in 1..=12 {
        s.tmux(&["new-window", "-d", "-t", "qstack:", "-n", &format!("qs{i}")]);
    }
    let (t, st) = (tx(&s), store(&s));
    for i in 1..=12 {
        t.close_window(&wid(&s, &format!("qstack:qs{i}")), None, &st)
            .unwrap();
    }
    assert_eq!(st.len().unwrap(), 10, "the last 10 closes");
    let mut order = Vec::new();
    for _ in 0..10 {
        let w = t.reopen(&st).unwrap().unwrap();
        order.push(fmt(&s, &w, "#{window_name}"));
    }
    assert_eq!(
        order,
        [
            "qs12", "qs11", "qs10", "qs9", "qs8", "qs7", "qs6", "qs5", "qs4", "qs3"
        ]
    );
    let before = window_ids(&s).len();
    assert_eq!(reopen_cli(&s).0, 4, "empty stack exits 4");
    assert_eq!(window_ids(&s).len(), before, "and creates nothing");
}

#[test]
fn reopen_cli_prints_the_new_window_id() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qcli"]);
    tx(&s)
        .close_window(&wid(&s, "alpha:qcli"), None, &store(&s))
        .unwrap();
    let (code, out) = reopen_cli(&s);
    assert_eq!(code, 0);
    assert_eq!(out, wid(&s, "alpha:qcli"));
}

#[test]
fn reopen_with_missing_cwd_falls_back_to_home() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let st = store(&s);
    st.push(ClosedWindow {
        session: "alpha".into(),
        index: 7,
        prev: "-".into(),
        next: "-".into(),
        automatic_rename: false,
        active: 0,
        layout: String::new(),
        name: "gone-dir".into(),
        paths: vec![
            "/nonexistent/th-gone".into(),
            "/nonexistent/th-gone2".into(),
        ],
    })
    .unwrap();
    let new = tx(&s).reopen(&st).unwrap().unwrap();
    assert_eq!(
        fmt(&s, &new, "#{window_index} #{window_name} #{window_panes}"),
        "7 gone-dir 2"
    );
    let home = std::env::var("HOME").unwrap();
    let paths = s.tmux(&["list-panes", "-t", &new, "-F", "#{pane_current_path}"]);
    assert_eq!(
        paths.lines().collect::<Vec<_>>(),
        [home.as_str(), home.as_str()],
        "each missing cwd falls back to $HOME"
    );
}

/// The sidebar pane is not part of the shape: one pane comes back, with
/// the main pane's cwd.
#[test]
fn close_skips_sidebar_panes_in_the_shape() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "alpha:",
        "-n",
        "qside",
        "-c",
        "/usr",
    ]);
    s.tmux(&[
        "split-window",
        "-d",
        "-h",
        "-t",
        "alpha:qside",
        "-c",
        "/etc",
        "sleep 1000",
    ]);
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "alpha:qside.1",
        "@pane_role",
        "sidebar",
    ]);
    s.wait_settled();
    let (t, st) = (tx(&s), store(&s));
    let w = wid(&s, "alpha:qside");
    assert_eq!(t.close_plan(&w).unwrap(), ClosePlan::Now);
    t.close_window(&w, None, &st).unwrap();
    let new = t.reopen(&st).unwrap().unwrap();
    s.wait_settled();
    assert_eq!(
        fmt(&s, &new, "#{window_panes} #{pane_current_path}"),
        "1 /usr"
    );
}
