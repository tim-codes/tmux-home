//! Per-repo git status, vendored from stray (https://github.com/tim-codes/stray,
//! `src/git.rs` at v0.3.0; MIT, Copyright (c) 2026 Tim O'Connell — see
//! NOTICE).
//!
//! Changes from stray:
//! - async, through `exec::Git` (scrubbed env, `LC_ALL=C`, read-only,
//!   timeouts, shared semaphore; spec §6 item 1);
//! - `status --untracked-files=normal` always, so `showUntrackedFiles=no`
//!   can't make a repo look clean (item 2);
//! - unpushed commits in one fork per branch, `log -n1001 … --not
//!   --remotes`, counted and capped (item 3; was `rev-list --count` + `log`);
//! - `worktree list --porcelain` parsing keeps `locked`, `prunable`,
//!   `detached` and `bare` (item 6), as a pure, tested function.

use std::path::{Path, PathBuf};

use super::exec::{Git, GitError};
use super::model::{Branch, FileEntry, Repo, RepoKind, StatusCounts, Worktree};
use super::scan::Found;

pub const UNPUSHED_CAP: usize = 1000;
const ENTRY_CAP: usize = 1000;
pub const COMMIT_LIST_CAP: usize = 20;

/// Diff for one working-tree file, for the preview pane. `code` is the
/// two-char status code, `path` the status entry path ("old -> new" for
/// renames).
pub async fn file_diff(git: &Git, root: &Path, code: &str, path: &str) -> String {
    // Renames render as "orig -> new"; diff wants the new path.
    let path = path.split(" -> ").last().unwrap_or(path);

    if code == "??" {
        // Untracked: diff against /dev/null so the whole file shows as added.
        // --no-index exits 1 when the files differ, so read stdout directly.
        return match git
            .output(root, &["diff", "--no-index", "--", "/dev/null", path])
            .await
        {
            Ok((_, o)) => o,
            Err(e) => format!("failed to run git: {e}"),
        };
    }

    // HEAD..worktree covers staged and unstaged changes in one view; fall back
    // for repos with no commits yet.
    match git.run(root, &["diff", "HEAD", "--", path]).await {
        Ok(o) if !o.is_empty() => o,
        Ok(_) => git
            .run(root, &["diff", "--", path])
            .await
            .unwrap_or_default(),
        Err(_) => match git.run(root, &["diff", "--cached", "--", path]).await {
            Ok(o) => o,
            Err(_) => git
                .run(root, &["diff", "--", path])
                .await
                .unwrap_or_else(|e| e.to_string()),
        },
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadInfo {
    pub head: String,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
}

pub fn parse_status(text: &str) -> (StatusCounts, HeadInfo) {
    let mut counts = StatusCounts::default();
    let mut info = HeadInfo {
        head: "?".to_string(),
        ..HeadInfo::default()
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            if rest == "(detached)" {
                info.detached = true;
                info.head = "detached".to_string();
            } else {
                info.head = rest.to_string();
            }
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            info.upstream = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let xy: Vec<char> = line.chars().skip(2).take(2).collect();
            if xy.first().is_some_and(|c| *c != '.') {
                counts.staged += 1;
            }
            if xy.get(1).is_some_and(|c| *c != '.') {
                counts.unstaged += 1;
            }
            let code: String = xy
                .iter()
                .map(|c| if *c == '.' { ' ' } else { *c })
                .collect();
            let path = if line.starts_with("1 ") {
                line.splitn(9, ' ').nth(8).unwrap_or("").to_string()
            } else {
                // "2" (rename): "<path>\t<origPath>" — show old -> new
                let p = line.splitn(10, ' ').nth(9).unwrap_or("");
                match p.split_once('\t') {
                    Some((new, orig)) => format!("{orig} -> {new}"),
                    None => p.to_string(),
                }
            };
            push_entry(&mut counts, code, path);
        } else if line.starts_with("u ") {
            counts.conflicts += 1;
            let code: String = line.chars().skip(2).take(2).collect();
            let path = line.splitn(11, ' ').nth(10).unwrap_or("").to_string();
            push_entry(&mut counts, code, path);
        } else if let Some(path) = line.strip_prefix("? ") {
            counts.untracked += 1;
            push_entry(&mut counts, "??".to_string(), path.to_string());
        }
    }
    (counts, info)
}

fn push_entry(counts: &mut StatusCounts, code: String, path: String) {
    if counts.entries.len() < ENTRY_CAP {
        counts.entries.push(FileEntry { code, path });
    }
}

/// `git status` of the working tree at `dir`: counts and the branch
/// header (upstream ahead/behind come from `branch.ab`, in the same fork).
pub async fn status_counts(git: &Git, dir: &Path) -> Result<(StatusCounts, HeadInfo), GitError> {
    let out = git
        .run(
            dir,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=normal",
            ],
        )
        .await?;
    Ok(parse_status(&out))
}

pub fn parse_track(track: &str) -> (usize, usize, bool) {
    // e.g. "[ahead 2, behind 1]" or "[gone]" or ""
    let t = track.trim_start_matches('[').trim_end_matches(']');
    if t == "gone" {
        return (0, 0, true);
    }
    let (mut ahead, mut behind) = (0, 0);
    for part in t.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind, false)
}

/// Commits on `refname` that no remote ref reaches (every commit when the
/// repo has no remote): one `log` fork, counted up to `UNPUSHED_CAP` (then
/// `capped`), the newest `COMMIT_LIST_CAP` kept as "abc1234 subject".
pub async fn unpushed(
    git: &Git,
    dir: &Path,
    refname: &str,
    has_remote: bool,
) -> Result<(usize, bool, Vec<String>), GitError> {
    let n = format!("-n{}", UNPUSHED_CAP + 1);
    let mut args = vec![
        "log",
        "--no-show-signature",
        "--format=%h %s",
        n.as_str(),
        refname,
    ];
    if has_remote {
        args.push("--not");
        args.push("--remotes");
    }
    args.push("--");
    let out = git.run(dir, &args).await?;
    let lines: Vec<&str> = out.lines().collect();
    let capped = lines.len() > UNPUSHED_CAP;
    let count = lines.len().min(UNPUSHED_CAP);
    let commits = lines
        .iter()
        .take(COMMIT_LIST_CAP)
        .map(|s| s.to_string())
        .collect();
    Ok((count, capped, commits))
}

async fn branches(
    git: &Git,
    dir: &Path,
    head: &str,
    has_remote: bool,
    errors: &mut Vec<String>,
) -> Vec<Branch> {
    let out = match git
        .run(
            dir,
            &[
                "for-each-ref",
                "refs/heads",
                "--format=%(refname:short)\t%(upstream:short)\t%(upstream:track)\t%(committerdate:unix)\t%(committerdate:relative)\t%(contents:subject)",
            ],
        )
        .await
    {
        Ok(o) => o,
        Err(e) => {
            errors.push(format!("for-each-ref: {e}"));
            return Vec::new();
        }
    };
    let mut result = Vec::new();
    for line in out.lines() {
        let mut parts = line.splitn(6, '\t');
        let name = parts.next().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let upstream = parts.next().unwrap_or("").to_string();
        let track = parts.next().unwrap_or("");
        let committer_unix: i64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let age = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        let (ahead, behind, gone) = parse_track(track);

        let refname = format!("refs/heads/{name}");
        let (unpushed, capped, commits) = match unpushed(git, dir, &refname, has_remote).await {
            Ok(u) => u,
            Err(e) => {
                errors.push(format!("log {name}: {e}"));
                (0, false, Vec::new())
            }
        };

        result.push(Branch {
            is_head: name == head,
            name,
            upstream: if upstream.is_empty() {
                None
            } else {
                Some(upstream)
            },
            gone,
            ahead,
            behind,
            unpushed,
            unpushed_capped: capped,
            commits,
            committer_unix,
            age,
            subject,
            integrated: false,
        });
    }
    // HEAD first, then branches with outstanding work, newest activity first.
    result.sort_by_key(|b| {
        (
            std::cmp::Reverse(b.is_head),
            std::cmp::Reverse(b.outstanding()),
            std::cmp::Reverse(b.committer_unix),
        )
    });
    result
}

/// Parses `git worktree list --porcelain`. The first record is the main
/// worktree. `prunable` (git's word for a worktree whose directory is gone)
/// sets `missing`; `locked`, `detached` and `bare` are kept (item 6).
pub fn parse_worktrees(out: &str) -> Vec<Worktree> {
    let mut result: Vec<Worktree> = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(w) = current.take() {
                result.push(w);
            }
            current = Some(Worktree {
                path: PathBuf::from(p),
                branch: None,
                is_main: result.is_empty(),
                missing: false,
                locked: false,
                detached: false,
                bare: false,
                status: StatusCounts::default(),
            });
            continue;
        }
        let Some(w) = current.as_mut() else { continue };
        let (key, _value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "branch" => {
                let b = &line["branch ".len()..];
                w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            }
            "prunable" => w.missing = true,
            "locked" => w.locked = true,
            "detached" => w.detached = true,
            "bare" => w.bare = true,
            _ => {}
        }
    }
    if let Some(w) = current.take() {
        result.push(w);
    }
    result
}

async fn worktrees(git: &Git, dir: &Path, errors: &mut Vec<String>) -> Vec<Worktree> {
    let out = match git.run(dir, &["worktree", "list", "--porcelain"]).await {
        Ok(o) => o,
        Err(e) => {
            errors.push(format!("worktree list: {e}"));
            return Vec::new();
        }
    };
    let mut result = parse_worktrees(&out);
    for w in result.iter_mut() {
        if w.is_main {
            continue;
        }
        if !w.path.exists() {
            w.missing = true;
            continue;
        }
        match status_counts(git, &w.path).await {
            Ok((counts, _)) => w.status = counts,
            Err(e) => errors.push(format!("worktree {}: {e}", w.path.display())),
        }
    }
    result
}

/// Full status of one found repo, for the repos view (stray's `collect`).
pub async fn collect(git: &Git, found: &Found, scan_root: &Path) -> Repo {
    let (root, kind) = match found {
        Found::Repo(p) => (p.clone(), RepoKind::Normal),
        Found::Bare(p) => (p.clone(), RepoKind::Bare),
        Found::Submodule(p) => (p.clone(), RepoKind::Submodule),
        Found::Worktree { path, main } => (
            path.clone(),
            RepoKind::OrphanWorktree { main: main.clone() },
        ),
    };
    let name = root
        .strip_prefix(scan_root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| root.to_string_lossy().into_owned());
    let name = if name.is_empty() {
        ".".to_string()
    } else {
        name
    };

    let mut repo = Repo {
        root: root.clone(),
        name,
        kind: kind.clone(),
        head: "?".to_string(),
        detached: false,
        upstream: None,
        ahead: 0,
        behind: 0,
        has_remote: true,
        status: StatusCounts::default(),
        stashes: 0,
        branches: Vec::new(),
        worktrees: Vec::new(),
        errors: Vec::new(),
        ignored: false,
    };

    let bare = kind == RepoKind::Bare;

    if !bare {
        match status_counts(git, &root).await {
            Ok((counts, info)) => {
                repo.status = counts;
                repo.head = info.head;
                repo.detached = info.detached;
                repo.upstream = info.upstream;
                repo.ahead = info.ahead;
                repo.behind = info.behind;
            }
            Err(e) => repo.errors.push(format!("status: {e}")),
        }
    } else {
        repo.head = git
            .run(&root, &["symbolic-ref", "--short", "HEAD"])
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "bare".to_string());
    }

    // Orphan worktrees share refs/stashes with their main repo (which is
    // outside the scan root), so only working-tree state is relevant here.
    if matches!(kind, RepoKind::OrphanWorktree { .. }) {
        return repo;
    }

    repo.has_remote = git
        .run(&root, &["remote"])
        .await
        .map(|o| !o.trim().is_empty())
        .unwrap_or(false);

    let mut errors = std::mem::take(&mut repo.errors);
    repo.branches = branches(git, &root, &repo.head, repo.has_remote, &mut errors).await;
    repo.worktrees = worktrees(git, &root, &mut errors).await;
    repo.errors = errors;

    if !bare {
        match git.run(&root, &["stash", "list", "--format=%gd"]).await {
            Ok(o) => repo.stashes = o.lines().count(),
            Err(e) => repo.errors.push(format!("stash: {e}")),
        }
    }

    repo
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_status (from stray) ----------------------------------------

    #[test]
    fn parse_status_branch_header() {
        let text = "\
# branch.oid abc123
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
";
        let (counts, info) = parse_status(text);
        assert_eq!(info.head, "main");
        assert!(!info.detached);
        assert_eq!(info.upstream.as_deref(), Some("origin/main"));
        assert_eq!(info.ahead, 2);
        assert_eq!(info.behind, 1);
        assert_eq!(counts.total(), 0);
    }

    #[test]
    fn parse_status_detached_head() {
        let text = "# branch.head (detached)\n";
        let (_, info) = parse_status(text);
        assert!(info.detached);
        assert_eq!(info.head, "detached");
    }

    #[test]
    fn parse_status_changed_entries_staged_and_unstaged() {
        // "1 " ordinary changed entries: XY sub mH mI mW hH hW path
        let text = "\
1 M. N... 100644 100644 100644 aaa bbb staged.txt
1 .M N... 100644 100644 100644 aaa bbb unstaged.txt
1 MM N... 100644 100644 100644 aaa bbb both.txt
";
        let (counts, _) = parse_status(text);
        assert_eq!(counts.staged, 2); // staged.txt, both.txt
        assert_eq!(counts.unstaged, 2); // unstaged.txt, both.txt
        assert_eq!(counts.entries.len(), 3);
        assert_eq!(counts.entries[0].code, "M ");
        assert_eq!(counts.entries[0].path, "staged.txt");
        assert_eq!(counts.entries[1].code, " M");
        assert_eq!(counts.entries[1].path, "unstaged.txt");
        assert_eq!(counts.entries[2].code, "MM");
        assert_eq!(counts.entries[2].path, "both.txt");
    }

    #[test]
    fn parse_status_rename_entry_formats_old_arrow_new() {
        // "2 " rename/copy entries: XY sub mH mI mW hH hW X<score> path\torigPath
        let text = "2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\told.txt\n";
        let (counts, _) = parse_status(text);
        assert_eq!(counts.staged, 1);
        assert_eq!(counts.entries.len(), 1);
        assert_eq!(counts.entries[0].path, "old.txt -> new.txt");
    }

    #[test]
    fn parse_status_conflict_entry() {
        // "u " unmerged entries: XY sub m1 m2 m3 mW h1 h2 h3 path
        let text = "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt\n";
        let (counts, _) = parse_status(text);
        assert_eq!(counts.conflicts, 1);
        assert_eq!(counts.entries.len(), 1);
        assert_eq!(counts.entries[0].code, "UU");
        assert_eq!(counts.entries[0].path, "conflict.txt");
    }

    #[test]
    fn parse_status_untracked_entry() {
        let text = "? new_file.txt\n";
        let (counts, _) = parse_status(text);
        assert_eq!(counts.untracked, 1);
        assert_eq!(counts.entries.len(), 1);
        assert_eq!(counts.entries[0].code, "??");
        assert_eq!(counts.entries[0].path, "new_file.txt");
    }

    #[test]
    fn parse_status_entry_cap_limits_stored_entries_not_counts() {
        let mut text = String::new();
        for i in 0..(ENTRY_CAP + 5) {
            text.push_str(&format!("? file{i}.txt\n"));
        }
        let (counts, _) = parse_status(&text);
        // The count reflects every entry seen...
        assert_eq!(counts.untracked, ENTRY_CAP + 5);
        // ...but stored entries are capped.
        assert_eq!(counts.entries.len(), ENTRY_CAP);
    }

    // ---- parse_track (from stray) -----------------------------------------

    #[test]
    fn parse_track_ahead_and_behind() {
        assert_eq!(parse_track("[ahead 2, behind 1]"), (2, 1, false));
    }

    #[test]
    fn parse_track_ahead_only() {
        assert_eq!(parse_track("[ahead 3]"), (3, 0, false));
    }

    #[test]
    fn parse_track_behind_only() {
        assert_eq!(parse_track("[behind 5]"), (0, 5, false));
    }

    #[test]
    fn parse_track_gone() {
        assert_eq!(parse_track("[gone]"), (0, 0, true));
    }

    #[test]
    fn parse_track_empty_is_up_to_date() {
        assert_eq!(parse_track(""), (0, 0, false));
    }

    // ---- parse_worktrees (item 6) -----------------------------------------

    #[test]
    fn parse_worktrees_keeps_every_attribute() {
        let text = "\
worktree /r
HEAD aaa
branch refs/heads/main

worktree /r-feat
HEAD bbb
branch refs/heads/feat/x
locked reason here

worktree /r-gone
HEAD ccc
detached
prunable gitdir file points to non-existent location

";
        let w = parse_worktrees(text);
        assert_eq!(w.len(), 3);
        assert!(w[0].is_main && !w[1].is_main && !w[2].is_main);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert_eq!(w[1].branch.as_deref(), Some("feat/x"));
        assert!(w[1].locked && !w[1].missing);
        assert!(w[2].detached && w[2].missing && w[2].branch.is_none());
    }

    #[test]
    fn parse_worktrees_bare_main() {
        let w = parse_worktrees("worktree /r.git\nbare\n\nworktree /wt\nHEAD a\nbranch refs/heads/x\n");
        assert!(w[0].bare && w[0].is_main);
        assert!(!w[1].bare);
    }
}
