use super::Tmux;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub windows: Vec<Window>,
    pub panes: Vec<Pane>,
    pub clients: Vec<Client>,
}

/// One change-detection hash per snapshot section. The daemon pushes a
/// snapshot only when one of these differs from the last pushed one, so a
/// section whose data changes on every read (an agent's elapsed time, a git
/// `checked_at`) must hash only its stable fields, or the daemon would push
/// on every poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sections {
    /// Sessions, windows, panes and clients.
    pub tmux: u64,
}

impl Snapshot {
    pub fn sections(&self) -> Sections {
        // Exhaustive on purpose: a field added to `Snapshot` fails to compile
        // here until it is given a section hash (or explicitly ignored).
        let Snapshot {
            sessions,
            windows,
            panes,
            clients,
        } = self;
        let mut h = DefaultHasher::new();
        (sessions, windows, panes, clients).hash(&mut h);
        Sections { tmux: h.finish() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Hash)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub attached: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Hash)]
pub struct Window {
    pub id: String,
    pub session_id: String,
    pub index: u32,
    pub name: String,
    pub automatic_rename: bool,
    pub active: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Hash)]
pub struct Pane {
    pub id: String,
    pub window_id: String,
    pub session_id: String,
    pub index: u32,
    pub active: bool,
    pub current_command: String,
    pub current_path: String,
    pub title: String,
    /// `@pane_role` (e.g. `sidebar`); sidebar panes are views, not work.
    #[serde(default)]
    pub role: String,
    /// The pane's agent options (`crate::agent::OPTIONS`), the non-empty
    /// ones only. Raw: `crate::agent` turns them into agent state. They
    /// change on agent events, never by the clock (a run's start time is
    /// stored, not its elapsed time), so they can be hashed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_opts: BTreeMap<String, String>,
    /// The pane's terminal (`/dev/ttys004`), for stopped-job checks.
    #[serde(default)]
    pub tty: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Hash)]
pub struct Client {
    pub name: String,
    pub tty: String,
    pub session_id: String,
}

const SEP: char = '\x1f';
/// Record terminator: every format below ends in `\x1e`, so a record is
/// `...\x1e\n`. Splitting on that pair (rather than on `\n`) keeps a newline
/// inside a field — legal in a directory name, so in `pane_current_path` —
/// from splitting one record into two.
const REC_END: &str = "\x1e\n";

// Field separator is ASCII unit separator \x1f (not typeable at a prompt, so it
// cannot appear in names tmux-home's users type in). Names are the *last* field
// in each format so a stray separator elsewhere can't shift the other fields.
// The agent options sit between the fixed fields and the names; a prompt is
// stored with newlines and `|` replaced, but a literal \x1f in one would
// shift the fields after it (the record is then misread, not lost).
const PANE_HEAD: &str = "#{session_id}\x1f#{window_id}\x1f#{window_index}\x1f#{window_active}\x1f#{automatic-rename}\x1f#{pane_id}\x1f#{pane_index}\x1f#{pane_active}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{@pane_role}\x1f#{pane_tty}\x1f";
const PANE_TAIL: &str = "#{session_name}\x1f#{pane_title}\x1f#{window_name}\x1e";
/// Fields before the agent options.
const HEAD_FIELDS: usize = 12;
/// Fields in a `list-panes` record.
fn pane_fields() -> usize {
    HEAD_FIELDS + crate::agent::OPTIONS.len() + 3
}

/// `#{@a}\x1f#{@b}\x1f…\x1f`: the agent options, each followed by a separator.
fn agent_opts_fmt() -> String {
    crate::agent::OPTIONS
        .iter()
        .map(|o| format!("#{{{o}}}\x1f"))
        .collect()
}

/// Agent options from their fields, in `agent::OPTIONS` order; empty
/// values are dropped.
fn agent_opts_from(fields: &[&str]) -> BTreeMap<String, String> {
    crate::agent::OPTIONS
        .iter()
        .zip(fields)
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

static PANE_FMT: LazyLock<String> =
    LazyLock::new(|| format!("{PANE_HEAD}{}{PANE_TAIL}", agent_opts_fmt()));
const CLIENT_FMT: &str = "#{client_name}\x1f#{client_tty}\x1f#{session_id}\x1f#{client_flags}\x1e";

/// Reads a full snapshot of the tmux server (synchronously; the daemon
/// uses `read_snapshot_async`).
///
/// A server with no sessions is alive and yields an *empty* snapshot: tmux
/// answers `list-panes -a`/`list-clients` there with "no current target",
/// and that is exactly the state the server is in while tmux.conf (and so
/// TPM's run of tmux-home.tmux) executes. An error is returned only when the
/// server itself can't be reached (`list-sessions` fails too).
pub fn read_snapshot(t: &Tmux) -> anyhow::Result<Snapshot> {
    match read_parsed(t) {
        Ok(s) => Ok(s),
        Err(e) => match session_count(t) {
            Err(_) => Err(e), // server unreachable
            Ok(0) => Ok(Snapshot::default()),
            // a session appeared between the two reads: read once more
            Ok(_) => read_parsed(t),
        },
    }
}

/// `read_snapshot` off the async executor.
pub async fn read_snapshot_async(t: &Tmux) -> anyhow::Result<Snapshot> {
    let t = t.clone();
    tokio::task::spawn_blocking(move || read_snapshot(&t)).await?
}

/// The panes of one window (or any `list-panes -t` target), parsed exactly
/// as a snapshot's are.
pub fn read_panes(t: &Tmux, target: &str) -> anyhow::Result<Vec<Pane>> {
    let out = t.run(&["list-panes", "-t", target, "-F", &PANE_FMT])?;
    Ok(parse(&out, "").panes)
}

fn read_parsed(t: &Tmux) -> anyhow::Result<Snapshot> {
    let panes = t.run(&["list-panes", "-a", "-F", &PANE_FMT])?;
    let clients = t.run(&["list-clients", "-F", CLIENT_FMT])?;
    Ok(parse(&panes, &clients))
}

/// Number of sessions on the server; an error means the server is
/// unreachable (unlike `has-session`/`list-panes`, `list-sessions` succeeds
/// on a live server with no sessions).
pub fn session_count(t: &Tmux) -> anyhow::Result<usize> {
    let out = t.run(&["list-sessions", "-F", "#{session_id}"])?;
    Ok(out.lines().filter(|l| !l.is_empty()).count())
}

/// Splits tmux output into records (see `REC_END`). A last record that
/// lacks its trailing `\n` still has its `\x1e` stripped.
fn records(out: &str) -> impl Iterator<Item = &str> {
    out.split(REC_END)
        .map(|r| r.strip_suffix('\x1e').unwrap_or(r))
        .filter(|r| !r.is_empty())
}

struct PaneRec<'a> {
    f: Vec<&'a str>,
    window_index: u32,
    pane_index: u32,
}

fn pane_record(rec: &str) -> Option<PaneRec<'_>> {
    let n = pane_fields();
    let f: Vec<&str> = rec.splitn(n, SEP).collect();
    if f.len() != n {
        return None;
    }
    Some(PaneRec {
        window_index: f[2].parse().ok()?,
        pane_index: f[6].parse().ok()?,
        f,
    })
}

/// Parses `list-panes`/`list-clients` output. An unparseable record is
/// logged and skipped rather than failing the whole read: one odd pane must
/// not blank the picture of every other one.
pub fn parse(panes: &str, clients: &str) -> Snapshot {
    let mut s = Snapshot::default();
    for rec in records(panes) {
        let Some(PaneRec {
            f,
            window_index,
            pane_index,
        }) = pane_record(rec)
        else {
            eprintln!("tmux-home: skipping unparseable list-panes record: {rec:?}");
            continue;
        };
        let (sid, wid) = (f[0].to_string(), f[1].to_string());
        let n = f.len();
        let (sname, title, wname) = (f[n - 3], f[n - 2], f[n - 1]);
        if !s.sessions.iter().any(|x| x.id == sid) {
            s.sessions.push(Session {
                id: sid.clone(),
                name: sname.to_string(),
                attached: 0,
            });
        }
        if !s.windows.iter().any(|w| w.id == wid && w.session_id == sid) {
            s.windows.push(Window {
                id: wid.clone(),
                session_id: sid.clone(),
                index: window_index,
                active: f[3] == "1",
                automatic_rename: f[4] == "1",
                name: wname.to_string(),
            });
        }
        s.panes.push(Pane {
            id: f[5].to_string(),
            window_id: wid,
            session_id: sid,
            index: pane_index,
            active: f[7] == "1",
            current_command: f[8].to_string(),
            current_path: f[9].to_string(),
            title: title.to_string(),
            role: f[10].to_string(),
            tty: f[11].to_string(),
            agent_opts: agent_opts_from(&f[HEAD_FIELDS..n - 3]),
        });
    }
    for rec in records(clients) {
        let f: Vec<&str> = rec.splitn(4, SEP).collect();
        if f.len() != 4 {
            eprintln!("tmux-home: skipping unparseable list-clients record: {rec:?}");
            continue;
        }
        if f[3].split(',').any(|x| x == "control-mode") {
            continue; // our own control client, never a user client
        }
        s.clients.push(Client {
            name: f[0].into(),
            tty: f[1].into(),
            session_id: f[2].into(),
        });
        if let Some(sess) = s.sessions.iter_mut().find(|x| x.id == f[2]) {
            sess.attached += 1;
        }
    }
    s.sessions.sort_by(|a, b| a.name.cmp(&b.name));
    s
}
