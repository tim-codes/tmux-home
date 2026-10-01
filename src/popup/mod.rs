//! `tmux-home popup`: the full-window window manager, run inside
//! `display-popup -E -B -w 100% -h 100%`.

pub mod agents;
pub mod app;
pub mod filter;
pub mod fresh;
pub mod git;

use crate::{
    client::{self, Answer},
    ipc::Reply,
    ops::{CloseOutcome, Tx},
    store::Store,
    tmux::snapshot::{Snapshot, read_snapshot},
};
use app::{Action, App, Mode, Row};
use fresh::{Guard, Stamp};
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyEventKind},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

/// Snapshots from the daemon (live) or direct reads (degraded).
struct Feed {
    snap: Snapshot,
    stamp: Stamp,
}

const DEGRADED_EVERY: Duration = Duration::from_secs(1);
const PREVIEW_EVERY: Duration = Duration::from_millis(1000);
/// Budget for the daemon's first reply to a subscription (a fresh read).
const SUBSCRIBE_BUDGET: Duration = Duration::from_millis(400);
/// Budget for a `refresh` after a write; past it the popup reads tmux itself.
const REFRESH_BUDGET: Duration = Duration::from_millis(300);
/// While degraded, a daemon is (re)started at most this often (at once
/// after a `Restart`).
const RESPAWN_EVERY: Duration = Duration::from_secs(5);

/// The feed thread: plain blocking I/O, no async runtime. It subscribes to
/// the daemon and forwards its pushes; when there is no daemon it uses the
/// shared degraded path (`client::revive`, then a direct read) every second.
fn spawn_feed(socket: PathBuf) -> mpsc::Receiver<Feed> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || feed_loop(socket, tx));
    rx
}

fn feed_loop(socket: PathBuf, tx: mpsc::Sender<Feed>) {
    let tmux = crate::tmux::Tmux::new(socket.clone());
    let mut last_spawn: Option<Instant> = None;
    loop {
        // live: subscribe until the daemon goes away
        let since = Instant::now();
        let restart = match client::ask(&socket, &client::subscribe_req("popup"), SUBSCRIBE_BUDGET)
        {
            Answer::Snapshot {
                epoch,
                seq,
                data,
                mut conn,
            } => {
                let mut next = Some((seq, data));
                while let Some((seq, snap)) = next.take() {
                    let stamp = Stamp::Live { epoch, seq, since };
                    if tx.send(Feed { snap, stamp }).is_err() {
                        return;
                    }
                    if let Ok(Some(Reply::Snapshot { seq, data, .. })) = conn.recv() {
                        next = Some((seq, data));
                    }
                }
                false
            }
            Answer::Restart => true,
            Answer::Down => false,
        };
        // degraded: (re)start a daemon now and then, read directly meanwhile
        if restart || last_spawn.is_none_or(|t| t.elapsed() > RESPAWN_EVERY) {
            last_spawn = Some(Instant::now());
            client::revive(&socket, restart);
        }
        let at = Instant::now();
        let Ok(snap) = read_snapshot(&tmux) else {
            return; // server gone
        };
        let stamp = Stamp::Direct { at };
        if tx.send(Feed { snap, stamp }).is_err() {
            return;
        }
        std::thread::sleep(DEGRADED_EVERY);
    }
}

struct Runtime {
    tx: Tx,
    client: Option<String>,
    store: Store,
    home: String,
    live: bool,
    preview: (Option<String>, String),
    preview_at: Instant,
    /// Set by `refresh` after our own write: older snapshots are held back.
    guard: Guard<Snapshot>,
}

impl Runtime {
    fn rows(&self, s: &Snapshot) -> Vec<Row> {
        app::build_rows(s, self.client.as_deref(), &self.home)
    }

    /// A read of tmux made now: through the daemon's `refresh` when live
    /// (so its subscribers see it too), else directly.
    fn read_now(&self) -> Option<(Snapshot, Stamp)> {
        if self.live {
            let since = Instant::now();
            if let Answer::Snapshot {
                epoch, seq, data, ..
            } = client::ask(&self.tx.tmux.socket, &client::refresh_req(), REFRESH_BUDGET)
            {
                return Some((data, Stamp::Live { epoch, seq, since }));
            }
        }
        let at = Instant::now();
        let snap = read_snapshot(&self.tx.tmux).ok()?;
        Some((snap, Stamp::Direct { at }))
    }

    /// Re-read tmux right after a write so the list reflects it at once;
    /// from then on, snapshots older than this read are ignored.
    fn refresh(&mut self, app: &mut App) {
        if let Some((s, stamp)) = self.read_now() {
            self.guard.set_floor(stamp);
            let rows = self.rows(&s);
            app.set_rows(rows);
        }
    }

    fn update_preview(&mut self, app: &App, force: bool) {
        let pane = app.selected().and_then(|r| r.preview_pane());
        if !force && pane == self.preview.0 && self.preview_at.elapsed() < PREVIEW_EVERY {
            return;
        }
        let text = pane
            .as_deref()
            .map(|p| self.tx.capture(p))
            .unwrap_or_else(|| "(no pane)".into());
        self.preview = (pane, text);
        self.preview_at = Instant::now();
    }

    /// Executes an action; returns false when the popup should exit.
    fn act(&mut self, app: &mut App, a: Action) -> bool {
        match a {
            Action::None | Action::Redraw => {}
            Action::Quit => return false,
            Action::Switch { sid, wid, pane } => {
                ratatui::restore();
                let _ = self.tx.switch_to(self.client.as_deref(), &sid, &wid);
                if let Some(p) = pane {
                    let _ = self.tx.select_pane(&p);
                }
                return false;
            }
            Action::Rename { wid, name } => {
                if let Err(e) = self.tx.rename(&wid, &name) {
                    app.notice = Some(format!(" rename failed: {e}"));
                }
                self.refresh(app);
            }
            Action::ResetName(wid) => {
                let _ = self.tx.reset_name(&wid);
                // automatic-rename applies on tmux's next status tick
                std::thread::sleep(Duration::from_millis(50));
                self.refresh(app);
            }
            Action::CloseStart(wid) => match self.tx.close_plan(&wid) {
                Ok(plan) => {
                    let next = app.close_plan(&wid, plan);
                    return self.act(app, next);
                }
                Err(_) => self.refresh(app),
            },
            Action::Close(wid) => {
                // the cursor moves to the next window; a pinned copy of the
                // closed window goes too, so that isn't simply the same
                // row position
                let pos = app.sel;
                let next = app.next_after_close(&wid);
                match self
                    .tx
                    .close_window(&wid, self.client.as_deref(), &self.store)
                {
                    Ok(CloseOutcome::Refused) => {
                        app.notice = Some(" can't close the last window on the server".into())
                    }
                    Ok(CloseOutcome::Closed) => {}
                    Ok(CloseOutcome::NotSaved(e)) => {
                        app.notice = Some(format!(" closed, but ^t can't reopen it: {e}"))
                    }
                    Err(e) => app.notice = Some(format!(" close failed: {e}")),
                }
                app.sel_wid = None;
                app.sel = pos;
                self.refresh(app);
                if let Some(k) = next {
                    app.select_wid(&k);
                }
            }
            Action::Reopen => match self.tx.reopen(&self.store) {
                Ok(Some(wid)) => {
                    app.notice = None;
                    app.clear_filter();
                    self.refresh(app);
                    app.want(&wid);
                }
                Ok(None) => app.notice = Some(" nothing to reopen".into()),
                Err(e) => app.notice = Some(format!(" reopen failed: {e}")),
            },
            Action::Swap { a, b } => {
                if let Err(e) = self.tx.swap(&a, &b) {
                    app.notice = Some(format!(" move failed: {e}"));
                }
                self.refresh(app);
                app.select_wid(&a);
            }
            Action::NewWindow { after, cwd, name } => {
                match self.tx.new_window_after(&after, &cwd, &name) {
                    Ok(wid) => {
                        self.refresh(app);
                        if !app.visible.iter().any(|&i| app.rows[i].wid == wid) {
                            app.clear_filter();
                        }
                        app.want(&wid);
                    }
                    Err(e) => app.notice = Some(format!(" new window failed: {e}")),
                }
            }
        }
        // the closed-window store reports a reset stack here, not on stderr
        if let Some(n) = self.store.take_notice() {
            app.notice = Some(format!(" {n}"));
        }
        true
    }
}

pub fn run(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let socket = client::current_socket(socket)
        .ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let tx = Tx::new(socket.clone());
    let client = tx.home_client();
    let store = Store::for_socket(&socket)?;
    let feed = spawn_feed(socket.clone());
    let mut rt = Runtime {
        tx,
        client,
        store,
        home: std::env::var("HOME").unwrap_or_default(),
        live: false,
        preview: (None, String::new()),
        preview_at: Instant::now(),
        guard: Guard::default(),
    };
    let mut app = App::new();
    app.home = rt.home.clone();
    // first picture: read tmux directly, so the cursor lands on the window
    // the client shows now; no floor, since nothing was written (the feed's
    // first snapshot is a fresh read too)
    if let Ok(s) = read_snapshot(&rt.tx.tmux) {
        app.set_rows(rt.rows(&s));
    }
    app.select_current();

    let mut term = ratatui::init();
    let result = event_loop(&mut term, &mut app, &mut rt, &feed);
    ratatui::restore();
    result
}

fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rt: &mut Runtime,
    feed: &mpsc::Receiver<Feed>,
) -> anyhow::Result<()> {
    let mut dirty = true;
    let mut drawn_at = 0;
    loop {
        // latest snapshot wins
        let mut latest = None;
        while let Ok(f) = feed.try_recv() {
            latest = Some(f);
        }
        if let Some(f) = &latest {
            let live = matches!(f.stamp, Stamp::Live { .. });
            dirty |= rt.live != live;
            rt.live = live;
        }
        let now = Instant::now();
        let apply = match latest {
            Some(f) => rt.guard.offer(f.snap, &f.stamp, now),
            None => None,
        }
        .or_else(|| rt.guard.tick(now));
        if let Some(snap) = apply {
            let rows = rt.rows(&snap);
            app.set_rows(rows);
            dirty = true;
        }
        let before = rt.preview_at;
        rt.update_preview(app, false);
        if rt.preview_at != before {
            dirty = true;
        }
        // agent rows show run time: redraw as the clock moves
        let now = crate::agent::now();
        if now != drawn_at && app.ticking() {
            dirty = true;
        }
        if dirty {
            term.draw(|f| draw(f, app, rt.live, &rt.preview.1, now))?;
            drawn_at = now;
            dirty = false;
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    let a = app.key(k);
                    if !rt.act(app, a) {
                        return Ok(());
                    }
                    rt.update_preview(app, false);
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

fn preview_rect(area: Rect, flip: bool) -> (Rect, Option<Rect>, Direction) {
    let (dir, pct, shown) = if area.width >= 160 {
        (Direction::Horizontal, 50, true)
    } else if area.height >= 30 {
        (Direction::Vertical, 33, true)
    } else {
        (Direction::Vertical, 50, false)
    };
    if shown == flip {
        return (area, None, dir);
    }
    let parts = Layout::default()
        .direction(dir)
        .constraints([
            Constraint::Percentage(100 - pct),
            Constraint::Percentage(pct),
        ])
        .split(area);
    (parts[0], Some(parts[1]), dir)
}

/// Draws the popup; `live` is false in degraded mode, `preview` is the
/// selected pane's captured text, `now` Unix seconds (run times).
fn draw(f: &mut Frame, app: &mut App, live: bool, preview: &str, now: u64) {
    let area = f.area();
    let bg = Block::default().style(Style::default().bg(Color::Reset));
    f.render_widget(bg, area);
    if app.mode == Mode::Help {
        f.render_widget(Paragraph::new(app::help()), area);
        return;
    }
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    // header
    let mut h = vec![Span::styled(" tmux-home", bold)];
    if !app.location.is_empty() {
        h.push(Span::raw(format!("   {}", app.location)));
    }
    if app.tally.total() > 0 {
        h.push(Span::raw(format!("  ·  agents: {}", app.tally.label())));
    }
    if !live {
        h.push(Span::styled("   (direct)", dim));
    }
    f.render_widget(Paragraph::new(Line::from(h)), v[0]);
    f.render_widget(
        Paragraph::new(Span::styled("F1 help ", dim)).alignment(ratatui::layout::Alignment::Right),
        v[0],
    );

    // prompt line
    let (prompt, edit) = match &app.mode {
        Mode::List | Mode::Help => ("> ".to_string(), Some(&app.filter)),
        Mode::Rename { edit, .. } => ("rename › ".to_string(), Some(edit)),
        Mode::NewWindow { edit, .. } => ("new window › ".to_string(), Some(edit)),
        Mode::Confirm { prompt, .. } => (prompt.clone(), None),
    };
    let text = edit.map(|e| e.as_string()).unwrap_or_default();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt.clone(), Style::default().fg(Color::Cyan)),
            Span::raw(text),
        ])),
        v[1],
    );
    if matches!(app.mode, Mode::List) {
        f.render_widget(
            Paragraph::new(Span::styled(
                // windows, not rows: a pinned copy isn't counted twice
                format!(
                    "{}/{} ",
                    app.visible.iter().filter(|&&i| !app.rows[i].pinned).count(),
                    app.rows.iter().filter(|r| !r.pinned).count()
                ),
                dim,
            ))
            .alignment(ratatui::layout::Alignment::Right),
            v[1],
        );
    }
    let cx = prompt.chars().count() + edit.map_or(0, |e| e.cur);
    f.set_cursor_position((v[1].x + cx as u16, v[1].y));

    // list + preview
    let (list, prev, dir) = preview_rect(v[2], app.preview_flip);
    draw_list(f, app, list, now);
    if let Some(p) = prev {
        let block = Block::default()
            .borders(if dir == Direction::Horizontal {
                Borders::LEFT
            } else {
                Borders::TOP
            })
            .border_style(dim);
        let mut inner = block.inner(p);
        f.render_widget(block, p);
        // an agent window: the agent card, then the git card (a window in
        // a repo), above the pane's last lines
        let mut cards = Vec::new();
        if let Some(a) = app.selected().and_then(|r| r.agents.as_ref()) {
            cards.extend(agents::card(a, now, inner.width as usize));
        }
        if let Some((root, g)) = app.selected().and_then(|r| r.git.as_ref()) {
            cards.extend(git::card(g, root, inner.width as usize, &app.home));
        }
        if !cards.is_empty() {
            let h = (cards.len() as u16).min(inner.height);
            f.render_widget(Paragraph::new(cards), Rect { height: h, ..inner });
            inner.y += h;
            inner.height -= h;
        }
        let lines: Vec<&str> = preview.lines().collect();
        let skip = lines.len().saturating_sub(inner.height as usize);
        let body: Vec<Line> = lines[skip..].iter().map(|l| Line::raw(*l)).collect();
        f.render_widget(Paragraph::new(body), inner);
    }

    f.render_widget(Paragraph::new(Span::styled(app.footer(), dim)), v[3]);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect, now: u64) {
    let h = area.height as usize;
    app.page = h.saturating_sub(1).max(1);
    let sw = app
        .rows
        .iter()
        .map(|r| r.session.chars().count())
        .max()
        .unwrap_or(0)
        .min(16);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let width = area.width as usize;
    // The list as lines: a NEEDS YOU group (when any window needs you)
    // gets a header, and so do the sessions below it.
    enum Item {
        Header(&'static str),
        Row(usize),
    }
    let mut items = Vec::new();
    let mut in_pinned = false;
    for (pos, &i) in app.visible.iter().enumerate() {
        let pinned = app.rows[i].pinned;
        if pinned && !in_pinned {
            items.push(Item::Header(" NEEDS YOU "));
        } else if !pinned && in_pinned {
            items.push(Item::Header(" sessions "));
        }
        in_pinned = pinned;
        items.push(Item::Row(pos));
    }
    let sel_line = items
        .iter()
        .position(|it| matches!(it, Item::Row(p) if *p == app.sel))
        .unwrap_or(0);
    let top = sel_line.saturating_sub(h.saturating_sub(1));
    let mut lines = Vec::new();
    // the session name is emphasised on the first row of each group
    // (counting rows above the viewport), dimmed on the rest
    let mut prev_sid: Option<&str> = None;
    for (n, it) in items.iter().enumerate() {
        let pos = match it {
            Item::Header(t) => {
                prev_sid = None;
                if n >= top && lines.len() < h {
                    let rule = "─".repeat(width.saturating_sub(t.chars().count() + 2));
                    lines.push(Line::styled(format!(" ─{t}{rule}"), dim));
                }
                continue;
            }
            Item::Row(p) => *p,
        };
        let r = &app.rows[app.visible[pos]];
        let first = r.pinned || prev_sid != Some(r.sid.as_str());
        prev_sid = Some(&r.sid);
        if n < top || lines.len() >= h {
            continue;
        }
        let selected = pos == app.sel;
        let sname: String = r.session.chars().take(sw).collect();
        let sstyle = if first {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        // the editor/confirm is on the selected row, pinned copy or not
        let editing = selected
            && match &app.mode {
                Mode::Rename { wid, .. } | Mode::Confirm { wid, .. } => wid == &r.wid,
                _ => false,
            };
        let mut spans = vec![
            Span::styled(
                if selected { "▌" } else { " " },
                Style::default().fg(Color::Magenta),
            ),
            Span::styled(format!("{sname:<sw$}"), sstyle),
            Span::raw(" "),
            Span::styled(
                if r.current { "▶" } else { " " },
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(format!(" {:>3}  ", r.index)),
            Span::styled(
                format!("{:<24}", r.name),
                if editing {
                    Style::default().add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default()
                },
            ),
        ];
        let badge = r.git.as_ref().map(|(_, g)| git::badge_spans(g));
        let used: usize = spans.iter().map(Span::width).sum();
        let room = width.saturating_sub(used);
        match &r.agents {
            Some(a) => spans.extend(agents::row_cell(a, now, badge, room)),
            None => {
                // the path gives way to the badge first, then the badge
                // drops whole pieces
                let cmd = format!("  {:<10}  ", r.cmd);
                let badge = badge.unwrap_or_default();
                let bw: usize = badge.iter().map(Span::width).sum();
                let room = room.saturating_sub(Span::raw(cmd.as_str()).width());
                let path = git::clip_to(&r.path, room.saturating_sub(bw));
                let pw = Span::raw(path.as_str()).width();
                spans.push(Span::styled(format!("{cmd}{path}"), dim));
                spans.extend(git::fit_badge(badge, room.saturating_sub(pw)));
            }
        }
        if selected {
            for s in spans.iter_mut().skip(1) {
                s.style = s.style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            }
        }
        lines.push(Line::from(spans));
    }
    if app.visible.is_empty() {
        lines.push(Line::styled("  (no matches)", dim));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// `tmux-home reopen`: prints the new window ID; exit 4 on an empty stack.
pub fn reopen_cli(socket: Option<PathBuf>) -> anyhow::Result<i32> {
    let socket = client::current_socket(socket)
        .ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let store = Store::for_socket(&socket)?;
    let reopened = Tx::new(socket).reopen(&store);
    if let Some(n) = store.take_notice() {
        eprintln!("tmux-home: {n}");
    }
    match reopened? {
        Some(w) => {
            println!("{w}");
            Ok(0)
        }
        None => Ok(4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::snapshot::{Client, Pane, Session, Window};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    };

    fn app() -> App {
        let mut s = Snapshot::default();
        s.sessions.push(Session {
            id: "$0".into(),
            name: "alpha".into(),
            attached: 1,
        });
        for (i, name) in ["editor", "a much longer window name than fits"]
            .into_iter()
            .enumerate()
        {
            s.windows.push(Window {
                id: format!("@{i}"),
                session_id: "$0".into(),
                index: i as u32,
                name: name.into(),
                automatic_rename: false,
                active: i == 0,
            });
            s.panes.push(Pane {
                id: format!("%{i}"),
                window_id: format!("@{i}"),
                session_id: "$0".into(),
                index: 0,
                active: true,
                current_command: "sh".into(),
                current_path: "/tmp".into(),
                title: String::new(),
                role: String::new(),
                agent_opts: Default::default(),
                tty: String::new(),
            });
        }
        s.clients.push(Client {
            name: "c".into(),
            tty: "c".into(),
            session_id: "$0".into(),
        });
        let mut a = App::new();
        a.set_rows(app::build_rows(&s, Some("c"), ""));
        a.select_current();
        a
    }

    fn screen(a: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, a, true, "line one\nline two", NOW))
            .unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const NOW: u64 = 1_790_000_000;

    /// Session "dev": an agent waiting on a permission prompt (`@0`), two
    /// running agents in one window (`@1`), a stale one (`@2`) and a plain
    /// shell (`@3`); the client is on `@3`.
    fn agent_app() -> App {
        app_of(&agent_snap())
    }

    fn app_of(s: &Snapshot) -> App {
        let mut a = App::new();
        a.home = "/home/u".into();
        a.set_rows(app::build_rows(s, Some("c"), "/home/u"));
        a.select_current();
        a
    }

    fn agent_snap() -> Snapshot {
        use crate::agent::tests::pane;
        let mut s = Snapshot::default();
        s.sessions.push(Session {
            id: "$0".into(),
            name: "dev".into(),
            attached: 1,
        });
        let started = (NOW - 12 * 60).to_string();
        let waiting = [
            ("@pane_agent", "claude"),
            ("@pane_status", "waiting"),
            ("@pane_attention", "notification"),
            ("@pane_wait_reason", "permission_prompt"),
            ("@pane_started_at", started.as_str()),
            (
                "@pane_prompt",
                "please refactor the api layer so that every handler returns typed errors and nothing panics",
            ),
            ("@pane_prompt_source", "user"),
            ("@pane_subagents", "Explore:a,Plan:b"),
            ("@pane_bg_cmd", "npm run dev"),
            ("@pane_permission_mode", "plan"),
            ("@pane_worktree_name", "fix-x"),
            ("@pane_worktree_branch", "fix/x"),
        ];
        let running = [
            ("@pane_agent", "claude"),
            ("@pane_status", "running"),
            ("@pane_started_at", started.as_str()),
        ];
        let stale = [("@pane_agent", "claude"), ("@pane_status", "waiting")];
        let panes = [
            ("@0", pane("%0", "2.1.283", &waiting)),
            ("@1", pane("%1", "2.1.283", &running)),
            ("@1", pane("%11", "2.1.283", &running)),
            ("@2", pane("%2", "fish", &stale)),
            ("@3", pane("%3", "fish", &[])),
        ];
        for (i, name) in ["fix", "api", "old", "shell"].into_iter().enumerate() {
            s.windows.push(Window {
                id: format!("@{i}"),
                session_id: "$0".into(),
                index: i as u32,
                name: name.into(),
                automatic_rename: false,
                active: i == 3,
            });
        }
        for (wid, mut p) in panes {
            p.window_id = wid.into();
            p.index = if p.id == "%11" { 1 } else { 0 };
            p.active = p.index == 0;
            s.panes.push(p);
        }
        s.clients.push(Client {
            name: "c".into(),
            tty: "c".into(),
            session_id: "$0".into(),
        });
        s
    }

    /// The agent fixture with git: the waiting agent (`@0`) works in a
    /// linked worktree, the plain shell (`@3`) sits in a dirty repo's
    /// subdirectory, and the stale window's repo (`@2`) timed out.
    fn git_snap() -> Snapshot {
        use crate::git::badge::{Phase, RepoStatus};
        use crate::git::repo::Operation;
        let mut s = agent_snap();
        for (pid, path) in [
            ("%0", "/home/u/dev/app.fix-x"),
            ("%2", "/home/u/dev/old"),
            ("%3", "/home/u/dev/app/src"),
        ] {
            s.panes
                .iter_mut()
                .find(|p| p.id == pid)
                .unwrap()
                .current_path = path.into();
        }
        let g = &mut s.git;
        g.paths.insert(
            "/home/u/dev/app.fix-x".into(),
            "/home/u/dev/app.fix-x".into(),
        );
        g.paths
            .insert("/home/u/dev/app/src".into(), "/home/u/dev/app".into());
        g.paths
            .insert("/home/u/dev/old".into(), "/home/u/dev/old".into());
        g.repos.insert(
            "/home/u/dev/app".into(),
            RepoStatus {
                branch: "main".into(),
                phase: Phase::Refs,
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
                staged: 1,
                modified: 3,
                untracked: 1,
                stashes: 1,
                stray: 2,
                stray_names: vec!["feat/a".into(), "spike".into()],
                operation: Some(Operation::Rebase),
                default_branch: Some("main".into()),
                has_remote: true,
                worktrees: 1,
                ..RepoStatus::default()
            },
        );
        g.repos.insert(
            "/home/u/dev/app.fix-x".into(),
            RepoStatus {
                branch: "fix/x".into(),
                linked: true,
                main_root: Some("/home/u/dev/app".into()),
                phase: Phase::Refs,
                upstream: Some("origin/fix/x".into()),
                modified: 1,
                has_remote: true,
                ..RepoStatus::default()
            },
        );
        g.repos.insert(
            "/home/u/dev/old".into(),
            RepoStatus {
                branch: "legacy".into(),
                phase: Phase::Status,
                stale: true,
                ..RepoStatus::default()
            },
        );
        s
    }

    #[test]
    fn git_badges_on_rows_and_the_card() {
        let mut a = app_of(&git_snap());
        let s = screen(&mut a, 200, 50);
        let line = |s: &str, pat: &str| {
            s.lines()
                .find(|l| l.contains(pat))
                .unwrap_or_else(|| panic!("no {pat:?} in\n{s}"))
                .to_string()
        };
        // a plain row: command, path, then the badge
        assert!(
            line(&s, " shell ").contains("fish        ~/d/a/src  main +!? ⇡2 ⇣1 $1 ⚠2 ↻ "),
            "{s}"
        );
        // an agent row shows the lead agent's worktree badge, in place of
        // the hook's `(wt) <branch>`, pinned copy and session row alike
        assert_eq!(s.matches("claude  fix/x (wt) ! |  plan").count(), 2, "{s}");
        assert!(!s.contains("(wt) fix/x"), "{s}");
        // a stale status ends in ~
        assert!(line(&s, " old ").contains("claude  legacy ~ "), "{s}");
        // the card of the selected (current) window
        for want in [
            "git        main → origin/main  ⇡2 ahead ⇣1 behind",
            "changes    1 staged · 3 modified · 1 untracked",
            "state      ↻ rebase in progress",
            "stash      $1",
            "stray      ⚠2 feat/a, spike",
            "worktree   ~/d/app (+1 linked)",
            "default    main",
        ] {
            assert!(s.contains(want), "no {want:?} in\n{s}");
        }
        // the agent window: agent card, then its worktree's git card
        a.select_wid("@0");
        let s = screen(&mut a, 200, 50);
        let agent = s.find("◐ waiting  ·  claude").expect(&s);
        let git = s
            .find("git        fix/x → origin/fix/x  in sync")
            .expect(&s);
        assert!(agent < git, "{s}");
        assert!(s.contains("~/d/app.fix-x (linked; main ~/d/app)"), "{s}");
        // a stale repo's card says so
        a.select_wid("@2");
        let s = screen(&mut a, 200, 50);
        assert!(s.contains("~ stale: the last check timed out"), "{s}");
        // filtering matches the branch
        let mut a = app_of(&git_snap());
        for c in "legacy".chars() {
            a.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(a.visible.len(), 1);
        assert_eq!(a.rows[a.visible[0]].name, "old");
    }

    /// Narrow terminals: the path gives way first, then whole badge pieces
    /// from the right — never part of one (`⚠12` must not become `⚠1`).
    #[test]
    fn badges_are_clipped_by_whole_pieces() {
        let mut snap = git_snap();
        snap.git.repos.get_mut("/home/u/dev/app").unwrap().stray = 12;
        let shell_pieces = ["main", "+!?", "⇡2", "⇣1", "$1", "⚠12", "↻"];
        let agent_pieces = ["fix/x", "(wt)", "!", "|"];
        for w in [80u16, 76, 72, 68, 64, 60, 56, 52, 48, 44] {
            let mut a = app_of(&snap);
            let s = screen(&mut a, w, 50);
            let row = |pat: &str| {
                s.lines()
                    .find(|l| l.contains(pat))
                    .unwrap_or_else(|| panic!("{w}: no {pat:?} in\n{s}"))
                    .to_string()
            };
            let shell = row(" shell ");
            // after the command: the (clipped, maybe gone) path, the badge
            let tail: Vec<&str> = shell
                .split_whitespace()
                .skip_while(|t| *t != "fish")
                .skip(1)
                .skip_while(|t| t.starts_with('~') || t.starts_with('…'))
                .collect();
            assert!(
                tail.iter().all(|t| shell_pieces.contains(t)),
                "{w}: partial piece in {shell:?}"
            );
            assert_eq!(
                tail[..],
                shell_pieces[..tail.len()],
                "{w}: pieces dropped from the right: {shell:?}"
            );
            assert!(shell.chars().count() <= w as usize);
            if w >= 80 {
                assert!(shell.ends_with("main +!? ⇡2 ⇣1 $1 ⚠12 ↻"), "{w}: {shell:?}");
                assert!(shell.contains('…'), "{w}: the path was clipped: {shell:?}");
            }
            // agent rows: the badge after the kind, whole pieces only
            for l in s
                .lines()
                .filter(|l| l.contains(" fix ") && l.contains("claude"))
            {
                let tail: Vec<&str> = l
                    .split_whitespace()
                    .skip_while(|t| *t != "claude")
                    .skip(1)
                    // up to the mode / reason, which the row may cut
                    .take_while(|t| !"plan".starts_with(t) && !"permission".starts_with(t))
                    .collect();
                assert_eq!(
                    tail[..],
                    agent_pieces[..tail.len()],
                    "{w}: agent badge {l:?}"
                );
            }
        }
    }

    #[test]
    fn help_has_the_badge_legend() {
        let mut a = app();
        a.key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        let s = screen(&mut a, 100, 50);
        for want in [
            "^t              reopen",
            "git badge",
            "⚠n stray branches",
            "~ stale",
            "press any key",
        ] {
            assert!(s.contains(want), "no {want:?} in\n{s}");
        }
    }

    #[test]
    fn agent_rows_tally_and_needs_you() {
        let mut a = agent_app();
        for (w, h) in [(200, 50), (100, 30)] {
            let s = screen(&mut a, w, h);
            let line = |pat: &str| {
                s.lines()
                    .find(|l| l.contains(pat))
                    .unwrap_or_else(|| panic!("no {pat:?} in\n{s}"))
                    .to_string()
            };
            // stale agents aren't counted
            assert!(
                line("tmux-home").contains("agents: 1 waiting · 2 running"),
                "{s}"
            );
            let lines: Vec<&str> = s.lines().collect();
            let needs = lines
                .iter()
                .position(|l| l.contains("─ NEEDS YOU ─"))
                .unwrap();
            assert!(lines[needs + 1].contains("fix"), "{s}");
            assert!(lines[needs + 1].contains("◐ waiting"), "{s}");
            assert!(lines[needs + 1].contains("permission"), "{s}");
            assert!(lines[needs + 2].contains("─ sessions ─"), "{s}");
            assert!(line("  api").contains("● running"), "{s}");
            assert!(line("  api").contains("12m  claude ×2"), "{s}");
            let old = line("  old");
            assert!(old.contains("◐ waiting (ended?)"), "{s}");
            assert!(
                line("  shell").contains("fish"),
                "a plain window keeps its command"
            );
            // the fix window is listed twice: pinned, and in its session
            assert_eq!(s.matches(" fix ").count(), 2, "{s}");
            assert!(line("> ").ends_with("4/4"), "windows, not rows: {s}");
        }
    }

    #[test]
    fn agent_card_in_the_preview() {
        let mut a = agent_app();
        a.select_wid("@0");
        for (w, h) in [(200, 50), (120, 40)] {
            let s = screen(&mut a, w, h);
            for want in [
                "◐ waiting  ·  claude  ·  run 12m",
                "needs you  permission",
                "subagents  +2 (Explore, Plan)",
                "background npm run dev",
                "worktree   fix-x (fix/x)",
                "mode       plan",
                "prompt     please refactor",
                "line two",
            ] {
                assert!(s.contains(want), "{w}x{h}: no {want:?} in\n{s}");
            }
        }
        // a stale window's card says so
        a.select_wid("@2");
        let s = screen(&mut a, 200, 50);
        assert!(s.contains("◐ waiting (ended?)  ·  claude"), "{s}");
        assert!(s.contains("left-over options"), "{s}");
        // a plain window: no card
        a.select_wid("@3");
        let s = screen(&mut a, 200, 50);
        assert!(!s.contains("·  claude"), "{s}");
    }

    /// ^r (or ^x's confirm) from a pinned copy marks that copy, the row
    /// the cursor is on; from the session row, that row.
    #[test]
    fn the_editor_shows_on_the_selected_copy() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let underlined = |a: &mut App| -> Vec<u16> {
            let mut t = Terminal::new(TestBackend::new(120, 20)).unwrap();
            t.draw(|f| draw(f, a, true, "", NOW)).unwrap();
            let buf = t.backend().buffer().clone();
            (0..20)
                .filter(|&y| (0..120).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED)))
                .collect()
        };
        let mut a = agent_app();
        a.key(ctrl('g'));
        assert_eq!(a.selected().unwrap().key, "!@0");
        a.key(ctrl('r'));
        assert!(matches!(&a.mode, Mode::Rename { wid, .. } if wid == "@0"));
        // header, prompt line, NEEDS YOU rule, then the pinned row (y=3)
        assert_eq!(underlined(&mut a), [3]);
        a.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        a.select_wid("@0");
        a.key(ctrl('r'));
        // ... the sessions rule (y=4), then the session row of fix (y=5)
        assert_eq!(underlined(&mut a), [5]);
    }

    #[test]
    fn no_agents_no_tally_no_group() {
        let s = screen(&mut app(), 200, 50);
        assert!(!s.contains("agents:") && !s.contains("NEEDS YOU"), "{s}");
    }

    #[test]
    fn preview_layout_by_size() {
        let at = |w, h, flip| {
            let (_, p, d) = preview_rect(Rect::new(0, 0, w, h), flip);
            (p.map(|r| (r.width, r.height)), d)
        };
        // >= 160 columns: right half
        assert_eq!(at(200, 50, false), (Some((100, 50)), Direction::Horizontal));
        // < 160 columns, >= 30 rows: bottom third
        assert_eq!(at(150, 40, false).1, Direction::Vertical);
        assert_eq!(at(150, 40, false).0.unwrap().0, 150);
        assert!(at(150, 40, false).0.unwrap().1 <= 14);
        // small: hidden until ^o, then the bottom half
        assert_eq!(at(120, 20, false).0, None);
        assert_eq!(at(120, 20, true), (Some((120, 10)), Direction::Vertical));
        // ^o hides a shown preview
        assert_eq!(at(200, 50, true).0, None);
    }

    #[test]
    fn side_and_bottom_borders() {
        let s = screen(&mut app(), 200, 50);
        assert!(s.contains('│') && s.contains("line two"), "{s}");
        let s = screen(&mut app(), 150, 40);
        assert!(!s.contains('│'), "{s}");
        assert!(
            s.lines()
                .any(|l| l.chars().count() == 150 && l.chars().all(|c| c == '─'))
        );
        let s = screen(&mut app(), 120, 20);
        assert!(!s.contains("line two"), "hidden when small:\n{s}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        let plain = |c| KeyEvent::new(c, KeyModifiers::NONE);
        for (w, h) in [(20, 3), (10, 1), (1, 1), (3, 2), (200, 1)] {
            for keys in [
                vec![],
                vec![ctrl('r')],
                vec![alt('n')],
                vec![ctrl('o')],
                vec![plain(KeyCode::F(1))],
                vec![plain(KeyCode::Char('z')), plain(KeyCode::Char('z'))],
                vec![plain(KeyCode::PageDown), plain(KeyCode::PageUp)],
            ] {
                let mut a = app();
                a.notice = Some(" a long notice that does not fit anywhere".into());
                for k in keys {
                    a.key(k);
                }
                screen(&mut a, w, h);
                assert!(a.page >= 1);
            }
            let mut a = app();
            a.close_plan(
                "@0",
                crate::ops::ClosePlan::Ask("close \"editor\"? (y/N) ".into()),
            );
            screen(&mut a, w, h);
        }
    }

    /// PgDn moves by at least one row whatever the list height.
    #[test]
    fn page_keys_move_at_least_one_row() {
        let mut a = app();
        screen(&mut a, 40, 4);
        a.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(a.sel, 1);
        a.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(a.sel, 0);
    }
}
