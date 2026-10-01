#![allow(dead_code)]
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct TestEnv {
    base: PathBuf,
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
        // SAFETY: tests that use TestEnv run single-threaded per process
        // (RUST_TEST_THREADS=1 in .cargo/config.toml).
        unsafe {
            std::env::set_var("TMUX_HOME_RUNTIME_DIR", &runtime);
            std::env::set_var("TMUX_HOME_STATE_DIR", &state);
        }
        TestEnv {
            base,
            runtime,
            state,
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
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
        // New windows run /bin/sh, not the user's $SHELL: predictable, fast
        // to start, and no rc files.
        for opt in [["automatic-rename", "off"], ["default-shell", "/bin/sh"]] {
            let out = Command::new("tmux")
                .args(["-L", &name, "set-option", "-g", opt[0], opt[1]])
                .env_remove("TMUX")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "set-option {opt:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
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
        // tmux leaves its socket file behind after kill-server.
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Pause after each keystroke sent to the outer pane, so the popup has
/// drawn before the next one.
pub const KEY_GAP: Duration = Duration::from_millis(150);
/// Pause after an Escape. Through the nested client a lone Escape reaches
/// the popup a few hundred ms late; a key sent sooner merges with it into
/// Meta-<key>.
pub const ESC_GAP: Duration = Duration::from_millis(600);

/// Polls `f` every 50 ms for up to 8 s; panics naming `what` on timeout.
pub fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A second throwaway server whose one pane runs a real client attached to
/// the inner server, so tmux-home's key binding can be pressed
/// (`send-keys`) and the popup read back (`capture-pane`).
pub struct Outer {
    pub name: String,
}

impl Outer {
    pub fn attach(inner: &TestServer, target: &str, cols: u16, rows: u16) -> Outer {
        let name = format!("th-outer-{}-{}", std::process::id(), rand_suffix());
        let attach = format!(
            "env -u TMUX tmux -S '{}' attach -t '{}'",
            inner.socket.display(),
            target
        );
        let (cols, rows) = (cols.to_string(), rows.to_string());
        let out = Command::new("tmux")
            .args(["-L", &name, "-f", "/dev/null", "new-session", "-d"])
            .args(["-s", "outer", "-x", &cols, "-y", &rows, &attach])
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        wait_until("a client attached to the inner server", || {
            !inner
                .tmux(&["list-clients", "-F", "#{client_name}"])
                .trim()
                .is_empty()
        });
        Outer { name }
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-L", &self.name])
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

    pub fn screen(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "outer"])
    }

    /// `send-keys` with tmux key names (`C-r`, `M-Down`, `Escape`, `F1`).
    pub fn keys(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", "outer"];
        args.extend_from_slice(keys);
        self.tmux(&args);
        std::thread::sleep(if keys.contains(&"Escape") {
            ESC_GAP
        } else {
            KEY_GAP
        });
    }

    /// Literal text (`--`: text may start with `-`).
    pub fn typed(&self, text: &str) {
        self.tmux(&["send-keys", "-t", "outer", "-l", "--", text]);
        std::thread::sleep(KEY_GAP);
    }

    /// Waits up to 8 s for the screen to match `re` (multi-line: `^` is a
    /// line start); panics with the screen.
    pub fn wait_for(&self, re: &str) {
        let r = regex::Regex::new(&format!("(?m){re}")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let s = self.screen();
            if r.is_match(&s) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "screen never matched {re:?}:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits up to 8 s for `re` to disappear from the screen.
    pub fn wait_gone(&self, re: &str) {
        let r = regex::Regex::new(&format!("(?m){re}")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let s = self.screen();
            if !r.is_match(&s) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "screen still matches {re:?}:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn popup_open(&self) -> bool {
        self.screen().contains("F1 help")
    }

    /// Presses `prefix .` and waits for the filter line.
    pub fn open_popup(&self) {
        self.keys(&["C-b", "."]);
        self.wait_for(r"^> .*\d+/\d+");
    }

    /// The popup row under the cursor (`▌`).
    pub fn cursor_row(&self) -> Option<String> {
        self.screen()
            .lines()
            .find(|l| l.starts_with('▌'))
            .map(str::to_string)
    }

    /// Waits until the cursor row shows window `name` (whole word after
    /// the index column).
    pub fn wait_cursor_on(&self, name: &str) {
        self.wait_for(&format!(r"^▌.*\d  {}( |$)", regex::escape(name)));
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.name, "kill-server"])
            .output();
    }
}

/// Runs tmux-home.tmux against `s`, using the test build of the binary.
pub fn install_binding(s: &TestServer) {
    let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tmux-home.tmux"))
        .env("TMUX_HOME_TMUX", format!("tmux -S {}", s.socket.display()))
        .env("TMUX_HOME_BIN", env!("CARGO_BIN_EXE_tmux-home"))
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tmux-home.tmux: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The bash suite's fixture: sessions alpha (editor, "win two") and beta
/// (logs, whose ACTIVE pane is a sidebar, and build), tmux-home's binding
/// installed (which starts a daemon), and a 200x50 client on alpha:editor.
/// Bind as `let (_env, s, o) = popup_fixture();` so the drop order (outer,
/// server, temp dirs) is right.
pub fn popup_fixture() -> (TestEnv, TestServer, Outer) {
    popup_fixture_sized(200, 50)
}

pub fn popup_fixture_sized(cols: u16, rows: u16) -> (TestEnv, TestServer, Outer) {
    let env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["rename-window", "-t", "alpha:0", "editor"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "win two"]);
    s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-x",
        "200",
        "-y",
        "50",
        "-n",
        "logs",
    ]);
    s.tmux(&["new-window", "-d", "-t", "beta:", "-n", "build"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "beta:logs"]);
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "beta:logs.1",
        "@pane_role",
        "sidebar",
    ]);
    s.tmux(&["select-pane", "-t", "beta:logs.1"]);
    s.tmux(&[
        "send-keys",
        "-t",
        "beta:logs.0",
        "echo MAIN-PANE-MARKER",
        "Enter",
    ]);
    s.tmux(&[
        "send-keys",
        "-t",
        "beta:logs.1",
        "echo SIDEBAR-MARKER",
        "Enter",
    ]);
    s.tmux(&["set-option", "-g", "escape-time", "50"]);
    install_binding(&s);
    let o = Outer::attach(&s, "alpha:editor", cols, rows);
    (env, s, o)
}

pub fn fmt(s: &TestServer, target: &str, f: &str) -> String {
    s.tmux(&["display-message", "-p", "-t", target, f])
        .trim()
        .to_string()
}

pub fn wid(s: &TestServer, target: &str) -> String {
    fmt(s, target, "#{window_id}")
}

pub fn window_ids(s: &TestServer) -> Vec<String> {
    s.tmux(&["list-windows", "-a", "-F", "#{window_id}"])
        .lines()
        .map(String::from)
        .collect()
}

pub fn window_names(s: &TestServer, session: &str) -> Vec<String> {
    s.tmux(&["list-windows", "-t", session, "-F", "#W"])
        .lines()
        .map(String::from)
        .collect()
}

pub fn sessions(s: &TestServer) -> Vec<String> {
    s.tmux(&["list-sessions", "-F", "#S"])
        .lines()
        .map(String::from)
        .collect()
}

/// "<session>:<window name>" of the (first) attached client.
pub fn client_at(s: &TestServer) -> String {
    s.tmux(&["list-clients", "-F", "#{session_name}:#{window_name}"])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Session names of the window rows on screen, top to bottom.
pub fn row_sessions(screen: &str) -> Vec<String> {
    let r = regex::Regex::new(r"^[▌ ](\S+) +[▶ ] +\d+  ").unwrap();
    screen
        .lines()
        .filter_map(|l| r.captures(l).map(|c| c[1].to_string()))
        .collect()
}
