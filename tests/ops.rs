//! `ops::Tx` writes against a throwaway tmux server.
mod common;
use common::{TestServer, rand_suffix};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tmux_home::{ops::Tx, store::Store};

fn wid(s: &TestServer, target: &str) -> String {
    s.tmux(&["display", "-p", "-t", target, "#{window_id}"])
        .trim()
        .to_string()
}

fn show(s: &TestServer, target: &str, fmt: &str) -> String {
    s.tmux(&["display", "-p", "-t", target, fmt])
        .trim()
        .to_string()
}

/// A canonical temp dir whose name contains `#`.
struct HashDir(PathBuf);

impl HashDir {
    fn new() -> HashDir {
        let d = std::env::temp_dir().join(format!(
            "th-ops-{}#{}#(true)",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&d).unwrap();
        HashDir(std::fs::canonicalize(&d).unwrap())
    }
    fn s(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for HashDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn store() -> (Store, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "th-ops-store-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    (Store::new(dir.join("srv"), None), dir)
}

#[test]
fn rename_keeps_hash_literally() {
    let s = TestServer::start();
    let tx = Tx::new(s.socket.clone());
    let w = wid(&s, "alpha:");
    for name in ["fix #12", "ab#", "x#(echo pwned)", "#{session_name}", "##"] {
        tx.rename(&w, name).unwrap();
        assert_eq!(tx.window_name(&w).unwrap(), name);
    }
}

#[test]
fn new_window_lands_in_a_hash_cwd_with_a_hash_name() {
    let s = TestServer::start();
    let tx = Tx::new(s.socket.clone());
    let d = HashDir::new();
    let new = tx
        .new_window_after(&wid(&s, "alpha:"), d.s(), "n#1#")
        .unwrap();
    assert_eq!(show(&s, &new, "#{window_name}"), "n#1#");
    assert_eq!(show(&s, &new, "#{pane_start_path}"), d.s());
}

#[test]
fn reopen_restores_hash_name_session_and_cwds() {
    let s = TestServer::start();
    let tx = Tx::new(s.socket.clone());
    let d = HashDir::new();
    let (store, sdir) = store();
    // session "s#1#", window "w#1#": the doubled `#` is tmux's own escape
    let dd = d.s().replace('#', "##");
    s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "s##1##",
        "-n",
        "w##1##",
        "-c",
        &dd,
        "/bin/sh",
    ]);
    s.tmux(&["split-window", "-d", "-t", "=s#1#:", "-c", &dd, "/bin/sh"]);
    s.wait_settled();
    let w = wid(&s, "=s#1#:");
    assert_eq!(show(&s, &w, "#{session_name} #{window_name}"), "s#1# w#1#");
    // the session's last window: the session ends, alpha lives on
    tx.close_window(&w, None, &store).unwrap();
    assert!(
        !s.tmux(&["list-sessions", "-F", "#{session_name}"])
            .contains("s#1#")
    );

    let new = tx.reopen(&store).unwrap().expect("reopened");
    assert_eq!(show(&s, &new, "#{session_name}"), "s#1#");
    assert_eq!(show(&s, &new, "#{window_name}"), "w#1#");
    let starts = s.tmux(&["list-panes", "-t", &new, "-F", "#{pane_start_path}"]);
    assert_eq!(starts.lines().collect::<Vec<_>>(), vec![d.s(), d.s()]);
    let _ = std::fs::remove_dir_all(sdir);
}

#[test]
fn reset_name_reenables_automatic_rename_when_global_is_off() {
    let s = TestServer::start();
    let tx = Tx::new(s.socket.clone());
    let w = wid(&s, "alpha:");
    tx.rename(&w, "manual").unwrap();
    assert_eq!(show(&s, &w, "#{?automatic-rename,on,off}"), "off");
    tx.reset_name(&w).unwrap();
    assert_eq!(show(&s, &w, "#{?automatic-rename,on,off}"), "on");
    let deadline = Instant::now() + Duration::from_secs(3);
    while tx.window_name(&w).unwrap() == "manual" {
        assert!(Instant::now() < deadline, "window never auto-renamed");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn reopen_failing_after_create_keeps_the_stack_popped() {
    let s = TestServer::start();
    let tx = Tx::new(s.socket.clone());
    let (store, sdir) = store();
    s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "alpha:",
        "-n",
        "victim",
        "/bin/sh",
    ]);
    s.wait_settled();
    tx.close_window(&wid(&s, "=alpha:victim"), None, &store)
        .unwrap();
    assert_eq!(store.len().unwrap(), 1);
    let count = || show(&s, "=alpha:", "#{session_windows}");
    assert_eq!(count(), "1");

    let r = tx.reopen_with(&store, |_, _, _| anyhow::bail!("injected"));
    assert!(
        r.is_ok(),
        "the window exists, so the reopen happened: {r:?}"
    );
    assert_eq!(count(), "2");
    assert!(store.is_empty().unwrap(), "the entry must stay popped");
    // the next ^t must not create a duplicate
    assert_eq!(tx.reopen(&store).unwrap(), None);
    assert_eq!(count(), "2");
    let _ = std::fs::remove_dir_all(sdir);
}

#[test]
fn busy_idle_shell_is_empty() {
    let s = TestServer::start();
    s.wait_settled();
    assert!(
        Tx::new(s.socket.clone())
            .busy_commands(&wid(&s, "alpha:"))
            .is_empty()
    );
}

#[test]
fn busy_lists_each_non_shell_command() {
    let s = TestServer::start();
    s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "alpha:",
        "-n",
        "qb-two",
        "sleep 1000",
    ]);
    s.tmux(&[
        "split-window",
        "-d",
        "-t",
        "alpha:qb-two",
        "tail -f /dev/null",
    ]);
    s.tmux(&["split-window", "-d", "-t", "alpha:qb-two", "sleep 1001"]);
    s.wait_settled();
    let got = Tx::new(s.socket.clone()).busy_commands(&wid(&s, "alpha:qb-two"));
    assert_eq!(got, ["sleep", "tail"]);
}

#[test]
fn busy_ignores_sidebar_panes() {
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qb-side"]);
    s.tmux(&[
        "split-window",
        "-d",
        "-h",
        "-t",
        "alpha:qb-side",
        "sleep 1000",
    ]);
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "alpha:qb-side.1",
        "@pane_role",
        "sidebar",
    ]);
    s.wait_settled();
    assert!(
        Tx::new(s.socket.clone())
            .busy_commands(&wid(&s, "alpha:qb-side"))
            .is_empty()
    );
}

/// ^z leaves the shell in the foreground: the pane looks idle, but the
/// stopped job would die with the window.
#[test]
fn busy_reports_a_stopped_job() {
    let s = TestServer::start();
    s.wait_settled();
    let w = wid(&s, "alpha:");
    s.tmux(&["send-keys", "-t", &w, "sleep 1000", "Enter"]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while show(&s, &w, "#{pane_current_command}") != "sleep" {
        assert!(Instant::now() < deadline, "sleep never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    s.tmux(&["send-keys", "-t", &w, "C-z"]);
    let tx = Tx::new(s.socket.clone());
    let deadline = Instant::now() + Duration::from_secs(3);
    while tx.busy_commands(&w) != ["sleep (stopped)"] {
        assert!(
            Instant::now() < deadline,
            "stopped job not reported: {:?}",
            tx.busy_commands(&w)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn rename_by_id_keeps_odd_characters() {
    let s = TestServer::start();
    let tx = Tx::new(s.socket.clone());
    let w = wid(&s, "alpha:");
    tx.rename(&w, "-dash (paren)+plus, comma 'q'").unwrap();
    assert_eq!(
        show(&s, &w, "#{window_name}"),
        "-dash (paren)+plus, comma 'q'"
    );
}

#[test]
fn rename_turns_automatic_rename_off() {
    let s = TestServer::start();
    let w = wid(&s, "alpha:");
    s.tmux(&["set-option", "-w", "-t", &w, "automatic-rename", "on"]);
    Tx::new(s.socket.clone()).rename(&w, "named").unwrap();
    assert_eq!(show(&s, &w, "#{?automatic-rename,on,off}"), "off");
}

/// The row's pane (and so the preview) is the main pane even when the
/// sidebar is active.
#[test]
fn capture_shows_the_main_pane_not_the_sidebar() {
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "logs"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "alpha:logs"]);
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "alpha:logs.1",
        "@pane_role",
        "sidebar",
    ]);
    s.tmux(&["select-pane", "-t", "alpha:logs.1"]);
    s.wait_settled();
    s.tmux(&[
        "send-keys",
        "-t",
        "alpha:logs.0",
        "echo MAIN-PANE-MARKER",
        "Enter",
    ]);
    s.tmux(&[
        "send-keys",
        "-t",
        "alpha:logs.1",
        "echo SIDEBAR-MARKER",
        "Enter",
    ]);
    let t = tmux_home::tmux::Tmux::new(s.socket.clone());
    let snap = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(tmux_home::tmux::snapshot::read_snapshot(&t))
        .unwrap()
        .0;
    let logs = wid(&s, "alpha:logs");
    let rows = tmux_home::popup::app::build_rows(&snap, None, "");
    let row = rows.iter().find(|r| r.wid == logs).unwrap();
    assert_eq!(
        row.pane.as_deref(),
        Some(show(&s, "alpha:logs.0", "#{pane_id}").as_str())
    );
    let tx = Tx::new(s.socket.clone());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut text = String::new();
    while text.matches("MAIN-PANE-MARKER").count() < 2 {
        assert!(Instant::now() < deadline, "{text:?}");
        std::thread::sleep(Duration::from_millis(50));
        text = tx.capture(row.pane.as_deref().unwrap());
    }
    assert!(!text.contains("SIDEBAR-MARKER"), "{text:?}");
    assert!(!text.ends_with('\n'), "trailing blank lines trimmed");
}

#[test]
fn swap_exchanges_places_and_keeps_the_current_window() {
    let s = TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "two"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "three"]);
    let (a, b, c) = (wid(&s, "alpha:0"), wid(&s, "alpha:1"), wid(&s, "alpha:2"));
    let tx = Tx::new(s.socket.clone());
    tx.swap(&a, &b).unwrap();
    assert_eq!(show(&s, &a, "#{window_index}"), "1");
    assert_eq!(show(&s, &b, "#{window_index}"), "0");
    assert_eq!(
        show(&s, &a, "#{window_active}"),
        "1",
        "current stays current"
    );
    // neither window current: the current one still stays
    tx.swap(&b, &c).unwrap();
    assert_eq!(show(&s, &c, "#{window_index}"), "0");
    assert_eq!(show(&s, &a, "#{window_active}"), "1");
}

#[test]
fn new_window_after_lands_next_with_its_name_and_cwd() {
    let s = TestServer::start();
    s.tmux(&["rename-window", "-t", "alpha:0", "first"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "last"]);
    let tx = Tx::new(s.socket.clone());
    let new = tx
        .new_window_after(&wid(&s, "alpha:first"), "/usr", "n #1")
        .unwrap();
    assert!(new.starts_with('@'), "{new}");
    let names = s.tmux(&["list-windows", "-t", "alpha", "-F", "#W"]);
    assert_eq!(names.lines().collect::<Vec<_>>(), ["first", "n #1", "last"]);
    assert_eq!(show(&s, &new, "#{pane_start_path}"), "/usr");
    assert_eq!(
        show(&s, "alpha:", "#{window_name}"),
        "first",
        "-d: the client's window is unchanged"
    );
    // empty name: tmux names it
    let unnamed = tx.new_window_after(&new, "/", "").unwrap();
    assert_eq!(show(&s, &unnamed, "#{window_index}"), "2");
}
