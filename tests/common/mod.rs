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
        // Automatic window rename lags pane_current_command by a further,
        // separately-timed hook (confirmed by probing: pane_current_command
        // flips to the real shell within ~100ms, but window_name can take
        // several hundred ms more to follow) — it is a second source of
        // startup churn independent of the one wait_settled rides out below.
        // Tests don't rely on automatic-rename, so turn it off globally to
        // remove that source of flakiness outright.
        let out = Command::new("tmux")
            .args(["-L", &name, "set-option", "-g", "automatic-rename", "off"])
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "set-option automatic-rename off: {}",
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

    /// A throwaway server that stays up with zero sessions (`exit-empty off`,
    /// then its only session killed) — the state a server is in while
    /// tmux.conf, and so TPM's run of tmux-home.tmux, executes.
    pub fn start_empty() -> TestServer {
        let s = TestServer::start();
        s.tmux(&["set-option", "-g", "exit-empty", "off"]);
        s.tmux(&["kill-session", "-t", "alpha"]);
        s
    }

    /// Polls `list-panes -a` until two consecutive reads of
    /// `pane_current_command`, `pane_current_path`, `pane_title` and
    /// `window_name` all agree, to ride out the brief churn a freshly
    /// spawned pane's shell (and, for window_name, tmux's automatic-rename
    /// hook following it) produces right after start. Panics if it never
    /// stabilizes within 3s.
    pub fn wait_settled(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut last: Option<String> = None;
        loop {
            let cur = self.tmux(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id} #{pane_current_command} #{pane_current_path} #{pane_title} #{window_name}",
            ]);
            if last.as_deref() == Some(cur.as_str()) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("panes never settled: last={last:?} cur={cur}");
            }
            last = Some(cur);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
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
