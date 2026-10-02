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
        .env("TMUX_HOME_DAEMON_DELAY", "0")
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
    s.tmux(&["set-option", "-g", "@home-sidebar-keys", ""]);
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

/// The tmux binary by absolute path, so a test can run the plugin with a
/// PATH that has no cargo on it.
fn tmux_path() -> String {
    let out = Command::new("sh")
        .args(["-c", "command -v tmux"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A copy of tmux-home.tmux in a fresh directory, so it has no
/// `target/release/tmux-home` next to it.
fn plugin_copy() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("th-plugin-")
        .tempdir()
        .unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tmux-home.tmux"),
        d.path().join("tmux-home.tmux"),
    )
    .unwrap();
    d
}

/// Runs the plugin copy in `dir` against `s` with `path` as PATH and no
/// TMUX_HOME_BIN (so a missing binary is built).
fn run_copy(s: &TestServer, dir: &std::path::Path, path: &str) {
    let out = Command::new(dir.join("tmux-home.tmux"))
        .env(
            "TMUX_HOME_TMUX",
            format!("{} -S {}", tmux_path(), s.socket.display()),
        )
        .env("PATH", path)
        .env("TMUX_HOME_DAEMON_DELAY", "0")
        .env_remove("TMUX_HOME_BIN")
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tmux-home.tmux: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A missing binary starts one background build; reloading the config while
/// it runs starts no second one. Its output goes to target/build.log, which
/// the key's message names; once it is done, a reload may build again.
#[test]
fn missing_binary_builds_once_across_reloads() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let plugin = plugin_copy();
    let stub = tempfile::tempdir().unwrap();
    let count = stub.path().join("count");
    let cargo = stub.path().join("cargo");
    std::fs::write(
        &cargo,
        format!(
            "#!/bin/sh\necho run >>'{}'\necho stub build \"$@\"\nsleep 1\n",
            count.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", stub.path().display());
    let runs = || {
        std::fs::read_to_string(&count)
            .unwrap_or_default()
            .lines()
            .count()
    };
    run_copy(&s, plugin.path(), &path);
    wait_until("the build started", || runs() == 1);
    run_copy(&s, plugin.path(), &path);
    run_copy(&s, plugin.path(), &path);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(runs(), 1, "reloads during a build start no other");
    let b = binding(&s, ".").expect("prefix . bound");
    let log = plugin.path().join("target/build.log");
    assert!(b.contains(&log.display().to_string()), "{b}");
    let lock = plugin.path().join("target/.building");
    wait_until("the build finished", || !lock.exists());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("stub build build --release"), "{text:?}");
    run_copy(&s, plugin.path(), &path);
    wait_until("a later reload builds again", || runs() == 2);
    wait_until("that build finished", || !lock.exists());
}

/// A lock left by a dead build whose pid now belongs to another live
/// process (here: this test) doesn't block builds: its start time differs.
#[test]
fn a_lock_with_a_reused_pid_is_taken_over() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let plugin = plugin_copy();
    let lock = plugin.path().join("target/.building");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("pid"), std::process::id().to_string()).unwrap();
    std::fs::write(lock.join("start"), "Thu Jan  1 00:00:00 1970").unwrap();
    let stub = tempfile::tempdir().unwrap();
    let count = stub.path().join("count");
    let cargo = stub.path().join("cargo");
    std::fs::write(
        &cargo,
        format!("#!/bin/sh\necho run >>'{}'\n", count.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    run_copy(
        &s,
        plugin.path(),
        &format!("{}:/usr/bin:/bin", stub.path().display()),
    );
    wait_until("the build started", || count.exists());
    wait_until("the build finished", || !lock.exists());
}

/// No cargo: no build is started and the key says how to build it.
#[test]
fn missing_cargo_says_so_and_builds_nothing() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let plugin = plugin_copy();
    run_copy(&s, plugin.path(), "/usr/bin:/bin");
    // (the message is shell-quoted in the binding)
    let b = binding(&s, ".").expect("prefix . bound").replace('\\', "");
    assert!(b.contains("cargo is not on PATH"), "{b}");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!plugin.path().join("target").exists(), "no build started");
}

/// A `#` in the binary's path survives run-shell's format expansion: the
/// daemon starts and the key opens the popup.
#[test]
fn hash_in_the_binary_path() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let d = tempfile::Builder::new()
        .prefix("th#plugin#")
        .tempdir()
        .unwrap();
    let bin = d.path().join("tmux#home");
    std::os::unix::fs::symlink(BIN, &bin).unwrap();
    run_plugin(&s, bin.to_str().unwrap());
    let sock = tmux_home::paths::Paths::for_socket(&s.socket).unwrap().sock;
    wait_until("daemon socket", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });
    let o = Outer::attach(&s, "alpha", 120, 30);
    o.open_popup();
    o.keys(&["Escape"]);
    o.wait_gone("F1 help");
}

#[test]
fn sidebar_keys_default_to_e_and_shift_e() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    run_plugin(&s, BIN);
    let e = binding(&s, "e").expect("prefix e bound");
    assert!(e.contains(&format!("{BIN} sidebar-toggle")), "{e}");
    assert!(e.contains("--window #{q:session_id}:#{q:window_id}"), "{e}");
    assert!(!e.contains("--session"), "{e}");
    let big = binding(&s, "E").expect("prefix E bound");
    assert!(big.contains("--session"), "{big}");
}

#[test]
fn sidebar_keys_are_configurable() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "@home-sidebar-keys", "- S"]);
    run_plugin(&s, BIN);
    assert_eq!(binding(&s, "e"), None);
    assert_eq!(binding(&s, "E"), None);
    assert!(binding(&s, "S").expect("prefix S").contains("--session"));
}

/// The daemon starts TMUX_HOME_DAEMON_DELAY seconds after the plugin
/// loads (tmux-continuum's restore check runs meanwhile).
#[test]
fn the_daemon_start_is_delayed() {
    let _env = TestEnv::new();
    let s = TestServer::start();
    let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tmux-home.tmux"))
        .env("TMUX_HOME_TMUX", format!("tmux -S {}", s.socket.display()))
        .env("TMUX_HOME_BIN", BIN)
        .env("TMUX_HOME_DAEMON_DELAY", "2")
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(out.status.success());
    let sock = tmux_home::paths::Paths::for_socket(&s.socket).unwrap().sock;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    assert!(!sock.exists(), "not yet");
    wait_until("daemon socket", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });
}

/// At login PATH may lack the directory tmux lives in: run the way TPM
/// runs it (run-shell, inside a server started with a bare PATH), the
/// plugin still finds tmux, binds its keys and starts the daemon.
#[test]
fn a_bare_path_still_binds_and_starts_the_daemon() {
    let env = TestEnv::new();
    let name = format!("th-test-{}-{}", std::process::id(), rand_suffix());
    let bare = "/usr/bin:/bin:/usr/sbin:/sbin";
    let out = Command::new(tmux_path())
        .env_clear()
        .env("PATH", bare)
        .env("HOME", std::env::var("HOME").unwrap())
        .env("TMUX_HOME_RUNTIME_DIR", &env.runtime)
        .env("TMUX_HOME_STATE_DIR", &env.state)
        .args([
            "-L",
            &name,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "alpha",
            "/bin/sh",
        ])
        .current_dir(neutral_cwd())
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
    let s = TestServer {
        name,
        socket: std::path::PathBuf::from(String::from_utf8(socket.stdout).unwrap().trim()),
    };
    assert_eq!(
        s.tmux(&["show-environment", "-g", "PATH"]).trim(),
        format!("PATH={bare}")
    );
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tmux-home.tmux");
    let r = s.tmux(&[
        "run-shell",
        &format!("TMUX_HOME_BIN='{BIN}' TMUX_HOME_DAEMON_DELAY=0 '{script}' 2>&1; echo rc=$?"),
    ]);
    assert!(r.contains("rc=0"), "{r}");
    let b = binding(&s, ".").expect("prefix . bound");
    assert!(
        b.contains("/tmux display-popup"),
        "tmux by absolute path: {b}"
    );
    assert!(binding(&s, "e").is_some(), "prefix e bound");
    let sock = tmux_home::paths::Paths::for_socket(&s.socket).unwrap().sock;
    wait_until("daemon socket", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });
    // the daemon's own tmux calls work with that PATH, and with no locale
    // set (no LANG/LC_*): it serves, and its records parse
    let (snap, live) =
        tmux_home::client::snapshot(&s.socket, std::time::Duration::from_millis(500)).unwrap();
    assert!(live);
    assert_eq!(snap.windows.len(), 1, "{snap:?}");
    assert_eq!(snap.sessions[0].name, "alpha");
}
