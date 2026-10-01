//! Finding repos under a root, vendored from stray
//! (https://github.com/tim-codes/stray, `src/scan.rs` at v0.3.0; MIT,
//! Copyright (c) 2026 Tim O'Connell — see NOTICE): the `.git`
//! file/dir/bare classification and the walk. Changes: rayon is replaced
//! by tokio tasks capped at `SCAN_PERMITS` (spec §6: the scan may hold at
//! most 2 of the daemon's 4 git permits), every call goes through
//! `exec::Git` (with `core.fsmonitor=false` for the scan), and `classify`
//! is public for the badge path's cwd → repo resolution.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::exec::Git;
use super::model::Repo;

/// Repos the scan collects at once.
pub const SCAN_PERMITS: usize = 2;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "venv",
    ".venv",
    "__pycache__",
    "dist",
    "build",
    "Pods",
    "DerivedData",
];

#[derive(Debug, Clone)]
pub enum Found {
    Repo(PathBuf),
    Bare(PathBuf),
    Submodule(PathBuf),
    /// A worktree checkout; `main` is the repo it belongs to.
    Worktree {
        path: PathBuf,
        main: PathBuf,
    },
}

impl Found {
    pub fn path(&self) -> &Path {
        match self {
            Found::Repo(p) | Found::Bare(p) | Found::Submodule(p) => p,
            Found::Worktree { path, .. } => path,
        }
    }
}

pub fn find_repos(root: &Path, max_depth: usize) -> Vec<Found> {
    let mut out = Vec::new();
    walk(root, 0, max_depth, &mut out);
    out
}

fn walk(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<Found>) {
    let git_path = dir.join(".git");
    if git_path.is_dir() {
        out.push(Found::Repo(dir.to_path_buf()));
        // keep descending: nested repos are rare but real
    } else if git_path.is_file() {
        if let Some(found) = classify_git_file(dir, &git_path) {
            out.push(found);
        }
        return; // a worktree/submodule checkout mirrors its main repo
    } else if looks_bare(dir) {
        out.push(Found::Bare(dir.to_path_buf()));
        return;
    }

    if depth >= max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue; // read_dir doesn't follow symlinks, so this also skips them
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        walk(&entry.path(), depth + 1, max_depth, out);
    }
}

pub fn classify_git_file(dir: &Path, git_file: &Path) -> Option<Found> {
    let content = fs::read_to_string(git_file).ok()?;
    let gitdir = content.lines().next()?.strip_prefix("gitdir: ")?.trim();
    let gitdir = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        dir.join(gitdir)
    };
    let gitdir = gitdir.canonicalize().unwrap_or(gitdir);
    let s = gitdir.to_string_lossy();
    if let Some(idx) = s.find("/worktrees/") {
        let mut main = s[..idx].to_string();
        // main is ".../repo/.git" for normal repos, ".../repo.git" for bare
        if main.ends_with("/.git") {
            main.truncate(main.len() - 5);
        }
        return Some(Found::Worktree {
            path: dir.to_path_buf(),
            main: PathBuf::from(main),
        });
    }
    if s.contains("/modules/") {
        return Some(Found::Submodule(dir.to_path_buf()));
    }
    Some(Found::Repo(dir.to_path_buf()))
}

pub fn looks_bare(dir: &Path) -> bool {
    if !(dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()) {
        return false;
    }
    fs::read_to_string(dir.join("config"))
        .map(|c| c.lines().any(|l| l.trim().replace(' ', "") == "bare=true"))
        .unwrap_or(false)
}

/// Scan for repos and collect full status for each, `SCAN_PERMITS` at a
/// time. `git` should have the scan timeout (fsmonitor is always off).
pub async fn inventory(git: &Git, root: &Path, max_depth: usize) -> Vec<Repo> {
    let found = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || find_repos(&root, max_depth))
            .await
            .unwrap_or_default()
    };

    // Worktrees whose main repo is also in the scan are listed under it
    // (via `git worktree list`), so drop the standalone entry.
    let repo_roots: HashSet<PathBuf> = found
        .iter()
        .filter(|f| !matches!(f, Found::Worktree { .. }))
        .map(|f| {
            f.path()
                .canonicalize()
                .unwrap_or_else(|_| f.path().to_path_buf())
        })
        .collect();
    let found: Vec<Found> = found
        .into_iter()
        .filter(|f| match f {
            Found::Worktree { main, .. } => {
                let main = main.canonicalize().unwrap_or_else(|_| main.clone());
                !repo_roots.contains(&main)
            }
            _ => true,
        })
        .collect();

    let cap = std::sync::Arc::new(tokio::sync::Semaphore::new(SCAN_PERMITS));
    let mut tasks = tokio::task::JoinSet::new();
    for f in found {
        let (git, cap, root) = (git.clone(), cap.clone(), root.to_path_buf());
        tasks.spawn(async move {
            let _permit = cap.acquire_owned().await;
            super::status::collect(&git, &f, &root).await
        });
    }
    let mut repos: Vec<Repo> = tasks.join_all().await;
    let patterns = super::ignore::load();
    for r in repos.iter_mut() {
        r.ignored = super::ignore::matches(&patterns, &r.name, &r.root);
    }
    repos.sort_by(|a, b| {
        b.needs_attention()
            .cmp(&a.needs_attention())
            .then_with(|| a.name.cmp(&b.name))
    });
    repos
}
