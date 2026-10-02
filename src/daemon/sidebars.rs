//! The daemon's sidebar task (spec §8), driven by the published snapshots
//! like the git task — no global tmux hooks:
//!
//! - **Auto-create:** with `@home-sidebar-auto on`, a window *created
//!   after* auto-create became allowed gets a sidebar, unless one of its
//!   sessions is listed in `@home-sidebar-exclude` (space-separated names)
//!   or it already has a sidebar. Each window is decided once, when first
//!   seen, so closing its sidebar with `prefix e` sticks. Auto-create is
//!   allowed (and stays so) once the server is past its start-up (`Gate`):
//!   at least `min_age` old, done restoring (with `@continuum-restore on`:
//!   until resurrect's post-restore hook sets `@home_restore_done`, at most
//!   `restore_hold`), and its windows and panes unchanged for `stable`.
//!   Windows that exist at that moment are never given one: a restore
//!   creates windows by index and later selects panes and types commands by
//!   index, so a sidebar added mid-restore shifts every index after it.
//! - **Width:** a tmux-home sidebar seen for the first time (a restored one
//!   keeps its saved width) is resized to `@home-sidebar-width`.
//! - **Cleanup:** a window whose panes are all sidebars (its last real pane
//!   exited) has its tmux-home sidebars killed, which closes it.
//! - **No duplicates:** a window holding more than one tmux-home sidebar
//!   keeps the oldest (lowest pane ID), resized to `@home-sidebar-width`.
//!
//! Only panes marked `@home_role=sidebar` are ever killed.

use super::Shared;
use crate::{
    sidebar::{self, Placement, ROLE},
    tmux::{Tmux, snapshot::Snapshot},
};
use std::{
    collections::{BTreeMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

/// When auto-create may start (see the module docs). Defaults: 30 s, 5 s,
/// 60 s; `TMUX_HOME_AUTO_MIN_AGE`, `TMUX_HOME_AUTO_STABLE_MS` and
/// `TMUX_HOME_RESTORE_HOLD` override them (tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gate {
    pub min_age: Duration,
    pub stable: Duration,
    pub restore_hold: Duration,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            min_age: Duration::from_secs(30),
            stable: Duration::from_secs(5),
            restore_hold: Duration::from_secs(60),
        }
    }
}

impl Gate {
    pub fn from_env() -> Gate {
        let num = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let d = Gate::default();
        Gate {
            min_age: num("TMUX_HOME_AUTO_MIN_AGE").map_or(d.min_age, Duration::from_secs),
            stable: num("TMUX_HOME_AUTO_STABLE_MS").map_or(d.stable, Duration::from_millis),
            restore_hold: num("TMUX_HOME_RESTORE_HOLD").map_or(d.restore_hold, Duration::from_secs),
        }
    }

    /// Past start-up, stability aside: old enough, and not (possibly)
    /// still restoring: `continuum` is `@continuum-restore on`,
    /// `restore_done` is `@home_restore_done` set.
    pub fn past_startup(&self, age: Duration, continuum: bool, restore_done: bool) -> bool {
        age >= self.min_age && (!continuum || restore_done || age >= self.restore_hold)
    }
}

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
    /// Sidebars to (re)size to the configured width: first seen, or the
    /// survivor of a duplicate.
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

/// Pure: windows already seen, panes already killed or sized.
#[derive(Debug)]
pub struct Planner {
    stable: Duration,
    /// Auto-create is allowed (latched).
    allowed: bool,
    /// Windows seen; those seen before `allowed` are pre-existing.
    known: HashSet<String>,
    /// The hash of the windows and panes, and since when it holds.
    structure: Option<(u64, Instant)>,
    /// Kills issued (pane IDs are never reused): not issued again while a
    /// snapshot from before the kill is still the published one.
    killed: HashSet<String>,
    /// Sidebars already resized when first seen.
    sized: HashSet<String>,
}

impl Planner {
    pub fn new(stable: Duration) -> Planner {
        Planner {
            stable,
            allowed: false,
            known: HashSet::new(),
            structure: None,
            killed: HashSet::new(),
            sized: HashSet::new(),
        }
    }

    pub fn allowed(&self) -> bool {
        self.allowed
    }

    /// Plans for snapshot `s` at `now`; `past_startup` is `Gate`'s verdict.
    /// Called on every snapshot and on a timer (stability needs the clock).
    pub fn observe(&mut self, s: &Snapshot, now: Instant, past_startup: bool) -> Plan {
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
        let mut h = DefaultHasher::new();
        for (w, (panes, _)) in &windows {
            w.hash(&mut h);
            let mut ids: Vec<&str> = panes.iter().map(|p| p.id.as_str()).collect();
            ids.sort();
            ids.hash(&mut h);
        }
        let h = h.finish();
        if self.structure.is_none_or(|(old, _)| old != h) {
            self.structure = Some((h, now));
        }
        let was_allowed = self.allowed;
        if !self.allowed
            && past_startup
            && self
                .structure
                .is_some_and(|(_, since)| now.duration_since(since) >= self.stable)
        {
            self.allowed = true;
        }
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
                ours.clear();
            } else if ours.len() > 1 {
                ours.sort_by_key(|p| pane_num(p));
                plan.kills.extend(ours[1..].iter().map(|p| p.to_string()));
                plan.resize.push(ours[0].to_string());
                self.sized.insert(ours[0].to_string());
                ours.truncate(1);
            }
            for p in ours {
                if self.sized.insert(p.to_string()) {
                    plan.resize.push(p.to_string());
                }
            }
            if self.known.insert(wid.to_string()) && was_allowed {
                plan.new_windows.push(NewWindow {
                    id: wid.to_string(),
                    sessions: sessions.clone(),
                    has_sidebar: panes.iter().any(|p| p.role == ROLE),
                });
            }
        }
        // window IDs are never reused: forget the ones gone
        self.known.retain(|w| windows.contains_key(w.as_str()));
        let live: HashSet<&str> = s.panes.iter().map(|p| p.id.as_str()).collect();
        self.killed.retain(|p| live.contains(p.as_str()));
        self.sized.retain(|p| live.contains(p.as_str()));
        self.killed.extend(plan.kills.iter().cloned());
        plan
    }
}

/// The server's start time (`#{start_time}`, Unix seconds).
fn server_start(t: &Tmux) -> Option<u64> {
    let out = t.run(&["display-message", "-p", "#{start_time}"]).ok()?;
    out.trim().parse().ok()
}

/// `Gate::past_startup`, reading the server's age and options.
fn past_startup(t: &Tmux, gate: &Gate, start: Option<u64>) -> bool {
    let Some(start) = start else {
        return false;
    };
    let age = Duration::from_secs(crate::agent::now().saturating_sub(start));
    let continuum = sidebar::global_option(t, "@continuum-restore").trim() == "on";
    let done = !sidebar::global_option(t, "@home_restore_done")
        .trim()
        .is_empty();
    gate.past_startup(age, continuum, done)
}

/// Carries out a plan (blocking: tmux commands). Failures are logged: a
/// pane or window can go between the snapshot and the command.
fn execute(t: &Tmux, plan: Plan) {
    for p in &plan.kills {
        if let Err(e) = t.run(&["kill-pane", "-t", p]) {
            eprintln!("tmux-home: sidebar cleanup: {e:#}");
        }
    }
    let place =
        (!plan.resize.is_empty() || !plan.new_windows.is_empty()).then(|| Placement::read(t));
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
        // the split is -d; still, never leave the focus on the sidebar
        let active = t.run(&["display-message", "-p", "-t", &w, "#{pane_id}"]);
        match sidebar::add(t, &w, &place, &bin) {
            Ok(_) => {
                if let Ok(a) = active {
                    let now = t
                        .run(&["display-message", "-p", "-t", &w, "#{pane_id}"])
                        .unwrap_or_default();
                    if now.trim() != a.trim() {
                        let _ = t.run(&["select-pane", "-t", a.trim()]);
                    }
                }
            }
            Err(e) => eprintln!("tmux-home: sidebar auto-create in {w}: {e:#}"),
        }
    }
}

/// How often the task re-plans without a new snapshot (stability, age).
const TICK: Duration = Duration::from_secs(1);

pub(super) fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    tokio::spawn(async move {
        // subscribe before anything that waits, so no snapshot is missed
        let mut snaps = shared.latest.subscribe();
        snaps.mark_changed();
        let t = shared.tmux.clone();
        let gate = Gate::from_env();
        let start = {
            let t = t.clone();
            tokio::task::spawn_blocking(move || server_start(&t))
                .await
                .ok()
                .flatten()
        };
        let mut planner = Planner::new(gate.stable);
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                r = snaps.changed() => if r.is_err() { return },
                _ = tick.tick() => {}
            }
            let snap = snaps.borrow_and_update().as_ref().map(|(_, s)| s.clone());
            let Some(snap) = snap else {
                continue;
            };
            let ready = planner.allowed() || {
                let t = t.clone();
                tokio::task::spawn_blocking(move || past_startup(&t, &gate, start))
                    .await
                    .unwrap_or(false)
            };
            let plan = planner.observe(&snap, Instant::now(), ready);
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

    const STABLE: Duration = Duration::from_secs(5);

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// A planner that has seen `s` while past start-up for `STABLE`: auto-
    /// create allowed, `s`'s windows pre-existing.
    fn allowed(s: &Snapshot) -> (Planner, Instant) {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE);
        p.observe(s, t0, true);
        p.observe(s, at(t0, 5000), true);
        assert!(p.allowed());
        (p, at(t0, 5000))
    }

    #[test]
    fn the_gate_waits_for_age_and_for_the_restore() {
        let g = Gate::default();
        let s = Duration::from_secs;
        assert!(!g.past_startup(s(10), false, false));
        assert!(g.past_startup(s(30), false, false));
        assert!(!g.past_startup(s(30), true, false), "continuum restoring");
        assert!(g.past_startup(s(30), true, true), "restore done");
        assert!(g.past_startup(s(60), true, false), "held at most 60 s");
        assert!(!g.past_startup(s(5), true, true), "and never young");
    }

    #[test]
    fn windows_seen_before_auto_create_is_allowed_never_get_one() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE);
        let one = snap(&[("@1", "$1")], vec![real("%1", "@1")]);
        let two = snap(
            &[("@1", "$1"), ("@2", "$1")],
            vec![real("%1", "@1"), real("%2", "@2")],
        );
        // young server / restoring: nothing, however long
        assert!(p.observe(&one, t0, false).new_windows.is_empty());
        assert!(p.observe(&two, at(t0, 9000), false).new_windows.is_empty());
        // past start-up, but the windows changed 0 s ago: not yet
        assert!(p.observe(&two, at(t0, 9000), true).new_windows.is_empty());
        assert!(!p.allowed());
        assert!(p.observe(&two, at(t0, 13_000), true).new_windows.is_empty());
        assert!(!p.allowed(), "stable 4 s only");
        // stable for 5 s: allowed; @1 and @2 are pre-existing
        assert!(p.observe(&two, at(t0, 14_000), true).new_windows.is_empty());
        assert!(p.allowed());
        let three = snap(
            &[("@1", "$1"), ("@2", "$1"), ("@3", "$1")],
            vec![real("%1", "@1"), real("%2", "@2"), real("%3", "@3")],
        );
        assert_eq!(
            ids(&p.observe(&three, at(t0, 14_100), false).new_windows),
            ["@3"],
            "allowed is latched"
        );
        assert!(
            p.observe(&three, at(t0, 15_000), true)
                .new_windows
                .is_empty(),
            "decided once"
        );
    }

    #[test]
    fn churn_keeps_auto_create_off() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE);
        for i in 0..20u64 {
            let ws: Vec<(String, &str)> = (0..=i).map(|n| (format!("@{n}"), "$1")).collect();
            let ws: Vec<(&str, &str)> = ws.iter().map(|(a, b)| (a.as_str(), *b)).collect();
            let s = snap(&ws, vec![]);
            p.observe(&s, at(t0, i * 1000), true);
            assert!(!p.allowed(), "a window every second: {i}");
        }
    }

    #[test]
    fn auto_skips_excluded_sessions_windows_with_a_sidebar_and_off() {
        let (mut p, now) = allowed(&snap(&[], vec![]));
        let mut old = pane("%4", "@4", ROLE, false);
        old.session_id = "$1".into();
        let s = snap(
            &[
                ("@1", "$1"),
                ("@2", "$2"),
                ("@3", "$1"),
                ("@4", "$1"),
                ("@5", "$1"),
            ],
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
        let new = p.observe(&s, now, true).new_windows;
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
    fn a_sidebar_is_sized_when_first_seen() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE);
        let s = snap(&[("@1", "$1")], vec![real("%1", "@1"), side("%2", "@1")]);
        assert_eq!(p.observe(&s, t0, false).resize, ["%2"]);
        assert!(p.observe(&s, t0, false).resize.is_empty(), "once");
    }

    #[test]
    fn a_window_left_with_only_sidebars_has_ours_killed_once() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE);
        let s = snap(&[("@1", "$1")], vec![real("%1", "@1"), side("%2", "@1")]);
        p.observe(&s, t0, false);
        assert_eq!(p.observe(&s, t0, false), Plan::default());
        let gone = snap(&[("@1", "$1")], vec![side("%2", "@1")]);
        assert_eq!(p.observe(&gone, t0, false).kills, ["%2"]);
        assert!(
            p.observe(&gone, t0, false).kills.is_empty(),
            "not issued twice"
        );
        // tmux-agent-sidebar's panes are never ours to kill
        let theirs = snap(
            &[("@1", "$1")],
            vec![pane("%3", "@1", ROLE, false), side("%2", "@1")],
        );
        assert_eq!(
            Planner::new(STABLE).observe(&theirs, t0, false).kills,
            ["%2"]
        );
        let only_theirs = snap(&[("@1", "$1")], vec![pane("%3", "@1", ROLE, false)]);
        assert!(
            Planner::new(STABLE)
                .observe(&only_theirs, t0, false)
                .kills
                .is_empty()
        );
    }

    #[test]
    fn duplicates_keep_the_oldest() {
        let s = snap(
            &[("@1", "$1")],
            vec![side("%12", "@1"), real("%1", "@1"), side("%9", "@1")],
        );
        let plan = Planner::new(STABLE).observe(&s, Instant::now(), false);
        assert_eq!(plan.kills, ["%12"]);
        assert_eq!(plan.resize, ["%9"]);
    }

    #[test]
    fn a_linked_windows_panes_count_once() {
        let s = snap(
            &[("@1", "$1"), ("@1", "$2")],
            vec![
                real("%1", "@1"),
                side("%2", "@1"),
                real("%1", "@1"),
                side("%2", "@1"),
            ],
        );
        let mut p = Planner::new(STABLE);
        assert_eq!(p.observe(&s, Instant::now(), false).resize, ["%2"]);
        assert_eq!(p.observe(&s, Instant::now(), false), Plan::default());
    }
}
