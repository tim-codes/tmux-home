pub mod control;
pub mod snapshot;
pub mod source;

use std::path::PathBuf;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct Tmux {
    pub socket: PathBuf,
}

impl Tmux {
    pub fn new(socket: PathBuf) -> Tmux {
        Tmux { socket }
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new("tmux");
        c.arg("-S")
            .arg(&self.socket)
            .env_remove("TMUX")
            .kill_on_drop(true);
        c
    }

    pub async fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = self.command().args(args).output().await?;
        if !out.status.success() {
            anyhow::bail!(
                "tmux {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
