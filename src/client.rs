//! Talking to the daemon, and what to do when it can't serve: the one
//! spawn/degraded path, shared by `query`, `status` and the popup. All of it
//! is synchronous (std sockets, std processes): only the daemon runs tokio.

use crate::{
    BUILD_ID,
    ipc::{Reply, Request, read_msg_sync, write_msg_sync},
    paths::Paths,
    tmux::{
        Tmux,
        snapshot::{Snapshot, read_snapshot},
    },
};
use std::{
    io::BufReader,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

/// How long an old daemon's socket is given to disappear after a `Restart`
/// reply before we give up waiting and spawn a replacement anyway. The old
/// daemon removes its socket and *then* releases its flock, so once the
/// socket is gone the lock is free (or about to be) and a replacement's
/// `try_lock` won't lose to it.
const RESTART_SOCKET_GONE_TIMEOUT: Duration = Duration::from_millis(1000);
const RESTART_SOCKET_POLL: Duration = Duration::from_millis(20);

/// `daemon.log` is moved to `daemon.log.1` once it reaches this size (checked
/// when a daemon is spawned).
pub const LOG_CAP: u64 = 1 << 20;

/// Resolve the tmux socket to target: `explicit` (`--socket`) if given, else
/// the first comma-separated field of `$TMUX` (set inside a tmux client).
pub fn current_socket(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
        let t = std::env::var("TMUX").ok()?;
        let s = t.split(',').next()?.to_string();
        (!s.is_empty()).then(|| PathBuf::from(s))
    })
}

pub fn query_req() -> Request {
    Request::Query { v: BUILD_ID.into() }
}

pub fn refresh_req() -> Request {
    Request::Refresh { v: BUILD_ID.into() }
}

pub fn subscribe_req(client: &str) -> Request {
    Request::Subscribe {
        v: BUILD_ID.into(),
        client: client.into(),
    }
}

/// A daemon connection that has delivered its first reply; a subscription
/// reads its pushes from it.
pub struct Conn {
    r: BufReader<UnixStream>,
}

impl Conn {
    /// The next reply (blocking); `None` when the daemon closed.
    pub fn recv(&mut self) -> anyhow::Result<Option<Reply>> {
        read_msg_sync(&mut self.r)
    }
}

/// The daemon's first answer to a request.
// a one-shot return value, matched at once: boxing the snapshot buys nothing
#[allow(clippy::large_enum_variant)]
pub enum Answer {
    Snapshot {
        epoch: u64,
        seq: u64,
        data: Snapshot,
        conn: Conn,
    },
    /// A daemon of another build answered; it is exiting.
    Restart,
    /// No daemon, no reply within the budget, or an error reply.
    Down,
}

/// Sends `req` and waits up to `budget` for the first reply.
pub fn ask(tmux_socket: &Path, req: &Request, budget: Duration) -> Answer {
    let Ok(p) = Paths::for_socket(tmux_socket) else {
        return Answer::Down;
    };
    let first = || -> anyhow::Result<(Option<Reply>, UnixStream)> {
        let s = UnixStream::connect(&p.sock)?;
        s.set_write_timeout(Some(budget))?;
        s.set_read_timeout(Some(budget))?;
        write_msg_sync(&mut &s, req)?;
        let mut r = BufReader::new(s.try_clone()?);
        let reply = read_msg_sync(&mut r)?;
        // a subscription then waits for pushes indefinitely (macOS refuses
        // this with EINVAL once the daemon has closed a one-shot reply)
        let _ = s.set_read_timeout(None);
        Ok((reply, s))
    };
    match first() {
        Ok((Some(Reply::Snapshot { epoch, seq, data }), s)) => Answer::Snapshot {
            epoch,
            seq,
            data,
            conn: Conn {
                r: BufReader::new(s),
            },
        },
        Ok((Some(Reply::Restart), _)) => Answer::Restart,
        _ => Answer::Down,
    }
}

/// Spawn a detached daemon for `tmux_socket`. Uses `TMUX_HOME_BIN` if set
/// (tests: `current_exe()` is the test binary, not `tmux-home`), else the
/// current executable. The daemon's stderr is appended to
/// `<state_dir>/daemon.log` (state dir created 0700) so its errors survive;
/// if that can't be opened, stderr is discarded rather than failing the spawn.
pub fn spawn_daemon(tmux_socket: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let bin = std::env::var_os("TMUX_HOME_BIN")
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)?;
    let stderr = match daemon_log(tmux_socket) {
        Ok(f) => Stdio::from(f),
        Err(e) => {
            eprintln!("tmux-home: daemon log unavailable: {e:#}");
            Stdio::null()
        }
    };
    std::process::Command::new(bin)
        .arg("daemon")
        .arg("--socket")
        .arg(tmux_socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// Moves `log` to `<log>.1` (replacing an older one) once it holds `cap`
/// bytes or more.
pub fn rotate_log(log: &Path, cap: u64) {
    if std::fs::metadata(log).is_ok_and(|m| m.len() >= cap) {
        let mut old = log.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(log, old);
    }
}

/// Opens (append, create) `<state_dir>/daemon.log`, creating the state dir
/// 0700 and rotating a log that has reached `LOG_CAP`.
fn daemon_log(tmux_socket: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = Paths::for_socket(tmux_socket)?.state_dir;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let log = dir.join("daemon.log");
    rotate_log(&log, LOG_CAP);
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?)
}

/// Wait for `sock` to stop existing (following a `Restart` reply, an old
/// daemon removes its socket and then releases its flock), polling every 20ms up
/// to `RESTART_SOCKET_GONE_TIMEOUT`. Never loops forever: gives up and
/// returns after the cap even if the socket is still there, so a wedged old
/// daemon can't block the degraded read past that bound.
fn wait_for_socket_gone(sock: &Path) {
    let deadline = Instant::now() + RESTART_SOCKET_GONE_TIMEOUT;
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(RESTART_SOCKET_POLL);
    }
}

/// Best-effort start of a daemon after `ask` found none usable. After a
/// `Restart`, waits for the old daemon's socket to go first: it still holds
/// the flock briefly after replying, and a replacement spawned too early
/// fails `try_lock` and exits silently, leaving no daemon. A failure is
/// reported on stderr and never aborts the caller's degraded read.
pub fn revive(tmux_socket: &Path, after_restart: bool) {
    if after_restart && let Ok(p) = Paths::for_socket(tmux_socket) {
        wait_for_socket_gone(&p.sock);
    }
    if let Err(e) = spawn_daemon(tmux_socket) {
        eprintln!("tmux-home: could not start daemon: {e:#}");
    }
}

/// The daemon's snapshot within `budget`; else (re)start a daemon in the
/// background and read tmux directly, returning `from_daemon = false`.
pub fn snapshot(tmux_socket: &Path, budget: Duration) -> anyhow::Result<(Snapshot, bool)> {
    match ask(tmux_socket, &query_req(), budget) {
        Answer::Snapshot { data, .. } => return Ok((data, true)),
        Answer::Restart => revive(tmux_socket, true),
        Answer::Down => revive(tmux_socket, false),
    }
    let snap = read_snapshot(&Tmux::new(tmux_socket.to_path_buf()))?;
    Ok((snap, false))
}

pub fn query(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let sock =
        current_socket(socket).ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let (snap, _) = snapshot(&sock, Duration::from_millis(150))?;
    println!("{}", serde_json::to_string_pretty(&snap)?);
    Ok(())
}

/// Whether `status` (running as `exe`) starts a daemon after a `Restart`
/// reply. `TMUX_HOME_STATUS_RESPAWN` (`env`) decides when set (`0`: no);
/// otherwise only the plugin's own build (`…/target/release/tmux-home`)
/// does. A dev build run against the live server would otherwise replace
/// the plugin's daemon, whose next status call replaces it back, and so on.
pub fn respawn_on_restart(exe: &Path, env: Option<&str>) -> bool {
    match env {
        Some(v) => v != "0",
        None => exe.ends_with("target/release/tmux-home"),
    }
}

/// `tmux-home status`: `●` if the daemon answers within 100 ms, else `○`;
/// nothing outside tmux. A daemon that is down stays down (the status line
/// must not start one), but one replaced by a newer build (`Restart`) is
/// respawned at once (see `respawn_on_restart`), so the chip shows `○` only
/// until its next refresh.
pub fn status(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let Some(socket) = current_socket(socket) else {
        return Ok(());
    };
    let up = match ask(&socket, &query_req(), Duration::from_millis(100)) {
        Answer::Snapshot { .. } => true,
        Answer::Restart => {
            let exe = std::env::current_exe().unwrap_or_default();
            let env = std::env::var("TMUX_HOME_STATUS_RESPAWN").ok();
            if respawn_on_restart(&exe, env.as_deref()) {
                revive(&socket, true);
            }
            false
        }
        Answer::Down => false,
    };
    println!("{}", if up { "●" } else { "○" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_plugin_build_respawns_from_status() {
        let release = Path::new("/p/tmux-home/target/release/tmux-home");
        let dev = Path::new("/p/tmux-home-mvp/target/debug/tmux-home");
        assert!(respawn_on_restart(release, None));
        assert!(!respawn_on_restart(dev, None));
        assert!(respawn_on_restart(dev, Some("1")));
        assert!(!respawn_on_restart(release, Some("0")));
    }

    #[test]
    fn log_rotates_at_the_cap() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("daemon.log");
        rotate_log(&log, 10); // missing: nothing to do
        assert!(!log.exists());
        std::fs::write(&log, b"123456789").unwrap();
        rotate_log(&log, 10);
        assert!(log.exists(), "under the cap: kept");
        std::fs::write(&log, b"0123456789").unwrap();
        std::fs::write(d.path().join("daemon.log.1"), b"older").unwrap();
        rotate_log(&log, 10);
        assert!(!log.exists());
        assert_eq!(
            std::fs::read(d.path().join("daemon.log.1")).unwrap(),
            b"0123456789"
        );
    }
}
