pub mod git;
pub mod sidebars;

use crate::{
    BUILD_ID,
    git::badge::GitSection,
    ipc::{Reply, Request, read_msg, write_msg},
    paths::Paths,
    tmux::{
        Tmux,
        snapshot::{Sections, Snapshot, git_hash, read_snapshot_async},
        source::{self, SourceEvent, SourceKind},
    },
};
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    signal::unix::{SignalKind, signal},
    sync::{Notify, watch},
};

/// The daemon's published snapshot and its `seq`.
type Published = Option<(u64, Snapshot)>;

/// What the daemon does with one read of tmux (`Model::offer`).
#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    /// The read started before one already offered: it is older than what
    /// is published, so it is dropped (reads overlap: the source's poll and
    /// a client's `refresh` run concurrently).
    Stale,
    /// Nothing tracked changed; the published snapshot (this seq) stands.
    Unchanged(u64),
    /// Publish it under this new seq.
    Changed(u64),
}

/// Change detection and ordering for the reads the daemon publishes. Pure:
/// it sees only each read's section hashes and start time.
#[derive(Debug, Default)]
pub struct Model {
    seq: u64,
    sections: Option<Sections>,
    read_at: Option<Instant>,
}

impl Model {
    pub fn offer(&mut self, sections: Sections, read_at: Instant) -> Offer {
        if self.read_at.is_some_and(|t| read_at < t) {
            return Offer::Stale;
        }
        self.read_at = Some(read_at);
        if self.sections == Some(sections) {
            return Offer::Unchanged(self.seq);
        }
        self.sections = Some(sections);
        self.seq += 1;
        Offer::Changed(self.seq)
    }

    /// Offers a new git section (by hash): the seq to publish it under, if
    /// it changed. Before the first tmux read nothing is published; that
    /// read carries the section.
    pub fn offer_git(&mut self, git: u64) -> Option<u64> {
        let s = self.sections.as_mut()?;
        if s.git == git {
            return None;
        }
        s.git = git;
        self.seq += 1;
        Some(self.seq)
    }
}

/// The model and the git section the next publish carries, under one lock.
#[derive(Default)]
struct Inner {
    model: Model,
    git: GitSection,
}

/// State shared by the daemon's main loop and its connections.
struct Shared {
    epoch: u64,
    version: String,
    tmux: Tmux,
    model: Mutex<Inner>,
    latest: watch::Sender<Published>,
    restart: Notify,
}

impl Shared {
    /// Offers a read that started at `at`; publishes it if it changed
    /// something. The model and the watch are updated under one lock, so
    /// subscribers see seqs in order.
    fn offer(&self, mut snap: Snapshot, at: Instant) {
        let mut m = self.model.lock().unwrap_or_else(|e| e.into_inner());
        snap.git = m.git.clone();
        if let Offer::Changed(seq) = m.model.offer(snap.sections(), at) {
            self.latest.send_replace(Some((seq, snap)));
        }
    }

    /// Marks every repo in the published git section stale (the git task
    /// died and is restarting): the badges keep their values, dimmed `~`.
    fn mark_git_stale(&self, why: &str) {
        let mut git = {
            let m = self.model.lock().unwrap_or_else(|e| e.into_inner());
            m.git.clone()
        };
        for st in git.repos.values_mut() {
            st.stale = true;
            st.error = Some(why.to_string());
        }
        self.offer_git(git);
    }

    /// Publishes a new git section over the latest tmux read, if it
    /// changed; tmux reads offered later carry it too.
    fn offer_git(&self, git: GitSection) {
        let mut m = self.model.lock().unwrap_or_else(|e| e.into_inner());
        let seq = m.model.offer_git(git_hash(&git));
        m.git = git;
        if let Some(seq) = seq {
            let snap = self.latest.borrow().as_ref().map(|(_, s)| s.clone());
            if let Some(mut snap) = snap {
                snap.git = m.git.clone();
                self.latest.send_replace(Some((seq, snap)));
            }
        }
    }

    /// Reads tmux now and offers that read: afterwards the published
    /// snapshot is at least as new as it.
    async fn refresh(&self) -> anyhow::Result<()> {
        let at = Instant::now();
        let snap = read_snapshot_async(&self.tmux).await?;
        self.offer(snap, at);
        Ok(())
    }
}

/// A name for this daemon instance, distinct from any earlier one's.
fn new_epoch() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    nanos ^ (u64::from(std::process::id()) << 40)
}

/// Backoff after a transient `accept()` error, to avoid a hot loop under a
/// persistent failure (e.g. the process is out of file descriptors).
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// How long a starting daemon waits for the lock before deciding another
/// daemon serves the server.
const LOCK_WAIT: Duration = Duration::from_millis(1000);

/// Timeout on reading a connection's initial request, so a client that
/// connects and never writes doesn't hold a `serve` task forever.
const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Size of `sockaddr_un.sun_path`; a bindable path must be shorter (it is
/// NUL-terminated).
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 108;

pub async fn run(tmux_socket: PathBuf, kind: SourceKind) -> anyhow::Result<()> {
    run_with_version(tmux_socket, kind, BUILD_ID).await
}

/// `run`, answering as build `version` (tests: a daemon of a different
/// build than its clients, to exercise the `Restart` handshake).
pub async fn run_with_version(
    tmux_socket: PathBuf,
    kind: SourceKind,
    version: impl Into<String>,
) -> anyhow::Result<()> {
    run_with(tmux_socket, kind, version, git::Config::default()).await
}

/// `run_with_version` with the git task's timings given (tests shrink them).
pub async fn run_with(
    tmux_socket: PathBuf,
    kind: SourceKind,
    version: impl Into<String>,
    git_cfg: git::Config,
) -> anyhow::Result<()> {
    let version = version.into();
    let paths = Paths::for_socket(&tmux_socket)?;
    let len = paths.sock.as_os_str().len();
    anyhow::ensure!(
        len < SUN_PATH_MAX,
        "daemon socket path too long for this platform ({len} bytes, limit {}): {} \
         — set TMUX_HOME_RUNTIME_DIR to a shorter directory",
        SUN_PATH_MAX - 1,
        paths.sock.display()
    );
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
    // Another daemon serves this server: exit. A daemon spawned as an old
    // build exits (after its `Restart` reply) finds the lock held for the
    // moment between that one removing its socket and releasing the lock,
    // so it retries briefly before giving up.
    let deadline = Instant::now() + LOCK_WAIT;
    while lock.try_lock().is_err() {
        if Instant::now() >= deadline {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = std::fs::remove_file(&paths.sock);
    let listener = UnixListener::bind(&paths.sock)?;

    let shared = Arc::new(Shared {
        epoch: new_epoch(),
        version,
        tmux: Tmux::new(tmux_socket.clone()),
        model: Mutex::new(Inner::default()),
        latest: watch::channel(None).0,
        restart: Notify::new(),
    });
    let mut events = source::start(kind, Tmux::new(tmux_socket));
    // git badges: a task of its own, fed by the published snapshots, so no
    // git work ever sits on the tmux poll's path
    let git_task = git::spawn(shared.clone(), git_cfg);
    // sidebars: auto-create, cleanup and dedupe, from the same snapshots
    let sidebar_task = sidebars::spawn(shared.clone());
    // tmux kills `run-shell -b` jobs on kill-server: on these signals, exit
    // through the normal cleanup below (remove the socket, release the lock).
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let result = loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(SourceEvent::Snapshot(s, at)) => shared.offer(s, at),
                Some(SourceEvent::Gone) | None => break Ok(()),
            },
            conn = listener.accept() => match conn {
                Ok((stream, _)) => { tokio::spawn(serve(stream, shared.clone())); }
                Err(e) => {
                    eprintln!("tmux-home: accept error: {e:#}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
            },
            _ = shared.restart.notified() => break Ok(()),
            _ = sigterm.recv() => break Ok(()),
            _ = sighup.recv() => break Ok(()),
            _ = sigint.recv() => break Ok(()),
        }
    };
    git_task.abort();
    sidebar_task.abort();
    let _ = std::fs::remove_file(&paths.sock);
    drop(lock);
    result
}

async fn serve(stream: UnixStream, shared: Arc<Shared>) {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let Ok(Ok(Some(req))) =
        tokio::time::timeout(INITIAL_REQUEST_TIMEOUT, read_msg::<_, Request>(&mut r)).await
    else {
        return;
    };
    if req.version() != shared.version {
        let _ = write_msg(&mut w, &Reply::Restart).await;
        shared.restart.notify_one();
        return;
    }
    let mut latest = shared.latest.subscribe();
    // A subscriber places its cursor from its first snapshot, and a
    // `refresh` follows the client's own write: both get a read made now,
    // not the source's last (up to a poll old).
    if matches!(req, Request::Subscribe { .. } | Request::Refresh { .. }) {
        let read = shared.refresh().await;
        if let Some(err) = refresh_failed(&req, read) {
            let _ = write_msg(&mut w, &err).await;
            return;
        }
    }
    // wait for the first snapshot if the source hasn't produced one yet
    if latest.wait_for(|s| s.is_some()).await.is_err() {
        return;
    }
    let cur = latest.borrow_and_update().clone();
    if send(&mut w, shared.epoch, cur).await.is_err() {
        return;
    }
    if let Request::Subscribe { .. } = req {
        while latest.changed().await.is_ok() {
            let cur = latest.borrow_and_update().clone();
            if send(&mut w, shared.epoch, cur).await.is_err() {
                return;
            }
        }
    }
}

/// What a failed fresh read means for `req`: a `refresh` must not be
/// answered with the published snapshot (it may predate the client's
/// write, and its seq would become the client's floor), so it gets an
/// error and the client reads tmux itself. A subscription falls back to the
/// published snapshot; newer ones follow.
fn refresh_failed(req: &Request, read: anyhow::Result<()>) -> Option<Reply> {
    let e = read.err()?;
    eprintln!("tmux-home: refresh failed: {e:#}");
    matches!(req, Request::Refresh { .. }).then(|| Reply::Error {
        msg: format!("refresh failed: {e:#}"),
    })
}

async fn send(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    epoch: u64,
    cur: Published,
) -> anyhow::Result<()> {
    let Some((seq, data)) = cur else {
        anyhow::bail!("no snapshot")
    };
    write_msg(w, &Reply::Snapshot { epoch, seq, data }).await
}

#[cfg(test)]
impl Shared {
    /// A `Shared` for unit tests (its tmux socket is never used).
    pub(crate) fn for_test() -> Shared {
        Shared {
            epoch: 1,
            version: "test".into(),
            tmux: Tmux::new(PathBuf::from("/nonexistent")),
            model: Mutex::new(Inner::default()),
            latest: watch::channel(None).0,
            restart: Notify::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(tmux: u64) -> Sections {
        Sections { tmux, git: 0 }
    }

    #[test]
    fn a_failed_refresh_is_an_error_but_a_subscription_goes_on() {
        let refresh = Request::Refresh { v: "x".into() };
        let sub = Request::Subscribe {
            v: "x".into(),
            client: "t".into(),
        };
        assert!(refresh_failed(&refresh, Ok(())).is_none());
        assert!(matches!(
            refresh_failed(&refresh, Err(anyhow::anyhow!("tmux gone"))),
            Some(Reply::Error { msg }) if msg.contains("tmux gone")
        ));
        assert!(refresh_failed(&sub, Err(anyhow::anyhow!("tmux gone"))).is_none());
    }

    #[test]
    fn first_read_publishes_then_only_changes_do() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut m = Model::default();
        assert_eq!(m.offer(sections(1), at(0)), Offer::Changed(1));
        assert_eq!(m.offer(sections(1), at(10)), Offer::Unchanged(1));
        assert_eq!(m.offer(sections(2), at(20)), Offer::Changed(2));
        // back to an earlier state is still a change
        assert_eq!(m.offer(sections(1), at(30)), Offer::Changed(3));
    }

    /// A read that started before an already-offered one finished after
    /// it: publishing it would put older data over newer.
    #[test]
    fn a_read_overtaken_by_a_newer_one_is_dropped() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut m = Model::default();
        assert_eq!(m.offer(sections(1), at(0)), Offer::Changed(1));
        // the refresh (started at 20) lands before the poll (started at 10)
        assert_eq!(m.offer(sections(2), at(20)), Offer::Changed(2));
        assert_eq!(m.offer(sections(1), at(10)), Offer::Stale);
        assert_eq!(m.offer(sections(2), at(20)), Offer::Unchanged(2));
    }

    /// A git section publishes under a new seq only when it changed, and
    /// only once a tmux read has been published (which then carries it).
    #[test]
    fn git_offers_bump_the_seq_only_on_change() {
        let mut m = Model::default();
        assert_eq!(m.offer_git(7), None, "nothing published yet");
        assert_eq!(m.offer(sections(1), Instant::now()), Offer::Changed(1));
        assert_eq!(m.offer_git(0), None, "same as the published one");
        assert_eq!(m.offer_git(7), Some(2));
        assert_eq!(m.offer_git(7), None);
        // a tmux read carrying the same git section is unchanged
        let s = Sections { tmux: 1, git: 7 };
        assert_eq!(m.offer(s, Instant::now()), Offer::Unchanged(2));
    }

    /// A restarting git task leaves the badges, marked stale.
    #[test]
    fn mark_git_stale_keeps_values() {
        use crate::git::badge::RepoStatus;
        let sh = Shared::for_test();
        sh.offer(Snapshot::default(), Instant::now());
        let mut g = GitSection::default();
        g.repos.insert(
            "/r".into(),
            RepoStatus {
                branch: "main".into(),
                ..RepoStatus::default()
            },
        );
        sh.offer_git(g);
        sh.mark_git_stale("git task restarted");
        let (_, snap) = sh.latest.borrow().clone().unwrap();
        let st = &snap.git.repos["/r"];
        assert_eq!(st.branch, "main");
        assert!(st.stale);
        assert_eq!(st.badge_text(), "main ~");
    }

    /// Change detection sees only the tmux section's content.
    #[test]
    fn sections_hash_tmux_content() {
        use crate::tmux::snapshot::Session;
        let mut a = Snapshot::default();
        assert_eq!(a.sections(), Snapshot::default().sections());
        a.sessions.push(Session {
            id: "$0".into(),
            name: "x".into(),
            attached: 0,
        });
        let b = a.clone();
        assert_eq!(a.sections(), b.sections());
        a.sessions[0].attached = 1;
        assert_ne!(a.sections(), b.sections());
    }
}
