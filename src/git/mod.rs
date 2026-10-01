//! Git status for badges and (later) the repos view (spec §6).
//!
//! - `model`, `status`, `scan`, `ignore`: stray's git layer, vendored (MIT,
//!   same author; see NOTICE), made async over `exec::Git`.
//! - `exec`: how every git call runs (scrubbed env, read-only, timeout,
//!   semaphore; item 1).
//! - `repo`: what needs no fork — cwd → repo, HEAD, operation in progress,
//!   stash count, change stamp (item 5).
//! - `refs`: the refs snapshot memo and what derives from refs: default
//!   branch, stray branches, integration, worktrees (items 4, 6, 7, 8).
//! - `badge`: the snapshot types and the badge text.
//!
//! A badge refresh runs in three stages, fast first, each published as it
//! lands: `head` (file reads only), `status` (one `git status`), `refs`
//! (one `for-each-ref` probe; more only when refs changed).

pub mod badge;
pub mod exec;
pub mod ignore;
pub mod model;
pub mod refs;
pub mod repo;
pub mod scan;
pub mod status;

use badge::{Phase, RepoStatus, STRAY_NAMES};
use exec::{Git, GitError};
use refs::{RefFields, RefsMemo};
use repo::{Head, RepoPaths};
use std::path::Path;

/// Stage 1, no fork: HEAD, linked-worktree, operation in progress and stash
/// count from files. Keeps the other fields (a later stage refreshes them).
pub fn apply_head(st: &mut RepoStatus, p: &RepoPaths) {
    match repo::read_head(&p.git_dir) {
        Head::Branch(b) => {
            st.branch = b;
            st.detached = false;
        }
        Head::Detached(c) => {
            st.branch = c;
            st.detached = true;
        }
        Head::Unknown => {}
    }
    st.linked = p.linked;
    st.main_root = p.main_root().map(|m| m.to_string_lossy().into_owned());
    st.operation = repo::operation_in_progress(&p.git_dir);
    st.stashes = repo::stash_count(&p.common_dir) as u32;
}

/// What stage 2 found.
#[derive(Clone, Debug, Default)]
pub struct StatusPart {
    pub counts: model::StatusCounts,
    pub info: status::HeadInfo,
}

pub fn apply_status(st: &mut RepoStatus, s: &StatusPart) {
    let c = &s.counts;
    st.staged = c.staged as u32;
    st.modified = c.unstaged as u32;
    st.untracked = c.untracked as u32;
    st.conflicts = c.conflicts as u32;
    st.upstream = s.info.upstream.clone();
    st.gone = s.info.gone;
    st.ahead = s.info.ahead as u32;
    st.behind = s.info.behind as u32;
    if s.info.detached {
        st.detached = true;
    } else if s.info.head != "?" && !s.info.head.is_empty() {
        // status knows the branch even where HEAD isn't a file (reftable)
        st.branch = s.info.head.clone();
        st.detached = false;
    }
    if st.phase == Phase::Head {
        st.phase = Phase::Status;
    }
}

/// Stage 3: the refs memo's fields onto this working tree's status.
pub fn apply_refs(st: &mut RepoStatus, f: &RefFields, p: &RepoPaths) {
    let stray = f.stray();
    st.stray = stray.len() as u32;
    st.stray_names = stray.into_iter().take(STRAY_NAMES).collect();
    st.default_branch = f.default_branch.clone();
    st.has_remote = f.has_remote;
    st.gone = !st.detached && f.branch(&st.branch).is_some_and(|b| b.gone);
    st.worktrees = f.worktrees.iter().filter(|w| !w.is_main).count() as u32;
    let canon = |q: &Path| q.canonicalize().unwrap_or_else(|_| q.to_path_buf());
    let root = canon(&p.root);
    let me = f.worktrees.iter().find(|w| canon(&w.path) == root);
    st.prunable = me.is_some_and(|w| w.missing);
    st.locked = me.is_some_and(|w| w.locked);
    st.mismatch = me.is_some_and(|w| mismatch(w, &f.worktrees));
    st.phase = Phase::Refs;
}

/// `⚑`: the worktree's branch is checked out in another worktree too, or a
/// linked worktree's directory isn't named after its branch: the name must
/// be the branch, or end in `.<branch>` or `-<branch>` (worktrunk's
/// templates), the branch sanitised as worktrunk does (`/` and `\` → `-`).
fn mismatch(w: &model::Worktree, all: &[model::Worktree]) -> bool {
    let Some(b) = &w.branch else { return false };
    let dup = all
        .iter()
        .filter(|o| o.branch.as_deref() == Some(b.as_str()))
        .count()
        > 1;
    if dup {
        return true;
    }
    if w.is_main {
        return false;
    }
    let dir = w
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let b = b.replace(['/', '\\'], "-");
    let named = dir == b || dir.ends_with(&format!(".{b}")) || dir.ends_with(&format!("-{b}"));
    !named
}

/// Stage 2 for real: `git status` → `StatusPart`.
pub async fn read_status(git: &Git, p: &RepoPaths) -> Result<StatusPart, GitError> {
    let (counts, info) = status::status_counts(git, &p.root).await?;
    Ok(StatusPart { counts, info })
}

/// Stage 2 with the guard re-read right before it (`Git::run_fresh`): the
/// guarded `Git` for the calls after, and the status.
pub async fn read_status_fresh(git: &Git, p: &RepoPaths) -> Result<(Git, StatusPart), GitError> {
    let (g, out) = git.run_fresh(&p.root, status::STATUS_ARGS).await?;
    let (counts, info) = status::parse_status(&out);
    Ok((g, StatusPart { counts, info }))
}

/// The badge of a repo whose config names a command that can't be switched
/// off (`exec::Guard`): only what the HEAD stage read from files, marked
/// `limited` (`⊗`).
pub fn limited(st: &RepoStatus) -> RepoStatus {
    RepoStatus {
        branch: st.branch.clone(),
        detached: st.detached,
        linked: st.linked,
        main_root: st.main_root.clone(),
        operation: st.operation,
        stashes: st.stashes,
        limited: true,
        ..RepoStatus::default()
    }
}

/// All three stages at once, for one-off callers (`query`, tests, perf).
pub async fn full_status(git: &Git, p: &RepoPaths, memo: &mut RefsMemo) -> RepoStatus {
    let mut st = RepoStatus::default();
    apply_head(&mut st, p);
    let git = &match read_status_fresh(git, p).await {
        Ok((g, s)) => {
            apply_status(&mut st, &s);
            g
        }
        Err(GitError::Limited(_)) => return limited(&st),
        Err(e) => {
            st.stale = true;
            st.error = Some(e.to_string());
            return st;
        }
    };
    match memo.refresh(git, p).await {
        Ok(_) => apply_refs(&mut st, &memo.fields, p),
        Err(e) => {
            st.stale = true;
            st.error = Some(e.to_string());
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt(path: &str, branch: Option<&str>, main: bool) -> model::Worktree {
        model::Worktree {
            path: PathBuf::from(path),
            branch: branch.map(String::from),
            is_main: main,
            missing: false,
            locked: false,
            detached: branch.is_none(),
            bare: false,
            status: Default::default(),
        }
    }

    #[test]
    fn a_gone_upstream_shows_no_sync_at_the_status_stage() {
        let mut st = RepoStatus {
            branch: "x".into(),
            ..RepoStatus::default()
        };
        let (counts, info) = status::parse_status("# branch.head x\n# branch.upstream origin/x\n");
        apply_status(&mut st, &StatusPart { counts, info });
        assert!(st.gone);
        assert_eq!(st.badge_text(), "x");
    }

    #[test]
    fn mismatch_rules() {
        let all = vec![
            wt("/r", Some("main"), true),
            wt("/r.feat-x", Some("feat/x"), false),
            wt("/elsewhere", Some("fix"), false),
            wt("/dup", Some("main"), false),
            wt("/d", None, false),
        ];
        assert!(mismatch(&all[0], &all), "main is also checked out at /dup");
        assert!(!mismatch(&all[1], &all), "named after its branch");
        assert!(mismatch(&all[2], &all), "not named after its branch");
        assert!(mismatch(&all[3], &all));
        assert!(!mismatch(&all[4], &all), "detached");
    }

    /// `⚑` for a linked worktree: its directory is the branch (sanitised
    /// like worktrunk: `/` and `\` as `-`), or ends in `.<branch>` or
    /// `-<branch>`; containment isn't enough, short names included.
    #[test]
    fn mismatch_needs_the_branch_as_the_name_or_its_suffix() {
        let main = wt("/r", Some("main"), true);
        let ok = |dir: &str, b: &str| {
            let w = wt(dir, Some(b), false);
            !mismatch(&w, &[main.clone(), w.clone()])
        };
        assert!(ok("/x", "x"));
        assert!(ok("/app.x", "x"));
        assert!(ok("/app-x", "x"));
        assert!(ok("/app.feat-x", "feat/x"));
        assert!(ok("/feat-x", "feat\\x"));
        assert!(!ok("/box", "x"), "contains x, isn't named after it");
        assert!(!ok("/x-old", "x"));
        assert!(!ok("/appx", "x"));
        assert!(!ok("/feat", "feat/x"), "the branch contains the dir");
    }
}
