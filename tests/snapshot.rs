mod common;
use tmux_home::tmux::{Tmux, snapshot::read_snapshot};

#[tokio::test]
async fn reads_sessions_windows_panes() {
    let s = common::TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "two"]);
    s.tmux(&["split-window", "-d", "-t", "alpha:two"]);
    s.tmux(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
    let (snap, _) = read_snapshot(&Tmux::new(s.socket.clone())).await.unwrap();
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

#[tokio::test]
async fn odd_names_round_trip() {
    let s = common::TestServer::start();
    // tmux 3.7c's rename-window rejects embedded control characters (e.g. a
    // literal tab) outright with "invalid window name", so this exercises
    // the other odd cases the format string must still round-trip correctly:
    // a colon (format-field-looking), a space, and non-ASCII text.
    let name = "a:b ç ✳ x";
    s.tmux(&["rename-window", "-t", "alpha:0", name]);
    let (snap, _) = read_snapshot(&Tmux::new(s.socket.clone())).await.unwrap();
    assert_eq!(snap.windows[0].name, name);
    assert!(!snap.windows[0].automatic_rename);
}

#[tokio::test]
async fn hash_changes_only_on_change() {
    let s = common::TestServer::start();
    let t = Tmux::new(s.socket.clone());
    // Give the freshly spawned pane's shell a moment to settle: tmux briefly
    // reports a transitional #{pane_current_command} (e.g. "zsh" before
    // "bash") right after spawn, which is a real, tmux-reported state change
    // and not a bug in read_snapshot — just not what this test is about.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (_, h1) = read_snapshot(&t).await.unwrap();
    let (_, h2) = read_snapshot(&t).await.unwrap();
    assert_eq!(h1, h2);
    s.tmux(&["new-window", "-d", "-t", "alpha"]);
    let (_, h3) = read_snapshot(&t).await.unwrap();
    assert_ne!(h1, h3);
}

#[tokio::test]
async fn dead_server_is_an_error() {
    let s = common::TestServer::start();
    let t = Tmux::new(s.socket.clone());
    s.tmux(&["kill-server"]);
    assert!(read_snapshot(&t).await.is_err());
}
