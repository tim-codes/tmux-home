//! The daemon's sidebar task (spec §8), driven by the published snapshots
//! like the git task — no global tmux hooks:
//!
//! - **Auto-create:** with `@home-sidebar-auto on`, a window *created
//!   after* auto-create became allowed gets a sidebar, unless one of its
//!   sessions is listed in `@home-sidebar-exclude` (space-separated names)
//!   or it already has a sidebar. Each window is decided once, when first
//!   seen, so closing its sidebar with `prefix E` sticks. Auto-create is
//!   allowed (and stays so) once the server is past its start-up (`Gate`):
//!   at least `min_age` old, done restoring (until `@home_restore_done` is
//!   set — by resurrect's post-restore hook, or by the config when there is
//!   nothing to restore — at most `restore_hold`), and its windows and
//!   panes unchanged for `stable`.
//!   Windows that exist at that moment are never given one: a restore
//!   creates windows by index and later selects panes and types commands by
//!   index, so a sidebar added mid-restore shifts every index after it.
//! - **Pins:** a window of a pinned session (`@home_sidebar_pin`, set by
//!   `sidebar-toggle --pin`; a linked window counts if any of its sessions
//!   is pinned) that lacks a sidebar gets one at once — no start-up gate,
//!   except never while a restore may be running — unless it is marked
//!   `@home_sidebar_off`. A pinned window that loses its sidebar (the window
//!   toggle, `q`, a kill) is marked so, and keeps none until the session is
//!   pinned again. The state dir's `pins` file keeps the pinned names: when
//!   auto-create becomes allowed (after a restore) the sessions named there
//!   are pinned again; a pinned session's rename is written to it, and a
//!   pinned session killed while others live is dropped from it.
//! - **Width:** a tmux-home sidebar seen for the first time (a restored one
//!   keeps its saved width until then) is resized to the sidebar width: the
//!   one set by hand (the state dir's `sidebar-width`), else
//!   `@home-sidebar-width`. One whose width changes without its window's
//!   width or panes changing, and not by a resize of ours, is being resized
//!   by hand: once that width has held for `settle`, and if it is more than
//!   a column off the last settled one, it becomes the sidebar width, as a
//!   percentage of its window, stored, and applied to every other sidebar
//!   (unless it has lost a sidebar's shape — not full height, off the edge,
//!   over half the window — as `select-layout` would leave it). Full
//!   height is every row its window's panes cover: a pane border status
//!   line (`pane-border-status`) takes a row no pane has.
//!   A window resized, or whose panes came or went (a pane killed beside
//!   the sidebar hands it its space), gets its sidebar set back to the
//!   width.
//! - **Cleanup:** a window whose panes are all sidebars (its last real pane
//!   exited) has its tmux-home sidebars killed, which closes it.
//! - **No duplicates:** a window holding more than one tmux-home sidebar
//!   keeps the oldest (lowest pane ID), resized to the sidebar width.
//!
//! Only panes marked `@home_role=sidebar` are ever killed or resized.

use super::Shared;
use crate::{
    sidebar::{self, OFF_OPTION, PIN_OPTION, Placement, ROLE, Width, state::State},
    tmux::{
        Tmux,
        snapshot::{Pane, Snapshot},
    },
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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

/// An unsigned environment variable (tests).
fn env_num(k: &str) -> Option<u64> {
    std::env::var(k)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Where the server is in its start-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Startup {
    /// Possibly still restoring: no sidebar is added at all.
    Restoring,
    /// Not restoring, but young: pinned sessions get sidebars, auto-create
    /// waits.
    Young,
    /// Past start-up: auto-create may start, once the windows are stable.
    Past,
}

impl Gate {
    pub fn from_env() -> Gate {
        let d = Gate::default();
        Gate {
            min_age: env_num("TMUX_HOME_AUTO_MIN_AGE").map_or(d.min_age, Duration::from_secs),
            stable: env_num("TMUX_HOME_AUTO_STABLE_MS").map_or(d.stable, Duration::from_millis),
            restore_hold: env_num("TMUX_HOME_RESTORE_HOLD")
                .map_or(d.restore_hold, Duration::from_secs),
        }
    }

    /// The start-up phase at server age `age`: restoring until
    /// `restore_done` (`@home_restore_done` set), for at most
    /// `restore_hold`; then young until `min_age`.
    pub fn startup(&self, age: Duration, restore_done: bool) -> Startup {
        if !restore_done && age < self.restore_hold {
            Startup::Restoring
        } else if age >= self.min_age {
            Startup::Past
        } else {
            Startup::Young
        }
    }
}

/// How long a sidebar's width must hold before a change made by hand counts
/// (a drag in progress is left alone): 700 ms, `TMUX_HOME_SIDEBAR_SETTLE_MS`
/// overrides it (tests).
pub fn settle_from_env() -> Duration {
    Duration::from_millis(env_num("TMUX_HOME_SIDEBAR_SETTLE_MS").unwrap_or(700))
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
    /// Sidebars to resize: (pane, columns).
    pub resize: Vec<(String, u32)>,
    /// Windows of pinned sessions that lack a sidebar: add one.
    pub pin_adds: Vec<String>,
    /// Pinned windows that lost their sidebar: mark `@home_sidebar_off`.
    pub mark_off: Vec<String>,
    /// Pin the sessions the `pins` file names (auto-create just became
    /// allowed: the restore, if any, is done).
    pub apply_pins: bool,
    /// Pinned sessions renamed: (old, new) for the `pins` file.
    pub pin_renames: Vec<(String, String)>,
    /// Pinned sessions killed (other sessions live on): out of `pins`.
    pub pins_gone: Vec<String>,
    /// A width set by hand settled: store it, a percentage.
    pub save_width: Option<u32>,
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

/// `width` as a whole percentage of `window_width` (> 0), within 1..=99.
fn pct(width: u32, window_width: u32) -> u32 {
    ((width * 100 + window_width / 2) / window_width).clamp(1, 99)
}

/// Columns a width may drift by without counting as a change: tmux rounds
/// pane sizes when it lays a window out.
const TOLERANCE: u32 = 1;
/// How long a resize of ours may take to show before it's given up on.
const EXPECT_FOR: Duration = Duration::from_secs(3);
/// How long before a pinned window's sidebar that didn't appear is added
/// again (the split failed, e.g. a window too narrow).
const RETRY_ADD: Duration = Duration::from_secs(10);

/// A tmux-home sidebar's width as last seen, to tell a width set by hand
/// from one a window resize, a layout change or the daemon made.
#[derive(Debug)]
struct Track {
    width: u32,
    window_width: u32,
    /// Its window's panes (a hash of their IDs).
    layout: u64,
    /// The width last taken as settled: ours, or set by hand.
    settled: u32,
    /// Since when its width has been changing without us (a drag).
    moved: Option<Instant>,
    /// When we resized it: its next change is ours.
    expect: Option<Instant>,
}

/// A window as one snapshot shows it.
#[derive(Default)]
struct Win<'a> {
    /// Its panes (a linked window's panes are listed once per session it
    /// is in; here once).
    panes: Vec<&'a Pane>,
    /// The names of its sessions.
    sessions: Vec<String>,
    /// One of its sessions is pinned.
    pinned: bool,
    /// Marked `@home_sidebar_off`.
    off: bool,
}

/// Pure: windows already seen, panes already killed, sidebars' widths.
#[derive(Debug)]
pub struct Planner {
    stable: Duration,
    settle: Duration,
    /// The sidebar width: set by the daemon from the options and the state
    /// file, and by a width set by hand.
    width: Width,
    /// Auto-create is allowed (latched).
    allowed: bool,
    /// Windows seen; those seen before `allowed` are pre-existing.
    known: HashSet<String>,
    /// The hash of the windows and panes, and since when it holds.
    structure: Option<(u64, Instant)>,
    /// Kills issued (pane IDs are never reused): not issued again while a
    /// snapshot from before the kill is still the published one.
    killed: HashSet<String>,
    /// tmux-home sidebars' widths, by pane.
    track: HashMap<String, Track>,
    /// Windows last seen with a sidebar.
    had: HashSet<String>,
    /// Pinned windows whose sidebar went: left without one (until their
    /// `@home_sidebar_off` mark, which says the same, shows).
    declined: HashSet<String>,
    /// Pinned windows given a sidebar, and when (not again before it shows).
    adding: HashMap<String, Instant>,
    /// Pinned sessions' names, by session ID.
    pinned: HashMap<String, String>,
}

impl Planner {
    pub fn new(stable: Duration) -> Planner {
        Planner {
            stable,
            settle: Duration::from_millis(700),
            width: sidebar::DEFAULT_WIDTH,
            allowed: false,
            known: HashSet::new(),
            structure: None,
            killed: HashSet::new(),
            track: HashMap::new(),
            had: HashSet::new(),
            declined: HashSet::new(),
            adding: HashMap::new(),
            pinned: HashMap::new(),
        }
    }

    /// How long a width set by hand must hold to count.
    pub fn settle(mut self, d: Duration) -> Planner {
        self.settle = d;
        self
    }

    pub fn allowed(&self) -> bool {
        self.allowed
    }

    pub fn width(&self) -> Width {
        self.width
    }

    /// The sidebar width, as configured (options, state file).
    pub fn set_width(&mut self, w: Width) {
        self.width = w;
    }

    /// Plans for snapshot `s` at `now`; `startup` is `Gate`'s verdict.
    /// Called on every snapshot and on a timer (stability and the width's
    /// settling need the clock).
    pub fn observe(&mut self, s: &Snapshot, now: Instant, startup: Startup) -> Plan {
        let mut plan = Plan::default();
        // window → its panes, sessions and marks, in snapshot order
        let mut windows: BTreeMap<&str, Win> = BTreeMap::new();
        let mut order: Vec<&str> = Vec::new();
        for w in &s.windows {
            let e = windows.entry(&w.id).or_insert_with(|| {
                order.push(&w.id);
                Win::default()
            });
            e.off |= w.sidebar_off;
            if let Some(sess) = s.sessions.iter().find(|x| x.id == w.session_id) {
                e.pinned |= sess.pinned;
                if !e.sessions.contains(&sess.name) {
                    e.sessions.push(sess.name.clone());
                }
            }
        }
        for p in &s.panes {
            if let Some(e) = windows.get_mut(p.window_id.as_str())
                && !e.panes.iter().any(|q| q.id == p.id)
            {
                e.panes.push(p);
            }
        }
        let mut h = DefaultHasher::new();
        for (w, win) in &windows {
            w.hash(&mut h);
            layout(&win.panes).hash(&mut h);
        }
        let h = h.finish();
        if self.structure.is_none_or(|(old, _)| old != h) {
            self.structure = Some((h, now));
        }
        let was_allowed = self.allowed;
        if !self.allowed
            && startup == Startup::Past
            && self
                .structure
                .is_some_and(|(_, since)| now.duration_since(since) >= self.stable)
        {
            self.allowed = true;
            plan.apply_pins = true;
        }
        self.sessions(s, &mut plan);
        // a sidebar's width set by hand, settled: (pane, percentage)
        let mut by_hand: Option<(String, u32)> = None;
        for wid in &order {
            let win = &windows[wid];
            let mut ours: Vec<&Pane> = win
                .panes
                .iter()
                .copied()
                .filter(|p| p.home_role == ROLE && !self.killed.contains(&p.id))
                .collect();
            let real = win.panes.iter().any(|p| p.role != ROLE);
            if !ours.is_empty() && !real {
                plan.kills.extend(ours.iter().map(|p| p.id.clone()));
                ours.clear();
            } else if ours.len() > 1 {
                ours.sort_by_key(|p| pane_num(&p.id));
                plan.kills.extend(ours[1..].iter().map(|p| p.id.clone()));
                ours.truncate(1);
                // the survivor back to the width, whatever it was
                self.track.remove(&ours[0].id);
            }
            let lay = layout(&win.panes);
            let span = rows(&win.panes);
            for p in ours {
                self.follow(p, lay, span, now, &mut plan, &mut by_hand);
            }
            if real {
                self.pin(wid, win, now, startup, &mut plan);
            }
            if self.known.insert(wid.to_string()) && was_allowed {
                plan.new_windows.push(NewWindow {
                    id: wid.to_string(),
                    sessions: win.sessions.clone(),
                    has_sidebar: win.panes.iter().any(|p| p.role == ROLE),
                });
            }
        }
        if let Some((from, pct)) = by_hand {
            self.width = Width::Pct(pct);
            plan.save_width = Some(pct);
            for (id, t) in self.track.iter_mut() {
                let cols = self.width.cols(t.window_width);
                if *id == from || cols == t.width || plan.kills.contains(id) {
                    continue;
                }
                plan.resize.retain(|(p, _)| p != id);
                plan.resize.push((id.clone(), cols));
                t.expect = Some(now);
            }
            plan.resize.sort_by_key(|(p, _)| pane_num(p));
        }
        // window IDs are never reused: forget the ones gone
        self.known.retain(|w| windows.contains_key(w.as_str()));
        self.had.retain(|w| windows.contains_key(w.as_str()));
        self.declined.retain(|w| windows.contains_key(w.as_str()));
        self.adding.retain(|w, _| windows.contains_key(w.as_str()));
        let live: HashSet<&str> = s.panes.iter().map(|p| p.id.as_str()).collect();
        self.killed.retain(|p| live.contains(p.as_str()));
        self.track.retain(|p, _| live.contains(p.as_str()));
        self.killed.extend(plan.kills.iter().cloned());
        plan
    }

    /// Follows pinned sessions' names: renames, and kills.
    fn sessions(&mut self, s: &Snapshot, plan: &mut Plan) {
        for sess in &s.sessions {
            if !sess.pinned {
                self.pinned.remove(&sess.id);
            } else if let Some(old) = self.pinned.insert(sess.id.clone(), sess.name.clone())
                && old != sess.name
            {
                plan.pin_renames.push((old, sess.name.clone()));
            }
        }
        // no sessions at all: the server is going (or a glitch), not a kill
        if !s.sessions.is_empty() {
            self.pinned.retain(|id, name| {
                let live = s.sessions.iter().any(|x| &x.id == id);
                if !live {
                    plan.pins_gone.push(name.clone());
                }
                live
            });
            plan.pins_gone.sort();
        }
    }

    /// Follows tmux-home sidebar `p`'s width (`lay`: its window's panes;
    /// `span`: the rows they cover, `rows`).
    fn follow(
        &mut self,
        p: &Pane,
        lay: u64,
        span: (u32, u32),
        now: Instant,
        plan: &mut Plan,
        by_hand: &mut Option<(String, u32)>,
    ) {
        let (w, ww) = (p.width, p.window_width);
        if ww > 0 && w >= ww {
            return; // zoomed: its width says nothing
        }
        let target = self.width.cols(ww);
        let Some(t) = self.track.get_mut(&p.id) else {
            // first seen (a restored sidebar has kept its saved width)
            let mut t = Track {
                width: w,
                window_width: ww,
                layout: lay,
                settled: w,
                moved: None,
                expect: None,
            };
            if target != w {
                plan.resize.push((p.id.clone(), target));
                t.expect = Some(now);
            }
            self.track.insert(p.id.clone(), t);
            return;
        };
        if t.window_width != ww || t.layout != lay {
            // the window resized, or panes came or went: back to the width
            *t = Track {
                width: w,
                window_width: ww,
                layout: lay,
                settled: w,
                moved: None,
                expect: None,
            };
            if target != w {
                plan.resize.push((p.id.clone(), target));
                t.expect = Some(now);
            }
        } else if let Some(at) = t.expect {
            // our resize: its first change is it (as far as tmux could)
            if w != t.width {
                t.width = w;
                t.settled = w;
                t.expect = None;
            } else if now.duration_since(at) >= EXPECT_FOR {
                t.expect = None;
            }
        } else if w != t.width {
            t.width = w;
            t.moved = Some(now);
        } else if let Some(since) = t.moved
            && now.duration_since(since) >= self.settle
        {
            t.moved = None;
            // still a sidebar's shape (full height, at an edge, at most
            // half the window): not a layout applied over it. Full height
            // is every row the window's panes cover, not the window's
            // height: a pane border status line takes a row of that.
            let full = p.top <= span.0 && p.top + p.height >= span.1;
            let shaped = full && (p.left == 0 || p.left + w >= ww) && w * 2 <= ww;
            if ww > 0 && shaped && w.abs_diff(t.settled) > TOLERANCE {
                *by_hand = Some((p.id.clone(), pct(w, ww)));
            }
            t.settled = w;
        }
    }

    /// A window (with a real pane) of a pinned session that lacks a
    /// sidebar gets one, unless it lost it while pinned (see the module
    /// docs).
    fn pin(&mut self, wid: &str, win: &Win, now: Instant, startup: Startup, plan: &mut Plan) {
        let has = win
            .panes
            .iter()
            .any(|p| p.role == ROLE && !self.killed.contains(&p.id) && !plan.kills.contains(&p.id));
        if has {
            self.had.insert(wid.to_string());
            self.declined.remove(wid);
            self.adding.remove(wid);
            return;
        }
        let lost = self.had.remove(wid);
        if !win.pinned || win.off || self.declined.contains(wid) {
            return;
        }
        if lost {
            // closed while pinned, by whatever means: on purpose
            self.declined.insert(wid.to_string());
            plan.mark_off.push(wid.to_string());
        } else if startup != Startup::Restoring
            && self
                .adding
                .get(wid)
                .is_none_or(|at| now.duration_since(*at) >= RETRY_ADD)
        {
            self.adding.insert(wid.to_string(), now);
            plan.pin_adds.push(wid.to_string());
        }
    }
}

/// A hash of `panes`' IDs, in any order.
fn layout(panes: &[&Pane]) -> u64 {
    let mut ids: Vec<&str> = panes.iter().map(|p| p.id.as_str()).collect();
    ids.sort();
    let mut h = DefaultHasher::new();
    ids.hash(&mut h);
    h.finish()
}

/// The rows `panes` cover, `(top, bottom)` (bottom exclusive): the
/// window's height less any pane border status line.
fn rows(panes: &[&Pane]) -> (u32, u32) {
    let top = panes.iter().map(|p| p.top).min().unwrap_or(0);
    let bottom = panes.iter().map(|p| p.top + p.height).max().unwrap_or(0);
    (top, bottom)
}

/// The server's start time (`#{start_time}`, Unix seconds).
fn server_start(t: &Tmux) -> Option<u64> {
    let out = t.run(&["display-message", "-p", "#{start_time}"]).ok()?;
    out.trim().parse().ok()
}

/// `Gate::startup`, reading the server's age and options (restoring while
/// its start time is unknown).
fn startup(t: &Tmux, gate: &Gate, start: Option<u64>) -> Startup {
    let Some(start) = start else {
        return Startup::Restoring;
    };
    let age = Duration::from_secs(crate::agent::now().saturating_sub(start));
    let done = !sidebar::global_option(t, "@home_restore_done")
        .trim()
        .is_empty();
    gate.startup(age, done)
}

/// Carries out a plan (blocking: tmux commands, the state files). Failures
/// are logged: a pane or window can go between the snapshot and the
/// command.
fn execute(t: &Tmux, plan: Plan) {
    for p in &plan.kills {
        if let Err(e) = t.run(&["kill-pane", "-t", p]) {
            eprintln!("tmux-home: sidebar cleanup: {e:#}");
        }
    }
    for (p, cols) in &plan.resize {
        let _ = t.run(&["resize-pane", "-t", p, "-x", &cols.to_string()]);
    }
    for w in &plan.mark_off {
        let _ = t.run(&["set-option", "-w", "-t", w, OFF_OPTION, "1"]);
    }
    let state = || State::for_socket(&t.socket);
    let log = |r: anyhow::Result<()>| {
        if let Err(e) = r {
            eprintln!("tmux-home: sidebar state: {e:#}");
        }
    };
    if let Some(pct) = plan.save_width {
        log(state().and_then(|s| s.set_width(pct)));
    }
    for (old, new) in &plan.pin_renames {
        log(state().and_then(|s| s.rename_pin(old, new)));
    }
    for name in &plan.pins_gone {
        log(state().and_then(|s| s.set_pin(name, false)));
    }
    if plan.apply_pins
        && let Ok(s) = state()
    {
        for name in s.pins() {
            // `=name:`, that session exactly (as a pane target, which
            // set-option takes); none by that name is no error
            let _ = t.run(&["set-option", "-t", &format!("={name}:"), PIN_OPTION, "1"]);
        }
    }
    let mut adds = if plan.new_windows.is_empty() {
        Vec::new()
    } else {
        Auto::from_options(
            &sidebar::global_option(t, "@home-sidebar-auto"),
            &sidebar::global_option(t, "@home-sidebar-exclude"),
        )
        .creates(&plan.new_windows)
    };
    for w in plan.pin_adds {
        if !adds.contains(&w) {
            adds.push(w);
        }
    }
    if adds.is_empty() {
        return;
    }
    let place = Placement::read(t);
    let bin = sidebar::sidebar_bin();
    for w in adds {
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
            Err(e) => eprintln!("tmux-home: sidebar in {w}: {e:#}"),
        }
    }
}

/// How often the task re-plans without a new snapshot (stability, age,
/// a width settling).
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
        let mut planner = Planner::new(gate.stable).settle(settle_from_env());
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
            // the start-up phase (latched once allowed) and the width, as
            // configured now
            let allowed = planner.allowed();
            let read = {
                let t = t.clone();
                tokio::task::spawn_blocking(move || {
                    let phase = if allowed {
                        Startup::Past
                    } else {
                        startup(&t, &gate, start)
                    };
                    (phase, sidebar::width(&t))
                })
                .await
            };
            let Ok((phase, width)) = read else {
                continue;
            };
            planner.set_width(width);
            let plan = planner.observe(&snap, Instant::now(), phase);
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
            ..Default::default()
        }
    }

    fn snap(windows: &[(&str, &str)], panes: Vec<Pane>) -> Snapshot {
        Snapshot {
            sessions: vec![
                Session {
                    id: "$1".into(),
                    name: "main".into(),
                    attached: 1,
                    pinned: false,
                },
                Session {
                    id: "$2".into(),
                    name: "scratch".into(),
                    attached: 0,
                    pinned: false,
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
                    sidebar_off: false,
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

    fn r(p: &str, cols: u32) -> (String, u32) {
        (p.into(), cols)
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
        p.observe(s, t0, Startup::Past);
        p.observe(s, at(t0, 5000), Startup::Past);
        assert!(p.allowed());
        (p, at(t0, 5000))
    }

    #[test]
    fn the_gate_waits_for_age_and_for_the_restore() {
        use Startup::*;
        let g = Gate::default();
        let s = Duration::from_secs;
        assert_eq!(g.startup(s(10), true), Young);
        assert_eq!(g.startup(s(30), true), Past);
        assert_eq!(g.startup(s(30), false), Restoring, "not done");
        assert_eq!(g.startup(s(10), false), Restoring, "not done");
        assert_eq!(g.startup(s(60), false), Past, "held at most 60 s");
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
        assert!(
            p.observe(&one, t0, Startup::Restoring)
                .new_windows
                .is_empty()
        );
        assert!(
            p.observe(&two, at(t0, 9000), Startup::Restoring)
                .new_windows
                .is_empty()
        );
        // past start-up, but the windows changed 0 s ago: not yet
        assert!(
            p.observe(&two, at(t0, 9000), Startup::Past)
                .new_windows
                .is_empty()
        );
        assert!(!p.allowed());
        assert!(
            p.observe(&two, at(t0, 13_000), Startup::Past)
                .new_windows
                .is_empty()
        );
        assert!(!p.allowed(), "stable 4 s only");
        // stable for 5 s: allowed; @1 and @2 are pre-existing
        assert!(
            p.observe(&two, at(t0, 14_000), Startup::Past)
                .new_windows
                .is_empty()
        );
        assert!(p.allowed());
        let three = snap(
            &[("@1", "$1"), ("@2", "$1"), ("@3", "$1")],
            vec![real("%1", "@1"), real("%2", "@2"), real("%3", "@3")],
        );
        assert_eq!(
            ids(&p
                .observe(&three, at(t0, 14_100), Startup::Restoring)
                .new_windows),
            ["@3"],
            "allowed is latched"
        );
        assert!(
            p.observe(&three, at(t0, 15_000), Startup::Past)
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
            p.observe(&s, at(t0, i * 1000), Startup::Past);
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
        let new = p.observe(&s, now, Startup::Past).new_windows;
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
        assert_eq!(p.observe(&s, t0, Startup::Restoring).resize, [r("%2", 32)]);
        assert!(
            p.observe(&s, t0, Startup::Restoring).resize.is_empty(),
            "once"
        );
    }

    #[test]
    fn a_window_left_with_only_sidebars_has_ours_killed_once() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE);
        let s = snap(&[("@1", "$1")], vec![real("%1", "@1"), side("%2", "@1")]);
        p.observe(&s, t0, Startup::Restoring);
        assert_eq!(p.observe(&s, t0, Startup::Restoring), Plan::default());
        let gone = snap(&[("@1", "$1")], vec![side("%2", "@1")]);
        assert_eq!(p.observe(&gone, t0, Startup::Restoring).kills, ["%2"]);
        assert!(
            p.observe(&gone, t0, Startup::Restoring).kills.is_empty(),
            "not issued twice"
        );
        // tmux-agent-sidebar's panes are never ours to kill
        let theirs = snap(
            &[("@1", "$1")],
            vec![pane("%3", "@1", ROLE, false), side("%2", "@1")],
        );
        assert_eq!(
            Planner::new(STABLE)
                .observe(&theirs, t0, Startup::Restoring)
                .kills,
            ["%2"]
        );
        let only_theirs = snap(&[("@1", "$1")], vec![pane("%3", "@1", ROLE, false)]);
        assert!(
            Planner::new(STABLE)
                .observe(&only_theirs, t0, Startup::Restoring)
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
        let plan = Planner::new(STABLE).observe(&s, Instant::now(), Startup::Restoring);
        assert_eq!(plan.kills, ["%12"]);
        assert_eq!(plan.resize, [r("%9", 32)]);
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
        assert_eq!(
            p.observe(&s, Instant::now(), Startup::Restoring).resize,
            [r("%2", 32)]
        );
        assert_eq!(
            p.observe(&s, Instant::now(), Startup::Restoring),
            Plan::default()
        );
    }

    /// `snap`, with these sessions pinned and these windows marked off.
    fn marked(mut s: Snapshot, pinned: &[&str], off: &[&str]) -> Snapshot {
        for x in &mut s.sessions {
            x.pinned = pinned.contains(&x.id.as_str());
        }
        for w in &mut s.windows {
            w.sidebar_off = off.contains(&w.id.as_str());
        }
        s
    }

    #[test]
    fn a_pinned_sessions_windows_get_a_sidebar_at_once_except_mid_restore() {
        let t0 = Instant::now();
        let s = marked(
            snap(
                &[
                    ("@1", "$1"),
                    ("@2", "$1"),
                    ("@3", "$2"),
                    ("@4", "$2"),
                    ("@4", "$1"),
                ],
                vec![
                    real("%1", "@1"),
                    side("%9", "@1"),
                    real("%2", "@2"),
                    real("%3", "@3"),
                    real("%4", "@4"),
                ],
            ),
            &["$1"],
            &[],
        );
        let mut p = Planner::new(STABLE);
        assert!(
            p.observe(&s, t0, Startup::Restoring).pin_adds.is_empty(),
            "never mid-restore"
        );
        // young, not allowed: still at once; a window linked into the
        // pinned session counts, the other session's don't
        let plan = p.observe(&s, at(t0, 100), Startup::Young);
        assert_eq!(plan.pin_adds, ["@2", "@4"]);
        assert!(!p.allowed());
        assert!(
            p.observe(&s, at(t0, 600), Startup::Young)
                .pin_adds
                .is_empty(),
            "not again while it is on its way"
        );
        assert_eq!(
            p.observe(&s, at(t0, 10_200), Startup::Young).pin_adds,
            ["@2", "@4"],
            "but again if it never came"
        );
        // a window with only sidebars left is cleanup's, not the pin's
        let gone = marked(snap(&[("@1", "$1")], vec![side("%9", "@1")]), &["$1"], &[]);
        assert!(
            Planner::new(STABLE)
                .observe(&gone, t0, Startup::Past)
                .pin_adds
                .is_empty()
        );
    }

    #[test]
    fn a_pinned_window_closed_by_hand_keeps_none_until_it_has_one_again() {
        let t0 = Instant::now();
        let with = |off: &[&str]| {
            marked(
                snap(&[("@1", "$1")], vec![real("%1", "@1"), side("%2", "@1")]),
                &["$1"],
                off,
            )
        };
        let without =
            |off: &[&str]| marked(snap(&[("@1", "$1")], vec![real("%1", "@1")]), &["$1"], off);
        let mut p = Planner::new(STABLE);
        assert!(p.observe(&with(&[]), t0, Startup::Past).pin_adds.is_empty());
        let plan = p.observe(&without(&[]), t0, Startup::Past);
        assert_eq!(plan.mark_off, ["@1"], "closed: marked off");
        assert!(plan.pin_adds.is_empty(), "and not put back");
        // the mark not showing yet (a snapshot from before it)
        assert_eq!(p.observe(&without(&[]), t0, Startup::Past), Plan::default());
        assert_eq!(
            p.observe(&without(&["@1"]), t0, Startup::Past),
            Plan::default()
        );
        // re-pinned: the toggle clears the mark and adds one; closed again,
        // marked again
        p.observe(&with(&[]), t0, Startup::Past);
        assert_eq!(p.observe(&without(&[]), t0, Startup::Past).mark_off, ["@1"]);
        // a marked window (a daemon restart) is left alone
        let mut fresh = Planner::new(STABLE);
        assert_eq!(
            fresh.observe(&without(&["@1"]), t0, Startup::Past).pin_adds,
            Vec::<String>::new()
        );
        // unpinned (the mark goes before the sidebars do): nothing marked
        let mut p = Planner::new(STABLE);
        p.observe(&with(&[]), t0, Startup::Past);
        let unpinned = marked(snap(&[("@1", "$1")], vec![real("%1", "@1")]), &[], &[]);
        assert_eq!(p.observe(&unpinned, t0, Startup::Past), Plan::default());
    }

    #[test]
    fn pins_are_reapplied_once_auto_create_is_allowed() {
        let t0 = Instant::now();
        let s = snap(&[("@1", "$1")], vec![real("%1", "@1")]);
        let mut p = Planner::new(STABLE);
        assert!(!p.observe(&s, t0, Startup::Young).apply_pins);
        assert!(!p.observe(&s, at(t0, 9000), Startup::Restoring).apply_pins);
        assert!(p.observe(&s, at(t0, 9100), Startup::Past).apply_pins);
        assert!(
            !p.observe(&s, at(t0, 9200), Startup::Past).apply_pins,
            "once"
        );
    }

    #[test]
    fn a_pinned_sessions_rename_and_kill_reach_the_pins_file() {
        let t0 = Instant::now();
        let s = marked(
            snap(
                &[("@1", "$1"), ("@2", "$2")],
                vec![real("%1", "@1"), side("%3", "@1"), real("%2", "@2")],
            ),
            &["$1"],
            &[],
        );
        let mut p = Planner::new(STABLE);
        p.observe(&s, t0, Startup::Past);
        let mut renamed = s.clone();
        renamed.sessions[0].name = "home".into();
        assert_eq!(
            p.observe(&renamed, t0, Startup::Past).pin_renames,
            [("main".to_string(), "home".to_string())]
        );
        assert!(
            p.observe(&renamed, t0, Startup::Past)
                .pin_renames
                .is_empty()
        );
        // an unpinned session's rename is no business of the file's
        let mut other = renamed.clone();
        other.sessions[1].name = "scratch2".into();
        assert_eq!(p.observe(&other, t0, Startup::Past), Plan::default());
        // killed, others live on: out of the file
        let mut killed = other.clone();
        killed.sessions.remove(0);
        killed.windows.remove(0);
        killed.panes.retain(|p| p.window_id != "@1");
        assert_eq!(p.observe(&killed, t0, Startup::Past).pins_gone, ["home"]);
        // the server going (no sessions): not a kill
        let mut p = Planner::new(STABLE);
        p.observe(&s, t0, Startup::Past);
        assert!(
            p.observe(&Snapshot::default(), t0, Startup::Past)
                .pins_gone
                .is_empty()
        );
    }

    /// A sidebar `w` wide in a window `ww` wide.
    fn wide(id: &str, wid: &str, w: u32, ww: u32) -> Pane {
        let mut p = side(id, wid);
        p.width = w;
        p.window_width = ww;
        p
    }

    /// Two windows, each with a real pane and a sidebar: @1 (200 wide,
    /// sidebar %2 `a` wide) and @2 (100 wide, sidebar %4 `b` wide).
    fn two(a: u32, b: u32) -> Snapshot {
        two_wide(a, 200, b)
    }

    fn two_wide(a: u32, ww: u32, b: u32) -> Snapshot {
        snap(
            &[("@1", "$1"), ("@2", "$1")],
            vec![
                real("%1", "@1"),
                wide("%2", "@1", a, ww),
                real("%3", "@2"),
                wide("%4", "@2", b, 100),
            ],
        )
    }

    const SETTLE: Duration = Duration::from_millis(700);

    #[test]
    fn a_width_set_by_hand_settles_then_spreads_as_a_percentage() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE).settle(SETTLE);
        assert_eq!(p.observe(&two(32, 32), t0, Startup::Past), Plan::default());
        // dragged to 50 of 200: not yet
        assert_eq!(
            p.observe(&two(50, 32), at(t0, 100), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(50, 32), at(t0, 700), Startup::Past),
            Plan::default()
        );
        // held for 700 ms: 25%, stored, and the other sidebar to 25 of 100
        let plan = p.observe(&two(50, 32), at(t0, 800), Startup::Past);
        assert_eq!(plan.save_width, Some(25));
        assert_eq!(plan.resize, [r("%4", 25)]);
        assert_eq!(p.width(), Width::Pct(25));
        // our resize showing is not one by hand
        assert_eq!(
            p.observe(&two(50, 25), at(t0, 900), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(50, 25), at(t0, 3000), Startup::Past),
            Plan::default()
        );
    }

    #[test]
    fn a_drag_in_progress_and_a_column_of_rounding_are_left_alone() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE).settle(SETTLE);
        p.observe(&two(32, 32), t0, Startup::Past);
        for (i, w) in [40, 48, 55, 61, 66].iter().enumerate() {
            let now = at(t0, 500 * (i as u64 + 1));
            assert_eq!(
                p.observe(&two(*w, 32), now, Startup::Past),
                Plan::default(),
                "{w}"
            );
        }
        assert_eq!(
            p.observe(&two(66, 32), at(t0, 3300), Startup::Past)
                .save_width,
            Some(33),
            "the drag over"
        );
        // a layout applied over it (even-horizontal): half the window or
        // more, or no longer full height, is not a width to learn
        let mut p = Planner::new(STABLE).settle(SETTLE);
        p.observe(&two(32, 32), t0, Startup::Past);
        p.observe(&two(100, 32), at(t0, 100), Startup::Past);
        assert_eq!(
            p.observe(&two(101, 32), at(t0, 200), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(101, 32), at(t0, 2000), Startup::Past),
            Plan::default()
        );
        let mut short = two(60, 32);
        short.panes[0].height = 50;
        short.panes[1].height = 20;
        for p in &mut short.panes[..2] {
            p.window_height = 50;
        }
        p.observe(&short, at(t0, 2100), Startup::Past);
        assert_eq!(
            p.observe(&short, at(t0, 4000), Startup::Past),
            Plan::default()
        );
        // one column off what settled: tmux's rounding, nothing to learn
        let mut p = Planner::new(STABLE).settle(SETTLE);
        p.observe(&two(32, 32), t0, Startup::Past);
        p.observe(&two(33, 32), at(t0, 100), Startup::Past);
        assert_eq!(
            p.observe(&two(33, 32), at(t0, 2000), Startup::Past),
            Plan::default()
        );
    }

    /// `pane-border-status top` (or `bottom`) takes a row of the window:
    /// a sidebar is then one row short of the window's height, but still
    /// every row its panes cover, so a width set by hand counts.
    #[test]
    fn a_pane_border_status_row_does_not_stop_a_width_set_by_hand() {
        for top in [1, 0] {
            let bordered = |a: u32| {
                let mut s = two(a, 32);
                for p in &mut s.panes {
                    p.top = top;
                    p.height = 49;
                    p.window_height = 50;
                }
                s
            };
            let t0 = Instant::now();
            let mut p = Planner::new(STABLE).settle(SETTLE);
            p.observe(&bordered(32), t0, Startup::Past);
            p.observe(&bordered(50), at(t0, 100), Startup::Past);
            let plan = p.observe(&bordered(50), at(t0, 900), Startup::Past);
            assert_eq!(plan.save_width, Some(25), "top {top}");
            assert_eq!(plan.resize, [r("%4", 25)], "top {top}");
        }
    }

    #[test]
    fn a_window_resize_or_a_pane_closing_restores_the_width_not_learns_it() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE).settle(SETTLE);
        p.set_width(Width::Pct(20));
        assert_eq!(
            p.observe(&two_wide(40, 200, 20), t0, Startup::Past),
            Plan::default()
        );
        // the window narrowed to 150, tmux scaling the sidebar to 35
        let plan = p.observe(&two_wide(35, 150, 20), at(t0, 100), Startup::Past);
        assert_eq!(plan.resize, [r("%2", 30)]);
        assert_eq!(plan.save_width, None);
        p.observe(&two_wide(30, 150, 20), at(t0, 200), Startup::Past);
        assert_eq!(
            p.observe(&two_wide(30, 150, 20), at(t0, 2000), Startup::Past),
            Plan::default()
        );
        // tmux scaled it to the width already: nothing to do
        assert_eq!(
            p.observe(&two_wide(40, 200, 20), at(t0, 2100), Startup::Past),
            Plan::default()
        );
        // a pane beside the sidebar killed hands it its space: given back
        let mut s = two_wide(40, 200, 20);
        s.panes.push(real("%7", "@1"));
        p.observe(&s, at(t0, 2200), Startup::Past);
        let plan = p.observe(&two_wide(116, 200, 20), at(t0, 2300), Startup::Past);
        assert_eq!(plan.resize, [r("%2", 40)]);
        assert_eq!(plan.save_width, None);
    }

    #[test]
    fn our_own_resizes_are_never_taken_for_one_by_hand() {
        let t0 = Instant::now();
        let mut p = Planner::new(STABLE).settle(SETTLE);
        // first seen at 50 (restored): to 32
        assert_eq!(
            p.observe(&two(50, 32), t0, Startup::Past).resize,
            [r("%2", 32)]
        );
        // a snapshot from before the resize, then the resize
        assert_eq!(
            p.observe(&two(50, 32), at(t0, 100), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(32, 32), at(t0, 200), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(32, 32), at(t0, 2000), Startup::Past),
            Plan::default()
        );
        // tmux could only do part of it (a narrow window): still ours
        let mut p = Planner::new(STABLE).settle(SETTLE);
        p.observe(&two(50, 32), t0, Startup::Past);
        assert_eq!(
            p.observe(&two(40, 32), at(t0, 100), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(40, 32), at(t0, 2000), Startup::Past),
            Plan::default()
        );
        // a zoomed sidebar fills its window: its width says nothing
        let mut p = Planner::new(STABLE).settle(SETTLE);
        p.observe(&two(32, 32), t0, Startup::Past);
        p.observe(&two(200, 32), at(t0, 100), Startup::Past);
        assert_eq!(
            p.observe(&two(200, 32), at(t0, 2000), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(32, 32), at(t0, 2100), Startup::Past),
            Plan::default()
        );
        assert_eq!(
            p.observe(&two(32, 32), at(t0, 4000), Startup::Past),
            Plan::default()
        );
    }
}
