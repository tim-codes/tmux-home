//! `tmux-home sidebar` (spec §8): a narrow pane that shows what needs you
//! across the server and the windows of its own session, live from the
//! daemon's snapshots (a direct read every second without one). Focused,
//! it takes the popup's movement keys: `↑`/`↓` (`^p`/`^n`, `k`/`j`) move a
//! selection, `⏎` switches to the selected window, `Esc` hands the focus
//! back. `tmux-home sidebar-toggle` adds and removes these panes.
//!
//! A sidebar pane is marked `@home_role=sidebar`: by whoever creates it
//! (the toggle, the daemon's auto-create) and by the sidebar process itself
//! at start, so a pane tmux-resurrect restores (a shell it runs `tmux-home
//! sidebar` in, from `@resurrect-processes`) is a sidebar again too.
//! Everything that walks windows (the popup's rows, `^x`'s close check,
//! `^t`'s reopen snapshot, the git task) skips sidebar panes.

pub mod model;
pub mod state;
pub mod view;

use crate::{
    client,
    ops::Tx,
    popup::{fresh::Stamp, spawn_feed},
    tmux::Tmux,
};
use model::{Entry, Sel};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::TryRecvError,
    time::Duration,
};

/// The role value that marks a sidebar pane.
pub const ROLE: &str = "sidebar";
/// Default sidebar width (`@home-sidebar-width`).
pub const DEFAULT_WIDTH: Width = Width::Cols(32);
/// The session option that pins a session's sidebar (`sidebar-toggle
/// --pin`); mirrors the state dir's `pins` file.
pub const PIN_OPTION: &str = "@home_sidebar_pin";
/// The window option that says its sidebar was closed on purpose (the
/// window toggle, `q`): a pinned session leaves the window without one.
pub const OFF_OPTION: &str = "@home_sidebar_off";

/// A sidebar width: columns, or a percentage of its window's width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    Cols(u32),
    Pct(u32),
}

impl Width {
    /// `32` or `20%`; zero, a percentage over 99 or anything else is none.
    pub fn parse(s: &str) -> Option<Width> {
        let s = s.trim();
        let (digits, pct) = match s.strip_suffix('%') {
            Some(d) => (d, true),
            None => (s, false),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: u32 = digits.parse().ok()?;
        match (n, pct) {
            (0, _) => None,
            (n, true) if n > 99 => None,
            (n, true) => Some(Width::Pct(n)),
            (n, false) => Some(Width::Cols(n)),
        }
    }

    /// Columns in a window `window_width` wide: a percentage is rounded to
    /// the nearest column, at least one.
    pub fn cols(self, window_width: u32) -> u32 {
        match self {
            Width::Cols(n) => n,
            Width::Pct(p) => ((window_width * p + 50) / 100).max(1),
        }
    }
}

impl std::fmt::Display for Width {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Width::Cols(n) => write!(f, "{n}"),
            Width::Pct(p) => write!(f, "{p}%"),
        }
    }
}

/// Where and how wide new sidebars are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub width: Width,
    pub left: bool,
}

impl Default for Placement {
    fn default() -> Self {
        Placement {
            width: DEFAULT_WIDTH,
            left: true,
        }
    }
}

impl Placement {
    /// From `@home-sidebar-width` and `@home-sidebar-side` (their values;
    /// empty when unset). A width that isn't a number or a percentage, or
    /// a side other than `right`, falls back to the default.
    pub fn from_options(width: &str, side: &str) -> Placement {
        Placement {
            width: Width::parse(width).unwrap_or(DEFAULT_WIDTH),
            left: side.trim() != "right",
        }
    }

    /// The options, with the width set by hand (the state dir's
    /// `sidebar-width`) over `@home-sidebar-width`.
    pub fn read(t: &Tmux) -> Placement {
        Placement {
            width: width(t),
            left: global_option(t, "@home-sidebar-side").trim() != "right",
        }
    }
}

/// The width sidebars get: the one set by hand (`sidebar-width` in the
/// state dir), else `@home-sidebar-width`, else the default.
pub fn width(t: &Tmux) -> Width {
    state::State::for_socket(&t.socket)
        .ok()
        .and_then(|s| s.width())
        .map(Width::Pct)
        .unwrap_or_else(|| {
            Width::parse(&global_option(t, "@home-sidebar-width")).unwrap_or(DEFAULT_WIDTH)
        })
}

/// A global option's value; empty when unset or unreadable.
pub fn global_option(t: &Tmux, name: &str) -> String {
    t.run(&["show-options", "-gqv", name])
        .map(|s| s.trim_end_matches('\n').to_string())
        .unwrap_or_default()
}

/// The binary a new sidebar pane runs: `TMUX_HOME_BIN` (tests), else this
/// executable.
pub fn sidebar_bin() -> PathBuf {
    std::env::var_os("TMUX_HOME_BIN")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("tmux-home"))
}

/// `split-window` arguments that add a sidebar to `window`: full height,
/// at its left (or right) edge, without taking focus, printing the new
/// pane's ID.
///
/// The pane runs `/bin/sh -c '"$0" sidebar --managed; exit' <bin>`, not the binary
/// itself: tmux-resurrect saves a pane's command as the full command line
/// of the pane process's *children* (`ps -ao ppid,args`, its default `ps`
/// strategy), so a binary that was the pane process would be saved as
/// nothing. With the shell in between it is saved as `<bin> sidebar
/// --managed`, which `"~tmux-home sidebar"` in `@resurrect-processes`
/// matches (and restores managed: marked, `q` closes the pane). The
/// `; exit` keeps `sh` from exec'ing the binary in its place.
pub fn split_args(window: &str, p: &Placement, bin: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["split-window", "-d", "-f", "-h"].map(String::from).into();
    if p.left {
        a.push("-b".into());
    }
    let width = p.width.to_string();
    for s in ["-l", &width, "-t", window, "-P", "-F", "#{pane_id}"] {
        a.push(s.into());
    }
    a.push("/bin/sh".into());
    a.push("-c".into());
    a.push("\"$0\" sidebar --managed; exit".into());
    a.push(bin.to_string_lossy().into_owned());
    a
}

/// Adds a sidebar to `window`, marks it, and returns its pane ID. A
/// percentage is turned into columns here, rounded as the daemon rounds
/// it, so the daemon finds the new sidebar already at its width.
pub fn add(t: &Tmux, window: &str, p: &Placement, bin: &Path) -> anyhow::Result<String> {
    let mut p = p.clone();
    if let Width::Pct(_) = p.width
        && let Some(ww) = t
            .run(&["display-message", "-p", "-t", window, "#{window_width}"])
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    {
        p.width = Width::Cols(p.width.cols(ww));
    }
    let args = split_args(window, &p, bin);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let pane = t.run(&args)?.trim().to_string();
    anyhow::ensure!(pane.starts_with('%'), "split-window printed {pane:?}");
    mark(t, &pane)?;
    Ok(pane)
}

/// Marks `pane` as a sidebar.
pub fn mark(t: &Tmux, pane: &str) -> anyhow::Result<()> {
    t.run(&["set-option", "-p", "-t", pane, "@home_role", ROLE])?;
    Ok(())
}

/// The panes of `target` (`list-panes -s` for a session, else a window):
/// (window ID, pane ID, is a tmux-home sidebar).
fn panes(t: &Tmux, target: &str, session: bool) -> anyhow::Result<Vec<(String, String, bool)>> {
    let mut args = vec!["list-panes"];
    if session {
        args.push("-s");
    }
    args.extend([
        "-t",
        target,
        "-F",
        "#{window_id}\x1f#{pane_id}\x1f#{@home_role}",
    ]);
    Ok(t.run(&args)?
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\x1f');
            Some((
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()? == ROLE,
            ))
        })
        .collect())
}

/// What a toggle does, decided from the panes it covers.
#[derive(Debug, PartialEq, Eq)]
pub enum Toggle {
    /// Add a sidebar to each of these windows.
    Add(Vec<String>),
    /// Kill these sidebar panes.
    Remove(Vec<String>),
}

/// Toggle over `panes` (window, pane, is a sidebar), in window order: on
/// in every window that lacks a sidebar if any does, else off everywhere.
/// For one window that is simply "add one, or remove it".
pub fn decide(panes: &[(String, String, bool)]) -> Toggle {
    let mut windows: Vec<&str> = Vec::new();
    for (w, _, _) in panes {
        if !windows.contains(&w.as_str()) {
            windows.push(w);
        }
    }
    let lacking: Vec<String> = windows
        .iter()
        .filter(|w| !panes.iter().any(|(pw, _, s)| pw == *w && *s))
        .map(|w| w.to_string())
        .collect();
    if lacking.is_empty() {
        Toggle::Remove(
            panes
                .iter()
                .filter(|(_, _, s)| *s)
                .map(|(_, p, _)| p.clone())
                .collect(),
        )
    } else {
        Toggle::Add(lacking)
    }
}

/// The sidebar panes a pin toggle turning off kills: those of
/// `session_panes` (window, pane, is a tmux-home sidebar) in windows that
/// no other pinned session holds (`pinned_elsewhere`: a linked window keeps
/// its sidebar while any of its sessions is pinned).
pub fn unpin_kills(
    session_panes: &[(String, String, bool)],
    pinned_elsewhere: &[String],
) -> Vec<String> {
    session_panes
        .iter()
        .filter(|(w, _, s)| *s && !pinned_elsewhere.contains(w))
        .map(|(_, p, _)| p.clone())
        .collect()
}

/// `tmux-home sidebar-toggle [--pin] [--window <target>]`: toggles a
/// sidebar in the target window (default: the one `$TMUX_PANE` is in), or
/// with `--pin` (`--session`) pins or unpins that window's session (see
/// `toggle_pin`). Never touches another session.
pub fn toggle(socket: Option<PathBuf>, pin: bool, window: Option<String>) -> anyhow::Result<()> {
    let socket = client::current_socket(socket)
        .ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let t = Tmux::new(socket);
    let target = match window {
        Some(w) => w,
        None => std::env::var("TMUX_PANE")
            .ok()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow::anyhow!("no --window and no $TMUX_PANE"))?,
    };
    let ids = t.run(&[
        "display-message",
        "-p",
        "-t",
        &target,
        "#{session_id}\x1f#{window_id}\x1f#{session_name}",
    ])?;
    let mut f = ids.trim_end_matches('\n').splitn(3, '\x1f');
    let (Some(sid), Some(wid), Some(name)) = (f.next(), f.next(), f.next()) else {
        anyhow::bail!("no window {target:?}");
    };
    if pin {
        toggle_pin(&t, sid, name)
    } else {
        toggle_window(&t, wid)
    }
}

/// The window toggle: adds a sidebar to `wid`, or removes it. Removing
/// marks the window `@home_sidebar_off` (before the kill, so the daemon
/// never sees it lacking one unmarked): a pinned session doesn't put it
/// back. Adding clears the mark.
fn toggle_window(t: &Tmux, wid: &str) -> anyhow::Result<()> {
    match decide(&panes(t, wid, false)?) {
        Toggle::Remove(ps) => {
            let _ = t.run(&["set-option", "-w", "-t", wid, OFF_OPTION, "1"]);
            for p in ps {
                let _ = t.run(&["kill-pane", "-t", &p]);
            }
        }
        Toggle::Add(ws) => {
            let _ = t.run(&["set-option", "-w", "-u", "-t", wid, OFF_OPTION]);
            let place = Placement::read(t);
            let bin = sidebar_bin();
            for w in ws {
                add(t, &w, &place, &bin)?;
            }
        }
    }
    Ok(())
}

/// The pin toggle for session `sid` (named `name`).
///
/// Pin: its name goes into the state dir's `pins`, every window of it loses
/// its `@home_sidebar_off` mark and gets a sidebar if it lacks one, and
/// then the session is marked `@home_sidebar_pin` (last, so the daemon
/// finds the sidebars already there). From then on the daemon gives its new
/// windows one too.
///
/// Unpin: the mark goes first (so the daemon never sees a pinned window
/// lose its sidebar), then the name, then every tmux-home sidebar of the
/// session, except in a window linked into another pinned session.
fn toggle_pin(t: &Tmux, sid: &str, name: &str) -> anyhow::Result<()> {
    let pinned = t
        .run(&["show-options", "-qv", "-t", sid, PIN_OPTION])
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0"
        })
        .unwrap_or(false);
    let state = state::State::for_socket(&t.socket);
    let covered = panes(t, sid, true)?;
    let mut windows: Vec<&str> = Vec::new();
    for (w, _, _) in &covered {
        if !windows.contains(&w.as_str()) {
            windows.push(w);
        }
    }
    if pinned {
        t.run(&["set-option", "-u", "-t", sid, PIN_OPTION])?;
        if let Err(e) = state.and_then(|s| s.set_pin(name, false)) {
            eprintln!("tmux-home: sidebar pins: {e:#}");
        }
        // windows held by another pinned session, by window ID
        let elsewhere: Vec<String> = t
            .run(&[
                "list-windows",
                "-a",
                "-F",
                &format!("#{{window_id}}\x1f#{{session_id}}\x1f#{{{PIN_OPTION}}}"),
            ])?
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\x1f');
                let (w, s, p) = (f.next()?, f.next()?, f.next()?);
                (s != sid && !p.is_empty() && p != "0").then(|| w.to_string())
            })
            .collect();
        for w in &windows {
            if !elsewhere.iter().any(|e| e == w) {
                let _ = t.run(&["set-option", "-w", "-u", "-t", w, OFF_OPTION]);
            }
        }
        for p in unpin_kills(&covered, &elsewhere) {
            let _ = t.run(&["kill-pane", "-t", &p]);
        }
    } else {
        if let Err(e) = state.and_then(|s| s.set_pin(name, true)) {
            eprintln!("tmux-home: sidebar pins: {e:#}");
        }
        for w in &windows {
            let _ = t.run(&["set-option", "-w", "-u", "-t", w, OFF_OPTION]);
        }
        if let Toggle::Add(ws) = decide(&covered) {
            let place = Placement::read(t);
            let bin = sidebar_bin();
            for w in ws {
                add(t, &w, &place, &bin)?;
            }
        }
        t.run(&["set-option", "-t", sid, PIN_OPTION, "1"])?;
    }
    Ok(())
}

/// How often the sidebar looks for input and snapshots.
const TICK: Duration = Duration::from_millis(100);

/// Seconds after the server starts before a sidebar starts a daemon
/// (`TMUX_HOME_SIDEBAR_REVIVE_AGE`, default 15): restored sidebars start
/// early in a restore, and the plugin starts the daemon later on purpose.
pub fn revive_age() -> u64 {
    std::env::var("TMUX_HOME_SIDEBAR_REVIVE_AGE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(15)
}

/// What a key does in a focused sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    /// Switch to the selected window.
    Go,
    /// Hand the focus back to the window's last pane.
    Back,
    /// Close the sidebar.
    Quit,
}

/// The popup's movement keys: `↑`/`^p`/`k`, `↓`/`^n`/`j`, `⏎`, `Esc`; and
/// `q`. Everything else (and every key release) is ignored.
pub fn key(k: &KeyEvent) -> Option<Key> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let plain = !k
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    match k.code {
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Enter => Some(Key::Go),
        KeyCode::Esc => Some(Key::Back),
        KeyCode::Char('p') if ctrl => Some(Key::Up),
        KeyCode::Char('n') if ctrl => Some(Key::Down),
        KeyCode::Char('k') if plain => Some(Key::Up),
        KeyCode::Char('j') if plain => Some(Key::Down),
        KeyCode::Char('q') if plain => Some(Key::Quit),
        _ => None,
    }
}

/// Hands the focus in the sidebar's window back: its last pane, else its
/// first pane that isn't a sidebar.
fn give_back(t: &Tmux, me: &str) {
    if t.run(&["last-pane", "-t", me]).is_ok() {
        return;
    }
    if let Ok(ps) = panes(t, me, false)
        && let Some((_, p, _)) = ps.iter().find(|(_, p, side)| !side && p != me)
    {
        let _ = t.run(&["select-pane", "-t", p]);
    }
}

/// The client to switch: the one showing the sidebar's session that was
/// active last (the one whose keys just reached us).
fn client_of(t: &Tmux, me: &str) -> Option<String> {
    let sid = t
        .run(&["display-message", "-p", "-t", me, "#{session_id}"])
        .ok()?;
    let out = t
        .run(&[
            "list-clients",
            "-t",
            sid.trim(),
            "-F",
            "#{client_activity}\x1f#{client_name}",
        ])
        .ok()?;
    out.lines()
        .filter_map(|l| {
            let (a, c) = l.split_once('\x1f')?;
            Some((a.parse::<u64>().unwrap_or(0), c.to_string()))
        })
        .max_by_key(|(a, _)| *a)
        .map(|(_, c)| c)
}

/// `⏎` on `e`: the sidebar's window gets its real pane back (so coming
/// back to it doesn't land in the sidebar), then the client goes to `e`'s
/// window.
fn go(t: &Tmux, socket: &Path, me: &str, e: &Entry) {
    give_back(t, me);
    let client = client_of(t, me);
    let _ = Tx::new(socket.to_path_buf()).switch_to(client.as_deref(), &e.sid, &e.wid);
}

/// `tmux-home sidebar [--managed]`: renders until `q` or until its server
/// goes. It never asks for the mouse. Focused, the movement keys select a
/// window, `⏎` switches to it and `Esc` hands the focus back (see the
/// module docs); the selection shows while the pane has the focus or a
/// key has moved it. `--managed` (the pane `sidebar-toggle` or
/// auto-create made, or that resurrect restored) marks its pane
/// `@home_role=sidebar`, and `q` then closes the pane; run by hand, it
/// marks nothing and `q` just exits.
pub fn run(socket: Option<PathBuf>, managed: bool) -> anyhow::Result<()> {
    let socket = client::current_socket(socket)
        .ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let me = std::env::var("TMUX_PANE")
        .ok()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow::anyhow!("not in a tmux pane ($TMUX_PANE is unset)"))?;
    let t = Tmux::new(socket.clone());
    if managed {
        mark(&t, &me)?;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let start: Option<u64> = t
        .run(&["display-message", "-p", "#{start_time}"])
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let feed = spawn_feed(socket.clone(), "sidebar", start.map(|s| s + revive_age()));
    let mut term = ratatui::init();
    let result = (|| -> anyhow::Result<()> {
        let mut m = model::Model::default();
        let mut sel: Option<Sel> = None;
        let mut live = false;
        let mut dirty = true;
        loop {
            let mut latest = None;
            loop {
                match feed.try_recv() {
                    Ok(f) => latest = Some(f),
                    Err(TryRecvError::Empty) => break,
                    // the feed ends when the server has gone
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            if let Some(f) = latest {
                let now_live = matches!(f.stamp, Stamp::Live { .. });
                dirty |= now_live != live;
                live = now_live;
                let next = model::Model::build(&f.snap, &me, &home);
                // focus in: start on the sidebar's own window; focus out:
                // drop the selection (only on a change: a key may beat
                // the snapshot that shows the focus arriving)
                if next.focused && !m.focused && sel.is_none() {
                    sel = next.here();
                    dirty = true;
                } else if !next.focused && m.focused && sel.is_some() {
                    sel = None;
                    dirty = true;
                }
                dirty |= next != m;
                m = next;
            }
            if dirty {
                term.draw(|f| view::draw(f, &m, live, sel.as_ref()))?;
                dirty = false;
            }
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(k) => match key(&k) {
                        Some(Key::Up) => {
                            sel = m.step(sel.as_ref(), -1);
                            dirty = true;
                        }
                        Some(Key::Down) => {
                            sel = m.step(sel.as_ref(), 1);
                            dirty = true;
                        }
                        Some(Key::Go) => {
                            let s = sel.take().or_else(|| m.here());
                            if let Some(e) = s.as_ref().and_then(|s| m.entry(s)) {
                                go(&t, &socket, &me, e);
                            }
                            dirty = true;
                        }
                        Some(Key::Back) => {
                            sel = None;
                            give_back(&t, &me);
                            dirty = true;
                        }
                        Some(Key::Quit) => {
                            // the pane, not just this process: a pane
                            // resurrect restored runs us in a shell;
                            // closed on purpose, so a pinned session
                            // leaves its window without one
                            if managed {
                                let _ = t.run(&["set-option", "-w", "-t", &me, OFF_OPTION, "1"]);
                                let _ = t.run(&["kill-pane", "-t", &me]);
                            }
                            return Ok(());
                        }
                        None => {}
                    },
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(w: &str, id: &str, side: bool) -> (String, String, bool) {
        (w.into(), id.into(), side)
    }

    #[test]
    fn one_window_toggles() {
        assert_eq!(
            decide(&[p("@1", "%1", false)]),
            Toggle::Add(vec!["@1".into()])
        );
        assert_eq!(
            decide(&[p("@1", "%2", true), p("@1", "%1", false)]),
            Toggle::Remove(vec!["%2".into()])
        );
    }

    #[test]
    fn a_session_turns_on_where_any_window_lacks_one_else_off() {
        let some = [
            p("@1", "%1", false),
            p("@1", "%5", true),
            p("@2", "%2", false),
            p("@3", "%3", false),
        ];
        assert_eq!(decide(&some), Toggle::Add(vec!["@2".into(), "@3".into()]));
        let all = [
            p("@1", "%1", false),
            p("@1", "%5", true),
            p("@2", "%2", false),
            p("@2", "%6", true),
        ];
        assert_eq!(decide(&all), Toggle::Remove(vec!["%5".into(), "%6".into()]));
    }

    #[test]
    fn keys_are_the_popups_movement_keys() {
        let k = |code, mods| key(&KeyEvent::new(code, mods));
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(k(KeyCode::Up, none), Some(Key::Up));
        assert_eq!(k(KeyCode::Char('p'), ctrl), Some(Key::Up));
        assert_eq!(k(KeyCode::Char('k'), none), Some(Key::Up));
        assert_eq!(k(KeyCode::Down, none), Some(Key::Down));
        assert_eq!(k(KeyCode::Char('n'), ctrl), Some(Key::Down));
        assert_eq!(k(KeyCode::Char('j'), none), Some(Key::Down));
        assert_eq!(k(KeyCode::Enter, none), Some(Key::Go));
        assert_eq!(k(KeyCode::Esc, none), Some(Key::Back));
        assert_eq!(k(KeyCode::Char('q'), none), Some(Key::Quit));
        assert_eq!(k(KeyCode::Char('q'), ctrl), None);
        assert_eq!(k(KeyCode::Char('j'), KeyModifiers::ALT), None);
        assert_eq!(k(KeyCode::Char('x'), none), None);
        assert_eq!(k(KeyCode::Char('p'), none), None);
        let mut up = KeyEvent::new(KeyCode::Up, none);
        up.kind = KeyEventKind::Release;
        assert_eq!(key(&up), None);
    }

    #[test]
    fn placement_from_options() {
        assert_eq!(Placement::from_options("", ""), Placement::default());
        assert_eq!(
            Placement::from_options("40", "right"),
            Placement {
                width: Width::Cols(40),
                left: false
            }
        );
        assert_eq!(Placement::from_options("20%", "left").width, Width::Pct(20));
        for bad in ["x", "0", "-3", "%", "3 0", "0%", "100%"] {
            assert_eq!(
                Placement::from_options(bad, "").width,
                Width::Cols(32),
                "{bad}"
            );
        }
    }

    #[test]
    fn widths_in_columns() {
        assert_eq!(Width::Cols(32).cols(200), 32);
        assert_eq!(Width::Pct(20).cols(200), 40);
        assert_eq!(Width::Pct(23).cols(173), 40, "39.79 rounds up");
        assert_eq!(Width::Pct(23).cols(171), 39, "39.33 rounds down");
        assert_eq!(Width::Pct(1).cols(10), 1, "at least one");
        assert_eq!(Width::Pct(20).to_string(), "20%");
        assert_eq!(Width::Cols(32).to_string(), "32");
    }

    #[test]
    fn unpin_spares_windows_another_pinned_session_holds() {
        let ps = [
            p("@1", "%1", false),
            p("@1", "%5", true),
            p("@2", "%2", false),
            p("@2", "%6", true),
            p("@3", "%3", false),
        ];
        assert_eq!(unpin_kills(&ps, &[]), ["%5", "%6"]);
        assert_eq!(unpin_kills(&ps, &["@2".into()]), ["%5"]);
    }

    #[test]
    fn split_runs_the_binary_under_a_shell() {
        let a = split_args("@3", &Placement::default(), Path::new("/p/tmux-home"));
        assert_eq!(
            a,
            [
                "split-window",
                "-d",
                "-f",
                "-h",
                "-b",
                "-l",
                "32",
                "-t",
                "@3",
                "-P",
                "-F",
                "#{pane_id}",
                "/bin/sh",
                "-c",
                "\"$0\" sidebar --managed; exit",
                "/p/tmux-home"
            ]
        );
        let right = Placement {
            left: false,
            ..Placement::default()
        };
        assert!(!split_args("@3", &right, Path::new("x")).contains(&"-b".to_string()));
    }
}
