//! `tmux-home popup`: the full-window window manager, run inside
//! `display-popup -E -B -w 100% -h 100%`.

pub mod app;
pub mod fresh;

use crate::{
    client::{self, Answer},
    ipc::Reply,
    ops::{CloseOutcome, Tx},
    store::Store,
    tmux::snapshot::{Snapshot, read_snapshot},
};
use app::{Action, App, HELP, Mode, Row};
use fresh::{Floor, Stamp};
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
const SUBSCRIBE_BUDGET: Duration = Duration::from_millis(150);
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
    /// Set by `refresh` after our own write: older snapshots are dropped.
    floor: Option<Floor>,
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
            self.floor = Some(Floor::of(stamp));
            let rows = self.rows(&s);
            app.set_rows(rows);
        }
    }

    fn update_preview(&mut self, app: &App, force: bool) {
        let pane = app.selected().and_then(|r| r.pane.clone());
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
            Action::Switch { sid, wid } => {
                ratatui::restore();
                let _ = self.tx.switch_to(self.client.as_deref(), &sid, &wid);
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
                // the cursor stays at the same row position (the next window)
                let pos = app.sel;
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
        floor: None,
    };
    let mut app = App::new();
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
        if let Some(f) = latest.filter(|f| fresh::accept(rt.floor.as_ref(), &f.stamp, now)) {
            let rows = rt.rows(&f.snap);
            app.set_rows(rows);
            dirty = true;
        }
        let before = rt.preview_at;
        rt.update_preview(app, false);
        if rt.preview_at != before {
            dirty = true;
        }
        if dirty {
            term.draw(|f| draw(f, app, rt.live, &rt.preview.1))?;
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
/// selected pane's captured text.
fn draw(f: &mut Frame, app: &mut App, live: bool, preview: &str) {
    let area = f.area();
    let bg = Block::default().style(Style::default().bg(Color::Reset));
    f.render_widget(bg, area);
    if app.mode == Mode::Help {
        f.render_widget(Paragraph::new(HELP), area);
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
                format!("{}/{} ", app.visible.len(), app.rows.len()),
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
    draw_list(f, app, list);
    if let Some(p) = prev {
        let block = Block::default()
            .borders(if dir == Direction::Horizontal {
                Borders::LEFT
            } else {
                Borders::TOP
            })
            .border_style(dim);
        let inner = block.inner(p);
        f.render_widget(block, p);
        let lines: Vec<&str> = preview.lines().collect();
        let skip = lines.len().saturating_sub(inner.height as usize);
        let body: Vec<Line> = lines[skip..].iter().map(|l| Line::raw(*l)).collect();
        f.render_widget(Paragraph::new(body), inner);
    }

    f.render_widget(Paragraph::new(Span::styled(app.footer(), dim)), v[3]);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let h = area.height as usize;
    app.page = h.saturating_sub(1).max(1);
    let sw = app
        .rows
        .iter()
        .map(|r| r.session.chars().count())
        .max()
        .unwrap_or(0)
        .min(16);
    let top = app.sel.saturating_sub(h.saturating_sub(1));
    let mut lines = Vec::new();
    let mut prev_sid: Option<&str> = None;
    // the session name is emphasised on the first row of each group
    // (counting rows above the viewport), dimmed on the rest
    if top > 0 {
        prev_sid = Some(&app.rows[app.visible[top - 1]].sid);
    }
    for (pos, &i) in app.visible.iter().enumerate().skip(top).take(h) {
        let r = &app.rows[i];
        let selected = pos == app.sel;
        let first = prev_sid != Some(r.sid.as_str());
        prev_sid = Some(&r.sid);
        let sname: String = r.session.chars().take(sw).collect();
        let sstyle = if first {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let editing = match &app.mode {
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
            Span::styled(
                format!("  {:<10}  {}", r.cmd, r.path),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ];
        if selected {
            for s in spans.iter_mut().skip(1) {
                s.style = s.style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            }
        }
        lines.push(Line::from(std::mem::take(&mut spans)));
    }
    if app.visible.is_empty() {
        lines.push(Line::styled(
            "  (no matches)",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// `tmux-home reopen`: prints the new window ID; exit 4 on an empty stack.
pub fn reopen_cli(socket: Option<PathBuf>) -> anyhow::Result<i32> {
    let socket = client::current_socket(socket)
        .ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let store = Store::for_socket(&socket)?;
    match Tx::new(socket).reopen(&store)? {
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
        t.draw(|f| draw(f, a, true, "line one\nline two")).unwrap();
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
