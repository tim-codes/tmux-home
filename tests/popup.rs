//! End to end: the Rust popup inside `display-popup` on a throwaway server,
//! driven through an outer throwaway server whose pane hosts the client.
mod common;
use common::{TestEnv, TestServer, rand_suffix};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    process::{Child, Command},
    time::{Duration, Instant},
};
use tmux_home::{paths::Paths, tmux::source::SourceKind};

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

/// An outer server whose pane hosts a client attached to `inner`'s alpha.
fn attach(inner: &TestServer) -> (Outer, String) {
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
    (outer, client)
}

fn open_popup(env: &TestEnv, inner: &TestServer, client: &str) -> Child {
    let bin = env!("CARGO_BIN_EXE_tmux-home");
    Command::new("tmux")
        .arg("-S")
        .arg(&inner.socket)
        .args([
            "display-popup",
            "-c",
            client,
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
        .unwrap()
}

/// The popup row under the cursor (`▌`), if one is drawn.
fn cursor_row(screen: &str) -> Option<String> {
    screen
        .lines()
        .find(|l| l.starts_with('▌'))
        .map(str::to_string)
}

#[test]
fn filter_and_enter_switches_the_invoking_client() {
    let env = TestEnv::new();
    let inner = TestServer::start();
    inner.tmux(&["new-session", "-d", "-s", "beta", "-n", "target", "/bin/sh"]);
    let target = inner.tmux(&["display", "-p", "-t", "beta:target", "#{window_id}"]);
    let target = target.trim();
    let (outer, client) = attach(&inner);
    let mut popup = open_popup(&env, &inner, &client);

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

/// The cursor opens on the client's current window even when the daemon's
/// cached snapshot predates a `select-window` made just before the popup.
#[test]
fn cursor_opens_on_the_window_selected_just_before() {
    let env = TestEnv::new();
    let inner = TestServer::start();
    inner.tmux(&["rename-window", "-t", "alpha:", "one"]);
    inner.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "two", "/bin/sh"]);
    let (outer, client) = attach(&inner);
    // a polling daemon (snapshots up to 500 ms old), already serving
    let socket = inner.socket.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tmux_home::daemon::run(socket, SourceKind::Poll))
    });
    let paths = Paths::for_socket(&inner.socket).unwrap();
    // ride the poll: once the daemon has seen this rename, its next read is
    // ~500 ms away, so its cache misses the select-window below
    inner.tmux(&["rename-window", "-t", "=alpha:two", "tick"]);
    wait(
        "daemon saw the rename",
        || {
            let Ok(mut s) = UnixStream::connect(&paths.sock) else {
                return false;
            };
            let q = format!("{{\"op\":\"query\",\"v\":\"{}\"}}\n", tmux_home::VERSION);
            let mut line = String::new();
            s.write_all(q.as_bytes()).is_ok()
                && BufReader::new(s).read_line(&mut line).is_ok()
                && line.contains("\"tick\"")
        },
        || outer.screen(),
    );
    inner.tmux(&["select-window", "-t", "=alpha:tick"]);
    let mut popup = open_popup(&env, &inner, &client);
    wait(
        "popup",
        || cursor_row(&outer.screen()).is_some(),
        || outer.screen(),
    );
    let row = cursor_row(&outer.screen()).unwrap();
    assert!(
        row.contains("tick"),
        "cursor on {row:?}\n{}",
        outer.screen()
    );
    outer.tmux(&["send-keys", "-t", "outer", "Escape"]);
    wait(
        "popup closed",
        || !outer.screen().contains("tmux-home"),
        || outer.screen(),
    );
    let _ = popup.wait();
}
