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
//! - never shows signatures or colour (`log.showSignature` would run the
//!   signing program; `color.ui=always` would put escapes in the output);
//! - has a timeout, and the child is killed when the call is dropped
//!   (`kill_on_drop`): a hung git must not hold a semaphore permit forever;
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
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Timeout => write!(f, "timed out"),
            GitError::Spawn(e) => write!(f, "failed to run git: {e}"),
            GitError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GitError {}

/// How git is run: timeout, optional shared semaphore, and whether to turn
/// fsmonitor off (the repos scan does, so scanning `~/dev` never starts an
/// fsmonitor daemon per repo).
#[derive(Clone, Debug)]
pub struct Git {
    pub timeout: Duration,
    pub gate: Option<Arc<Semaphore>>,
    pub no_fsmonitor: bool,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            timeout: BADGE_TIMEOUT,
            gate: None,
            no_fsmonitor: false,
        }
    }
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

    /// The command for `git -C dir <args>`, scrubbed and read-only.
    pub fn command(&self, dir: &Path, args: &[&str]) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(git_bin());
        for (k, _) in std::env::vars_os() {
            let Some(k) = k.to_str() else { continue };
            if k.starts_with("GIT_") && !KEEP_ENV.contains(&k) {
                c.env_remove(k);
            }
        }
        c.env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .arg("--no-optional-locks")
            .args(["-c", "color.ui=false", "-c", "log.showSignature=false"]);
        if self.no_fsmonitor {
            c.args(["-c", "core.fsmonitor=false"]);
        }
        c.arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        c
    }

    /// Runs git to completion (or its timeout) under a permit.
    async fn raw(&self, dir: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
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
            .command(dir, args)
            .spawn()
            .map_err(|e| GitError::Spawn(e.to_string()))?;
        // on timeout the future, and with it the child, is dropped: killed
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Err(_) => Err(GitError::Timeout),
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
