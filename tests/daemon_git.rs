//! The daemon's git task on a throwaway tmux server with windows in temp
//! repos: badges reach subscribers within ~2 s, git changes follow, and no
//! git work — even hung — delays a tmux push.
mod common;
use common::*;
use std::path::Path;
use std::time::{Duration, Instant};
use tmux_home::{
    git::badge::{Phase, RepoStatus},
    ipc::{Reply, Request, read_msg, write_msg},
    paths::Paths,
    tmux::{snapshot::Snapshot, source::SourceKind},
};
use tokio::{
    io::BufReader,
    net::{UnixStream, unix::OwnedReadHalf},
};

struct Sub {
    r: BufReader<OwnedReadHalf>,
    _w: tokio::net::unix::OwnedWriteHalf,
}

async fn subscribe(p: &Paths) -> Sub {
    let mut s = None;
    for _ in 0..100 {
        if let Ok(c) = UnixStream::connect(&p.sock).await {
            s = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (r, mut w) = s.expect("daemon socket").into_split();
    write_msg(
        &mut w,
        &Request::Subscribe {
            v: tmux_home::BUILD_ID.into(),
            client: "test".into(),
        },
    )
    .await
    .unwrap();
    Sub {
        r: BufReader::new(r),
        _w: w,
    }
}

impl Sub {
    /// Reads pushes until `f` holds for one, within `within`; returns it.
    async fn until(
        &mut self,
        within: Duration,
        what: &str,
        mut f: impl FnMut(&Snapshot) -> bool,
    ) -> Snapshot {
        let deadline = Instant::now() + within;
        let mut last = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, read_msg::<_, Reply>(&mut self.r)).await {
                Ok(Ok(Some(Reply::Snapshot { data, .. }))) => {
                    if f(&data) {
                        return data;
                    }
                    last = Some(data);
                }
                Ok(other) => panic!("unexpected reply {other:?}"),
                Err(_) => panic!(
                    "no snapshot with {what} within {within:?}; last git: {:#?}",
                    last.map(|s| s.git)
                ),
            }
        }
    }
}

/// The git task with test timings: stamps checked and pushes allowed every
/// 100 ms (the spec's 1 s each would put the waits below near their limit).
fn fast() -> tmux_home::daemon::git::Config {
    tmux_home::daemon::git::Config {
        publish_every: Duration::from_millis(100),
        stamp_every: Duration::from_millis(100),
        ..Default::default()
    }
}

/// How long a rename of `from` to `to` takes to reach the subscriber.
async fn rename_push(s: &TestServer, sub: &mut Sub, from: &str, to: &str) -> Duration {
    let t0 = Instant::now();
    s.tmux(&["rename-window", "-t", &format!("=alpha:{from}"), to]);
    sub.until(Duration::from_secs(5), "the rename", |x| {
        x.windows.iter().any(|w| w.name == to)
    })
    .await;
    t0.elapsed()
}

fn status<'a>(s: &'a Snapshot, dir: &Path) -> Option<&'a RepoStatus> {
    s.git.for_cwd(dir.to_str().unwrap())
}

#[tokio::test]
async fn badges_follow_windows_and_git_changes() {
    let _env = TestEnv::new();
    git_env();
    let t = TempDir::new("dg");
    let repo = t.repo("app");
    write(&repo, "notes.txt", "x");
    let s = TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run_with(
        s.socket.clone(),
        SourceKind::Poll,
        tmux_home::BUILD_ID,
        fast(),
    ));
    let mut sub = subscribe(&p).await;
    sub.until(Duration::from_secs(3), "a first snapshot", |_| true)
        .await;

    // a window in the repo: its badge within ~2 s
    let t0 = Instant::now();
    s.tmux(&[
        "new-window",
        "-d",
        "-n",
        "app",
        "-c",
        repo.to_str().unwrap(),
    ]);
    let snap = sub
        .until(Duration::from_secs(3), "the repo's status", |x| {
            status(x, &repo).is_some_and(|g| g.phase == Phase::Refs)
        })
        .await;
    let took = t0.elapsed();
    let g = status(&snap, &repo).unwrap();
    assert_eq!(g.badge_text(), "main ?", "{g:?}");
    assert_eq!(g.default_branch.as_deref(), Some("main"));
    eprintln!("first badge after {took:?}");

    // a git change (the index moves): picked up by the change stamp
    let t0 = Instant::now();
    git_at(&repo, &["add", "notes.txt"]);
    sub.until(Duration::from_secs(3), "the staged file", |x| {
        status(x, &repo).is_some_and(|g| g.staged == 1 && g.untracked == 0)
    })
    .await;
    eprintln!("staged change after {:?}", t0.elapsed());

    // a commit on a new branch: HEAD moves, the badge follows
    git_at(&repo, &["switch", "-q", "-c", "feat"]);
    git_at(&repo, &["commit", "-q", "-m", "notes"]);
    let snap = sub
        .until(Duration::from_secs(3), "the new branch", |x| {
            status(x, &repo).is_some_and(|g| g.branch == "feat" && !g.dirty())
        })
        .await;
    assert_eq!(status(&snap, &repo).unwrap().badge_text(), "feat");

    // the window goes: so does the repo
    s.tmux(&["kill-window", "-t", "=alpha:app"]);
    sub.until(Duration::from_secs(3), "the repo dropped", |x| {
        x.git.repos.is_empty() && x.git.paths.is_empty()
    })
    .await;

    s.tmux(&["kill-server"]);
    let _ = tokio::time::timeout(Duration::from_secs(5), d).await;
}

/// Every git hangs: tmux changes still push at the poll's pace, and the
/// fast-first stage (HEAD, no fork) still publishes the branch.
#[tokio::test]
async fn hung_git_never_delays_tmux_pushes() {
    let _env = TestEnv::new();
    git_env();
    let t = TempDir::new("hung");
    let fake = fake_git(&t.0, "exec sleep 600");
    let repos: Vec<_> = (0..6).map(|i| t.repo(&format!("r{i}"))).collect();
    // SAFETY: single-threaded tests
    unsafe { std::env::set_var("TMUX_HOME_GIT", &fake) };
    let s = TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run_with(
        s.socket.clone(),
        SourceKind::Poll,
        tmux_home::BUILD_ID,
        fast(),
    ));
    let mut sub = subscribe(&p).await;
    sub.until(Duration::from_secs(3), "a first snapshot", |_| true)
        .await;
    // baseline: renames of a window in no repo, nothing hanging yet
    s.tmux(&["new-window", "-d", "-n", "w", "-c", t.0.to_str().unwrap()]);
    sub.until(Duration::from_secs(3), "the window", |x| {
        x.windows.iter().any(|w| w.name == "w")
    })
    .await;
    let mut base = Duration::ZERO;
    for i in 0..4 {
        base = base.max(rename_push(&s, &mut sub, "w", &format!("b{i}")).await);
        rename_push(&s, &mut sub, &format!("b{i}"), "w").await;
    }
    // more repos than permits: every permit is held by a hung git
    for (i, r) in repos.iter().enumerate() {
        s.tmux(&[
            "new-window",
            "-d",
            "-n",
            &format!("w{i}"),
            "-c",
            r.to_str().unwrap(),
        ]);
    }
    let snap = sub
        .until(Duration::from_secs(3), "HEAD of every repo", |x| {
            repos
                .iter()
                .all(|r| status(x, r).is_some_and(|g| g.branch == "main"))
        })
        .await;
    for r in &repos {
        assert_eq!(status(&snap, r).unwrap().phase, Phase::Head, "status hangs");
    }
    // tmux keeps flowing: renames are pushed as fast as without git (give
    // or take one 500 ms poll)
    let mut hung = Duration::ZERO;
    for i in 0..4 {
        hung = hung.max(rename_push(&s, &mut sub, "w", &format!("h{i}")).await);
        rename_push(&s, &mut sub, &format!("h{i}"), "w").await;
    }
    eprintln!("rename push: baseline max {base:?}, with hung git max {hung:?}");
    assert!(
        hung <= base + tmux_home::tmux::source::POLL_EVERY,
        "hung git delayed tmux: {hung:?} vs {base:?}"
    );
    unsafe { std::env::remove_var("TMUX_HOME_GIT") };
    s.tmux(&["kill-server"]);
    let _ = tokio::time::timeout(Duration::from_secs(5), d).await;
}

/// A status that times out keeps the last values, marked stale (`~`).
#[tokio::test]
async fn a_timed_out_status_is_marked_stale() {
    let _env = TestEnv::new();
    git_env();
    let t = TempDir::new("stale");
    let repo = t.repo("app");
    write(&repo, "u.txt", "x");
    // hang `status` once the marker exists; everything else is real git
    let marker = t.0.join("hang");
    let fake = fake_git(
        &t.0,
        &format!(
            "case \" $* \" in *\" status \"*) [ -e '{}' ] && exec sleep 600;; esac\nexec git \"$@\"",
            marker.display()
        ),
    );
    unsafe {
        std::env::set_var("TMUX_HOME_GIT", &fake);
        std::env::set_var("TMUX_HOME_GIT_TIMEOUT_MS", "400");
    }
    let s = TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run_with(
        s.socket.clone(),
        SourceKind::Poll,
        tmux_home::BUILD_ID,
        fast(),
    ));
    let mut sub = subscribe(&p).await;
    s.tmux(&[
        "new-window",
        "-d",
        "-n",
        "app",
        "-c",
        repo.to_str().unwrap(),
    ]);
    sub.until(Duration::from_secs(3), "the status", |x| {
        status(x, &repo).is_some_and(|g| g.phase == Phase::Refs && g.untracked == 1)
    })
    .await;
    std::fs::write(&marker, "").unwrap();
    git_at(&repo, &["add", "u.txt"]); // moves the stamp: a refresh, which hangs
    let snap = sub
        .until(Duration::from_secs(3), "a stale status", |x| {
            status(x, &repo).is_some_and(|g| g.stale)
        })
        .await;
    let g = status(&snap, &repo).unwrap();
    assert_eq!((g.untracked, g.staged), (1, 0), "last good values: {g:?}");
    assert_eq!(g.badge_text(), "main ? ~");
    assert!(g.error.is_none(), "a timeout isn't an error: {g:?}");
    unsafe {
        std::env::remove_var("TMUX_HOME_GIT");
        std::env::remove_var("TMUX_HOME_GIT_TIMEOUT_MS");
    }
    s.tmux(&["kill-server"]);
    let _ = tokio::time::timeout(Duration::from_secs(5), d).await;
}
