//! A repo's status as the snapshot carries it (`Snapshot::git`), and the
//! compact badge drawn from it (spec §6 "Badge symbols").
//!
//! `branch (wt) +!?✘ ⇡n ⇣n | $n ⚠n ↻⊟⊞⊘⚑⊗ ~`:
//! `+` staged · `!` modified · `?` untracked · `✘` conflicts ·
//! `⇡n ⇣n` ahead/behind upstream · `|` in sync with it · `$n` stashes ·
//! `⚠n` stray branches · `↻` operation in progress · `⊟` prunable ·
//! `⊞` locked · `⊘` detached · `⚑` branch/worktree mismatch ·
//! `⊗` limited (the repo's config names a command tmux-home can't switch
//! off: HEAD only) · `~` stale (the last check timed out or failed; the
//! values are older).
//! `↑↓` stay reserved for "vs default branch" (later).

use super::repo::Operation;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The git section of a snapshot: every repo some window or agent pane is
/// in, and which pane cwd is in which repo.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, Hash)]
pub struct GitSection {
    /// Pane cwd → its repo's root (only cwds inside a repo).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub paths: BTreeMap<String, String>,
    /// Repo (working tree) root → its status.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repos: BTreeMap<String, RepoStatus>,
}

impl GitSection {
    /// The status of the repo `cwd` is in, if known.
    pub fn for_cwd(&self, cwd: &str) -> Option<&RepoStatus> {
        self.repos.get(self.paths.get(cwd)?)
    }
}

/// How far a refresh has got (fast-first: each lands and is published
/// before the next starts).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Only HEAD, read from `.git/HEAD`.
    #[default]
    Head,
    /// Plus `git status`.
    Status,
    /// Plus the ref-derived fields.
    Refs,
}

/// One working tree's status. Only stable values: nothing here changes
/// with the clock, so the section hashes for change detection as is.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, Hash)]
pub struct RepoStatus {
    /// Branch name, or the short commit when detached; empty if unknown.
    pub branch: String,
    #[serde(default)]
    pub detached: bool,
    /// A linked worktree (`(wt)`), and its main worktree's directory.
    #[serde(default)]
    pub linked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_root: Option<String>,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The upstream branch no longer exists on the remote.
    #[serde(default)]
    pub gone: bool,
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
    #[serde(default)]
    pub staged: u32,
    #[serde(default)]
    pub modified: u32,
    #[serde(default)]
    pub untracked: u32,
    #[serde(default)]
    pub conflicts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<Operation>,
    #[serde(default)]
    pub stashes: u32,
    /// Stray branches (unpushed, no upstream or a gone one, not
    /// integrated), by name; at most `STRAY_NAMES` kept, `stray` counts all.
    #[serde(default)]
    pub stray: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stray_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub has_remote: bool,
    /// Linked worktrees the repo has (the main one not counted).
    #[serde(default)]
    pub worktrees: u32,
    /// This worktree's `worktree list` attributes.
    #[serde(default)]
    pub prunable: bool,
    #[serde(default)]
    pub locked: bool,
    /// Its branch is also checked out elsewhere, or it isn't at a path
    /// named after its branch.
    #[serde(default)]
    pub mismatch: bool,
    /// The repo's config names a command under a key tmux-home can't
    /// neutralise (`exec::Guard`): only HEAD is shown (`⊗`); no status or
    /// diff-based check runs.
    #[serde(default)]
    pub limited: bool,
    /// The last check timed out or failed: the values are the last good ones.
    #[serde(default)]
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub const STRAY_NAMES: usize = 8;

/// What a badge piece means, for styling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Branch,
    Worktree,
    Dirty,
    Conflict,
    Sync,
    InSync,
    Stash,
    Stray,
    State,
    Stale,
}

impl RepoStatus {
    pub fn dirty(&self) -> bool {
        self.staged + self.modified + self.untracked + self.conflicts > 0
    }

    /// The badge as styled pieces, separated by single spaces when joined.
    pub fn badge(&self) -> Vec<(String, Part)> {
        let mut v = Vec::new();
        if !self.branch.is_empty() {
            v.push((self.branch.clone(), Part::Branch));
        }
        if self.linked {
            v.push(("(wt)".into(), Part::Worktree));
        }
        let mut flags = String::new();
        for (n, c) in [
            (self.staged, '+'),
            (self.modified, '!'),
            (self.untracked, '?'),
        ] {
            if n > 0 {
                flags.push(c);
            }
        }
        if !flags.is_empty() {
            v.push((flags, Part::Dirty));
        }
        if self.conflicts > 0 {
            v.push(("✘".into(), Part::Conflict));
        }
        if self.upstream.is_some() && !self.gone {
            if self.ahead == 0 && self.behind == 0 {
                if self.phase != Phase::Head {
                    v.push(("|".into(), Part::InSync));
                }
            } else {
                if self.ahead > 0 {
                    v.push((format!("⇡{}", self.ahead), Part::Sync));
                }
                if self.behind > 0 {
                    v.push((format!("⇣{}", self.behind), Part::Sync));
                }
            }
        }
        if self.stashes > 0 {
            v.push((format!("${}", self.stashes), Part::Stash));
        }
        if self.stray > 0 {
            v.push((format!("⚠{}", self.stray), Part::Stray));
        }
        let mut state = String::new();
        if self.operation.is_some() {
            state.push('↻');
        }
        for (on, c) in [
            (self.prunable, '⊟'),
            (self.locked, '⊞'),
            (self.detached, '⊘'),
            (self.mismatch, '⚑'),
        ] {
            if on {
                state.push(c);
            }
        }
        if self.limited {
            state.push('⊗');
        }
        if !state.is_empty() {
            v.push((state, Part::State));
        }
        if self.stale {
            v.push(("~".into(), Part::Stale));
        }
        v
    }

    /// The badge as plain text.
    pub fn badge_text(&self) -> String {
        self.badge()
            .into_iter()
            .map(|(s, _)| s)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The badge legend (F1 help, README).
pub const LEGEND: &str = "  git badge       branch (wt) +!?✘ ⇡n⇣n | $n ⚠n ↻⊟⊞⊘⚑⊗ ~
                  (wt) linked worktree   + staged   ! modified
                  ? untracked   ✘ conflicts   ⇡n ⇣n ahead/behind upstream
                  | in sync with upstream   $n stashes
                  ⚠n stray branches: unpushed, no upstream (or gone),
                     not merged into the default branch (no remote:
                     any branch not merged into the default branch)
                  ↻ merge/rebase/cherry-pick/revert/bisect in progress
                  ⊟ prunable  ⊞ locked  ⊘ detached  ⚑ branch/path mismatch
                  ⊗ limited: the repo's config names a command that
                     can't be switched off; only HEAD is shown
                  ~ stale: the last check timed out or failed";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_symbols_in_order() {
        let s = RepoStatus {
            branch: "main".into(),
            phase: Phase::Refs,
            upstream: Some("origin/main".into()),
            ahead: 2,
            behind: 1,
            staged: 1,
            modified: 3,
            untracked: 1,
            stashes: 1,
            stray: 2,
            operation: Some(Operation::Rebase),
            ..RepoStatus::default()
        };
        assert_eq!(s.badge_text(), "main +!? ⇡2 ⇣1 $1 ⚠2 ↻");
    }

    #[test]
    fn badge_in_sync_worktree_conflicts_stale() {
        let s = RepoStatus {
            branch: "feat/x".into(),
            linked: true,
            phase: Phase::Status,
            upstream: Some("origin/feat/x".into()),
            conflicts: 2,
            locked: true,
            mismatch: true,
            stale: true,
            ..RepoStatus::default()
        };
        assert_eq!(s.badge_text(), "feat/x (wt) ✘ | ⊞⚑ ~");
        // HEAD only: no sync claim yet; detached shows ⊘
        let h = RepoStatus {
            branch: "abc1234".into(),
            detached: true,
            upstream: Some("origin/x".into()),
            ..RepoStatus::default()
        };
        assert_eq!(h.badge_text(), "abc1234 ⊘");
        // a gone upstream shows no arrows
        let g = RepoStatus {
            branch: "old".into(),
            phase: Phase::Refs,
            upstream: Some("origin/old".into()),
            gone: true,
            ..RepoStatus::default()
        };
        assert_eq!(g.badge_text(), "old");
    }

    #[test]
    fn section_lookup() {
        let mut g = GitSection::default();
        g.paths.insert("/r/src".into(), "/r".into());
        g.repos.insert(
            "/r".into(),
            RepoStatus {
                branch: "main".into(),
                ..RepoStatus::default()
            },
        );
        assert_eq!(g.for_cwd("/r/src").map(|r| r.branch.as_str()), Some("main"));
        assert!(g.for_cwd("/elsewhere").is_none());
    }
}
