mod common;
use std::time::Duration;
use tmux_home::tmux::{
    Tmux,
    source::{SourceEvent, SourceKind, start},
};

async fn next_snap(
    rx: &mut tokio::sync::mpsc::Receiver<SourceEvent>,
    within: Duration,
) -> Option<SourceEvent> {
    tokio::time::timeout(within, rx.recv()).await.ok().flatten()
}

async fn check_source(kind: SourceKind, max_latency: Duration) {
    let s = common::TestServer::start();
    // Settle: a freshly started pane's pane_current_command changes briefly
    // after start (macOS /bin/sh re-execs), which would otherwise change the
    // snapshot hash and produce a spurious event right after the initial one.
    s.wait_settled();
    let mut rx = start(kind, Tmux::new(s.socket.clone()));
    let Some(SourceEvent::Snapshot(first)) = next_snap(&mut rx, Duration::from_secs(2)).await
    else {
        panic!("no initial")
    };
    assert_eq!(first.windows.len(), 1);
    // no change -> no event
    assert!(
        next_snap(&mut rx, Duration::from_millis(700))
            .await
            .is_none()
    );
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "fresh"]);
    let Some(SourceEvent::Snapshot(snap)) = next_snap(&mut rx, max_latency).await else {
        panic!("no change seen")
    };
    assert!(snap.windows.iter().any(|w| w.name == "fresh"));
    s.tmux(&["kill-server"]);
    loop {
        match next_snap(&mut rx, Duration::from_secs(3)).await {
            Some(SourceEvent::Gone) => break,
            Some(SourceEvent::Snapshot(_)) => continue,
            None => panic!("source did not report Gone"),
        }
    }
}

#[tokio::test]
async fn poll_source() {
    check_source(SourceKind::Poll, Duration::from_millis(1200)).await;
}

#[tokio::test]
async fn control_source() {
    check_source(SourceKind::Control, Duration::from_millis(300)).await;
}

/// The control client's attached session, read from `list-clients`
/// (`#{client_flags}` identifies our control-mode client; everything after
/// the first space is its `#{session_name}`).
fn control_client_session(s: &common::TestServer) -> String {
    let clients = s.tmux(&["list-clients", "-F", "#{client_flags} #{session_name}"]);
    clients
        .lines()
        .find_map(|l| {
            let (flags, session) = l.split_once(' ')?;
            flags
                .split(',')
                .any(|f| f == "control-mode")
                .then(|| session.to_string())
        })
        .expect("no control-mode client found in list-clients")
}

#[tokio::test]
async fn control_sees_other_sessions_and_renames() {
    let s = common::TestServer::start();
    s.wait_settled();
    // `attach-session` with no `-t` attaches to the server's current/most
    // recently created session — "alpha" is the only one that exists yet,
    // so the control client lands there deterministically.
    let mut rx = start(SourceKind::Control, Tmux::new(s.socket.clone()));
    let _ = next_snap(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(
        control_client_session(&s),
        "alpha",
        "control client should be attached to alpha, not a session created afterwards"
    );

    s.tmux(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
    s.wait_settled();
    assert_eq!(
        control_client_session(&s),
        "alpha",
        "creating beta must not move the already-attached control client onto it"
    );

    // a window added to a session the control client is NOT attached to
    s.tmux(&["new-window", "-d", "-t", "beta", "-n", "elsewhere"]);
    s.wait_settled();
    let Some(SourceEvent::Snapshot(snap)) = next_snap(&mut rx, Duration::from_millis(300)).await
    else {
        panic!("missed other-session window")
    };
    assert!(snap.windows.iter().any(|w| w.name == "elsewhere"));
    s.tmux(&["rename-window", "-t", "beta:elsewhere", "renamed"]);
    let Some(SourceEvent::Snapshot(snap)) = next_snap(&mut rx, Duration::from_millis(300)).await
    else {
        panic!("missed rename")
    };
    assert!(snap.windows.iter().any(|w| w.name == "renamed"));
}

/// Drains events for `within`, failing on `Gone`, and returns the last snapshot seen.
async fn last_snap_no_gone(
    rx: &mut tokio::sync::mpsc::Receiver<SourceEvent>,
    within: Duration,
) -> Option<tmux_home::tmux::snapshot::Snapshot> {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = None;
    while let Ok(ev) = tokio::time::timeout_at(deadline, rx.recv()).await {
        match ev {
            Some(SourceEvent::Snapshot(s)) => last = Some(s),
            Some(SourceEvent::Gone) | None => {
                panic!("source reported Gone while the server is alive")
            }
        }
    }
    last
}

/// Asserts a window added in `session` shows up in the source within 300 ms.
async fn assert_sees_new_window(
    s: &common::TestServer,
    rx: &mut tokio::sync::mpsc::Receiver<SourceEvent>,
    session: &str,
    name: &str,
) {
    assert_sees_new_window_within(s, rx, session, name, Duration::from_millis(300)).await;
}

async fn assert_sees_new_window_within(
    s: &common::TestServer,
    rx: &mut tokio::sync::mpsc::Receiver<SourceEvent>,
    session: &str,
    name: &str,
    within: Duration,
) {
    s.tmux(&["new-window", "-d", "-t", session, "-n", name]);
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(SourceEvent::Snapshot(snap))) => {
                if snap.windows.iter().any(|w| w.name == name) {
                    return;
                }
            }
            Ok(Some(SourceEvent::Gone)) | Ok(None) => panic!("source Gone"),
            Err(_) => panic!("window {name} in {session} not seen within {within:?}"),
        }
    }
}

#[tokio::test]
async fn control_survives_its_session_being_killed() {
    let s = common::TestServer::start();
    s.tmux(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
    s.wait_settled();
    let mut rx = start(SourceKind::Control, Tmux::new(s.socket.clone()));
    let _ = next_snap(&mut rx, Duration::from_secs(2)).await;
    let joined = control_client_session(&s);
    let other = if joined == "alpha" { "beta" } else { "alpha" };

    // detach-on-destroy is on (the default): a normal client would be detached here
    s.tmux(&["kill-session", "-t", &joined]);
    let snap = last_snap_no_gone(&mut rx, Duration::from_secs(1))
        .await
        .expect("no snapshot after killing the control client's session");
    assert!(snap.sessions.iter().all(|x| x.name != joined));
    assert_eq!(control_client_session(&s), other);
    assert_sees_new_window(&s, &mut rx, other, "after-kill").await;
}

#[tokio::test]
async fn control_survives_being_detached() {
    let s = common::TestServer::start();
    s.wait_settled();
    let mut rx = start(SourceKind::Control, Tmux::new(s.socket.clone()));
    let _ = next_snap(&mut rx, Duration::from_secs(2)).await;
    // what `tmux attach -d -t alpha` does to every other client on alpha
    s.tmux(&["detach-client", "-s", "alpha"]);
    let _ = last_snap_no_gone(&mut rx, Duration::from_secs(1)).await;
    assert_eq!(control_client_session(&s), "alpha");
    assert_sees_new_window(&s, &mut rx, "alpha", "after-detach").await;
}

/// A server with no sessions (tmux.conf time, or after the last session is
/// killed under `exit-empty off`) is alive: the source reports an empty
/// snapshot, then pushes the first session once it exists.
async fn check_empty_server(kind: SourceKind, max_latency: Duration) {
    let s = common::TestServer::start_empty();
    let mut rx = start(kind, Tmux::new(s.socket.clone()));
    let Some(SourceEvent::Snapshot(first)) = next_snap(&mut rx, Duration::from_secs(2)).await
    else {
        panic!("no initial snapshot from a server without sessions")
    };
    assert!(first.sessions.is_empty() && first.panes.is_empty());
    s.tmux(&["new-session", "-d", "-s", "late", "/bin/sh"]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(SourceEvent::Snapshot(snap)))
                if snap.sessions.iter().any(|x| x.name == "late") =>
            {
                break;
            }
            Ok(Some(SourceEvent::Snapshot(_))) => continue,
            Ok(Some(SourceEvent::Gone)) | Ok(None) => panic!("source Gone on a live server"),
            Err(_) => panic!("new session not pushed"),
        }
    }
    // and the source keeps tracking afterwards (control: now attached)
    assert_sees_new_window_within(&s, &mut rx, "late", "after-empty", max_latency).await;
    // last session gone again, server still up: back to an empty snapshot
    s.tmux(&["kill-session", "-t", "late"]);
    let snap = last_snap_no_gone(&mut rx, Duration::from_secs(1))
        .await
        .expect("no snapshot after the last session was killed");
    assert!(snap.sessions.is_empty());
    s.tmux(&["kill-server"]);
    loop {
        match next_snap(&mut rx, Duration::from_secs(3)).await {
            Some(SourceEvent::Gone) => break,
            Some(SourceEvent::Snapshot(_)) => continue,
            None => panic!("source did not report Gone"),
        }
    }
}

#[tokio::test]
async fn poll_source_empty_server() {
    check_empty_server(SourceKind::Poll, Duration::from_millis(1200)).await;
}

#[tokio::test]
async fn control_source_empty_server() {
    check_empty_server(SourceKind::Control, Duration::from_millis(300)).await;
}
