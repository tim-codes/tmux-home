mod common;

use std::time::Duration;
use tmux_home::{
    VERSION,
    ipc::{Reply, Request, read_msg, write_msg},
};

#[tokio::test]
async fn degraded_then_daemon() {
    // SAFETY: tests that touch process env run single-threaded per process
    // (RUST_TEST_THREADS=1 in .cargo/config.toml), same pattern as TestEnv.
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

/// Exercises the client's `Reply::Restart` arm against a daemon that is up
/// and answering: an in-process daemon built at a *different* version
/// replies `Restart` to this client's `VERSION`, removes its socket and
/// exits. `client::snapshot` must wait for that socket to go before spawning
/// the replacement (or the replacement's `try_lock` can lose to the old
/// flock and exit silently), return degraded data, and a real daemon must be
/// serving within ~5 s.
#[tokio::test]
async fn restart_reply_spawns_replacement_daemon() {
    // SAFETY: see degraded_then_daemon above.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"));
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    const OLD: &str = "0.0.0-old";
    assert_ne!(VERSION, OLD);
    let old = tokio::spawn(tmux_home::daemon::run_with_version(
        s.socket.clone(),
        tmux_home::tmux::source::SourceKind::Poll,
        OLD,
    ));

    // Wait until the old daemon is up and serving (a request at *its*
    // version gets a snapshot, not a Restart), so the client below
    // certainly reaches it rather than falling into the no-daemon arm.
    let paths = tmux_home::paths::Paths::for_socket(&s.socket).unwrap();
    let mut up = false;
    for _ in 0..100 {
        if let Ok(stream) = tokio::net::UnixStream::connect(&paths.sock).await {
            let (r, mut w) = stream.into_split();
            write_msg(&mut w, &Request::Query { v: OLD.into() })
                .await
                .unwrap();
            let mut r = tokio::io::BufReader::new(r);
            if let Ok(Some(Reply::Snapshot { .. })) = read_msg::<_, Reply>(&mut r).await {
                up = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(up, "old-version daemon never came up");

    let (snap, from_daemon) = tmux_home::client::snapshot(&s.socket, Duration::from_millis(500))
        .await
        .unwrap();
    assert!(
        !from_daemon,
        "a Restart reply must fall back to a direct read"
    );
    assert_eq!(snap.windows.len(), 1);
    // The old daemon only exits on Restart (or its server going, which it
    // hasn't): its having exited shows the Restart arm ran.
    tokio::time::timeout(Duration::from_secs(2), old)
        .await
        .expect("old daemon should exit after replying Restart")
        .unwrap()
        .unwrap();

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

/// Starting the daemon is best-effort: a client whose daemon can't be
/// spawned still gets its degraded direct read.
#[tokio::test]
async fn spawn_failure_does_not_abort_degraded_read() {
    // SAFETY: see degraded_then_daemon above.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", "/nonexistent/tmux-home");
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let (snap, from_daemon) = tmux_home::client::snapshot(&s.socket, Duration::from_millis(150))
        .await
        .unwrap();
    assert!(!from_daemon);
    assert_eq!(snap.windows.len(), 1);
    s.tmux(&["kill-server"]);
}

/// The spawned daemon's stderr goes to `<state_dir>/daemon.log`, so its
/// errors aren't lost. A daemon pointed at a dead server logs why its
/// source ended.
#[tokio::test]
async fn spawned_daemon_logs_to_state_dir() {
    // SAFETY: see degraded_then_daemon above.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"));
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    s.tmux(&["kill-server"]);
    tmux_home::client::spawn_daemon(&s.socket).unwrap();
    let p = tmux_home::paths::Paths::for_socket(&s.socket).unwrap();
    let log = p.state_dir.join("daemon.log");
    let mut text = String::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("source ended") {
            break;
        }
    }
    assert!(text.contains("source ended"), "daemon.log: {text:?}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&p.state_dir)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
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
