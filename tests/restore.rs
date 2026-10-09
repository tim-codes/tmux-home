//! Agent-session restore end to end (pass 7), on a throwaway server: the
//! hook records sessions on live "claude" panes, `agents-snapshot` saves
//! them, the windows are then rebuilt as resurrect would (same session,
//! indexes and directories; fresh shells, new pane IDs), and
//! `restore-agents` types the resume command into each, which runs a fake
//! `claude` on `PATH` that logs its directory, account and arguments.

mod common;
use common::{TestEnv, TestServer, fake_claude, wait_until};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Dirs {
    base: PathBuf,
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

const BIN: &str = env!("CARGO_BIN_EXE_tmux-home");

fn hook(s: &TestServer, pane: &str, event: &str, payload: &str, config_dir: Option<&Path>) {
    let mut c = Command::new(BIN);
    c.args(["hook", "claude", event])
        .env("TMUX", format!("{},1,0", s.socket.display()))
        .env("TMUX_PANE", pane)
        .env_remove("CLAUDE_CONFIG_DIR");
    if let Some(d) = config_dir {
        c.env("CLAUDE_CONFIG_DIR", d);
    }
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(payload.as_bytes());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run(s: &TestServer, cmd: &str) {
    let out = Command::new(BIN)
        .args([cmd, "--socket"])
        .arg(&s.socket)
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

fn opt(s: &TestServer, pane: &str, name: &str) -> String {
    s.tmux(&["display", "-p", "-t", pane, &format!("#{{{name}}}")])
        .trim_end_matches('\n')
        .to_string()
}

/// A window at `alpha:<index>` running `cmd` (a shell when `None`) in
/// `dir`; its pane ID. The shell runs under `env -i` with `PATH` = `path`:
/// an interactive shell's startup files (the user's, or macOS's
/// path_helper) would rebuild `PATH` and could find a real `claude`
/// before the fake.
fn window(s: &TestServer, index: u32, dir: &Path, path: &str, cmd: Option<&str>) -> String {
    let target = format!("alpha:{index}");
    let dir = dir.to_str().unwrap();
    let shell = format!("exec /usr/bin/env -i PATH='{path}' /bin/sh -i");
    s.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        &target,
        "-c",
        dir,
        cmd.unwrap_or(&shell),
    ])
    .trim()
    .to_string()
}

/// A fake `claude` in `<base>/bin` that appends `$PWD|$CLAUDE_CONFIG_DIR
/// (or unset)|args` to `<base>/claude.log`; the bin dir and the log.
fn fake_claude_cli(base: &Path) -> (PathBuf, PathBuf) {
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = base.join("claude.log");
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$PWD\" \"${{CLAUDE_CONFIG_DIR-unset}}\" \"$*\" >> '{}'\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// A temp dir (canonical: tmux reports `/private/var/…` on macOS),
/// removed on drop.
fn temp_base(tag: &str) -> Dirs {
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "th-{tag}-{}-{}",
        std::process::id(),
        common::rand_suffix()
    ));
    std::fs::create_dir_all(&base).unwrap();
    Dirs { base }
}

fn payload(id: &str, transcript: &Path, cwd: &Path) -> String {
    serde_json::json!({
        "session_id": id,
        "transcript_path": transcript,
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "source": "startup",
    })
    .to_string()
}

#[test]
fn sessions_are_resumed_in_their_directory_and_account() {
    let env = TestEnv::new();
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "th-restore-{}-{}",
        std::process::id(),
        common::rand_suffix()
    ));
    let dirs = Dirs { base: base.clone() };
    let mk = |rel: &str| {
        let p = base.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    };
    let (proj_a, proj_b, proj_c) = (mk("it's a"), mk("b"), mk("c"));
    let acct = mk("acct");
    let (bin, log) = fake_claude_cli(&base);
    let transcript = |name: &str| {
        let p = base.join(format!("{name}.jsonl"));
        std::fs::write(&p, "{}\n").unwrap();
        p
    };
    let (ta, tb, tc) = (transcript("a"), transcript("b"), transcript("c"));
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let s = TestServer::start();

    // live agents: A in another account, B in the default one, C whose
    // transcript will be gone; window 4 is a plain shell
    let ids = [
        "00000000-0000-4000-8000-00000000000a",
        "00000000-0000-4000-8000-00000000000b",
        "00000000-0000-4000-8000-00000000000c",
    ];
    let a = window(&s, 1, &proj_a, &path, Some(&fake_claude()));
    let b = window(&s, 2, &proj_b, &path, Some(&fake_claude()));
    let c = window(&s, 3, &proj_c, &path, Some(&fake_claude()));
    window(&s, 4, &proj_a, &path, None);
    wait_until("fake claudes running", || {
        [&a, &b, &c]
            .iter()
            .all(|p| opt(&s, p, "pane_current_command") == "2.1.283")
    });
    hook(
        &s,
        &a,
        "SessionStart",
        &payload(ids[0], &ta, &proj_a),
        Some(&acct),
    );
    hook(&s, &b, "SessionStart", &payload(ids[1], &tb, &proj_b), None);
    hook(&s, &c, "SessionStart", &payload(ids[2], &tc, &proj_c), None);
    assert_eq!(opt(&s, &a, "@home_transcript"), ta.to_str().unwrap());
    assert_eq!(opt(&s, &a, "@home_config_dir"), acct.to_str().unwrap());
    assert_eq!(opt(&s, &b, "@home_config_dir"), "");

    run(&s, "agents-snapshot");
    let snap: tmux_home::restore::Snapshot = serde_json::from_slice(
        &std::fs::read(
            tmux_home::paths::Paths::for_socket(&s.socket)
                .unwrap()
                .state_dir
                .join("agents.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let locs: Vec<String> = snap.entries.iter().map(|e| e.location()).collect();
    assert_eq!(locs, ["alpha:1.0", "alpha:2.0", "alpha:3.0"]);

    // "reboot": the windows come back as shells where they were
    std::fs::remove_file(&tc).unwrap();
    for w in 1..=4 {
        s.tmux(&["kill-window", "-t", &format!("alpha:{w}")]);
    }
    let a2 = window(&s, 1, &proj_a, &path, None);
    let b2 = window(&s, 2, &proj_b, &path, None);
    let c2 = window(&s, 3, &proj_c, &path, None);
    let d2 = window(&s, 4, &proj_a, &path, None);
    s.tmux(&["set", "-g", "@home-restore-agents-delay", "0.2"]);

    // off by default: nothing happens
    run(&s, "restore-agents");
    std::thread::sleep(Duration::from_millis(600));
    assert!(!log.exists());
    assert_eq!(opt(&s, &a2, "@home_restore"), "");

    s.tmux(&["set", "-g", "@home-restore-agents", "on"]);
    let t0 = Instant::now();
    run(&s, "restore-agents");
    assert!(t0.elapsed() < Duration::from_secs(2), "returns at once");
    let state = env.state.join(tmux_home::paths::server_key(&s.socket));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&log).is_ok_and(|l| l.lines().count() == 2) {
        if Instant::now() > deadline {
            panic!(
                "not resumed; restore.log: {:?}; claude.log: {:?}; a2: {}",
                std::fs::read_to_string(state.join("restore.log")),
                std::fs::read_to_string(&log),
                s.tmux(&["capture-pane", "-p", "-t", &a2])
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // typed in snapshot order (restore.log), but each shell runs its line
    // when it gets to it
    let mut lines: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    lines.sort(); // "…/b" < "…/it's a"
    assert_eq!(
        lines,
        [
            format!("{}|unset|--resume {}", proj_b.display(), ids[1]),
            format!(
                "{}|{}|--resume {}",
                proj_a.display(),
                acct.display(),
                ids[0]
            ),
        ]
    );
    assert_eq!(opt(&s, &a2, "@home_restore_typed"), "1");
    assert_eq!(
        opt(&s, &c2, "@home_restore"),
        "",
        "transcript gone: not claimed"
    );
    assert_eq!(opt(&s, &d2, "@home_restore"), "");
    let rlog = std::fs::read_to_string(state.join("restore.log")).unwrap();
    assert!(rlog.contains("transcript gone"), "{rlog}");

    // once per server: a second run (a manual restore) types nothing
    run(&s, "restore-agents");
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
    let cmd = opt(&s, &b2, "pane_current_command");
    assert!(
        tmux_home::restore::restorable_shell(&cmd),
        "back at the shell: {cmd}"
    );
    drop(dirs);
}

/// Through tmux-resurrect itself (skipped when it isn't installed): its
/// save runs `agents-snapshot` from the post-save hook, the server goes
/// away, a new one on the same socket restores, and the post-restore hook
/// resumes the session. Hooks find the server from `$TMUX`, as in a real
/// config.
#[test]
fn through_resurrect_save_and_restore() {
    let scripts = std::path::PathBuf::from(std::env::var("HOME").unwrap())
        .join(".tmux/plugins/tmux-resurrect/scripts");
    if !scripts.join("restore.sh").exists() {
        eprintln!("skipped: tmux-resurrect isn't installed");
        return;
    }
    let _env = TestEnv::new();
    let dirs = temp_base("resurrect");
    let base = dirs.base.clone();
    let proj = base.join("proj");
    let rdir = base.join("resurrect");
    for d in [&proj, &rdir] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (bin, log) = fake_claude_cli(&base);
    let transcript = base.join("t.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let id = "00000000-0000-4000-8000-0000000000aa";
    let envs = [("HOME", base.as_os_str())];
    let setup = |s: &TestServer| {
        s.tmux(&["set", "-g", "@resurrect-dir", rdir.to_str().unwrap()]);
        s.tmux(&[
            "set",
            "-g",
            "@resurrect-hook-post-save-all",
            &format!("{BIN} agents-snapshot"),
        ]);
        s.tmux(&[
            "set",
            "-g",
            "@resurrect-hook-post-restore-all",
            &format!("{BIN} restore-agents; tmux set -g @home_restore_done 1"),
        ]);
        s.tmux(&["set", "-g", "@home-restore-agents", "on"]);
        s.tmux(&["set", "-g", "@home-restore-agents-delay", "0.3"]);
        // restored panes: a shell that can only find the fake claude
        s.tmux(&[
            "set",
            "-g",
            "default-command",
            &format!("exec /usr/bin/env -i PATH='{path}' /bin/sh -i"),
        ]);
    };

    let a = TestServer::start_with(&envs);
    setup(&a);
    let pane = window(&a, 1, &proj, &path, Some(&fake_claude()));
    wait_until("fake claude running", || {
        opt(&a, &pane, "pane_current_command") == "2.1.283"
    });
    hook(
        &a,
        &pane,
        "SessionStart",
        &payload(id, &transcript, &proj),
        None,
    );
    let r = a.tmux(&[
        "run-shell",
        &format!("{}/save.sh quiet 2>&1; echo rc=$?", scripts.display()),
    ]);
    assert!(r.contains("rc=0"), "{r}");
    let state = tmux_home::paths::Paths::for_socket(&a.socket)
        .unwrap()
        .state_dir;
    let snap = std::fs::read_to_string(state.join("agents.json")).expect("snapshot saved");
    assert!(snap.contains(id) && snap.contains("\"alpha\""), "{snap}");

    // the "reboot": the same socket, a fresh server
    let name = a.name.clone();
    drop(a);
    let out = Command::new("tmux")
        .envs(envs)
        .args([
            "-L",
            &name,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "0",
            "/bin/sh",
        ])
        .current_dir(common::neutral_cwd())
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let socket = Command::new("tmux")
        .args(["-L", &name, "display", "-p", "#{socket_path}"])
        .env_remove("TMUX")
        .output()
        .unwrap();
    let b = TestServer {
        name,
        socket: PathBuf::from(String::from_utf8(socket.stdout).unwrap().trim()),
    };
    setup(&b);
    let r = b.tmux(&[
        "run-shell",
        &format!("{}/restore.sh 2>&1; echo rc=$?", scripts.display()),
    ]);
    assert!(r.contains("rc=0"), "{r}");
    assert_eq!(opt(&b, "alpha:1", "@home_restore"), id, "claimed");
    wait_until("resumed", || {
        std::fs::read_to_string(&log).is_ok_and(|l| !l.is_empty())
    });
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("{}|unset|--resume {id}\n", proj.display())
    );
    assert_eq!(opt(&b, "alpha:1", "@home_restore_typed"), "1");
}
