#![allow(dead_code)]
use std::path::PathBuf;
use std::process::Command;

pub struct TestEnv {
    pub runtime: PathBuf,
    pub state: PathBuf,
}

impl TestEnv {
    pub fn new() -> TestEnv {
        let base =
            std::env::temp_dir().join(format!("th-test-{}-{}", std::process::id(), rand_suffix()));
        let runtime = base.join("run");
        let state = base.join("state");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        // SAFETY: tests that use TestEnv run single-threaded per process (see Cargo.toml [[test]] harness note below).
        unsafe {
            std::env::set_var("TMUX_HOME_RUNTIME_DIR", &runtime);
            std::env::set_var("TMUX_HOME_STATE_DIR", &state);
        }
        TestEnv { runtime, state }
    }
}

pub fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{n:08x}")
}

pub struct TestServer {
    pub name: String,
    pub socket: PathBuf,
}

impl TestServer {
    /// A throwaway server with one detached 200x50 session "alpha" running /bin/sh.
    pub fn start() -> TestServer {
        let name = format!("th-test-{}-{}", std::process::id(), rand_suffix());
        let out = Command::new("tmux")
            .args([
                "-L",
                &name,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "alpha",
                "-x",
                "200",
                "-y",
                "50",
                "/bin/sh",
            ])
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let socket = Command::new("tmux")
            .args(["-L", &name, "display", "-p", "#{socket_path}"])
            .env_remove("TMUX")
            .output()
            .unwrap();
        let socket = PathBuf::from(String::from_utf8(socket.stdout).unwrap().trim());
        TestServer { name, socket }
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}
