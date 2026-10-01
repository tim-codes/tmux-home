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
/// daemon removes its socket after releasing its flock, so this bounds how
/// long a replacement daemon might fail `try_lock` and exit silently.
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
/// current executable.
pub fn spawn_daemon(tmux_socket: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let bin = std::env::var_os("TMUX_HOME_BIN")
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)?;
    std::process::Command::new(bin)
        .arg("daemon")
        .arg("--socket")
        .arg(tmux_socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// Wait for `sock` to stop existing (an old daemon removes its socket after
/// releasing its flock, following a `Restart` reply), polling every 20ms up
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
            // wait for its socket to disappear before spawning a
            // replacement, or a replacement spawned too early can fail
            // try_lock and exit silently, leaving no daemon.
            wait_for_socket_gone(&p.sock).await;
            spawn_daemon(tmux_socket)?;
        }
        _ => {
            spawn_daemon(tmux_socket)?;
        }
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
