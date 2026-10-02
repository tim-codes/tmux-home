//! `tmux-home sidebar` (spec §8): a narrow, read-only pane that shows what
//! needs you across the server and the windows of its own session, live
//! from the daemon's snapshots (a direct read every second without one).
//! `tmux-home sidebar-toggle` adds and removes these panes.
//!
//! A sidebar pane is marked `@home_role=sidebar`: by whoever creates it
//! (the toggle, the daemon's auto-create) and by the sidebar process itself
//! at start, so a pane tmux-resurrect restores (a shell it runs `tmux-home
//! sidebar` in, from `@resurrect-processes`) is a sidebar again too.
//! Everything that walks windows (the popup's rows, `^x`'s close check,
//! `^t`'s reopen snapshot, the git task) skips sidebar panes.

pub mod model;
pub mod view;

use crate::{
    client,
    popup::{fresh::Stamp, spawn_feed},
    tmux::Tmux,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::TryRecvError,
    time::Duration,
};

/// The role value that marks a sidebar pane.
pub const ROLE: &str = "sidebar";
/// Default sidebar width (`@home-sidebar-width`).
pub const DEFAULT_WIDTH: &str = "32";

/// Where and how wide new sidebars are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// A `split-window -l` size: columns, or a percentage (`20%`).
    pub width: String,
    pub left: bool,
}

impl Default for Placement {
    fn default() -> Self {
        Placement {
            width: DEFAULT_WIDTH.into(),
            left: true,
        }
    }
}

impl Placement {
    /// From `@home-sidebar-width` and `@home-sidebar-side` (their values;
    /// empty when unset). A width that isn't a number or a percentage, or
    /// a side other than `right`, falls back to the default.
    pub fn from_options(width: &str, side: &str) -> Placement {
        let w = width.trim();
        let digits = w.strip_suffix('%').unwrap_or(w);
        let ok = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && digits != "0";
        Placement {
            width: if ok { w.into() } else { DEFAULT_WIDTH.into() },
            left: side.trim() != "right",
        }
    }

    pub fn read(t: &Tmux) -> Placement {
        Placement::from_options(
            &global_option(t, "@home-sidebar-width"),
            &global_option(t, "@home-sidebar-side"),
        )
    }
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
/// The pane runs `/bin/sh -c '"$0" sidebar; exit' <bin>`, not the binary
/// itself: tmux-resurrect saves a pane's command as the full command line
/// of the pane process's *children* (`ps -ao ppid,args`, its default `ps`
/// strategy), so a binary that was the pane process would be saved as
/// nothing. With the shell in between it is saved as `<bin> sidebar`,
/// which `"~tmux-home sidebar"` in `@resurrect-processes` matches. The
/// `; exit` keeps `sh` from exec'ing the binary in its place.
pub fn split_args(window: &str, p: &Placement, bin: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["split-window", "-d", "-f", "-h"].map(String::from).into();
    if p.left {
        a.push("-b".into());
    }
    for s in ["-l", &p.width, "-t", window, "-P", "-F", "#{pane_id}"] {
        a.push(s.into());
    }
    a.push("/bin/sh".into());
    a.push("-c".into());
    a.push("\"$0\" sidebar; exit".into());
    a.push(bin.to_string_lossy().into_owned());
    a
}

/// Adds a sidebar to `window`, marks it, and returns its pane ID.
pub fn add(t: &Tmux, window: &str, p: &Placement, bin: &Path) -> anyhow::Result<String> {
    let args = split_args(window, p, bin);
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

/// `tmux-home sidebar-toggle [--session] [--window <target>]`: toggles a
/// sidebar in the target window (default: the one `$TMUX_PANE` is in), or
/// with `--session` in every window of that window's session — on if any
/// window lacks one, else off. Never touches another session.
pub fn toggle(
    socket: Option<PathBuf>,
    session: bool,
    window: Option<String>,
) -> anyhow::Result<()> {
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
        "#{session_id}\x1f#{window_id}",
    ])?;
    let (sid, wid) = ids
        .trim_end()
        .split_once('\x1f')
        .ok_or_else(|| anyhow::anyhow!("no window {target:?}"))?;
    let covered = if session {
        panes(&t, sid, true)?
    } else {
        panes(&t, wid, false)?
    };
    match decide(&covered) {
        Toggle::Remove(ps) => {
            for p in ps {
                let _ = t.run(&["kill-pane", "-t", &p]);
            }
        }
        Toggle::Add(ws) => {
            let place = Placement::read(&t);
            let bin = sidebar_bin();
            for w in ws {
                add(&t, &w, &place, &bin)?;
            }
        }
    }
    Ok(())
}

/// How often the sidebar looks for input and snapshots.
const TICK: Duration = Duration::from_millis(100);

/// `tmux-home sidebar`: marks its own pane and renders until `q` (which
/// closes its pane) or until its server goes. It never asks for the mouse
/// and ignores every other key.
pub fn run(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let socket = client::current_socket(socket)
        .ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let me = std::env::var("TMUX_PANE")
        .ok()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow::anyhow!("not in a tmux pane ($TMUX_PANE is unset)"))?;
    let t = Tmux::new(socket.clone());
    mark(&t, &me)?;
    let home = std::env::var("HOME").unwrap_or_default();
    let feed = spawn_feed(socket, "sidebar");
    let mut term = ratatui::init();
    let result = (|| -> anyhow::Result<()> {
        let mut m = model::Model::default();
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
                dirty |= next != m;
                m = next;
            }
            if dirty {
                term.draw(|f| view::draw(f, &m, live))?;
                dirty = false;
            }
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(k)
                        if k.kind != KeyEventKind::Release
                            && k.code == KeyCode::Char('q')
                            && !k
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        // the pane, not just this process: a pane
                        // resurrect restored runs us in a shell
                        let _ = t.run(&["kill-pane", "-t", &me]);
                        return Ok(());
                    }
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
    fn placement_from_options() {
        assert_eq!(Placement::from_options("", ""), Placement::default());
        assert_eq!(
            Placement::from_options("40", "right"),
            Placement {
                width: "40".into(),
                left: false
            }
        );
        assert_eq!(Placement::from_options("20%", "left").width, "20%");
        for bad in ["x", "0", "-3", "%", "3 0"] {
            assert_eq!(Placement::from_options(bad, "").width, "32", "{bad}");
        }
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
                "\"$0\" sidebar; exit",
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
