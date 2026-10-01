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
    use super::control::{Line, Notification, parse_line};
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut child = tmux
        .command()
        .args([
            "-C",
            "attach-session",
            "-f",
            "no-output,ignore-size,read-only",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    stdin
        .write_all(b"refresh-client -B 'th-panes:%*:#{pane_current_command}#{pane_current_path}#{pane_title}'\n")
        .await?;

    let mut last = Some(refresh(&tmux, &tx, None).await?);
    let mut dirty = false;
    let debounce = Duration::from_millis(30);
    // A pinned, reused timer: only `reset` on the *first* Changed notification
    // after a refresh, so a steady stream of control-mode lines (even
    // Line::Other ones, e.g. %output) can't keep restarting a freshly-created
    // sleep future and indefinitely postpone the debounced refresh.
    let debounce_sleep = tokio::time::sleep(Duration::from_secs(3600));
    tokio::pin!(debounce_sleep);
    let mut resync = tokio::time::interval(RESYNC_EVERY);
    resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    resync.tick().await; // first tick is immediate
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) }; // EOF: server gone
                match parse_line(&line) {
                    Line::Notify(Notification::Exit(_)) => return Ok(()),
                    Line::Notify(Notification::Changed(_)) if !dirty => {
                        dirty = true;
                        debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                    }
                    _ => {}
                }
            }
            () = &mut debounce_sleep, if dirty => {
                dirty = false;
                last = Some(refresh(&tmux, &tx, last).await?);
            }
            _ = resync.tick() => {
                last = Some(refresh(&tmux, &tx, last).await?);
            }
        }
    }
}

pub async fn spike_control(_socket: std::path::PathBuf) -> anyhow::Result<()> {
    anyhow::bail!("not yet")
}
