//! Pure popup state: rows, session pages, filter, selection and modes. Keys
//! map to `Action`s the runtime executes against tmux; nothing here does
//! I/O.

use super::filter::Query;
use crate::agent::{PaneAgent, WindowAgents};
use crate::git::badge::RepoStatus;
use crate::tmux::snapshot::Snapshot;
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::time::{Duration, Instant};

/// How long the selection keeps wanting a window that isn't in the list
/// (`App::want`) before it settles on the row under the cursor.
pub const WANT_FOR: Duration = Duration::from_secs(2);

/// One window: the popup's window row (with its pane rows beneath) and
/// the sidebar's line.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub sid: String,
    pub session: String,
    pub wid: String,
    pub index: u32,
    pub name: String,
    /// The pane the row describes: the active pane unless it is a
    /// sidebar, else the first non-sidebar pane.
    pub pane: Option<String>,
    pub cmd: String,
    /// Full cwd (new windows open here) and its short form (display).
    pub cwd: String,
    pub path: String,
    /// The invoking client's current window.
    pub current: bool,
    /// The window is its session's current window.
    pub active: bool,
    /// The window's agents (SPEC §7), if any pane has agent state.
    pub agents: Option<WindowAgents>,
    /// Git status (root, status) of the repo the row's pane — the lead
    /// agent's pane in an agent window — is in, when the daemon knows it.
    pub git: Option<(String, RepoStatus)>,
    /// The window's non-sidebar panes, in index order.
    pub panes: Vec<PaneRow>,
    /// A copy of a window that needs you, pinned in the NEEDS YOU group
    /// above the session's windows.
    pub pinned: bool,
    /// Selection identity: the window ID, `!`-prefixed on a pinned copy.
    pub key: String,
    pub(crate) hay: String,
    /// The agents' prompts, lowercased (matched by substring).
    pub(crate) prompts: String,
}

/// One pane of a window: a pane row in the popup's tree.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneRow {
    pub id: String,
    pub index: u32,
    pub cmd: String,
    /// Full cwd and its short form.
    pub cwd: String,
    pub path: String,
    /// The pane title (often just the host name; see `App::host`).
    pub title: String,
    pub agent: Option<PaneAgent>,
    /// Git status (root, status) of the repo the pane's cwd is in.
    pub git: Option<(String, RepoStatus)>,
    /// Geometry in its window, in cells: left, top, width, height.
    pub rect: (u32, u32, u32, u32),
    pub(crate) hay: String,
}

impl Row {
    /// The pane the window is shown by: the lead agent's in an agent
    /// window, else the row's pane.
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

/// The command a pane shows: Claude Code's executable is named after its
/// version (`2.1.296`), so a pane tagged as an agent whose command is a
/// bare `N.N.N` shows the agent's name instead: `@home_agent` (tmux-home's
/// hooks), else the legacy `@pane_agent` older sessions still carry.
/// Anything else, an untagged version-named process included, is shown as
/// tmux reports it.
pub fn display_cmd(p: &crate::tmux::snapshot::Pane) -> String {
    let tag = |k: &str| p.agent_opts.get(k).filter(|v| !v.is_empty());
    match tag("@home_agent").or_else(|| tag("@pane_agent")) {
        Some(agent) if crate::agent::is_version(&p.current_command) => agent.clone(),
        _ => p.current_command.clone(),
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
    let git_of = |cwd: &str| {
        s.git
            .paths
            .get(cwd)
            .and_then(|root| Some((root.clone(), s.git.repos.get(root)?.clone())))
    };
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
                .map(|p| (display_cmd(p), p.current_path.clone()))
                .unwrap_or_default();
            let path = short_path(&cwd, home);
            // an automatic name is the active pane's command: shown as
            // that command is (`display_cmd`)
            let wname = match pane {
                Some(p) if w.automatic_rename && w.name == p.current_command => cmd.clone(),
                _ => w.name.replace(['\t', '\n'], " "),
            };
            let agents = WindowAgents::of(panes.iter().copied());
            let kind = agents
                .as_ref()
                .map(|a| a.lead().state.kind.name().to_string())
                .unwrap_or_default();
            // the badge's repo: where the lead agent works, else the row pane
            let git_cwd = agents
                .as_ref()
                .and_then(|a| panes.iter().find(|p| p.id == a.lead().pane))
                .map(|p| p.current_path.as_str())
                .unwrap_or(&cwd);
            let git = git_of(git_cwd);
            let branch = git.as_ref().map(|(_, g)| g.branch.as_str()).unwrap_or("");
            let hay = format!("{} {} {} {} {} {}", w.index, wname, cmd, path, kind, branch);
            let prompts = agents
                .iter()
                .flat_map(|a| &a.panes)
                .filter_map(|p| p.state.prompt.as_deref())
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            let pane_rows = panes
                .iter()
                .map(|p| {
                    let agent = agents
                        .as_ref()
                        .and_then(|a| a.panes.iter().find(|x| x.pane == p.id))
                        .cloned();
                    let git = git_of(&p.current_path);
                    let path = short_path(&p.current_path, home);
                    let title = p.title.replace(['\t', '\n'], " ");
                    let cmd = display_cmd(p);
                    let hay = format!(
                        "{} {} {} {} {}",
                        cmd,
                        path,
                        title,
                        agent.as_ref().map(|a| a.state.kind.name()).unwrap_or(""),
                        git.as_ref().map(|(_, g)| g.branch.as_str()).unwrap_or("")
                    );
                    PaneRow {
                        id: p.id.clone(),
                        index: p.index,
                        cmd,
                        cwd: p.current_path.clone(),
                        path,
                        title,
                        agent,
                        git,
                        rect: (p.left, p.top, p.width, p.height),
                        hay,
                    }
                })
                .collect();
            rows.push(Row {
                sid: sess.id.clone(),
                session: sess.name.clone(),
                wid: w.id.clone(),
                index: w.index,
                name: wname,
                pane: pane.map(|p| p.id.clone()),
                cmd,
                cwd,
                path,
                current: Some(&sess.id) == csid.as_ref() && w.active,
                active: w.active,
                agents,
                git,
                panes: pane_rows,
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

/// tmux's prefix inside the popup. Inside the popup tmux does not see the
/// prefix, so a prefixed chord arrives here as two keys:
/// - prefix + one of the `@home-keys` (the chord that opened the popup)
///   closes it, so the key toggles;
/// - prefix + `)` / `(` (tmux's own next/previous-session keys) pages to
///   the next / previous session.
#[derive(Clone, Debug, PartialEq)]
pub struct Chord {
    pub prefix: (KeyCode, KeyModifiers),
    pub keys: Vec<char>,
}

impl Chord {
    /// From tmux's `prefix` option (`C-a`, `M-x`, `x`) and the
    /// whitespace-separated `@home-keys`. Only single-character keys can
    /// arrive as typed text, so others (`F5`, `C-h`) are skipped (no key
    /// left: paging still works). None if the prefix is unusable (`None`,
    /// `C-Space`).
    pub fn parse(prefix: &str, keys: &str) -> Option<Chord> {
        let (mods, rest) = if let Some(r) = prefix.strip_prefix("C-") {
            (KeyModifiers::CONTROL, r)
        } else if let Some(r) = prefix.strip_prefix("M-") {
            (KeyModifiers::ALT, r)
        } else {
            (KeyModifiers::NONE, prefix)
        };
        let mut cs = rest.chars();
        let (Some(c), None) = (cs.next(), cs.next()) else {
            return None;
        };
        // a control character arrives as its lowercase letter
        let c = if mods == KeyModifiers::CONTROL {
            c.to_ascii_lowercase()
        } else {
            c
        };
        let keys: Vec<char> = keys
            .split_whitespace()
            .filter_map(|w| {
                let mut cs = w.chars();
                match (cs.next(), cs.next()) {
                    (Some(c), None) => Some(c),
                    _ => None,
                }
            })
            .collect();
        Some(Chord {
            prefix: (KeyCode::Char(c), mods),
            keys,
        })
    }

    /// From the popup's environment, set by the plugin's binding:
    /// `TMUX_HOME_PREFIX` and `TMUX_HOME_KEYS` (optional).
    pub fn from_env() -> Option<Chord> {
        Chord::parse(
            &std::env::var("TMUX_HOME_PREFIX").ok()?,
            &std::env::var("TMUX_HOME_KEYS").unwrap_or_default(),
        )
    }

    fn is_prefix(&self, k: &KeyEvent) -> bool {
        let m = k.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
        k.code == self.prefix.0 && m == self.prefix.1
    }

    fn is_home_key(&self, k: &KeyEvent) -> bool {
        let m = k.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
        matches!(k.code, KeyCode::Char(c) if m.is_empty() && self.keys.contains(&c))
    }

    /// `)`: +1 (next session), `(`: -1 (previous), after the prefix.
    fn page_dir(&self, k: &KeyEvent) -> Option<isize> {
        let m = k.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
        match k.code {
            KeyCode::Char(')') if m.is_empty() => Some(1),
            KeyCode::Char('(') if m.is_empty() => Some(-1),
            _ => None,
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
        /// The pane focused after the switch: the selected pane row's, or
        /// an agent window's lead pane.
        pane: Option<String>,
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
    /// The preview's size changed (M-j/M-k: a share of the list and
    /// preview area) or went back to automatic (M-=, None): remember it
    /// in `@home-preview-pct`.
    PreviewPct(Option<u16>),
}

/// The switch keys' modifiers: what `CSI 1;8P` (C-M-S-F1) and its kin
/// (xterm's modifier 8) decode to.
const SWITCH_KEY_MODS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT);

/// The switch keys' function keys, for windows 1 … 10: C-M-S-F`n` for
/// window `n`, except window 3's C-M-S-F12. C-M-S-F3 is `CSI 1;8R`, which
/// crossterm reads as a cursor position report, never as a key.
pub const SWITCH_FKEYS: [u8; 10] = [1, 2, 12, 4, 5, 6, 7, 8, 9, 10];

/// The window index a switch key selects (the terminal sends them for
/// Cmd+Opt+1 … Cmd+Opt+0; `@home-switch-keys` binds the same keys
/// outside the popup).
pub fn switch_key(k: &KeyEvent) -> Option<u32> {
    match k.code {
        KeyCode::F(n) if k.modifiers == SWITCH_KEY_MODS => SWITCH_FKEYS
            .iter()
            .position(|&f| f == n)
            .map(|i| i as u32 + 1),
        _ => None,
    }
}

pub const FOOTER_LIST: &str = " ⏎ go   ^r rename   M-r auto-name   ^x close   ^t reopen   M-↑↓ move   M-n new   ^o preview   M-j/k size   pfx ( ) session   Esc clear/close   F1 keys";
pub const FOOTER_EDIT: &str = " ⏎ save   Esc cancel   (empty name cancels)";
pub const FOOTER_NEW: &str = " ⏎ create   Esc cancel   (empty = automatic name)";
pub const FOOTER_CLOSE: &str = " y close   any other key or Esc cancels";

/// A session page, as the tab strip shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub sid: String,
    pub name: String,
    /// The invoking client's session.
    pub client: bool,
    /// Its windows that need you.
    pub attn: usize,
}

/// One line of the list: a window row, or one of its pane rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    /// Index into `App::rows`.
    pub row: usize,
    /// Index into that row's `panes`; `None` for the window row.
    pub pane: Option<usize>,
}

pub struct App {
    pub rows: Vec<Row>,
    /// The session pages, by name.
    pub tabs: Vec<Tab>,
    /// The page shown: a session ID.
    pub tab: Option<String>,
    /// Which rows match the filter (every page).
    matched: Vec<bool>,
    /// The page's matching rows as list lines, in list order: the page's
    /// NEEDS YOU copies, then its windows, each followed by its panes.
    pub visible: Vec<Item>,
    pub filter: LineEdit,
    /// Position in `visible`.
    pub sel: usize,
    /// Key of the selection (`Row::key`, `/<pane id>` added on a pane
    /// row), kept across updates.
    pub sel_key: Option<String>,
    /// Until when a `sel_key` missing from the rows is still wanted (a
    /// window the popup itself just made); see `want`.
    want_until: Option<Instant>,
    pub mode: Mode,
    pub notice: Option<String>,
    pub preview_flip: bool,
    /// The preview's share of the list and preview area, in percent, set
    /// with M-j/M-k; None: sized to the list (see `popup::preview_height`).
    pub preview_pct: Option<u16>,
    /// Rows of the list and preview area at the last draw (0: not drawn).
    pub split_area: u16,
    /// Rows of the preview at the last draw; None: hidden.
    pub split_prev: Option<u16>,
    /// `$HOME`, for shortening paths in the preview's cards.
    pub home: String,
    /// The host name: a pane title that is just this isn't shown.
    pub host: String,
    /// List lines per PgUp/PgDn.
    pub page: usize,
    /// Preview scroll of the selected pane: lines above the bottom (0 =
    /// the latest output). Back to 0 whenever the selection changes.
    pub preview_scroll: usize,
    /// Rows of the selected pane the preview showed at the last draw (0:
    /// hidden, or a window row is selected: nothing scrolls).
    pub preview_rows: usize,
    /// The list's screen rows at the last draw: (y, position in
    /// `visible`) for each row, and the list's x range.
    pub list_hits: Vec<(u16, usize)>,
    pub list_x: std::ops::Range<u16>,
    /// tmux's prefix and the open keys (see `Chord`).
    pub chord: Option<Chord>,
    /// The last key was the chord's prefix.
    armed: bool,
    matcher: Matcher,
}

/// The list and the preview each keep at least this many rows, when the
/// area has room for both.
pub const PREVIEW_MIN_ROWS: u16 = 5;

/// Percentage points M-j / M-k move the list/preview divider.
pub const PREVIEW_STEP: u16 = 5;

/// The preview's rows for `pct` percent of `avail` rows (before clamping).
pub fn pct_rows(avail: u16, pct: u16) -> u16 {
    (u32::from(avail) * u32::from(pct.min(100)) / 100) as u16
}

/// `@home-preview-pct` as the popup takes it: 1..=99, else auto.
pub fn parse_preview_pct(v: &str) -> Option<u16> {
    v.trim().parse().ok().filter(|p| (1..=99).contains(p))
}

/// Lines the preview moves per mouse-wheel notch.
pub const WHEEL_LINES: usize = 3;

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

/// The session pages of `rows`, by name.
fn tabs_of(rows: &[Row]) -> Vec<Tab> {
    let mut tabs: Vec<Tab> = Vec::new();
    for r in rows.iter().filter(|r| !r.pinned) {
        let i = match tabs.iter().position(|t| t.sid == r.sid) {
            Some(i) => i,
            None => {
                tabs.push(Tab {
                    sid: r.sid.clone(),
                    name: r.session.clone(),
                    client: false,
                    attn: 0,
                });
                tabs.len() - 1
            }
        };
        tabs[i].client |= r.current;
        tabs[i].attn += usize::from(r.needs_you());
    }
    tabs.sort_by(|a, b| a.name.cmp(&b.name));
    tabs
}

impl App {
    pub fn new() -> App {
        App {
            rows: vec![],
            tabs: vec![],
            tab: None,
            matched: vec![],
            visible: vec![],
            filter: LineEdit::default(),
            sel: 0,
            sel_key: None,
            want_until: None,
            mode: Mode::List,
            notice: None,
            preview_flip: false,
            preview_pct: None,
            split_area: 0,
            split_prev: None,
            home: String::new(),
            host: String::new(),
            page: 10,
            preview_scroll: 0,
            preview_rows: 0,
            list_hits: vec![],
            list_x: 0..0,
            chord: None,
            armed: false,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn selected_item(&self) -> Option<Item> {
        self.visible.get(self.sel).copied()
    }

    /// The selected line's window (a pane row's too).
    pub fn selected(&self) -> Option<&Row> {
        self.selected_item().map(|it| &self.rows[it.row])
    }

    /// The selected pane row's pane; `None` on a window row.
    pub fn selected_pane(&self) -> Option<&PaneRow> {
        let it = self.selected_item()?;
        self.rows[it.row].panes.get(it.pane?)
    }

    pub fn item_key(&self, it: Item) -> String {
        let r = &self.rows[it.row];
        match it.pane {
            Some(p) => format!("{}/{}", r.key, r.panes[p].id),
            None => r.key.clone(),
        }
    }

    /// The page's tab position (0 when there is none).
    fn tab_pos(&self, sid: &str) -> usize {
        self.tabs.iter().position(|t| t.sid == sid).unwrap_or(0)
    }

    /// `Switch` to window `index` of the client's session (no client: the
    /// page shown), with the window's own active pane; `None` when it has
    /// no window at that index.
    fn window_switch(&mut self, index: u32) -> Action {
        let sid = match self.tabs.iter().find(|t| t.client) {
            Some(t) => t.sid.clone(),
            None => match &self.tab {
                Some(s) => s.clone(),
                None => return Action::None,
            },
        };
        match self
            .rows
            .iter()
            .find(|r| !r.pinned && r.sid == sid && r.index == index)
        {
            Some(r) => Action::Switch {
                sid: r.sid.clone(),
                wid: r.wid.clone(),
                pane: None,
            },
            None => Action::None,
        }
    }

    /// The page the popup opens on: the client's session, else the first.
    fn home_tab(&self) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.client)
            .or(self.tabs.first())
            .map(|t| t.sid.clone())
    }

    /// New rows from a snapshot. The page stays (unless its session went),
    /// the selection stays on its line (a pane row whose pane went: its
    /// window), or at its position if the window went; an open editor is
    /// left alone unless its window vanished.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.set_rows_at(rows, Instant::now());
    }

    /// `set_rows` at time `now`.
    pub fn set_rows_at(&mut self, rows: Vec<Row>, now: Instant) {
        self.rows = rows;
        self.tabs = tabs_of(&self.rows);
        if !self
            .tab
            .as_ref()
            .is_some_and(|t| self.tabs.iter().any(|x| &x.sid == t))
        {
            self.tab = self.home_tab();
        }
        self.refilter();
        let wanted = self.want_until.is_some_and(|t| now < t);
        match self.sel_key.clone().and_then(|k| self.resolve(&k)) {
            Some(p) => {
                self.sel = p;
                self.clamp();
            }
            None => {
                // a window the popup just made or reopened (`want`) can be
                // missing from a snapshot read before the write, or be on
                // another page: keep wanting it for a while
                let other = self
                    .sel_key
                    .as_deref()
                    .filter(|_| wanted)
                    .and_then(|k| self.rows.iter().find(|r| r.key == k && !r.pinned))
                    .map(|r| r.sid.clone());
                match other {
                    Some(sid) => {
                        self.tab = Some(sid);
                        self.layout();
                        let k = self.sel_key.clone().unwrap_or_default();
                        if let Some(p) = self.pos_of(&k) {
                            self.set_sel(p);
                        }
                    }
                    None if wanted => self.clamp_pos(),
                    // otherwise the window has gone (its ID is never
                    // reused): the selection becomes the row now under
                    // the cursor
                    None => {
                        self.want_until = None;
                        self.clamp();
                    }
                }
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

    /// The position of `key`'s line, or of what stands for it now: a
    /// pinned copy no longer pinned (answered) is its window's row, a
    /// pane row whose pane went is its window's.
    fn resolve(&self, key: &str) -> Option<usize> {
        let unpinned = key.strip_prefix('!').unwrap_or(key);
        let win = |k: &str| k.split('/').next().unwrap_or(k).to_string();
        [
            key.to_string(),
            unpinned.to_string(),
            win(key),
            win(unpinned),
        ]
        .iter()
        .find_map(|k| self.pos_of(k))
    }

    /// Open on the client's session, its current window selected (no
    /// client: the first session, its active window).
    pub fn select_current(&mut self) {
        self.tab = self.home_tab();
        self.layout();
        let p = self
            .window_pos(|r| r.current)
            .or(self.window_pos(|r| r.active));
        self.set_sel(p.unwrap_or(0));
    }

    /// Position of the first window row (not a pinned copy) on the page
    /// for which `f` holds.
    fn window_pos(&self, f: impl Fn(&Row) -> bool) -> Option<usize> {
        self.visible.iter().position(|it| {
            let r = &self.rows[it.row];
            it.pane.is_none() && !r.pinned && f(r)
        })
    }

    /// Select the line with this key, paging to its session if needed.
    pub fn select_key(&mut self, key: &str) {
        if self.pos_of(key).is_none() {
            let row_key = key.split('/').next().unwrap_or(key);
            if let Some(sid) = self
                .rows
                .iter()
                .find(|r| r.key == row_key)
                .map(|r| r.sid.clone())
            {
                self.tab = Some(sid);
                self.layout();
            }
        }
        if let Some(p) = self.pos_of(key) {
            self.set_sel(p);
        }
    }

    pub fn select_wid(&mut self, wid: &str) {
        self.select_key(wid);
    }

    /// Select `wid`, a window the popup itself just created, even if the
    /// rows don't show it yet; it is wanted for `WANT_FOR`.
    pub fn want(&mut self, wid: &str) {
        self.want_at(wid, Instant::now());
    }

    /// `want` at time `now`.
    pub fn want_at(&mut self, wid: &str, now: Instant) {
        self.select_key(wid);
        self.sel_key = Some(wid.to_string());
        self.want_until = Some(now + WANT_FOR);
    }

    /// Position of the line with this key on the page.
    fn pos_of(&self, key: &str) -> Option<usize> {
        self.visible.iter().position(|&it| self.item_key(it) == key)
    }

    fn set_sel(&mut self, p: usize) {
        self.sel = p;
        self.clamp();
    }

    fn clamp(&mut self) {
        self.clamp_pos();
        let key = self.selected_item().map(|it| self.item_key(it));
        if key != self.sel_key {
            // a new selection's preview starts at its latest output
            self.preview_scroll = 0;
        }
        self.sel_key = key;
    }

    /// Scrolls the selected pane's preview `d` lines back (up) or forward
    /// (down); nothing when the preview is hidden or a window row is
    /// selected. The top is clamped at the next draw.
    pub fn scroll_preview(&mut self, d: isize) {
        if self.preview_rows == 0 || self.selected_pane().is_none() {
            return;
        }
        self.preview_scroll = self.preview_scroll.saturating_add_signed(d);
    }

    /// A mouse event: the wheel scrolls the preview wherever the pointer
    /// is (never the list); a left click on a row selects it (list mode
    /// only). Nothing else does anything.
    pub fn mouse(&mut self, m: MouseEvent) -> Action {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_preview(WHEEL_LINES as isize),
            MouseEventKind::ScrollDown => self.scroll_preview(-(WHEEL_LINES as isize)),
            MouseEventKind::Down(MouseButton::Left)
                if self.mode == Mode::List && self.list_x.contains(&m.column) =>
            {
                match self.list_hits.iter().find(|(y, _)| *y == m.row) {
                    Some(&(_, pos)) => self.set_sel(pos),
                    None => return Action::None,
                }
            }
            _ => return Action::None,
        }
        Action::Redraw
    }

    fn clamp_pos(&mut self) {
        if self.visible.is_empty() {
            self.sel = 0;
        } else if self.sel >= self.visible.len() {
            self.sel = self.visible.len() - 1;
        }
    }

    /// Matches every row against the filter, then lays out the page. A
    /// window matches if it or any of its panes does.
    fn refilter(&mut self) {
        let q = Query::parse(&self.filter.as_string());
        if q.is_empty() {
            self.matched = vec![true; self.rows.len()];
        } else {
            let pat = Pattern::parse(&q.text, CaseMatching::Smart, Normalization::Smart);
            let mut buf = Vec::new();
            let m = &mut self.matcher;
            let mut hit = |hay: &str| pat.score(Utf32Str::new(hay, &mut buf), m).is_some();
            self.matched = self
                .rows
                .iter()
                .map(|r| {
                    q.tokens_match(r)
                        && (q.text.is_empty()
                            || hit(&r.hay)
                            || r.panes.iter().any(|p| hit(&p.hay))
                            || q.prompt_match(r))
                })
                .collect();
        }
        self.layout();
    }

    /// The page's lines from `matched`: each matching window of the page
    /// (NEEDS YOU copies first, as `rows` has them), then all its panes.
    fn layout(&mut self) {
        let tab = self.tab.clone();
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(i, r)| self.matched.get(*i) == Some(&true) && Some(&r.sid) == tab.as_ref())
            .flat_map(|(i, r)| {
                std::iter::once(Item { row: i, pane: None }).chain((0..r.panes.len()).map(
                    move |p| Item {
                        row: i,
                        pane: Some(p),
                    },
                ))
            })
            .collect();
    }

    /// Windows on the page: (matching the filter, all). Pinned copies
    /// aren't counted twice.
    pub fn counts(&self) -> (usize, usize) {
        let on = |r: &Row| !r.pinned && Some(&r.sid) == self.tab.as_ref();
        let shown = self
            .visible
            .iter()
            .filter(|it| it.pane.is_none() && on(&self.rows[it.row]))
            .count();
        (shown, self.rows.iter().filter(|r| on(r)).count())
    }

    /// The key of the window the cursor should land on once `wid` is
    /// closed: the next window row below the selection (pinned copies and
    /// `wid`'s own lines skipped, as they go too); `None` at the end.
    pub fn next_after_close(&self, wid: &str) -> Option<String> {
        self.visible
            .iter()
            .skip(self.sel + 1)
            .find(|it| {
                let r = &self.rows[it.row];
                it.pane.is_none() && !r.pinned && r.wid != wid
            })
            .map(|&it| self.item_key(it))
    }

    /// Some visible row shows a live run time, so the clock must tick.
    pub fn ticking(&self) -> bool {
        self.visible.iter().any(|it| {
            self.rows[it.row].agents.as_ref().is_some_and(|a| {
                a.panes
                    .iter()
                    .any(|p| p.live() && p.state.run_started.is_some())
            })
        })
    }

    /// `^g`: the next window needing you (that the filter lets through),
    /// after the selection, page by page, cycling; it is visited in its
    /// page's NEEDS YOU group, paging there if it is on another session.
    fn jump_attention(&mut self) {
        let mut stops: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                !r.pinned && self.matched.get(i) == Some(&true) && r.needs_you()
            })
            .collect();
        let at = |a: &App, i: usize| (a.tab_pos(&a.rows[i].sid), a.rows[i].index);
        stops.sort_by_key(|&i| at(self, i));
        if stops.is_empty() {
            self.notice = Some(" nothing needs you".into());
            return;
        }
        let next = match self.selected_item() {
            Some(it) => {
                let cur = &self.rows[it.row];
                match stops.iter().position(|&i| self.rows[i].wid == cur.wid) {
                    Some(k) => stops[(k + 1) % stops.len()],
                    None => {
                        let here = (self.tab_pos(&cur.sid), cur.index);
                        *stops
                            .iter()
                            .find(|&&i| at(self, i) > here)
                            .unwrap_or(&stops[0])
                    }
                }
            }
            None => stops[0],
        };
        let key = format!("!{}", self.rows[next].wid);
        self.select_key(&key);
    }

    /// `prefix )` / `prefix (`: the next / previous session's page
    /// (cycling), on its current window; the filter is kept.
    fn page_tab(&mut self, d: isize) {
        let n = self.tabs.len();
        if n == 0 {
            return;
        }
        let cur = self.tab.as_deref().map_or(0, |t| self.tab_pos(t));
        let next = (cur as isize + d).rem_euclid(n as isize) as usize;
        self.tab = Some(self.tabs[next].sid.clone());
        self.want_until = None;
        self.layout();
        let p = self.window_pos(|r| r.active);
        self.set_sel(p.unwrap_or(0));
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
        // --cycle, like the fzf version; within the page
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
        // Prefixed chords. The open-key chord closes, in every mode (an
        // unsaved rename or new-window name is dropped, a pending close is
        // not done); `prefix )` / `prefix (` page, in the list. The prefix
        // itself is still handled as usual below (^a: cursor to the
        // start), so anything else after it acts as it always did.
        if let Some(c) = &self.chord {
            let armed = std::mem::take(&mut self.armed);
            if armed && c.is_home_key(&k) {
                return Action::Quit;
            }
            if armed
                && self.mode == Mode::List
                && let Some(d) = c.page_dir(&k)
            {
                self.page_tab(d);
                // the prefix may have moved the filter's cursor (^a)
                self.filter.cur = self.filter.text.len();
                return Action::Redraw;
            }
            self.armed = c.is_prefix(&k);
        }
        // Switch keys (C-M-S-F1 … F10, the terminal's Cmd+Opt+1 … 0; see
        // `SWITCH_FKEYS`): go to
        // that window of the client's session, from any mode, as the root
        // binding does outside the popup (tmux gives the popup its keys
        // first). An index with no window does nothing.
        if let Some(n) = switch_key(&k) {
            return self.window_switch(n);
        }
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
        // window actions act on the selected line's window, pane row or not
        let sel = self.selected().cloned();
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let half = (self.preview_rows / 2).max(1) as isize;
        match k.code {
            // the preview scrolls on Shift; the list never does
            KeyCode::Up if shift && !alt && !ctrl => self.scroll_preview(1),
            KeyCode::Down if shift && !alt && !ctrl => self.scroll_preview(-1),
            KeyCode::PageUp if shift => self.scroll_preview(half),
            KeyCode::PageDown if shift => self.scroll_preview(-half),
            KeyCode::Up if alt => return self.swap_action(-1),
            KeyCode::Down if alt => return self.swap_action(1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char('p') | KeyCode::Char('k') if ctrl => self.move_by(-1),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => self.move_by(1),
            KeyCode::PageUp => self.page_by(-1),
            KeyCode::PageDown => self.page_by(1),
            KeyCode::Enter => {
                let pane = self.selected_pane().map(|p| p.id.clone());
                return match sel {
                    Some(r) => Action::Switch {
                        pane: pane.or_else(|| r.agents.as_ref().map(|a| a.lead().pane.clone())),
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
            KeyCode::Char('k') if alt && !ctrl => return self.resize_preview(true),
            KeyCode::Char('j') if alt && !ctrl => return self.resize_preview(false),
            KeyCode::Char('=') if alt && !ctrl => {
                self.preview_pct = None;
                return Action::PreviewPct(None);
            }
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

    /// M-k (`up`: the divider up, the preview larger) / M-j (down,
    /// smaller), by `PREVIEW_STEP` points of the area at the last draw,
    /// from the split shown (auto or set); the list and the preview keep
    /// `PREVIEW_MIN_ROWS` each, and nothing moves when they can't. M-k on
    /// a hidden preview shows it (as ^o does); M-j never hides it.
    fn resize_preview(&mut self, up: bool) -> Action {
        let avail = self.split_area;
        let Some(prev) = self.split_prev else {
            if up && avail > 0 {
                self.preview_flip = !self.preview_flip;
                return Action::Redraw;
            }
            return Action::None;
        };
        let fits = |p: &u16| {
            let r = pct_rows(avail, *p);
            r >= PREVIEW_MIN_ROWS && avail - r >= PREVIEW_MIN_ROWS
        };
        let (Some(lo), Some(hi)) = ((1..=99).find(fits), (1..=99).rev().find(fits)) else {
            return Action::None;
        };
        let cur = self.preview_pct.unwrap_or_else(|| {
            ((u32::from(prev) * 100 + u32::from(avail) / 2) / u32::from(avail)) as u16
        });
        let new = if up {
            cur.saturating_add(PREVIEW_STEP)
        } else {
            cur.saturating_sub(PREVIEW_STEP)
        }
        .clamp(lo, hi);
        if self.preview_pct == Some(new) {
            return Action::None;
        }
        self.preview_pct = Some(new);
        Action::PreviewPct(Some(new))
    }

    /// M-↑/M-↓: swap the selected line's window with the window above /
    /// below it on the page (pinned copies never move).
    fn swap_action(&mut self, d: isize) -> Action {
        let Some(it) = self.selected_item() else {
            return Action::None;
        };
        if self.rows[it.row].pinned {
            return Action::None;
        }
        let wins: Vec<usize> = self
            .visible
            .iter()
            .filter(|w| w.pane.is_none() && !self.rows[w.row].pinned)
            .map(|w| w.row)
            .collect();
        let Some(i) = wins.iter().position(|&r| r == it.row) else {
            return Action::None;
        };
        let j = i as isize + d;
        if j < 0 || j as usize >= wins.len() {
            return Action::None;
        }
        let (cur, other) = (&self.rows[it.row], &self.rows[wins[j as usize]]);
        if other.sid != cur.sid {
            return Action::None;
        }
        Action::Swap {
            a: cur.wid.clone(),
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

/// The F1 page: the keys, and the legends (the columns, then the git
/// badge).
pub fn help() -> (String, String) {
    (
        HELP.to_string(),
        format!("{COLUMNS}\n\n{}", crate::git::badge::LEGEND),
    )
}

pub const HELP: &str = "tmux-home — keys   (press any key to return)

  type            filter the session's windows (name, command, path,
                  agent prompt; a window matching by a pane shows all its
                  panes); tokens narrow it, combined with the text:
                  @attn @agent @running @waiting @background @idle @error
  prefix )  (     next / previous session page (the filter is kept)
  ↑ ↓  ^p ^n  ^k ^j   move selection over windows and panes
  PgUp PgDn       page
  S-↑ S-↓         scroll the selected pane in the preview a line (the
                  mouse wheel: 3 lines); nothing on a window row
  S-PgUp S-PgDn   scroll it half a page; moving the selection returns it
                  to the latest output
  ← →  Home End   edit the filter
  ⏎               switch to the selected window (on a pane row: and that
                  pane) and close
  ^g              jump to the next window that needs you, on any page
  Esc             clear the filter; close if it is already empty
  prefix + key    the key that opened the popup closes it, from any mode
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
  M-k M-j         preview larger / smaller (the list/preview divider up /
                  down 5%; each keeps 5 rows); M-k shows a hidden
                  preview; remembered across opens (@home-preview-pct)
  M-=             back to the automatic size: the list's lines plus 2,
                  the preview the rest (half if it would get under a third)
  C-M-S-F1…F10    go to window 1…10 of your session and close (F12 for
                  window 3; sent by the terminal for Cmd+Opt+1…0; nothing
                  if there is no such window)
  F1  ^/          this help
  (window keys act on a pane row's window)";

pub const COLUMNS: &str = "  columns         window row / pane row
  #               window index / ├ .N └ .N pane index
  NM              window name / pane title (blank: the host name)
  CM              — / the pane's command
  ST              — / the agent's state, icon and word (● running
                  ◐ waiting ✕ error ◎ background ○ idle; dim with
                  (ended?): the agent has gone)
  T               — / the agent's run time
  PA              — / the pane's directory
  GT              — / git badge of the pane's directory (below)";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::snapshot::{Client, Pane, Session, Window};
    use ratatui::crossterm::event::KeyEvent;

    /// Sessions alpha ($0: editor @0, win two @1) and beta ($1: logs @2
    /// with a sidebar pane, build @3), one pane each (%0..%3); the client
    /// is on beta.
    fn snap() -> Snapshot {
        let mut s = Snapshot::default();
        for (sid, name) in [("$0", "alpha"), ("$1", "beta")] {
            s.sessions.push(Session {
                pinned: false,
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
                sidebar_off: false,
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
                active: true,
                current_command: "fish".into(),
                current_path: "/home/u/dev/x".into(),
                ..Default::default()
            });
        }
        // a sidebar pane never describes the row, and gets no pane row
        s.panes.push(Pane {
            id: "%9".into(),
            window_id: "@2".into(),
            session_id: "$1".into(),
            index: 1,
            active: true,
            current_command: "sidebar".into(),
            current_path: "/".into(),
            role: "sidebar".into(),
            ..Default::default()
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
    fn alt(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::ALT)
    }
    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            app.key(key(KeyCode::Char(c)));
        }
    }
    /// The keys of every visible line (windows and panes).
    fn keys_visible(app: &App) -> Vec<String> {
        app.visible.iter().map(|&it| app.item_key(it)).collect()
    }
    /// The keys of the visible window rows.
    fn wins(app: &App) -> String {
        app.visible
            .iter()
            .filter(|it| it.pane.is_none())
            .map(|&it| app.item_key(it))
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn sel(app: &App) -> String {
        app.sel_key.clone().unwrap_or_default()
    }

    fn rows_of(s: &Snapshot) -> Vec<Row> {
        build_rows(s, Some("/dev/ttys1"), "/home/u")
    }

    fn app_of(s: &Snapshot) -> App {
        let mut a = App::new();
        a.set_rows(rows_of(s));
        a.select_current();
        a
    }

    fn app() -> App {
        app_of(&snap())
    }

    /// Claude Code runs as its version: a tagged agent pane shows the
    /// agent's name instead, nothing else changes.
    #[test]
    fn display_cmd_names_version_named_agents() {
        let tagged = |cmd: &str, opts: &[(&str, &str)]| {
            let mut p = Pane {
                current_command: cmd.into(),
                ..Default::default()
            };
            for (k, v) in opts {
                p.agent_opts.insert(k.to_string(), v.to_string());
            }
            p
        };
        let p = |cmd: &str, tag: Option<&str>| match tag {
            Some(t) => tagged(cmd, &[("@home_agent", t)]),
            None => tagged(cmd, &[]),
        };
        assert_eq!(display_cmd(&p("2.1.296", Some("claude"))), "claude");
        assert_eq!(display_cmd(&p("node", Some("claude"))), "node");
        assert_eq!(display_cmd(&p("2.1", Some("claude"))), "2.1");
        assert_eq!(display_cmd(&p("2.1.296", None)), "2.1.296");
        assert_eq!(display_cmd(&p("fish", None)), "fish");
        // older sessions are tagged with the legacy @pane_agent instead;
        // @home_agent wins when both are set
        let legacy = [("@pane_agent", "codex")];
        assert_eq!(display_cmd(&tagged("2.1.296", &legacy)), "codex");
        assert_eq!(display_cmd(&tagged("fish", &legacy)), "fish");
        let both = [("@home_agent", "claude"), ("@pane_agent", "codex")];
        assert_eq!(display_cmd(&tagged("2.1.296", &both)), "claude");
        let empty = [("@home_agent", ""), ("@pane_agent", "claude")];
        assert_eq!(display_cmd(&tagged("2.1.296", &empty)), "claude");
        // in the rows: the pane's command, the hay and an automatic name
        let mut s = snap();
        s.panes[3] = Pane {
            current_command: "2.1.296".into(),
            ..s.panes[3].clone()
        };
        s.panes[3]
            .agent_opts
            .insert("@home_agent".into(), "claude".into());
        s.windows[3].name = "2.1.296".into();
        s.windows[3].automatic_rename = true;
        let rows = rows_of(&s);
        let build = rows.iter().find(|r| r.wid == "@3").unwrap();
        assert_eq!(
            (build.cmd.as_str(), build.name.as_str()),
            ("claude", "claude")
        );
        assert_eq!(build.panes[0].cmd, "claude");
        assert!(build.panes[0].hay.starts_with("claude "));
        assert!(!build.hay.contains("2.1.296"));
        // a name typed by hand is kept, even if it looks like a version
        s.windows[3].automatic_rename = false;
        assert_eq!(rows_of(&s)[1].name, "2.1.296");
    }

    #[test]
    fn opens_on_the_client_session_page() {
        let a = app();
        let tabs: Vec<_> = a.tabs.iter().map(|t| (t.name.as_str(), t.client)).collect();
        assert_eq!(tabs, [("alpha", false), ("beta", true)], "tabs by name");
        assert_eq!(a.tab.as_deref(), Some("$1"));
        assert_eq!(keys_visible(&a), ["@2", "@2/%2", "@3", "@3/%3"]);
        assert_eq!(sel(&a), "@2");
        assert_eq!(a.counts(), (2, 2), "the page's windows");
        let logs = a.selected().unwrap();
        assert_eq!(logs.pane.as_deref(), Some("%2"));
        assert_eq!(logs.path, "~/d/x");
    }

    #[test]
    fn switch_keys_go_to_a_window_of_the_clients_session_from_any_page_and_mode() {
        let wk = |n| KeyEvent::new(KeyCode::F(n), SWITCH_KEY_MODS);
        assert_eq!(switch_key(&wk(1)), Some(1));
        assert_eq!(switch_key(&wk(12)), Some(3));
        assert_eq!(switch_key(&wk(3)), None, "unreadable: F12 stands in");
        assert_eq!(switch_key(&wk(10)), Some(10));
        assert_eq!(switch_key(&wk(11)), None);
        assert_eq!(switch_key(&key(KeyCode::F(1))), None, "plain F1 is help");
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(switch_key(&KeyEvent::new(KeyCode::F(2), ctrl_alt)), None);

        let mut a = app();
        let to_build = Action::Switch {
            sid: "$1".into(),
            wid: "@3".into(),
            pane: None,
        };
        // the client is on beta ($1): its window 1 is @3, from any page
        a.page_tab(-1);
        assert_eq!(a.tab.as_deref(), Some("$0"));
        assert_eq!(a.key(wk(1)), to_build);
        // a missing index does nothing
        assert_eq!(a.key(wk(7)), Action::None);
        // in an editor too, dropping the edit
        a.key(ctrl('r'));
        assert!(matches!(a.mode, Mode::Rename { .. }));
        assert_eq!(a.key(wk(1)), to_build);
        // with a filter that hides the window
        let mut a = app();
        typed(&mut a, "logs");
        assert_eq!(a.key(wk(1)), to_build);
    }

    #[test]
    fn move_wraps_within_the_page_over_windows_and_panes() {
        let mut a = app();
        for want in ["@2/%2", "@3", "@3/%3", "@2"] {
            a.key(key(KeyCode::Down));
            assert_eq!(sel(&a), want);
        }
        a.key(ctrl('k'));
        assert_eq!(sel(&a), "@3/%3", "cycles, never to another page");
        a.key(ctrl('p'));
        a.key(ctrl('j'));
        a.key(ctrl('n'));
        assert_eq!(sel(&a), "@2");
        assert_eq!(a.selected_pane(), None);
        a.key(key(KeyCode::Down));
        assert_eq!(a.selected_pane().unwrap().id, "%2");
        assert_eq!(a.selected().unwrap().wid, "@2", "a pane row's window");
    }

    #[test]
    fn filter_enter_and_esc() {
        let mut a = app();
        typed(&mut a, "bui");
        assert_eq!(keys_visible(&a), ["@3", "@3/%3"], "a window with its panes");
        assert_eq!(sel(&a), "@3");
        assert_eq!(a.counts(), (1, 2));
        // the selection is kept by key across an update
        a.key(key(KeyCode::Down));
        a.set_rows(rows_of(&snap()));
        assert_eq!(sel(&a), "@3/%3");
        // ⏎ on a pane row: its window, and that pane
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Action::Switch {
                sid: "$1".into(),
                wid: "@3".into(),
                pane: Some("%3".into()),
            }
        );
        // on a window row: the window, its panes left alone
        a.key(key(KeyCode::Up));
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Action::Switch {
                sid: "$1".into(),
                wid: "@3".into(),
                pane: None,
            }
        );
        // the filter is the page's: alpha's windows don't show
        a.clear_filter();
        typed(&mut a, "two");
        assert!(a.visible.is_empty());
        // Esc clears, then closes
        assert_eq!(a.key(key(KeyCode::Esc)), Action::Redraw);
        assert_eq!(a.visible.len(), 4);
        assert_eq!(a.key(key(KeyCode::Esc)), Action::Quit);
    }

    /// A window is shown when it or any of its panes matches, then with
    /// all its panes.
    #[test]
    fn a_pane_match_shows_its_window_and_every_pane() {
        let mut s = snap();
        let mut vim = s.panes[3].clone();
        vim.id = "%7".into();
        vim.index = 1;
        vim.active = false;
        vim.current_command = "vim".into();
        vim.title = "notes.md".into();
        s.panes.push(vim);
        let mut a = app_of(&s);
        typed(&mut a, "vim");
        assert_eq!(keys_visible(&a), ["@3", "@3/%3", "@3/%7"]);
        a.clear_filter();
        typed(&mut a, "notes");
        assert_eq!(wins(&a), "@3", "titles match too");
        let rows = rows_of(&s);
        let build = rows.iter().find(|r| r.wid == "@3").unwrap();
        let ids: Vec<_> = build.panes.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["%3", "%7"]);
        assert_eq!(build.cmd, "fish", "the active pane describes the window");
    }

    #[test]
    fn rename_mode() {
        let mut a = app();
        typed(&mut a, "bui");
        a.key(key(KeyCode::Down)); // the pane row: acts on its window
        a.key(ctrl('r'));
        assert!(
            matches!(&a.mode, Mode::Rename { wid, edit } if wid == "@3" && edit.as_string() == "build")
        );
        // list keys don't act while editing; a snapshot leaves the editor alone
        a.key(ctrl('x'));
        a.set_rows(rows_of(&snap()));
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
        a.set_rows(rows_of(&s));
        assert_eq!(a.mode, Mode::List);
        assert!(a.notice.is_some());
    }

    #[test]
    fn one_row_per_window_with_its_panes() {
        let rows = rows_of(&snap());
        assert_eq!(rows.len(), 4, "the sidebar pane adds no row");
        let logs = rows.iter().find(|r| r.name == "logs").unwrap();
        assert_eq!((logs.sid.as_str(), logs.wid.as_str()), ("$1", "@2"));
        // the active pane is a sidebar: the row describes the main pane
        assert_eq!(logs.pane.as_deref(), Some("%2"));
        assert_eq!(logs.cmd, "fish");
        assert!(logs.active);
        let panes: Vec<_> = logs.panes.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(panes, ["%2"], "no pane row for the sidebar");
        assert_eq!(logs.panes[0].path, "~/d/x");
    }

    #[test]
    fn no_client_opens_on_the_first_session() {
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
        let mut a = App::new();
        a.set_rows(rows);
        a.select_current();
        assert_eq!(a.tab.as_deref(), Some("$0"));
        assert_eq!(sel(&a), "@0", "its active window");
        assert!(a.tabs.iter().all(|t| !t.client));
    }

    #[test]
    fn tabs_follow_the_client_and_the_page_stays() {
        let mut a = app();
        let cur: Vec<_> = a
            .rows
            .iter()
            .filter(|r| r.current)
            .map(|r| &r.wid)
            .collect();
        assert_eq!(cur, ["@2"]);
        // the client moves to alpha: its mark follows, the page stays
        let mut s = snap();
        s.clients[0].session_id = "$0".into();
        a.set_rows(rows_of(&s));
        assert!(a.tabs[0].client && !a.tabs[1].client);
        assert_eq!(a.tab.as_deref(), Some("$1"));
        assert_eq!(sel(&a), "@2");
        // the page's session goes: back to the client's
        s.windows.retain(|w| w.session_id != "$1");
        s.sessions.retain(|x| x.id != "$1");
        a.set_rows(rows_of(&s));
        assert_eq!(a.tab.as_deref(), Some("$0"));
        assert_eq!(a.tabs.len(), 1);
    }

    #[test]
    fn typing_filters_and_returns_to_top() {
        let mut a = app();
        a.key(key(KeyCode::Down));
        a.key(key(KeyCode::Down));
        typed(&mut a, "bui");
        assert_eq!(wins(&a), "@3");
        assert_eq!(a.sel, 0);
        // fzf-syntax characters are literal
        let mut rows = rows_of(&snap());
        rows[0].name = "renamed (x)+y, z".into();
        rows[0].hay = "0 renamed (x)+y, z fish ~/d/x".into();
        a.set_rows(rows);
        a.clear_filter();
        typed(&mut a, "(x)+");
        assert_eq!(wins(&a), "@2");
    }

    #[test]
    fn alt_r_resets_the_selected_lines_window() {
        let mut a = app();
        a.key(key(KeyCode::Down));
        assert_eq!(
            a.key(alt(KeyCode::Char('r'))),
            Action::ResetName("@2".into())
        );
        a.key(key(KeyCode::Down));
        assert_eq!(
            a.key(alt(KeyCode::Char('r'))),
            Action::ResetName("@3".into())
        );
        assert_eq!(a.mode, Mode::List, "the popup stays in the list");
    }

    #[test]
    fn preview_pct_option_values() {
        assert_eq!(parse_preview_pct("35\n"), Some(35));
        for v in ["", "0", "100", "-5", "auto", "50%"] {
            assert_eq!(parse_preview_pct(v), None, "{v:?}");
        }
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
        a.key(key(KeyCode::Down));
        assert_eq!(
            a.key(ctrl('x')),
            Action::CloseStart("@2".into()),
            "from a pane row: its window"
        );
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
        a.key(key(KeyCode::Down));
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
            sidebar_off: false,
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
    /// back to that window with the next snapshot. The window is in
    /// another session: the page follows it.
    #[test]
    fn selection_survives_a_stale_snapshot() {
        let t0 = Instant::now();
        let mut a = app();
        a.set_rows_at(rows_of(&with_made()), t0);
        a.want_at("@7", t0);
        assert_eq!(a.tab.as_deref(), Some("$0"), "paged to alpha");
        a.set_rows_at(rows_of(&snap()), t0);
        assert_eq!(a.sel_key.as_deref(), Some("@7"));
        a.set_rows_at(rows_of(&with_made()), t0);
        assert_eq!(sel(&a), "@7");
        // made from the other page and shown only later: paged to as well
        let mut a = app();
        a.set_rows_at(rows_of(&snap()), t0);
        a.want_at("@7", t0);
        assert_eq!(a.tab.as_deref(), Some("$1"));
        a.set_rows_at(rows_of(&with_made()), t0);
        assert_eq!(a.tab.as_deref(), Some("$0"));
        assert_eq!(sel(&a), "@7");
    }

    /// The wanting is bounded: once `WANT_FOR` has passed, a window still
    /// missing is given up and the line under the cursor is selected.
    #[test]
    fn a_wanted_window_is_given_up_after_a_while() {
        let t0 = Instant::now();
        let mut a = app();
        a.set_rows_at(rows_of(&with_made()), t0);
        a.want_at("@7", t0);
        let pos = a.sel;
        let later = t0 + WANT_FOR;
        a.set_rows_at(rows_of(&snap()), later);
        let under = sel(&a);
        assert_ne!(under, "@7");
        assert_eq!(a.sel, pos.min(a.visible.len() - 1));
        // and it is not taken back when the window shows up again
        a.set_rows_at(rows_of(&with_made()), later);
        assert_eq!(sel(&a), under);
    }

    /// A selected window closed elsewhere isn't wanted: the selection moves
    /// to the line now under the cursor and follows it; a selected pane
    /// that goes hands the selection to its window.
    #[test]
    fn a_vanished_window_or_pane_hands_the_selection_on() {
        let mut a = app();
        a.select_wid("@0"); // alpha/editor, first row of alpha
        assert_eq!(a.tab.as_deref(), Some("$0"), "select pages");
        let mut s = snap();
        s.windows.retain(|w| w.id != "@0");
        a.set_rows(rows_of(&s));
        let under = sel(&a);
        assert_eq!(under, "@1");
        // a window appears above it: the selection follows its row
        s.windows.push(Window {
            sidebar_off: false,
            id: "@8".into(),
            session_id: "$0".into(),
            index: 0,
            name: "new first".into(),
            automatic_rename: false,
            active: false,
        });
        s.windows
            .iter_mut()
            .filter(|w| w.session_id == "$0" && w.id != "@8")
            .for_each(|w| w.index += 1);
        a.set_rows(rows_of(&s));
        assert_eq!(sel(&a), under);
        // its pane is selected, then goes: the window row
        a.key(key(KeyCode::Down));
        assert_eq!(sel(&a), "@1/%1");
        s.panes.retain(|p| p.id != "%1");
        a.set_rows(rows_of(&s));
        assert_eq!(sel(&a), "@1");
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
        app_of(&agent_snap())
    }

    #[test]
    fn needs_you_rows_are_pinned_on_their_page() {
        let mut a = agent_app();
        // beta has nothing that needs you; alpha's tab counts one
        assert_eq!(wins(&a), "@2 @3");
        let attn: Vec<_> = a.tabs.iter().map(|t| t.attn).collect();
        assert_eq!(attn, [1, 0]);
        assert!(a.footer().contains("^g"));
        a.select_wid("@0");
        assert_eq!(
            keys_visible(&a),
            ["!@1", "!@1/%1", "@0", "@0/%0", "@1", "@1/%1"],
            "the copy with its panes, then the windows"
        );
        let panes = &a.rows[a.visible[1].row].panes;
        assert_eq!(
            panes[0].agent.as_ref().unwrap().state.status,
            crate::agent::Status::Waiting
        );
    }

    #[test]
    fn filter_tokens() {
        let mut a = agent_app();
        let q = |a: &mut App, s: &str| {
            a.clear_filter();
            typed(a, s);
            wins(a)
        };
        // beta's page
        assert_eq!(q(&mut a, "@running"), "@3");
        assert_eq!(q(&mut a, "@waiting"), "", "alpha's wait isn't on this page");
        assert_eq!(q(&mut a, "codex"), "@3");
        // alpha's page
        a.select_wid("@0");
        assert_eq!(q(&mut a, "@waiting"), "!@1 @1");
        assert_eq!(q(&mut a, "@running"), "");
        assert_eq!(
            q(&mut a, "@running @waiting"),
            "!@1 @1",
            "statuses are alternatives"
        );
        assert_eq!(q(&mut a, "@error"), "", "a stale error isn't an error");
        assert_eq!(q(&mut a, "@agent"), "!@1 @0 @1", "stale agents are agents");
        assert_eq!(q(&mut a, "@attn"), "!@1 @1");
        assert_eq!(q(&mut a, "@idle"), "");
        assert_eq!(q(&mut a, "@agent editor"), "@0");
        // the prompt is filterable (by words)
        assert_eq!(q(&mut a, "staging deploy"), "!@1 @1");
        assert_eq!(q(&mut a, "@bogus"), "", "unknown tokens are text");
        assert_eq!(q(&mut a, "s:alpha"), "", "s: is gone: paging replaces it");
        assert_eq!(q(&mut a, "@background"), "");
    }

    #[test]
    fn ctrl_g_cycles_windows_that_need_you_across_pages() {
        let mut s = agent_snap();
        // beta/logs (%2) needs you too
        s.panes[2].current_command = "node".into();
        s.panes[2]
            .agent_opts
            .insert("@pane_agent".into(), "claude".into());
        s.panes[2]
            .agent_opts
            .insert("@pane_attention".into(), "notification".into());
        let mut a = app_of(&s);
        assert_eq!(
            keys_visible(&a),
            ["!@2", "!@2/%2", "@2", "@2/%2", "@3", "@3/%3"]
        );
        assert_eq!(sel(&a), "@2");
        // from beta/logs: next is alpha/win two, paging there, in NEEDS YOU
        a.key(ctrl('g'));
        assert_eq!(sel(&a), "!@1");
        assert_eq!(a.tab.as_deref(), Some("$0"));
        a.key(ctrl('g'));
        assert_eq!(sel(&a), "!@2", "cycles, pinned copies only");
        assert_eq!(a.tab.as_deref(), Some("$1"));
        a.key(ctrl('g'));
        assert_eq!(sel(&a), "!@1");
        // from a pane row of a window that doesn't need you: the next one
        a.select_key("@0/%0");
        a.key(ctrl('g'));
        assert_eq!(sel(&a), "!@1");
        // nothing needs you: a notice, the selection stays
        let mut a = app();
        a.key(ctrl('g'));
        assert_eq!(sel(&a), "@2");
        assert!(a.footer().contains("nothing needs you"));
    }

    #[test]
    fn an_answered_pinned_row_hands_over_to_its_window() {
        let mut a = agent_app();
        a.key(ctrl('g'));
        assert_eq!(sel(&a), "!@1");
        // M-↑↓ never moves a pinned copy
        assert_eq!(a.key(alt(KeyCode::Down)), Action::None);
        // the agent was answered: the selection stays on the window
        let mut s = agent_snap();
        s.panes[1]
            .agent_opts
            .insert("@pane_status".into(), "running".into());
        a.set_rows(rows_of(&s));
        assert_eq!(sel(&a), "@1");
        // and from the copy's pane row: that pane in the window
        let mut a = agent_app();
        a.key(ctrl('g'));
        a.key(key(KeyCode::Down));
        assert_eq!(sel(&a), "!@1/%1");
        a.set_rows(rows_of(&s));
        assert_eq!(sel(&a), "@1/%1");
    }

    /// ⏎ on an agent window also focuses its lead pane (the one that
    /// needs you); on a plain window it leaves the panes alone; on a pane
    /// row, that pane.
    #[test]
    fn enter_focuses_the_lead_pane_or_the_selected_one() {
        let mut s = agent_snap();
        // win two gets a second, running agent pane, active
        let mut p = s.panes[1].clone();
        p.id = "%7".into();
        p.index = 1;
        p.agent_opts.insert("@pane_status".into(), "running".into());
        s.panes[1].active = false;
        s.panes.push(p);
        let mut a = app_of(&s);
        a.select_wid("@1");
        assert_eq!(a.selected().unwrap().pane.as_deref(), Some("%7"));
        assert_eq!(a.selected().unwrap().preview_pane().as_deref(), Some("%1"));
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Action::Switch {
                sid: "$0".into(),
                wid: "@1".into(),
                pane: Some("%1".into()),
            }
        );
        a.select_key("@1/%7");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Action::Switch {
                sid: "$0".into(),
                wid: "@1".into(),
                pane: Some("%7".into()),
            }
        );
        a.select_wid("@2");
        assert!(matches!(
            a.key(key(KeyCode::Enter)),
            Action::Switch { pane: None, .. }
        ));
    }

    #[test]
    fn background_token() {
        let mut s = agent_snap();
        s.panes[3]
            .agent_opts
            .insert("@pane_status".into(), "background".into());
        let mut a = app_of(&s);
        typed(&mut a, "@background");
        assert_eq!(wins(&a), "@3");
        a.clear_filter();
        typed(&mut a, "@running @background");
        assert_eq!(wins(&a), "@3");
    }

    /// Closing a window that's also pinned removes its copy too; the
    /// cursor goes to the next window row, not one of the window's own
    /// pane rows.
    #[test]
    fn close_lands_on_the_next_window() {
        let mut a = agent_app();
        a.select_wid("@3");
        assert_eq!(a.next_after_close("@3"), None, "the page's last");
        a.select_key("@2/%2");
        assert_eq!(a.next_after_close("@2").as_deref(), Some("@3"));
        // alpha: [!@1, @0, @1]
        a.key(ctrl('g'));
        assert_eq!(sel(&a), "!@1");
        assert_eq!(a.next_after_close("@1").as_deref(), Some("@0"));
        a.select_wid("@0");
        assert_eq!(a.next_after_close("@0").as_deref(), Some("@1"));
        a.select_wid("@1");
        assert_eq!(a.next_after_close("@1"), None);
    }

    /// The run-time clock only needs ticking while a visible row shows one.
    #[test]
    fn ticks_only_for_visible_runs() {
        let mut a = agent_app();
        assert!(!a.ticking(), "no run_started anywhere");
        let mut s = agent_snap();
        s.panes[3]
            .agent_opts
            .insert("@pane_started_at".into(), "100".into());
        a.set_rows(rows_of(&s));
        assert!(a.ticking());
        typed(&mut a, "logs");
        assert!(!a.ticking(), "the running row is filtered out");
        // a stale agent's left-over start time doesn't tick
        a.clear_filter();
        a.select_wid("@0");
        s.panes[0]
            .agent_opts
            .insert("@pane_started_at".into(), "100".into());
        a.set_rows(rows_of(&s));
        assert!(!a.ticking(), "on alpha's page, only the stale one");
    }

    #[test]
    fn swap_moves_over_windows_not_lines() {
        let mut a = app();
        let swap = Action::Swap {
            a: "@2".into(),
            b: "@3".into(),
        };
        assert_eq!(a.key(alt(KeyCode::Down)), swap);
        a.key(key(KeyCode::Down));
        assert_eq!(a.key(alt(KeyCode::Down)), swap, "from its pane row too");
        a.key(key(KeyCode::Down));
        assert_eq!(
            a.key(alt(KeyCode::Down)),
            Action::None,
            "the page's last window"
        );
        assert_eq!(
            a.key(alt(KeyCode::Up)),
            Action::Swap {
                a: "@3".into(),
                b: "@2".into()
            }
        );
    }

    fn chord_app() -> App {
        let mut a = app();
        a.chord = Chord::parse("C-a", ".");
        a
    }

    #[test]
    fn chord_parses_prefix_and_single_char_keys() {
        let c = Chord::parse("C-a", "w f").unwrap();
        assert_eq!(c.prefix, (KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(c.keys, vec!['w', 'f']);
        let c = Chord::parse("C-B", ". F5 C-h").unwrap();
        assert_eq!(c.prefix, (KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(c.keys, vec!['.'], "only single characters can be typed");
        assert_eq!(
            Chord::parse("M-x", "h").unwrap().prefix,
            (KeyCode::Char('x'), KeyModifiers::ALT)
        );
        assert_eq!(Chord::parse("None", "."), None);
        assert_eq!(Chord::parse("C-Space", "."), None);
        // no usable open key: the prefix still pages
        assert_eq!(Chord::parse("C-a", "").unwrap().keys, Vec::<char>::new());
        assert_eq!(Chord::parse("C-a", "F5").unwrap().keys, Vec::<char>::new());
    }

    #[test]
    fn prefix_parens_page_through_the_sessions() {
        let mut a = chord_app();
        let paren = |c| key(KeyCode::Char(c));
        assert_eq!(a.tab.as_deref(), Some("$1"));
        a.key(ctrl('a'));
        assert_eq!(a.key(paren(')')), Action::Redraw);
        assert_eq!(a.tab.as_deref(), Some("$0"), "next, wrapping");
        assert_eq!(sel(&a), "@0", "on the session's current window");
        assert!(a.filter.is_empty(), "nothing typed");
        a.key(ctrl('a'));
        a.key(paren(')'));
        assert_eq!(a.tab.as_deref(), Some("$1"));
        a.key(ctrl('a'));
        a.key(paren('('));
        assert_eq!(a.tab.as_deref(), Some("$0"), "previous, wrapping");
        // the filter is kept and applied to the new page
        typed(&mut a, "o");
        assert_eq!(wins(&a), "@0 @1");
        a.key(ctrl('a'));
        a.key(paren('('));
        assert_eq!(a.filter.as_string(), "o");
        assert_eq!(wins(&a), "@2", "beta's logs");
        // a bare paren is text, not paging
        typed(&mut a, ")");
        assert_eq!(a.tab.as_deref(), Some("$1"));
        assert_eq!(a.filter.as_string(), "o)");
        // in an editor the chord types the paren
        a.clear_filter();
        a.key(ctrl('r'));
        a.key(ctrl('a'));
        a.key(paren(')'));
        assert!(matches!(&a.mode, Mode::Rename { edit, .. } if edit.as_string() == ")logs"));
        assert_eq!(a.tab.as_deref(), Some("$1"));
        // without a chord there is no paging
        let mut a = app();
        a.key(ctrl('a'));
        a.key(paren(')'));
        assert_eq!(a.tab.as_deref(), Some("$1"));
        assert_eq!(a.filter.as_string(), ")");
    }

    #[test]
    fn the_open_chord_closes_the_list() {
        let mut a = chord_app();
        assert_eq!(a.key(ctrl('a')), Action::Redraw);
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Quit);
        // also with a filter typed: no '.' lands in it first
        let mut a = chord_app();
        typed(&mut a, "bu");
        a.key(ctrl('a'));
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Quit);
        assert_eq!(a.filter.as_string(), "bu");
    }

    #[test]
    fn the_prefix_then_another_key_acts_as_before() {
        let mut a = chord_app();
        typed(&mut a, "ld");
        // ^a still moves the filter's cursor to the start
        a.key(ctrl('a'));
        assert_eq!(a.filter.cur, 0);
        // a non-home key after it is handled as usual: typed at the start
        assert_eq!(a.key(key(KeyCode::Char('b'))), Action::Redraw);
        typed(&mut a, "u");
        assert_eq!(a.filter.as_string(), "buld");
        // a home key not straight after the prefix is just text
        typed(&mut a, ".");
        assert_eq!(a.filter.as_string(), "bu.ld");
        // ^a then a non-home key, then the home key: typed
        a.key(ctrl('a'));
        a.key(key(KeyCode::Right));
        typed(&mut a, ".");
        assert_eq!(a.filter.as_string(), "b.u.ld");
        // ^a ^a .: the second ^a re-arms
        a.key(ctrl('a'));
        a.key(ctrl('a'));
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Quit);
    }

    #[test]
    fn without_a_chord_the_home_key_is_typed() {
        let mut a = app();
        a.key(ctrl('a'));
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Redraw);
        assert_eq!(a.filter.as_string(), ".");
    }

    #[test]
    fn the_open_chord_closes_from_every_mode() {
        use crate::ops::ClosePlan;
        // rename: the unsaved name is dropped
        let mut a = chord_app();
        a.key(ctrl('r'));
        typed(&mut a, "x");
        a.key(ctrl('a'));
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Quit);
        // new window
        let mut a = chord_app();
        a.key(alt(KeyCode::Char('n')));
        a.key(ctrl('a'));
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Quit);
        // close confirm: ^a cancels it (as any key does), then '.' closes
        // the popup; the window is not closed
        let mut a = chord_app();
        a.close_plan("@2", ClosePlan::Ask("busy".into()));
        assert!(matches!(a.mode, Mode::Confirm { .. }));
        assert_eq!(a.key(ctrl('a')), Action::Redraw);
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Quit);
        // help
        let mut a = chord_app();
        a.key(key(KeyCode::F(1)));
        a.key(ctrl('a'));
        assert_eq!(a.key(key(KeyCode::Char('.'))), Action::Quit);
    }
}
