//! tmux-home.tmux against a throwaway server: key bindings, daemon start,
//! and the message shown while the binary isn't built.
mod common;
use common::*;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_tmux-home");

fn run_plugin(s: &TestServer, bin: &str) {
    let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tmux-home.tmux"))
        .env("TMUX_HOME_TMUX", format!("tmux -S {}", s.socket.display()))
        .env("TMUX_HOME_BIN", bin)
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tmux-home.tmux: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The prefix-table binding for `key`, if tmux-home made one.
fn binding(s: &TestServer, key: &str) -> Option<String> {
    s.tmux(&["list-keys", "-T", "prefix"])
        .lines()
        .find(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.get(3) == Some(&key) && l.contains("tmux-home")
        })
        .map(str::to_string)
}

#[test]
fn empty_home_keys_binds_nothing() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-keys", ""]);
    run_plugin(&s, BIN);
    assert_eq!(binding(&s, "."), None);
    assert!(
        !s.tmux(&["list-keys", "-T", "prefix"]).contains("tmux-home"),
        "no key bound"
    );
}

#[test]
fn default_binds_prefix_dot_to_the_rust_popup() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let w_before = s.tmux(&["list-keys", "-T", "prefix", "w"]);
    run_plugin(&s, BIN);
    let b = binding(&s, ".").expect("prefix . bound");
    assert!(b.contains("run-shell -b"), "{b}");
    assert!(b.contains("display-popup -c #{q:client_name}"), "{b}");
    assert!(b.contains("TMUX_HOME_CLIENT=#{q:client_name}"), "{b}");
    assert!(b.contains(&format!("{BIN} popup")), "{b}");
    assert_eq!(
        s.tmux(&["list-keys", "-T", "prefix", "w"]),
        w_before,
        "w is left alone"
    );
    assert_eq!(binding(&s, "f"), None, "f is left alone");
}

#[test]
fn custom_keys_bind_each() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-keys", "w f"]);
    run_plugin(&s, BIN);
    assert!(binding(&s, "w").is_some());
    assert!(binding(&s, "f").is_some());
    assert_eq!(binding(&s, "."), None);
}

#[test]
fn starts_the_daemon_when_the_binary_exists() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    run_plugin(&s, BIN);
    let sock = tmux_home::paths::Paths::for_socket(&s.socket).unwrap().sock;
    wait_until("daemon socket", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });
}

/// No bash fallback: without a binary the key shows a one-line message,
/// and (TMUX_HOME_BIN being set) no build is started.
#[test]
fn missing_binary_shows_a_message() {
    let env = TestEnv::new();
    let s = TestServer::start();
    run_plugin(&s, "/nonexistent/tmux-home");
    let b = binding(&s, ".").expect("prefix . bound");
    assert!(b.contains("display-message"), "{b}");
    assert!(b.contains("not") && b.contains("built"), "{b}");
    assert!(!b.contains("bin/tmux-home"), "no bash fallback: {b}");
    let sock = tmux_home::paths::Paths::for_socket(&s.socket).unwrap().sock;
    assert!(!sock.exists(), "no daemon without a binary");
    // pressing the key shows the message on the client
    let o = Outer::attach(&s, "alpha", 120, 30);
    o.keys(&["C-b", "."]);
    o.wait_for("tmux-home: not built yet");
    drop(o);
    drop(s);
    drop(env);
}
