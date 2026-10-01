//! `tmux-home popup`: the full-window window manager, run inside
//! `display-popup -E -B -w 100% -h 100%`.

pub mod app;

use crate::{
    VERSION,
    ipc::{Reply, Request, read_msg, write_msg},
    ops::{CloseOutcome, Tx},
    paths::Paths,
    store::Store,
    tmux::{
        Tmux,
        snapshot::{Snapshot, read_snapshot},
    },
};
use app::{Action, App, HELP, Mode, Row};
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyEventKind},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Snapshots from the daemon (live) or direct reads (degraded).
struct Feed {
    snap: Snapshot,
    live: bool,
    /// When a direct read started (degraded mode only).
    read_at: Option<Instant>,
}

const DEGRADED_EVERY: Duration = Duration::from_secs(1);
const PREVIEW_EVERY: Duration = Duration::from_millis(1000);

fn spawn_feed(socket: PathBuf) -> mpsc::Receiver<Feed> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(feed_loop(socket, tx));
    });
    rx
}

async fn feed_loop(socket: PathBuf, tx: mpsc::Sender<Feed>) {
    let Ok(paths) = Paths::for_socket(&socket) else {
        return;
    };
    let tmux = Tmux::new(socket.clone());
    let mut last_spawn: Option<Instant> = None;
    loop {
        // live: subscribe until the daemon goes away
        if let Ok(Ok(s)) = tokio::time::timeout(
            Duration::from_millis(150),
            tokio::net::UnixStream::connect(&paths.sock),
        )
        .await
        {
            let (r, mut w) = s.into_split();
            let mut r = tokio::io::BufReader::new(r);
            let req = Request::Subscribe {
                v: VERSION.into(),
                client: "popup".into(),
            };
            if write_msg(&mut w, &req).await.is_ok() {
                loop {
                    match read_msg::<_, Reply>(&mut r).await {
                        Ok(Some(Reply::Snapshot { data, .. })) => {
                            if tx
                                .send(Feed {
                                    snap: data,
                                    live: true,
                                    read_at: None,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(Some(Reply::Restart)) => {
                            // the old daemon removes its socket, then exits
                            for _ in 0..50 {
                                if !paths.sock.exists() {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                            break;
                        }
                        _ => break,
                    }
                }
            }
        }
        // degraded: (re)start a daemon now and then, read directly meanwhile
        if last_spawn.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            last_spawn = Some(Instant::now());
            let _ = crate::client::spawn_daemon(&socket);
        }
        let read_at = Some(Instant::now());
        match read_snapshot(&tmux).await {
            Ok((snap, _)) => {
                if tx
                    .send(Feed {
                        snap,
                        live: false,
                        read_at,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Err(_) => return, // server gone
        }
        tokio::time::sleep(DEGRADED_EVERY).await;
    }
}

pub fn socket_from_env(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
        let t = std::env::var("TMUX").ok()?;
        let s = t.split(',').next()?.to_string();
        (!s.is_empty()).then(|| PathBuf::from(s))
    })
}

struct Runtime {
    tx: Tx,
    tmux: Tmux,
    rt: tokio::runtime::Runtime,
    client: Option<String>,
    store: Store,
    home: String,
    live: bool,
    preview: (Option<String>, String),
    preview_at: Instant,
    /// When the last direct read after our own write started: a degraded
    /// feed read that started earlier predates the write and is dropped.
    read_at: Option<Instant>,
}

impl Runtime {
    fn rows(&self, s: &Snapshot) -> Vec<Row> {
        app::build_rows(s, self.client.as_deref(), &self.home)
    }

    /// Re-read tmux right after a write so the list reflects it at once.
    fn refresh(&mut self, app: &mut App) {
        let at = Instant::now();
        if let Ok((s, _)) = self.rt.block_on(read_snapshot(&self.tmux)) {
            self.read_at = Some(at);
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
                    app.select_wid(&wid);
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
                        app.select_wid(&wid);
                    }
                    Err(e) => app.notice = Some(format!(" new window failed: {e}")),
                }
            }
        }
        true
    }
}

pub fn run(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let socket = socket_from_env(socket)
        .ok_or_else(|| anyhow::anyhow!("not inside tmux and no --socket"))?;
    let tx = Tx::new(socket.clone());
    let client = tx.home_client();
    let store = Store::for_socket(&socket)?;
    let feed = spawn_feed(socket.clone());
    let mut rt = Runtime {
        tx,
        tmux: Tmux::new(socket.clone()),
        rt: tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
        client,
        store,
        home: std::env::var("HOME").unwrap_or_default(),
        live: false,
        preview: (None, String::new()),
        preview_at: Instant::now(),
        read_at: None,
    };
    let mut app = App::new();
    // first picture: read tmux directly, so the cursor lands on the window
    // the client shows now (the daemon's snapshot can be a poll behind);
    // the feed only brings updates after this
    rt.refresh(&mut app);
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
            dirty |= rt.live != f.live;
            rt.live = f.live;
        }
        if let Some(f) = latest.filter(|f| f.read_at.is_none_or(|t| Some(t) >= rt.read_at)) {
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
            term.draw(|f| draw(f, app, rt))?;
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

fn draw(f: &mut Frame, app: &mut App, rt: &Runtime) {
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
    if !rt.live {
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
        let lines: Vec<&str> = rt.preview.1.lines().collect();
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
    let socket = socket_from_env(socket)
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

/// `tmux-home status`: `●` if the daemon answers within 100 ms, else `○`;
/// nothing outside tmux. Never starts a daemon.
pub fn status(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let Some(socket) = socket_from_env(socket) else {
        return Ok(());
    };
    let up = daemon_up(&socket);
    println!("{}", if up { "●" } else { "○" });
    Ok(())
}

fn daemon_up(socket: &Path) -> bool {
    let Ok(p) = Paths::for_socket(socket) else {
        return false;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    rt.block_on(async {
        let ask = async {
            let s = tokio::net::UnixStream::connect(&p.sock).await?;
            let (r, mut w) = s.into_split();
            write_msg(&mut w, &Request::Query { v: VERSION.into() }).await?;
            read_msg::<_, Reply>(&mut tokio::io::BufReader::new(r)).await
        };
        matches!(
            tokio::time::timeout(Duration::from_millis(100), ask).await,
            Ok(Ok(Some(Reply::Snapshot { .. })))
        )
    })
}
