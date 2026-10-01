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
pub struct Session {
    pub id: String,
    pub name: String,
    pub attached: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Window {
    pub id: String,
    pub session_id: String,
    pub index: u32,
    pub name: String,
    pub automatic_rename: bool,
    pub active: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pane {
    pub id: String,
    pub window_id: String,
    pub session_id: String,
    pub index: u32,
    pub active: bool,
    pub current_command: String,
    pub current_path: String,
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
const PANE_FMT: &str = "#{session_id}\x1f#{window_id}\x1f#{window_index}\x1f#{window_active}\x1f#{automatic-rename}\x1f#{pane_id}\x1f#{pane_index}\x1f#{pane_active}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{session_name}\x1f#{pane_title}\x1f#{window_name}\x1e";
const CLIENT_FMT: &str = "#{client_name}\x1f#{client_tty}\x1f#{session_id}\x1f#{client_flags}\x1e";

/// Reads a full snapshot of the tmux server and a hash of its contents.
///
/// The hash is computed over the *parsed* `Snapshot`, not the raw tmux output:
/// `list-clients` output includes tmux-home's own control-mode client, which
/// `parse` filters out. Hashing the raw output would therefore report changes
/// that no user-visible state actually underwent.
///
/// A server with no sessions is alive and yields an *empty* snapshot: tmux
/// answers `list-panes -a`/`list-clients` there with "no current target",
/// and that is exactly the state the server is in while tmux.conf (and so
/// TPM's run of tmux-home.tmux) executes. An error is returned only when the
/// server itself can't be reached (`list-sessions` fails too).
pub async fn read_snapshot(t: &Tmux) -> anyhow::Result<(Snapshot, u64)> {
    let snapshot = match read_parsed(t).await {
        Ok(s) => s,
        Err(e) => match session_count(t).await {
            Err(_) => return Err(e), // server unreachable
            Ok(0) => Snapshot::default(),
            // a session appeared between the two reads: read once more
            Ok(_) => read_parsed(t).await?,
        },
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(&snapshot)?.hash(&mut h);
    Ok((snapshot, h.finish()))
}

async fn read_parsed(t: &Tmux) -> anyhow::Result<Snapshot> {
    let panes = t.run(&["list-panes", "-a", "-F", PANE_FMT]).await?;
    let clients = t.run(&["list-clients", "-F", CLIENT_FMT]).await?;
    Ok(parse(&panes, &clients))
}

/// Number of sessions on the server; an error means the server is
/// unreachable (unlike `has-session`/`list-panes`, `list-sessions` succeeds
/// on a live server with no sessions).
pub async fn session_count(t: &Tmux) -> anyhow::Result<usize> {
    let out = t.run(&["list-sessions", "-F", "#{session_id}"]).await?;
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
    let f: Vec<&str> = rec.splitn(13, SEP).collect();
    if f.len() != 13 {
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
        if !s.sessions.iter().any(|x| x.id == sid) {
            s.sessions.push(Session {
                id: sid.clone(),
                name: f[10].to_string(),
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
                name: f[12].to_string(),
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
            title: f[11].to_string(),
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
