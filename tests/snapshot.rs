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
    let opts = "\x1f".repeat(tmux_home::agent::OPTIONS.len());
    let good = format!(
        "$0\x1f@0\x1f0\x1f1\x1f0\x1f%0\x1f0\x1f1\x1fsh\x1f/\x1f\x1f/dev/ttys9\x1f\x1f0\x1f0\x1f80\x1f24\x1f80\x1f24\x1f1\x1f\x1f4242\x1f{opts}alpha\x1fmy pane\x1ft\x1fw\x1e\n"
    );
    let panes = format!("garbage\x1e\n{good}");
    let snap = tmux_home::tmux::snapshot::parse(&panes, "also garbage\x1e\n");
    assert_eq!(snap.panes.len(), 1);
    assert_eq!(snap.panes[0].tty, "/dev/ttys9");
    assert_eq!((snap.panes[0].width, snap.panes[0].height), (80, 24));
    assert_eq!(snap.windows[0].name, "w");
    assert_eq!(snap.panes[0].pid, 4242);
    assert_eq!(
        (snap.panes[0].name.as_str(), snap.panes[0].title.as_str()),
        ("my pane", "t")
    );
    assert!(snap.sessions[0].pinned, "@home_sidebar_pin");
    assert!(!snap.windows[0].sidebar_off, "@home_sidebar_off");
    assert!(snap.clients.is_empty());
}

/// A session's pin is its own (a linked window's panes, listed once per
/// session, see each session's); a window's off mark is the window's.
#[test]
fn sidebar_pin_and_off_marks_are_read() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    s.tmux(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"]);
    s.tmux(&["link-window", "-d", "-s", "alpha:0", "-t", "beta:5"]);
    s.tmux(&["set-option", "-t", "alpha", "@home_sidebar_pin", "1"]);
    s.tmux(&["set-option", "-w", "-t", "beta:0", "@home_sidebar_off", "1"]);
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    let pinned: Vec<(&str, bool)> = snap
        .sessions
        .iter()
        .map(|x| (x.name.as_str(), x.pinned))
        .collect();
    assert_eq!(pinned, [("alpha", true), ("beta", false)]);
    let off: Vec<(u32, bool)> = snap
        .windows
        .iter()
        .map(|w| (w.index, w.sidebar_off))
        .collect();
    assert_eq!(off, [(0, false), (0, true), (5, false)]);
}

#[test]
fn server_without_sessions_is_an_empty_snapshot() {
    let s = common::TestServer::start_empty();
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    assert_eq!(snap, tmux_home::tmux::snapshot::Snapshot::default());
}

/// The agent options are read per pane (empty ones dropped); a run's start
/// time is stored, so the hash holds still while a run goes on and moves
/// only when an option changes.
#[test]
fn agent_options_are_read_and_hashed() {
    let s = common::TestServer::start();
    let t = Tmux::new(s.socket.clone());
    s.wait_settled();
    let pane = s.tmux(&["display", "-p", "-t", "alpha:0", "#{pane_id}"]);
    let pane = pane.trim();
    for (k, v) in [
        ("@pane_agent", "claude"),
        ("@pane_status", "running"),
        ("@pane_started_at", "1790864748"),
        ("@pane_prompt", "fix | the \x1e build"),
    ] {
        s.tmux(&["set-option", "-p", "-t", pane, k, v]);
    }
    let snap = read_snapshot(&t).unwrap();
    let p = &snap.panes[0];
    assert_eq!(p.agent_opts.get("@pane_status").unwrap(), "running");
    assert_eq!(
        p.agent_opts.get("@pane_prompt").unwrap(),
        "fix | the \x1e build"
    );
    assert!(!p.agent_opts.contains_key("@pane_attention"));
    let h1 = snap.sections();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(read_snapshot(&t).unwrap().sections(), h1, "no tick");
    s.tmux(&["set-option", "-p", "-t", pane, "@pane_status", "waiting"]);
    assert_ne!(read_snapshot(&t).unwrap().sections(), h1);
}

/// Each pane's place in its window, and the window's size: what the
/// popup's preview lays the window out from.
#[test]
fn pane_geometry_is_read() {
    let s = common::TestServer::start();
    s.tmux(&["resize-window", "-t", "alpha:0", "-x", "100", "-y", "30"]);
    s.tmux(&["split-window", "-d", "-h", "-l", "40", "-t", "alpha:0"]);
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    let mut panes: Vec<_> = snap.panes.iter().collect();
    panes.sort_by_key(|p| p.index);
    let geo: Vec<_> = panes
        .iter()
        .map(|p| (p.left, p.top, p.width, p.height))
        .collect();
    assert_eq!(geo, [(0, 0, 59, 30), (60, 0, 40, 30)]);
    assert!(
        panes
            .iter()
            .all(|p| (p.window_width, p.window_height) == (100, 30))
    );
}
