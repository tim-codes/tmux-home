//! The daemon's autosave task: tmux-resurrect saves on an interval, in
//! place of tmux-continuum's (which rode on the status line, so it saved
//! only while a client was attached, and on a server whose restore never
//! ran just as on one whose had).
//!
//! - **When:** every `@home-save-interval` minutes (default 5; 0 = off),
//!   read on each tick, so a config reload applies. The time of the last
//!   autosave is the server option `@home_save_last` (Unix seconds), so a
//!   daemon restarted (a rebuild) keeps the rhythm. A server that has none
//!   gets "now" on the daemon's first look: the first save is an interval
//!   after the daemon starts, never at once (as continuum's).
//! - **Only a restored server:** nothing is saved until `@home_restore_done`
//!   is set (by resurrect's post-restore hook, or by the config when there
//!   was nothing to restore): until then the server's windows aren't the
//!   user's sessions, and a save would replace the snapshot a restore needs.
//! - **How:** `$(@resurrect-save-script-path) quiet` (resurrect sets that
//!   option; else `~/.tmux/plugins/tmux-resurrect/scripts/save.sh`), as
//!   continuum runs it, with `$TMUX` naming this server (the script and its
//!   hooks run bare `tmux`) and the tmux binary's directory first on `PATH`.
//! - **One at a time:** the task waits for each save (at most
//!   `SAVE_TIMEOUT`, then it is killed) before it looks again, so two saves
//!   of its own never overlap; ticks missed meanwhile are dropped.
//!
//! Saves, failures and skips go to `<state_dir>/autosave.log`.

use super::Shared;
use crate::tmux::Tmux;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Minutes between autosaves (default `DEFAULT_INTERVAL`; 0 = off).
pub const OPT_INTERVAL: &str = "@home-save-interval";
pub const DEFAULT_INTERVAL: u64 = 5;
/// Server option: when the last autosave started (Unix seconds).
pub const LAST: &str = "@home_save_last";
/// The log, in the state dir.
pub const LOG: &str = "autosave.log";
/// A save that takes longer is killed.
pub const SAVE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct Config {
    /// How often the task looks.
    pub tick: Duration,
    /// Seconds per `@home-save-interval` unit: 60 (tests: 1).
    pub unit: u64,
}

impl Default for Config {
    /// 15 s ticks, minutes; `TMUX_HOME_SAVE_TICK_MS` and
    /// `TMUX_HOME_SAVE_UNIT_SECS` override them (tests).
    fn default() -> Self {
        let env = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        Config {
            tick: env("TMUX_HOME_SAVE_TICK_MS")
                .map_or(Duration::from_secs(15), Duration::from_millis),
            unit: env("TMUX_HOME_SAVE_UNIT_SECS")
                .filter(|u| *u > 0)
                .unwrap_or(60),
        }
    }
}

/// `@home-save-interval`: whole minutes, at most a day; else the default.
pub fn parse_interval(v: &str) -> u64 {
    match v.trim() {
        "" => DEFAULT_INTERVAL,
        v => v
            .parse::<u64>()
            .map_or(DEFAULT_INTERVAL, |m| m.min(24 * 60)),
    }
}

/// What a tick does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// `@home-save-interval 0`.
    Off,
    /// No `@home_save_last` yet: start the clock now.
    Start,
    /// Not due yet.
    Wait,
    /// Due, but the server isn't done restoring.
    NotRestored,
    Save,
}

/// The decision at `now`, `interval_secs` apart (0 = off), the last save
/// at `last`.
pub fn decide(interval_secs: u64, restore_done: bool, last: Option<u64>, now: u64) -> Decision {
    if interval_secs == 0 {
        return Decision::Off;
    }
    let Some(last) = last else {
        return Decision::Start;
    };
    if now < last.saturating_add(interval_secs) {
        Decision::Wait
    } else if !restore_done {
        Decision::NotRestored
    } else {
        Decision::Save
    }
}

fn global(t: &Tmux, name: &str) -> String {
    t.run(&["show-options", "-gqv", name])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn log(socket: &Path, msg: &str) {
    crate::hook::append_log(socket, LOG, msg);
}

/// What one tick reads from the server.
struct Look {
    interval: u64,
    restore_done: bool,
    last: Option<u64>,
}

fn look(t: &Tmux) -> Look {
    Look {
        interval: parse_interval(&global(t, OPT_INTERVAL)),
        restore_done: !global(t, "@home_restore_done").is_empty(),
        last: global(t, LAST).parse().ok(),
    }
}

/// resurrect's save script: `@resurrect-save-script-path`, else the TPM
/// install's.
fn save_script(t: &Tmux) -> Option<PathBuf> {
    let p = global(t, "@resurrect-save-script-path");
    if !p.is_empty() {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".tmux/plugins/tmux-resurrect/scripts/save.sh"))
}

/// Runs one save; a line for the log.
async fn save(t: &Tmux) -> String {
    let (script, pid) = {
        let t = t.clone();
        match tokio::task::spawn_blocking(move || {
            (save_script(&t), t.run(&["display-message", "-p", "#{pid}"]))
        })
        .await
        {
            Ok(r) => r,
            Err(e) => return format!("autosave: {e}"),
        }
    };
    let Some(script) = script else {
        return "autosave: no save script (HOME unset)".into();
    };
    let pid = match pid {
        Ok(p) => p.trim().to_string(),
        Err(e) => return format!("autosave: {e:#}"),
    };
    let mut path = std::ffi::OsString::new();
    if let Some(dir) = crate::tmux::tmux_bin().parent().filter(|d| d.is_absolute()) {
        path.push(dir);
        path.push(":");
    }
    path.push(std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()));
    let mut c = tokio::process::Command::new(&script);
    c.arg("quiet")
        .env("TMUX", format!("{},{pid},0", t.socket.display()))
        .env("PATH", path)
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let t0 = Instant::now();
    match tokio::time::timeout(SAVE_TIMEOUT, c.output()).await {
        Err(_) => format!("autosave: {} timed out, killed", script.display()),
        Ok(Err(e)) => format!("autosave: {}: {e}", script.display()),
        Ok(Ok(out)) if out.status.success() => {
            format!("autosave: saved in {:.1}s", t0.elapsed().as_secs_f64())
        }
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            let err = &err[err.len().saturating_sub(200)..];
            format!("autosave: save failed ({}): {err}", out.status)
        }
    }
}

pub(super) fn spawn(shared: Arc<Shared>, cfg: Config) -> JoinHandle<()> {
    tokio::spawn(async move {
        let t = shared.tmux.clone();
        let socket = t.socket.clone();
        let mut tick = tokio::time::interval(cfg.tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // log a skip once per stretch of not being restored
        let mut skipped = false;
        loop {
            tick.tick().await;
            let l = {
                let t = t.clone();
                match tokio::task::spawn_blocking(move || look(&t)).await {
                    Ok(l) => l,
                    Err(_) => continue,
                }
            };
            let now = crate::agent::now();
            match decide(
                l.interval.saturating_mul(cfg.unit),
                l.restore_done,
                l.last,
                now,
            ) {
                Decision::Off | Decision::Wait => {}
                Decision::Start => {
                    let _ = t
                        .run_async(&["set-option", "-g", LAST, &now.to_string()])
                        .await;
                }
                Decision::NotRestored => {
                    if !skipped {
                        log(&socket, "autosave: skip: server not restored yet");
                        skipped = true;
                    }
                }
                Decision::Save => {
                    skipped = false;
                    let _ = t
                        .run_async(&["set-option", "-g", LAST, &now.to_string()])
                        .await;
                    let msg = save(&t).await;
                    log(&socket, &msg);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_option() {
        assert_eq!(parse_interval(""), 5);
        assert_eq!(parse_interval(" 15 "), 15);
        assert_eq!(parse_interval("0"), 0);
        assert_eq!(parse_interval("99999"), 24 * 60);
        for bad in ["-1", "x", "1.5"] {
            assert_eq!(parse_interval(bad), 5, "{bad}");
        }
    }

    #[test]
    fn decisions() {
        use Decision::*;
        assert_eq!(decide(0, true, Some(0), 1000), Off);
        assert_eq!(decide(300, true, None, 1000), Start, "never at once");
        assert_eq!(decide(300, true, Some(1000), 1299), Wait);
        assert_eq!(decide(300, true, Some(1000), 1300), Save);
        assert_eq!(decide(300, false, Some(1000), 1300), NotRestored);
        assert_eq!(decide(300, false, Some(1000), 1100), Wait);
    }
}
