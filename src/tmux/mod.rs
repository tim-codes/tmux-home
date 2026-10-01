pub mod control;
pub mod snapshot;
pub mod source;

use std::path::PathBuf;

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

    fn base(&self) -> std::process::Command {
        let mut c = std::process::Command::new("tmux");
        c.arg("-S").arg(&self.socket).env_remove("TMUX");
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
