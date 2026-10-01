use super::{
    Tmux,
    snapshot::{Snapshot, read_snapshot},
};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum SourceKind {
    Control,
    Poll,
}

#[derive(Debug)]
pub enum SourceEvent {
    Snapshot(Snapshot),
    Gone,
}

pub const POLL_EVERY: Duration = Duration::from_millis(500);
pub const RESYNC_EVERY: Duration = Duration::from_secs(5);

/// Spawns the source task for `kind` against `tmux`. The first event sent is
/// always an initial `Snapshot`. The task stops after sending `Gone`.
pub fn start(kind: SourceKind, tmux: Tmux) -> mpsc::Receiver<SourceEvent> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let r = match kind {
            SourceKind::Poll => poll(tmux, tx.clone()).await,
            SourceKind::Control => control(tmux, tx.clone()).await,
        };
        if let Err(e) = r {
            eprintln!("tmux-home: source ended: {e:#}");
        }
        let _ = tx.send(SourceEvent::Gone).await;
    });
    rx
}

/// Re-reads the snapshot and sends it if its hash differs from `last`.
/// Returns the new hash.
async fn refresh(
    tmux: &Tmux,
    tx: &mpsc::Sender<SourceEvent>,
    last: Option<u64>,
) -> anyhow::Result<u64> {
    let (snap, h) = read_snapshot(tmux).await?;
    if Some(h) != last {
        tx.send(SourceEvent::Snapshot(snap)).await?;
    }
    Ok(h)
}

async fn poll(tmux: Tmux, tx: mpsc::Sender<SourceEvent>) -> anyhow::Result<()> {
    let mut last = None;
    loop {
        last = Some(refresh(&tmux, &tx, last).await?);
        tokio::time::sleep(POLL_EVERY).await;
    }
}

async fn control(tmux: Tmux, tx: mpsc::Sender<SourceEvent>) -> anyhow::Result<()> {
    poll(tmux, tx).await // replaced in Task 6
}

pub async fn spike_control(_socket: std::path::PathBuf) -> anyhow::Result<()> {
    anyhow::bail!("not yet")
}
