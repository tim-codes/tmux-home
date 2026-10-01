mod common;
use tmux_home::tmux::{Tmux, snapshot::read_snapshot};

#[test]
fn reads_sessions_windows_panes() {
    let s = common::TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "two"]);
    s.tmux(&["split-window", "-d", "-t", "alpha:two"]);
    s.tmux(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    let names: Vec<_> = snap.sessions.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert_eq!(snap.windows.len(), 3);
    let two = snap.windows.iter().find(|w| w.name == "two").unwrap();
    assert_eq!(
        snap.panes.iter().filter(|p| p.window_id == two.id).count(),
        2
    );
    assert!(snap.windows.iter().all(|w| w.id.starts_with('@')));
    assert!(snap.panes.iter().all(|p| p.id.starts_with('%')));
}

#[test]
fn odd_names_round_trip() {
    let s = common::TestServer::start();
    // tmux 3.7c's rename-window rejects embedded control characters (e.g. a
    // literal tab) outright with "invalid window name", so this exercises
    // the other odd cases the format string must still round-trip correctly:
    // a colon (format-field-looking), a space, and non-ASCII text.
    let name = "a:b ç ✳ x";
    s.tmux(&["rename-window", "-t", "alpha:0", name]);
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    assert_eq!(snap.windows[0].name, name);
    assert!(!snap.windows[0].automatic_rename);
}

#[test]
fn hash_changes_only_on_change() {
    let s = common::TestServer::start();
    let t = Tmux::new(s.socket.clone());
    // Give the freshly spawned pane's shell a moment to settle: tmux briefly
    // reports a transitional #{pane_current_command} (e.g. "zsh" before
    // "bash") right after spawn, which is a real, tmux-reported state change
    // and not a bug in read_snapshot — just not what this test is about.
    s.wait_settled();
    let h1 = read_snapshot(&t).unwrap().sections();
    let h2 = read_snapshot(&t).unwrap().sections();
    assert_eq!(h1, h2);
    s.tmux(&["new-window", "-d", "-t", "alpha"]);
    let h3 = read_snapshot(&t).unwrap().sections();
    assert_ne!(h1, h3);
}

#[test]
fn dead_server_is_an_error() {
    let s = common::TestServer::start();
    let t = Tmux::new(s.socket.clone());
    s.tmux(&["kill-server"]);
    assert!(read_snapshot(&t).is_err());
}

#[test]
fn newline_in_pane_path_round_trips() {
    let env = common::TestEnv::new();
    let s = common::TestServer::start();
    let dir = env.state.join("nl\ndir");
    std::fs::create_dir_all(&dir).unwrap();
    // pane_current_path is the shell's cwd as the OS reports it, i.e. with
    // symlinks resolved (macOS: /var -> /private/var).
    let dir = dir.canonicalize().unwrap();
    s.tmux(&["new-window", "-d", "-n", "nl", "-c", dir.to_str().unwrap()]);
    s.wait_settled();
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    let nl = snap.windows.iter().find(|w| w.name == "nl").unwrap();
    let pane = snap.panes.iter().find(|p| p.window_id == nl.id).unwrap();
    assert_eq!(pane.current_path, dir.to_str().unwrap());
    assert_eq!(snap.windows.len(), 2);
}

#[test]
fn unparseable_record_is_skipped() {
    let good = "$0\x1f@0\x1f0\x1f1\x1f0\x1f%0\x1f0\x1f1\x1fsh\x1f/\x1f\x1falpha\x1ft\x1fw\x1e\n";
    let panes = format!("garbage\x1e\n{good}");
    let snap = tmux_home::tmux::snapshot::parse(&panes, "also garbage\x1e\n");
    assert_eq!(snap.panes.len(), 1);
    assert!(snap.clients.is_empty());
}

#[test]
fn server_without_sessions_is_an_empty_snapshot() {
    let s = common::TestServer::start_empty();
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    assert_eq!(snap, tmux_home::tmux::snapshot::Snapshot::default());
}
