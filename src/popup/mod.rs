//! `tmux-home popup`: the full-window window manager, run inside
//! `display-popup -E -B -w 100% -h 100%`.

pub mod agents;
pub mod app;
pub mod filter;
pub mod fresh;
pub mod git;
pub mod preview;

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
    crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
        execute,
    },
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
/// A new selection's pane is captured once it has been selected this long:
/// holding ↓ through the list doesn't capture every pane passed.
const PREVIEW_SETTLE: Duration = Duration::from_millis(60);
/// Lines of history the preview captures (`@home-preview-history`).
const PREVIEW_HISTORY: usize = 2000;
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
    /// The previewed pane and its parsed capture.
    preview: (Option<String>, Vec<Line<'static>>),
    preview_at: Instant,
    /// The pane the selection wants previewed, and since when.
    preview_want: (Option<String>, Instant),
    /// Lines of scrollback to capture.
    history: usize,
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

    /// Re-captures the selected pane: a second after the last capture, or
    /// once a new selection has settled (`PREVIEW_SETTLE`; the first
    /// capture is at once).
    fn update_preview(&mut self, app: &mut App) {
        let pane = app.selected().and_then(|r| r.preview_pane());
        if pane != self.preview_want.0 {
            self.preview_want = (pane.clone(), Instant::now());
        }
        if pane == self.preview.0 {
            if self.preview_at.elapsed() < PREVIEW_EVERY {
                return;
            }
        } else if self.preview.0.is_some() && self.preview_want.1.elapsed() < PREVIEW_SETTLE {
            return;
        } else {
            // another pane (an agent window's lead can change too): from
            // its latest output
            app.preview_scroll = 0;
        }
        let lines = match pane.as_deref() {
            Some(p) => preview::parse(&self.tx.capture_styled(p, self.history)),
            None => vec![Line::raw("(no pane)")],
        };
        self.preview = (pane, lines);
        self.preview_at = Instant::now();
    }

    /// Executes an action; returns false when the popup should exit.
    fn act(&mut self, app: &mut App, a: Action) -> bool {
        match a {
            Action::None | Action::Redraw => {}
            Action::Quit => return false,
            Action::Switch { sid, wid, pane } => {
                let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
    let history = tx
        .run(&["show-options", "-gqv", "@home-preview-history"])
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(PREVIEW_HISTORY);
    let mut rt = Runtime {
        tx,
        client,
        store,
        home: std::env::var("HOME").unwrap_or_default(),
        live: false,
        preview: (None, Vec::new()),
        preview_at: Instant::now(),
        preview_want: (None, Instant::now()),
        history,
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
    // the wheel scrolls the preview (tmux passes mouse events on to a
    // popup whose program asks for them)
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut term, &mut app, &mut rt, &feed);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
        rt.update_preview(app);
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
                    rt.update_preview(app);
                    dirty = true;
                }
                Event::Mouse(m) => dirty |= app.mouse(m) != Action::None,
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
/// selected pane's parsed capture, `now` Unix seconds (run times).
fn draw(f: &mut Frame, app: &mut App, live: bool, preview: &[Line<'static>], now: u64) {
    let area = f.area();
    let bg = Block::default().style(Style::default().bg(Color::Reset));
    f.render_widget(bg, area);
    app.preview_rows = 0;
    app.list_hits.clear();
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
        // the pane's lines, clipped at the edge (see `preview`), scrolled
        // `preview_scroll` lines back; the offset is clamped at the top
        let (offset, body) = preview::window(preview, inner.height as usize, app.preview_scroll);
        app.preview_scroll = offset;
        app.preview_rows = inner.height as usize;
        f.render_widget(Paragraph::new(body.to_vec()), inner);
        if offset > 0 && inner.height > 0 && inner.width > 0 {
            let tag = format!(" ↑{offset} ");
            let w = (tag.chars().count() as u16).min(inner.width);
            f.render_widget(
                Paragraph::new(Span::styled(
                    tag,
                    Style::default().add_modifier(Modifier::REVERSED),
                )),
                Rect {
                    x: inner.right() - w,
                    width: w,
                    height: 1,
                    ..inner
                },
            );
        }
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
    app.list_x = area.x..area.right();
    let mut hits = Vec::new();
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
        hits.push((area.y + lines.len() as u16, pos));
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
    app.list_hits = hits;
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
        let p = preview::parse("line one\nline two");
        t.draw(|f| draw(f, a, true, &p, NOW)).unwrap();
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
            t.draw(|f| draw(f, a, true, &[], NOW)).unwrap();
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

    fn wheel(
        kind: ratatui::crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
    ) -> ratatui::crossterm::event::MouseEvent {
        ratatui::crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The preview in colour: SGR colours and attributes reach the cells.
    #[test]
    fn the_preview_renders_colours() {
        let mut a = app();
        let p =
            preview::parse("\x1b[31mred\x1b[0m \x1b[1;44mbold on blue\x1b[0m \x1b[7mrev\x1b[0m");
        let mut t = Terminal::new(TestBackend::new(200, 50)).unwrap();
        t.draw(|f| draw(f, &mut a, true, &p, NOW)).unwrap();
        let buf = t.backend().buffer().clone();
        // the side preview starts at x=101 (after its border), its text at
        // the top (y=2, under the header and the prompt line)
        let row: String = (101..130)
            .map(|x| buf[(x, 2)].symbol().to_string())
            .collect();
        assert!(row.starts_with("red bold on blue rev"), "{row:?}");
        assert_eq!(buf[(101, 2)].fg, Color::Red);
        assert_eq!(buf[(105, 2)].bg, Color::Blue);
        assert!(buf[(105, 2)].modifier.contains(Modifier::BOLD));
        assert!(buf[(118, 2)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(104, 2)].bg, Color::Reset, "reset between spans");
    }

    /// Long lines are clipped at the edge, not wrapped; wide characters
    /// and tabs keep the columns.
    #[test]
    fn preview_lines_are_clipped_not_wrapped() {
        let mut a = app();
        let long = format!("宽\tx{}END", "-".repeat(200));
        let p = preview::parse(&format!("{long}\nlast"));
        let mut t = Terminal::new(TestBackend::new(200, 50)).unwrap();
        t.draw(|f| draw(f, &mut a, true, &p, NOW)).unwrap();
        let buf = t.backend().buffer().clone();
        let line = |y: u16| -> String {
            (101..200)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        };
        assert_eq!(line(3), format!("last{}", " ".repeat(95)), "not wrapped");
        let l = line(2);
        assert!(l.starts_with("宽"), "{l:?}");
        assert_eq!(buf[(109, 2)].symbol(), "x", "the tab reaches column 8");
        assert!(!l.contains("END"));
        assert_eq!(buf[(199, 2)].symbol(), "-", "clipped at the right edge");
    }

    /// The wheel scrolls the preview, wherever the pointer is, and never
    /// the list; moving the selection brings the preview back to the end.
    #[test]
    fn the_wheel_scrolls_only_the_preview() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind as K};
        let text: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let p = preview::parse(&text.join("\n"));
        let mut a = app();
        let draw_at = |a: &mut App| {
            let mut t = Terminal::new(TestBackend::new(200, 50)).unwrap();
            t.draw(|f| draw(f, a, true, &p, NOW)).unwrap();
            let buf = t.backend().buffer().clone();
            (0..50)
                .map(|y| {
                    (101..200)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let s = draw_at(&mut a);
        assert!(s[48].starts_with("line 100"), "{:?}", s[48]);
        let sel = a.sel;
        // over the list (x=10) and over the preview (x=150) alike
        for x in [10, 150] {
            assert_eq!(a.mouse(wheel(K::ScrollUp, x, 5)), Action::Redraw);
        }
        assert_eq!((a.sel, a.preview_scroll), (sel, 6));
        let s = draw_at(&mut a);
        assert!(s[48].starts_with("line 94"), "{:?}", s[48]);
        assert!(s[2].ends_with(" ↑6 "), "the offset shows: {:?}", s[2]);
        a.mouse(wheel(K::ScrollDown, 150, 5));
        assert_eq!((a.sel, a.preview_scroll), (sel, 3));
        // past the top: clamped at the next draw
        for _ in 0..100 {
            a.mouse(wheel(K::ScrollUp, 150, 5));
        }
        let s = draw_at(&mut a);
        assert!(s[2].starts_with("line 1 "), "{:?}", s[2]);
        assert_eq!(a.preview_scroll, 100 - 47);
        a.mouse(wheel(K::ScrollDown, 150, 5));
        assert_eq!(a.preview_scroll, 100 - 47 - 3, "down moves at once");
        // keys: S-↑/S-↓ a line, S-PgUp/S-PgDn half a page, list unmoved
        let shift = |c| KeyEvent::new(c, KeyModifiers::SHIFT);
        a.preview_scroll = 0;
        a.key(shift(KeyCode::Up));
        a.key(shift(KeyCode::Up));
        a.key(shift(KeyCode::Down));
        assert_eq!((a.sel, a.preview_scroll), (sel, 1));
        a.key(shift(KeyCode::PageUp));
        assert_eq!((a.sel, a.preview_scroll), (sel, 1 + 47 / 2));
        a.key(shift(KeyCode::PageDown));
        assert_eq!((a.sel, a.preview_scroll), (sel, 1));
        // a selection change resets the scroll; so does coming back
        a.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_ne!(a.sel, sel);
        assert_eq!(a.preview_scroll, 0);
        a.mouse(wheel(K::ScrollUp, 150, 5));
        a.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!((a.sel, a.preview_scroll), (sel, 0));
        // a snapshot that keeps the selection keeps the scroll
        a.mouse(wheel(K::ScrollUp, 150, 5));
        let rows = a.rows.clone();
        a.set_rows(rows);
        assert_eq!(a.preview_scroll, 3);
        // a left click on a row selects it; on the preview, nothing
        draw_at(&mut a);
        assert_eq!(
            a.mouse(wheel(K::Down(MouseButton::Left), 150, 3)),
            Action::None
        );
        assert_eq!(a.sel, sel);
        a.mouse(wheel(K::Down(MouseButton::Left), 10, 3));
        assert_eq!(
            a.selected().unwrap().name,
            "a much longer window name than fits"
        );
        assert_eq!(a.preview_scroll, 0);
        // the rest of the mouse does nothing
        for k in [
            K::Down(MouseButton::Right),
            K::Moved,
            K::Up(MouseButton::Left),
        ] {
            assert_eq!(a.mouse(wheel(k, 10, 2)), Action::None);
        }
    }

    /// With the preview hidden the wheel does nothing.
    #[test]
    fn no_preview_no_scroll() {
        use ratatui::crossterm::event::MouseEventKind as K;
        let mut a = app();
        screen(&mut a, 120, 20); // small: hidden
        assert_eq!(a.preview_rows, 0);
        a.mouse(wheel(K::ScrollUp, 10, 5));
        a.key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(a.preview_scroll, 0);
        a.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        screen(&mut a, 120, 20);
        assert!(a.preview_rows > 0);
    }

    /// The editors ignore the preview keys, and Shift-arrows in the list
    /// don't reach the filter.
    #[test]
    fn preview_keys_leave_the_editors_alone() {
        let mut a = app();
        screen(&mut a, 200, 50);
        a.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        for c in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            a.key(KeyEvent::new(c, KeyModifiers::SHIFT));
        }
        assert!(matches!(&a.mode, Mode::Rename { edit, .. } if edit.as_string() == "editor"));
        assert_eq!(a.preview_scroll, 0);
        a.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        a.key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert!(a.filter.is_empty());
        assert_eq!(a.preview_scroll, 1);
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
