# R0 — Foundations + control-mode spike: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust `tmux-home` binary whose daemon (one per tmux server) keeps a live snapshot of every session/window/pane, pushes it to clients over a unix socket, and serves `tmux-home query --json` — with the control-mode-vs-poll decision made from measurements.

**Architecture:** Single crate (lib + bin). `tmux::` reads tmux state with one `list-panes -a -F` call and learns *when* to re-read either from a control-mode client (`ControlMode`) or a timer with change detection (`Poll`), both behind `TmuxSource`. `daemon::` owns the latest `Snapshot` and pushes it as NDJSON to subscribers; `client::` connects (starting the daemon if needed) and falls back to a direct one-shot read.

**Tech Stack:** Rust 1.98 (edition 2024), tokio, serde/serde_json, clap (derive), anyhow, sha1_smol. tmux 3.7c. No other runtime deps in R0.

**Spec:** `docs/superpowers/specs/2026-09-29-rust-daemon-design.md` (§2, §3, §4, §9 paths, §12, §13 R0). Read it before starting.

## Global Constraints

- Every tmux call in code and tests targets an explicit socket (`-S <path>`); tests use `tmux -L th-test-<random> -f /dev/null` and never the default server.
- Never touch the user's real state dir or runtime dir in tests: tests set `TMUX_HOME_RUNTIME_DIR` and `TMUX_HOME_STATE_DIR` to temp dirs.
- Socket/lock path: `${TMUX_HOME_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-$TMPDIR}/tmux-home-$UID}/<sha1(socket_path) first 12 hex>.{sock,lock}`.
- Protocol is newline-delimited JSON; every client request carries `"v"` = `env!("CARGO_PKG_VERSION")`.
- Writes target tmux IDs (`$n`, `@n`, `%n`), never indexes or names.
- The control-mode client must be excluded from every client list tmux-home builds (flag `control-mode` in `#{client_flags}`).
- Add dependencies with `cargo add` (it resolves current versions); do not hand-write version numbers from memory.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass at every commit.
- The bash popup (`bin/tmux-home`, `tests/run`) is untouched in R0 and its suite still passes.

## Review Focus

1. **tmux server has no sessions / dies mid-read** → `read_snapshot` returns an error the caller handles; the daemon exits cleanly (no panic, no busy loop). Tests: `dead_server_is_an_error` (Task 3), `query_and_subscribe` exit check (Task 9).
2. **Names containing tabs, `:` or unicode** (window named `a	b:ç ✳`) → round-trip exactly through the snapshot. Test in Task 3.
3. **Two daemons started at once for the same server** → exactly one serves; the second exits 0. Test in Task 9.
4. **Stale socket file from a crashed daemon** → the next daemon removes it and binds. Test in Task 9.
5. **Client from a different version** → daemon replies `restart` and exits; client can start a new one. Test in Task 9.

---

## File structure

```
Cargo.toml
src/main.rs              CLI (clap): daemon | query | spike-control | --version
src/lib.rs               module declarations
src/paths.rs             runtime/state paths derived from a tmux socket path
src/tmux/mod.rs          Tmux handle: run commands against one socket
src/tmux/snapshot.rs     Snapshot types + list-panes/list-clients parsing
src/tmux/control.rs      control-mode line parser (pure)
src/tmux/source.rs       TmuxSource trait, Poll, ControlMode
src/ipc.rs               protocol types + NDJSON framing
src/daemon.rs            lock, socket server, push loop, version handshake
src/client.rs            connect-or-start, subscribe, degraded one-shot
tests/common/mod.rs      TestServer (throwaway tmux) + temp dirs
tests/snapshot.rs        Task 3
tests/source.rs          Tasks 5–6
tests/daemon.rs          Tasks 8–9
tests/client.rs          Task 10
docs/superpowers/notes/r0-control-mode.md   spike results + decision (Task 7)
```

---

### Task 1: Crate scaffold

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/lib.rs`, `rust-toolchain.toml`
- Modify: `.gitignore` (create if missing)

**Interfaces:**
- Produces: binary `tmux-home` with `--version`; library crate `tmux_home`.

- [ ] **Step 1: Create the crate**

```bash
cd ~/dev/tmux-home
cargo init --name tmux-home --edition 2024
cargo add tokio --features full
cargo add serde --features derive
cargo add serde_json anyhow sha1_smol
cargo add clap --features derive
printf '/target\n' >> .gitignore
printf '[toolchain]\nchannel = "stable"\ncomponents = ["clippy", "rustfmt"]\n' > rust-toolchain.toml
```

`cargo init` must not clobber `bin/`, `tests/run` or `README.md`; check `git status` shows only new files plus `.gitignore`.

- [ ] **Step 2: Write `src/lib.rs` and `src/main.rs`**

```rust
// src/lib.rs
pub mod paths;
pub mod tmux;
pub mod ipc;
pub mod daemon;
pub mod client;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```

```rust
// src/main.rs
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tmux-home", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon for one tmux server (normally started automatically).
    Daemon {
        #[arg(long)]
        socket: std::path::PathBuf,
        #[arg(long, value_enum, default_value = "control")]
        source: tmux_home::tmux::source::SourceKind,
    },
    /// Print the current snapshot as JSON.
    Query {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// R0 spike: measure control-mode side effects on a server and print a report.
    SpikeControl {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.cmd {
        Cmd::Daemon { socket, source } => rt.block_on(tmux_home::daemon::run(socket, source)),
        Cmd::Query { socket, json: _ } => rt.block_on(tmux_home::client::query(socket)),
        Cmd::SpikeControl { socket } => rt.block_on(tmux_home::tmux::source::spike_control(socket)),
    }
}
```

Create empty stub modules so it compiles: `src/paths.rs`, `src/ipc.rs`, `src/daemon.rs`, `src/client.rs`, `src/tmux/mod.rs` (with `pub mod snapshot; pub mod control; pub mod source;`) and the three submodules. Stubs: `pub async fn run(_: std::path::PathBuf, _: super::tmux::source::SourceKind) -> anyhow::Result<()> { anyhow::bail!("not yet") }` in daemon, `pub async fn query(_: Option<std::path::PathBuf>) -> anyhow::Result<()> { anyhow::bail!("not yet") }` in client, and in `source.rs`:

```rust
#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum SourceKind { Control, Poll }

pub async fn spike_control(_: std::path::PathBuf) -> anyhow::Result<()> { anyhow::bail!("not yet") }
```

- [ ] **Step 3: Verify**

Run: `cargo build && ./target/debug/tmux-home --version && cargo fmt --check && cargo clippy --all-targets -- -D warnings && tests/run | tail -1`
Expected: `tmux-home 0.1.0`, no warnings, bash suite `failed: 0`.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock rust-toolchain.toml .gitignore src
git commit -m "R0: Rust crate scaffold (daemon/query/spike-control stubs)"
```

---

### Task 2: Paths + test harness

**Files:**
- Create: `src/paths.rs`, `tests/common/mod.rs`, `tests/paths.rs`

**Interfaces:**
- Produces:
  - `paths::Paths { pub sock: PathBuf, pub lock: PathBuf, pub state_dir: PathBuf }`
  - `paths::Paths::for_socket(tmux_socket: &Path) -> anyhow::Result<Paths>`
  - `paths::server_key(tmux_socket: &Path) -> String` (12 lowercase hex chars)
  - test helper `common::TestServer::start() -> TestServer` with `.socket: PathBuf`, `.tmux(args: &[&str]) -> String` (stdout, panics on failure), `Drop` kills the server; `common::TestEnv::new()` setting `TMUX_HOME_RUNTIME_DIR` / `TMUX_HOME_STATE_DIR` to fresh temp dirs (returned struct holds them).

- [ ] **Step 1: Failing test**

```rust
// tests/paths.rs
mod common;
use std::path::Path;
use tmux_home::paths::{server_key, Paths};

#[test]
fn key_is_stable_12_hex() {
    let k = server_key(Path::new("/private/tmp/tmux-501/default"));
    assert_eq!(k.len(), 12);
    assert!(k.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(k, server_key(Path::new("/private/tmp/tmux-501/default")));
    assert_ne!(k, server_key(Path::new("/private/tmp/tmux-501/other")));
}

#[test]
fn paths_honour_env_overrides() {
    let env = common::TestEnv::new();
    let p = Paths::for_socket(Path::new("/tmp/x/sock")).unwrap();
    let key = server_key(Path::new("/tmp/x/sock"));
    assert_eq!(p.sock, env.runtime.join(format!("{key}.sock")));
    assert_eq!(p.lock, env.runtime.join(format!("{key}.lock")));
    assert_eq!(p.state_dir, env.state.join(&key));
}
```

- [ ] **Step 2: Run** — `cargo test --test paths` → FAIL (unresolved imports).

- [ ] **Step 3: Implement**

```rust
// src/paths.rs
use std::path::{Path, PathBuf};

pub struct Paths {
    pub sock: PathBuf,
    pub lock: PathBuf,
    pub state_dir: PathBuf,
}

pub fn server_key(tmux_socket: &Path) -> String {
    let hex = sha1_smol::Sha1::from(tmux_socket.as_os_str().as_encoded_bytes()).hexdigest();
    hex[..12].to_string()
}

fn runtime_root() -> PathBuf {
    if let Some(d) = std::env::var_os("TMUX_HOME_RUNTIME_DIR") {
        return d.into();
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| std::env::var_os("TMPDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into());
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc_getuid() };
    base.join(format!("tmux-home-{uid}"))
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn state_root() -> PathBuf {
    if let Some(d) = std::env::var_os("TMUX_HOME_STATE_DIR") {
        return d.into();
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("tmux-home")
}

impl Paths {
    pub fn for_socket(tmux_socket: &Path) -> anyhow::Result<Paths> {
        let key = server_key(tmux_socket);
        let rt = runtime_root();
        Ok(Paths {
            sock: rt.join(format!("{key}.sock")),
            lock: rt.join(format!("{key}.lock")),
            state_dir: state_root().join(key),
        })
    }
}
```

```rust
// tests/common/mod.rs
#![allow(dead_code)]
use std::path::PathBuf;
use std::process::Command;

pub struct TestEnv {
    pub runtime: PathBuf,
    pub state: PathBuf,
}

impl TestEnv {
    pub fn new() -> TestEnv {
        let base = std::env::temp_dir().join(format!("th-test-{}-{}", std::process::id(), rand_suffix()));
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
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
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
            .args(["-L", &name, "-f", "/dev/null", "new-session", "-d", "-s", "alpha", "-x", "200", "-y", "50", "/bin/sh"])
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let socket = Command::new("tmux")
            .args(["-L", &name, "display", "-p", "#{socket_path}"])
            .env_remove("TMUX")
            .output()
            .unwrap();
        let socket = PathBuf::from(String::from_utf8(socket.stdout).unwrap().trim());
        TestServer { name, socket }
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux").arg("-S").arg(&self.socket).args(args).env_remove("TMUX").output().unwrap();
        assert!(out.status.success(), "tmux {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux").arg("-S").arg(&self.socket).arg("kill-server").output();
    }
}
```

Env mutation is process-global: in `Cargo.toml` add, for every integration test that uses `TestEnv`, nothing — instead run integration tests with `--test-threads=1`. Add to `.cargo/config.toml`:

```toml
[env]
RUST_TEST_THREADS = "1"
```

- [ ] **Step 4: Run** — `cargo test --test paths` → PASS.
- [ ] **Step 5: Commit** — `git add -A src/paths.rs tests .cargo && git commit -m "R0: runtime/state paths keyed by tmux socket; test harness"`

---

### Task 3: Tmux handle + snapshot reader

**Files:**
- Create: `src/tmux/mod.rs` (replace stub), `src/tmux/snapshot.rs`, `tests/snapshot.rs`

**Interfaces:**
- Consumes: `common::TestServer`.
- Produces:
  - `tmux::Tmux { socket: PathBuf }`, `Tmux::new(socket: PathBuf) -> Tmux`, `async fn run(&self, args: &[&str]) -> anyhow::Result<String>` (stdout; `Err` with stderr on non-zero exit)
  - `snapshot::Snapshot { sessions: Vec<Session>, windows: Vec<Window>, panes: Vec<Pane>, clients: Vec<Client> }` (all `Serialize, Deserialize, Clone, Debug, PartialEq`)
  - `Session { id: String, name: String, attached: u32 }`, `Window { id: String, session_id: String, index: u32, name: String, automatic_rename: bool, active: bool }`, `Pane { id: String, window_id: String, session_id: String, index: u32, active: bool, current_command: String, current_path: String, title: String }`, `Client { name: String, tty: String, session_id: String }`
  - `async fn read_snapshot(t: &Tmux) -> anyhow::Result<(Snapshot, u64)>` — the `u64` is a hash of the raw tmux output, used for change detection.

- [ ] **Step 1: Failing test**

```rust
// tests/snapshot.rs
mod common;
use tmux_home::tmux::{snapshot::read_snapshot, Tmux};

#[tokio::test]
async fn reads_sessions_windows_panes() {
    let s = common::TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "two"]);
    s.tmux(&["split-window", "-d", "-t", "alpha:two"]);
    s.tmux(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
    let (snap, _) = read_snapshot(&Tmux::new(s.socket.clone())).await.unwrap();
    let names: Vec<_> = snap.sessions.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert_eq!(snap.windows.len(), 3);
    let two = snap.windows.iter().find(|w| w.name == "two").unwrap();
    assert_eq!(snap.panes.iter().filter(|p| p.window_id == two.id).count(), 2);
    assert!(snap.windows.iter().all(|w| w.id.starts_with('@')));
    assert!(snap.panes.iter().all(|p| p.id.starts_with('%')));
}

#[tokio::test]
async fn odd_names_round_trip() {
    let s = common::TestServer::start();
    let name = "a\tb:ç ✳ x";
    s.tmux(&["rename-window", "-t", "alpha:0", name]);
    let (snap, _) = read_snapshot(&Tmux::new(s.socket.clone())).await.unwrap();
    assert_eq!(snap.windows[0].name, name);
    assert!(!snap.windows[0].automatic_rename);
}

#[tokio::test]
async fn hash_changes_only_on_change() {
    let s = common::TestServer::start();
    let t = Tmux::new(s.socket.clone());
    let (_, h1) = read_snapshot(&t).await.unwrap();
    let (_, h2) = read_snapshot(&t).await.unwrap();
    assert_eq!(h1, h2);
    s.tmux(&["new-window", "-d", "-t", "alpha"]);
    let (_, h3) = read_snapshot(&t).await.unwrap();
    assert_ne!(h1, h3);
}

#[tokio::test]
async fn dead_server_is_an_error() {
    let s = common::TestServer::start();
    let t = Tmux::new(s.socket.clone());
    s.tmux(&["kill-server"]);
    assert!(read_snapshot(&t).await.is_err());
}
```

- [ ] **Step 2: Run** — `cargo test --test snapshot` → FAIL.

- [ ] **Step 3: Implement**

```rust
// src/tmux/mod.rs
pub mod control;
pub mod snapshot;
pub mod source;

use std::path::PathBuf;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct Tmux {
    pub socket: PathBuf,
}

impl Tmux {
    pub fn new(socket: PathBuf) -> Tmux {
        Tmux { socket }
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new("tmux");
        c.arg("-S").arg(&self.socket).env_remove("TMUX").kill_on_drop(true);
        c
    }

    pub async fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = self.command().args(args).output().await?;
        if !out.status.success() {
            anyhow::bail!("tmux {:?}: {}", args, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
```

Field separator is ASCII unit separator `\x1f` (cannot appear in names typed at a prompt). Names are the **last** field so a stray separator can't shift the others.

```rust
// src/tmux/snapshot.rs
use super::Tmux;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub windows: Vec<Window>,
    pub panes: Vec<Pane>,
    pub clients: Vec<Client>,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Session { pub id: String, pub name: String, pub attached: u32 }
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Window { pub id: String, pub session_id: String, pub index: u32, pub name: String, pub automatic_rename: bool, pub active: bool }
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pane { pub id: String, pub window_id: String, pub session_id: String, pub index: u32, pub active: bool, pub current_command: String, pub current_path: String, pub title: String }
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Client { pub name: String, pub tty: String, pub session_id: String }

const SEP: char = '\x1f';

const PANE_FMT: &str = "#{session_id}\x1f#{window_id}\x1f#{window_index}\x1f#{window_active}\x1f#{automatic-rename}\x1f#{pane_id}\x1f#{pane_index}\x1f#{pane_active}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{session_name}\x1f#{pane_title}\x1f#{window_name}";
const CLIENT_FMT: &str = "#{client_name}\x1f#{client_tty}\x1f#{session_id}\x1f#{client_flags}";

pub async fn read_snapshot(t: &Tmux) -> anyhow::Result<(Snapshot, u64)> {
    let panes = t.run(&["list-panes", "-a", "-F", PANE_FMT]).await?;
    let clients = t.run(&["list-clients", "-F", CLIENT_FMT]).await?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    panes.hash(&mut h);
    clients.hash(&mut h);
    Ok((parse(&panes, &clients)?, h.finish()))
}

pub fn parse(panes: &str, clients: &str) -> anyhow::Result<Snapshot> {
    let mut s = Snapshot::default();
    for line in panes.lines() {
        let f: Vec<&str> = line.splitn(13, SEP).collect();
        anyhow::ensure!(f.len() == 13, "bad list-panes line: {line:?}");
        let (sid, wid) = (f[0].to_string(), f[1].to_string());
        if !s.sessions.iter().any(|x| x.id == sid) {
            s.sessions.push(Session { id: sid.clone(), name: f[10].to_string(), attached: 0 });
        }
        if !s.windows.iter().any(|w| w.id == wid && w.session_id == sid) {
            s.windows.push(Window {
                id: wid.clone(), session_id: sid.clone(), index: f[2].parse()?, active: f[3] == "1",
                automatic_rename: f[4] == "1", name: f[12].to_string(),
            });
        }
        s.panes.push(Pane {
            id: f[5].to_string(), window_id: wid, session_id: sid, index: f[6].parse()?, active: f[7] == "1",
            current_command: f[8].to_string(), current_path: f[9].to_string(), title: f[11].to_string(),
        });
    }
    for line in clients.lines() {
        let f: Vec<&str> = line.splitn(4, SEP).collect();
        anyhow::ensure!(f.len() == 4, "bad list-clients line: {line:?}");
        if f[3].split(',').any(|x| x == "control-mode") {
            continue; // our own control client, never a user client
        }
        s.clients.push(Client { name: f[0].into(), tty: f[1].into(), session_id: f[2].into() });
        if let Some(sess) = s.sessions.iter_mut().find(|x| x.id == f[2]) {
            sess.attached += 1;
        }
    }
    s.sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(s)
}
```

Note: the window name is the last field but the pane title (field 11) is not; a `\x1f` inside a pane title would shift the name. Guard: `splitn(13)` keeps the tail intact, and titles containing `\x1f` are not producible by tmux's `select-pane -T` in practice. Leave as is; covered by Review Focus #2 for names.

- [ ] **Step 4: Run** — `cargo test --test snapshot` → PASS (4 tests).
- [ ] **Step 5: Commit** — `git add src/tmux tests/snapshot.rs && git commit -m "R0: Tmux handle and snapshot reader"`

---

### Task 4: Control-mode line parser (pure)

**Files:**
- Create: `src/tmux/control.rs` (replace stub)

**Interfaces:**
- Produces:
  - `enum Line { BlockStart, BlockEnd { error: bool }, Notify(Notification), Other }`
  - `enum Notification { Changed(&'static str), Exit(Option<String>), Output }` — `Changed` carries the notification name (e.g. `"window-add"`) for every structural or subscription notification.
  - `fn parse_line(line: &str) -> Line`
  - `const STRUCTURAL: &[&str]`

- [ ] **Step 1: Failing unit tests** (in `control.rs`, `#[cfg(test)] mod tests`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks() {
        assert!(matches!(parse_line("%begin 1727600000 12 1"), Line::BlockStart));
        assert!(matches!(parse_line("%end 1727600000 12 1"), Line::BlockEnd { error: false }));
        assert!(matches!(parse_line("%error 1727600000 12 1"), Line::BlockEnd { error: true }));
    }

    #[test]
    fn structural_and_subscription_are_changes() {
        for l in ["%window-add @3", "%window-close @3", "%unlinked-window-add @4", "%window-renamed @3 new name",
                  "%sessions-changed", "%session-window-changed $1 @3", "%layout-change @3 b25f,80x24,0,0,2 b25f,80x24,0,0,2 *",
                  "%window-pane-changed @3 %5", "%subscription-changed th $1 @3 0 %5 : fish", "%session-renamed $1 x"] {
            assert!(matches!(parse_line(l), Line::Notify(Notification::Changed(_))), "{l}");
        }
    }

    #[test]
    fn exit_and_output() {
        assert!(matches!(parse_line("%exit"), Line::Notify(Notification::Exit(None))));
        assert!(matches!(parse_line("%exit server exited"), Line::Notify(Notification::Exit(Some(_)))));
        assert!(matches!(parse_line("%output %1 hello"), Line::Notify(Notification::Output)));
        assert!(matches!(parse_line("some command output"), Line::Other));
        assert!(matches!(parse_line("%client-detached /dev/ttys001"), Line::Other));
    }
}
```

- [ ] **Step 2: Run** — `cargo test --lib control` → FAIL.

- [ ] **Step 3: Implement**

```rust
// src/tmux/control.rs
//! tmux control-mode (`tmux -C`) line classification. The daemon only needs
//! to know *that* something changed; it then re-reads the whole snapshot.

pub const STRUCTURAL: &[&str] = &[
    "window-add", "window-close", "window-renamed", "unlinked-window-add", "unlinked-window-close",
    "unlinked-window-renamed", "sessions-changed", "session-changed", "session-renamed",
    "session-window-changed", "layout-change", "window-pane-changed", "pane-mode-changed",
    "subscription-changed", "client-session-changed",
];

#[derive(Debug)]
pub enum Line { BlockStart, BlockEnd { error: bool }, Notify(Notification), Other }

#[derive(Debug)]
pub enum Notification { Changed(&'static str), Exit(Option<String>), Output }

pub fn parse_line(line: &str) -> Line {
    let Some(rest) = line.strip_prefix('%') else { return Line::Other };
    let (name, tail) = rest.split_once(' ').unwrap_or((rest, ""));
    match name {
        "begin" => Line::BlockStart,
        "end" => Line::BlockEnd { error: false },
        "error" => Line::BlockEnd { error: true },
        "exit" => Line::Notify(Notification::Exit((!tail.is_empty()).then(|| tail.to_string()))),
        "output" | "extended-output" => Line::Notify(Notification::Output),
        _ => match STRUCTURAL.iter().find(|s| **s == name) {
            Some(s) => Line::Notify(Notification::Changed(s)),
            None => Line::Other,
        },
    }
}
```

- [ ] **Step 4: Run** — `cargo test --lib control` → PASS.
- [ ] **Step 5: Commit** — `git add src/tmux/control.rs && git commit -m "R0: control-mode line parser"`

---

### Task 5: `TmuxSource` + `Poll`

**Files:**
- Modify: `src/tmux/source.rs`
- Create: `tests/source.rs`

**Interfaces:**
- Consumes: `read_snapshot`, `Tmux`.
- Produces:
  - `pub enum SourceEvent { Snapshot(Snapshot), Gone }` — `Gone` = server unreachable; the source stops after sending it.
  - `pub fn start(kind: SourceKind, tmux: Tmux) -> tokio::sync::mpsc::Receiver<SourceEvent>` — spawns the source task; first event is always an initial `Snapshot`.
  - `Poll`: re-reads every 500 ms; sends only when the hash changes.

- [ ] **Step 1: Failing test**

```rust
// tests/source.rs
mod common;
use std::time::Duration;
use tmux_home::tmux::{source::{start, SourceEvent, SourceKind}, Tmux};

async fn next_snap(rx: &mut tokio::sync::mpsc::Receiver<SourceEvent>, within: Duration) -> Option<SourceEvent> {
    tokio::time::timeout(within, rx.recv()).await.ok().flatten()
}

async fn check_source(kind: SourceKind, max_latency: Duration) {
    let s = common::TestServer::start();
    let mut rx = start(kind, Tmux::new(s.socket.clone()));
    let Some(SourceEvent::Snapshot(first)) = next_snap(&mut rx, Duration::from_secs(2)).await else { panic!("no initial") };
    assert_eq!(first.windows.len(), 1);
    // no change -> no event
    assert!(next_snap(&mut rx, Duration::from_millis(700)).await.is_none());
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "fresh"]);
    let Some(SourceEvent::Snapshot(snap)) = next_snap(&mut rx, max_latency).await else { panic!("no change seen") };
    assert!(snap.windows.iter().any(|w| w.name == "fresh"));
    s.tmux(&["kill-server"]);
    loop {
        match next_snap(&mut rx, Duration::from_secs(3)).await {
            Some(SourceEvent::Gone) => break,
            Some(SourceEvent::Snapshot(_)) => continue,
            None => panic!("source did not report Gone"),
        }
    }
}

#[tokio::test]
async fn poll_source() {
    check_source(SourceKind::Poll, Duration::from_millis(1200)).await;
}
```

- [ ] **Step 2: Run** — `cargo test --test source poll_source` → FAIL.

- [ ] **Step 3: Implement** (Control arm added in Task 6; until then it falls back to Poll)

```rust
// src/tmux/source.rs
use super::{snapshot::{read_snapshot, Snapshot}, Tmux};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum SourceKind { Control, Poll }

#[derive(Debug)]
pub enum SourceEvent { Snapshot(Snapshot), Gone }

pub const POLL_EVERY: Duration = Duration::from_millis(500);
pub const RESYNC_EVERY: Duration = Duration::from_secs(5);

pub fn start(kind: SourceKind, tmux: Tmux) -> mpsc::Receiver<SourceEvent> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let r = match kind {
            SourceKind::Poll => poll(tmux, tx.clone()).await,
            SourceKind::Control => super::source::control(tmux, tx.clone()).await,
        };
        if let Err(e) = r {
            eprintln!("tmux-home: source ended: {e:#}");
        }
        let _ = tx.send(SourceEvent::Gone).await;
    });
    rx
}

/// Re-read and send if the hash differs from `last`. Returns the new hash.
async fn refresh(tmux: &Tmux, tx: &mpsc::Sender<SourceEvent>, last: Option<u64>) -> anyhow::Result<u64> {
    let (snap, h) = read_snapshot(tmux).await?;
    if Some(h) != last {
        tx.send(SourceEvent::Snapshot(snap)).await?;
    }
    Ok(h)
}

async fn poll(tmux: Tmux, tx: mpsc::Sender<SourceEvent>) -> anyhow::Result<()> {
    let mut last = None;
    loop {
        last = Some(refresh(&tmux, &tx, last).await?);
        tokio::time::sleep(POLL_EVERY).await;
    }
}

async fn control(tmux: Tmux, tx: mpsc::Sender<SourceEvent>) -> anyhow::Result<()> {
    poll(tmux, tx).await // replaced in Task 6
}

pub async fn spike_control(_: std::path::PathBuf) -> anyhow::Result<()> { anyhow::bail!("not yet") }
```

- [ ] **Step 4: Run** — `cargo test --test source poll_source` → PASS.
- [ ] **Step 5: Commit** — `git add src/tmux/source.rs tests/source.rs && git commit -m "R0: TmuxSource with Poll implementation"`

---

### Task 6: `ControlMode` source

**Files:**
- Modify: `src/tmux/source.rs` (replace `control`), `tests/source.rs` (add test)

**Interfaces:**
- Consumes: `control::parse_line`, `refresh`.
- Produces: `SourceKind::Control` behaviour: attach `tmux -S <sock> -C attach-session -f no-output,ignore-size,read-only` (stdin piped, kept open), subscribe with `refresh-client -B th-panes:%*:#{pane_current_command}#{pane_current_path}#{pane_title}`, re-read on any `Changed` (debounced 30 ms), resync every `RESYNC_EVERY`, end on `%exit` or EOF.

- [ ] **Step 1: Failing test** (append to `tests/source.rs`)

```rust
#[tokio::test]
async fn control_source() {
    check_source(SourceKind::Control, Duration::from_millis(300)).await;
}

#[tokio::test]
async fn control_sees_other_sessions_and_renames() {
    let s = common::TestServer::start();
    s.tmux(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
    let mut rx = start(SourceKind::Control, Tmux::new(s.socket.clone()));
    let _ = next_snap(&mut rx, Duration::from_secs(2)).await;
    // a window added to a session the control client is NOT attached to
    s.tmux(&["new-window", "-d", "-t", "beta", "-n", "elsewhere"]);
    let Some(SourceEvent::Snapshot(snap)) = next_snap(&mut rx, Duration::from_millis(300)).await else { panic!("missed other-session window") };
    assert!(snap.windows.iter().any(|w| w.name == "elsewhere"));
    s.tmux(&["rename-window", "-t", "beta:elsewhere", "renamed"]);
    let Some(SourceEvent::Snapshot(snap)) = next_snap(&mut rx, Duration::from_millis(300)).await else { panic!("missed rename") };
    assert!(snap.windows.iter().any(|w| w.name == "renamed"));
}
```

If `control_sees_other_sessions_and_renames` cannot pass within 300 ms (tmux doesn't notify for unattached sessions and the subscription doesn't cover it), **do not weaken the test** — record it in Task 7's notes; the spike decides the default.

- [ ] **Step 2: Run** — `cargo test --test source control` → FAIL (latency: poll fallback is too slow for 300 ms).

- [ ] **Step 3: Implement**

```rust
async fn control(tmux: Tmux, tx: mpsc::Sender<SourceEvent>) -> anyhow::Result<()> {
    use super::control::{parse_line, Line, Notification};
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut child = tmux
        .command()
        .args(["-C", "attach-session", "-f", "no-output,ignore-size,read-only"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    stdin
        .write_all(b"refresh-client -B 'th-panes:%*:#{pane_current_command}#{pane_current_path}#{pane_title}'\n")
        .await?;

    let mut last = Some(refresh(&tmux, &tx, None).await?);
    let mut dirty = false;
    let debounce = Duration::from_millis(30);
    let mut resync = tokio::time::interval(RESYNC_EVERY);
    resync.tick().await; // first tick is immediate
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) }; // EOF: server gone
                match parse_line(&line) {
                    Line::Notify(Notification::Exit(_)) => return Ok(()),
                    Line::Notify(Notification::Changed(_)) => dirty = true,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(debounce), if dirty => {
                dirty = false;
                last = Some(refresh(&tmux, &tx, last).await?);
            }
            _ = resync.tick() => {
                last = Some(refresh(&tmux, &tx, last).await?);
            }
        }
    }
}
```

`stdin` must stay alive for the loop (dropping it makes tmux detach the control client) — it is held in scope until the function returns.

- [ ] **Step 4: Run** — `cargo test --test source` → `poll_source`, `control_source` PASS; record the outcome of `control_sees_other_sessions_and_renames` (pass/fail + observed latency) for Task 7.
- [ ] **Step 5: Commit** — `git add -A src tests && git commit -m "R0: ControlMode source (control client + subscription, debounced re-read)"`

---

### Task 7: Control-mode spike — measure and decide

**Files:**
- Modify: `src/tmux/source.rs` (`spike_control`)
- Create: `docs/superpowers/notes/r0-control-mode.md`
- Modify: `src/main.rs` default for `--source` only if the decision is Poll.

**Interfaces:**
- Produces: `tmux-home spike-control --socket <path>` printing a report; the default `SourceKind` for `daemon`.

- [ ] **Step 1: Implement the spike command**

It runs against a server you point it at (always a test server — never the default socket), and prints, before and 1 s after starting a `SourceKind::Control` source:

```rust
pub async fn spike_control(socket: std::path::PathBuf) -> anyhow::Result<()> {
    let t = Tmux::new(socket);
    let probe = |t: Tmux| async move {
        let sess = t.run(&["list-sessions", "-F", "#{session_name} attached=#{session_attached} size=#{window_width}x#{window_height}"]).await?;
        let clients = t.run(&["list-clients", "-F", "#{client_name} #{client_flags} #{session_name} #{client_width}x#{client_height}"]).await?;
        let doa = t.run(&["show", "-gv", "detach-on-destroy"]).await?;
        anyhow::Ok(format!("sessions:\n{sess}clients:\n{clients}detach-on-destroy: {doa}"))
    };
    println!("== before ==\n{}", probe(t.clone()).await?);
    let mut rx = start(SourceKind::Control, t.clone());
    let _ = rx.recv().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("== with control client ==\n{}", probe(t.clone()).await?);
    // latency for a change in a session the control client is not attached to
    let other = t.run(&["new-session", "-d", "-P", "-F", "#{session_id}", "/bin/sh"]).await?;
    let t0 = std::time::Instant::now();
    t.run(&["new-window", "-d", "-t", other.trim()]).await?;
    loop {
        match tokio::time::timeout(Duration::from_secs(6), rx.recv()).await {
            Ok(Some(SourceEvent::Snapshot(_))) => { println!("other-session change seen after {:?}", t0.elapsed()); break; }
            Ok(Some(SourceEvent::Gone)) | Ok(None) => anyhow::bail!("source gone"),
            Err(_) => { println!("other-session change NOT seen within 6 s"); break; }
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Run it against a throwaway server with a real attached client of a different size**

```bash
tmux -L th-spike -f /dev/null new-session -d -s main -x 200 -y 50 /bin/sh
tmux -L th-spike-outer -f /dev/null new-session -d -x 120 -y 30 "tmux -L th-spike attach -t main"
sleep 1
./target/debug/tmux-home spike-control --socket "$(tmux -L th-spike display -p '#{socket_path}')" | tee /tmp/spike.txt
tmux -L th-spike kill-server; tmux -L th-spike-outer kill-server
```

- [ ] **Step 3: Record results and decide**

Write `docs/superpowers/notes/r0-control-mode.md` with: the report output; the `control_sees_other_sessions_and_renames` result from Task 6; and a decision against these criteria —

| Criterion | Required for Control to be default |
| --- | --- |
| `session_attached` of the session the control client joined | unchanged, **or** tmux-home's own snapshot corrects it (the `control-mode` filter in `parse`) and nothing else user-visible depends on it |
| window size of that session | unchanged (the 120x30 client still determines it) |
| other-session changes | seen within 300 ms |
| `detach-on-destroy` interaction | killing the control client's session doesn't kill the daemon's source silently (re-attach or Gone) |

If all pass: keep `default_value = "control"`. Otherwise set `default_value = "poll"` in `main.rs`, and add a line to the spec §3 recording the reason. Either way, if a fix (e.g. attaching to a dedicated hidden session, or `refresh-client -B` for `@*` windows) makes a criterion pass, implement it in `control()` and re-run Steps 2–3.

- [ ] **Step 4: Commit** — `git add docs/superpowers/notes src && git commit -m "R0: control-mode spike results and source decision"`

---

### Task 8: Protocol types + framing

**Files:**
- Modify: `src/ipc.rs`

**Interfaces:**
- Produces:
  - `#[serde(tag = "op", rename_all = "snake_case")] enum Request { Subscribe { v: String, client: String }, Query { v: String } }`
  - `#[serde(tag = "type", rename_all = "snake_case")] enum Reply { Snapshot { seq: u64, data: Snapshot }, Restart, Error { msg: String } }`
  - `async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> anyhow::Result<()>` (one JSON line + `\n`)
  - `async fn read_msg<R: AsyncBufRead + Unpin, T: DeserializeOwned>(r: &mut R) -> anyhow::Result<Option<T>>` (`None` on EOF)

- [ ] **Step 1: Failing unit test** (in `ipc.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn round_trip() {
        let (a, b) = tokio::io::duplex(4096);
        let (_, mut w) = tokio::io::split(a);
        let (r, _) = tokio::io::split(b);
        let mut r = tokio::io::BufReader::new(r);
        write_msg(&mut w, &Request::Subscribe { v: "1".into(), client: "popup".into() }).await.unwrap();
        let got: Request = read_msg(&mut r).await.unwrap().unwrap();
        assert!(matches!(got, Request::Subscribe { ref client, .. } if client == "popup"));
        drop(w);
        assert!(read_msg::<_, Request>(&mut r).await.unwrap().is_none());
    }
    #[test]
    fn wire_shape() {
        let s = serde_json::to_string(&Request::Query { v: "0.1.0".into() }).unwrap();
        assert_eq!(s, r#"{"op":"query","v":"0.1.0"}"#);
        assert_eq!(serde_json::to_string(&Reply::Restart).unwrap(), r#"{"type":"restart"}"#);
    }
}
```

- [ ] **Step 2: Run** — `cargo test --lib ipc` → FAIL.
- [ ] **Step 3: Implement**

```rust
// src/ipc.rs
use crate::tmux::snapshot::Snapshot;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Subscribe { v: String, client: String },
    Query { v: String },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Snapshot { seq: u64, data: Snapshot },
    Restart,
    Error { msg: String },
}

impl Request {
    pub fn version(&self) -> &str {
        match self { Request::Subscribe { v, .. } | Request::Query { v } => v }
    }
}

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_msg<R: AsyncBufRead + Unpin, T: DeserializeOwned>(r: &mut R) -> anyhow::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(line.trim_end())?))
}
```

- [ ] **Step 4: Run** — PASS. **Step 5: Commit** — `git commit -am "R0: NDJSON protocol types and framing"`

---

### Task 9: Daemon

**Files:**
- Modify: `src/daemon.rs`
- Create: `tests/daemon.rs`

**Interfaces:**
- Consumes: `Paths`, `source::start`, `ipc::*`, `Tmux`.
- Produces: `pub async fn run(tmux_socket: PathBuf, kind: SourceKind) -> anyhow::Result<()>`:
  1. create runtime dir (0700); open + `File::try_lock()` the lock file — if already locked, return `Ok(())` (another daemon serves);
  2. remove a leftover socket file, bind `UnixListener` at `paths.sock`;
  3. start the source; keep `latest: Option<(u64 seq, Snapshot)>` in a `tokio::sync::watch` channel;
  4. per connection: read one `Request`; if `version() != VERSION` → write `Reply::Restart` and **exit the process after replying** (flush, remove socket, return); `Query` → write the latest snapshot and close; `Subscribe` → write latest then each new one as it arrives;
  5. on `SourceEvent::Gone` → remove the socket file and return `Ok(())`.

- [ ] **Step 1: Failing tests**

```rust
// tests/daemon.rs
mod common;
use std::time::Duration;
use tmux_home::{ipc::{read_msg, write_msg, Reply, Request}, paths::Paths, tmux::source::SourceKind};
use tokio::{io::BufReader, net::UnixStream};

async fn connect(p: &Paths) -> UnixStream {
    for _ in 0..100 {
        if let Ok(s) = UnixStream::connect(&p.sock).await { return s; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket never appeared");
}

async fn ask(p: &Paths, req: Request) -> Option<Reply> {
    let s = connect(p).await;
    let (r, mut w) = s.into_split();
    write_msg(&mut w, &req).await.unwrap();
    read_msg(&mut BufReader::new(r)).await.unwrap()
}

fn v() -> String { tmux_home::VERSION.to_string() }

#[tokio::test]
async fn query_and_subscribe() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let Some(Reply::Snapshot { data, .. }) = ask(&p, Request::Query { v: v() }).await else { panic!() };
    assert_eq!(data.windows.len(), 1);

    let (r, mut w) = connect(&p).await.into_split();
    let mut r = BufReader::new(r);
    write_msg(&mut w, &Request::Subscribe { v: v(), client: "test".into() }).await.unwrap();
    let _first: Reply = read_msg(&mut r).await.unwrap().unwrap();
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "pushed"]);
    let got = tokio::time::timeout(Duration::from_secs(2), read_msg::<_, Reply>(&mut r)).await.unwrap().unwrap().unwrap();
    let Reply::Snapshot { data, .. } = got else { panic!() };
    assert!(data.windows.iter().any(|w| w.name == "pushed"));

    s.tmux(&["kill-server"]);
    tokio::time::timeout(Duration::from_secs(5), d).await.expect("daemon should exit with its server").unwrap().unwrap();
    assert!(!p.sock.exists());
}

#[tokio::test]
async fn second_daemon_is_a_no_op() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d1 = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let _ = connect(&p).await;
    let d2 = tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll);
    tokio::time::timeout(Duration::from_secs(2), d2).await.expect("second daemon must return at once").unwrap();
    assert!(ask(&p, Request::Query { v: v() }).await.is_some(), "first daemon still serving");
    d1.abort();
}

#[tokio::test]
async fn stale_socket_file_is_replaced() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    std::fs::create_dir_all(p.sock.parent().unwrap()).unwrap();
    std::fs::write(&p.sock, b"stale").unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    assert!(ask(&p, Request::Query { v: v() }).await.is_some());
    d.abort();
}

#[tokio::test]
async fn version_mismatch_restarts() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let reply = ask(&p, Request::Query { v: "0.0.0-old".into() }).await;
    assert!(matches!(reply, Some(Reply::Restart)));
    tokio::time::timeout(Duration::from_secs(2), d).await.expect("daemon exits after restart").unwrap().unwrap();
}
```

- [ ] **Step 2: Run** — `cargo test --test daemon` → FAIL.

- [ ] **Step 3: Implement**

```rust
// src/daemon.rs
use crate::{
    ipc::{read_msg, write_msg, Reply, Request},
    paths::Paths,
    tmux::{snapshot::Snapshot, source::{self, SourceEvent, SourceKind}, Tmux},
    VERSION,
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};
use tokio::{io::BufReader, net::{UnixListener, UnixStream}, sync::{watch, Notify}};

type Latest = watch::Receiver<Option<(u64, Snapshot)>>;

pub async fn run(tmux_socket: PathBuf, kind: SourceKind) -> anyhow::Result<()> {
    let paths = Paths::for_socket(&tmux_socket)?;
    let dir = paths.sock.parent().expect("socket has a parent");
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&paths.lock)?;
    if lock.try_lock().is_err() {
        return Ok(()); // another daemon serves this server
    }
    let _ = std::fs::remove_file(&paths.sock);
    let listener = UnixListener::bind(&paths.sock)?;

    let (tx, latest) = watch::channel(None);
    let mut events = source::start(kind, Tmux::new(tmux_socket));
    let restart = Arc::new(Notify::new());
    let mut seq = 0u64;

    let result = loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(SourceEvent::Snapshot(s)) => { seq += 1; let _ = tx.send(Some((seq, s))); }
                Some(SourceEvent::Gone) | None => break Ok(()),
            },
            conn = listener.accept() => {
                let (stream, _) = conn?;
                tokio::spawn(serve(stream, latest.clone(), restart.clone()));
            }
            _ = restart.notified() => break Ok(()),
        }
    };
    let _ = std::fs::remove_file(&paths.sock);
    drop(lock);
    result
}

async fn serve(stream: UnixStream, mut latest: Latest, restart: Arc<Notify>) {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let Ok(Some(req)) = read_msg::<_, Request>(&mut r).await else { return };
    if req.version() != VERSION {
        let _ = write_msg(&mut w, &Reply::Restart).await;
        restart.notify_one();
        return;
    }
    // wait for the first snapshot if the source hasn't produced one yet
    if latest.wait_for(|s| s.is_some()).await.is_err() { return; }
    let cur = latest.borrow_and_update().clone();
    if send(&mut w, cur).await.is_err() { return; }
    if let Request::Subscribe { .. } = req {
        while latest.changed().await.is_ok() {
            let cur = latest.borrow_and_update().clone();
            if send(&mut w, cur).await.is_err() { return; }
        }
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, cur: Option<(u64, Snapshot)>) -> anyhow::Result<()> {
    let Some((seq, data)) = cur else { anyhow::bail!("no snapshot") };
    write_msg(w, &Reply::Snapshot { seq, data }).await
}
```

- [ ] **Step 4: Run** — `cargo test --test daemon` → 4 PASS; `cargo clippy --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** — `git add src/daemon.rs tests/daemon.rs && git commit -m "R0: daemon — lock, socket, push, version handshake, exit with server"`

---

### Task 10: Client — connect-or-start, degraded query

**Files:**
- Modify: `src/client.rs`, `src/main.rs` (Query socket default)
- Create: `tests/client.rs`

**Interfaces:**
- Consumes: `Paths`, `ipc`, `read_snapshot`, `VERSION`.
- Produces:
  - `pub async fn current_socket(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf>` — `explicit` (from `--socket`), else `$TMUX` (first comma field), else error.
  - `pub async fn snapshot(tmux_socket: &Path, budget: Duration) -> anyhow::Result<(Snapshot, bool /*from_daemon*/)>` — try daemon within `budget`; on `Restart` or failure, spawn `current_exe() daemon --socket <path>` detached (stdin/out/err null, `setsid` via `process_group(0)`), and return a direct `read_snapshot` result with `from_daemon = false`.
  - `pub async fn query(socket: Option<PathBuf>) -> anyhow::Result<()>` — prints `serde_json::to_string_pretty(&snapshot)`.

- [ ] **Step 1: Failing test**

```rust
// tests/client.rs
mod common;
use std::time::Duration;
use tmux_home::client::snapshot;

#[tokio::test]
async fn degraded_then_daemon() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    // no daemon yet: direct read, and a daemon gets started in the background
    let (snap, from_daemon) = snapshot(&s.socket, Duration::from_millis(150)).await.unwrap();
    assert_eq!(snap.windows.len(), 1);
    assert!(!from_daemon);
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if snapshot(&s.socket, Duration::from_millis(150)).await.unwrap().1 { ok = true; break; }
    }
    assert!(ok, "background daemon never came up");
    s.tmux(&["kill-server"]); // daemon exits with it (covered in daemon tests)
}

#[test]
fn query_cli_prints_json() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tmux-home"))
        .args(["query", "--json", "--socket"]).arg(&s.socket)
        .output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["sessions"][0]["name"], "alpha");
}
```

The background daemon is spawned via `current_exe()`, which in tests is the test binary — so `snapshot` must spawn `env!("CARGO_BIN_EXE_tmux-home")` when set at compile time for tests. Implement with `std::env::var_os("TMUX_HOME_BIN").map(PathBuf::from).unwrap_or(std::env::current_exe()?)`, and in `tests/client.rs` set `TMUX_HOME_BIN` to `env!("CARGO_BIN_EXE_tmux-home")` at the top of `degraded_then_daemon` (inside the same `unsafe` pattern as `TestEnv`).

- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement**

```rust
// src/client.rs
use crate::{ipc::{read_msg, write_msg, Reply, Request}, paths::Paths, tmux::{snapshot::{read_snapshot, Snapshot}, Tmux}, VERSION};
use std::{path::{Path, PathBuf}, process::Stdio, time::Duration};
use tokio::{io::BufReader, net::UnixStream};

pub async fn current_socket(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit { return Ok(p); }
    let tmux = std::env::var("TMUX").map_err(|_| anyhow::anyhow!("not inside tmux and no --socket given"))?;
    Ok(PathBuf::from(tmux.split(',').next().unwrap_or_default()))
}

async fn ask_daemon(p: &Paths) -> anyhow::Result<Reply> {
    let s = UnixStream::connect(&p.sock).await?;
    let (r, mut w) = s.into_split();
    write_msg(&mut w, &Request::Query { v: VERSION.into() }).await?;
    read_msg(&mut BufReader::new(r)).await?.ok_or_else(|| anyhow::anyhow!("daemon closed"))
}

pub fn spawn_daemon(tmux_socket: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let bin = std::env::var_os("TMUX_HOME_BIN").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?;
    std::process::Command::new(bin)
        .arg("daemon").arg("--socket").arg(tmux_socket)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

pub async fn snapshot(tmux_socket: &Path, budget: Duration) -> anyhow::Result<(Snapshot, bool)> {
    let p = Paths::for_socket(tmux_socket)?;
    match tokio::time::timeout(budget, ask_daemon(&p)).await {
        Ok(Ok(Reply::Snapshot { data, .. })) => return Ok((data, true)),
        Ok(Ok(Reply::Restart)) => {
            tokio::time::sleep(Duration::from_millis(50)).await; // old daemon exits
            spawn_daemon(tmux_socket)?;
        }
        _ => spawn_daemon(tmux_socket)?,
    }
    let (snap, _) = read_snapshot(&Tmux::new(tmux_socket.to_path_buf())).await?;
    Ok((snap, false))
}

pub async fn query(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let sock = current_socket(socket).await?;
    let (snap, _) = snapshot(&sock, Duration::from_millis(150)).await?;
    println!("{}", serde_json::to_string_pretty(&snap)?);
    Ok(())
}
```

- [ ] **Step 4: Run** — `cargo test` (all) → PASS; `tests/run` bash suite still `failed: 0`.
- [ ] **Step 5: Commit** — `git add src/client.rs src/main.rs tests/client.rs && git commit -m "R0: client — daemon query with degraded direct read and lazy start"`

---

### Task 11: Start the daemon from the TPM entry; push; PR

**Files:**
- Modify: `tmux-home.tmux`, `README.md` (Build section)

**Interfaces:**
- Consumes: `tmux-home daemon --socket`.

- [ ] **Step 1: Edit `tmux-home.tmux`** — after the bindings loop, append:

```bash
# Phase 2 daemon: start one for this server if the Rust binary is built.
# A second start is a no-op (lock), and it exits with the server.
RUST_BIN="$CURRENT_DIR/target/release/tmux-home"
if [[ -x $RUST_BIN ]]; then
	t run-shell -b "$(printf '%q' "$RUST_BIN") daemon --socket #{q:socket_path}"
fi
```

- [ ] **Step 2: README** — add a "Build (phase 2, in progress)" section: `cargo build --release`; `tmux-home query --json` prints the daemon's snapshot; the popup is still the bash one.

- [ ] **Step 3: Verify on a throwaway server** (never the default one)

```bash
cargo build --release
tmux -L th-tpm -f /dev/null new-session -d /bin/sh
TMUX_HOME_TMUX="tmux -L th-tpm" ./tmux-home.tmux
sleep 1
./target/release/tmux-home query --json --socket "$(tmux -L th-tpm display -p '#{socket_path}')" | head -5
pgrep -fl "tmux-home daemon"
tmux -L th-tpm kill-server; sleep 1; pgrep -fl "tmux-home daemon" || echo "daemon exited"
```

Expected: JSON printed; one daemon while the server lives; "daemon exited" after.

- [ ] **Step 4: Full checks** — `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && tests/run | tail -1`.
- [ ] **Step 5: Commit and push** — `git commit -am "R0: start the daemon from the TPM entry; build docs" && git push`, then update draft PR #2's description with the R0 summary and the spike decision.

---

## Later milestones (outline — each gets its own detailed plan before work starts)

- **R1 — popup parity.** ratatui + crossterm + nucleo-matcher popup over `client::snapshot`/subscribe; grouped list, type-first filter, preview (capture-pane), `⏎` via invoking client, `^r`/`M-r`, `^x` with confirm rules, `^t` + `closed.json` store (`store::` module, `state.lock`), `M-↑↓`, `M-n`, F1 help. Port the 143 bash checks as Rust integration tests (real popup on a test server, `capture-pane`), then switch `@home-keys` to the Rust binary and delete `bin/tmux-home` + `tests/run`.
- **R2 — agents.** `agents::AgentAdapter`, Claude adapter ported from tmux-agent-sidebar's handlers, `tmux-home hook claude <event>` (writes `@home_*` pane options, then socket, 50 ms budget), snapshot gains `@home_*` fields, derived status/NEEDS YOU/staleness, agent card, `^g`, filter tokens; fixtures in `tests/fixtures/claude/`.
- **R3 — git + repos.** Vendor stray's `git.rs`/`model.rs`/`scan.rs`/`ignore.rs`; apply spec §6 items 1–8; per-window `RepoStatus` with fast-first publishing, adaptive interval, 4-permit semaphore (scan ≤ 2), timeouts; Repos view, `Tab` views, `M-d` density, badges; `NOTICE`.
- **R4 — sidebar.** `sidebar-toggle [--session]`, `tmux-home sidebar` renderer (shared widgets, compact density), auto-create via `%window-add`, `@home-sidebar-*` options, last-pane cleanup, `sidebars.json` + resurrect handling.
- **R5 — cutover.** Dotfiles PR per spec §11.
