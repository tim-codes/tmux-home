//! The daemon's git task (spec §6 "Per window"): badges for every repo a
//! window (or an agent pane) is in, kept off the tmux poll's path.
//!
//! - **Input:** the published snapshots (a `watch`), never the poll itself.
//!   Each one gives the targets — every window's row pane cwd and every
//!   agent pane's cwd — and which windows have focus (the active window of
//!   each session a client shows).
//! - **Keyed by repo root:** cwd → root is a walk up for `.git` (no fork,
//!   on the blocking pool), cached and re-checked every `RESOLVE_EVERY`.
//!   One `RepoStatus` per root, shared by every window in that repo.
//! - **When:** a root is refreshed when a target's cwd changes (incl. a new
//!   window), when one of its windows gains focus, when its change stamp
//!   moves (index, HEAD, reflogs, packed-refs, … — `git::repo::stamp`,
//!   checked every `STAMP_EVERY`), and on the adaptive interval
//!   `max(10 s, 20 × last status duration)`.
//! - **Per-repo coalescing:** at most one refresh per root in flight; a
//!   trigger meanwhile sets `rerun`, which starts one more when it ends.
//! - **Fast first:** a refresh publishes HEAD (file reads), then `git
//!   status`, then the ref-derived fields (only when the refs memo saw a
//!   change), each as it lands.
//! - **Bounded:** every git call holds one of `PERMITS` semaphore permits
//!   and has a timeout (killed on drop). A timed-out or failed status keeps
//!   its last values, marked `stale`.
//! - **Output:** the `git` section, published through `Shared::offer_git`
//!   at most once per `PUBLISH_EVERY`.

use super::Shared;
use crate::git::{
    self,
    badge::{GitSection, RepoStatus},
    exec::{BADGE_TIMEOUT, Git, GitError},
    refs::RefsMemo,
    repo::{self, RepoPaths},
};
use crate::tmux::snapshot::Snapshot;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
};

/// Git processes the daemon runs at once (spec §6 "Concurrency").
pub const PERMITS: usize = 4;
pub const PUBLISH_EVERY: Duration = Duration::from_secs(1);
pub const STAMP_EVERY: Duration = Duration::from_secs(1);
pub const RESOLVE_EVERY: Duration = Duration::from_secs(10);
pub const MIN_INTERVAL: Duration = Duration::from_secs(10);
pub const INTERVAL_FACTOR: u32 = 20;
/// A focus change doesn't re-run a refresh started this recently.
pub const FOCUS_GAP: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct Config {
    pub timeout: Duration,
    pub permits: usize,
    pub publish_every: Duration,
    pub min_interval: Duration,
}

impl Default for Config {
    /// The spec's values; `TMUX_HOME_GIT_TIMEOUT_MS` overrides the per-call
    /// timeout (tests, slow disks).
    fn default() -> Self {
        let timeout = std::env::var("TMUX_HOME_GIT_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(BADGE_TIMEOUT);
        Config {
            timeout,
            permits: PERMITS,
            publish_every: PUBLISH_EVERY,
            min_interval: MIN_INTERVAL,
        }
    }
}

/// When a root is next due, from how long its last status took:
/// `max(min, 20 × took)`.
pub fn next_interval(min: Duration, took: Duration) -> Duration {
    (took * INTERVAL_FACTOR).max(min)
}

/// Every pane cwd the badges need, by window, and the focused windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Targets {
    /// (window ID, cwd) pairs: each window's row pane (its active
    /// non-sidebar pane) and every pane with agent state.
    pub cwds: HashSet<(String, String)>,
    /// Windows that are their session's active window in a session some
    /// client shows.
    pub focused: HashSet<String>,
}

pub fn targets(s: &Snapshot) -> Targets {
    let mut t = Targets::default();
    for w in &s.windows {
        let panes: Vec<_> = s
            .panes
            .iter()
            .filter(|p| p.window_id == w.id && p.session_id == w.session_id && p.role != "sidebar")
            .collect();
        let row = panes
            .iter()
            .find(|p| p.active)
            .or(panes.iter().min_by_key(|p| p.index));
        for p in row
            .into_iter()
            .chain(panes.iter().filter(|p| !p.agent_opts.is_empty()))
        {
            if !p.current_path.is_empty() {
                t.cwds.insert((w.id.clone(), p.current_path.clone()));
            }
        }
        if w.active && s.clients.iter().any(|c| c.session_id == w.session_id) {
            t.focused.insert(w.id.clone());
        }
    }
    t
}

/// What a running refresh reports.
enum Msg {
    /// A stage landed: the root's status as it now stands.
    Stage(PathBuf, RepoStatus),
    /// The refresh ended; `took` is its `git status` time (the timeout, if
    /// it timed out), `None` if it never got that far.
    Done {
        root: PathBuf,
        memo: Box<RefsMemo>,
        took: Option<Duration>,
    },
}

struct Tracked {
    paths: RepoPaths,
    status: RepoStatus,
    /// `None` while a refresh holds it.
    memo: Option<Box<RefsMemo>>,
    in_flight: bool,
    rerun: bool,
    started: Option<Instant>,
    due: Instant,
    stamp: Option<u64>,
}

struct Resolved {
    root: Option<RepoPaths>,
    at: Instant,
}

struct Task {
    cfg: Config,
    shared: Arc<Shared>,
    git: Git,
    tx: mpsc::UnboundedSender<Msg>,
    /// Running refreshes; dropped (their git children killed) with the task.
    workers: JoinSet<()>,
    targets: Targets,
    cwds: HashMap<String, Resolved>,
    repos: HashMap<PathBuf, Tracked>,
    dirty: bool,
    published: Option<Instant>,
    stamped: Instant,
}

pub(super) fn spawn(shared: Arc<Shared>, cfg: Config) -> JoinHandle<()> {
    tokio::spawn(run(shared, cfg))
}

async fn run(shared: Arc<Shared>, cfg: Config) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut snaps = shared.latest.subscribe();
    let git = Git {
        timeout: cfg.timeout,
        gate: Some(Arc::new(Semaphore::new(cfg.permits))),
        no_fsmonitor: false,
    };
    let mut t = Task {
        cfg,
        shared,
        git,
        tx,
        workers: JoinSet::new(),
        targets: Targets::default(),
        cwds: HashMap::new(),
        repos: HashMap::new(),
        dirty: false,
        published: None,
        stamped: Instant::now(),
    };
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = snaps.changed() => {
                if r.is_err() {
                    return;
                }
                let snap = snaps.borrow_and_update().as_ref().map(|(_, s)| targets(s));
                if let Some(new) = snap {
                    t.retarget(new).await;
                }
            }
            Some(m) = rx.recv() => t.receive(m),
            _ = tick.tick() => t.tick().await,
        }
        t.publish();
    }
}

impl Task {
    /// New targets from a snapshot: refresh what a cwd change or a focus
    /// change touches.
    async fn retarget(&mut self, new: Targets) {
        if new == self.targets {
            return;
        }
        let changed: Vec<String> = new
            .cwds
            .difference(&self.targets.cwds)
            .map(|(_, c)| c.clone())
            .collect();
        let focused: Vec<String> = new
            .cwds
            .iter()
            .filter(|(w, _)| new.focused.contains(w) && !self.targets.focused.contains(w))
            .map(|(_, c)| c.clone())
            .collect();
        self.targets = new;
        // a changed cwd is resolved afresh (it may have become a repo)
        for c in &changed {
            self.cwds.remove(c);
        }
        // roots new to the task start their first refresh in `resolve`
        let known: HashSet<PathBuf> = self.repos.keys().cloned().collect();
        self.resolve().await;
        for (c, focus) in changed
            .into_iter()
            .map(|c| (c, false))
            .chain(focused.into_iter().map(|c| (c, true)))
        {
            if let Some(root) = self.root_of(&c)
                && known.contains(&root)
            {
                self.trigger(&root, focus);
            }
        }
    }

    fn root_of(&self, cwd: &str) -> Option<PathBuf> {
        Some(self.cwds.get(cwd)?.root.as_ref()?.root.clone())
    }

    /// Resolves every target cwd not resolved within `RESOLVE_EVERY`, then
    /// tracks the roots in use and drops the others.
    async fn resolve(&mut self) {
        let now = Instant::now();
        let todo: Vec<String> = self
            .targets
            .cwds
            .iter()
            .map(|(_, c)| c.clone())
            .filter(|c| {
                self.cwds
                    .get(c)
                    .is_none_or(|r| now.duration_since(r.at) >= RESOLVE_EVERY)
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !todo.is_empty() {
            let found = tokio::task::spawn_blocking(move || {
                todo.into_iter()
                    .map(|c| {
                        let r = repo::resolve(Path::new(&c));
                        (c, r)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            for (c, root) in found {
                self.cwds.insert(c, Resolved { root, at: now });
            }
        }
        let live: HashSet<&String> = self.targets.cwds.iter().map(|(_, c)| c).collect();
        self.cwds.retain(|c, _| live.contains(c));
        let mut used: HashMap<PathBuf, RepoPaths> = HashMap::new();
        for r in self.cwds.values() {
            if let Some(p) = &r.root {
                used.insert(p.root.clone(), p.clone());
            }
        }
        let before = self.repos.len();
        self.repos.retain(|root, _| used.contains_key(root));
        if self.repos.len() != before {
            self.dirty = true;
        }
        for (root, paths) in used {
            if let Some(tr) = self.repos.get_mut(&root) {
                tr.paths = paths; // e.g. a worktree re-registered
                continue;
            }
            self.repos.insert(
                root.clone(),
                Tracked {
                    paths,
                    status: RepoStatus::default(),
                    memo: Some(Box::default()),
                    in_flight: false,
                    rerun: false,
                    started: None,
                    due: now,
                    stamp: None,
                },
            );
            self.dirty = true;
            self.trigger(&root, false);
        }
    }

    /// Starts a refresh of `root`, or asks the running one for another.
    fn trigger(&mut self, root: &Path, focus: bool) {
        let Some(tr) = self.repos.get_mut(root) else {
            return;
        };
        if focus && tr.started.is_some_and(|s| s.elapsed() < FOCUS_GAP) {
            return;
        }
        if tr.in_flight {
            tr.rerun = true;
            return;
        }
        let Some(memo) = tr.memo.take() else { return };
        tr.in_flight = true;
        tr.rerun = false;
        tr.started = Some(Instant::now());
        self.workers.spawn(refresh(
            self.git.clone(),
            tr.paths.clone(),
            tr.status.clone(),
            memo,
            self.tx.clone(),
        ));
    }

    fn receive(&mut self, m: Msg) {
        match m {
            Msg::Stage(root, st) => {
                if let Some(tr) = self.repos.get_mut(&root)
                    && tr.status != st
                {
                    tr.status = st;
                    self.dirty = true;
                }
            }
            Msg::Done { root, memo, took } => {
                let Some(tr) = self.repos.get_mut(&root) else {
                    return;
                };
                tr.memo = Some(memo);
                tr.in_flight = false;
                let took = took.unwrap_or(self.cfg.timeout);
                tr.due = Instant::now() + next_interval(self.cfg.min_interval, took);
                if tr.rerun {
                    self.trigger(&root, false);
                }
            }
        }
    }

    async fn tick(&mut self) {
        while self.workers.try_join_next().is_some() {}
        let now = Instant::now();
        if now.duration_since(self.stamped) >= STAMP_EVERY {
            self.stamped = now;
            self.resolve().await;
            let paths: Vec<RepoPaths> = self.repos.values().map(|t| t.paths.clone()).collect();
            let stamps = tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .map(|p| (p.root.clone(), repo::stamp(&p)))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            for (root, s) in stamps {
                let Some(tr) = self.repos.get_mut(&root) else {
                    continue;
                };
                let moved = tr.stamp.is_some_and(|old| old != s);
                tr.stamp = Some(s);
                if moved {
                    self.trigger(&root, false);
                }
            }
        }
        let due: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, t)| !t.in_flight && t.due <= now)
            .map(|(r, _)| r.clone())
            .collect();
        for r in due {
            self.trigger(&r, false);
        }
    }

    /// The git section as it stands.
    fn section(&self) -> GitSection {
        let mut g = GitSection::default();
        for (c, r) in &self.cwds {
            if let Some(p) = &r.root {
                g.paths
                    .insert(c.clone(), p.root.to_string_lossy().into_owned());
            }
        }
        g.repos = self
            .repos
            .iter()
            .map(|(r, t)| (r.to_string_lossy().into_owned(), t.status.clone()))
            .collect::<BTreeMap<_, _>>();
        g
    }

    /// Publishes the section if it changed, at most once per
    /// `publish_every`.
    fn publish(&mut self) {
        if !self.dirty
            || self
                .published
                .is_some_and(|t| t.elapsed() < self.cfg.publish_every)
        {
            return;
        }
        self.dirty = false;
        self.published = Some(Instant::now());
        self.shared.offer_git(self.section());
    }
}

/// One refresh of one root, fast first; reports each stage as it lands.
async fn refresh(
    git: Git,
    p: RepoPaths,
    mut st: RepoStatus,
    mut memo: Box<RefsMemo>,
    tx: mpsc::UnboundedSender<Msg>,
) {
    let root = p.root.clone();
    let done = |memo, took| Msg::Done {
        root: root.clone(),
        memo,
        took,
    };
    // 1. HEAD, operation, stashes: file reads, on the blocking pool
    let head = {
        let (p, st) = (p.clone(), st.clone());
        tokio::task::spawn_blocking(move || {
            let mut st = st;
            git::apply_head(&mut st, &p);
            st
        })
        .await
    };
    if let Ok(h) = head {
        st = h;
        let _ = tx.send(Msg::Stage(root.clone(), st.clone()));
    }
    // 2. git status
    let t0 = Instant::now();
    match git::read_status(&git, &p).await {
        Ok(s) => {
            git::apply_status(&mut st, &s);
            st.stale = false;
            st.error = None;
            let _ = tx.send(Msg::Stage(root.clone(), st.clone()));
        }
        Err(e) => {
            st.stale = true;
            st.error = (e != GitError::Timeout).then(|| e.to_string());
            let _ = tx.send(Msg::Stage(root.clone(), st.clone()));
            let took = (e == GitError::Timeout).then_some(git.timeout);
            let _ = tx.send(done(memo, took));
            return;
        }
    }
    let took = t0.elapsed();
    // 3. refs: a probe, then more only if something changed
    match memo.refresh(&git, &p).await {
        Ok(_) => {
            git::apply_refs(&mut st, &memo.fields, &p);
            let _ = tx.send(Msg::Stage(root.clone(), st));
        }
        Err(e) => {
            st.stale = true;
            st.error = (e != GitError::Timeout).then(|| e.to_string());
            let _ = tx.send(Msg::Stage(root.clone(), st));
        }
    }
    let _ = tx.send(done(memo, Some(took)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::snapshot::{Client, Pane, Window};

    fn pane(id: &str, wid: &str, active: bool, path: &str) -> Pane {
        Pane {
            id: id.into(),
            window_id: wid.into(),
            session_id: "$0".into(),
            index: id[1..].parse().unwrap(),
            active,
            current_command: "zsh".into(),
            current_path: path.into(),
            title: String::new(),
            role: String::new(),
            agent_opts: Default::default(),
            tty: String::new(),
        }
    }

    fn win(id: &str, active: bool) -> Window {
        Window {
            id: id.into(),
            session_id: "$0".into(),
            index: id[1..].parse().unwrap(),
            name: id.into(),
            automatic_rename: false,
            active,
        }
    }

    #[test]
    fn interval_is_at_least_the_minimum() {
        let min = Duration::from_secs(10);
        assert_eq!(next_interval(min, Duration::from_millis(30)), min);
        assert_eq!(
            next_interval(min, Duration::from_secs(1)),
            Duration::from_secs(20)
        );
    }

    #[test]
    fn targets_are_row_panes_and_agent_panes() {
        let mut s = Snapshot {
            windows: vec![win("@1", true), win("@2", false)],
            ..Snapshot::default()
        };
        let mut agent = pane("%3", "@1", false, "/agent");
        agent
            .agent_opts
            .insert("@home_status".into(), "running".into());
        let mut side = pane("%4", "@2", true, "/sidebar");
        side.role = "sidebar".into();
        s.panes = vec![
            pane("%1", "@1", true, "/a"),
            pane("%2", "@1", false, "/not-active"),
            agent,
            side,
            pane("%5", "@2", false, "/b"),
        ];
        let t = targets(&s);
        let mut cwds: Vec<_> = t.cwds.iter().map(|(w, c)| format!("{w}:{c}")).collect();
        cwds.sort();
        assert_eq!(cwds, ["@1:/a", "@1:/agent", "@2:/b"]);
        assert!(t.focused.is_empty(), "no client, no focus");
        s.clients.push(Client {
            name: "c".into(),
            tty: "/dev/x".into(),
            session_id: "$0".into(),
        });
        assert_eq!(targets(&s).focused, HashSet::from(["@1".to_string()]));
    }
}
