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
    // Settle: a freshly started pane's pane_current_command changes for ~300ms
    // after start (macOS /bin/sh re-execs), which would otherwise change the
    // snapshot hash and produce a spurious event right after the initial one.
    tokio::time::sleep(Duration::from_millis(600)).await;
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
