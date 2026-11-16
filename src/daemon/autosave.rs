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
//!   `SAVE_TIMEOUT`) before it looks again, so two saves of its own never
//!   overlap; ticks missed meanwhile are dropped. A save runs in its own
//!   process group, and a timed-out one is killed as a group: resurrect's
//!   subshells, its `ps` per pane and the post-save hook with it. (Killing
//!   only the script left those running, re-parented to launchd, while the
//!   next save started: 14 at once under load, issue #35.) While any
//!   process of the last save's group is still alive, no new save starts.
//! - **Not under load:** a due save is skipped while the 1-minute load
//!   average is above `@home-save-max-load` (default twice the logical
//!   CPUs; 0 = no limit), and tried again on the next tick.
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
/// Skip a due save while the 1-minute load average is above this
/// (default `default_max_load()`; 0 = no limit).
pub const OPT_MAX_LOAD: &str = "@home-save-max-load";

/// Twice the logical CPUs.
pub fn default_max_load() -> f64 {
    std::thread::available_parallelism().map_or(8, |n| n.get()) as f64 * 2.0
}

/// `@home-save-max-load`: a non-negative number; else the default.
pub fn parse_max_load(v: &str) -> f64 {
    match v.trim().parse::<f64>() {
        Ok(x) if x.is_finite() && x >= 0.0 => x,
        _ => default_max_load(),
    }
}

/// True when a save should wait for the load to drop.
pub fn too_loaded(load: Option<f64>, max: f64) -> bool {
    max > 0.0 && load.is_some_and(|l| l > max)
}

/// The 1-minute load average, if the system tells.
fn load_avg() -> Option<f64> {
    let mut l = [0f64; 3];
    // SAFETY: getloadavg writes at most 1 element into a 3-element buffer
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 1) };
    (n >= 1).then_some(l[0])
}

/// True while any process of group `pgid` is alive.
fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks; a negative pid names the group
    unsafe { libc::kill(-pgid, 0) == 0 }
}

fn kill_group(pgid: i32) {
    // SAFETY: SIGKILL to the save's own process group, never pgid <= 1
    if pgid > 1 {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

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
    max_load: f64,
}

fn look(t: &Tmux) -> Look {
    Look {
        interval: parse_interval(&global(t, OPT_INTERVAL)),
        restore_done: !global(t, "@home_restore_done").is_empty(),
        last: global(t, LAST).parse().ok(),
        max_load: parse_max_load(&global(t, OPT_MAX_LOAD)),
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

/// Runs one save in its own process group; a line for the log. `group`
/// gets the group's id as soon as the script starts.
async fn save(t: &Tmux, group: &mut Option<i32>) -> String {
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let t0 = Instant::now();
    let child = match c.spawn() {
        Ok(c) => c,
        Err(e) => return format!("autosave: {}: {e}", script.display()),
    };
    // process_group(0): the group's id is the script's pid
    let pgid = child.id().map(|p| p as i32);
    *group = pgid;
    match tokio::time::timeout(SAVE_TIMEOUT, child.wait_with_output()).await {
        Err(_) => {
            if let Some(g) = pgid {
                kill_group(g);
            }
            format!(
                "autosave: {} timed out after {}s, killed its process group",
                script.display(),
                SAVE_TIMEOUT.as_secs()
            )
        }
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
        // ... and once per stretch of high load, or of a save still running
        let mut load_skipped = false;
        let mut busy_skipped = false;
        // the last save's process group
        let mut group: Option<i32> = None;
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
                    if let Some(g) = group.filter(|g| group_alive(*g)) {
                        if !busy_skipped {
                            log(
                                &socket,
                                &format!(
                                    "autosave: skip: last save's process group {g} still running"
                                ),
                            );
                            busy_skipped = true;
                        }
                        continue;
                    }
                    busy_skipped = false;
                    let load = load_avg();
                    if too_loaded(load, l.max_load) {
                        if !load_skipped {
                            log(
                                &socket,
                                &format!(
                                    "autosave: skip: load {:.1} above {} ({OPT_MAX_LOAD})",
                                    load.unwrap_or_default(),
                                    l.max_load
                                ),
                            );
                            load_skipped = true;
                        }
                        continue;
                    }
                    load_skipped = false;
                    let _ = t
                        .run_async(&["set-option", "-g", LAST, &now.to_string()])
                        .await;
                    let msg = save(&t, &mut group).await;
                    log(&socket, &msg);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

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
    fn max_load_option() {
        let d = default_max_load();
        assert!(d >= 2.0);
        assert_eq!(parse_max_load(""), d);
        assert_eq!(parse_max_load(" 12.5 "), 12.5);
        assert_eq!(parse_max_load("0"), 0.0);
        for bad in ["-1", "x", "inf", "NaN"] {
            assert_eq!(parse_max_load(bad), d, "{bad}");
        }
    }

    #[test]
    fn load_gate() {
        assert!(!too_loaded(Some(57.0), 0.0), "0 = no limit");
        assert!(too_loaded(Some(57.0), 24.0));
        assert!(!too_loaded(Some(24.0), 24.0));
        assert!(!too_loaded(None, 24.0), "unknown load never blocks");
    }

    #[test]
    fn group_kill_takes_orphans_with_it() {
        // a parent that leaves a long-lived child behind, as save.sh's
        // subshells were left when only the script was killed
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 60 & sleep 60"])
            .process_group(0)
            .spawn()
            .unwrap();
        let g = child.id() as i32;
        assert!(group_alive(g));
        kill_group(g);
        let mut child = child;
        let _ = child.wait();
        let t0 = Instant::now();
        while group_alive(g) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!group_alive(g), "the backgrounded sleep died too");
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
