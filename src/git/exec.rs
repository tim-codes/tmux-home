//! The one way tmux-home runs git (spec §6 item 1, after worktrunk's
//! `shell_exec.rs`; reimplemented).
//!
//! Every call:
//! - is read-only: `--no-optional-locks` and `GIT_OPTIONAL_LOCKS=0`, so not
//!   even `status` refreshes the index; nothing here fetches or writes;
//! - runs in a scrubbed environment: every inherited `GIT_*` variable is
//!   removed (a daemon started from a git hook or alias would otherwise
//!   inherit `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/… and query the
//!   wrong repo), except the three that only choose which *user* config
//!   files git reads (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`,
//!   `GIT_CONFIG_NOSYSTEM`); `LC_ALL=C`, because parsed text such as
//!   `upstream:track` is translated otherwise; `GIT_TERMINAL_PROMPT=0`;
//! - never runs a command the repo's or the user's config names:
//!   `core.fsmonitor=false` (a hook script would run, `true` would start a
//!   resident `fsmonitor--daemon`), `core.hooksPath=/dev/null`, no
//!   transport at all (`protocol.allow=never`, `GIT_NO_LAZY_FETCH=1`, no
//!   credential helper), no signatures (`log.showSignature`), and no
//!   clean/process filters or textconv: attributes are read from the empty
//!   tree (`--attr-source`) instead of the working tree's `.gitattributes`,
//!   global and system attributes are off, and every driver named in
//!   `info/attributes` is overridden to nothing. The price: `status` re-hashes
//!   a stat-dirty file without its clean filter or eol attributes, so an LFS
//!   or `text=auto` file can show as modified until the user's own git
//!   refreshes the index;
//! - never shows colour (`color.ui=always` would put escapes in the output);
//! - runs in its own process group with a timeout: on timeout the whole
//!   group is killed (and the child again on drop, `kill_on_drop`), so a
//!   hung git — or anything it started — can't hold a permit forever;
//! - optionally holds a permit of a shared semaphore while it runs, so the
//!   daemon never has more than a few git processes at once.

use std::{
    path::Path,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;

/// Environment variables the scrub keeps: they pick the user's config
/// files, never the repository.
const KEEP_ENV: &[&str] = &[
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

/// Default per-call timeout for badge work (spec §6).
pub const BADGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default per-call timeout for the repos scan (spec §6).
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// The call ran past its timeout and was killed.
    Timeout,
    /// git could not be started.
    Spawn(String),
    /// git exited non-zero; its stderr.
    Failed(String),
    /// The repo's config names a command under a key `-c` can't override
    /// (`Guard`): nothing but file reads may run there.
    Limited(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Timeout => write!(f, "timed out"),
            GitError::Spawn(e) => write!(f, "failed to run git: {e}"),
            GitError::Failed(e) => write!(f, "{e}"),
            GitError::Limited(k) => write!(
                f,
                "limited: config key {k:?} names a command tmux-home can't switch off"
            ),
        }
    }
}

impl std::error::Error for GitError {}

/// How git is run: timeout, optional shared semaphore, and the repo's
/// guard (its command-running config keys, each overridden). A `Git`
/// without a guard reads one before every call; `guarded` reads it once
/// for a series of calls in one repo.
#[derive(Clone, Debug)]
pub struct Git {
    pub timeout: Duration,
    pub gate: Option<Arc<Semaphore>>,
    pub guard: Option<Arc<Guard>>,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            timeout: BADGE_TIMEOUT,
            gate: None,
            guard: None,
        }
    }
}

/// Every config key, in every scope git reads for the repo, that can make
/// a read-only call run a command — plus the few keys the refs stage needs
/// (item 7), so one `config --get-regexp` serves both.
pub const GUARD_REGEX: &str = concat!(
    r"^((filter|diff|merge)\..+\.(clean|smudge|process|textconv|command|driver|required)",
    r"|credential\..+|core\.(fsmonitor|hookspath|sshcommand|askpass)|diff\.external",
    r"|worktrunk\.default-branch|init\.defaultbranch|remote\..+\.url)$"
);

/// A repo's command-running config keys, read once (`Git::guarded`): the
/// `-c key=value` overrides that switch each one off, by its exact name,
/// and the raw `config --get-regexp` output (the refs stage parses it).
#[derive(Clone, Debug, Default)]
pub struct Guard {
    pub overrides: Vec<String>,
    pub config: String,
}

/// The overrides for `config -z --get-regexp GUARD_REGEX` output, or the key
/// that can't be overridden (an `=` in it, or a record that doesn't parse
/// as one of the expected keys — e.g. a newline in a subsection).
pub fn parse_guard(out: &str) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    for rec in out.split('\0').filter(|r| !r.is_empty()) {
        let key = rec.split_once('\n').map_or(rec, |(k, _)| k);
        if key.contains('=') {
            return Err(key.to_string());
        }
        let (section, rest) = key.split_once('.').ok_or_else(|| key.to_string())?;
        match section {
            // single-valued keys SAFE_CONFIG already overrides, and the refs
            // stage's own keys
            "core" | "worktrunk" | "init" | "remote" => continue,
            "diff" if rest == "external" => continue,
            "credential" if rest == "helper" => continue,
            _ => {}
        }
        let Some((sub, var)) = rest.rsplit_once('.') else {
            if section == "credential" {
                continue; // credential.<var> other than helper: not a command
            }
            return Err(key.to_string());
        };
        let value = match (section, var) {
            ("filter", "required") => "false",
            (
                "filter" | "diff" | "merge",
                "clean" | "smudge" | "process" | "textconv" | "command" | "driver",
            ) => "",
            ("credential", "helper") => "",
            ("credential", _) => continue,
            _ => return Err(key.to_string()),
        };
        if sub.is_empty() {
            return Err(key.to_string());
        }
        v.push(format!("{key}={value}"));
        if section == "filter" {
            v.push(format!("{section}.{sub}.required=false"));
        }
    }
    v.sort();
    v.dedup();
    Ok(v)
}

/// Logs a limited repo once per process.
fn log_limited(dir: &Path, key: &str) {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(dir.to_path_buf())
    {
        eprintln!(
            "tmux-home: {}: config key {key:?} names a command that can't be switched off; \
             git badges there show HEAD only",
            dir.display()
        );
    }
}

/// Config overrides on every call (see the module docs).
const SAFE_CONFIG: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.attributesFile=/dev/null",
    "protocol.allow=never",
    "credential.helper=",
    "color.ui=false",
    "log.showSignature=false",
    "diff.external=",
];

const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// Per-repo facts the safety flags need, from files: the empty tree's ID in
/// the repo's hash, and the filter/diff driver names `info/attributes`
/// uses (in-tree and global attributes are not read at all). The names are
/// a second layer under the guard, which overrides every configured driver.
fn repo_flags(dir: &Path) -> (&'static str, Vec<String>) {
    let Some(p) = super::repo::resolve(dir) else {
        return (EMPTY_TREE_SHA1, Vec::new());
    };
    let sha256 = std::fs::read_to_string(p.common_dir.join("config"))
        .map(|c| {
            c.lines().any(|l| {
                let l = l.replace([' ', '\t'], "").to_ascii_lowercase();
                l == "objectformat=sha256"
            })
        })
        .unwrap_or(false);
    let mut drivers = Vec::new();
    for dir in [&p.common_dir, &p.git_dir] {
        let Ok(text) = std::fs::read_to_string(dir.join("info/attributes")) else {
            continue;
        };
        for tok in text.split_whitespace() {
            for kind in ["filter=", "diff=", "merge="] {
                // any name as-is; one with an `=` can't go through `-c`,
                // and if config defines it the guard fails closed
                if let Some(name) = tok.strip_prefix(kind)
                    && !name.is_empty()
                    && !name.contains('=')
                {
                    drivers.push(format!("{}.{name}", &kind[..kind.len() - 1]));
                }
            }
        }
    }
    drivers.sort();
    drivers.dedup();
    let tree = if sha256 {
        EMPTY_TREE_SHA256
    } else {
        EMPTY_TREE_SHA1
    };
    (tree, drivers)
}

/// The git binary: `$TMUX_HOME_GIT` if set (tests point it at a slow or
/// hanging stand-in), else `git` from `PATH`.
fn git_bin() -> std::ffi::OsString {
    std::env::var_os("TMUX_HOME_GIT")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "git".into())
}

impl Git {
    pub fn with_timeout(timeout: Duration) -> Git {
        Git {
            timeout,
            ..Git::default()
        }
    }

    /// This `Git` with `dir`'s guard read (one fork, unless it has one):
    /// every later call overrides the repo's command-running keys.
    /// `GitError::Limited` when one can't be overridden.
    pub async fn guarded(&self, dir: &Path) -> Result<Git, GitError> {
        if self.guard.is_some() {
            return Ok(self.clone());
        }
        let args = ["config", "-z", "--get-regexp", GUARD_REGEX];
        let out = self.spawn(dir, &args, &[]).await?;
        let config = match out.status.code() {
            Some(0) | Some(1) => String::from_utf8_lossy(&out.stdout).into_owned(),
            _ => return Err(failed(&out)),
        };
        let overrides = parse_guard(&config).map_err(|k| {
            log_limited(dir, &k);
            GitError::Limited(k)
        })?;
        Ok(Git {
            guard: Some(Arc::new(Guard { overrides, config })),
            ..self.clone()
        })
    }

    /// The command for `git -C dir <args>`, scrubbed and read-only, with
    /// `overrides` (`key=value`) passed as `-c`.
    pub fn command(
        &self,
        dir: &Path,
        args: &[&str],
        overrides: &[String],
    ) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(git_bin());
        for (k, _) in std::env::vars_os() {
            let Some(k) = k.to_str() else { continue };
            if k.starts_with("GIT_") && !KEEP_ENV.contains(&k) {
                c.env_remove(k);
            }
        }
        c.env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("LC_ALL", "C")
            .arg("--no-optional-locks");
        let (empty_tree, drivers) = repo_flags(dir);
        c.arg(format!("--attr-source={empty_tree}"));
        for kv in SAFE_CONFIG {
            c.args(["-c", kv]);
        }
        for d in drivers {
            // "filter.lfs" → its commands off; "diff.x" → no textconv/command
            for key in [
                "clean", "smudge", "process", "textconv", "command", "driver",
            ] {
                c.arg("-c").arg(format!("{d}.{key}="));
            }
            if d.starts_with("filter.") {
                c.arg("-c").arg(format!("{d}.required=false"));
            }
        }
        for kv in overrides {
            c.arg("-c").arg(kv);
        }
        c.process_group(0);
        c.arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        c
    }

    /// Runs git under the repo's guard (read first, if this `Git` has none).
    async fn raw(&self, dir: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
        let g = self.guarded(dir).await?;
        let overrides = &g.guard.as_ref().expect("guarded").overrides;
        self.spawn(dir, args, overrides).await
    }

    /// Runs git to completion (or its timeout) under a permit.
    async fn spawn(
        &self,
        dir: &Path,
        args: &[&str],
        overrides: &[String],
    ) -> Result<std::process::Output, GitError> {
        let _permit = match &self.gate {
            Some(g) => Some(
                g.clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| GitError::Spawn(e.to_string()))?,
            ),
            None => None,
        };
        let child = self
            .command(dir, args, overrides)
            .spawn()
            .map_err(|e| GitError::Spawn(e.to_string()))?;
        let pgid = child.id();
        // on timeout the whole process group is killed; the future, and with
        // it the child, is dropped too (kill_on_drop)
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Err(_) => {
                if let Some(pg) = pgid.and_then(|p| i32::try_from(p).ok()) {
                    // SAFETY: kill(2) on our own child's process group
                    unsafe { libc::kill(-pg, libc::SIGKILL) };
                }
                Err(GitError::Timeout)
            }
            Ok(Err(e)) => Err(GitError::Spawn(e.to_string())),
            Ok(Ok(out)) if out.status.code().is_none() => {
                Err(GitError::Failed("git was killed by a signal".into()))
            }
            Ok(Ok(out)) => Ok(out),
        }
    }

    /// Runs git and returns its exit status and stdout when it exits 0 or
    /// 1, for commands whose exit code is the answer (`merge-base
    /// --is-ancestor`, `diff --quiet`, `config --get-regexp`); any other
    /// exit is an error carrying stderr.
    pub async fn output(
        &self,
        dir: &Path,
        args: &[&str],
    ) -> Result<(ExitStatus, String), GitError> {
        let out = self.raw(dir, args).await?;
        match out.status.code() {
            Some(0) | Some(1) => Ok((
                out.status,
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )),
            _ => Err(failed(&out)),
        }
    }

    /// Runs git; its stdout, or an error carrying its stderr.
    pub async fn run(&self, dir: &Path, args: &[&str]) -> Result<String, GitError> {
        let out = self.raw(dir, args).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(failed(&out))
        }
    }
}

fn failed(out: &std::process::Output) -> GitError {
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    GitError::Failed(if err.is_empty() {
        format!("git exited with {}", out.status)
    } else {
        err
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_overrides_each_key_verbatim() {
        let out = "filter.a+b.clean\nx\0filter.with space.process\ny\0diff.Odd.Name.textconv\nz\0\
                   credential.https://h.example.helper\nh\0credential.helper\nh\0\
                   core.fsmonitor\ntrue\0remote.origin.url\nu\0merge.m.driver\nd\0";
        let v = parse_guard(out).unwrap();
        for want in [
            "filter.a+b.clean=",
            "filter.a+b.required=false",
            "filter.with space.process=",
            "filter.with space.required=false",
            "diff.Odd.Name.textconv=",
            "credential.https://h.example.helper=",
            "merge.m.driver=",
        ] {
            assert!(v.iter().any(|k| k == want), "{want} missing: {v:?}");
        }
        assert!(
            !v.iter()
                .any(|k| k.starts_with("core.") || k.starts_with("remote."))
        );
    }

    #[test]
    fn guard_fails_closed_on_unrepresentable_keys() {
        assert_eq!(
            parse_guard("filter.a=b.clean\nx\0"),
            Err("filter.a=b.clean".into())
        );
        // a record that isn't one of the expected keys (e.g. split by a
        // newline in a subsection) fails closed too
        assert!(parse_guard("filter.a\0").is_err());
        assert!(parse_guard("diff.x.weird\nv\0").is_err());
    }

    #[test]
    fn info_attributes_names_are_full_keys() {
        let d = std::env::temp_dir().join(format!("th-exec-{}", std::process::id()));
        std::fs::create_dir_all(d.join(".git/info")).unwrap();
        std::fs::write(d.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            d.join(".git/info/attributes"),
            "*.a filter=lfs diff=a+b\n*.b filter=x=y merge=m\n",
        )
        .unwrap();
        let (tree, drivers) = repo_flags(&d);
        assert_eq!(tree, EMPTY_TREE_SHA1);
        assert_eq!(drivers, ["diff.a+b", "filter.lfs", "merge.m"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
