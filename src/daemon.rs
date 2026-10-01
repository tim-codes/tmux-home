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
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{Notify, watch},
};

type Latest = watch::Receiver<Option<(u64, Snapshot)>>;

pub async fn run(tmux_socket: PathBuf, kind: SourceKind) -> anyhow::Result<()> {
    let paths = Paths::for_socket(&tmux_socket)?;
    let dir = paths.sock.parent().expect("socket has a parent");
    std::fs::create_dir_all(dir)?;
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
            conn = listener.accept() => {
                let (stream, _) = conn?;
                tokio::spawn(serve(stream, latest.clone(), restart.clone()));
            }
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
    let Ok(Some(req)) = read_msg::<_, Request>(&mut r).await else {
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
