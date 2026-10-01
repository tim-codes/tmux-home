//! The git layer against real temp repos: stray's tests carried over
//! (`file_diff`), and one or more per spec §6 item. Every repo is a
//! throwaway under the temp dir; no user config is read (`git_env`).
mod common;
use common::*;
use std::path::Path;
use std::time::{Duration, Instant};
use tmux_home::git::{
    self,
    badge::{Phase, RepoStatus},
    exec::{Git, GitError},
    refs::RefsMemo,
    repo::{self, Operation},
    scan,
    status::{self, file_diff},
};

fn paths(dir: &Path) -> repo::RepoPaths {
    repo::resolve(dir).expect("a repo")
}

async fn full(dir: &Path) -> RepoStatus {
    git::full_status(&Git::default(), &paths(dir), &mut RefsMemo::default()).await
}

// ---- file_diff (carried over from stray) ---------------------------------

#[tokio::test]
async fn file_diff_modified_tracked_file_combines_staged_and_unstaged() {
    git_env();
    let t = TempDir::new("modified");
    let root = t.repo("r");
    write(&root, "file.txt", "a\n");
    commit_all(&root, "file");
    // Stage one change...
    write(&root, "file.txt", "a\nb\n");
    git_at(&root, &["add", "file.txt"]);
    // ...then leave a further change unstaged.
    write(&root, "file.txt", "a\nb\nc\n");
    let diff = file_diff(&Git::default(), &root, "MM", "file.txt").await;
    assert!(diff.contains("+b"), "staged change:\n{diff}");
    assert!(diff.contains("+c"), "unstaged change:\n{diff}");
}

#[tokio::test]
async fn file_diff_untracked_file_shows_all_added_against_dev_null() {
    git_env();
    let t = TempDir::new("untracked");
    let root = t.repo("r");
    write(&root, "new.txt", "hello\nworld\n");
    let diff = file_diff(&Git::default(), &root, "??", "new.txt").await;
    assert!(diff.contains("/dev/null"), "diff:\n{diff}");
    assert!(diff.contains("+hello"), "diff:\n{diff}");
    assert!(diff.contains("+world"), "diff:\n{diff}");
}

#[tokio::test]
async fn file_diff_rename_uses_new_path_from_arrow_notation() {
    git_env();
    let t = TempDir::new("rename");
    let root = t.repo("r");
    write(&root, "old.txt", "hello\n");
    commit_all(&root, "old");
    git_at(&root, &["mv", "old.txt", "new.txt"]);
    let diff = file_diff(&Git::default(), &root, "R.", "old.txt -> new.txt").await;
    assert!(diff.contains("new.txt"), "diff:\n{diff}");
    assert!(diff.contains("+hello"), "diff:\n{diff}");
}

// ---- stray's collect / scan, over the async layer --------------------------

#[tokio::test]
async fn scan_finds_and_collects_repos_and_worktrees() {
    git_env();
    let t = TempDir::new("scan");
    let a = t.repo("a");
    let _b = t.repo("nested/b");
    write(&a, "dirty.txt", "x\n");
    git_at(&a, &["worktree", "add", "-q", "../a-wt", "-b", "wt"]);
    let git = Git::with_timeout(tmux_home::git::exec::SCAN_TIMEOUT);
    let repos = scan::inventory(&git, &t.0, 8).await;
    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    // the worktree is listed under its main repo, not on its own
    assert_eq!(names, ["a", "nested/b"], "{repos:#?}");
    let a = &repos[0];
    assert_eq!(a.status.untracked, 1);
    assert_eq!(a.worktrees.len(), 2);
    assert!(!a.has_remote && a.no_remote());
    assert_eq!(a.branches.len(), 2);
}

// ---- item 1: scrubbed env, read-only, timeout ------------------------------

#[tokio::test]
async fn every_call_is_scrubbed_and_read_only() {
    git_env();
    let t = TempDir::new("scrub");
    let root = t.repo("r");
    let log = t.0.join("env.log");
    let fake = fake_git(
        &t.0,
        &format!("{{ env; echo ARGS \"$@\"; }} > '{}'", log.display()),
    );
    // SAFETY: single-threaded tests
    unsafe {
        std::env::set_var("GIT_DIR", "/nonexistent/.git");
        std::env::set_var("GIT_WORK_TREE", "/nonexistent");
        std::env::set_var("GIT_INDEX_FILE", "/nonexistent/index");
        std::env::set_var("TMUX_HOME_GIT", &fake);
    }
    let _ = Git::default().run(&root, &["status"]).await;
    let seen = std::fs::read_to_string(&log).unwrap();
    for gone in ["GIT_DIR=", "GIT_WORK_TREE=", "GIT_INDEX_FILE="] {
        assert!(!seen.contains(gone), "{gone} leaked:\n{seen}");
    }
    for kept in [
        "GIT_OPTIONAL_LOCKS=0",
        "LC_ALL=C",
        "GIT_CONFIG_GLOBAL=/dev/null",
        "ARGS --no-optional-locks",
        "log.showSignature=false",
    ] {
        assert!(seen.contains(kept), "{kept} missing:\n{seen}");
    }
    // with the real git, the inherited GIT_DIR doesn't redirect the query
    unsafe { std::env::remove_var("TMUX_HOME_GIT") };
    write(&root, "u.txt", "x");
    let st = full(&root).await;
    assert_eq!((st.branch.as_str(), st.untracked), ("main", 1), "{st:?}");
    unsafe {
        std::env::remove_var("GIT_DIR");
        std::env::remove_var("GIT_WORK_TREE");
        std::env::remove_var("GIT_INDEX_FILE");
    }
}

#[tokio::test]
async fn status_never_rewrites_the_index() {
    git_env();
    let t = TempDir::new("ro");
    let root = t.repo("r");
    // a stat-dirty file: a status with optional locks would refresh the index
    std::thread::sleep(Duration::from_millis(20));
    write(&root, "README", "hello\n");
    let index = root.join(".git/index");
    let before = std::fs::metadata(&index).unwrap().modified().unwrap();
    let st = full(&root).await;
    assert!(!st.dirty(), "{st:?}");
    let after = std::fs::metadata(&index).unwrap().modified().unwrap();
    assert_eq!(before, after, "the index was written");
    assert!(!root.join(".git/index.lock").exists());
}

#[tokio::test]
async fn a_hung_git_times_out_and_is_killed() {
    git_env();
    let t = TempDir::new("hang");
    let pid = t.0.join("pid");
    let fake = fake_git(
        &t.0,
        &format!("echo $$ > '{}'; exec sleep 30", pid.display()),
    );
    unsafe { std::env::set_var("TMUX_HOME_GIT", &fake) };
    let t0 = Instant::now();
    // long enough for the stand-in to start and write its pid under load
    let r = Git::with_timeout(Duration::from_millis(1500))
        .run(&t.0, &["status"])
        .await;
    unsafe { std::env::remove_var("TMUX_HOME_GIT") };
    assert_eq!(r, Err(GitError::Timeout));
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let pid = std::fs::read_to_string(&pid).unwrap();
    // killed: gone, or a zombie until tokio's reaper collects it
    wait_until("the hung git to be killed", || {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        stat.trim().is_empty() || stat.trim().starts_with('Z')
    });
}

#[tokio::test]
async fn the_semaphore_bounds_concurrent_gits() {
    git_env();
    let t = TempDir::new("gate");
    let fake = fake_git(&t.0, "sleep 0.3");
    unsafe { std::env::set_var("TMUX_HOME_GIT", &fake) };
    let git = Git {
        gate: Some(std::sync::Arc::new(tokio::sync::Semaphore::new(2))),
        ..Git::default()
    };
    let t0 = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let (g, d) = (git.clone(), t.0.clone());
        set.spawn(async move { g.run(&d, &["x"]).await });
    }
    set.join_all().await;
    unsafe { std::env::remove_var("TMUX_HOME_GIT") };
    // 4 calls of 0.3 s, 2 at a time: two rounds
    assert!(
        t0.elapsed() >= Duration::from_millis(550),
        "{:?}",
        t0.elapsed()
    );
}

// ---- item 2: untracked files always counted --------------------------------

#[tokio::test]
async fn untracked_counted_despite_show_untracked_no() {
    git_env();
    let t = TempDir::new("untr");
    let root = t.repo("r");
    git_at(&root, &["config", "status.showUntrackedFiles", "no"]);
    write(&root, "new.txt", "x");
    let st = full(&root).await;
    assert_eq!(st.untracked, 1, "{st:?}");
    assert_eq!(st.badge_text(), "main ?");
}

// ---- item 3: unpushed commits, one fork per branch -------------------------

#[tokio::test]
async fn unpushed_counts_and_keeps_twenty() {
    git_env();
    let t = TempDir::new("unp");
    let root = t.repo("r");
    with_origin(&root);
    git_at(&root, &["switch", "-q", "-c", "work"]);
    for i in 0..25 {
        write(&root, "f", &i.to_string());
        commit_all(&root, &format!("c{i}"));
    }
    let (n, capped, commits) = status::unpushed(&Git::default(), &root, "refs/heads/work", true)
        .await
        .unwrap();
    assert_eq!((n, capped, commits.len()), (25, false, 20));
    assert!(commits[0].ends_with(" c24"), "newest first: {commits:?}");
    // nothing unpushed on main
    let (n, ..) = status::unpushed(&Git::default(), &root, "refs/heads/main", true)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

// ---- item 4: the refs memo -------------------------------------------------

#[tokio::test]
async fn refs_memo_probes_once_when_nothing_changed() {
    git_env();
    let t = TempDir::new("memo");
    let root = t.repo("r");
    with_origin(&root);
    for b in ["a", "b", "c"] {
        git_at(&root, &["switch", "-q", "-c", b, "main"]);
        write(&root, b, b);
        commit_all(&root, b);
    }
    git_at(&root, &["switch", "-q", "main"]);
    let (git, p) = (Git::default(), paths(&root));
    let mut memo = RefsMemo::default();
    assert!(memo.refresh(&git, &p).await.unwrap());
    let first = memo.last_forks;
    // probe + config + worktree list + 3 × log + integration checks
    assert!(first >= 6, "{first}");
    assert_eq!(memo.fields.stray().len(), 3);
    // unchanged: probe, config and worktree list only
    assert!(!memo.refresh(&git, &p).await.unwrap());
    assert_eq!(memo.last_forks, 3);
    // one branch moves: only it is recomputed
    git_at(&root, &["switch", "-q", "a"]);
    write(&root, "a2", "x");
    commit_all(&root, "a2");
    assert!(memo.refresh(&git, &p).await.unwrap());
    assert!(
        memo.last_forks < first,
        "{} forks, first {first}",
        memo.last_forks
    );
    assert_eq!(
        memo.fields.branch("a").map(|b| b.unpushed),
        Some(2),
        "{:?}",
        memo.fields
    );
    // a push changes the remotes: recomputed, a is no longer stray
    git_at(&root, &["push", "-q", "-u", "origin", "a"]);
    assert!(memo.refresh(&git, &p).await.unwrap());
    let mut stray = memo.fields.stray();
    stray.sort();
    assert_eq!(stray, ["b", "c"]);
}

// ---- item 5: operation in progress, conflicts ------------------------------

#[tokio::test]
async fn merge_conflict_shows_conflicts_and_the_operation() {
    git_env();
    let t = TempDir::new("conflict");
    let root = t.repo("r");
    git_at(&root, &["switch", "-q", "-c", "other"]);
    write(&root, "README", "theirs\n");
    commit_all(&root, "theirs");
    git_at(&root, &["switch", "-q", "main"]);
    write(&root, "README", "ours\n");
    commit_all(&root, "ours");
    let out = std::process::Command::new("git")
        .args(["-C", root.to_str().unwrap(), "merge", "other"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(!out.status.success(), "the merge conflicts");
    let st = full(&root).await;
    assert_eq!(st.operation, Some(Operation::Merge));
    assert_eq!(st.conflicts, 1);
    // `other` is unmerged in a repo with no remote: stray too
    assert_eq!(st.badge_text(), "main ✘ ⚠1 ↻");
}

#[tokio::test]
async fn stashes_from_the_reflog() {
    git_env();
    let t = TempDir::new("stash");
    let root = t.repo("r");
    for i in 0..2 {
        write(&root, "README", &format!("change {i}\n"));
        git_at(&root, &["stash", "-q"]);
    }
    assert_eq!(repo::stash_count(&root.join(".git")), 2);
    git_at(&root, &["stash", "drop", "-q", "stash@{1}"]);
    let st = full(&root).await;
    assert_eq!(st.stashes, 1);
    assert_eq!(st.badge_text(), "main $1");
}

// ---- item 6: worktrees -----------------------------------------------------

#[tokio::test]
async fn linked_worktree_flags() {
    git_env();
    let t = TempDir::new("wt");
    let root = t.repo("app");
    git_at(
        &root,
        &["worktree", "add", "-q", "../app.feat-x", "-b", "feat/x"],
    );
    git_at(
        &root,
        &["worktree", "add", "-q", "../elsewhere", "-b", "fix"],
    );
    git_at(&root, &["worktree", "add", "-q", "--detach", "../app.det"]);
    git_at(&root, &["worktree", "lock", "../app.feat-x"]);
    let wt = t.0.join("app.feat-x");
    let st = full(&wt).await;
    assert!(st.linked && st.locked && !st.mismatch, "{st:?}");
    assert_eq!(st.main_root.as_deref(), root.to_str());
    assert_eq!(st.badge_text(), "feat/x (wt) ⊞");
    let st = full(&t.0.join("elsewhere")).await;
    assert!(st.mismatch, "not named after its branch: {st:?}");
    assert_eq!(st.badge_text(), "fix (wt) ⚑");
    let st = full(&t.0.join("app.det")).await;
    assert!(st.detached, "{st:?}");
    assert!(st.badge_text().ends_with(" (wt) ⊘"), "{}", st.badge_text());
    // the main worktree counts its linked ones
    let st = full(&root).await;
    assert_eq!((st.linked, st.worktrees), (false, 3));
    assert_eq!(st.badge_text(), "main");
    // a worktree whose directory is gone is prunable (seen from the main)
    std::fs::remove_dir_all(t.0.join("elsewhere")).unwrap();
    let mut memo = RefsMemo::default();
    memo.refresh(&Git::default(), &paths(&root)).await.unwrap();
    let gone = memo
        .fields
        .worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some("fix"))
        .unwrap();
    assert!(gone.missing);
}

// ---- item 7: default branch, read-only --------------------------------------

#[tokio::test]
async fn default_branch_from_config_remote_head_or_inference() {
    git_env();
    let t = TempDir::new("default");
    let root = t.repo("r");
    git_at(&root, &["switch", "-q", "-c", "dev"]);
    // local inference: main
    assert_eq!(full(&root).await.default_branch.as_deref(), Some("main"));
    // origin/HEAD wins
    with_origin(&root);
    git_at(&root, &["push", "-q", "origin", "dev"]);
    git_at(&root, &["remote", "set-head", "origin", "dev"]);
    assert_eq!(full(&root).await.default_branch.as_deref(), Some("dev"));
    // worktrunk's setting wins over both
    git_at(&root, &["config", "worktrunk.default-branch", "release"]);
    assert_eq!(full(&root).await.default_branch.as_deref(), Some("release"));
    // nothing written back
    git_at(&root, &["config", "--unset", "worktrunk.default-branch"]);
    full(&root).await;
    let cfg = std::fs::read_to_string(root.join(".git/config")).unwrap();
    assert!(!cfg.contains("worktrunk"), "{cfg}");
}

// ---- item 8: stray branches and integration --------------------------------

#[tokio::test]
async fn stray_branches_exclude_integrated_ones() {
    git_env();
    let t = TempDir::new("stray");
    let root = t.repo("r");
    with_origin(&root);
    // merged into local main (main not pushed yet): integrated, ancestor
    git_at(&root, &["switch", "-q", "-c", "merged"]);
    write(&root, "m", "m");
    commit_all(&root, "m");
    git_at(&root, &["switch", "-q", "main"]);
    git_at(&root, &["merge", "-q", "--ff-only", "merged"]);
    // squash-merged: main's tree equals the branch's
    git_at(&root, &["switch", "-q", "-c", "squashed"]);
    write(&root, "s", "s");
    commit_all(&root, "s1");
    write(&root, "s", "s2");
    commit_all(&root, "s2");
    git_at(&root, &["switch", "-q", "main"]);
    git_at(&root, &["merge", "-q", "--squash", "squashed"]);
    git_at(&root, &["commit", "-q", "-m", "squash"]);
    git_at(&root, &["push", "-q", "origin", "main"]);
    // real stray work
    git_at(&root, &["switch", "-q", "-c", "spike"]);
    write(&root, "x", "x");
    commit_all(&root, "spike");
    // pushed, then its upstream deleted: gone, still stray
    git_at(&root, &["switch", "-q", "-c", "old", "main"]);
    write(&root, "o", "o");
    commit_all(&root, "old");
    git_at(&root, &["push", "-q", "-u", "origin", "old"]);
    write(&root, "o", "o2");
    commit_all(&root, "old2");
    git_at(&root, &["push", "-q", "origin", "--delete", "old"]);
    git_at(&root, &["switch", "-q", "main"]);
    let st = full(&root).await;
    let mut names = st.stray_names.clone();
    names.sort();
    assert_eq!(names, ["old", "spike"], "{st:?}");
    assert_eq!(st.badge_text(), "main | ⚠2");
}

// ---- fast-first stages -------------------------------------------------------

#[tokio::test]
async fn stages_fill_the_status_in_order() {
    git_env();
    let t = TempDir::new("stages");
    let root = t.repo("r");
    with_origin(&root);
    write(&root, "README", "edit\n");
    let p = paths(&root);
    let mut st = RepoStatus::default();
    git::apply_head(&mut st, &p);
    assert_eq!((st.branch.as_str(), st.phase), ("main", Phase::Head));
    assert_eq!(st.badge_text(), "main", "no sync claim before status");
    let s = git::read_status(&Git::default(), &p).await.unwrap();
    git::apply_status(&mut st, &s);
    assert_eq!(st.phase, Phase::Status);
    assert_eq!(st.badge_text(), "main ! |");
    let mut memo = RefsMemo::default();
    memo.refresh(&Git::default(), &p).await.unwrap();
    git::apply_refs(&mut st, &memo.fields, &p);
    assert_eq!(st.phase, Phase::Refs);
    assert_eq!(st.default_branch.as_deref(), Some("main"));
    assert!(st.has_remote);
}

// ---- review fixes: no repo-configured command ever runs ----------------------

/// Stops any fsmonitor daemon a failing test may have left in `dir`.
struct StopFsmonitor(std::path::PathBuf);

impl Drop for StopFsmonitor {
    fn drop(&mut self) {
        let _ = std::process::Command::new("git")
            .args(["-C", self.0.to_str().unwrap(), "fsmonitor--daemon", "stop"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output();
    }
}

fn fsmonitor_daemons(dir: &Path) -> usize {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "command="])
        .output()
        .unwrap();
    let _ = dir;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("fsmonitor--daemon"))
        .count()
}

#[tokio::test]
async fn fsmonitor_hooks_and_daemons_never_run() {
    git_env();
    let t = TempDir::new("fsmon");
    let root = t.repo("r");
    let _stop = StopFsmonitor(root.clone());
    let marker = t.0.join("fsmonitor-ran");
    let hook = fake_git(&t.0, &format!("touch '{}'", marker.display()));
    git_at(&root, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
    write(&root, "README", "edited\n");
    let st = full(&root).await;
    assert_eq!(st.modified, 1, "{st:?}");
    assert!(!marker.exists(), "the fsmonitor hook ran");
    // the builtin daemon: never started
    let before = fsmonitor_daemons(&root);
    git_at(&root, &["config", "core.fsmonitor", "true"]);
    full(&root).await;
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !root.join(".git/fsmonitor--daemon").exists(),
        "an fsmonitor daemon started"
    );
    assert_eq!(fsmonitor_daemons(&root), before);
}

#[tokio::test]
async fn clean_filters_and_textconv_never_run() {
    git_env();
    let t = TempDir::new("filter");
    let root = t.repo("r");
    write(&root, "data.txt", "one\n");
    commit_all(&root, "data");
    let marker = t.0.join("filter-ran");
    let script = fake_git(&t.0, &format!("touch '{}'; cat", marker.display()));
    let conv = fake_git(&t.0, &format!("touch '{}'; cat \"$1\"", marker.display()));
    write(&root, ".gitattributes", "*.txt filter=mark diff=mark\n");
    commit_all(&root, "attrs");
    let s = script.to_str().unwrap();
    git_at(&root, &["config", "filter.mark.clean", s]);
    git_at(&root, &["config", "filter.mark.process", s]);
    git_at(
        &root,
        &["config", "diff.mark.textconv", conv.to_str().unwrap()],
    );
    git_at(&root, &["config", "diff.external", s]);
    // stat-dirty and content-changed: status must re-read it
    std::thread::sleep(Duration::from_millis(20));
    write(&root, "data.txt", "two\n");
    let _ = std::fs::remove_file(&marker);
    let st = full(&root).await;
    assert_eq!(st.modified, 1, "{st:?}");
    let diff = file_diff(&Git::default(), &root, " M", "data.txt").await;
    assert!(diff.contains("+two"), "{diff}");
    assert!(!marker.exists(), "a filter, textconv or external diff ran");
    // a driver named only in .git/info/attributes
    write(&root, ".git/info/attributes", "*.txt filter=local\n");
    git_at(&root, &["config", "filter.local.clean", s]);
    git_at(&root, &["config", "filter.local.required", "true"]);
    std::thread::sleep(Duration::from_millis(20));
    write(&root, "data.txt", "three\n");
    let st = full(&root).await;
    assert!(!st.stale, "{st:?}");
    assert_eq!(st.modified, 1, "{st:?}");
    assert!(!marker.exists(), "the info/attributes filter ran");
}

#[tokio::test]
async fn a_timeout_kills_the_whole_process_group() {
    git_env();
    let t = TempDir::new("pgrp");
    let pid = t.0.join("child-pid");
    let fake = fake_git(
        &t.0,
        &format!("sleep 30 & echo $! > '{}'; wait", pid.display()),
    );
    unsafe { std::env::set_var("TMUX_HOME_GIT", &fake) };
    // long enough for the stand-in to start and write its pid under load
    let r = Git::with_timeout(Duration::from_millis(1500))
        .run(&t.0, &["status"])
        .await;
    unsafe { std::env::remove_var("TMUX_HOME_GIT") };
    assert_eq!(r, Err(GitError::Timeout));
    let pid = std::fs::read_to_string(&pid).unwrap();
    wait_until("git's child to be killed too", || {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        stat.trim().is_empty() || stat.trim().starts_with('Z')
    });
}

#[tokio::test]
async fn no_lazy_fetch_and_no_transport() {
    git_env();
    let t = TempDir::new("nolazy");
    let log = t.0.join("env.log");
    let fake = fake_git(
        &t.0,
        &format!("{{ env; echo ARGS \"$@\"; }} > '{}'", log.display()),
    );
    unsafe { std::env::set_var("TMUX_HOME_GIT", &fake) };
    let _ = Git::default().run(&t.0, &["status"]).await;
    unsafe { std::env::remove_var("TMUX_HOME_GIT") };
    let seen = std::fs::read_to_string(&log).unwrap();
    for want in [
        "GIT_NO_LAZY_FETCH=1",
        "core.fsmonitor=false",
        "protocol.allow=never",
        "core.hooksPath=/dev/null",
    ] {
        assert!(seen.contains(want), "{want} missing:\n{seen}");
    }
}

// ---- review fix: what the refs memo key misses -------------------------------

#[tokio::test]
async fn worktree_and_config_changes_show_without_a_ref_moving() {
    git_env();
    let t = TempDir::new("memo2");
    let root = t.repo("app");
    git_at(&root, &["branch", "other"]);
    git_at(
        &root,
        &["worktree", "add", "-q", "../app.feat", "-b", "feat"],
    );
    let wt = t.0.join("app.feat");
    let (git, p) = (Git::default(), paths(&wt));
    let mut memo = RefsMemo::default();
    let mut st = RepoStatus::default();
    let again = async |memo: &mut RefsMemo, st: &mut RepoStatus| {
        memo.refresh(&git, &p).await.unwrap();
        git::apply_head(st, &p);
        git::apply_refs(st, &memo.fields, &p);
    };
    again(&mut memo, &mut st).await;
    assert!(!st.locked && !st.has_remote && !st.mismatch, "{st:?}");
    // lock: no ref moves
    git_at(&root, &["worktree", "lock", "../app.feat"]);
    again(&mut memo, &mut st).await;
    assert!(st.locked, "lock not seen: {st:?}");
    // a branch switch inside the linked worktree: no ref moves either
    git_at(&wt, &["switch", "-q", "other"]);
    again(&mut memo, &mut st).await;
    let me = memo
        .fields
        .worktrees
        .iter()
        .find(|w| w.path.ends_with("app.feat"))
        .unwrap();
    assert_eq!(
        me.branch.as_deref(),
        Some("other"),
        "{:?}",
        memo.fields.worktrees
    );
    assert!(st.mismatch, "app.feat on `other`: {st:?}");
    // a remote added: config only
    git_at(&root, &["remote", "add", "origin", "/nonexistent.git"]);
    again(&mut memo, &mut st).await;
    assert!(st.has_remote, "remote add not seen: {st:?}");
    // worktrunk's default branch: config only
    git_at(&root, &["config", "worktrunk.default-branch", "other"]);
    again(&mut memo, &mut st).await;
    assert_eq!(st.default_branch.as_deref(), Some("other"));
}

// ---- review fix: ⚠ in a repo with no remote ----------------------------------

/// No remote: every commit is "unpushed to nowhere" (as stray counts
/// them); `⚠` counts the non-default branches not merged into the default
/// branch.
#[tokio::test]
async fn stray_without_a_remote_is_unmerged_non_default_branches() {
    git_env();
    let t = TempDir::new("noremote");
    let root = t.repo("r");
    git_at(&root, &["branch", "same"]); // at main: integrated
    git_at(&root, &["switch", "-q", "-c", "merged"]);
    write(&root, "m", "m");
    commit_all(&root, "m");
    git_at(&root, &["switch", "-q", "main"]);
    git_at(&root, &["merge", "-q", "--ff-only", "merged"]);
    git_at(&root, &["switch", "-q", "-c", "spike"]);
    write(&root, "s", "s");
    commit_all(&root, "s1");
    write(&root, "s", "s2");
    commit_all(&root, "s2");
    git_at(&root, &["switch", "-q", "main"]);
    let st = full(&root).await;
    assert!(!st.has_remote);
    assert_eq!(st.stray_names, ["spike"], "{st:?}");
    assert_eq!(st.badge_text(), "main ⚠1");
}

// ---- re-review: odd driver names, from config, in sha1 and sha256 repos ----

/// A repo of `format` with `x.odd` committed, then `.git/info/attributes`
/// naming `attr` for it and the config keys `keys` set to a script that
/// leaves `marker`; finally `x.odd` is changed (stat-dirty and different).
fn odd_repo(
    t: &TempDir,
    format: &str,
    attr: &str,
    keys: &[&str],
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = t.repo_in(&format!("r-{format}"), format);
    write(&root, "x.odd", "one\n");
    commit_all(&root, "odd");
    let marker = t.0.join(format!("marker-{format}"));
    let script = fake_git(&t.0, &format!("touch '{}'; cat", marker.display()));
    write(&root, ".git/info/attributes", &format!("*.odd {attr}\n"));
    for k in keys {
        git_at(&root, &["config", k, script.to_str().unwrap()]);
    }
    std::thread::sleep(Duration::from_millis(20));
    write(&root, "x.odd", "two\n");
    (root, marker)
}

#[tokio::test]
async fn odd_driver_names_never_run() {
    git_env();
    for format in ["sha1", "sha256"] {
        for (attr, keys) in [
            (
                "filter=a+b",
                &["filter.a+b.clean", "filter.a+b.process"][..],
            ),
            ("diff=x+y", &["diff.x+y.textconv", "diff.x+y.command"][..]),
            // a name attributes can't even spell: config is the only layer
            (
                "filter=plain",
                &["filter.with space.clean", "filter.plain.clean"][..],
            ),
        ] {
            let t = TempDir::new("odd");
            let (root, marker) = odd_repo(&t, format, attr, keys);
            let st = full(&root).await;
            assert!(!st.limited && !st.stale, "{format} {attr}: {st:?}");
            assert_eq!(st.modified, 1, "{format} {attr}: {st:?}");
            let diff = file_diff(&Git::default(), &root, " M", "x.odd").await;
            assert!(diff.contains("+two"), "{format} {attr}: {diff}");
            assert!(!marker.exists(), "{format} {attr}: a driver ran");
        }
    }
}

/// A key `-c` can't carry (an `=` in the subsection): fail closed — no
/// status, no diff checks, a HEAD-only badge marked limited.
#[tokio::test]
async fn an_unrepresentable_key_fails_closed() {
    git_env();
    for format in ["sha1", "sha256"] {
        let t = TempDir::new("eq");
        let (root, marker) = odd_repo(&t, format, "filter=a=b", &["filter.a=b.clean"]);
        let st = full(&root).await;
        assert!(st.limited, "{format}: {st:?}");
        assert_eq!(st.branch, "main");
        assert_eq!(st.phase, tmux_home::git::badge::Phase::Head);
        assert_eq!(st.badge_text(), "main ⊗", "{format}");
        let diff = file_diff(&Git::default(), &root, " M", "x.odd").await;
        assert!(!diff.contains("+two"), "{format}: diffed anyway: {diff}");
        assert!(!marker.exists(), "{format}: the driver ran");
    }
}
