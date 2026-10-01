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

// Field separator is ASCII unit separator \x1f (not typeable at a prompt, so it
// cannot appear in names tmux-home's users type in). Names are the *last* field
// in each format so a stray separator elsewhere can't shift the other fields.
const PANE_FMT: &str = "#{session_id}\x1f#{window_id}\x1f#{window_index}\x1f#{window_active}\x1f#{automatic-rename}\x1f#{pane_id}\x1f#{pane_index}\x1f#{pane_active}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{session_name}\x1f#{pane_title}\x1f#{window_name}";
const CLIENT_FMT: &str = "#{client_name}\x1f#{client_tty}\x1f#{session_id}\x1f#{client_flags}";

/// Reads a full snapshot of the tmux server and a hash of its contents.
///
/// The hash is computed over the *parsed* `Snapshot`, not the raw tmux output:
/// `list-clients` output includes tmux-home's own control-mode client, which
/// `parse` filters out. Hashing the raw output would therefore report changes
/// that no user-visible state actually underwent.
pub async fn read_snapshot(t: &Tmux) -> anyhow::Result<(Snapshot, u64)> {
    let panes = t.run(&["list-panes", "-a", "-F", PANE_FMT]).await?;
    let clients = t.run(&["list-clients", "-F", CLIENT_FMT]).await?;
    let snapshot = parse(&panes, &clients)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(&snapshot)?.hash(&mut h);
    Ok((snapshot, h.finish()))
}

pub fn parse(panes: &str, clients: &str) -> anyhow::Result<Snapshot> {
    let mut s = Snapshot::default();
    for line in panes.lines() {
        let f: Vec<&str> = line.splitn(13, SEP).collect();
        anyhow::ensure!(f.len() == 13, "bad list-panes line: {line:?}");
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
                index: f[2].parse()?,
                active: f[3] == "1",
                automatic_rename: f[4] == "1",
                name: f[12].to_string(),
            });
        }
        s.panes.push(Pane {
            id: f[5].to_string(),
            window_id: wid,
            session_id: sid,
            index: f[6].parse()?,
            active: f[7] == "1",
            current_command: f[8].to_string(),
            current_path: f[9].to_string(),
            title: f[11].to_string(),
        });
    }
    for line in clients.lines() {
        let f: Vec<&str> = line.splitn(4, SEP).collect();
        anyhow::ensure!(f.len() == 4, "bad list-clients line: {line:?}");
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
    Ok(s)
}
