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
