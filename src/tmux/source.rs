use super::{
    Tmux,
    snapshot::{Snapshot, read_snapshot, session_count},
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

/// Pause before re-attaching a control client that ended while the server is
/// still up, so a client that keeps exiting immediately can't spin.
const REATTACH_AFTER: Duration = Duration::from_millis(100);

/// Control-mode source. The control client can end while the server lives on
/// (`detach-client`, `attach -d` from another client on the same session), so
/// it is re-attached until the server itself is gone. A server with no
/// sessions has nothing to attach to; it is polled until a session appears.
async fn control(tmux: Tmux, tx: mpsc::Sender<SourceEvent>) -> anyhow::Result<()> {
    let mut last = None;
    loop {
        match session_count(&tmux).await {
            Err(_) => return Ok(()), // server gone
            Ok(0) => {
                last = Some(refresh(&tmux, &tx, last).await?);
                tokio::time::sleep(POLL_EVERY).await;
            }
            Ok(_) => {
                last = Some(control_attached(&tmux, &tx, last).await?);
                tokio::time::sleep(REATTACH_AFTER).await;
            }
        }
    }
}

/// Runs one control client until it exits; returns the last snapshot hash.
async fn control_attached(
    tmux: &Tmux,
    tx: &mpsc::Sender<SourceEvent>,
    last: Option<u64>,
) -> anyhow::Result<u64> {
    use super::control::{Line, Notification, parse_line};
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut child = tmux
        .command()
        .args([
            "-C",
            "attach-session",
            "-f",
            // no-detach-on-destroy: when the joined session is killed, move
            // to another session instead of exiting (detach-on-destroy).
            "no-output,ignore-size,read-only,no-detach-on-destroy",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    // A failed write means the client already exited (e.g. the last session
    // went away between `session_count` and the attach); the EOF below then
    // ends this attachment normally, so it is not an error here.
    let _ = stdin
        .write_all(b"refresh-client -B 'th-panes:%*:#{pane_current_command}#{pane_current_path}#{pane_title}'\n")
        .await;

    let mut last = refresh(tmux, tx, last).await?;
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
                let Some(line) = line? else { return Ok(last) }; // EOF: client gone
                match parse_line(&line) {
                    Line::Notify(Notification::Exit(_)) => return Ok(last),
                    Line::Notify(Notification::Changed(_)) if !dirty => {
                        dirty = true;
                        debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                    }
                    _ => {}
                }
            }
            () = &mut debounce_sleep, if dirty => {
                dirty = false;
                last = refresh(tmux, tx, Some(last)).await?;
            }
            _ = resync.tick() => {
                last = refresh(tmux, tx, Some(last)).await?;
            }
        }
    }
}

/// Name and session of the control-mode client(s) attached to `t`.
async fn spike_control_clients(t: &Tmux) -> anyhow::Result<String> {
    let out = t
        .run(&[
            "list-clients",
            "-F",
            "#{client_flags} #{client_name} #{session_name}",
        ])
        .await?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (flags, rest) = l.split_once(' ')?;
            flags
                .split(',')
                .any(|f| f == "control-mode")
                .then_some(rest)
        })
        .collect::<Vec<_>>()
        .join("; "))
}

/// Drains `rx` for `within`; reports whether the source survived.
async fn spike_drain(rx: &mut mpsc::Receiver<SourceEvent>, within: Duration) -> &'static str {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(SourceEvent::Snapshot(_))) => continue,
            Ok(Some(SourceEvent::Gone)) | Ok(None) => return "source reported Gone",
            Err(_) => return "source still running",
        }
    }
}

/// R0 spike: measures what a control-mode source does to the server it watches.
/// Only ever point it at a throwaway server: it creates and kills sessions.
pub async fn spike_control(socket: std::path::PathBuf) -> anyhow::Result<()> {
    let t = Tmux::new(socket);
    let probe = |t: Tmux| async move {
        let sess = t
            .run(&[
                "list-sessions",
                "-F",
                "#{session_name} attached=#{session_attached} size=#{window_width}x#{window_height}",
            ])
            .await?;
        let clients = t
            .run(&[
                "list-clients",
                "-F",
                "#{client_name} #{client_flags} #{session_name} #{client_width}x#{client_height}",
            ])
            .await?;
        let doa = t.run(&["show", "-gv", "detach-on-destroy"]).await?;
        let ls = t.run(&["ls"]).await?;
        let best = t
            .run(&["display", "-p", "#{client_name}"])
            .await
            .unwrap_or_else(|e| format!("({e})\n"));
        anyhow::Ok(format!(
            "sessions:\n{sess}clients:\n{clients}detach-on-destroy: {doa}tmux ls:\n{ls}\
             display -p #{{client_name}} (no client context): {best}"
        ))
    };
    println!("== before ==\n{}", probe(t.clone()).await?);
    let mut rx = start(SourceKind::Control, t.clone());
    let _ = rx.recv().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("== with control client ==\n{}", probe(t.clone()).await?);

    // latency for a change in a session the control client is not attached to
    let other = t
        .run(&["new-session", "-d", "-P", "-F", "#{session_id}", "/bin/sh"])
        .await?;
    let other = other.trim();
    let windows_in = |snap: &Snapshot| {
        snap.windows
            .iter()
            .filter(|w| w.session_id == other)
            .count()
    };
    println!(
        "control client (name session): {}",
        spike_control_clients(&t).await?
    );
    let t0 = std::time::Instant::now();
    t.run(&["new-window", "-d", "-t", other]).await?;
    loop {
        match tokio::time::timeout(Duration::from_secs(6), rx.recv()).await {
            Ok(Some(SourceEvent::Snapshot(snap))) if windows_in(&snap) >= 2 => {
                println!("other-session change seen after {:?}", t0.elapsed());
                break;
            }
            Ok(Some(SourceEvent::Snapshot(_))) => continue, // an earlier change
            Ok(Some(SourceEvent::Gone)) | Ok(None) => anyhow::bail!("source gone"),
            Err(_) => {
                println!("other-session change NOT seen within 6 s");
                break;
            }
        }
    }

    // detach-on-destroy: kill the session the control client is attached to
    let joined = spike_control_clients(&t).await?;
    let joined_session = joined.rsplit(' ').next().unwrap_or_default().to_string();
    println!("\n== kill-session -t {joined_session} (control client: {joined}) ==");
    t.run(&["kill-session", "-t", &joined_session]).await?;
    println!("{}", spike_drain(&mut rx, Duration::from_secs(1)).await);
    println!("control client after: {}", spike_control_clients(&t).await?);
    println!("{}", probe(t.clone()).await?);

    // what `attach -d` from another client does to every client on that session
    let joined = spike_control_clients(&t).await?;
    let joined_session = joined.rsplit(' ').next().unwrap_or_default().to_string();
    println!("== detach-client -s {joined_session} (control client: {joined}) ==");
    t.run(&["detach-client", "-s", &joined_session]).await?;
    println!("{}", spike_drain(&mut rx, Duration::from_secs(1)).await);
    println!("control client after: {}", spike_control_clients(&t).await?);
    Ok(())
}
