//! The repo model, vendored from stray (https://github.com/tim-codes/stray,
//! `src/model.rs` at v0.3.0; MIT, Copyright (c) 2026 Tim O'Connell — see
//! NOTICE). Changes: `Worktree` gains the `worktree list --porcelain`
//! attributes stray ignored (`locked`, `detached`, `bare`; spec §6 item 6)
//! and `Branch` an `integrated` flag (item 8).

use std::path::{Path, PathBuf};

/// Path for display: abbreviates $HOME to ~.
pub fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(rest) = s.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    s
}

#[derive(Debug, Clone, PartialEq)]
pub enum RepoKind {
    Normal,
    Bare,
    Submodule,
    /// A worktree checkout whose main repo lives outside the scanned root.
    OrphanWorktree {
        main: PathBuf,
    },
}

/// One changed path, `git status -s` style: a 2-char XY code plus path
/// (renames render as "old -> new").
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub code: String,
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct StatusCounts {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicts: usize,
    pub entries: Vec<FileEntry>,
}

impl StatusCounts {
    pub fn total(&self) -> usize {
        self.staged + self.unstaged + self.untracked + self.conflicts
    }
}

#[derive(Debug, Clone)]
pub struct Branch {
    pub name: String,
    pub is_head: bool,
    pub upstream: Option<String>,
    /// Upstream is configured but the remote branch no longer exists.
    pub gone: bool,
    pub ahead: usize,
    pub behind: usize,
    /// Commits on this branch that exist on no remote ref at all.
    pub unpushed: usize,
    pub unpushed_capped: bool,
    /// The unpushed commits themselves ("abc1234 subject"), newest first, capped.
    pub commits: Vec<String>,
    pub committer_unix: i64,
    pub age: String,
    pub subject: String,
    /// Merged into the default branch by a cheap check (same commit,
    /// ancestor, no added changes, equal trees); `crate::git::refs`.
    pub integrated: bool,
}

impl Branch {
    /// Branch has work not safely on a remote, or is out of sync with it.
    pub fn outstanding(&self) -> bool {
        self.unpushed > 0
            || self.upstream.is_none()
            || self.gone
            || self.ahead > 0
            || self.behind > 0
    }

    pub fn unpushed_display(&self) -> String {
        if self.unpushed_capped {
            format!("{}+", self.unpushed)
        } else {
            self.unpushed.to_string()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_main: bool,
    /// `prunable` in `worktree list`, or its directory is gone.
    pub missing: bool,
    pub locked: bool,
    pub detached: bool,
    pub bare: bool,
    pub status: StatusCounts,
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
    pub name: String,
    pub kind: RepoKind,
    pub head: String,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub has_remote: bool,
    pub status: StatusCounts,
    pub stashes: usize,
    pub branches: Vec<Branch>,
    pub worktrees: Vec<Worktree>,
    pub errors: Vec<String>,
    /// Matched by a ~/.strayignore entry; hidden unless toggled visible.
    pub ignored: bool,
}

impl Repo {
    pub fn dirty(&self) -> usize {
        self.status.total()
    }

    pub fn unpushed_total(&self) -> usize {
        self.branches.iter().map(|b| b.unpushed).sum()
    }

    /// Dirty file count across linked (non-main) worktrees.
    pub fn worktree_dirty(&self) -> usize {
        self.worktrees
            .iter()
            .filter(|w| !w.is_main)
            .map(|w| w.status.total())
            .sum()
    }

    pub fn no_remote(&self) -> bool {
        !self.has_remote
            && !self.branches.is_empty()
            && !matches!(self.kind, RepoKind::OrphanWorktree { .. })
    }

    pub fn needs_attention(&self) -> bool {
        self.dirty() > 0
            || self.unpushed_total() > 0
            || self.stashes > 0
            || self.worktree_dirty() > 0
            || self.branches.iter().any(|b| b.gone)
            || self.no_remote()
            || !self.errors.is_empty()
    }

    /// One-word-ish summary of why this repo needs attention (or "clean").
    pub fn summary(&self) -> String {
        if !self.needs_attention() {
            return "clean".to_string();
        }
        let mut parts = Vec::new();
        if self.dirty() > 0 {
            parts.push(format!("{} dirty", self.dirty()));
        }
        if self.unpushed_total() > 0 {
            parts.push(format!("{} unpushed", self.unpushed_total()));
        }
        if self.stashes > 0 {
            parts.push(format!("{} stashed", self.stashes));
        }
        if self.worktree_dirty() > 0 {
            parts.push(format!("{} dirty in worktrees", self.worktree_dirty()));
        }
        if self.no_remote() {
            parts.push("no remote".to_string());
        }
        if !self.errors.is_empty() {
            parts.push("errors".to_string());
        }
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_branch() -> Branch {
        Branch {
            name: "main".to_string(),
            is_head: true,
            upstream: Some("origin/main".to_string()),
            gone: false,
            ahead: 0,
            behind: 0,
            unpushed: 0,
            unpushed_capped: false,
            commits: Vec::new(),
            committer_unix: 0,
            age: String::new(),
            subject: String::new(),
            integrated: false,
        }
    }

    fn base_repo() -> Repo {
        Repo {
            root: PathBuf::from("/tmp/repo"),
            name: "repo".to_string(),
            kind: RepoKind::Normal,
            head: "main".to_string(),
            detached: false,
            upstream: Some("origin/main".to_string()),
            ahead: 0,
            behind: 0,
            has_remote: true,
            status: StatusCounts::default(),
            stashes: 0,
            branches: Vec::new(),
            worktrees: Vec::new(),
            errors: Vec::new(),
            ignored: false,
        }
    }

    // ---- Branch::outstanding ---------------------------------------

    #[test]
    fn branch_clean_and_synced_is_not_outstanding() {
        assert!(!base_branch().outstanding());
    }

    #[test]
    fn branch_with_unpushed_commits_is_outstanding() {
        let mut b = base_branch();
        b.unpushed = 1;
        assert!(b.outstanding());
    }

    #[test]
    fn branch_with_no_upstream_is_outstanding() {
        let mut b = base_branch();
        b.upstream = None;
        assert!(b.outstanding());
    }

    #[test]
    fn branch_gone_is_outstanding() {
        let mut b = base_branch();
        b.gone = true;
        assert!(b.outstanding());
    }

    #[test]
    fn branch_ahead_or_behind_is_outstanding() {
        let mut b = base_branch();
        b.ahead = 1;
        assert!(b.outstanding());

        let mut b = base_branch();
        b.behind = 1;
        assert!(b.outstanding());
    }

    // ---- Branch::unpushed_display -----------------------------------

    #[test]
    fn unpushed_display_uncapped() {
        let mut b = base_branch();
        b.unpushed = 42;
        b.unpushed_capped = false;
        assert_eq!(b.unpushed_display(), "42");
    }

    #[test]
    fn unpushed_display_capped_appends_plus() {
        let mut b = base_branch();
        b.unpushed = 1000;
        b.unpushed_capped = true;
        assert_eq!(b.unpushed_display(), "1000+");
    }

    // ---- Repo::needs_attention ---------------------------------------

    #[test]
    fn clean_repo_does_not_need_attention() {
        assert!(!base_repo().needs_attention());
    }

    #[test]
    fn dirty_status_needs_attention() {
        let mut r = base_repo();
        r.status.staged = 1;
        assert!(r.needs_attention());
    }

    #[test]
    fn unpushed_branch_needs_attention() {
        let mut r = base_repo();
        let mut b = base_branch();
        b.unpushed = 1;
        r.branches.push(b);
        assert!(r.needs_attention());
    }

    #[test]
    fn stashes_need_attention() {
        let mut r = base_repo();
        r.stashes = 1;
        assert!(r.needs_attention());
    }

    #[test]
    fn errors_need_attention() {
        let mut r = base_repo();
        r.errors.push("boom".to_string());
        assert!(r.needs_attention());
    }

    #[test]
    fn no_remote_with_branches_needs_attention() {
        let mut r = base_repo();
        r.has_remote = false;
        r.branches.push(base_branch());
        assert!(r.needs_attention());
    }

    #[test]
    fn no_remote_without_branches_does_not_need_attention() {
        let mut r = base_repo();
        r.has_remote = false;
        assert!(!r.no_remote());
        assert!(!r.needs_attention());
    }
}
