//! What can be read about a repo without running git: where it is (cwd →
//! root), its HEAD, an operation in progress, its stash count, and a cheap
//! change stamp. These run on every badge refresh and on the daemon's
//! change watch, so they are plain file reads, never a fork.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where a working tree's git data lives.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepoPaths {
    /// The working tree's top directory (the one holding `.git`).
    pub root: PathBuf,
    /// Its git dir: `<root>/.git`, or for a linked worktree or submodule
    /// the directory its `.git` file names.
    pub git_dir: PathBuf,
    /// The git dir shared by all of a repo's worktrees (refs, config,
    /// stash): `git_dir` itself unless `<git_dir>/commondir` says otherwise.
    pub common_dir: PathBuf,
    /// A linked worktree (`git worktree add`), not the main one.
    pub linked: bool,
}

impl RepoPaths {
    /// The main worktree's directory, for a linked worktree whose common
    /// dir is a `<main>/.git`.
    pub fn main_root(&self) -> Option<PathBuf> {
        if !self.linked {
            return None;
        }
        let c = &self.common_dir;
        (c.file_name()? == ".git").then(|| c.parent().map(Path::to_path_buf))?
    }
}

/// The repository whose working tree contains `cwd`: the nearest ancestor
/// with a `.git` directory or file (a linked worktree, a submodule). Bare
/// repositories have no working tree and give `None`, as does a `.git`
/// file that doesn't name a git dir.
pub fn resolve(cwd: &Path) -> Option<RepoPaths> {
    for dir in cwd.ancestors() {
        let dot = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&dot) else {
            continue;
        };
        let git_dir = if meta.is_dir() {
            dot
        } else {
            let text = std::fs::read_to_string(&dot).ok()?;
            let gd = text.lines().next()?.strip_prefix("gitdir: ")?.trim();
            let gd = Path::new(gd);
            if gd.is_absolute() {
                gd.to_path_buf()
            } else {
                dir.join(gd)
            }
        };
        if !git_dir.join("HEAD").is_file() {
            return None;
        }
        let (common_dir, linked) = match std::fs::read_to_string(git_dir.join("commondir")) {
            Ok(c) => {
                let c = Path::new(c.trim());
                let c = if c.is_absolute() {
                    c.to_path_buf()
                } else {
                    git_dir.join(c)
                };
                (c.canonicalize().unwrap_or(c), true)
            }
            Err(_) => (git_dir.clone(), false),
        };
        return Some(RepoPaths {
            root: dir.to_path_buf(),
            git_dir,
            common_dir,
            linked,
        });
    }
    None
}

/// HEAD as read from `<git_dir>/HEAD`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    /// Detached at this commit (abbreviated to 7).
    Detached(String),
    /// Unreadable, or a ref storage (reftable) that keeps HEAD elsewhere.
    Unknown,
}

pub fn read_head(git_dir: &Path) -> Head {
    let Ok(text) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return Head::Unknown;
    };
    let text = text.trim();
    if let Some(r) = text.strip_prefix("ref: ") {
        return match r.strip_prefix("refs/heads/") {
            // reftable repos point HEAD at this placeholder
            Some(".invalid") | None => Head::Unknown,
            Some(b) => Head::Branch(b.to_string()),
        };
    }
    if text.len() >= 40 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        return Head::Detached(text[..7].to_string());
    }
    Head::Unknown
}

/// A multi-step git operation left open in a working tree (`↻`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl Operation {
    pub fn word(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::Rebase => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
            Operation::Bisect => "bisect",
        }
    }
}

/// The operation in progress in the working tree whose git dir is
/// `git_dir`, from the state files git leaves there (spec §6 item 5; the
/// approach of worktrunk's `operation_in_progress`, reimplemented). A
/// sequence stopped between picks keeps only `sequencer/todo`.
pub fn operation_in_progress(git_dir: &Path) -> Option<Operation> {
    let has = |p: &str| git_dir.join(p).exists();
    if has("MERGE_HEAD") {
        Some(Operation::Merge)
    } else if has("rebase-merge") || has("rebase-apply") {
        Some(Operation::Rebase)
    } else if has("CHERRY_PICK_HEAD") {
        Some(Operation::CherryPick)
    } else if has("REVERT_HEAD") {
        Some(Operation::Revert)
    } else if let Some(op) = std::fs::read_to_string(git_dir.join("sequencer/todo"))
        .ok()
        .and_then(|t| match t.split_whitespace().next() {
            Some("pick" | "p") => Some(Operation::CherryPick),
            Some("revert") => Some(Operation::Revert),
            _ => None,
        })
    {
        Some(op)
    } else if has("BISECT_LOG") {
        Some(Operation::Bisect)
    } else {
        None
    }
}

/// Stash entries: the lines of `refs/stash`'s reflog (each `git stash
/// push` adds one, `drop`/`pop` remove theirs), so no fork.
pub fn stash_count(common_dir: &Path) -> usize {
    std::fs::read_to_string(common_dir.join("logs/refs/stash"))
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// A cheap fingerprint of the files git rewrites when the repo changes
/// through git itself: the index (`add`, `commit`, `checkout`), HEAD and
/// its reflog (any move of HEAD), `packed-refs`, `FETCH_HEAD`, the stash
/// reflog, the operation state files and the worktree registry. Edits to
/// working files don't show here (nothing short of a status does); the
/// adaptive interval covers those.
pub fn stamp(p: &RepoPaths) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    let files = [
        p.git_dir.join("index"),
        p.git_dir.join("HEAD"),
        p.git_dir.join("logs/HEAD"),
        p.git_dir.join("MERGE_HEAD"),
        p.git_dir.join("rebase-merge"),
        p.git_dir.join("rebase-apply"),
        p.git_dir.join("CHERRY_PICK_HEAD"),
        p.git_dir.join("REVERT_HEAD"),
        p.git_dir.join("BISECT_LOG"),
        p.common_dir.join("packed-refs"),
        p.common_dir.join("FETCH_HEAD"),
        p.common_dir.join("logs/refs/stash"),
        p.common_dir.join("refs/heads"),
        p.common_dir.join("worktrees"),
    ];
    for f in &files {
        match std::fs::metadata(f) {
            Ok(m) => (m.modified().ok(), m.len()).hash(&mut h),
            Err(_) => 0u8.hash(&mut h),
        }
    }
    h.finish()
}

/// Modification time of the worktree registry (`<common>/worktrees`),
/// part of the refs memo's key: `git worktree add` of an existing branch
/// changes no ref.
pub fn worktrees_mtime(common_dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(common_dir.join("worktrees"))
        .and_then(|m| m.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "th-repo-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn head_branch_detached_unknown() {
        let d = tmp("head");
        std::fs::write(d.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(read_head(&d), Head::Branch("feat/x".into()));
        std::fs::write(d.join("HEAD"), format!("{}\n", "ab12".repeat(10))).unwrap();
        assert_eq!(read_head(&d), Head::Detached("ab12ab1".into()));
        std::fs::write(d.join("HEAD"), "ref: refs/heads/.invalid\n").unwrap();
        assert_eq!(read_head(&d), Head::Unknown);
        std::fs::remove_file(d.join("HEAD")).unwrap();
        assert_eq!(read_head(&d), Head::Unknown);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn operations_from_state_files() {
        let d = tmp("op");
        assert_eq!(operation_in_progress(&d), None);
        std::fs::write(d.join("BISECT_LOG"), "").unwrap();
        assert_eq!(operation_in_progress(&d), Some(Operation::Bisect));
        std::fs::create_dir_all(d.join("sequencer")).unwrap();
        std::fs::write(d.join("sequencer/todo"), "revert abc x\n").unwrap();
        assert_eq!(operation_in_progress(&d), Some(Operation::Revert));
        std::fs::write(d.join("CHERRY_PICK_HEAD"), "").unwrap();
        assert_eq!(operation_in_progress(&d), Some(Operation::CherryPick));
        std::fs::create_dir_all(d.join("rebase-merge")).unwrap();
        assert_eq!(operation_in_progress(&d), Some(Operation::Rebase));
        std::fs::write(d.join("MERGE_HEAD"), "").unwrap();
        assert_eq!(operation_in_progress(&d), Some(Operation::Merge));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_walks_up_and_reads_git_files() {
        let d = tmp("resolve");
        let main = d.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(main.join("src/deep")).unwrap();
        let p = resolve(&main.join("src/deep")).unwrap();
        assert_eq!(p.root, main);
        assert!(!p.linked);
        assert_eq!(p.common_dir, main.join(".git"));
        // a linked worktree: .git file → registration, commondir → main
        let wt = d.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let reg = main.join(".git/worktrees/wt");
        std::fs::write(reg.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
        std::fs::write(reg.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", reg.display())).unwrap();
        let p = resolve(&wt).unwrap();
        assert!(p.linked);
        assert_eq!(p.git_dir, reg);
        assert_eq!(p.common_dir, main.join(".git").canonicalize().unwrap());
        assert_eq!(
            p.main_root().unwrap(),
            main.canonicalize().unwrap(),
            "the main worktree"
        );
        assert_eq!(read_head(&p.git_dir), Head::Branch("feat".into()));
        // not a repo
        assert_eq!(resolve(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
