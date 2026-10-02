//! Synchronous tmux writes for the popup and CLI: rename, close (with the
//! bash version's safety rules), reopen, reorder, new window, preview.
//! Every write targets IDs, never indexes or names.

use crate::{
    store::{ClosedWindow, Store},
    tmux::Tmux,
};
use std::{path::PathBuf, process::Command};

const US: char = '\x1f';

/// tmux format-expands names and paths given to `rename-window`,
/// `new-window -n/-c`, `new-session -s/-c` and `split-window -c` (so `#(…)`
/// would even run a shell command); doubling `#` makes them literal.
pub fn literal(s: &str) -> String {
    s.replace('#', "##")
}

#[derive(Clone, Debug)]
pub struct Tx {
    pub tmux: Tmux,
}

impl Tx {
    pub fn new(socket: PathBuf) -> Tx {
        Tx {
            tmux: Tmux::new(socket),
        }
    }

    pub fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        self.tmux.run(args)
    }

    fn line(&self, args: &[&str]) -> anyhow::Result<String> {
        Ok(self.run(args)?.trim_end_matches('\n').to_string())
    }

    fn display(&self, target: &str, fmt: &str) -> anyhow::Result<String> {
        self.line(&["display-message", "-p", "-t", target, fmt])
    }

    /// The client whose view we manage: `TMUX_HOME_CLIENT`, else the client
    /// tmux reports for us.
    pub fn home_client(&self) -> Option<String> {
        if let Ok(c) = std::env::var("TMUX_HOME_CLIENT")
            && !c.is_empty()
        {
            return Some(c);
        }
        self.line(&["display-message", "-p", "#{client_name}"])
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Session ID the client is showing.
    pub fn client_session(&self, client: &str) -> Option<String> {
        let out = self
            .run(&["list-clients", "-F", "#{client_name}\x1f#{session_id}"])
            .ok()?;
        out.lines().find_map(|l| {
            let (c, s) = l.split_once(US)?;
            (c == client).then(|| s.to_string())
        })
    }

    pub fn switch_to(&self, client: Option<&str>, sid: &str, wid: &str) -> anyhow::Result<()> {
        let target = format!("{sid}:{wid}");
        match client {
            Some(c) => self.run(&["switch-client", "-c", c, "-t", &target])?,
            None => self.run(&["switch-client", "-t", &target])?,
        };
        Ok(())
    }

    /// Make `pane` its window's active pane (by `%id`).
    pub fn select_pane(&self, pane: &str) -> anyhow::Result<()> {
        self.run(&["select-pane", "-t", pane])?;
        Ok(())
    }

    pub fn window_name(&self, wid: &str) -> anyhow::Result<String> {
        self.display(wid, "#{window_name}")
    }

    pub fn rename(&self, wid: &str, name: &str) -> anyhow::Result<()> {
        self.run(&["rename-window", "-t", wid, "--", &literal(name)])?;
        Ok(())
    }

    pub fn reset_name(&self, wid: &str) -> anyhow::Result<()> {
        // `on`, not unset: unset inherits the global value, which may be off
        self.run(&["set-option", "-w", "-t", wid, "automatic-rename", "on"])?;
        Ok(())
    }

    /// Swap two windows of a session, leaving its current window (by ID)
    /// current. tmux 3.7's `swap-window -d` selects the destination when
    /// neither window is current, so the old current window is re-selected.
    pub fn swap(&self, a: &str, b: &str) -> anyhow::Result<()> {
        let sid = self.display(a, "#{session_id}")?;
        let cur = self.display(&sid, "#{window_id}").ok();
        self.run(&["swap-window", "-d", "-s", a, "-t", b])?;
        if let Some(c) = cur
            && self.display(&sid, "#{window_id}").ok().as_deref() != Some(c.as_str())
        {
            let _ = self.run(&["select-window", "-t", &c]);
        }
        Ok(())
    }

    /// New window after `after`, in `cwd`; returns its ID.
    pub fn new_window_after(&self, after: &str, cwd: &str, name: &str) -> anyhow::Result<String> {
        let (cwd, name) = (literal(cwd), literal(name));
        let mut args = vec![
            "new-window",
            "-a",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-t",
            after,
        ];
        if !cwd.is_empty() {
            args.extend(["-c", cwd.as_str()]);
        }
        if !name.is_empty() {
            args.extend(["-n", name.as_str()]);
        }
        self.line(&args)
    }

    /// A pane with escape sequences (`-e`: colours and attributes) and up
    /// to `history` lines of scrollback above its visible part.
    pub fn capture_styled(&self, pane: &str, history: usize) -> String {
        let start = format!("-{history}");
        self.run(&["capture-pane", "-p", "-e", "-S", &start, "-t", pane])
            .unwrap_or_default()
    }

    /// Visible text of a pane, trailing blank lines trimmed.
    pub fn capture(&self, pane: &str) -> String {
        let Ok(out) = self.run(&["capture-pane", "-p", "-t", pane]) else {
            return String::new();
        };
        let mut lines: Vec<&str> = out.lines().collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Distinct non-shell commands of a window's non-sidebar panes, plus
    /// any job stopped with ^z in a pane that sits at its shell prompt
    /// (`vim (stopped)`): the shell is back in the foreground, so the pane
    /// looks idle, but closing it would kill the job.
    pub fn busy_commands(&self, wid: &str) -> Vec<String> {
        self.busy(wid).commands
    }

    /// `busy_commands`, plus whether a live agent in the window is mid-run
    /// (running or waiting). An agent pane is named by its agent kind
    /// (`claude`), not its command (Claude reports its version).
    pub fn busy(&self, wid: &str) -> Busy {
        let mut busy = Busy::default();
        let Ok(panes) = crate::tmux::snapshot::read_panes(&self.tmux, wid) else {
            return busy;
        };
        let mut add = |c: String| {
            if !busy.commands.contains(&c) {
                busy.commands.push(c);
            }
        };
        for pane in panes.iter().filter(|p| p.role != "sidebar") {
            let agent = crate::agent::pane_agent(pane).filter(|a| a.live());
            if let Some(a) = &agent {
                busy.agent_working |= a.state.status.working();
            }
            if !is_shell(&pane.current_command) {
                match agent {
                    Some(a) => add(a.state.kind.name().to_string()),
                    None => add(pane.current_command.clone()),
                }
            } else {
                for job in stopped_jobs(&pane.tty) {
                    add(format!("{job} (stopped)"));
                }
            }
        }
        busy
    }

    fn sessions(&self) -> Vec<String> {
        self.run(&["list-sessions", "-F", "#{session_id}"])
            .map(|o| o.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// What ^x should do with a window, by the bash rules.
    pub fn close_plan(&self, wid: &str) -> anyhow::Result<ClosePlan> {
        let info = self.display(
            wid,
            "#{session_id}\x1f#{session_name}\x1f#{session_windows}\x1f#{window_name}",
        )?;
        let f: Vec<&str> = info.splitn(4, US).collect();
        anyhow::ensure!(f.len() == 4, "bad window info {info:?}");
        let (sid, sname, wname) = (f[0], f[1], f[3]);
        let nwin: u32 = f[2].parse().unwrap_or(1);
        let last_in_session = nwin <= 1;
        if last_in_session && !self.sessions().iter().any(|s| s != sid) {
            return Ok(ClosePlan::Refuse);
        }
        let Busy {
            commands: busy,
            agent_working,
        } = self.busy(wid);
        if busy.is_empty() && !last_in_session {
            return Ok(ClosePlan::Now);
        }
        let mut why = String::new();
        if agent_working {
            why.push_str(" agent still working —");
        }
        if !busy.is_empty() {
            why.push_str(&format!(" running: {}", busy.join(", ")));
        }
        if last_in_session {
            why.push_str(&format!(" — session \"{sname}\" will end"));
        }
        Ok(ClosePlan::Ask(format!("close \"{wname}\"?{why} (y/N) ")))
    }

    /// Close a window without taking the popup down with it: never the last
    /// window on the server; if it is the last of the client's session, the
    /// client moves to another session first. Snapshots it for reopen.
    pub fn close_window(
        &self,
        wid: &str,
        client: Option<&str>,
        store: &Store,
    ) -> anyhow::Result<CloseOutcome> {
        let info = self.display(wid, "#{session_id}\x1f#{session_windows}")?;
        let (sid, nwin) = info.split_once(US).unwrap_or((&info, "1"));
        if nwin.parse::<u32>().unwrap_or(1) <= 1 {
            let Some(other) = self.sessions().into_iter().find(|s| s != sid) else {
                return Ok(CloseOutcome::Refused);
            };
            if let Some(c) = client
                && self.client_session(c).as_deref() == Some(sid)
            {
                self.run(&["switch-client", "-c", c, "-t", &other])?;
            }
        }
        let snap = self.snapshot_window(wid);
        self.run(&["kill-window", "-t", wid])?;
        // the window is gone now: a failure to save it for reopen is
        // reported as such, never as a failed close
        let saved = snap.and_then(|s| store.push(s));
        Ok(match saved {
            Ok(()) => CloseOutcome::Closed,
            Err(e) => CloseOutcome::NotSaved(format!("{e:#}")),
        })
    }

    pub fn snapshot_window(&self, wid: &str) -> anyhow::Result<ClosedWindow> {
        let info = self.display(
            wid,
            "#{session_id}\x1f#{session_name}\x1f#{window_index}\x1f#{?automatic-rename,on,off}\x1f#{window_panes}\x1f#{window_layout}\x1f#{window_name}",
        )?;
        let f: Vec<&str> = info.splitn(7, US).collect();
        anyhow::ensure!(f.len() == 7, "bad window info {info:?}");
        let sid = f[0];
        let wins: Vec<String> = self
            .run(&["list-windows", "-t", sid, "-F", "#{window_id}"])?
            .lines()
            .map(str::to_string)
            .collect();
        let pos = wins.iter().position(|w| w == wid);
        let prev = pos
            .and_then(|p| p.checked_sub(1))
            .map(|p| wins[p].clone())
            .unwrap_or_else(|| "-".into());
        let next = pos
            .and_then(|p| wins.get(p + 1).cloned())
            .unwrap_or_else(|| "-".into());
        let mut paths = Vec::new();
        let mut active = 0;
        for l in self
            .run(&[
                "list-panes",
                "-t",
                wid,
                "-F",
                "#{pane_active}\x1f#{@pane_role}\x1f#{pane_current_path}",
            ])?
            .lines()
        {
            let p: Vec<&str> = l.splitn(3, US).collect();
            if p.len() != 3 || p[1] == "sidebar" {
                continue;
            }
            if p[0] == "1" {
                active = paths.len();
            }
            paths.push(p[2].to_string());
        }
        let npanes: usize = f[4].parse().unwrap_or(0);
        // a sidebar pane makes the layout's pane count wrong: drop it
        let layout = if npanes == paths.len() {
            f[5].to_string()
        } else {
            String::new()
        };
        if paths.is_empty() {
            paths.push(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        }
        Ok(ClosedWindow {
            session: f[1].into(),
            index: f[2].parse().unwrap_or(0),
            prev,
            next,
            automatic_rename: f[3] == "on",
            active,
            layout,
            name: f[6].replace('\n', " "),
            paths,
        })
    }

    fn window_in_session(&self, wid: &str, sname: &str) -> bool {
        wid != "-" && self.display(wid, "#{session_name}").ok().as_deref() == Some(sname)
    }

    /// Rebuild the most recently closed window (fresh shells, old cwds) at
    /// its old place: old index if free, else after/before its old
    /// neighbour, else at the end; recreating its session if gone.
    /// `Ok(None)` when the stack is empty.
    pub fn reopen(&self, store: &Store) -> anyhow::Result<Option<String>> {
        self.reopen_with(store, Tx::finish)
    }

    /// `reopen` with the steps after the window's creation (`finish`)
    /// replaceable, so tests can make them fail.
    #[doc(hidden)]
    pub fn reopen_with(
        &self,
        store: &Store,
        finish: impl FnOnce(&Tx, &str, &ClosedWindow) -> anyhow::Result<()>,
    ) -> anyhow::Result<Option<String>> {
        let Some(e) = store.pop()? else {
            return Ok(None);
        };
        // only a failed create puts the entry back: once the window exists,
        // un-popping would make the next reopen create a duplicate
        let new = match self.create(&e) {
            Ok(w) => w,
            Err(err) => {
                let _ = store.unpop(e);
                return Err(err);
            }
        };
        // the rest is best-effort: the window is already back
        let _ = finish(self, &new, &e);
        Ok(Some(new))
    }

    /// Create the window (and its session if gone); returns its ID.
    fn create(&self, e: &ClosedWindow) -> anyhow::Result<String> {
        let first = literal(e.paths.first().map_or("/", String::as_str));
        let fmt = ["-P", "-F", "#{window_id}", "-c", first.as_str()];
        let new_in = |extra: &[&str]| -> anyhow::Result<String> {
            let mut a = vec!["new-window", "-d"];
            a.extend(extra);
            a.extend(fmt);
            self.line(&a)
        };
        let has = self
            .run(&["has-session", "-t", &format!("={}", e.session)])
            .is_ok();
        let new = if has {
            let at = format!("={}:{}", e.session, e.index);
            new_in(&["-t", &at])
                .or_else(|err| {
                    if self.window_in_session(&e.prev, &e.session) {
                        new_in(&["-a", "-t", &e.prev])
                    } else {
                        Err(err)
                    }
                })
                .or_else(|err| {
                    if self.window_in_session(&e.next, &e.session) {
                        new_in(&["-b", "-t", &e.next])
                    } else {
                        Err(err)
                    }
                })
                .or_else(|_| new_in(&["-t", &format!("={}:", e.session)]))?
        } else {
            let mut a = vec!["new-session", "-d"];
            a.extend(fmt);
            let sname = literal(&e.session);
            a.extend(["-s", sname.as_str()]);
            // size it like the layout it is about to get
            let size = e.layout.split(',').nth(1).unwrap_or("").to_string();
            let wh: Vec<&str> = size.split('x').collect();
            let ok = wh.len() == 2 && wh.iter().all(|s| s.parse::<u32>().is_ok());
            if ok {
                a.extend(["-x", wh[0], "-y", wh[1]]);
            }
            let new = self.line(&a)?;
            let _ = self.run(&[
                "move-window",
                "-s",
                &new,
                "-t",
                &format!("={}:{}", e.session, e.index),
            ]);
            new
        };
        Ok(new)
    }

    /// Panes, layout, name and active pane of a freshly created window.
    fn finish(&self, new: &str, e: &ClosedWindow) -> anyhow::Result<()> {
        let new = new.to_string();
        // split the LAST pane each time so pane order matches the paths;
        // retile as we go so small windows keep room for the next split
        let mut pane = self.display(&new, "#{pane_id}")?;
        for p in e.paths.iter().skip(1) {
            match self.line(&[
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &pane,
                "-c",
                &literal(p),
            ]) {
                Ok(id) => pane = id,
                Err(_) => break,
            }
            let _ = self.run(&["select-layout", "-t", &new, "tiled"]);
        }
        if !e.layout.is_empty() {
            let _ = self.run(&["select-layout", "-t", &new, &e.layout]);
        }
        if e.automatic_rename {
            self.run(&["set-option", "-w", "-t", &new, "automatic-rename", "on"])?;
        } else {
            self.rename(&new, &e.name)?;
        }
        let panes = self.run(&["list-panes", "-t", &new, "-F", "#{pane_id}"])?;
        if let Some(p) = panes.lines().nth(e.active) {
            let _ = self.run(&["select-pane", "-t", p]);
        }
        Ok(())
    }
}

/// What is running in a window (`Tx::busy`).
#[derive(Debug, Default, PartialEq)]
pub struct Busy {
    pub commands: Vec<String>,
    /// A live agent is running or waiting.
    pub agent_working: bool,
}

#[derive(Debug, PartialEq)]
pub enum ClosePlan {
    /// Last window on the server: never closed.
    Refuse,
    /// Every pane idle and not the session's last window.
    Now,
    /// Ask y/N with this prompt.
    Ask(String),
}

#[derive(Debug, PartialEq)]
pub enum CloseOutcome {
    Closed,
    /// Closed, but not pushed on the reopen stack (why).
    NotSaved(String),
    Refused,
}

/// Commands of the stopped (state `T`) processes on a terminal, by `ps`.
/// Empty if `ps` fails: a missed warning, never a refused close.
pub fn stopped_jobs(tty: &str) -> Vec<String> {
    let tty = tty.strip_prefix("/dev/").unwrap_or(tty);
    if tty.is_empty() {
        return vec![];
    }
    let Ok(out) = Command::new("ps")
        .args(["-o", "stat=,comm=", "-t", tty])
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (stat, comm) = l.trim().split_once(char::is_whitespace)?;
            stat.starts_with('T').then(|| {
                let comm = comm.trim();
                comm.rsplit('/').next().unwrap_or(comm).to_string()
            })
        })
        .collect()
}

/// A pane is idle when it sits at a shell prompt (login shells show "-zsh").
pub fn is_shell(cmd: &str) -> bool {
    let c = cmd.trim_start_matches('-');
    let c = c.rsplit('/').next().unwrap_or(c);
    matches!(
        c,
        "bash"
            | "zsh"
            | "fish"
            | "sh"
            | "dash"
            | "ksh"
            | "mksh"
            | "tcsh"
            | "csh"
            | "nu"
            | "ash"
            | "yash"
    )
}
