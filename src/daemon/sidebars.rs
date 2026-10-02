//! The daemon's sidebar task (spec §8), driven by the published snapshots
//! like the git task — no global tmux hooks:
//!
//! - **Auto-create:** with `@home-sidebar-auto on`, a window that appears
//!   in a snapshot gets a sidebar unless one of its sessions is listed in
//!   `@home-sidebar-exclude` (space-separated names) or it already has a
//!   sidebar (tmux-home's or tmux-agent-sidebar's). Each window is decided
//!   once, when first seen, so closing its sidebar with `prefix e` sticks.
//!   Windows already there when the daemon starts are not new (a daemon
//!   restarted by a rebuild adds nothing), unless the server itself started
//!   moments ago (`FRESH_SERVER`): then they are its first windows.
//! - **Cleanup:** a window whose panes are all sidebars (its last real pane
//!   exited) has its tmux-home sidebars killed, which closes it.
//! - **No duplicates:** a window holding more than one tmux-home sidebar
//!   (an auto-created one and one tmux-resurrect restored, say) keeps the
//!   oldest (lowest pane ID), resized to `@home-sidebar-width`.
//!
//! Only panes marked `@home_role=sidebar` are ever killed.

use super::Shared;
use crate::{
    sidebar::{self, Placement, ROLE},
    tmux::{Tmux, snapshot::Snapshot},
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinHandle;

/// A server younger than this when the daemon starts is starting up: the
/// windows in the daemon's first snapshot count as new.
pub const FRESH_SERVER: Duration = Duration::from_secs(15);

/// A window seen for the first time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWindow {
    pub id: String,
    /// The names of the sessions it is in (more than one when linked).
    pub sessions: Vec<String>,
    /// It already has a sidebar pane (any role `sidebar`).
    pub has_sidebar: bool,
}

/// What one snapshot calls for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub new_windows: Vec<NewWindow>,
    /// tmux-home sidebar panes to kill.
    pub kills: Vec<String>,
    /// Sidebars that survived a duplicate: back to the configured width.
    pub resize: Vec<String>,
}

/// `@home-sidebar-auto` and `@home-sidebar-exclude`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Auto {
    pub on: bool,
    pub exclude: Vec<String>,
}

impl Auto {
    pub fn from_options(auto: &str, exclude: &str) -> Auto {
        Auto {
            on: matches!(auto.trim(), "on" | "1" | "yes" | "true"),
            exclude: exclude.split_whitespace().map(String::from).collect(),
        }
    }

    /// The new windows that get a sidebar.
    pub fn creates(&self, new: &[NewWindow]) -> Vec<String> {
        if !self.on {
            return Vec::new();
        }
        new.iter()
            .filter(|w| !w.has_sidebar && !w.sessions.iter().any(|s| self.exclude.contains(s)))
            .map(|w| w.id.clone())
            .collect()
    }
}

/// `%12` → 12, for "oldest pane" (pane IDs only grow).
fn pane_num(id: &str) -> u64 {
    id.trim_start_matches('%').parse().unwrap_or(u64::MAX)
}

/// Pure: windows already seen, panes already killed.
#[derive(Debug)]
pub struct Planner {
    /// `None` until the first snapshot.
    known: Option<HashSet<String>>,
    /// The first snapshot's windows count as new.
    fresh_server: bool,
    /// Kills issued (pane IDs are never reused): not issued again while a
    /// snapshot from before the kill is still the published one.
    killed: HashSet<String>,
}

impl Planner {
    pub fn new(fresh_server: bool) -> Planner {
        Planner {
            known: None,
            fresh_server,
            killed: HashSet::new(),
        }
    }

    pub fn observe(&mut self, s: &Snapshot) -> Plan {
        let mut plan = Plan::default();
        // window → its panes (a linked window's panes are listed once per
        // session it is in) and its sessions' names, in snapshot order
        let mut windows: BTreeMap<&str, (Vec<&crate::tmux::snapshot::Pane>, Vec<String>)> =
            BTreeMap::new();
        let mut order: Vec<&str> = Vec::new();
        for w in &s.windows {
            let e = windows.entry(&w.id).or_insert_with(|| {
                order.push(&w.id);
                Default::default()
            });
            if let Some(sess) = s.sessions.iter().find(|x| x.id == w.session_id)
                && !e.1.contains(&sess.name)
            {
                e.1.push(sess.name.clone());
            }
        }
        for p in &s.panes {
            if let Some(e) = windows.get_mut(p.window_id.as_str())
                && !e.0.iter().any(|q| q.id == p.id)
            {
                e.0.push(p);
            }
        }
        let first = self.known.is_none();
        let known = self.known.get_or_insert_with(HashSet::new);
        for wid in &order {
            let (panes, sessions) = &windows[wid];
            let mut ours: Vec<&str> = panes
                .iter()
                .filter(|p| p.home_role == ROLE && !self.killed.contains(&p.id))
                .map(|p| p.id.as_str())
                .collect();
            let real = panes.iter().any(|p| p.role != ROLE);
            if !ours.is_empty() && !real {
                plan.kills.extend(ours.iter().map(|p| p.to_string()));
            } else if ours.len() > 1 {
                ours.sort_by_key(|p| pane_num(p));
                plan.kills.extend(ours[1..].iter().map(|p| p.to_string()));
                plan.resize.push(ours[0].to_string());
            }
            if known.insert(wid.to_string()) && (!first || self.fresh_server) {
                plan.new_windows.push(NewWindow {
                    id: wid.to_string(),
                    sessions: sessions.clone(),
                    has_sidebar: panes.iter().any(|p| p.role == ROLE),
                });
            }
        }
        // window IDs are never reused: forget the ones gone
        known.retain(|w| windows.contains_key(w.as_str()));
        let live: HashSet<&str> = s.panes.iter().map(|p| p.id.as_str()).collect();
        self.killed.retain(|p| live.contains(p.as_str()));
        self.killed.extend(plan.kills.iter().cloned());
        plan
    }
}

/// How long ago the server started (`#{start_time}`); `None` if unknown.
fn server_age(t: &Tmux) -> Option<Duration> {
    let out = t.run(&["display-message", "-p", "#{start_time}"]).ok()?;
    let start: u64 = out.trim().parse().ok()?;
    Some(Duration::from_secs(crate::agent::now().saturating_sub(start)))
}

/// Carries out a plan (blocking: tmux commands). Failures are logged: a
/// pane or window can go between the snapshot and the command.
fn execute(t: &Tmux, plan: Plan) {
    for p in &plan.kills {
        if let Err(e) = t.run(&["kill-pane", "-t", p]) {
            eprintln!("tmux-home: sidebar cleanup: {e:#}");
        }
    }
    let place = (!plan.resize.is_empty() || !plan.new_windows.is_empty())
        .then(|| Placement::read(t));
    for p in &plan.resize {
        let w = place.as_ref().map(|p| p.width.clone()).unwrap_or_default();
        let _ = t.run(&["resize-pane", "-t", p, "-x", &w]);
    }
    if plan.new_windows.is_empty() {
        return;
    }
    let auto = Auto::from_options(
        &sidebar::global_option(t, "@home-sidebar-auto"),
        &sidebar::global_option(t, "@home-sidebar-exclude"),
    );
    let bin = sidebar::sidebar_bin();
    for w in auto.creates(&plan.new_windows) {
        let place = place.clone().unwrap_or_default();
        if let Err(e) = sidebar::add(t, &w, &place, &bin) {
            eprintln!("tmux-home: sidebar auto-create in {w}: {e:#}");
        }
    }
}

pub(super) fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let t = shared.tmux.clone();
        let fresh = {
            let t = t.clone();
            tokio::task::spawn_blocking(move || server_age(&t))
                .await
                .ok()
                .flatten()
                .is_some_and(|a| a < FRESH_SERVER)
        };
        let mut planner = Planner::new(fresh);
        let mut snaps = shared.latest.subscribe();
        while snaps.changed().await.is_ok() {
            let plan = {
                let cur = snaps.borrow_and_update();
                let Some((_, s)) = cur.as_ref() else {
                    continue;
                };
                planner.observe(s)
            };
            if plan == Plan::default() {
                continue;
            }
            let t = t.clone();
            let _ = tokio::task::spawn_blocking(move || execute(&t, plan)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::snapshot::{Pane, Session, Window};

    fn pane(id: &str, wid: &str, role: &str, home: bool) -> Pane {
        Pane {
            id: id.into(),
            window_id: wid.into(),
            session_id: "$1".into(),
            index: 0,
            active: false,
            current_command: "sh".into(),
            current_path: "/".into(),
            title: String::new(),
            role: role.into(),
            home_role: if home { ROLE.into() } else { String::new() },
            agent_opts: Default::default(),
            tty: String::new(),
        }
    }

    fn snap(windows: &[(&str, &str)], panes: Vec<Pane>) -> Snapshot {
        Snapshot {
            sessions: vec![
                Session {
                    id: "$1".into(),
                    name: "main".into(),
                    attached: 1,
                },
                Session {
                    id: "$2".into(),
                    name: "scratch".into(),
                    attached: 0,
                },
            ],
            windows: windows
                .iter()
                .map(|(id, sid)| Window {
                    id: id.to_string(),
                    session_id: sid.to_string(),
                    index: 0,
                    name: "w".into(),
                    automatic_rename: false,
                    active: true,
                })
                .collect(),
            panes,
            ..Snapshot::default()
        }
    }

    fn real(id: &str, wid: &str) -> Pane {
        pane(id, wid, "", false)
    }

    fn side(id: &str, wid: &str) -> Pane {
        pane(id, wid, ROLE, true)
    }

    fn ids(n: &[NewWindow]) -> Vec<&str> {
        n.iter().map(|w| w.id.as_str()).collect()
    }

    #[test]
    fn windows_there_at_start_are_not_new_on_a_running_server() {
        let mut p = Planner::new(false);
        let s = snap(&[("@1", "$1")], vec![real("%1", "@1")]);
        assert!(p.observe(&s).new_windows.is_empty());
        let s2 = snap(
            &[("@1", "$1"), ("@2", "$1")],
            vec![real("%1", "@1"), real("%2", "@2")],
        );
        assert_eq!(ids(&p.observe(&s2).new_windows), ["@2"]);
        assert!(p.observe(&s2).new_windows.is_empty(), "decided once");
    }

    #[test]
    fn a_fresh_servers_first_windows_are_new() {
        let mut p = Planner::new(true);
        let s = snap(&[("@1", "$1")], vec![real("%1", "@1")]);
        assert_eq!(ids(&p.observe(&s).new_windows), ["@1"]);
        assert!(p.observe(&s).new_windows.is_empty());
    }

    #[test]
    fn auto_skips_excluded_sessions_windows_with_a_sidebar_and_off() {
        let mut p = Planner::new(true);
        let mut old = pane("%4", "@4", ROLE, false);
        old.session_id = "$1".into();
        let s = snap(
            &[("@1", "$1"), ("@2", "$2"), ("@3", "$1"), ("@4", "$1"), ("@5", "$1")],
            vec![
                real("%1", "@1"),
                real("%2", "@2"),
                real("%3", "@3"),
                side("%9", "@3"),
                real("%5", "@4"),
                old,
                real("%6", "@5"),
            ],
        );
        let new = p.observe(&s).new_windows;
        let auto = Auto::from_options("on", "scratch other");
        assert_eq!(auto.creates(&new), ["@1", "@5"]);
        assert!(Auto::from_options("off", "").creates(&new).is_empty());
        assert!(Auto::from_options("", "").creates(&new).is_empty());
        // a window linked into an excluded session is excluded too
        let linked = [NewWindow {
            id: "@7".into(),
            sessions: vec!["main".into(), "scratch".into()],
            has_sidebar: false,
        }];
        assert!(auto.creates(&linked).is_empty());
    }

    #[test]
    fn a_window_left_with_only_sidebars_has_ours_killed_once() {
        let mut p = Planner::new(false);
        let s = snap(&[("@1", "$1")], vec![real("%1", "@1"), side("%2", "@1")]);
        assert_eq!(p.observe(&s), Plan::default());
        let gone = snap(&[("@1", "$1")], vec![side("%2", "@1")]);
        assert_eq!(p.observe(&gone).kills, ["%2"]);
        assert!(p.observe(&gone).kills.is_empty(), "not issued twice");
        // tmux-agent-sidebar's panes are never ours to kill
        let mut q = Planner::new(false);
        let theirs = snap(
            &[("@1", "$1")],
            vec![pane("%3", "@1", ROLE, false), side("%2", "@1")],
        );
        assert_eq!(q.observe(&theirs).kills, ["%2"]);
        let only_theirs = snap(&[("@1", "$1")], vec![pane("%3", "@1", ROLE, false)]);
        assert!(Planner::new(false).observe(&only_theirs).kills.is_empty());
    }

    #[test]
    fn duplicates_keep_the_oldest() {
        let mut p = Planner::new(false);
        let s = snap(
            &[("@1", "$1")],
            vec![side("%12", "@1"), real("%1", "@1"), side("%9", "@1")],
        );
        let plan = p.observe(&s);
        assert_eq!(plan.kills, ["%12"]);
        assert_eq!(plan.resize, ["%9"]);
    }

    #[test]
    fn a_linked_windows_panes_count_once() {
        let mut p = Planner::new(false);
        let s = snap(
            &[("@1", "$1"), ("@1", "$2")],
            vec![real("%1", "@1"), side("%2", "@1"), real("%1", "@1"), side("%2", "@1")],
        );
        assert_eq!(p.observe(&s), Plan::default());
    }
}
