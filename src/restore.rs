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
//!   resurrect restores by `session:window.pane`, so each pane running
//!   Claude at that moment (`list-panes`) is saved under that location to
//!   `<state_dir>/agents.json`. Only panes that run Claude are saved: a
//!   session the user exited, or that crashed, is not resumed whatever its
//!   last hook said. Only a server that is done restoring
//!   (`@home_restore_done`, set by the post-restore hook, or by the config
//!   when there was nothing to restore) is saved: until then its panes
//!   aren't the user's sessions yet, and a save would overwrite the
//!   snapshot the restore needs.
//! - **Restore** (`tmux-home restore-agents`, from
//!   `@resurrect-hook-post-restore-all`). Once per server (a run that
//!   claims nothing doesn't count, so a later restore can still try): each saved
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
/// Server option: `restore-agents` has claimed panes on this server (Unix
/// seconds).
pub const GUARD: &str = "@home_agents_restored";
/// Server option: the server is done restoring (or had nothing to
/// restore), so its panes are the user's: set by the config, in
/// resurrect's post-restore hook. `agents-snapshot` saves only then.
pub const RESTORE_DONE: &str = "@home_restore_done";
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
    /// The name given to the pane in the popup (`@home_pane_name`),
    /// given back to the pane it is restored into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_name: Option<String>,
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
    /// `pane_pid`.
    pub pid: u32,
    /// A Claude Code process runs under the pane (`agent::procs`; set by
    /// `list_panes`): a wrapper can leave it under the pane's shell.
    pub agent_proc: bool,
    pub pane_name: String,
    pub path: String,
    pub session: String,
}

/// The fields `LivePane` reads; free text (path, session name) last.
pub const PANE_FMT: &str = "#{pane_id}\x1f#{window_index}\x1f#{pane_index}\x1f#{pane_current_command}\x1f#{pane_in_mode}\x1f#{pane_dead}\x1f#{@home_agent}\x1f#{@home_session_id}\x1f#{@home_restore}\x1f#{@home_restore_typed}\x1f#{@home_transcript}\x1f#{@home_cwd}\x1f#{@home_config_dir}\x1f#{pane_pid}\x1f#{@home_pane_name}\x1f#{pane_current_path}\x1f#{session_name}\x1e";

/// Parses `list-panes -F PANE_FMT` output; malformed records are dropped.
pub fn parse_panes(out: &str) -> Vec<LivePane> {
    out.split('\x1e')
        .map(|r| r.trim_start_matches('\n'))
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let f: Vec<&str> = r.splitn(17, '\x1f').collect();
            if f.len() != 17 {
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
                pid: f[13].parse().unwrap_or(0),
                agent_proc: false,
                pane_name: f[14].into(),
                path: f[15].into(),
                session: f[16].into(),
            })
        })
        .collect()
}

/// A session forked from another, as a process shows it: Claude Code's
/// backgrounding re-hosts the conversation under a new id in a detached
/// process (no `TMUX_PANE`, so no hooks reach the pane), started as
/// `claude --session-id <new> --fork-session --resume <dir>/<parent>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fork {
    pub parent: String,
    pub new: String,
    /// Seconds since the process started: the smallest is the newest.
    pub age: u64,
}

/// How many forks of forks `follow_fork` chases.
const MAX_FORK_DEPTH: usize = 8;

fn session_id_like(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `ps` elapsed time, `[[dd-]hh:]mm:ss`, in seconds.
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = s
        .split_once('-')
        .map_or((0, s), |(d, r)| (d.parse().unwrap_or(0), r));
    rest.split(':')
        .try_fold(0u64, |t, f| Some(t * 60 + f.parse::<u64>().ok()?))
        .map(|t| t + days * 86400)
}

/// The forks in `ps -axww -o etime=,args=` output: lines whose args have
/// `--fork-session`, `--session-id <new>` and `--resume <parent>` (a bare
/// id, or a transcript path ending in `<parent>.jsonl`). Args are split
/// on whitespace, so a path with spaces is not matched.
pub fn parse_forks(out: &str) -> Vec<Fork> {
    out.lines()
        .filter_map(|l| {
            let (etime, args) = l.trim_start().split_once(char::is_whitespace)?;
            let mut t = args.split_whitespace();
            let (mut forked, mut new, mut from) = (false, None, None);
            while let Some(a) = t.next() {
                match a {
                    "--fork-session" => forked = true,
                    "--session-id" => new = t.next(),
                    "--resume" => from = t.next(),
                    _ => {}
                }
            }
            let from = from?.rsplit('/').next()?;
            let parent = from.strip_suffix(".jsonl").unwrap_or(from);
            let new = new?;
            (forked && session_id_like(parent) && session_id_like(new)).then(|| Fork {
                parent: parent.into(),
                new: new.into(),
                age: parse_etime(etime).unwrap_or(u64::MAX),
            })
        })
        .collect()
}

/// The fork processes now running; empty when `ps` can't be run.
fn read_forks() -> Vec<Fork> {
    std::process::Command::new("ps")
        .args(["-axww", "-o", "etime=,args="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_forks(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Where a session recorded as `id` (transcript `transcript`) has gone by
/// forking: the newest fork of it, then of that, up to `MAX_FORK_DEPTH`;
/// each hop needs its transcript beside the recorded one (`exists`). `None`
/// when it was never forked.
pub fn follow_fork(
    id: &str,
    transcript: &str,
    forks: &[Fork],
    exists: impl Fn(&Path) -> bool,
) -> Option<(String, String)> {
    let dir = Path::new(transcript).parent()?;
    let mut cur = (id.to_string(), transcript.to_string());
    let mut seen = vec![id.to_string()];
    for _ in 0..MAX_FORK_DEPTH {
        let next = forks
            .iter()
            .filter(|f| f.parent == cur.0 && !seen.contains(&f.new))
            .filter(|f| exists(&dir.join(format!("{}.jsonl", f.new))))
            .min_by_key(|f| f.age);
        let Some(f) = next else { break };
        seen.push(f.new.clone());
        cur = (
            f.new.clone(),
            dir.join(format!("{}.jsonl", f.new))
                .to_string_lossy()
                .into_owned(),
        );
    }
    (cur.0 != id).then_some(cur)
}

/// Points every Claude pane whose session was forked at the fork; returns
/// the panes changed. Cwd and account stay: the fork runs in both.
pub fn adopt_forks(
    panes: &mut [LivePane],
    forks: &[Fork],
    exists: impl Fn(&Path) -> bool,
) -> Vec<usize> {
    let mut changed = Vec::new();
    for (i, p) in panes.iter_mut().enumerate() {
        if p.agent != "claude" || p.session_id.is_empty() || p.transcript.is_empty() {
            continue;
        }
        if let Some((id, transcript)) = follow_fork(&p.session_id, &p.transcript, forks, &exists) {
            p.session_id = id;
            p.transcript = transcript;
            changed.push(i);
        }
    }
    changed
}

/// The entries to save from the live panes. A pane is saved when it runs
/// Claude (a Claude Code process under it, or `looks_alive`) and its hooks have recorded a session with a
/// transcript. A pane `restore-agents` claimed whose claude hasn't
/// recorded its session yet (still waiting for its turn, or sitting at
/// the resume dialog) keeps its `prev` entry, at its current location,
/// so a save during the restore loses nothing.
pub fn snapshot_entries(panes: &[LivePane], prev: &Snapshot) -> Vec<Entry> {
    let mut out = Vec::new();
    for p in panes.iter().filter(|p| !p.dead) {
        let alive = p.agent_proc || AgentKind::Claude.looks_alive(&p.command);
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
                pane_name: (!p.pane_name.is_empty()).then(|| p.pane_name.clone()),
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

/// Whether `e` can be resumed in pane `p`: the pane is live, in the saved
/// directory, and the transcript and launch directory still exist
/// (`exists`); when `typing`, also at a shell prompt and not in copy mode.
/// (At claim time, right after the restore, a pane may still be running
/// resurrect's `cat` of its saved contents before it execs the shell; the
/// worker checks again just before it types.) The resume command, or why
/// not. Claims are checked by the callers.
pub fn check(
    e: &Entry,
    p: Option<&LivePane>,
    typing: bool,
    exists: impl Fn(&Path) -> bool,
) -> Result<String, String> {
    let Some(p) = p else {
        return Err("no such pane".into());
    };
    if p.dead {
        return Err("pane is dead".into());
    }
    if typing && p.in_mode {
        return Err("pane is in a mode".into());
    }
    if typing && !restorable_shell(&p.command) {
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
                _ => check(e, p, false, &exists).map(|_| p.expect("checked")),
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
    let mut panes = parse_panes(&t.run(&["list-panes", "-a", "-F", PANE_FMT])?);
    if let Some(procs) = crate::agent::procs::ProcTable::read() {
        for p in panes.iter_mut().filter(|p| p.pid != 0) {
            p.agent_proc = procs.agent_under(p.pid).is_some();
        }
    }
    Ok(panes)
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

/// Writes the snapshot; `None`, leaving the old one, when the server isn't
/// done restoring (`RESTORE_DONE` unset).
pub fn save(t: &Tmux, socket: &Path) -> anyhow::Result<Option<usize>> {
    use std::io::Write;
    if global(t, RESTORE_DONE).is_empty() {
        log(socket, "snapshot: skip save: server not restored yet");
        return Ok(None);
    }
    let mut panes = list_panes(t)?;
    // follow backgrounded sessions to their forks, in the pane's options
    // too, so the sidebar and later saves agree
    let forks = read_forks();
    for i in adopt_forks(&mut panes, &forks, Path::exists) {
        let p = &panes[i];
        log(
            socket,
            &format!("snapshot: {} follows a fork to {}", p.id, p.session_id),
        );
        for (opt, v) in [
            ("@home_session_id", &p.session_id),
            ("@home_transcript", &p.transcript),
        ] {
            t.run(&["set-option", "-p", "-t", &p.id, opt, v])?;
        }
    }
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
    Ok(Some(snap.entries.len()))
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
/// The server counts as done (`GUARD`) only once a pane is claimed: a run
/// that finds nothing to resume (no snapshot, an unreadable one, every
/// pane skipped) leaves a later restore (`prefix C-r`) free to try.
fn claim(t: &Tmux, socket: &Path) -> anyhow::Result<()> {
    if global(t, OPT_ON) != "on" {
        return Ok(());
    }
    if !global(t, GUARD).is_empty() {
        log(socket, "restore: already ran on this server");
        return Ok(());
    }
    let snap = load(socket)?;
    let panes = list_panes(t)?;
    let mut claimed = 0;
    for (e, r) in plan(&snap, &panes, Path::exists) {
        match r {
            Ok(p) => {
                t.run(&["set-option", "-p", "-t", &p.id, CLAIM, &e.session_id])?;
                if let Some(n) = e.pane_name.as_deref().filter(|n| !n.is_empty()) {
                    t.run(&["set-option", "-p", "-t", &p.id, crate::ops::PANE_NAME, n])?;
                }
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
        t.run(&["set-option", "-g", GUARD, &crate::agent::now().to_string()])?;
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

/// Workers take turns: an exclusive lock on `restore.lock` beside the
/// snapshot, held until the worker is done. Two restores close together
/// can both claim a pane (each sees it unclaimed before the other marks
/// it); taking turns, the later worker finds it already typed into
/// (`TYPED`, set in the same tmux command as the keys) and skips it.
fn worker_lock(socket: &Path) -> anyhow::Result<std::fs::File> {
    let path = snapshot_path(socket)?.with_file_name("restore.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    f.lock()?;
    Ok(f)
}

/// The background half: for each claimed pane, in snapshot order, waits
/// the delay, checks the pane again (still a shell prompt where it was)
/// and types the command; a pane that has moved on loses its claim.
fn run_worker(t: &Tmux, socket: &Path) -> anyhow::Result<()> {
    let _turn = worker_lock(socket)?;
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
        match check(e, Some(p), true, Path::exists) {
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
            pane_name: None,
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
        let out = "%3\x1f1\x1f0\x1f2.1.295\x1f0\x1f0\x1fclaude\x1fsid\x1f\x1f\x1f/t/sid.jsonl\x1f/w\x1f/h/.c\x1f42\x1fapi\x1f/w/x\x1fmain\x1e\n%4\x1f2\x1f1\x1ffish\x1f1\x1f0\x1f\x1f\x1fsid2\x1f1\x1f\x1f\x1f\x1f\x1f\x1f/w\x1fmy session\x1e\nbroken\x1e\n";
        let p = parse_panes(out);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].id, "%3");
        assert_eq!((p[0].window, p[0].pane), (1, 0));
        assert_eq!(p[0].command, "2.1.295");
        assert_eq!(p[0].transcript, "/t/sid.jsonl");
        assert_eq!(p[0].config_dir, "/h/.c");
        assert_eq!(p[0].path, "/w/x");
        assert_eq!(
            (p[0].pane_name.as_str(), p[1].pane_name.as_str()),
            ("api", "")
        );
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
                pane_name: None,
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
        // a wrapper left claude under the shell (Kimi's launcher): the
        // process under the pane says it runs
        let p = LivePane {
            command: "fish".into(),
            agent_proc: true,
            pane_name: "api work".into(),
            ..agent_pane("%1", "k")
        };
        let e = snapshot_entries(&[p], &Snapshot::default());
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].session_id, "k");
        assert_eq!(e[0].pane_name.as_deref(), Some("api work"), "its name kept");
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

    const FORK_PS: &str = "   05:12 /x/versions/2.1.310 --session-id 200e3f73-f8f9-4d97-862a-0a4e7e2b48d2 --fork-session --resume /h/.claude/projects/-p/a0dcc65b-182c-4139-bc09-9d2aa83520c5.jsonl --reply-on-resume
 1-02:03:04 claude --bg-pty-host /tmp/cc-daemon-501/x/pty/200e3f73.sock
       00:09 claude --session-id bbb --fork-session --resume a0dcc65b-182c-4139-bc09-9d2aa83520c5
       00:01 claude --session-id ccc --resume a0dcc65b-182c-4139-bc09-9d2aa83520c5
";

    fn fork(parent: &str, new: &str, age: u64) -> Fork {
        Fork {
            parent: parent.into(),
            new: new.into(),
            age,
        }
    }

    #[test]
    fn forks_are_read_from_process_args() {
        let f = parse_forks(FORK_PS);
        assert_eq!(
            f,
            [
                fork(
                    "a0dcc65b-182c-4139-bc09-9d2aa83520c5",
                    "200e3f73-f8f9-4d97-862a-0a4e7e2b48d2",
                    312
                ),
                fork("a0dcc65b-182c-4139-bc09-9d2aa83520c5", "bbb", 9),
            ]
        );
        assert_eq!(parse_etime("1-02:03:04"), Some(93784));
        assert_eq!(parse_etime("12:34"), Some(754));
        assert_eq!(parse_etime("x"), None);
        assert!(parse_forks("1:00 claude --fork-session --resume\n").is_empty());
    }

    #[test]
    fn a_fork_is_followed_when_its_transcript_exists() {
        let all = |_: &Path| true;
        let forks = [
            fork("a", "b", 50),
            fork("a", "b2", 10),
            fork("b2", "c", 5),
            fork("c", "a", 1),
        ];
        // newest fork, then its fork; a cycle back to the start stops
        assert_eq!(
            follow_fork("a", "/t/a.jsonl", &forks, all),
            Some(("c".into(), "/t/c.jsonl".into()))
        );
        // a hop without a transcript beside the recorded one isn't taken
        let no_c = |p: &Path| !p.ends_with("c.jsonl");
        assert_eq!(
            follow_fork("a", "/t/a.jsonl", &forks, no_c),
            Some(("b2".into(), "/t/b2.jsonl".into()))
        );
        assert_eq!(follow_fork("a", "/t/a.jsonl", &forks, |_| false), None);
        assert_eq!(follow_fork("z", "/t/z.jsonl", &forks, all), None);
        // depth cap
        let chain: Vec<Fork> = (0..20)
            .map(|i| fork(&format!("s{i}"), &format!("s{}", i + 1), 1))
            .collect();
        let (id, _) = follow_fork("s0", "/t/s0.jsonl", &chain, all).unwrap();
        assert_eq!(id, format!("s{MAX_FORK_DEPTH}"));
    }

    #[test]
    fn a_forked_pane_is_saved_as_its_fork() {
        let mut panes = [agent_pane("%1", "a"), agent_pane("%2", "other")];
        let forks = [fork("a", "b", 1)];
        assert_eq!(adopt_forks(&mut panes, &forks, |_| true), [0]);
        let e = snapshot_entries(&panes, &Snapshot::default());
        assert_eq!(
            (e[0].session_id.as_str(), e[0].transcript.as_str()),
            ("b", "/t/b.jsonl")
        );
        assert_eq!(e[0].cwd, "/w/api", "cwd kept");
        assert_eq!(
            e[0].config_dir.as_deref(),
            Some("/h/.claude-work"),
            "account kept"
        );
        assert_eq!(e[1].session_id, "other");
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
        assert!(check(&e, Some(&ok), true, all).is_ok());
        assert_eq!(check(&e, None, true, all).unwrap_err(), "no such pane");
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
            let err = check(&e, Some(&p), true, all).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        let no_transcript = |p: &Path| !p.to_string_lossy().ends_with(".jsonl");
        assert!(
            check(&e, Some(&ok), true, no_transcript)
                .unwrap_err()
                .contains("transcript gone")
        );
        let no_dir = |p: &Path| p != Path::new("/w/api");
        assert!(
            check(&e, Some(&ok), true, no_dir)
                .unwrap_err()
                .contains("directory gone")
        );
    }

    #[test]
    fn claims_before_the_shell_is_up_but_types_only_into_one() {
        let e = entry();
        let cat = LivePane {
            command: "cat".into(),
            ..shell("%1", "main", 1, 0, "/w/api")
        };
        assert!(check(&e, Some(&cat), false, |_| true).is_ok());
        assert!(check(&e, Some(&cat), true, |_| true).is_err());
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
