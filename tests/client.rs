mod common;

use std::time::Duration;
use tmux_home::{
    VERSION,
    ipc::{Reply, Request, read_msg, write_msg},
};

#[tokio::test]
async fn degraded_then_daemon() {
    // SAFETY: tests that touch process env run single-threaded per process
    // (see Cargo.toml [[test]] harness note), same pattern as TestEnv.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"));
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    // no daemon yet: direct read, and a daemon gets started in the background
    let (snap, from_daemon) = tmux_home::client::snapshot(&s.socket, Duration::from_millis(150))
        .await
        .unwrap();
    assert_eq!(snap.windows.len(), 1);
    assert!(!from_daemon);
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if tmux_home::client::snapshot(&s.socket, Duration::from_millis(150))
            .await
            .unwrap()
            .1
        {
            ok = true;
            break;
        }
    }
    assert!(ok, "background daemon never came up");
    s.tmux(&["kill-server"]); // daemon exits with it (covered in daemon tests)
}

#[test]
fn query_cli_prints_json() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tmux-home"))
        .args(["query", "--json", "--socket"])
        .arg(&s.socket)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["sessions"][0]["name"], "alpha");
    s.tmux(&["kill-server"]);
}

/// Exercises the Restart race ruling: sends a version-mismatched request to
/// a real, running daemon (via `tmux_home::daemon::run`), which replies
/// `Restart`, removes its socket, and exits (releasing its flock) shortly
/// after. `client::snapshot` must wait for that socket to disappear before
/// spawning a replacement daemon, rather than racing it — otherwise the
/// replacement's `try_lock` can fail against the still-held flock and it
/// exits silently, leaving no daemon running.
#[tokio::test]
async fn restart_reply_does_not_race_old_daemon_exit() {
    // SAFETY: see degraded_then_daemon above.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"));
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();

    // Start a real daemon in-process, at this test binary's VERSION.
    let socket = s.socket.clone();
    tokio::spawn(async move {
        let _ = tmux_home::daemon::run(socket, tmux_home::tmux::source::SourceKind::Poll).await;
    });

    // Give it a moment to bind its socket, then provoke a Restart reply by
    // sending a request carrying a different version.
    let paths = tmux_home::paths::Paths::for_socket(&s.socket).unwrap();
    let mut got_restart = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let Ok(stream) = tokio::net::UnixStream::connect(&paths.sock).await else {
            continue;
        };
        let (r, mut w) = stream.into_split();
        if write_msg(
            &mut w,
            &Request::Query {
                v: "0.0.0-old".into(),
            },
        )
        .await
        .is_err()
        {
            continue;
        }
        let mut r = tokio::io::BufReader::new(r);
        if let Ok(Some(Reply::Restart)) = read_msg::<_, Reply>(&mut r).await {
            got_restart = true;
            break;
        }
    }
    assert!(
        got_restart,
        "daemon never replied Restart to a version mismatch"
    );
    assert_ne!(VERSION, "0.0.0-old");

    // client::snapshot should ride out the race: wait for the old daemon's
    // socket to go, spawn a replacement, and eventually a daemon (the
    // replacement) serves this server again.
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if tmux_home::client::snapshot(&s.socket, Duration::from_millis(150))
            .await
            .unwrap()
            .1
        {
            ok = true;
            break;
        }
    }
    assert!(ok, "replacement daemon never came up after Restart");
    s.tmux(&["kill-server"]);
}

#[test]
fn query_cli_on_server_without_sessions() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start_empty();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tmux-home"))
        .args(["query", "--socket"])
        .arg(&s.socket)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["sessions"], serde_json::json!([]));
    s.tmux(&["kill-server"]);
}
