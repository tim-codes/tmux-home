//! `tmux-home hook claude <event>` end to end: recorded payload shapes
//! (tests/fixtures/claude) piped into the binary, with `$TMUX_PANE` on a
//! pane of a throwaway server; asserts on the pane's `@home_*` options and
//! on the agent state the popup derives from a snapshot.

mod common;
use common::{TestEnv, TestServer, fake_claude};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tmux_home::agent::{PaneAgent, Status, WaitReason, pane_agent};
use tmux_home::tmux::{Tmux, snapshot::read_snapshot};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/claude/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// A server with a live "claude" pane (`fake_claude`, whose command is a
/// version number, as Claude's is); the pane's ID.
fn setup() -> (TestEnv, TestServer, String) {
    let env = TestEnv::new();
    let s = TestServer::start();
    let pane = s.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-n",
        "agent",
        &fake_claude(),
    ]);
    let pane = pane.trim().to_string();
    common::wait_until("fake claude running", || {
        s.tmux(&["display", "-p", "-t", &pane, "#{pane_current_command}"])
            .trim()
            == "2.1.283"
    });
    (env, s, pane)
}

/// Runs the hook binary with `stdin`, `$TMUX` set to `tmux` and
/// `$TMUX_PANE` to `pane` (each unset when `None`).
fn hook_raw(args: &[&str], tmux: Option<&str>, pane: Option<&str>, stdin: &str) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tmux-home"));
    c.arg("hook")
        .args(args)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    if let Some(t) = tmux {
        c.env("TMUX", t);
    }
    if let Some(p) = pane {
        c.env("TMUX_PANE", p);
    }
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // a hook that exits without reading may close stdin first
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait_with_output().unwrap()
}

fn tmux_env(s: &TestServer) -> String {
    format!("{},1,0", s.socket.display())
}

/// `hook claude <event>` with a fixture; asserts it was silent and 0.
fn hook(s: &TestServer, pane: &str, event: &str, fixture_name: &str) {
    let out = hook_raw(
        &["claude", event],
        Some(&tmux_env(s)),
        Some(pane),
        &fixture(fixture_name),
    );
    assert_silent_ok(&out);
}

fn assert_silent_ok(out: &Output) {
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The pane's `@home_*` option value.
fn opt(s: &TestServer, pane: &str, name: &str) -> String {
    s.tmux(&["display", "-p", "-t", pane, &format!("#{{{name}}}")])
        .trim_end_matches('\n')
        .to_string()
}

/// Every `@home_*` option set on the pane, by name.
fn home_opts(s: &TestServer, pane: &str) -> Vec<String> {
    s.tmux(&["show-options", "-p", "-t", pane])
        .lines()
        .filter(|l| l.starts_with("@home_"))
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect()
}

/// What the popup sees for the pane: its agent, from a fresh snapshot.
fn derived(s: &TestServer, pane: &str) -> Option<PaneAgent> {
    let snap = read_snapshot(&Tmux::new(s.socket.clone())).unwrap();
    snap.panes
        .iter()
        .find(|p| p.id == pane)
        .and_then(pane_agent)
}

#[test]
fn a_session_from_start_to_end() {
    let (_env, s, pane) = setup();
    hook(&s, &pane, "SessionStart", "session_start");
    assert_eq!(opt(&s, &pane, "@home_agent"), "claude");
    assert_eq!(opt(&s, &pane, "@home_status"), "idle");
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Idle);
    assert!(!a.stale && !a.needs_you());

    hook(&s, &pane, "UserPromptSubmit", "user_prompt_submit");
    assert_eq!(opt(&s, &pane, "@home_status"), "running");
    assert!(opt(&s, &pane, "@home_run_started").parse::<u64>().is_ok());
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Running);
    assert_eq!(
        a.state.prompt.as_deref(),
        Some("Add a unit test for the parser and run it then report back")
    );
    assert!(a.state.run_started.is_some());

    hook(&s, &pane, "Notification", "notification_permission");
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Waiting);
    assert_eq!(a.state.wait_reason, Some(WaitReason::Permission));
    assert!(a.needs_you());

    hook(&s, &pane, "Stop", "stop");
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Idle);
    assert_eq!(a.state.run_started, None);
    assert!(a.state.prompt_is_reply && !a.needs_you());

    hook(&s, &pane, "StopFailure", "stop_failure");
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Error);
    assert!(a.needs_you());

    hook(&s, &pane, "PermissionDenied", "permission_denied");
    assert_eq!(
        derived(&s, &pane).unwrap().state.wait_reason,
        Some(WaitReason::PermissionDenied)
    );

    hook(&s, &pane, "SessionEnd", "session_end");
    assert_eq!(home_opts(&s, &pane), Vec::<String>::new());
    assert_eq!(derived(&s, &pane), None);
}

/// A subagent's SessionEnd must not wipe its parent; the parent's own
/// SessionEnd, once its subagents are done, does.
#[test]
fn subagent_session_end_leaves_the_parent() {
    let (_env, s, pane) = setup();
    hook(&s, &pane, "SessionStart", "session_start");
    hook(&s, &pane, "UserPromptSubmit", "user_prompt_submit");
    hook(&s, &pane, "SubagentStart", "subagent_start");
    assert_eq!(opt(&s, &pane, "@home_subagents"), "Explore:agent-0001");
    hook(&s, &pane, "SessionEnd", "session_end");
    hook(&s, &pane, "SessionEnd", "session_end_subagent");
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Running, "parent still running");
    assert_eq!(a.state.subagents, ["Explore"]);
    assert!(a.state.run_started.is_some());
    hook(&s, &pane, "SubagentStop", "subagent_stop");
    assert_eq!(opt(&s, &pane, "@home_subagents"), "");
    assert_eq!(derived(&s, &pane).unwrap().state.status, Status::Running);
    hook(&s, &pane, "Stop", "stop");
    assert_eq!(derived(&s, &pane).unwrap().state.status, Status::Idle);
    hook(&s, &pane, "SessionEnd", "session_end");
    assert_eq!(derived(&s, &pane), None);
}

/// With the sidebar's options alongside: `@home_*` wins for the pane, and
/// the sidebar's live background shell routes Stop to background (keeping
/// the run's start). A pane without `@home_*` falls back to `@pane_*`.
#[test]
fn alongside_the_sidebar() {
    let (_env, s, pane) = setup();
    for (k, v) in [
        ("@pane_agent", "claude"),
        ("@pane_status", "waiting"),
        ("@pane_bg_cmd", "npm run dev"),
    ] {
        s.tmux(&["set-option", "-p", "-t", &pane, k, v]);
    }
    assert_eq!(derived(&s, &pane).unwrap().state.status, Status::Waiting);
    hook(&s, &pane, "UserPromptSubmit", "user_prompt_submit");
    let started = opt(&s, &pane, "@home_run_started");
    assert_eq!(derived(&s, &pane).unwrap().state.status, Status::Running);
    hook(&s, &pane, "Stop", "stop");
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Background);
    assert_eq!(a.state.bg_cmd.as_deref(), Some("npm run dev"));
    assert_eq!(opt(&s, &pane, "@home_run_started"), started);
    // the sidebar's sweep finds the shell gone; next Stop is idle
    s.tmux(&["set-option", "-p", "-u", "-t", &pane, "@pane_bg_cmd"]);
    hook(&s, &pane, "Stop", "stop");
    let a = derived(&s, &pane).unwrap();
    assert_eq!(a.state.status, Status::Idle);
    assert_eq!((a.state.bg_cmd, a.state.run_started), (None, None));
    // our options never touch the sidebar's
    assert_eq!(opt(&s, &pane, "@pane_status"), "waiting");
}

/// Values tmux could misread (a trailing `;`, a leading `-`) land intact.
#[test]
fn awkward_values_round_trip() {
    let (_env, s, pane) = setup();
    let p = r#"{"prompt": "-n fix it; then ship;"}"#;
    let out = hook_raw(
        &["claude", "UserPromptSubmit"],
        Some(&tmux_env(&s)),
        Some(&pane),
        p,
    );
    assert_silent_ok(&out);
    assert_eq!(opt(&s, &pane, "@home_prompt"), "-n fix it; then ship;");
    assert_eq!(opt(&s, &pane, "@home_status"), "running");
}

/// args, $TMUX, $TMUX_PANE, stdin.
type Case<'a> = (Vec<&'a str>, Option<&'a str>, Option<&'a str>, &'a str);

/// Whatever goes wrong, exit 0 with no output; bad input is logged.
#[test]
fn failures_are_silent_no_ops() {
    let (env, s, pane) = setup();
    let t = tmux_env(&s);
    let ok = fixture("stop");
    let cases: Vec<Case> = vec![
        (vec![], Some(&t), Some(&pane), &ok),
        (vec!["claude"], Some(&t), Some(&pane), &ok),
        (vec!["claude", "Stop"], Some(&t), None, &ok),
        (vec!["claude", "Stop"], None, Some(&pane), &ok),
        (
            vec!["claude", "Stop"],
            Some("/nonexistent/sock,1,0"),
            Some(&pane),
            &ok,
        ),
        (vec!["claude", "Stop"], Some(&t), Some("%999"), &ok),
        (vec!["claude", "Stop"], Some(&t), Some(&pane), "{not json"),
        (vec!["claude", "Stop"], Some(&t), Some(&pane), "[1, 2]"),
        (vec!["nosuch", "Stop"], Some(&t), Some(&pane), &ok),
        (vec!["claude", "PostToolUse"], Some(&t), Some(&pane), &ok),
        (
            vec!["claude", "Stop", "extra", "--flag"],
            Some(&t),
            Some(&pane),
            "{not json",
        ),
        (vec!["--help"], Some(&t), Some(&pane), &ok),
    ];
    for (args, tm, p, stdin) in cases {
        let out = hook_raw(&args, tm, p, stdin);
        assert_eq!(out.status.code(), Some(0), "{args:?} {tm:?} {p:?}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{args:?}");
    }
    assert_eq!(
        home_opts(&s, &pane),
        Vec::<String>::new(),
        "nothing written"
    );
    let key = tmux_home::paths::server_key(&s.socket);
    let log = std::fs::read_to_string(env.state.join(key).join("hook.log")).unwrap();
    assert!(log.contains("claude Stop: bad JSON"), "{log}");
    assert!(log.contains("not a JSON object"), "{log}");
    assert!(log.contains("claude Stop: no $TMUX_PANE"), "{log}");
    assert!(log.contains("nosuch Stop: unknown agent"), "{log}");
    assert!(log.contains("%999"), "{log}");
}

/// The log rotates at its cap instead of growing.
#[test]
fn hook_log_is_capped() {
    let (env, s, pane) = setup();
    let dir = env.state.join(tmux_home::paths::server_key(&s.socket));
    std::fs::create_dir_all(&dir).unwrap();
    let big = "x".repeat(tmux_home::hook::LOG_CAP as usize);
    std::fs::write(dir.join("hook.log"), &big).unwrap();
    let out = hook_raw(&["claude", "Stop"], Some(&tmux_env(&s)), Some(&pane), "{");
    assert_silent_ok(&out);
    assert_eq!(
        std::fs::read_to_string(dir.join("hook.log.1")).unwrap(),
        big
    );
    let log = std::fs::read_to_string(dir.join("hook.log")).unwrap();
    assert!(log.contains("bad JSON") && log.len() < 200, "{log}");
}

/// 100 invocations: p50 and p95 wall time, printed (run with
/// `--nocapture` to see them). The bound is loose (debug build, shared CI
/// machines); the release numbers are in the README.
#[test]
fn timing() {
    let (_env, s, pane) = setup();
    let t = tmux_env(&s);
    let (a, b) = (fixture("user_prompt_submit"), fixture("stop"));
    let mut d: Vec<Duration> = (0..100)
        .map(|i| {
            let (ev, p) = if i % 2 == 0 {
                ("UserPromptSubmit", &a)
            } else {
                ("Stop", &b)
            };
            let t0 = Instant::now();
            let out = hook_raw(&["claude", ev], Some(&t), Some(&pane), p);
            let el = t0.elapsed();
            assert_eq!(out.status.code(), Some(0));
            el
        })
        .collect();
    d.sort();
    let (p50, p95) = (d[49], d[94]);
    eprintln!("hook timing over 100 runs: p50 {p50:?}, p95 {p95:?}");
    assert_eq!(opt(&s, &pane, "@home_status"), "idle");
    assert!(p50 < Duration::from_millis(100), "p50 {p50:?}");
}
