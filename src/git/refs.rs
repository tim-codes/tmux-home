//! Ref-derived fields, behind a refs snapshot memo (spec §6 items 4, 7, 8;
//! the ideas of worktrunk's `ref_snapshot.rs`/`sha_cache.rs`,
//! `config.rs::default_branch` and `git/mod.rs::check_integration`,
//! reimplemented).
//!
//! Each refresh runs three cheap forks: the probe (`for-each-ref
//! refs/heads refs/remotes refs/stash`), the few config keys that matter
//! (`config --get-regexp`), and `worktree list` (worktrees change without
//! any ref moving: a lock, a switch inside a linked worktree). When probe
//! and config output are what the memo last saw, nothing ref-derived can
//! have changed and no other git runs. Otherwise the fields are recomputed, and per-branch results
//! (unpushed count, integration) are reused for every branch whose SHA —
//! and, for unpushed, the set of remote SHAs — is unchanged. The memo
//! lives in the daemon's memory, never in `.git`.

use super::exec::{Git, GitError};
use super::model::Worktree;
use super::repo::RepoPaths;
use super::status::{parse_track, parse_worktrees, unpushed};
use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

/// One line of the probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefRow {
    /// Full name: `refs/heads/main`, `refs/remotes/origin/HEAD`, `refs/stash`.
    pub name: String,
    pub sha: String,
    /// For a symbolic ref (`refs/remotes/origin/HEAD`), its target.
    pub symref: String,
    /// The branch's upstream, full name (`refs/remotes/origin/main`).
    pub upstream: String,
    /// `upstream:track`: `[ahead 1, behind 2]`, `[gone]` or empty.
    pub track: String,
}

const PROBE_FMT: &str =
    "--format=%(refname)%00%(objectname)%00%(symref)%00%(upstream)%00%(upstream:track)";

pub fn parse_refs(out: &str) -> Vec<RefRow> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\0').collect();
            (f.len() == 5).then(|| RefRow {
                name: f[0].into(),
                sha: f[1].into(),
                symref: f[2].into(),
                upstream: f[3].into(),
                track: f[4].into(),
            })
        })
        .collect()
}

/// A local branch as the badge sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub upstream: Option<String>,
    pub gone: bool,
    pub unpushed: usize,
    pub integrated: bool,
}

impl BranchInfo {
    /// A stray branch (`⚠`): commits that are on no remote, no upstream (or
    /// a gone one) to push them to, and not merged into the default branch.
    /// In a repo with no remote every commit is on none: there it is any
    /// non-default branch not merged into the default branch.
    pub fn stray(&self) -> bool {
        self.unpushed > 0 && (self.upstream.is_none() || self.gone) && !self.integrated
    }
}

/// What the memo computes from refs.
#[derive(Clone, Debug, Default)]
pub struct RefFields {
    pub has_remote: bool,
    pub default_branch: Option<String>,
    pub branches: Vec<BranchInfo>,
    pub worktrees: Vec<Worktree>,
}

impl RefFields {
    pub fn stray(&self) -> Vec<String> {
        self.branches
            .iter()
            .filter(|b| b.stray())
            .map(|b| b.name.clone())
            .collect()
    }

    pub fn branch(&self, name: &str) -> Option<&BranchInfo> {
        self.branches.iter().find(|b| b.name == name)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RefsMemo {
    key: Option<u64>,
    /// (branch SHA, remotes key) → (unpushed, capped).
    unpushed: HashMap<(String, u64), usize>,
    /// (branch SHA, target SHA) → integrated.
    integrated: HashMap<(String, String), bool>,
    pub fields: RefFields,
    /// Forks the last `refresh` ran (tests and the perf note).
    pub last_forks: usize,
}

/// The default branch, read-only (spec §6 item 7): `worktrunk.default-branch`
/// config, then `<primary remote>/HEAD` as recorded locally, then local
/// inference (the only branch, `init.defaultBranch` if it exists, then
/// main/master/develop/trunk). No `ls-remote`, nothing written back.
pub fn default_branch(
    config: &HashMap<String, String>,
    remotes: &[String],
    rows: &[RefRow],
) -> Option<String> {
    if let Some(b) = config.get("worktrunk.default-branch") {
        let b = b.trim();
        if !b.is_empty() {
            return Some(b.to_string());
        }
    }
    let primary = remotes.iter().find(|r| *r == "origin").or(remotes.first());
    if let Some(r) = primary {
        let head = format!("refs/remotes/{r}/HEAD");
        let prefix = format!("refs/remotes/{r}/");
        if let Some(b) = rows
            .iter()
            .find(|x| x.name == head)
            .and_then(|x| x.symref.strip_prefix(&prefix))
        {
            return Some(b.to_string());
        }
    }
    let local: Vec<&str> = rows
        .iter()
        .filter_map(|x| x.name.strip_prefix("refs/heads/"))
        .collect();
    if local.len() == 1 {
        return Some(local[0].to_string());
    }
    if let Some(b) = config.get("init.defaultbranch")
        && local.contains(&b.trim())
    {
        return Some(b.trim().to_string());
    }
    ["main", "master", "develop", "trunk"]
        .into_iter()
        .find(|n| local.contains(n))
        .map(str::to_string)
}

/// `config -z --get-regexp` output: `key\nvalue\0` records, keys lowercased
/// by git. Remote names are taken from `remote.<name>.url`.
pub fn parse_config(out: &str) -> (HashMap<String, String>, Vec<String>) {
    let mut cfg = HashMap::new();
    let mut remotes = Vec::new();
    for rec in out.split('\0').filter(|r| !r.is_empty()) {
        let (k, v) = rec.split_once('\n').unwrap_or((rec, ""));
        if let Some(r) = k
            .strip_prefix("remote.")
            .and_then(|r| r.strip_suffix(".url"))
        {
            if !remotes.iter().any(|x| x == r) {
                remotes.push(r.to_string());
            }
        } else {
            cfg.insert(k.to_string(), v.to_string());
        }
    }
    (cfg, remotes)
}

/// Is `b` merged into `t`? The cheap tiers of worktrunk's integration
/// check, cheapest first: same commit, ancestor, no changes added since
/// the merge base, equal trees (a squash merge with nothing after it).
async fn integrated(git: &Git, dir: &std::path::Path, b: &str, t: &str) -> Result<bool, GitError> {
    if b == t {
        return Ok(true);
    }
    let (st, _) = git
        .output(dir, &["merge-base", "--is-ancestor", b, t])
        .await?;
    if st.success() {
        return Ok(true);
    }
    let (st, mb) = git.output(dir, &["merge-base", t, b]).await?;
    if !st.success() {
        return Ok(false); // no common history
    }
    let (st, _) = git
        .output(
            dir,
            &[
                "diff-tree",
                "--no-ext-diff",
                "--no-textconv",
                "--quiet",
                "-r",
                mb.trim(),
                b,
                "--",
            ],
        )
        .await?;
    if st.success() {
        return Ok(true);
    }
    let trees = git
        .run(
            dir,
            &[
                "rev-parse",
                &format!("{b}^{{tree}}"),
                &format!("{t}^{{tree}}"),
            ],
        )
        .await?;
    let t: Vec<&str> = trees.lines().collect();
    Ok(t.len() == 2 && t[0] == t[1])
}

impl RefsMemo {
    /// Probes the refs and, when they changed since the last call,
    /// recomputes `fields`. Returns whether anything was recomputed.
    pub async fn refresh(&mut self, git: &Git, p: &RepoPaths) -> Result<bool, GitError> {
        let dir = &p.root;
        let mut forks = 1;
        let out = git
            .run(
                dir,
                &[
                    "for-each-ref",
                    PROBE_FMT,
                    "refs/heads",
                    "refs/remotes",
                    "refs/stash",
                ],
            )
            .await?;
        // config and worktrees change without any ref moving (`remote add`,
        // `worktree lock`, a switch inside a linked worktree): both are read
        // on every refresh, config into the key, worktrees outside the memo
        // the guard's config read (one fork, unless the caller's `Git` is
        // already guarded) carries the keys item 7 needs; its output is
        // part of the key, so a new filter or remote counts as a change
        if git.guard.is_none() {
            forks += 1;
        }
        let git = &git.guarded(dir).await?;
        let cfg_out = git.guard.as_ref().expect("guarded").config.clone();
        forks += 1;
        self.fields.worktrees = match git.run(dir, &["worktree", "list", "--porcelain"]).await {
            Ok(o) => parse_worktrees(&o),
            Err(GitError::Timeout) => return Err(GitError::Timeout),
            Err(_) => Vec::new(),
        };
        let mut h = DefaultHasher::new();
        (&out, &cfg_out).hash(&mut h);
        let key = h.finish();
        if self.key == Some(key) {
            self.last_forks = forks;
            return Ok(false);
        }
        let rows = parse_refs(&out);
        let (cfg, remotes) = parse_config(&cfg_out);
        let remote_rows: Vec<&RefRow> = rows
            .iter()
            .filter(|r| r.name.starts_with("refs/remotes/") && r.symref.is_empty())
            .collect();
        let has_remote = !remotes.is_empty() || !remote_rows.is_empty();
        let remote_shas: BTreeSet<&str> = remote_rows.iter().map(|r| r.sha.as_str()).collect();
        let remotes_key = {
            let mut h = DefaultHasher::new();
            for r in &remote_rows {
                (&r.name, &r.sha).hash(&mut h);
            }
            h.finish()
        };
        let default = default_branch(&cfg, &remotes, &rows);

        // integration targets: the default branch, and its upstream when
        // that differs (local main may lag or lead origin/main)
        let sha_of = |name: &str| rows.iter().find(|r| r.name == name).map(|r| r.sha.clone());
        let mut targets: Vec<String> = Vec::new();
        if let Some(d) = &default {
            let local = format!("refs/heads/{d}");
            if let Some(s) = sha_of(&local) {
                targets.push(s);
            }
            let up = rows
                .iter()
                .find(|r| r.name == local)
                .map(|r| r.upstream.clone())
                .filter(|u| !u.is_empty())
                .or_else(|| {
                    remotes
                        .iter()
                        .find(|r| *r == "origin")
                        .or(remotes.first())
                        .map(|r| format!("refs/remotes/{r}/{d}"))
                });
            if let Some(s) = up.as_deref().and_then(sha_of)
                && !targets.contains(&s)
            {
                targets.push(s);
            }
        }

        let mut unpushed_memo = HashMap::new();
        let mut integrated_memo = HashMap::new();
        let mut branches = Vec::new();
        for r in rows.iter().filter(|r| r.name.starts_with("refs/heads/")) {
            let name = r.name["refs/heads/".len()..].to_string();
            let (_, _, gone) = parse_track(&r.track);
            let upstream = (!r.upstream.is_empty()).then(|| {
                r.upstream
                    .strip_prefix("refs/remotes/")
                    .unwrap_or(&r.upstream)
                    .to_string()
            });
            // commits on no remote: none when the tip is itself a remote
            // ref's commit; else from the memo, else one fork. With no
            // remote at all every commit is on none (stray's count), and
            // only non-default branches are candidates
            let mut n = 0;
            let candidate = if has_remote {
                !remote_shas.contains(r.sha.as_str())
            } else {
                default.as_deref() != Some(name.as_str())
            };
            if candidate {
                let k = (r.sha.clone(), remotes_key);
                n = match self.unpushed.get(&k) {
                    Some(n) => *n,
                    None => {
                        forks += 1;
                        match unpushed(git, dir, &r.name, has_remote).await {
                            Ok((n, _, _)) => n,
                            Err(GitError::Timeout) => return Err(GitError::Timeout),
                            Err(_) => 0,
                        }
                    }
                };
                unpushed_memo.insert(k, n);
            }
            let mut b = BranchInfo {
                name,
                upstream,
                gone,
                unpushed: n,
                integrated: false,
            };
            if b.unpushed > 0 && (b.upstream.is_none() || b.gone) {
                for t in &targets {
                    let k = (r.sha.clone(), t.clone());
                    let yes = match self.integrated.get(&k) {
                        Some(y) => *y,
                        None => {
                            forks += 1;
                            match integrated(git, dir, &r.sha, t).await {
                                Ok(y) => y,
                                Err(GitError::Timeout) => return Err(GitError::Timeout),
                                Err(_) => false,
                            }
                        }
                    };
                    integrated_memo.insert(k, yes);
                    if yes {
                        b.integrated = true;
                        break;
                    }
                }
            }
            branches.push(b);
        }
        // keep only what the current refs can use
        self.unpushed = unpushed_memo;
        self.integrated = integrated_memo;
        self.fields = RefFields {
            has_remote,
            default_branch: default,
            branches,
            worktrees: std::mem::take(&mut self.fields.worktrees),
        };
        self.key = Some(key);
        self.last_forks = forks;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, sha: &str, symref: &str) -> RefRow {
        RefRow {
            name: name.into(),
            sha: sha.into(),
            symref: symref.into(),
            upstream: String::new(),
            track: String::new(),
        }
    }

    #[test]
    fn parse_refs_rows() {
        let out = "refs/heads/main\0aaa\0\0refs/remotes/origin/main\0[ahead 1]\nrefs/remotes/origin/HEAD\0aaa\0refs/remotes/origin/main\0\0\nbad line\n";
        let r = parse_refs(out);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].upstream, "refs/remotes/origin/main");
        assert_eq!(r[0].track, "[ahead 1]");
        assert_eq!(r[1].symref, "refs/remotes/origin/main");
    }

    #[test]
    fn parse_config_records() {
        let (cfg, remotes) = parse_config(
            "init.defaultbranch\ntrunk\0remote.origin.url\ngit@x:y\0remote.up.url\nz\0",
        );
        assert_eq!(
            cfg.get("init.defaultbranch").map(String::as_str),
            Some("trunk")
        );
        assert_eq!(remotes, ["origin", "up"]);
    }

    #[test]
    fn default_branch_order() {
        let none = HashMap::new();
        let rows = vec![
            row("refs/heads/dev", "a", ""),
            row("refs/heads/master", "b", ""),
            row("refs/heads/trunk", "c", ""),
        ];
        // local inference: main/master/develop/trunk
        assert_eq!(default_branch(&none, &[], &rows).as_deref(), Some("master"));
        // init.defaultBranch, when that branch exists
        let mut cfg = HashMap::new();
        cfg.insert("init.defaultbranch".into(), "trunk".into());
        assert_eq!(default_branch(&cfg, &[], &rows).as_deref(), Some("trunk"));
        cfg.insert("init.defaultbranch".into(), "nope".into());
        assert_eq!(default_branch(&cfg, &[], &rows).as_deref(), Some("master"));
        // <remote>/HEAD beats inference
        let mut with_head = rows.clone();
        with_head.push(row(
            "refs/remotes/origin/HEAD",
            "a",
            "refs/remotes/origin/dev",
        ));
        let origin = ["origin".to_string()];
        assert_eq!(
            default_branch(&none, &origin, &with_head).as_deref(),
            Some("dev")
        );
        // worktrunk's config beats everything
        cfg.insert("worktrunk.default-branch".into(), "release".into());
        assert_eq!(
            default_branch(&cfg, &origin, &with_head).as_deref(),
            Some("release")
        );
        // the only branch
        let one = vec![row("refs/heads/solo", "a", "")];
        assert_eq!(default_branch(&none, &[], &one).as_deref(), Some("solo"));
        assert_eq!(
            default_branch(&none, &[], &rows[..1]).as_deref(),
            Some("dev")
        );
    }

    #[test]
    fn stray_rules() {
        let b = |unpushed, up: Option<&str>, gone, integrated| BranchInfo {
            name: "x".into(),
            upstream: up.map(String::from),
            gone,
            unpushed,
            integrated,
        };
        assert!(b(1, None, false, false).stray());
        assert!(b(1, Some("origin/x"), true, false).stray());
        assert!(
            !b(1, Some("origin/x"), false, false).stray(),
            "⇡, not stray"
        );
        assert!(!b(0, None, false, false).stray(), "nothing unpushed");
        assert!(!b(3, None, false, true).stray(), "integrated");
    }
}
