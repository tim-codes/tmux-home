//! Agent-session restore (pass 7): after a reboot, tmux-resurrect brings
//! back every pane as a plain shell; the panes that were running Claude
//! Code get `claude --resume <id>` typed into them, in the session's
//! directory and account. Opt-in (`@home-restore-agents on`); it resumes
//! sessions that were running, and never starts a new agent.
//!
//! Three parts:
//!
//! - **Record.** `tmux-home hook claude …` keeps the pane's session in its
//!   `@home_*` options: `@home_session_id` plus the resume keys
//!   `@home_transcript`, `@home_cwd` and `@home_config_dir` (see
//!   `agent::claude`). Pane options die with the server, need no pruning
//!   and are written atomically by tmux, so the live record is the pane
//!   itself; SessionEnd's teardown wipes it.
//! - **Snapshot** (`tmux-home agents-snapshot`, from resurrect's
//!   `@resurrect-hook-post-save-all`). Pane IDs don't survive a restore;
//!   resurrect restores by `session:window.pane`, so the record is saved
//!   under that location, read from the same layout resurrect just saved,
//!   to `<state_dir>/agents.json`. Only panes that run Claude at that
//!   moment are saved: a session the user exited, or that crashed, is not
//!   resumed whatever its last hook said.
//! - **Restore** (`tmux-home restore-agents`, from
//!   `@resurrect-hook-post-restore-all`). Once per server: each saved
//!   entry whose pane is back at a shell prompt, in the saved directory,
//!   with its transcript still on disk, is claimed (`@home_restore`), and a
//!   detached worker types the command into each claimed pane, one every
//!   `@home-restore-agents-delay` seconds (marking it
//!   `@home_restore_typed`), so a dozen sessions don't start their MCP
//!   servers at once and resurrect isn't held up. Typing into the shell
//!   (not `respawn-pane`) leaves a usable shell when claude exits.
//!
//! Everything is logged to `<state_dir>/restore.log`; neither command ever
//! fails resurrect's hook.

use crate::agent::AgentKind;
use crate::tmux::Tmux;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The snapshot, in the server's state dir.
pub const SNAPSHOT: &str = "agents.json";
/// The log both commands write.
pub const LOG: &str = "restore.log";
/// `on` enables `restore-agents` (default off).
pub const OPT_ON: &str = "@home-restore-agents";
/// Seconds before each launch (default `DEFAULT_DELAY`).
pub const OPT_DELAY: &str = "@home-restore-agents-delay";
pub const DEFAULT_DELAY: f64 = 2.0;
/// Server option: `restore-agents` has run on this server (Unix seconds).
pub const GUARD: &str = "@home_agents_restored";
/// Pane option: the session id `restore-agents` claimed the pane for.
pub const CLAIM: &str = "@home_restore";
/// Pane option: the resume command has been typed into the pane.
pub const TYPED: &str = "@home_restore_typed";

/// One saved agent pane.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub session: String,
    pub window: u32,
    pub pane: u32,
    /// `pane_current_path` when saved (what resurrect restores the shell
    /// into); the restored pane must be there too.
    pub path: String,
    pub session_id: String,
    pub transcript: String,
    /// The session's launch directory (`cd` target).
    pub cwd: String,
    /// `$CLAUDE_CONFIG_DIR`; `None` for the default account.
    #[serde(default)]
    pub config_dir: Option<String>,
}

impl Entry {
    pub fn location(&self) -> String {
        format!("{}:{}.{}", self.session, self.window, self.pane)
    }
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Unix seconds.
    pub saved: u64,
    pub entries: Vec<Entry>,
}

/// One pane, as `PANE_FMT` lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LivePane {
    pub id: String,
    pub window: u32,
    pub pane: u32,
    pub command: String,
    pub in_mode: bool,
    pub dead: bool,
    pub agent: String,
    pub session_id: String,
    pub claim: String,
    pub typed: bool,
    pub transcript: String,
    pub cwd: String,
    pub config_dir: String,
    pub path: String,
    pub session: String,
}

/// The fields `LivePane` reads; free text (path, session name) last.
pub const PANE_FMT: &str = "#{pane_id}\x1f#{window_index}\x1f#{pane_index}\x1f#{pane_current_command}\x1f#{pane_in_mode}\x1f#{pane_dead}\x1f#{@home_agent}\x1f#{@home_session_id}\x1f#{@home_restore}\x1f#{@home_restore_typed}\x1f#{@home_transcript}\x1f#{@home_cwd}\x1f#{@home_config_dir}\x1f#{pane_current_path}\x1f#{session_name}\x1e";

/// Parses `list-panes -F PANE_FMT` output; malformed records are dropped.
pub fn parse_panes(out: &str) -> Vec<LivePane> {
    out.split('\x1e')
        .map(|r| r.trim_start_matches('\n'))
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let f: Vec<&str> = r.splitn(15, '\x1f').collect();
            if f.len() != 15 {
                return None;
            }
            Some(LivePane {
                id: f[0].into(),
                window: f[1].parse().ok()?,
                pane: f[2].parse().ok()?,
                command: f[3].into(),
                in_mode: f[4] == "1",
                dead: f[5] == "1",
                agent: f[6].into(),
                session_id: f[7].into(),
                claim: f[8].into(),
                typed: !f[9].is_empty(),
                transcript: f[10].into(),
                cwd: f[11].into(),
                config_dir: f[12].into(),
                path: f[13].into(),
                session: f[14].into(),
            })
        })
        .collect()
}

/// The entries to save from the live panes. A pane is saved when it runs
/// Claude (`looks_alive`) and its hooks have recorded a session with a
/// transcript. A pane `restore-agents` claimed whose claude hasn't
/// recorded its session yet (still waiting for its turn, or sitting at
/// the resume dialog) keeps its `prev` entry, at its current location,
/// so a save during the restore loses nothing.
pub fn snapshot_entries(panes: &[LivePane], prev: &Snapshot) -> Vec<Entry> {
    let mut out = Vec::new();
    for p in panes.iter().filter(|p| !p.dead) {
        let alive = AgentKind::Claude.looks_alive(&p.command);
        if alive && p.agent == "claude" && !p.session_id.is_empty() && !p.transcript.is_empty() {
            out.push(Entry {
                session: p.session.clone(),
                window: p.window,
                pane: p.pane,
                path: p.path.clone(),
                session_id: p.session_id.clone(),
                transcript: p.transcript.clone(),
                cwd: if p.cwd.is_empty() {
                    p.path.clone()
                } else {
                    p.cwd.clone()
                },
                config_dir: (!p.config_dir.is_empty()).then(|| p.config_dir.clone()),
            });
        } else if !p.claim.is_empty()
            && p.session_id.is_empty()
            && (alive || (!p.typed && restorable_shell(&p.command)))
            && let Some(e) = prev.entries.iter().find(|e| e.session_id == p.claim)
        {
            out.push(Entry {
                session: p.session.clone(),
                window: p.window,
                pane: p.pane,
                // where the pane is now is what resurrect saves with it
                path: p.path.clone(),
                ..e.clone()
            });
        }
    }
    out
}

/// Shells the resume command is written for: POSIX shells and fish (≥ 3,
/// for `&&`). Login shells show as `-zsh`.
pub fn restorable_shell(cmd: &str) -> bool {
    let c = cmd.trim_start_matches('-');
    let c = c.rsplit('/').next().unwrap_or(c);
    matches!(
        c,
        "fish" | "bash" | "zsh" | "sh" | "dash" | "ksh" | "mksh" | "ash" | "yash"
    )
}

/// `s` single-quoted for both POSIX shells and fish. Fish reads `\\` and
/// `\'` as escapes inside single quotes, POSIX shells don't; outside
/// quotes both read them as a backslash and a quote. So every `'` and `\`
/// is written outside the quotes, escaped.
pub fn quote(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('\'');
    for c in s.chars() {
        match c {
            '\'' => q.push_str("'\\''"),
            '\\' => q.push_str("'\\\\'"),
            c => q.push(c),
        }
    }
    q.push('\'');
    q
}

/// The line typed into the pane: `cd <cwd> && env <account> claude
/// --resume <id>`. `env` runs the `claude` on `PATH`, bypassing any shell
/// function of that name (one that pins an account, say), and sets or
/// clears `CLAUDE_CONFIG_DIR` to the recorded account. Values that can't
/// be typed safely (control characters, a relative cwd, an id that isn't
/// `[A-Za-z0-9_-]`) are refused.
pub fn command(e: &Entry) -> Result<String, String> {
    let id = &e.session_id;
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("unusable session id {id:?}"));
    }
    if !e.cwd.starts_with('/') {
        return Err(format!("cwd {:?} is not absolute", e.cwd));
    }
    let dir = e.config_dir.as_deref().unwrap_or("");
    if [e.cwd.as_str(), dir]
        .iter()
        .any(|v| v.chars().any(char::is_control))
    {
        return Err("control character in a path".into());
    }
    let account = match &e.config_dir {
        Some(d) => format!("CLAUDE_CONFIG_DIR={}", quote(d)),
        None => "-u CLAUDE_CONFIG_DIR".into(),
    };
    Ok(format!(
        "cd {} && env {account} claude --resume {id}",
        quote(&e.cwd)
    ))
}

/// Whether `e` can be resumed in pane `p` now: the pane is a live shell
/// prompt (not in copy mode) in the saved directory, and the transcript
/// and launch directory still exist (`exists`). The resume command, or
/// why not. Claims are checked by the callers.
pub fn check(
    e: &Entry,
    p: Option<&LivePane>,
    exists: impl Fn(&Path) -> bool,
) -> Result<String, String> {
    let Some(p) = p else {
        return Err("no such pane".into());
    };
    if p.dead {
        return Err("pane is dead".into());
    }
    if p.in_mode {
        return Err("pane is in a mode".into());
    }
    if !restorable_shell(&p.command) {
        return Err(format!("pane runs {:?}, not a shell", p.command));
    }
    if p.path != e.path {
        return Err(format!("pane is in {:?}, saved in {:?}", p.path, e.path));
    }
    if !exists(Path::new(&e.transcript)) {
        return Err(format!("transcript gone: {}", e.transcript));
    }
    if !exists(Path::new(&e.cwd)) {
        return Err(format!("directory gone: {}", e.cwd));
    }
    command(e)
}

/// The live pane at `e`'s location.
pub fn find<'a>(panes: &'a [LivePane], e: &Entry) -> Option<&'a LivePane> {
    panes
        .iter()
        .find(|p| p.session == e.session && p.window == e.window && p.pane == e.pane)
}

/// What `restore-agents` claims: for each entry, its pane and command, or
/// why it is skipped. A pane already claimed or typed into is never
/// claimed again.
pub fn plan<'a>(
    snap: &'a Snapshot,
    panes: &'a [LivePane],
    exists: impl Fn(&Path) -> bool,
) -> Vec<(&'a Entry, Result<&'a LivePane, String>)> {
    snap.entries
        .iter()
        .map(|e| {
            let p = find(panes, e);
            let r = match p {
                Some(p) if !p.claim.is_empty() || p.typed => Err("already restored".to_string()),
                _ => check(e, p, &exists).map(|_| p.expect("checked")),
            };
            (e, r)
        })
        .collect()
}

/// `@home-restore-agents-delay`, in seconds: a non-negative number, else
/// the default.
pub fn parse_delay(v: &str) -> Duration {
    let s = v
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|d| d.is_finite() && *d >= 0.0 && *d <= 600.0)
        .unwrap_or(DEFAULT_DELAY);
    Duration::from_secs_f64(s)
}

fn log(socket: &Path, msg: &str) {
    crate::hook::append_log(socket, LOG, msg);
}

fn snapshot_path(socket: &Path) -> anyhow::Result<PathBuf> {
    Ok(crate::paths::Paths::for_socket(socket)?
        .state_dir
        .join(SNAPSHOT))
}

/// The saved snapshot (empty when there is none or it can't be read).
pub fn load(socket: &Path) -> anyhow::Result<Snapshot> {
    let path = snapshot_path(socket)?;
    match std::fs::read(&path) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot::default()),
        Err(e) => Err(e.into()),
    }
}

fn list_panes(t: &Tmux) -> anyhow::Result<Vec<LivePane>> {
    Ok(parse_panes(&t.run(&[
        "list-panes",
        "-a",
        "-F",
        PANE_FMT,
    ])?))
}

fn global(t: &Tmux, name: &str) -> String {
    t.run(&["show-options", "-gqv", name])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// `tmux-home agents-snapshot`: saves the agent panes under their
/// locations, atomically (temp file + rename; a concurrent save just
/// wins or loses whole). Errors go to the log; the exit is always 0.
pub fn snapshot_cli(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let Some(socket) = crate::client::current_socket(socket) else {
        eprintln!("tmux-home: not inside tmux (pass --socket)");
        return Ok(());
    };
    if let Err(e) = save(&Tmux::new(socket.clone()), &socket) {
        log(&socket, &format!("snapshot: {e:#}"));
    }
    Ok(())
}

pub fn save(t: &Tmux, socket: &Path) -> anyhow::Result<usize> {
    use std::io::Write;
    let panes = list_panes(t)?;
    let prev = load(socket).unwrap_or_default();
    let snap = Snapshot {
        saved: crate::agent::now(),
        entries: snapshot_entries(&panes, &prev),
    };
    let path = snapshot_path(socket)?;
    let dir = path.parent().expect("state dir");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let tmp = dir.join(format!("{SNAPSHOT}.{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(&snap)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(snap.entries.len())
}

/// `tmux-home restore-agents [--worker]`. Always exits 0.
pub fn restore_cli(socket: Option<PathBuf>, worker: bool) -> anyhow::Result<()> {
    let Some(socket) = crate::client::current_socket(socket) else {
        eprintln!("tmux-home: not inside tmux (pass --socket)");
        return Ok(());
    };
    let t = Tmux::new(socket.clone());
    let r = if worker {
        run_worker(&t, &socket)
    } else {
        claim(&t, &socket)
    };
    if let Err(e) = r {
        log(&socket, &format!("restore: {e:#}"));
    }
    Ok(())
}

/// The foreground half, quick: once per server and only when enabled,
/// claims the panes to resume and starts the worker that types into them.
fn claim(t: &Tmux, socket: &Path) -> anyhow::Result<()> {
    if global(t, OPT_ON) != "on" {
        return Ok(());
    }
    if !global(t, GUARD).is_empty() {
        log(socket, "restore: already ran on this server");
        return Ok(());
    }
    t.run(&["set-option", "-g", GUARD, &crate::agent::now().to_string()])?;
    let snap = load(socket)?;
    let panes = list_panes(t)?;
    let mut claimed = 0;
    for (e, r) in plan(&snap, &panes, Path::exists) {
        match r {
            Ok(p) => {
                t.run(&["set-option", "-p", "-t", &p.id, CLAIM, &e.session_id])?;
                claimed += 1;
            }
            Err(why) => log(
                socket,
                &format!("skip {} {}: {why}", e.location(), e.session_id),
            ),
        }
    }
    log(
        socket,
        &format!("restore: {claimed} of {} saved agents", snap.entries.len()),
    );
    if claimed > 0 {
        spawn_worker(socket)?;
    }
    Ok(())
}

fn spawn_worker(socket: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let bin = std::env::var_os("TMUX_HOME_BIN")
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)?;
    std::process::Command::new(bin)
        .args(["restore-agents", "--worker", "--socket"])
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// The background half: for each claimed pane, in snapshot order, waits
/// the delay, checks the pane again (still a shell prompt where it was)
/// and types the command; a pane that has moved on loses its claim.
fn run_worker(t: &Tmux, socket: &Path) -> anyhow::Result<()> {
    let delay = parse_delay(&global(t, OPT_DELAY));
    let snap = load(socket)?;
    for e in &snap.entries {
        let ours = |p: &LivePane| p.claim == e.session_id && !p.typed;
        if !list_panes(t)?.iter().any(ours) {
            continue;
        }
        std::thread::sleep(delay);
        let panes = list_panes(t)?;
        let Some(p) = panes.iter().find(|p| ours(p)) else {
            continue;
        };
        match check(e, Some(p), Path::exists) {
            Ok(cmd) => {
                t.run(&[
                    "set-option",
                    "-p",
                    "-t",
                    &p.id,
                    TYPED,
                    "1",
                    ";",
                    "send-keys",
                    "-t",
                    &p.id,
                    "-l",
                    &cmd,
                    ";",
                    "send-keys",
                    "-t",
                    &p.id,
                    "Enter",
                ])?;
                log(
                    socket,
                    &format!("resumed {} {} in {}", e.location(), e.session_id, p.id),
                );
            }
            Err(why) => {
                let _ = t.run(&["set-option", "-p", "-u", "-t", &p.id, CLAIM]);
                log(
                    socket,
                    &format!("skip {} {}: {why}", e.location(), e.session_id),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            session: "main".into(),
            window: 1,
            pane: 0,
            path: "/w/api".into(),
            session_id: "00000000-0000-4000-8000-000000000001".into(),
            transcript: "/h/.claude/projects/-w-api/00000000-0000-4000-8000-000000000001.jsonl"
                .into(),
            cwd: "/w/api".into(),
            config_dir: None,
        }
    }

    fn shell(id: &str, session: &str, window: u32, pane: u32, path: &str) -> LivePane {
        LivePane {
            id: id.into(),
            session: session.into(),
            window,
            pane,
            command: "fish".into(),
            path: path.into(),
            ..LivePane::default()
        }
    }

    fn agent_pane(id: &str, sid: &str) -> LivePane {
        LivePane {
            command: "2.1.295".into(),
            agent: "claude".into(),
            session_id: sid.into(),
            transcript: format!("/t/{sid}.jsonl"),
            cwd: "/w/api".into(),
            config_dir: "/h/.claude-work".into(),
            ..shell(id, "main", 1, 0, "/w/api/sub")
        }
    }

    #[test]
    fn parses_list_panes() {
        let out = "%3\x1f1\x1f0\x1f2.1.295\x1f0\x1f0\x1fclaude\x1fsid\x1f\x1f\x1f/t/sid.jsonl\x1f/w\x1f/h/.c\x1f/w/x\x1fmain\x1e\n%4\x1f2\x1f1\x1ffish\x1f1\x1f0\x1f\x1f\x1fsid2\x1f1\x1f\x1f\x1f\x1f/w\x1fmy session\x1e\nbroken\x1e\n";
        let p = parse_panes(out);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].id, "%3");
        assert_eq!((p[0].window, p[0].pane), (1, 0));
        assert_eq!(p[0].command, "2.1.295");
        assert_eq!(p[0].transcript, "/t/sid.jsonl");
        assert_eq!(p[0].config_dir, "/h/.c");
        assert_eq!(p[0].path, "/w/x");
        assert!(!p[0].typed && p[0].claim.is_empty());
        assert_eq!(p[1].session, "my session");
        assert!(p[1].in_mode && p[1].typed);
        assert_eq!(p[1].claim, "sid2");
    }

    #[test]
    fn snapshot_saves_running_claude_panes_only() {
        let panes = [
            agent_pane("%1", "a"),
            // claude exited (or crashed) back to the shell: not resumed
            LivePane {
                command: "fish".into(),
                ..agent_pane("%2", "b")
            },
            // something else in the pane now (SessionEnd missed)
            LivePane {
                command: "nvim".into(),
                ..agent_pane("%3", "c")
            },
            // hooks predating pass 7: no transcript recorded
            LivePane {
                transcript: String::new(),
                ..agent_pane("%4", "d")
            },
            LivePane {
                dead: true,
                ..agent_pane("%5", "e")
            },
            shell("%6", "main", 2, 0, "/w"),
        ];
        let e = snapshot_entries(&panes, &Snapshot::default());
        assert_eq!(e.len(), 1);
        assert_eq!(
            e[0],
            Entry {
                session: "main".into(),
                window: 1,
                pane: 0,
                path: "/w/api/sub".into(),
                session_id: "a".into(),
                transcript: "/t/a.jsonl".into(),
                cwd: "/w/api".into(),
                config_dir: Some("/h/.claude-work".into()),
            }
        );
        // no cwd recorded: the pane's directory; no account: None
        let p = LivePane {
            cwd: String::new(),
            config_dir: String::new(),
            ..agent_pane("%1", "a")
        };
        let e = snapshot_entries(&[p], &Snapshot::default());
        assert_eq!(
            (e[0].cwd.as_str(), e[0].config_dir.clone()),
            ("/w/api/sub", None)
        );
    }

    #[test]
    fn snapshot_keeps_claimed_panes_until_their_claude_records_itself() {
        let prev = Snapshot {
            saved: 1,
            entries: vec![entry()],
        };
        let id = entry().session_id;
        let claimed = |command: &str, typed: bool| LivePane {
            command: command.into(),
            claim: id.clone(),
            typed,
            ..shell("%9", "main", 3, 1, "/w/api")
        };
        // waiting for its turn
        let e = snapshot_entries(&[claimed("fish", false)], &prev);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].window, e[0].pane), (3, 1), "at its current location");
        assert_eq!(e[0].session_id, id);
        // claude started, its first hook not yet in
        assert_eq!(
            snapshot_entries(&[claimed("2.1.295", true)], &prev).len(),
            1
        );
        // typed, and back at the shell: the resume failed or was exited
        assert!(snapshot_entries(&[claimed("fish", true)], &prev).is_empty());
        // once claude has recorded a session, that is what is saved
        let live = LivePane {
            claim: id.clone(),
            typed: true,
            ..agent_pane("%9", "new")
        };
        let e = snapshot_entries(&[live], &prev);
        assert_eq!(e[0].session_id, "new");
    }

    #[test]
    fn shells() {
        for s in ["fish", "-zsh", "bash", "/bin/sh", "dash"] {
            assert!(restorable_shell(s), "{s}");
        }
        for s in ["nu", "tcsh", "nvim", "2.1.295", "node", ""] {
            assert!(!restorable_shell(s), "{s}");
        }
    }

    #[test]
    fn command_line() {
        let e = entry();
        assert_eq!(
            command(&e).unwrap(),
            "cd '/w/api' && env -u CLAUDE_CONFIG_DIR claude --resume 00000000-0000-4000-8000-000000000001"
        );
        let e = Entry {
            config_dir: Some("/h/.claude-work".into()),
            cwd: "/w/it's".into(),
            ..entry()
        };
        assert_eq!(
            command(&e).unwrap(),
            "cd '/w/it'\\''s' && env CLAUDE_CONFIG_DIR='/h/.claude-work' claude --resume 00000000-0000-4000-8000-000000000001"
        );
        for bad in [
            Entry {
                session_id: "x; rm -rf ~".into(),
                ..entry()
            },
            Entry {
                session_id: String::new(),
                ..entry()
            },
            Entry {
                cwd: "rel".into(),
                ..entry()
            },
            Entry {
                cwd: "/a\nb".into(),
                ..entry()
            },
            Entry {
                config_dir: Some("/a\x1bb".into()),
                ..entry()
            },
        ] {
            assert!(command(&bad).is_err(), "{bad:?}");
        }
    }

    /// The quoting round-trips through each shell that is installed.
    #[test]
    fn quote_round_trips_in_real_shells() {
        let nasty = r#"/a b/it's/\back\\slash/$HOME/`x`/"q"/~/*/#/;&|/ü"#;
        for sh in ["fish", "bash", "zsh", "sh", "dash"] {
            let Ok(out) = std::process::Command::new(sh)
                .arg("-c")
                .arg(format!("printf %s {}", quote(nasty)))
                .output()
            else {
                continue; // not installed
            };
            assert!(out.status.success(), "{sh}: {:?}", out);
            assert_eq!(String::from_utf8_lossy(&out.stdout), nasty, "{sh}");
        }
    }

    #[test]
    fn check_rules() {
        let e = entry();
        let ok = shell("%1", "main", 1, 0, "/w/api");
        let all = |_: &Path| true;
        assert!(check(&e, Some(&ok), all).is_ok());
        assert_eq!(check(&e, None, all).unwrap_err(), "no such pane");
        let cases: [(LivePane, &str); 4] = [
            (
                LivePane {
                    command: "nvim".into(),
                    ..ok.clone()
                },
                "not a shell",
            ),
            (
                LivePane {
                    in_mode: true,
                    ..ok.clone()
                },
                "in a mode",
            ),
            (
                LivePane {
                    dead: true,
                    ..ok.clone()
                },
                "dead",
            ),
            (
                LivePane {
                    path: "/Users/x".into(),
                    ..ok.clone()
                },
                "saved in",
            ),
        ];
        for (p, why) in cases {
            let err = check(&e, Some(&p), all).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        let no_transcript = |p: &Path| !p.to_string_lossy().ends_with(".jsonl");
        assert!(
            check(&e, Some(&ok), no_transcript)
                .unwrap_err()
                .contains("transcript gone")
        );
        let no_dir = |p: &Path| p != Path::new("/w/api");
        assert!(
            check(&e, Some(&ok), no_dir)
                .unwrap_err()
                .contains("directory gone")
        );
    }

    #[test]
    fn plan_never_claims_twice() {
        let snap = Snapshot {
            saved: 1,
            entries: vec![
                entry(),
                Entry {
                    window: 2,
                    session_id: "b".into(),
                    ..entry()
                },
                Entry {
                    window: 3,
                    session_id: "c".into(),
                    ..entry()
                },
            ],
        };
        let panes = [
            shell("%1", "main", 1, 0, "/w/api"),
            LivePane {
                claim: "b".into(),
                ..shell("%2", "main", 2, 0, "/w/api")
            },
            LivePane {
                typed: true,
                ..shell("%3", "main", 3, 0, "/w/api")
            },
        ];
        let p = plan(&snap, &panes, |_| true);
        assert_eq!(p[0].1.as_ref().unwrap().id, "%1");
        assert_eq!(p[1].1.as_ref().unwrap_err(), "already restored");
        assert_eq!(p[2].1.as_ref().unwrap_err(), "already restored");
    }

    #[test]
    fn delay_option() {
        assert_eq!(parse_delay(""), Duration::from_secs(2));
        assert_eq!(parse_delay("0.5"), Duration::from_millis(500));
        assert_eq!(parse_delay("0"), Duration::ZERO);
        for bad in ["-1", "x", "NaN", "inf", "1e9"] {
            assert_eq!(parse_delay(bad), Duration::from_secs(2), "{bad}");
        }
    }

    #[test]
    fn snapshot_file_round_trips() {
        let s = Snapshot {
            saved: 7,
            entries: vec![entry()],
        };
        let back: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
