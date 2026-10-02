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
//!   Each tracking of a root is a new generation: a root dropped (its
//!   windows gone) has its refresh aborted, and anything a refresh of an
//!   older generation still reports is ignored.
//! - **Fast first:** a refresh publishes HEAD (file reads), then `git
//!   status`, then the ref-derived fields (only when the refs memo saw a
//!   change), each as it lands.
//! - **Bounded:** every git call holds one of `PERMITS` semaphore permits
//!   and has a timeout (killed on drop). A timed-out or failed status keeps
//!   its last values, marked `stale`.
//! - **Output:** the `git` section, published through `Shared::offer_git`
//!   at most once per `PUBLISH_EVERY`.
//! - **Failure:** a refresh that panics resets its root (stale, a fresh
//!   memo); a panic of the task itself marks every badge stale and the task
//!   restarts (`spawn` supervises it).

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
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, mpsc},
    task::{AbortHandle, JoinHandle, JoinSet},
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
/// Pause before restarting a git task that panicked.
const RESTART_AFTER: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct Config {
    /// Per git call.
    pub timeout: Duration,
    pub permits: usize,
    /// At most one git publish per this.
    pub publish_every: Duration,
    /// How often the change stamps are checked.
    pub stamp_every: Duration,
    /// The adaptive interval's floor.
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
            stamp_every: STAMP_EVERY,
            min_interval: MIN_INTERVAL,
        }
    }
}

impl Config {
    fn tick(&self) -> Duration {
        TICK.min(self.stamp_every).min(self.publish_every)
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

/// What a running refresh reports; `generation` is the tracking it belongs to.
enum Msg {
    /// A stage landed: the root's status as it now stands.
    Stage {
        root: PathBuf,
        generation: u64,
        status: RepoStatus,
    },
    /// The refresh ended; `took` is its `git status` time (the timeout, if
    /// it timed out), `None` if it never got that far.
    Done {
        root: PathBuf,
        generation: u64,
        memo: Box<RefsMemo>,
        took: Option<Duration>,
    },
}

struct Tracked {
    /// Which tracking of this root this is.
    generation: u64,
    paths: RepoPaths,
    status: RepoStatus,
    /// `None` while a refresh holds it.
    memo: Option<Box<RefsMemo>>,
    in_flight: bool,
    /// The running refresh, aborted when the root is dropped.
    worker: Option<AbortHandle>,
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
    /// Which root (and generation) each running refresh is for.
    worker_of: HashMap<tokio::task::Id, (PathBuf, u64)>,
    next_gen: u64,
    targets: Targets,
    cwds: HashMap<String, Resolved>,
    repos: HashMap<PathBuf, Tracked>,
    dirty: bool,
    published: Option<Instant>,
    stamped: Instant,
}

/// Aborts a task when dropped: the supervisor's run of the git task ends
/// with the supervisor.
struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts the git task under a supervisor: if it panics, every badge is
/// marked stale and it starts again. Aborting the returned handle stops it.
pub(super) fn spawn(shared: Arc<Shared>, cfg: Config) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let h = tokio::spawn(run(shared.clone(), cfg.clone()));
            let guard = AbortOnDrop(h.abort_handle());
            let r = h.await;
            drop(guard);
            match r {
                Err(e) if e.is_panic() => {
                    eprintln!("tmux-home: git task panicked; restarting: {e}");
                    shared.mark_git_stale("git task restarted");
                    tokio::time::sleep(RESTART_AFTER).await;
                }
                _ => return,
            }
        }
    })
}

async fn run(shared: Arc<Shared>, cfg: Config) {
    let mut snaps = shared.latest.subscribe();
    let (mut t, mut rx) = Task::new(shared, cfg);
    let mut tick = tokio::time::interval(t.cfg.tick());
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
    fn new(shared: Arc<Shared>, cfg: Config) -> (Task, mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let git = Git {
            timeout: cfg.timeout,
            gate: Some(Arc::new(Semaphore::new(cfg.permits))),
            guard: None,
        };
        let t = Task {
            stamped: Instant::now(),
            cfg,
            shared,
            git,
            tx,
            workers: JoinSet::new(),
            worker_of: HashMap::new(),
            next_gen: 0,
            targets: Targets::default(),
            cwds: HashMap::new(),
            repos: HashMap::new(),
            dirty: false,
            published: None,
        };
        (t, rx)
    }

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
        let gone: Vec<PathBuf> = self
            .repos
            .keys()
            .filter(|r| !used.contains_key(*r))
            .cloned()
            .collect();
        for root in gone {
            self.untrack(&root);
        }
        for (root, paths) in used {
            if let Some(tr) = self.repos.get_mut(&root) {
                tr.paths = paths; // e.g. a worktree re-registered
                continue;
            }
            self.track(root.clone(), paths);
            self.trigger(&root, false);
        }
    }

    /// Starts tracking `root` as a new generation; returns it.
    fn track(&mut self, root: PathBuf, paths: RepoPaths) -> u64 {
        self.next_gen += 1;
        self.repos.insert(
            root,
            Tracked {
                generation: self.next_gen,
                paths,
                status: RepoStatus::default(),
                memo: Some(Box::default()),
                in_flight: false,
                worker: None,
                rerun: false,
                started: None,
                due: Instant::now(),
                stamp: None,
            },
        );
        self.dirty = true;
        self.next_gen
    }

    /// Stops tracking `root`; its running refresh is aborted (its git
    /// killed) and anything it already sent is ignored.
    fn untrack(&mut self, root: &Path) {
        if let Some(tr) = self.repos.remove(root) {
            if let Some(w) = tr.worker {
                w.abort();
            }
            self.dirty = true;
        }
    }

    /// Runs `fut` as the refresh of `root`'s current generation.
    fn spawn_worker(&mut self, root: &Path, fut: impl Future<Output = ()> + Send + 'static) {
        let Some(tr) = self.repos.get_mut(root) else {
            return;
        };
        let h = self.workers.spawn(fut);
        self.worker_of
            .insert(h.id(), (root.to_path_buf(), tr.generation));
        tr.worker = Some(h);
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
        let fut = refresh(
            self.git.clone(),
            tr.paths.clone(),
            tr.generation,
            tr.status.clone(),
            memo,
            self.tx.clone(),
        );
        self.spawn_worker(root, fut);
    }

    /// The tracked root `root` if it is still generation `generation`.
    fn current(&mut self, root: &Path, generation: u64) -> Option<&mut Tracked> {
        self.repos
            .get_mut(root)
            .filter(|t| t.generation == generation)
    }

    fn receive(&mut self, m: Msg) {
        match m {
            Msg::Stage {
                root,
                generation,
                status,
            } => {
                if let Some(tr) = self.current(&root, generation)
                    && tr.status != status
                {
                    tr.status = status;
                    self.dirty = true;
                }
            }
            Msg::Done {
                root,
                generation,
                memo,
                took,
            } => {
                let timeout = self.cfg.timeout;
                let min = self.cfg.min_interval;
                let Some(tr) = self.current(&root, generation) else {
                    return;
                };
                tr.memo = Some(memo);
                tr.in_flight = false;
                tr.worker = None;
                tr.due = Instant::now() + next_interval(min, took.unwrap_or(timeout));
                if tr.rerun {
                    self.trigger(&root, false);
                }
            }
        }
    }

    /// Collects finished refreshes. One that panicked never sent `Done`:
    /// its root is reset (stale, a fresh memo) so it can refresh again.
    fn reap(&mut self) {
        while let Some(r) = self.workers.try_join_next_with_id() {
            let (id, err) = match r {
                Ok((id, ())) => (id, None),
                Err(e) => (e.id(), Some(e)),
            };
            let Some((root, generation)) = self.worker_of.remove(&id) else {
                continue;
            };
            let Some(e) = err.filter(|e| e.is_panic()) else {
                continue;
            };
            eprintln!("tmux-home: git refresh of {} panicked: {e}", root.display());
            let min = self.cfg.min_interval;
            if let Some(tr) = self.current(&root, generation) {
                tr.in_flight = false;
                tr.worker = None;
                tr.rerun = false;
                tr.memo = Some(Box::default());
                tr.status.stale = true;
                tr.status.error = Some("internal error: the refresh panicked".into());
                tr.due = Instant::now() + min;
                self.dirty = true;
            }
        }
    }

    async fn tick(&mut self) {
        self.reap();
        let now = Instant::now();
        if now.duration_since(self.stamped) >= self.cfg.stamp_every {
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
    generation: u64,
    mut st: RepoStatus,
    mut memo: Box<RefsMemo>,
    tx: mpsc::UnboundedSender<Msg>,
) {
    let root = p.root.clone();
    let stage = |status: RepoStatus| Msg::Stage {
        root: root.clone(),
        generation,
        status,
    };
    let done = |memo, took| Msg::Done {
        root: root.clone(),
        generation,
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
        let _ = tx.send(stage(st.clone()));
    }
    // 2. the guard (the repo's command-running config keys, each to be
    // overridden; one that can't be leaves a HEAD-only badge, `limited`),
    // read afresh, then `git status` right after it under the same permit
    let t0 = Instant::now();
    let git = match git::read_status_fresh(&git, &p).await {
        Ok((g, s)) => {
            git::apply_status(&mut st, &s);
            st.limited = false;
            st.stale = false;
            st.error = None;
            let _ = tx.send(stage(st.clone()));
            g
        }
        Err(e) => {
            // back off from what it cost: the timeout, or the real time
            let took = if e == GitError::Timeout {
                git.timeout
            } else {
                t0.elapsed()
            };
            if let GitError::Limited(_) = e {
                st = git::limited(&st);
            } else {
                st.stale = true;
                st.error = (e != GitError::Timeout).then(|| e.to_string());
            }
            let _ = tx.send(stage(st));
            let _ = tx.send(done(memo, Some(took)));
            return;
        }
    };
    let took = t0.elapsed();
    // 3. refs: a probe, then more only if something changed
    match memo.refresh(&git, &p).await {
        Ok(_) => {
            git::apply_refs(&mut st, &memo.fields, &p);
            let _ = tx.send(stage(st));
        }
        Err(e) => {
            st.stale = true;
            st.error = (e != GitError::Timeout).then(|| e.to_string());
            let _ = tx.send(stage(st));
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
            home_role: String::new(),
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

    fn paths(root: &str) -> RepoPaths {
        RepoPaths {
            root: root.into(),
            git_dir: format!("{root}/.git").into(),
            common_dir: format!("{root}/.git").into(),
            linked: false,
        }
    }

    fn task() -> (Task, mpsc::UnboundedReceiver<Msg>) {
        Task::new(Arc::new(Shared::for_test()), Config::default())
    }

    fn named(branch: &str) -> RepoStatus {
        RepoStatus {
            branch: branch.into(),
            ..RepoStatus::default()
        }
    }

    /// A root dropped while its refresh ran, then tracked again: the old
    /// refresh is aborted and whatever it still reports is ignored.
    #[tokio::test]
    async fn an_old_generation_never_touches_a_new_one() {
        let (mut t, _rx) = task();
        let root = PathBuf::from("/r");
        let g1 = t.track(root.clone(), paths("/r"));
        // generation 1's refresh is running (forever)
        let memo1 = t.repos.get_mut(&root).unwrap().memo.take().unwrap();
        t.repos.get_mut(&root).unwrap().in_flight = true;
        t.spawn_worker(&root, std::future::pending());
        t.untrack(&root);
        let g2 = t.track(root.clone(), paths("/r"));
        assert_ne!(g1, g2);
        let tr = t.repos.get_mut(&root).unwrap();
        tr.in_flight = true;
        tr.memo = None;
        tr.status = named("new");
        // late messages from generation 1
        t.receive(Msg::Stage {
            root: root.clone(),
            generation: g1,
            status: named("old"),
        });
        t.receive(Msg::Done {
            root: root.clone(),
            generation: g1,
            memo: memo1,
            took: Some(Duration::ZERO),
        });
        let tr = &t.repos[&root];
        assert_eq!(tr.status.branch, "new");
        assert!(
            tr.in_flight && tr.memo.is_none(),
            "generation 2's refresh still owns it"
        );
        // generation 1's worker was aborted
        let r = t.workers.join_next().await.unwrap();
        assert!(r.unwrap_err().is_cancelled());
        // generation 2's own messages land
        t.receive(Msg::Stage {
            root: root.clone(),
            generation: g2,
            status: named("newer"),
        });
        t.receive(Msg::Done {
            root: root.clone(),
            generation: g2,
            memo: Box::default(),
            took: Some(Duration::ZERO),
        });
        let tr = &t.repos[&root];
        assert_eq!(tr.status.branch, "newer");
        assert!(!tr.in_flight && tr.memo.is_some());
    }

    /// A refresh that panics resets its root instead of leaving it in
    /// flight forever.
    #[tokio::test]
    async fn a_panicked_refresh_resets_its_root() {
        let (mut t, _rx) = task();
        let root = PathBuf::from("/r");
        t.track(root.clone(), paths("/r"));
        let tr = t.repos.get_mut(&root).unwrap();
        tr.memo = None;
        tr.in_flight = true;
        tr.status = named("main");
        t.spawn_worker(&root, async { panic!("boom") });
        while !t.workers.is_empty() {
            tokio::task::yield_now().await;
            t.reap();
        }
        let tr = &t.repos[&root];
        assert!(!tr.in_flight && tr.memo.is_some(), "can refresh again");
        assert!(tr.status.stale && tr.status.error.is_some());
        assert_eq!(tr.status.branch, "main", "keeps its values");
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
