//! End to end: the Rust popup inside `display-popup` on a throwaway server,
//! driven through an outer throwaway server whose pane hosts the client.
mod common;
use common::{TestEnv, TestServer, rand_suffix};
use std::{
    process::Command,
    time::{Duration, Instant},
};

struct Outer(String);

impl Outer {
    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-L", &self.0, "-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn screen(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "outer"])
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
    }
}

fn wait(what: &str, mut f: impl FnMut() -> bool, screen: impl Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !f() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}",
            screen()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn filter_and_enter_switches_the_invoking_client() {
    let env = TestEnv::new();
    let inner = TestServer::start();
    inner.tmux(&["new-session", "-d", "-s", "beta", "-n", "target", "/bin/sh"]);
    let target = inner.tmux(&["display", "-p", "-t", "beta:target", "#{window_id}"]);
    let target = target.trim();

    let outer = Outer(format!("th-outer-{}-{}", std::process::id(), rand_suffix()));
    let attach = format!(
        "env -u TMUX tmux -S {} attach -t alpha",
        inner.socket.display()
    );
    outer.tmux(&[
        "new-session",
        "-d",
        "-s",
        "outer",
        "-x",
        "200",
        "-y",
        "50",
        &attach,
    ]);
    let mut client = String::new();
    wait(
        "client attached",
        || {
            client = inner
                .tmux(&["list-clients", "-F", "#{client_name}"])
                .trim()
                .to_string();
            !client.is_empty()
        },
        || outer.screen(),
    );

    let bin = env!("CARGO_BIN_EXE_tmux-home");
    let mut popup = Command::new("tmux")
        .arg("-S")
        .arg(&inner.socket)
        .args([
            "display-popup",
            "-c",
            &client,
            "-E",
            "-B",
            "-w",
            "100%",
            "-h",
            "100%",
        ])
        .arg("-e")
        .arg(format!("TMUX_HOME_CLIENT={client}"))
        .arg("-e")
        .arg(format!("TMUX_HOME_STATE_DIR={}", env.state.display()))
        .arg("-e")
        .arg(format!("TMUX_HOME_RUNTIME_DIR={}", env.runtime.display()))
        .arg(format!("{bin} popup"))
        .env_remove("TMUX")
        .spawn()
        .unwrap();

    wait(
        "popup",
        || outer.screen().contains("tmux-home"),
        || outer.screen(),
    );
    outer.tmux(&["send-keys", "-t", "outer", "-l", "targ"]);
    wait(
        "filter",
        || outer.screen().contains("1/2"),
        || outer.screen(),
    );
    outer.tmux(&["send-keys", "-t", "outer", "Enter"]);
    wait(
        "switch",
        || {
            inner
                .tmux(&["display", "-p", "-c", &client, "#{window_id}"])
                .trim()
                == target
        },
        || outer.screen(),
    );
    wait(
        "popup closed",
        || !outer.screen().contains("tmux-home"),
        || outer.screen(),
    );
    let _ = popup.wait();
}
