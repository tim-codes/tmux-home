use crate::{
    VERSION,
    ipc::{Reply, Request, read_msg, write_msg},
    paths::Paths,
    tmux::{
        Tmux,
        snapshot::Snapshot,
        source::{self, SourceEvent, SourceKind},
    },
};
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{Notify, watch},
};

type Latest = watch::Receiver<Option<(u64, Snapshot)>>;

/// Backoff after a transient `accept()` error, to avoid a hot loop under a
/// persistent failure (e.g. the process is out of file descriptors).
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Timeout on reading a connection's initial request, so a client that
/// connects and never writes doesn't hold a `serve` task forever.
const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn run(tmux_socket: PathBuf, kind: SourceKind) -> anyhow::Result<()> {
    let paths = Paths::for_socket(&tmux_socket)?;
    let dir = paths.sock.parent().expect("socket has a parent");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // DirBuilder only applies `mode` to directories it creates; if `dir`
    // already existed with wider permissions, enforce 0700 explicitly.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)?;
    if lock.try_lock().is_err() {
        return Ok(()); // another daemon serves this server
    }
    let _ = std::fs::remove_file(&paths.sock);
    let listener = UnixListener::bind(&paths.sock)?;

    let (tx, latest) = watch::channel(None);
    let mut events = source::start(kind, Tmux::new(tmux_socket));
    let restart = Arc::new(Notify::new());
    let mut seq = 0u64;

    let result = loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(SourceEvent::Snapshot(s)) => { seq += 1; let _ = tx.send(Some((seq, s))); }
                Some(SourceEvent::Gone) | None => break Ok(()),
            },
            conn = listener.accept() => match conn {
                Ok((stream, _)) => { tokio::spawn(serve(stream, latest.clone(), restart.clone())); }
                Err(e) => {
                    eprintln!("tmux-home: accept error: {e:#}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
            },
            _ = restart.notified() => break Ok(()),
        }
    };
    let _ = std::fs::remove_file(&paths.sock);
    drop(lock);
    result
}

async fn serve(stream: UnixStream, mut latest: Latest, restart: Arc<Notify>) {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let Ok(Ok(Some(req))) =
        tokio::time::timeout(INITIAL_REQUEST_TIMEOUT, read_msg::<_, Request>(&mut r)).await
    else {
        return;
    };
    if req.version() != VERSION {
        let _ = write_msg(&mut w, &Reply::Restart).await;
        restart.notify_one();
        return;
    }
    // wait for the first snapshot if the source hasn't produced one yet
    if latest.wait_for(|s| s.is_some()).await.is_err() {
        return;
    }
    let cur = latest.borrow_and_update().clone();
    if send(&mut w, cur).await.is_err() {
        return;
    }
    if let Request::Subscribe { .. } = req {
        while latest.changed().await.is_ok() {
            let cur = latest.borrow_and_update().clone();
            if send(&mut w, cur).await.is_err() {
                return;
            }
        }
    }
}

async fn send(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    cur: Option<(u64, Snapshot)>,
) -> anyhow::Result<()> {
    let Some((seq, data)) = cur else {
        anyhow::bail!("no snapshot")
    };
    write_msg(w, &Reply::Snapshot { seq, data }).await
}
