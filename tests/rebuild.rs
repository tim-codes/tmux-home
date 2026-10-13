//! An in-place rebuild of the binary under running sidebars (the plugin's
//! `target/release/tmux-home` rebuilt while its sidebar panes run): the
//! daemon is replaced once and then stays put, and each sidebar replaces
//! itself with the new binary, in the same pane.

mod common;

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_tmux-home");

/// A copy of this build at `dst` that is another build: its build ID
/// patched (same length) in place, re-signed on macOS (an edited binary's
/// signature no longer holds, and arm64 macOS kills it at exec).
fn other_build(dst: &Path) {
    let id = tmux_home::BUILD_ID.as_bytes();
    let last = *id.last().unwrap();
    let mut other = id.to_vec();
    *other.last_mut().unwrap() = if last == b'x' { b'y' } else { b'x' };
    let mut bytes = std::fs::read(BIN).unwrap();
    let mut n = 0;
    let mut i = 0;
    while let Some(at) = bytes[i..].windows(id.len()).position(|w| w == id) {
        let at = i + at;
        bytes[at..at + id.len()].copy_from_slice(&other);
        i = at + id.len();
        n += 1;
    }
    assert!(n > 0, "build ID not found in the binary");
    install(dst, &bytes);
    if cfg!(target_os = "macos") {
        let out = Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(dst)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "codesign: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Writes `bytes` to `dst` the way a build does: a new file renamed over
/// the old one (the running image keeps the old inode).
fn install(dst: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = dst.with_extension("tmp");
    std::fs::write(&tmp, bytes).unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, dst).unwrap();
}

/// `(pid, ppid, stat)` of each process whose command line contains `pat`.
fn procs(pat: &str) -> Vec<(u32, u32, String)> {
    let out = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,stat=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(pat))
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((
                f.next()?.parse().ok()?,
                f.next()?.parse().ok()?,
                f.next()?.into(),
            ))
        })
        .collect()
}

/// `(pid, stat)` of each child of `parent`.
fn children(parent: u32) -> Vec<(u32, String)> {
    let out = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,stat="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid: u32 = f.next()?.parse().ok()?;
            let ppid: u32 = f.next()?.parse().ok()?;
            (ppid == parent).then(|| (pid, f.next().unwrap_or("").to_string()))
        })
        .collect()
}

#[test]
fn a_rebuild_under_a_running_sidebar_settles_on_the_new_build() {
    let _env = common::TestEnv::new();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "rebuild-{}-{}",
        std::process::id(),
        common::rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("tmux-home");
    other_build(&bin);
    let s = common::TestServer::start_with(&[
        ("TMUX_HOME_BIN", bin.as_os_str()),
        ("TMUX_HOME_SIDEBAR_REVIVE_AGE", "0".as_ref()),
    ]);
    // a sidebar pane as `split_args` makes it, running the old build
    let pane = s.tmux(&[
        "split-window",
        "-d",
        "-h",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "alpha:",
        "/bin/sh",
        "-c",
        "\"$0\" sidebar --managed; exit",
        bin.to_str().unwrap(),
    ]);
    let pane = pane.trim().to_string();
    let daemon = format!("daemon --socket {}", s.socket.display());
    let sidebar_pid = || {
        procs(&format!("{} sidebar --managed", bin.display()))
            .into_iter()
            .find(|(_, _, st)| !st.starts_with('Z'))
            .map(|(pid, _, _)| pid)
    };
    common::wait_until("the old build's sidebar and daemon run", || {
        sidebar_pid().is_some() && !procs(&daemon).is_empty()
    });
    let sidebar = sidebar_pid().unwrap();

    // the rebuild, then a client of the new build (the status line's)
    install(&bin, &std::fs::read(BIN).unwrap());
    let st = Command::new(&bin)
        .args(["status", "--socket"])
        .arg(&s.socket)
        .env("TMUX_HOME_STATUS_RESPAWN", "1")
        .output()
        .unwrap();
    assert!(st.status.success());

    // settle, then the daemon must stay one process
    std::thread::sleep(Duration::from_secs(3));
    let mut seen = std::collections::BTreeSet::new();
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
        for (pid, _, st) in procs(&daemon) {
            if !st.starts_with('Z') {
                seen.insert(pid);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        seen.len(),
        1,
        "the daemon kept restarting (pids {seen:?}): a restart loop"
    );
    // the new build's daemon serves the new build
    common::wait_until("the new build's daemon serves", || {
        matches!(
            tmux_home::client::ask(
                &s.socket,
                &tmux_home::client::query_req(),
                Duration::from_millis(500)
            ),
            tmux_home::client::Answer::Snapshot { .. }
        )
    });

    // the sidebar replaced itself in place: same process, same pane
    assert_eq!(sidebar_pid(), Some(sidebar), "the sidebar process is gone");
    assert_eq!(
        common::fmt(&s, &pane, "#{pane_dead}"),
        "0",
        "the sidebar pane died"
    );
    let log = tmux_home::paths::Paths::for_socket(&s.socket)
        .unwrap()
        .state_dir
        .join("daemon.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        text.contains("binary replaced"),
        "no re-exec noted; daemon.log: {text:?}"
    );
    // and holds no exited daemons
    let zombies: Vec<_> = children(sidebar)
        .into_iter()
        .filter(|(_, st)| st.starts_with('Z'))
        .collect();
    assert!(zombies.is_empty(), "zombies under the sidebar: {zombies:?}");
    s.tmux(&["kill-server"]);
    let _ = std::fs::remove_dir_all(&dir);
}
