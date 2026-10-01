use crate::{
    VERSION,
    ipc::{Reply, Request, read_msg, write_msg},
    paths::Paths,
    tmux::{
        Tmux,
        snapshot::{Snapshot, read_snapshot},
    },
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::BufReader, net::UnixStream};

/// How long an old daemon's socket is given to disappear after a `Restart`
/// reply before we give up waiting and spawn a replacement anyway. The old
/// daemon removes its socket and *then* releases its flock, so once the
/// socket is gone the lock is free (or about to be) and a replacement's
/// `try_lock` won't lose to it.
const RESTART_SOCKET_GONE_TIMEOUT: Duration = Duration::from_millis(1000);
const RESTART_SOCKET_POLL: Duration = Duration::from_millis(20);

/// Resolve the tmux socket to target: `explicit` (`--socket`) if given, else
/// the first comma-separated field of `$TMUX` (set inside a tmux client).
pub async fn current_socket(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let tmux = std::env::var("TMUX")
        .map_err(|_| anyhow::anyhow!("not inside tmux and no --socket given"))?;
    Ok(PathBuf::from(tmux.split(',').next().unwrap_or_default()))
}

async fn ask_daemon(p: &Paths) -> anyhow::Result<Reply> {
    let s = UnixStream::connect(&p.sock).await?;
    let (r, mut w) = s.into_split();
    write_msg(&mut w, &Request::Query { v: VERSION.into() }).await?;
    read_msg(&mut BufReader::new(r))
        .await?
        .ok_or_else(|| anyhow::anyhow!("daemon closed connection without replying"))
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

/// Opens (append, create) `<state_dir>/daemon.log`, creating the state dir 0700.
fn daemon_log(tmux_socket: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = Paths::for_socket(tmux_socket)?.state_dir;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?)
}

/// Best-effort `spawn_daemon`: a failure is reported on stderr and never
/// aborts the caller's degraded read.
fn try_spawn_daemon(tmux_socket: &Path) {
    if let Err(e) = spawn_daemon(tmux_socket) {
        eprintln!("tmux-home: could not start daemon: {e:#}");
    }
}

/// Wait for `sock` to stop existing (following a `Restart` reply, an old
/// daemon removes its socket and then releases its flock), polling every 20ms up
/// to `RESTART_SOCKET_GONE_TIMEOUT`. Never loops forever: gives up and
/// returns after the cap even if the socket is still there, so a wedged old
/// daemon can't block the degraded read past that bound.
async fn wait_for_socket_gone(sock: &Path) {
    let deadline = std::time::Instant::now() + RESTART_SOCKET_GONE_TIMEOUT;
    while sock.exists() {
        if std::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(RESTART_SOCKET_POLL).await;
    }
}

/// Try the daemon within `budget`; on timeout, failure, or a version-mismatch
/// `Restart` reply, (re)spawn a daemon in the background and fall back to a
/// direct read, returning `from_daemon = false`.
pub async fn snapshot(tmux_socket: &Path, budget: Duration) -> anyhow::Result<(Snapshot, bool)> {
    let p = Paths::for_socket(tmux_socket)?;
    match tokio::time::timeout(budget, ask_daemon(&p)).await {
        Ok(Ok(Reply::Snapshot { data, .. })) => return Ok((data, true)),
        Ok(Ok(Reply::Restart)) => {
            // The old daemon still holds the flock briefly after replying;
            // it removes its socket first and releases the flock right
            // after, so wait for the socket to disappear before spawning a
            // replacement, or a replacement spawned too early can fail
            // try_lock and exit silently, leaving no daemon.
            wait_for_socket_gone(&p.sock).await;
            try_spawn_daemon(tmux_socket);
        }
        _ => try_spawn_daemon(tmux_socket),
    }
    let (snap, _) = read_snapshot(&Tmux::new(tmux_socket.to_path_buf())).await?;
    Ok((snap, false))
}

pub async fn query(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let sock = current_socket(socket).await?;
    let (snap, _) = snapshot(&sock, Duration::from_millis(150)).await?;
    println!("{}", serde_json::to_string_pretty(&snap)?);
    Ok(())
}
