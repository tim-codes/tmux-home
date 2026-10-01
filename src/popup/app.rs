//! Pure popup state: rows, filter, selection and modes. Keys map to
//! `Action`s the runtime executes against tmux; nothing here does I/O.

use super::filter::Query;
use crate::agent::{Tally, WindowAgents};
use crate::tmux::snapshot::Snapshot;
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

/// How long the selection keeps wanting a window that isn't in the list
/// (`App::want`) before it settles on the row under the cursor.
pub const WANT_FOR: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub sid: String,
    pub session: String,
    pub wid: String,
    pub index: u32,
    pub name: String,
    /// The pane the row describes and the preview captures: the active pane
    /// unless it is a sidebar, else the first non-sidebar pane.
    pub pane: Option<String>,
    pub cmd: String,
    /// Full cwd (new windows open here) and its short form (display).
    pub cwd: String,
    pub path: String,
    /// The invoking client's current window.
    pub current: bool,
    /// The window's agents (SPEC §7), if any pane has agent state.
    pub agents: Option<WindowAgents>,
    /// A copy of a window that needs you, pinned in the NEEDS YOU group
    /// above the sessions.
    pub pinned: bool,
    /// Selection identity: the window ID, `!`-prefixed on a pinned copy.
    pub key: String,
    pub(crate) hay: String,
    /// The agents' prompts, lowercased (matched by substring).
    pub(crate) prompts: String,
}

impl Row {
    /// The pane the preview shows: the lead agent's in an agent window,
    /// else the row's pane.
    pub fn preview_pane(&self) -> Option<String> {
        match &self.agents {
            Some(a) => Some(a.lead().pane.clone()),
            None => self.pane.clone(),
        }
    }

    pub fn needs_you(&self) -> bool {
        self.agents.as_ref().is_some_and(WindowAgents::needs_you)
    }
}

/// Rows grouped by session (the client's session first, then by name),
/// windows in index order; windows that need you are also pinned, in the
/// same order, above them all.
pub fn build_rows(s: &Snapshot, client: Option<&str>, home: &str) -> Vec<Row> {
    let csid = client
        .and_then(|c| s.clients.iter().find(|x| x.name == c))
        .map(|c| c.session_id.clone());
    let mut sessions: Vec<_> = s.sessions.iter().collect();
    sessions.sort_by(|a, b| {
        let ra = Some(&a.id) != csid.as_ref();
        let rb = Some(&b.id) != csid.as_ref();
        (ra, &a.name).cmp(&(rb, &b.name))
    });
    let mut rows = Vec::new();
    for sess in sessions {
        let mut wins: Vec<_> = s
            .windows
            .iter()
            .filter(|w| w.session_id == sess.id)
            .collect();
        wins.sort_by_key(|w| w.index);
        for w in wins {
            let mut panes: Vec<_> = s
                .panes
                .iter()
                .filter(|p| p.window_id == w.id && p.session_id == sess.id && p.role != "sidebar")
                .collect();
            panes.sort_by_key(|p| p.index);
            let pane = panes.iter().find(|p| p.active).or(panes.first()).copied();
            let (cmd, cwd) = pane
                .map(|p| (p.current_command.clone(), p.current_path.clone()))
                .unwrap_or_default();
            let path = short_path(&cwd, home);
            let agents = WindowAgents::of(panes.iter().copied());
            let kind = agents
                .as_ref()
                .map(|a| a.lead().state.kind.name().to_string())
                .unwrap_or_default();
            let hay = format!(
                "{} {} {} {} {} {}",
                sess.name, w.index, w.name, cmd, path, kind
            );
            let prompts = agents
                .iter()
                .flat_map(|a| &a.panes)
                .filter_map(|p| p.state.prompt.as_deref())
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            rows.push(Row {
                sid: sess.id.clone(),
                session: sess.name.clone(),
                wid: w.id.clone(),
                index: w.index,
                name: w.name.replace(['\t', '\n'], " "),
                pane: pane.map(|p| p.id.clone()),
                cmd,
                cwd,
                path,
                current: Some(&sess.id) == csid.as_ref() && w.active,
                agents,
                pinned: false,
                key: w.id.clone(),
                hay,
                prompts,
            });
        }
    }
    let mut pinned: Vec<Row> = rows
        .iter()
        .filter(|r| r.needs_you())
        .map(|r| Row {
            pinned: true,
            key: format!("!{}", r.wid),
            ..r.clone()
        })
        .collect();
    pinned.append(&mut rows);
    pinned
}

/// `~/dev/dotfiles/src` → `~/d/d/src`.
pub fn short_path(p: &str, home: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let p = if !home.is_empty() && p == home {
        return "~".into();
    } else if !home.is_empty() && p.starts_with(&format!("{home}/")) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    };
    let parts: Vec<&str> = p.split('/').collect();
    let n = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if i > 0 && i + 1 < n && s.chars().count() > 1 {
                s.chars().next().unwrap().to_string()
            } else {
                s.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A one-line text editor (filter, rename, new-window name).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineEdit {
    pub text: Vec<char>,
    pub cur: usize,
}

impl LineEdit {
    pub fn with(s: &str) -> LineEdit {
        let text: Vec<char> = s.chars().collect();
        let cur = text.len();
        LineEdit { text, cur }
    }
    pub fn as_string(&self) -> String {
        self.text.iter().collect()
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub fn clear(&mut self) {
        self.text.clear();
        self.cur = 0;
    }
    /// Applies an editing key; true if the key was an editing key.
    pub fn key(&mut self, k: &KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Char('a') if ctrl => self.cur = 0,
            KeyCode::Char('e') if ctrl => self.cur = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cur);
                self.cur = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let mut i = self.cur;
                while i > 0 && self.text[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && self.text[i - 1] != ' ' {
                    i -= 1;
                }
                self.text.drain(i..self.cur);
                self.cur = i;
            }
            KeyCode::Char('h') if ctrl => self.backspace(),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.text.insert(self.cur, c);
                self.cur += 1;
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => {
                if self.cur < self.text.len() {
                    self.text.remove(self.cur);
                }
            }
            KeyCode::Left => self.cur = self.cur.saturating_sub(1),
            KeyCode::Right => self.cur = (self.cur + 1).min(self.text.len()),
            KeyCode::Home => self.cur = 0,
            KeyCode::End => self.cur = self.text.len(),
            _ => return false,
        }
        true
    }
    fn backspace(&mut self) {
        if self.cur > 0 {
            self.cur -= 1;
            self.text.remove(self.cur);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    List,
    Rename {
        wid: String,
        edit: LineEdit,
    },
    NewWindow {
        after: String,
        cwd: String,
        edit: LineEdit,
    },
    Confirm {
        wid: String,
        prompt: String,
    },
    Help,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    Switch {
        sid: String,
        wid: String,
    },
    Rename {
        wid: String,
        name: String,
    },
    ResetName(String),
    /// Decide how to close (runtime asks tmux, then calls `close_plan`).
    CloseStart(String),
    /// Confirmed: close it.
    Close(String),
    Reopen,
    Swap {
        a: String,
        b: String,
    },
    NewWindow {
        after: String,
        cwd: String,
        name: String,
    },
    Redraw,
}

pub const FOOTER_LIST: &str = " ⏎ go   ^r rename   M-r auto-name   ^x close   ^t reopen   M-↑↓ move   M-n new   ^o preview   Esc clear/close   F1 keys";
pub const FOOTER_EDIT: &str = " ⏎ save   Esc cancel   (empty name cancels)";
pub const FOOTER_NEW: &str = " ⏎ create   Esc cancel   (empty = automatic name)";
pub const FOOTER_CLOSE: &str = " y close   any other key or Esc cancels";

pub struct App {
    pub rows: Vec<Row>,
    /// Indices into `rows` that match the filter, in list order.
    pub visible: Vec<usize>,
    pub filter: LineEdit,
    /// Position in `visible`.
    pub sel: usize,
    /// Window ID of the selection, kept across updates.
    pub sel_wid: Option<String>,
    /// Until when a `sel_wid` missing from the rows is still wanted (a
    /// window the popup itself just made); see `want`.
    want_until: Option<Instant>,
    pub mode: Mode,
    pub notice: Option<String>,
    pub preview_flip: bool,
    /// " <session> ▸ <index>" of the invoking client.
    pub location: String,
    /// Live agents by status (header).
    pub tally: Tally,
    pub page: usize,
    matcher: Matcher,
}

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

impl App {
    pub fn new() -> App {
        App {
            rows: vec![],
            visible: vec![],
            filter: LineEdit::default(),
            sel: 0,
            sel_wid: None,
            want_until: None,
            mode: Mode::List,
            notice: None,
            preview_flip: false,
            location: String::new(),
            tally: Tally::default(),
            page: 10,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn selected(&self) -> Option<&Row> {
        self.visible.get(self.sel).map(|&i| &self.rows[i])
    }

    /// New rows from a snapshot. The selection stays on its window (or at
    /// its position if the window went); an open editor is left alone
    /// unless its window vanished.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.set_rows_at(rows, Instant::now());
    }

    /// `set_rows` at time `now`.
    pub fn set_rows_at(&mut self, rows: Vec<Row>, now: Instant) {
        self.location = rows
            .iter()
            .find(|r| r.current)
            .map(|r| format!("{} ▸ {}", r.session, r.index))
            .unwrap_or_default();
        self.tally = Tally::default();
        for p in rows
            .iter()
            .filter(|r| !r.pinned)
            .filter_map(|r| r.agents.as_ref())
            .flat_map(|a| &a.panes)
        {
            self.tally.add(p);
        }
        self.rows = rows;
        self.refilter();
        // a pinned copy that's no longer pinned (answered): its window
        let key = self.sel_wid.clone().map(|k| match self.pos_of(&k) {
            None if k.starts_with('!') => k[1..].to_string(),
            _ => k,
        });
        match key.and_then(|w| self.pos_of(&w)) {
            Some(p) => {
                self.sel = p;
                self.clamp();
            }
            // a window the popup just made or reopened (`want`) can be
            // missing from a snapshot read before the write: keep wanting
            // it for a while
            None if self.want_until.is_some_and(|t| now < t) => self.clamp_pos(),
            // otherwise the window has gone (its ID is never reused): the
            // selection becomes the row now under the cursor
            None => {
                self.want_until = None;
                self.clamp();
            }
        }
        let gone = |w: &String| !self.rows.iter().any(|r| &r.wid == w);
        let vanished = match &self.mode {
            Mode::Rename { wid, .. } | Mode::Confirm { wid, .. } => gone(wid),
            Mode::NewWindow { after, .. } => gone(after),
            _ => false,
        };
        if vanished {
            self.mode = Mode::List;
            self.notice = Some(" that window has gone".into());
        }
    }

    /// Put the selection on the client's current window (startup).
    pub fn select_current(&mut self) {
        if let Some(p) = self
            .visible
            .iter()
            .position(|&i| self.rows[i].current && !self.rows[i].pinned)
        {
            self.set_sel(p);
        }
    }

    pub fn select_wid(&mut self, wid: &str) {
        if let Some(p) = self.pos_of(wid) {
            self.set_sel(p);
        }
    }

    /// Select `wid`, a window the popup itself just created, even if the
    /// rows don't show it yet; it is wanted for `WANT_FOR`.
    pub fn want(&mut self, wid: &str) {
        self.want_at(wid, Instant::now());
    }

    /// `want` at time `now`.
    pub fn want_at(&mut self, wid: &str, now: Instant) {
        self.select_wid(wid);
        self.sel_wid = Some(wid.to_string());
        self.want_until = Some(now + WANT_FOR);
    }

    /// Position of the row with this key (a window ID selects the window's
    /// row in its session, not a pinned copy).
    fn pos_of(&self, key: &str) -> Option<usize> {
        self.visible.iter().position(|&i| self.rows[i].key == key)
    }

    fn set_sel(&mut self, p: usize) {
        self.sel = p;
        self.clamp();
    }

    fn clamp(&mut self) {
        self.clamp_pos();
        self.sel_wid = self.selected().map(|r| r.key.clone());
    }

    fn clamp_pos(&mut self) {
        if self.visible.is_empty() {
            self.sel = 0;
        } else if self.sel >= self.visible.len() {
            self.sel = self.visible.len() - 1;
        }
    }

    fn refilter(&mut self) {
        let q = Query::parse(&self.filter.as_string());
        if q.is_empty() {
            self.visible = (0..self.rows.len()).collect();
            return;
        }
        let pat = Pattern::parse(&q.text, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let m = &mut self.matcher;
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                q.tokens_match(r)
                    && (q.text.is_empty()
                        || pat.score(Utf32Str::new(&r.hay, &mut buf), m).is_some()
                        || q.prompt_match(r))
            })
            .map(|(i, _)| i)
            .collect();
    }

    /// `^g`: the next window needing you, after the selection, cycling. A
    /// window pinned in NEEDS YOU is visited there, not again in its
    /// session.
    fn jump_attention(&mut self) {
        let mut seen = Vec::new();
        let stops: Vec<usize> = (0..self.visible.len())
            .filter(|&p| {
                let r = &self.rows[self.visible[p]];
                r.needs_you() && !seen.contains(&r.wid) && {
                    seen.push(r.wid.clone());
                    true
                }
            })
            .collect();
        if stops.is_empty() {
            self.notice = Some(" nothing needs you".into());
            return;
        }
        let cur = self.selected().map(|r| r.wid.clone());
        let next = match stops
            .iter()
            .position(|&p| Some(&self.rows[self.visible[p]].wid) == cur.as_ref())
        {
            Some(i) => stops[(i + 1) % stops.len()],
            None => *stops.iter().find(|&&p| p > self.sel).unwrap_or(&stops[0]),
        };
        self.set_sel(next);
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.refilter();
        self.clamp();
    }

    fn move_by(&mut self, d: isize) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        // --cycle, like the fzf version
        let p = (self.sel as isize + d).rem_euclid(n as isize) as usize;
        self.set_sel(p);
    }

    fn page_by(&mut self, d: isize) {
        let n = self.visible.len() as isize;
        if n == 0 {
            return;
        }
        let p = (self.sel as isize + d * self.page.max(1) as isize).clamp(0, n - 1);
        self.set_sel(p as usize);
    }

    /// The runtime's verdict on a ^x.
    pub fn close_plan(&mut self, wid: &str, plan: crate::ops::ClosePlan) -> Action {
        use crate::ops::ClosePlan;
        match plan {
            ClosePlan::Refuse => {
                self.notice = Some(" can't close the last window on the server".into());
                Action::Redraw
            }
            ClosePlan::Now => Action::Close(wid.into()),
            ClosePlan::Ask(prompt) => {
                self.mode = Mode::Confirm {
                    wid: wid.into(),
                    prompt,
                };
                Action::Redraw
            }
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match &mut self.mode {
            Mode::Help => {
                self.mode = Mode::List;
                Action::Redraw
            }
            Mode::Confirm { wid, .. } => {
                let wid = wid.clone();
                self.mode = Mode::List;
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl && !alt => Action::Close(wid),
                    _ => Action::Redraw,
                }
            }
            Mode::Rename { wid, edit } => match k.code {
                KeyCode::Esc => {
                    self.mode = Mode::List;
                    Action::Redraw
                }
                KeyCode::Enter => {
                    let (wid, name) = (wid.clone(), edit.as_string());
                    self.mode = Mode::List;
                    if name.is_empty() {
                        Action::Redraw
                    } else {
                        Action::Rename { wid, name }
                    }
                }
                _ => {
                    edit.key(&k);
                    Action::Redraw
                }
            },
            Mode::NewWindow { after, cwd, edit } => match k.code {
                KeyCode::Esc => {
                    self.mode = Mode::List;
                    Action::Redraw
                }
                KeyCode::Enter => {
                    let a = Action::NewWindow {
                        after: after.clone(),
                        cwd: cwd.clone(),
                        name: edit.as_string(),
                    };
                    self.mode = Mode::List;
                    a
                }
                _ => {
                    edit.key(&k);
                    Action::Redraw
                }
            },
            Mode::List => self.list_key(k, ctrl, alt),
        }
    }

    fn list_key(&mut self, k: KeyEvent, ctrl: bool, alt: bool) -> Action {
        let sel = self.selected().cloned();
        match k.code {
            KeyCode::Up if alt => return self.swap_action(-1),
            KeyCode::Down if alt => return self.swap_action(1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char('p') | KeyCode::Char('k') if ctrl => self.move_by(-1),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => self.move_by(1),
            KeyCode::PageUp => self.page_by(-1),
            KeyCode::PageDown => self.page_by(1),
            KeyCode::Enter => {
                return match sel {
                    Some(r) => Action::Switch {
                        sid: r.sid,
                        wid: r.wid,
                    },
                    None => Action::None,
                };
            }
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    return Action::Quit;
                }
                self.notice = None;
                self.clear_filter();
                self.set_sel(0);
            }
            KeyCode::Char('c') | KeyCode::Char('q') if ctrl => return Action::Quit,
            KeyCode::Char('g') if ctrl => self.jump_attention(),
            KeyCode::Char('r') if ctrl => {
                if let Some(r) = sel {
                    self.notice = None;
                    self.mode = Mode::Rename {
                        wid: r.wid,
                        edit: LineEdit::with(&r.name),
                    };
                }
            }
            KeyCode::Char('r') if alt => {
                if let Some(r) = sel {
                    return Action::ResetName(r.wid);
                }
            }
            KeyCode::Char('n') if alt => {
                if let Some(r) = sel {
                    self.notice = None;
                    self.mode = Mode::NewWindow {
                        after: r.wid,
                        cwd: r.cwd,
                        edit: LineEdit::default(),
                    };
                }
            }
            KeyCode::Char('x') if ctrl => {
                if let Some(r) = sel {
                    return Action::CloseStart(r.wid);
                }
            }
            KeyCode::Char('t') if ctrl => return Action::Reopen,
            KeyCode::Char('o') if ctrl => self.preview_flip = !self.preview_flip,
            KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Char('/') | KeyCode::Char('7') if ctrl => self.mode = Mode::Help,
            _ => {
                let before = self.filter.clone();
                if self.filter.key(&k) && self.filter.text != before.text {
                    // the filter changed: back to the top, drop any notice
                    self.notice = None;
                    self.refilter();
                    self.set_sel(0);
                }
            }
        }
        Action::Redraw
    }

    /// M-↑/M-↓: swap with the adjacent window in the list, same session only.
    fn swap_action(&mut self, d: isize) -> Action {
        let Some(cur) = self.selected().cloned() else {
            return Action::None;
        };
        let p = self.sel as isize + d;
        if p < 0 || p as usize >= self.visible.len() {
            return Action::None;
        }
        let other = &self.rows[self.visible[p as usize]];
        if other.sid != cur.sid || cur.pinned || other.pinned {
            return Action::None;
        }
        Action::Swap {
            a: cur.wid,
            b: other.wid.clone(),
        }
    }

    pub fn footer(&self) -> String {
        if let Some(n) = &self.notice {
            return n.clone();
        }
        match self.mode {
            Mode::List | Mode::Help if self.rows.iter().any(|r| r.pinned) => {
                FOOTER_LIST.replacen("   ^r rename", "   ^g needs you   ^r rename", 1)
            }
            Mode::List | Mode::Help => FOOTER_LIST.into(),
            Mode::Rename { .. } => FOOTER_EDIT.into(),
            Mode::NewWindow { .. } => FOOTER_NEW.into(),
            Mode::Confirm { .. } => FOOTER_CLOSE.into(),
        }
    }
}

pub const HELP: &str = "tmux-home — keys

  type            filter windows (session, name, command, path, agent
                  prompt); tokens narrow it, combined with the text:
                  @attn @agent @running @waiting @idle @error s:<session>
  ↑ ↓  ^p ^n  ^k ^j   move selection
  PgUp PgDn       page
  ← →  Home End   edit the filter
  ⏎               switch to the selected window and close
  ^g              jump to the next window that needs you (NEEDS YOU)
  Esc             clear the filter; close if it is already empty
  ^r              rename the window inline (⏎ save, Esc/empty cancel)
  M-r             reset the window to its automatic name
  ^x              close the window; asks first (y/N) if anything but a
                  shell is running in it (a job stopped with ^z counts),
                  or if it is its session's last
  ^t              reopen the last closed window (up to 10 back): same
                  place, name, panes, layout and directories — but fresh
                  shells; what was running in it is gone
  M-↑ M-↓         move the window up / down within its session
  M-n             new window after the selection (inline name)
  ^o              toggle the preview
  F1  ^/          this help

press any key to return";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::snapshot::{Client, Pane, Session, Window};
    use ratatui::crossterm::event::KeyEvent;

    fn snap() -> Snapshot {
        let mut s = Snapshot::default();
        for (sid, name) in [("$0", "alpha"), ("$1", "beta")] {
            s.sessions.push(Session {
                id: sid.into(),
                name: name.into(),
                attached: 0,
            });
        }
        for (i, (sid, wid, idx, name)) in [
            ("$0", "@0", 0, "editor"),
            ("$0", "@1", 1, "win two"),
            ("$1", "@2", 0, "logs"),
            ("$1", "@3", 1, "build"),
        ]
        .into_iter()
        .enumerate()
        {
            s.windows.push(Window {
                id: wid.into(),
                session_id: sid.into(),
                index: idx,
                name: name.into(),
                automatic_rename: false,
                active: idx == 0,
            });
            s.panes.push(Pane {
                id: format!("%{i}"),
                window_id: wid.into(),
                session_id: sid.into(),
                index: 0,
                active: true,
                current_command: "fish".into(),
                current_path: "/home/u/dev/x".into(),
                title: String::new(),
                role: String::new(),
                agent_opts: Default::default(),
            });
        }
        // a sidebar pane never describes the row
        s.panes.push(Pane {
            id: "%9".into(),
            window_id: "@2".into(),
            session_id: "$1".into(),
            index: 1,
            active: true,
            current_command: "sidebar".into(),
            current_path: "/".into(),
            title: String::new(),
            role: "sidebar".into(),
            agent_opts: Default::default(),
        });
        s.clients.push(Client {
            name: "/dev/ttys1".into(),
            tty: "/dev/ttys1".into(),
            session_id: "$1".into(),
        });
        s
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            app.key(key(KeyCode::Char(c)));
        }
    }
    fn names(app: &App) -> Vec<&str> {
        app.visible
            .iter()
            .map(|&i| app.rows[i].name.as_str())
            .collect()
    }

    fn app() -> App {
        let mut a = App::new();
        a.set_rows(build_rows(&snap(), Some("/dev/ttys1"), "/home/u"));
        a.select_current();
        a
    }

    #[test]
    fn rows_grouped_client_session_first() {
        let a = app();
        assert_eq!(names(&a), ["logs", "build", "editor", "win two"]);
        assert_eq!(a.selected().unwrap().wid, "@2");
        assert_eq!(a.location, "beta ▸ 0");
        assert_eq!(a.rows[0].pane.as_deref(), Some("%2"));
        assert_eq!(a.rows[0].path, "~/d/x");
    }

    #[test]
    fn filter_and_move() {
        let mut a = app();
        a.key(key(KeyCode::Down));
        assert_eq!(a.selected().unwrap().name, "build");
        a.key(ctrl('j'));
        a.key(ctrl('n'));
        assert_eq!(a.selected().unwrap().name, "win two");
        a.key(ctrl('k'));
        assert_eq!(a.selected().unwrap().name, "editor");
        a.key(key(KeyCode::Down));
        a.key(key(KeyCode::Down)); // cycles
        assert_eq!(a.selected().unwrap().name, "logs");
        typed(&mut a, "two");
        assert_eq!(names(&a), ["win two"]);
        assert_eq!(a.selected().unwrap().wid, "@1");
        // selection is kept by ID across an update
        a.set_rows(build_rows(&snap(), Some("/dev/ttys1"), "/home/u"));
        assert_eq!(a.selected().unwrap().wid, "@1");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Action::Switch {
                sid: "$0".into(),
                wid: "@1".into()
            }
        );
        // Esc clears, then closes
        assert_eq!(a.key(key(KeyCode::Esc)), Action::Redraw);
        assert_eq!(names(&a).len(), 4);
        assert_eq!(a.key(key(KeyCode::Esc)), Action::Quit);
    }

    #[test]
    fn rename_mode() {
        let mut a = app();
        typed(&mut a, "bui");
        a.key(ctrl('r'));
        assert!(
            matches!(&a.mode, Mode::Rename { wid, edit } if wid == "@3" && edit.as_string() == "build")
        );
        // list keys don't act while editing; a snapshot leaves the editor alone
        a.key(ctrl('x'));
        a.set_rows(build_rows(&snap(), Some("/dev/ttys1"), "/home/u"));
        typed(&mut a, "er");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Action::Rename {
                wid: "@3".into(),
                name: "builder".into()
            }
        );
        assert_eq!(a.mode, Mode::List);
        assert_eq!(a.filter.as_string(), "bui", "filter kept");
        // Esc cancels; empty name cancels
        a.key(ctrl('r'));
        assert_eq!(a.key(key(KeyCode::Esc)), Action::Redraw);
        assert_eq!(a.mode, Mode::List);
        a.key(ctrl('r'));
        a.key(ctrl('u'));
        assert_eq!(a.key(key(KeyCode::Enter)), Action::Redraw);
        // a vanished target closes the editor with a notice
        a.key(ctrl('r'));
        let mut s = snap();
        s.windows.retain(|w| w.id != "@3");
        a.set_rows(build_rows(&s, Some("/dev/ttys1"), "/home/u"));
        assert_eq!(a.mode, Mode::List);
        assert!(a.notice.is_some());
    }

    fn alt(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::ALT)
    }

    #[test]
    fn one_row_per_window_with_ids_and_main_pane() {
        let rows = build_rows(&snap(), Some("/dev/ttys1"), "/home/u");
        assert_eq!(rows.len(), 4, "the sidebar pane adds no row");
        let logs = rows.iter().find(|r| r.name == "logs").unwrap();
        assert_eq!((logs.sid.as_str(), logs.wid.as_str()), ("$1", "@2"));
        // the active pane is a sidebar: the row describes the main pane
        assert_eq!(logs.pane.as_deref(), Some("%2"));
        assert_eq!(logs.cmd, "fish");
    }

    #[test]
    fn no_client_sessions_by_name_windows_by_index() {
        let mut s = snap();
        s.windows.reverse();
        s.sessions.reverse();
        let rows = build_rows(&s, None, "/home/u");
        let got: Vec<_> = rows
            .iter()
            .map(|r| format!("{} {}", r.session, r.name))
            .collect();
        assert_eq!(
            got,
            ["alpha editor", "alpha win two", "beta logs", "beta build"]
        );
        assert!(rows.iter().all(|r| !r.current));
    }

    #[test]
    fn header_follows_client_and_current_is_marked() {
        let mut a = app();
        assert_eq!(a.location, "beta ▸ 0");
        let cur: Vec<_> = a
            .rows
            .iter()
            .filter(|r| r.current)
            .map(|r| &r.wid)
            .collect();
        assert_eq!(cur, ["@2"]);
        // the client moves to alpha: header, order and marker follow
        let mut s = snap();
        s.clients[0].session_id = "$0".into();
        a.set_rows(build_rows(&s, Some("/dev/ttys1"), "/home/u"));
        assert_eq!(a.location, "alpha ▸ 0");
        assert_eq!(names(&a), ["editor", "win two", "logs", "build"]);
        assert!(a.rows[0].current);
    }

    #[test]
    fn typing_filters_and_returns_to_top() {
        let mut a = app();
        a.key(key(KeyCode::Down));
        a.key(key(KeyCode::Down));
        typed(&mut a, "bui");
        assert_eq!(names(&a), ["build"]);
        assert_eq!(a.sel, 0);
        // fzf-syntax characters are literal
        let mut rows = build_rows(&snap(), Some("/dev/ttys1"), "/home/u");
        rows[0].name = "renamed (x)+y, z".into();
        rows[0].hay = "beta 0 renamed (x)+y, z fish ~/d/x".into();
        a.set_rows(rows);
        a.clear_filter();
        typed(&mut a, "(x)+");
        assert_eq!(names(&a), ["renamed (x)+y, z"]);
    }

    #[test]
    fn alt_r_resets_selected() {
        let mut a = app();
        a.key(key(KeyCode::Down));
        assert_eq!(
            a.key(alt(KeyCode::Char('r'))),
            Action::ResetName("@3".into())
        );
        assert_eq!(a.mode, Mode::List, "the popup stays in the list");
    }

    #[test]
    fn ctrl_o_toggles_preview_and_help_returns_on_any_key() {
        let mut a = app();
        a.key(ctrl('o'));
        assert!(a.preview_flip);
        a.key(ctrl('o'));
        assert!(!a.preview_flip);
        for open in [key(KeyCode::F(1)), ctrl('/'), ctrl('7')] {
            a.key(open);
            assert_eq!(a.mode, Mode::Help);
            assert_eq!(a.key(key(KeyCode::Char('q'))), Action::Redraw);
            assert_eq!(a.mode, Mode::List);
        }
    }

    #[test]
    fn close_verdicts() {
        use crate::ops::ClosePlan;
        let mut a = app();
        assert_eq!(a.key(ctrl('x')), Action::CloseStart("@2".into()));
        // idle: closes at once
        assert_eq!(
            a.close_plan("@2", ClosePlan::Now),
            Action::Close("@2".into())
        );
        // last window on the server: refused with a notice
        assert_eq!(a.close_plan("@2", ClosePlan::Refuse), Action::Redraw);
        assert!(a.footer().contains("last window on the server"));
        // busy: the prompt asks; only y closes
        for (k, want) in [
            (key(KeyCode::Char('n')), Action::Redraw),
            (key(KeyCode::Esc), Action::Redraw),
            (key(KeyCode::Enter), Action::Redraw),
            (ctrl('y'), Action::Redraw),
            (key(KeyCode::Char('y')), Action::Close("@2".into())),
        ] {
            typed(&mut a, "lo");
            a.close_plan("@2", ClosePlan::Ask("close \"logs\"? (y/N) ".into()));
            assert!(matches!(a.mode, Mode::Confirm { .. }));
            assert_eq!(a.footer(), FOOTER_CLOSE);
            assert_eq!(a.key(k), want);
            assert_eq!(a.mode, Mode::List);
            assert_eq!(a.filter.as_string(), "lo", "the filter is kept");
            a.clear_filter();
        }
    }

    #[test]
    fn notice_clears_once_you_type() {
        let mut a = app();
        assert_eq!(a.key(ctrl('t')), Action::Reopen);
        a.notice = Some(" nothing to reopen".into());
        a.key(key(KeyCode::Down));
        assert!(a.footer().contains("nothing to reopen"), "moving keeps it");
        typed(&mut a, "x");
        assert!(!a.footer().contains("nothing to reopen"));
    }

    #[test]
    fn alt_n_names_a_new_window_after_the_selection() {
        let mut a = app();
        a.key(alt(KeyCode::Char('n')));
        assert!(matches!(&a.mode, Mode::NewWindow { after, .. } if after == "@2"));
        assert_eq!(a.key(key(KeyCode::Esc)), Action::Redraw);
        assert_eq!(a.mode, Mode::List);
        a.key(alt(KeyCode::Char('n')));
        typed(&mut a, "n #1");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Action::NewWindow {
                after: "@2".into(),
                cwd: "/home/u/dev/x".into(),
                name: "n #1".into()
            }
        );
    }

    fn with_made() -> Snapshot {
        let mut made = snap();
        made.windows.push(Window {
            id: "@7".into(),
            session_id: "$0".into(),
            index: 2,
            name: "made".into(),
            automatic_rename: false,
            active: false,
        });
        made
    }

    /// A snapshot read before a write (here: one without the window the
    /// popup just made and selected) lands after it; the selection comes
    /// back to that window with the next snapshot.
    #[test]
    fn selection_survives_a_stale_snapshot() {
        let t0 = Instant::now();
        let mut a = app();
        a.set_rows_at(build_rows(&with_made(), Some("/dev/ttys1"), "/home/u"), t0);
        a.want_at("@7", t0);
        a.set_rows_at(build_rows(&snap(), Some("/dev/ttys1"), "/home/u"), t0);
        assert_eq!(a.sel_wid.as_deref(), Some("@7"));
        a.set_rows_at(build_rows(&with_made(), Some("/dev/ttys1"), "/home/u"), t0);
        assert_eq!(a.selected().unwrap().wid, "@7");
    }

    /// The wanting is bounded: once `WANT_FOR` has passed, a window still
    /// missing is given up and the row under the cursor is selected.
    #[test]
    fn a_wanted_window_is_given_up_after_a_while() {
        let t0 = Instant::now();
        let mut a = app();
        a.set_rows_at(build_rows(&with_made(), Some("/dev/ttys1"), "/home/u"), t0);
        a.want_at("@7", t0);
        let pos = a.sel;
        let later = t0 + WANT_FOR;
        a.set_rows_at(build_rows(&snap(), Some("/dev/ttys1"), "/home/u"), later);
        let under = a.selected().unwrap().wid.clone();
        assert_ne!(under, "@7");
        assert_eq!(a.sel_wid.as_deref(), Some(under.as_str()));
        assert_eq!(a.sel, pos.min(a.visible.len() - 1));
        // and it is not taken back when the window shows up again
        a.set_rows_at(
            build_rows(&with_made(), Some("/dev/ttys1"), "/home/u"),
            later,
        );
        assert_eq!(a.selected().unwrap().wid, under);
    }

    /// A selected window closed elsewhere isn't wanted: the selection moves
    /// to the row now under the cursor and follows that row, instead of
    /// staying pinned to a row position by a dead ID.
    #[test]
    fn a_vanished_window_hands_the_selection_to_the_row_under_the_cursor() {
        let mut a = app();
        a.select_wid("@0"); // alpha/editor, first row of alpha
        let mut s = snap();
        s.windows.retain(|w| w.id != "@0");
        a.set_rows(build_rows(&s, Some("/dev/ttys1"), "/home/u"));
        let under = a.selected().unwrap().wid.clone();
        assert_eq!(a.sel_wid.as_deref(), Some(under.as_str()));
        // a window appears above it: the selection follows its row
        s.windows.push(Window {
            id: "@8".into(),
            session_id: "$1".into(),
            index: 0,
            name: "new first".into(),
            automatic_rename: false,
            active: false,
        });
        s.windows
            .iter_mut()
            .filter(|w| w.session_id == "$1")
            .for_each(|w| {
                if w.id != "@8" {
                    w.index += 1
                }
            });
        a.set_rows(build_rows(&s, Some("/dev/ttys1"), "/home/u"));
        assert_eq!(a.selected().unwrap().wid, under);
    }

    /// `snap()` with agents: alpha/win two waits (prompt "deploy the
    /// frontend"), beta/build runs, alpha/editor is stale (a shell).
    fn agent_snap() -> Snapshot {
        let mut s = snap();
        let set = |p: &mut Pane, cmd: &str, opts: &[(&str, &str)]| {
            p.current_command = cmd.into();
            for (k, v) in opts {
                p.agent_opts.insert(k.to_string(), v.to_string());
            }
        };
        set(
            &mut s.panes[1],
            "node",
            &[
                ("@pane_agent", "claude"),
                ("@pane_status", "waiting"),
                ("@pane_prompt", "Deploy the frontend to staging"),
            ],
        );
        set(
            &mut s.panes[3],
            "node",
            &[("@pane_agent", "codex"), ("@pane_status", "running")],
        );
        set(
            &mut s.panes[0],
            "fish",
            &[("@pane_agent", "claude"), ("@pane_status", "error")],
        );
        s
    }

    fn agent_app() -> App {
        let mut a = App::new();
        a.set_rows(build_rows(&agent_snap(), Some("/dev/ttys1"), "/home/u"));
        a.select_current();
        a
    }

    fn keys_visible(app: &App) -> Vec<&str> {
        app.visible
            .iter()
            .map(|&i| app.rows[i].key.as_str())
            .collect()
    }

    #[test]
    fn needs_you_rows_are_pinned_first() {
        let a = agent_app();
        assert_eq!(keys_visible(&a), ["!@1", "@2", "@3", "@0", "@1"]);
        assert_eq!(
            a.tally.label(),
            "1 waiting · 1 running",
            "stale not counted"
        );
        // the cursor opens on the client's window, not on a pinned copy
        assert_eq!(a.selected().unwrap().key, "@2");
        assert!(a.footer().contains("^g"));
    }

    #[test]
    fn filter_tokens() {
        let mut a = agent_app();
        let mut q = |s: &str| {
            a.clear_filter();
            typed(&mut a, s);
            keys_visible(&a).join(" ")
        };
        assert_eq!(q("@waiting"), "!@1 @1");
        assert_eq!(q("@running"), "@3");
        assert_eq!(
            q("@running @waiting"),
            "!@1 @3 @1",
            "statuses are alternatives"
        );
        assert_eq!(q("@error"), "", "a stale error isn't an error");
        assert_eq!(q("@agent"), "!@1 @3 @0 @1", "stale agents are agents");
        assert_eq!(q("@attn"), "!@1 @1");
        assert_eq!(q("@idle"), "");
        assert_eq!(q("s:al"), "!@1 @0 @1");
        assert_eq!(q("s:BE @agent"), "@3");
        assert_eq!(q("@agent s:alpha editor"), "@0");
        assert_eq!(q("s:beta @waiting"), "");
        // the prompt is filterable (by words), and the agent kind fuzzily
        assert_eq!(q("staging deploy"), "!@1 @1");
        assert_eq!(q("codex"), "@3");
        assert_eq!(q("@bogus"), "", "unknown tokens are text");
    }

    #[test]
    fn ctrl_g_cycles_windows_that_need_you() {
        let mut s = agent_snap();
        // beta/logs (%2) needs you too
        s.panes[2].current_command = "node".into();
        s.panes[2]
            .agent_opts
            .insert("@pane_agent".into(), "claude".into());
        s.panes[2]
            .agent_opts
            .insert("@pane_attention".into(), "notification".into());
        let mut a = App::new();
        a.set_rows(build_rows(&s, Some("/dev/ttys1"), "/home/u"));
        a.select_current();
        assert_eq!(keys_visible(&a), ["!@2", "!@1", "@2", "@3", "@0", "@1"]);
        assert_eq!(a.selected().unwrap().key, "@2");
        // from beta/logs (pinned as !@2): next is alpha/win two, in NEEDS YOU
        a.key(ctrl('g'));
        assert_eq!(a.selected().unwrap().key, "!@1");
        a.key(ctrl('g'));
        assert_eq!(
            a.selected().unwrap().key,
            "!@2",
            "cycles, pinned copies only"
        );
        a.key(ctrl('g'));
        assert_eq!(a.selected().unwrap().key, "!@1");
        // nothing needs you: a notice, the selection stays
        let mut a = app();
        a.key(ctrl('g'));
        assert_eq!(a.selected().unwrap().key, "@2");
        assert!(a.footer().contains("nothing needs you"));
    }

    #[test]
    fn an_answered_pinned_row_hands_over_to_its_window() {
        let mut a = agent_app();
        a.key(ctrl('g'));
        assert_eq!(a.selected().unwrap().key, "!@1");
        // M-↑↓ never moves a pinned copy
        let alt = |c| KeyEvent::new(c, KeyModifiers::ALT);
        assert_eq!(a.key(alt(KeyCode::Down)), Action::None);
        // the agent was answered: the selection stays on the window
        let mut s = agent_snap();
        s.panes[1]
            .agent_opts
            .insert("@pane_status".into(), "running".into());
        a.set_rows(build_rows(&s, Some("/dev/ttys1"), "/home/u"));
        assert_eq!(a.selected().unwrap().key, "@1");
        assert_eq!(a.selected().unwrap().wid, "@1");
    }

    #[test]
    fn swap_stays_in_session() {
        let mut a = app();
        let alt = |c| KeyEvent::new(c, KeyModifiers::ALT);
        assert_eq!(
            a.key(alt(KeyCode::Down)),
            Action::Swap {
                a: "@2".into(),
                b: "@3".into()
            }
        );
        a.key(key(KeyCode::Down));
        assert_eq!(
            a.key(alt(KeyCode::Down)),
            Action::None,
            "next row is another session"
        );
    }
}
