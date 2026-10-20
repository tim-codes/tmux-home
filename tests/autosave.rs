//! The daemon's autosave (in place of tmux-continuum's), on throwaway
//! servers: with `TMUX_HOME_SAVE_UNIT_SECS=1` the interval is in seconds.
//! A fake save script logs each run's start and end, and which server its
//! bare `tmux` reaches; the real resurrect save (when installed) writes
//! into a temp `@resurrect-dir`.

mod common;
use common::{TestEnv, TestServer, wait_until};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_tmux-home");

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn daemon(s: &TestServer) -> Daemon {
    Daemon(
        Command::new(BIN)
            .args(["daemon", "--socket"])
            .arg(&s.socket)
            .env_remove("TMUX")
            .env("TMUX_HOME_SAVE_TICK_MS", "100")
            .env("TMUX_HOME_SAVE_UNIT_SECS", "1")
            .spawn()
            .unwrap(),
    )
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp(tag: &str) -> Dir {
    let d = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "th-{tag}-{}-{}",
        std::process::id(),
        common::rand_suffix()
    ));
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

/// A save script that takes `secs`, logging `start <socket>` / `end` to
/// `log`.
fn fake_save(dir: &Path, log: &Path, secs: f64) -> PathBuf {
    let p = dir.join("save.sh");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\n[ \"$1\" = quiet ] || exit 3\n\
             echo \"start $(tmux display -p '#{{socket_path}}')\" >> '{log}'\n\
             sleep {secs}\necho end >> '{log}'\n",
            log = log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn autosave_log(s: &TestServer) -> String {
    std::fs::read_to_string(
        tmux_home::paths::Paths::for_socket(&s.socket)
            .unwrap()
            .state_dir
            .join("autosave.log"),
    )
    .unwrap_or_default()
}

fn runs(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn saves_on_the_interval_only_once_restored_and_one_at_a_time() {
    let _env = TestEnv::new();
    let d = temp("autosave");
    let log = d.0.join("runs.log");
    let script = fake_save(&d.0, &log, 1.5);
    let s = TestServer::start();
    s.tmux(&["set", "-gu", "@home_restore_done"]);
    s.tmux(&[
        "set",
        "-g",
        "@resurrect-save-script-path",
        script.to_str().unwrap(),
    ]);
    s.tmux(&["set", "-g", "@home-save-interval", "1"]);
    let _d = daemon(&s);

    // not restored: never saved, the skip logged once
    std::thread::sleep(Duration::from_millis(2500));
    assert!(runs(&log).is_empty(), "{:?}", runs(&log));
    let alog = autosave_log(&s);
    assert_eq!(
        alog.matches("skip: server not restored yet").count(),
        1,
        "{alog}"
    );

    // restored: saves every interval, each after the last has ended
    s.tmux(&["set", "-g", "@home_restore_done", "1"]);
    wait_until("three saves", || {
        runs(&log).iter().filter(|l| *l == "end").count() >= 3
    });
    let r = runs(&log);
    let socket = format!("start {}", s.socket.display());
    for pair in r.chunks(2) {
        assert_eq!(pair[0], socket, "a save of this server: {r:?}");
        if pair.len() == 2 {
            assert_eq!(pair[1], "end", "never two at once: {r:?}");
        }
    }
    assert!(autosave_log(&s).contains("autosave: saved in"));

    // 0: off
    s.tmux(&["set", "-g", "@home-save-interval", "0"]);
    std::thread::sleep(Duration::from_millis(1700)); // a save in flight ends
    let n = runs(&log).len();
    std::thread::sleep(Duration::from_millis(2000));
    assert_eq!(runs(&log).len(), n, "off");
}

#[test]
fn the_first_save_waits_an_interval() {
    let _env = TestEnv::new();
    let d = temp("autosave-first");
    let log = d.0.join("runs.log");
    let script = fake_save(&d.0, &log, 0.0);
    let s = TestServer::start();
    s.tmux(&[
        "set",
        "-g",
        "@resurrect-save-script-path",
        script.to_str().unwrap(),
    ]);
    s.tmux(&["set", "-g", "@home-save-interval", "3"]);
    let _d = daemon(&s);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(runs(&log).is_empty(), "not at once");
    assert!(
        !s.tmux(&["show", "-gqv", "@home_save_last"])
            .trim()
            .is_empty(),
        "clock started"
    );
    wait_until("the first save", || !runs(&log).is_empty());
}

/// Through tmux-resurrect's own save (skipped when it isn't installed):
/// the layout lands in the temp `@resurrect-dir`, and `last` points at it.
#[test]
fn runs_resurrects_save_into_its_dir() {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let script = home.join(".tmux/plugins/tmux-resurrect/scripts/save.sh");
    if !script.exists() {
        eprintln!("skipped: tmux-resurrect isn't installed");
        return;
    }
    let _env = TestEnv::new();
    let d = temp("autosave-resurrect");
    let rdir = d.0.join("resurrect");
    let s = TestServer::start();
    s.tmux(&["set", "-g", "@resurrect-dir", rdir.to_str().unwrap()]);
    s.tmux(&[
        "set",
        "-g",
        "@resurrect-save-script-path",
        script.to_str().unwrap(),
    ]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "marked"]);
    s.tmux(&["set", "-g", "@home-save-interval", "1"]);
    let _d = daemon(&s);
    wait_until("a resurrect save", || rdir.join("last").exists());
    let layout = std::fs::read_to_string(rdir.join("last")).unwrap();
    assert!(
        layout
            .lines()
            .any(|l| l.starts_with("window\talpha\t1\t:marked")),
        "{layout}"
    );
}
