mod common;

use std::time::Duration;
use tmux_home::{
    BUILD_ID, VERSION,
    client::{Answer, ask},
    ipc::Request,
};

/// `client::snapshot` until it is served by a daemon, for up to ~5 s.
fn wait_daemon_serving(s: &common::TestServer) -> bool {
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if tmux_home::client::snapshot(&s.socket, Duration::from_millis(150))
            .unwrap()
            .1
        {
            return true;
        }
    }
    false
}

#[test]
fn degraded_then_daemon() {
    // SAFETY: tests that touch process env run single-threaded per process
    // (RUST_TEST_THREADS=1 in .cargo/config.toml), same pattern as TestEnv.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"));
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    // no daemon yet: direct read, and a daemon gets started in the background
    let (snap, from_daemon) =
        tmux_home::client::snapshot(&s.socket, Duration::from_millis(150)).unwrap();
    assert_eq!(snap.windows.len(), 1);
    assert!(!from_daemon);
    assert!(wait_daemon_serving(&s), "background daemon never came up");
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

/// A daemon of another *build* of the same package version (a rebuild):
/// starts in-process answering as that build, and waits until it serves.
fn old_build_daemon(
    s: &common::TestServer,
) -> (
    tokio::runtime::Runtime,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let old = format!("{VERSION}+g000000000000");
    assert_ne!(BUILD_ID, old);
    assert!(BUILD_ID.starts_with(&format!("{VERSION}+")));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let d = rt.spawn(tmux_home::daemon::run_with_version(
        s.socket.clone(),
        tmux_home::tmux::source::SourceKind::Poll,
        old.clone(),
    ));
    // Wait until it is up and serving (a request as *its* build gets a
    // snapshot, not a Restart), so the client certainly reaches it rather
    // than falling into the no-daemon arm.
    let req = Request::Query { v: old };
    let mut up = false;
    for _ in 0..100 {
        if let Answer::Snapshot { .. } = ask(&s.socket, &req, Duration::from_millis(500)) {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(up, "old-build daemon never came up");
    (rt, d)
}

/// Pass 3 exit test: a rebuild at the same package version replaces the
/// running daemon. The old build's daemon replies `Restart` to this build's
/// ID, removes its socket and exits; `client::snapshot` waits for that
/// socket to go before spawning the replacement (or the replacement's
/// `try_lock` can lose to the old flock and exit silently), returns
/// degraded data, and the new build's daemon is serving within ~5 s.
#[test]
fn rebuild_at_the_same_version_replaces_the_daemon() {
    // SAFETY: see degraded_then_daemon above.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"));
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let (rt, old) = old_build_daemon(&s);
    let (snap, from_daemon) =
        tmux_home::client::snapshot(&s.socket, Duration::from_millis(500)).unwrap();
    assert!(
        !from_daemon,
        "a Restart reply must fall back to a direct read"
    );
    assert_eq!(snap.windows.len(), 1);
    // The old daemon only exits on Restart (or its server going, which it
    // hasn't): its having exited shows the Restart arm ran.
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), old).await })
        .expect("old daemon should exit after replying Restart")
        .unwrap()
        .unwrap();
    assert!(wait_daemon_serving(&s), "new build's daemon never came up");
    s.tmux(&["kill-server"]);
}

/// Runs `tmux-home status` (a dev build) with `respawn` as
/// TMUX_HOME_STATUS_RESPAWN (unset if `None`).
///
/// It runs from a copy outside `target/`: `status` decides from its own
/// path whether it is the plugin's build (`…/target/release/tmux-home`),
/// and under `cargo test --release` the test binary is exactly that.
fn status(s: &common::TestServer, respawn: Option<&str>) -> String {
    use std::sync::OnceLock;
    static DEV: OnceLock<std::path::PathBuf> = OnceLock::new();
    let dev = DEV.get_or_init(|| {
        let d = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dev-build");
        std::fs::create_dir_all(&d).unwrap();
        let tmp = d.join(format!("tmux-home.{}", std::process::id()));
        std::fs::copy(env!("CARGO_BIN_EXE_tmux-home"), &tmp).unwrap();
        let bin = d.join("tmux-home");
        std::fs::rename(&tmp, &bin).unwrap();
        bin
    });
    let mut c = std::process::Command::new(dev);
    c.args(["status", "--socket"]).arg(&s.socket);
    match respawn {
        Some(v) => c.env("TMUX_HOME_STATUS_RESPAWN", v),
        None => c.env_remove("TMUX_HOME_STATUS_RESPAWN"),
    };
    let out = c.output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// `status` shows ○ for a daemon of an old build, and (allowed to) respawns
/// the current build's daemon there and then, so the chip is ● again on
/// its next run.
#[test]
fn status_respawns_a_daemon_after_a_restart() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let (rt, old) = old_build_daemon(&s);
    assert_eq!(status(&s, Some("1")), "○");
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), old).await })
        .expect("old daemon should exit after replying Restart")
        .unwrap()
        .unwrap();
    common::wait_until("status ● again", || status(&s, Some("1")) == "●");
    s.tmux(&["kill-server"]);
}

/// A dev build's `status` doesn't replace another build's daemon: no
/// ping-pong with the plugin's binary on a live server.
#[test]
fn a_dev_build_status_does_not_respawn() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let (rt, old) = old_build_daemon(&s);
    assert_eq!(status(&s, None), "○");
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), old).await })
        .expect("old daemon should exit after replying Restart")
        .unwrap()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(status(&s, None), "○", "no daemon was started");
    s.tmux(&["kill-server"]);
}

/// Starting the daemon is best-effort: a client whose daemon can't be
/// spawned still gets its degraded direct read.
#[test]
fn spawn_failure_does_not_abort_degraded_read() {
    // SAFETY: see degraded_then_daemon above.
    unsafe {
        std::env::set_var("TMUX_HOME_BIN", "/nonexistent/tmux-home");
    }
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let (snap, from_daemon) =
        tmux_home::client::snapshot(&s.socket, Duration::from_millis(150)).unwrap();
    assert!(!from_daemon);
    assert_eq!(snap.windows.len(), 1);
    s.tmux(&["kill-server"]);
}

/// The spawned daemon's stderr goes to `<state_dir>/daemon.log`, so its
/// errors aren't lost. A daemon pointed at a dead server logs why its
/// source ended.
#[test]
fn spawned_daemon_logs_to_state_dir() {
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
        std::thread::sleep(Duration::from_millis(100));
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
