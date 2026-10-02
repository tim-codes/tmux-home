pub mod control;
pub mod snapshot;
pub mod source;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where tmux is looked for when the running server can't say.
pub const KNOWN_TMUX: &[&str] = &[
    "/opt/homebrew/bin/tmux",
    "/usr/local/bin/tmux",
    "/usr/bin/tmux",
];

/// The tmux binary, by absolute path, resolved once: never from `PATH`
/// alone, which at login (launchd, a minimal `run-shell`) may lack
/// Homebrew's directory. An absolute path also keeps the commands tmux-home
/// runs from showing as `tmux …` in `ps`, which tmux-continuum counts as
/// another tmux server (and then skips its auto-restore). In order: the
/// running server's own binary (its pid is `$TMUX`'s second field), the
/// usual install locations, a `PATH` search, and the bare name.
pub fn tmux_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        resolve_tmux(
            std::env::var("TMUX").ok().as_deref(),
            std::env::var_os("PATH").as_deref(),
            server_exe,
            is_executable,
        )
    })
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The executable of process `pid`: `/proc/<pid>/exe` (Linux), else
/// `ps -o comm=` (macOS gives the full path).
fn server_exe(pid: u32) -> Option<PathBuf> {
    if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        return Some(p);
    }
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// `tmux_bin`'s choice, its inputs given (tests).
pub fn resolve_tmux(
    tmux_env: Option<&str>,
    path: Option<&std::ffi::OsStr>,
    exe_of: impl Fn(u32) -> Option<PathBuf>,
    executable: impl Fn(&Path) -> bool,
) -> PathBuf {
    let server = tmux_env
        .and_then(|t| t.split(',').nth(1))
        .and_then(|p| p.trim().parse::<u32>().ok())
        .and_then(&exe_of)
        .filter(|p| p.is_absolute() && p.file_name().is_some_and(|n| n == "tmux") && executable(p));
    if let Some(p) = server {
        return p;
    }
    if let Some(p) = KNOWN_TMUX.iter().map(Path::new).find(|p| executable(p)) {
        return p.to_path_buf();
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join("tmux"))
        .find(|p| p.is_absolute() && executable(p))
        .unwrap_or_else(|| PathBuf::from("tmux"))
}

/// The one way tmux-home runs tmux: `tmux -S <socket>` with `$TMUX`
/// removed (so a command run from inside tmux targets `socket`, not the
/// caller's server). Commands run synchronously (`run`: the popup, the CLI,
/// `ops`); async callers (the daemon) use `run_async`, which runs the same
/// thing on tokio's blocking pool. `command` is for the long-lived
/// control-mode client.
#[derive(Clone, Debug)]
pub struct Tmux {
    pub socket: PathBuf,
}

impl Tmux {
    pub fn new(socket: PathBuf) -> Tmux {
        Tmux { socket }
    }

    /// `tmux -u -S <socket>` (absolute `tmux_bin()`, `$TMUX` removed), to add
    /// arguments to.
    pub fn base(&self) -> std::process::Command {
        let mut c = std::process::Command::new(tmux_bin());
        // -u: UTF-8 output whatever the locale; without it (no LANG/LC_*,
        // as under launchd) tmux prints the \x1f field separators as `_`
        c.arg("-u").arg("-S").arg(&self.socket).env_remove("TMUX");
        c
    }

    /// An async `tmux -S <socket>` command (killed on drop).
    pub fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::from(self.base());
        c.kill_on_drop(true);
        c
    }

    /// Runs `tmux <args>`; its stdout, or an error carrying its stderr.
    pub fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = self.base().args(args).output()?;
        if !out.status.success() {
            anyhow::bail!(
                "tmux {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `run` off the async executor.
    pub async fn run_async(&self, args: &[&str]) -> anyhow::Result<String> {
        let t = self.clone();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            t.run(&args)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_is_the_servers_binary_then_known_places_then_path() {
        let all = |_: &Path| true;
        let srv = |pid| (pid == 42).then(|| PathBuf::from("/opt/x/bin/tmux"));
        assert_eq!(
            resolve_tmux(Some("/tmp/s,42,0"), None, srv, all),
            Path::new("/opt/x/bin/tmux")
        );
        // not a tmux, or relative: the known places
        let odd = |_| Some(PathBuf::from("bash"));
        assert_eq!(
            resolve_tmux(Some("/tmp/s,42,0"), None, odd, all),
            Path::new(KNOWN_TMUX[0])
        );
        let only = |want: &'static str| move |p: &Path| p == Path::new(want);
        assert_eq!(
            resolve_tmux(None, None, srv, only("/usr/bin/tmux")),
            Path::new("/usr/bin/tmux")
        );
        let path = std::ffi::OsString::from("/nix/bin:/odd/bin");
        assert_eq!(
            resolve_tmux(None, Some(&path), srv, only("/odd/bin/tmux")),
            Path::new("/odd/bin/tmux")
        );
        assert_eq!(
            resolve_tmux(None, Some(&path), srv, |_| false),
            Path::new("tmux")
        );
    }
}
