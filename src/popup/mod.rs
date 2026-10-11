//! `tmux-home popup`: the full-window window manager, run inside
//! `display-popup -E -B -w 100% -h 100%`.

pub mod agents;
pub mod app;
pub mod filter;
pub mod fresh;
pub mod git;
pub mod preview;
pub mod term;

use crate::{
    client::{self, Answer},
    ipc::Reply,
    ops::{CloseOutcome, Tx},
    store::Store,
    tmux::snapshot::{Snapshot, read_snapshot},
};
use app::{Action, App, Item, Mode, Row};
use fresh::{Guard, Stamp};
use preview::Shot;
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
pub(crate) struct Feed {
    pub snap: Snapshot,
    pub stamp: Stamp,
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
/// `client` names the subscriber in the daemon's requests. Without a
/// daemon it starts one, but not before Unix time `revive_from` (the
/// sidebar: not while the server is starting up); meanwhile it reads tmux
/// directly and keeps trying to subscribe.
pub(crate) fn spawn_feed(
    socket: PathBuf,
    client: &'static str,
    revive_from: Option<u64>,
) -> mpsc::Receiver<Feed> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || feed_loop(socket, client, revive_from, tx));
    rx
}

fn feed_loop(
    socket: PathBuf,
    client: &'static str,
    revive_from: Option<u64>,
    tx: mpsc::Sender<Feed>,
) {
    let tmux = crate::tmux::Tmux::new(socket.clone());
    let mut last_spawn: Option<Instant> = None;
    loop {
        // live: subscribe until the daemon goes away
        let since = Instant::now();
        let restart = match client::ask(&socket, &client::subscribe_req(client), SUBSCRIBE_BUDGET) {
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
        let may = revive_from.is_none_or(|t| crate::agent::now() >= t);
        if may && (restart || last_spawn.is_none_or(|t| t.elapsed() > RESPAWN_EVERY)) {
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

/// What the preview shows: a window's panes, one of them selected (a
/// pane row) or none (the window row).
#[derive(Clone, Debug, PartialEq)]
struct Target {
    wid: String,
    panes: Vec<String>,
    pane: Option<String>,
}

struct Runtime {
    tx: Tx,
    client: Option<String>,
    store: Store,
    home: String,
    live: bool,
    /// The previewed window (and selected pane), and its panes' captures.
    preview: (Option<Target>, Shot),
    preview_at: Instant,
    /// What the selection wants previewed, and since when.
    preview_want: (Option<Target>, Instant),
    /// Lines of scrollback to capture for a selected pane.
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

    /// Re-captures the selected window's panes: a second after the last
    /// capture, or once a new selection has settled (`PREVIEW_SETTLE`; the
    /// first capture is at once). Each pane's visible part; a selected
    /// pane's with history too, as it can be scrolled.
    fn update_preview(&mut self, app: &mut App) {
        let target = app.selected().map(|r| Target {
            wid: r.wid.clone(),
            panes: r.panes.iter().map(|p| p.id.clone()).collect(),
            pane: app.selected_pane().map(|p| p.id.clone()),
        });
        if target != self.preview_want.0 {
            self.preview_want = (target.clone(), Instant::now());
        }
        if target == self.preview.0 {
            if self.preview_at.elapsed() < PREVIEW_EVERY {
                return;
            }
        } else if self.preview.0.is_some() && self.preview_want.1.elapsed() < PREVIEW_SETTLE {
            return;
        } else {
            // another window or pane: from its latest output
            app.preview_scroll = 0;
        }
        let mut shot = Shot::default();
        for p in target.iter().flat_map(|t| &t.panes) {
            let selected = target.as_ref().and_then(|t| t.pane.as_ref()) == Some(p);
            let history = if selected { self.history } else { 0 };
            let lines = preview::parse(&self.tx.capture_styled(p, history));
            shot.panes.insert(p.clone(), lines);
        }
        self.preview = (target, shot);
        self.preview_at = Instant::now();
    }

    /// Executes an action; returns false when the popup should exit.
    fn act(&mut self, app: &mut App, a: Action) -> bool {
        match a {
            Action::None | Action::Redraw => {}
            Action::Quit => return false,
            Action::Switch { sid, wid, pane } => {
                // mouse off before leaving the alternate screen and before
                // tmux shows the target window
                term::MouseGuard::off();
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
                app.sel_key = None;
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
                        if !app.visible.iter().any(|it| app.rows[it.row].wid == wid) {
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
    let feed = spawn_feed(socket.clone(), "popup", None);
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
        preview: (None, Shot::default()),
        preview_at: Instant::now(),
        preview_want: (None, Instant::now()),
        history,
        guard: Guard::default(),
    };
    let mut app = App::new();
    app.home = rt.home.clone();
    app.host = rt
        .tx
        .run(&["display-message", "-p", "#{host}"])
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    app.chord = app::Chord::from_env();
    // first picture: read tmux directly, so the cursor lands on the window
    // the client shows now; no floor, since nothing was written (the feed's
    // first snapshot is a fresh read too)
    if let Ok(s) = read_snapshot(&rt.tx.tmux) {
        app.set_rows(rt.rows(&s));
    }
    app.select_current();

    let mut term = ratatui::init();
    // after ratatui's: a panic turns the mouse off, then ratatui restores
    term::install_panic_hook();
    // the wheel scrolls the preview (tmux passes mouse events on to a
    // popup whose program asks for them); off again when the guard drops,
    // whichever way the loop ends
    let mouse = term::MouseGuard::on();
    let result = event_loop(&mut term, &mut app, &mut rt, &feed);
    drop(mouse);
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
            if term::test_panic("loop") {
                panic!("TMUX_HOME_TEST_PANIC=loop");
            }
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

/// The preview takes the bottom half of the list area when the popup is
/// at least this tall; below that only after ^o.
const PREVIEW_MIN_HEIGHT: u16 = 30;

/// The list and the preview below it. `height` is the popup's; `flip`
/// (^o) shows a hidden preview or hides a shown one.
fn preview_rect(area: Rect, height: u16, flip: bool) -> (Rect, Option<Rect>) {
    if (height >= PREVIEW_MIN_HEIGHT) == flip {
        return (area, None);
    }
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    (parts[0], Some(parts[1]))
}

/// The table's columns, in order (see `app::COLUMNS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Idx,
    Name,
    Cmd,
    State,
    Time,
    Agent,
    Path,
    Git,
    Msg,
}

const COLS: [Col; 9] = [
    Col::Idx,
    Col::Name,
    Col::Cmd,
    Col::State,
    Col::Time,
    Col::Agent,
    Col::Path,
    Col::Git,
    Col::Msg,
];

/// Dropped in this order when the row is too narrow (after `Msg`, which
/// only takes what is left).
const DROP: [Col; 6] = [
    Col::Git,
    Col::Path,
    Col::Time,
    Col::Agent,
    Col::Cmd,
    Col::State,
];
/// The selection bar, the current-window mark and a space.
const GUTTER: usize = 3;
const SEP: &str = "  ";
const NAME_MIN: usize = 8;
const MSG_MIN: usize = 8;

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Idx => "#",
            Col::Name => "NM",
            Col::Cmd => "CM",
            Col::State => "ST",
            Col::Time => "T",
            Col::Agent => "AG",
            Col::Path => "PA",
            Col::Git => "GT",
            Col::Msg => "MS",
        }
    }

    /// The widest a column gets.
    fn cap(self) -> usize {
        match self {
            Col::Idx | Col::Msg => usize::MAX,
            Col::Name | Col::Path => 24,
            Col::Cmd | Col::Agent => 12,
            Col::State => 18,
            Col::Time => 6,
            Col::Git => 32,
        }
    }
}

/// The column widths of a page; a width of 0 is a column not shown.
#[derive(Clone, Debug, PartialEq)]
struct Table {
    w: [usize; 9],
}

impl Table {
    fn on(&self, c: Col) -> bool {
        self.w[c as usize] > 0
    }

    /// Columns used (gutter and separators included), `Msg` aside.
    fn used(&self) -> usize {
        let cols: Vec<usize> = COLS[..8]
            .iter()
            .filter(|&&c| self.on(c))
            .map(|&c| self.w[c as usize])
            .collect();
        GUTTER + cols.iter().sum::<usize>() + SEP.len() * cols.len().saturating_sub(1)
    }
}

fn spans_width(v: &[Span]) -> usize {
    v.iter().map(Span::width).sum()
}

/// `s` in at most `w` columns (display width), cut with `…`.
fn clip_w(s: &str, w: usize) -> String {
    if Span::raw(s).width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let mut b = [0; 4];
        let cw = Span::raw(&*c.encode_utf8(&mut b)).width();
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    if w > 0 {
        out.push('…');
    }
    out
}

/// Whether a pane title says more than the host name tmux gives a pane.
fn shows_title(title: &str, host: &str) -> bool {
    let short = host.split('.').next().unwrap_or(host);
    !title.trim().is_empty() && title != host && title != short
}

/// The cells of a line, one per column (empty: nothing to show).
fn cells(app: &App, it: Item, now: u64) -> [Vec<Span<'static>>; 9] {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let r = &app.rows[it.row];
    let mut c: [Vec<Span<'static>>; 9] = Default::default();
    let mut set = |col: Col, s: Span<'static>| c[col as usize] = vec![s];
    match it.pane {
        None => {
            set(Col::Idx, Span::raw(r.index.to_string()));
            set(Col::Name, Span::raw(r.name.clone()));
            if let Some(a) = &r.agents {
                set(
                    Col::State,
                    match a.status() {
                        Some(s) => agents::state_span(s, false),
                        None => agents::state_span(a.lead().state.status, true),
                    },
                );
                let n = a.live_count();
                let kind = match n {
                    0 | 1 => a.lead().state.kind.name().to_string(),
                    n => format!("{} ×{n}", a.lead().state.kind.name()),
                };
                set(
                    Col::Agent,
                    Span::styled(kind, if a.stale() { dim } else { Style::default() }),
                );
            }
        }
        Some(pi) => {
            let p = &r.panes[pi];
            let branch = if pi + 1 == r.panes.len() {
                '└'
            } else {
                '├'
            };
            set(
                Col::Idx,
                Span::styled(format!("{branch} .{}", p.index), dim),
            );
            if shows_title(&p.title, &app.host) {
                set(Col::Name, Span::styled(p.title.clone(), dim));
            }
            set(Col::Cmd, Span::raw(p.cmd.clone()));
            if let Some(a) = &p.agent {
                set(Col::State, agents::state_span(a.state.status, a.stale));
                if let Some(t) = a.state.run_started.filter(|_| a.live()) {
                    set(Col::Time, Span::raw(crate::agent::elapsed(t, now)));
                }
                set(
                    Col::Agent,
                    Span::styled(
                        a.state.kind.name().to_string(),
                        if a.stale { dim } else { Style::default() },
                    ),
                );
                if a.needs_you() {
                    set(
                        Col::Msg,
                        Span::styled(agents::need_reason(a), Style::default().fg(Color::Yellow)),
                    );
                } else if let Some(pr) = &a.state.prompt {
                    set(Col::Msg, Span::styled(pr.replace('\n', " "), dim));
                }
            }
            set(Col::Path, Span::styled(p.path.clone(), dim));
            if let Some((_, g)) = &p.git {
                c[Col::Git as usize] = git::badge_pieces(g);
            }
        }
    }
    c
}

/// Every line of the page, filter or not: column widths don't move as
/// the filter narrows the list.
fn page_items(app: &App) -> Vec<Item> {
    app.rows
        .iter()
        .enumerate()
        .filter(|(_, r)| Some(&r.sid) == app.tab.as_ref())
        .flat_map(|(i, r)| {
            std::iter::once(Item { row: i, pane: None }).chain((0..r.panes.len()).map(move |p| {
                Item {
                    row: i,
                    pane: Some(p),
                }
            }))
        })
        .collect()
}

/// The page's column widths in `width` columns: each column as wide as
/// its widest cell (capped; at least its label), columns nothing fills
/// left out. `MS` takes what the others leave, if that is enough; too
/// narrow, it goes, then the others right to left (`DROP`), then `NM`
/// gives way down to `NAME_MIN`.
fn table(app: &App, width: usize, now: u64) -> Table {
    let mut w = [0usize; 9];
    for it in page_items(app) {
        for (i, cell) in cells(app, it, now).iter().enumerate() {
            w[i] = w[i].max(spans_width(cell));
        }
    }
    for c in COLS {
        let i = c as usize;
        if w[i] > 0 || matches!(c, Col::Idx | Col::Name) {
            w[i] = w[i].min(c.cap()).max(c.label().len());
        }
    }
    let mut t = Table { w };
    // MS goes first: it shows only if everything else fits
    let rest = width.saturating_sub(t.used() + SEP.len());
    let msg = &mut t.w[Col::Msg as usize];
    *msg = if *msg > 0 && rest >= MSG_MIN { rest } else { 0 };
    for c in DROP {
        if t.used() <= width {
            break;
        }
        t.w[c as usize] = 0;
    }
    let over = t.used().saturating_sub(width);
    let name = &mut t.w[Col::Name as usize];
    *name = name.saturating_sub(over).max(NAME_MIN.min(*name));
    t
}

/// A line's cells laid out in the table's columns (after the gutter).
fn table_spans(t: &Table, cells: [Vec<Span<'static>>; 9]) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut first = true;
    for (c, cell) in COLS.iter().zip(cells) {
        let w = t.w[*c as usize];
        if w == 0 {
            continue;
        }
        if !first {
            out.push(Span::raw(SEP));
        }
        first = false;
        let fitted = if *c == Col::Git {
            git::fit_badge(cell, w)
        } else {
            cell.into_iter()
                .map(|s| Span::styled(clip_w(&s.content, w), s.style))
                .collect()
        };
        let pad = w.saturating_sub(spans_width(&fitted));
        out.extend(fitted);
        if pad > 0 {
            out.push(Span::raw(" ".repeat(pad)));
        }
    }
    out
}

/// Draws the popup; `live` is false in degraded mode, `shot` the selected
/// window's pane captures, `now` Unix seconds (run times).
fn draw(f: &mut Frame, app: &mut App, live: bool, shot: &Shot, now: u64) {
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
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let dim = Style::default().add_modifier(Modifier::DIM);

    draw_tabs(f, app, live, v[0]);

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
        // the page's windows, not lines
        let (shown, all) = app.counts();
        f.render_widget(
            Paragraph::new(Span::styled(format!("{shown}/{all} "), dim))
                .alignment(ratatui::layout::Alignment::Right),
            v[1],
        );
    }
    let cx = prompt.chars().count() + edit.map_or(0, |e| e.cur);
    f.set_cursor_position((v[1].x + cx as u16, v[1].y));

    // column header, list, preview
    let (list, prev) = preview_rect(v[3], area.height, app.preview_flip);
    let t = table(app, list.width as usize, now);
    let mut head = vec![Span::raw(" ".repeat(GUTTER))];
    head.extend(table_spans(&t, COLS.map(|c| vec![Span::raw(c.label())])));
    f.render_widget(Paragraph::new(Line::from(head).style(dim)), v[2]);
    draw_list(f, app, list, &t, now);
    if let Some(p) = prev {
        draw_preview(f, app, p, shot, now);
    }

    f.render_widget(Paragraph::new(Span::styled(app.footer(), dim)), v[4]);
}

/// The tab strip: every session; the page shown reversed and bold, the
/// client's session marked `▶`, `●N` its windows that need you. Tabs
/// that don't fit around the page shown give way (`‹`/`›`).
fn draw_tabs(f: &mut Frame, app: &App, live: bool, area: Rect) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let right = format!("{}F1 help ", if live { "" } else { "(direct)  " });
    f.render_widget(
        Paragraph::new(Span::styled(right.clone(), dim))
            .alignment(ratatui::layout::Alignment::Right),
        area,
    );
    if app.tabs.is_empty() {
        f.render_widget(Paragraph::new(" tmux-home"), area);
        return;
    }
    let room = (area.width as usize).saturating_sub(right.chars().count() + 3);
    let tab = |t: &app::Tab| -> Vec<Span<'static>> {
        let base = if Some(&t.sid) == app.tab.as_ref() {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        let mark = if t.client { "▶" } else { "" };
        let mut v = vec![Span::styled(format!(" {mark}{} ", t.name), base)];
        if t.attn > 0 {
            v.push(Span::styled(
                format!("●{} ", t.attn),
                base.fg(Color::Yellow),
            ));
        }
        v
    };
    let tabs: Vec<Vec<Span>> = app.tabs.iter().map(tab).collect();
    let w: Vec<usize> = tabs.iter().map(|t| spans_width(t)).collect();
    let cur = app
        .tab
        .as_ref()
        .and_then(|s| app.tabs.iter().position(|t| &t.sid == s))
        .unwrap_or(0);
    let (mut lo, mut hi, mut total) = (cur, cur, w[cur]);
    loop {
        let mut grew = false;
        if hi + 1 < tabs.len() && total + 1 + w[hi + 1] <= room {
            hi += 1;
            total += 1 + w[hi];
            grew = true;
        }
        if lo > 0 && total + 1 + w[lo - 1] <= room {
            lo -= 1;
            total += 1 + w[lo];
            grew = true;
        }
        if !grew {
            break;
        }
    }
    let mut spans = vec![Span::styled(if lo > 0 { "‹" } else { " " }, dim)];
    for (i, t) in tabs.into_iter().enumerate().take(hi + 1).skip(lo) {
        if i > lo {
            spans.push(Span::raw(" "));
        }
        spans.extend(t);
    }
    if hi + 1 < app.tabs.len() {
        spans.push(Span::styled("›", dim));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect, t: &Table, now: u64) {
    let h = area.height as usize;
    app.page = h.saturating_sub(1).max(1);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let width = area.width as usize;
    // The list as lines: a NEEDS YOU group (when a window of the page
    // needs you) gets a header, and so do the session's windows below it.
    enum ListLine {
        Header(String),
        Row(usize),
    }
    let mut items = Vec::new();
    let mut in_pinned = false;
    for (pos, it) in app.visible.iter().enumerate() {
        let r = &app.rows[it.row];
        if r.pinned && !in_pinned {
            items.push(ListLine::Header(" NEEDS YOU ".into()));
        } else if !r.pinned && in_pinned {
            items.push(ListLine::Header(format!(" {} ", r.session)));
        }
        in_pinned = r.pinned;
        items.push(ListLine::Row(pos));
    }
    let sel_line = items
        .iter()
        .position(|it| matches!(it, ListLine::Row(p) if *p == app.sel))
        .unwrap_or(0);
    let top = sel_line.saturating_sub(h.saturating_sub(1));
    app.list_x = area.x..area.right();
    let sel_row = app.selected_item().map(|it| it.row);
    let mut hits = Vec::new();
    let mut lines = Vec::new();
    for (n, line) in items.iter().enumerate() {
        if n < top || lines.len() >= h {
            continue;
        }
        let pos = match line {
            ListLine::Header(text) => {
                let rule = "─".repeat(width.saturating_sub(text.chars().count() + 2));
                lines.push(Line::styled(format!(" ─{text}{rule}"), dim));
                continue;
            }
            ListLine::Row(p) => *p,
        };
        let it = app.visible[pos];
        let r = &app.rows[it.row];
        hits.push((area.y + lines.len() as u16, pos));
        let selected = pos == app.sel;
        let mut cells = cells(app, it, now);
        // the editor/confirm is on the selected line's window row (its
        // pinned copy or not)
        let editing = it.pane.is_none()
            && sel_row == Some(it.row)
            && match &app.mode {
                Mode::Rename { wid, .. } | Mode::Confirm { wid, .. } => wid == &r.wid,
                _ => false,
            };
        if editing {
            for s in &mut cells[Col::Name as usize] {
                s.style = s.style.add_modifier(Modifier::UNDERLINED);
            }
        }
        let mut spans = vec![
            Span::styled(
                if selected { "▌" } else { " " },
                Style::default().fg(Color::Magenta),
            ),
            Span::styled(
                if r.current && it.pane.is_none() {
                    "▶"
                } else {
                    " "
                },
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" "),
        ];
        spans.extend(table_spans(t, cells));
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

/// The preview: the agent and git cards of the selected line (a window
/// row: its lead pane's; a pane row: that pane's), then the window's pane
/// layout, each pane's capture in its scaled rectangle; on a pane row
/// that pane is outlined in magenta and scrolls.
fn draw_preview(f: &mut Frame, app: &mut App, area: Rect, shot: &Shot, now: u64) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let block = Block::default().borders(Borders::TOP).border_style(dim);
    let mut inner = block.inner(area);
    f.render_widget(block, area);
    let (Some(r), pane) = (app.selected().cloned(), app.selected_pane().cloned()) else {
        return;
    };
    let width = inner.width as usize;
    let mut cards = Vec::new();
    match &pane {
        Some(p) => {
            if let Some(a) = &p.agent {
                let one = crate::agent::WindowAgents {
                    panes: vec![a.clone()],
                };
                cards.extend(agents::card(&one, now, width));
            }
            if let Some((root, g)) = &p.git {
                cards.extend(git::card(g, root, width, &app.home));
            }
        }
        None => {
            if let Some(a) = &r.agents {
                cards.extend(agents::card(a, now, width));
            }
            if let Some((root, g)) = &r.git {
                cards.extend(git::card(g, root, width, &app.home));
            }
        }
    }
    // the layout's frame takes the place of the last card's rule
    cards.pop();
    if !cards.is_empty() {
        let keep = (inner.height / 3).max(5);
        let h = (cards.len() as u16).min(inner.height.saturating_sub(keep));
        f.render_widget(Paragraph::new(cards), Rect { height: h, ..inner });
        inner.y += h;
        inner.height -= h;
    }
    let geo: Vec<_> = r.panes.iter().map(|p| p.rect).collect();
    let rects = preview::place(&geo, inner);
    let hl = pane
        .as_ref()
        .and_then(|p| r.panes.iter().position(|x| x.id == p.id));
    for (i, (p, rect)) in r.panes.iter().zip(&rects).enumerate() {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }
        let lines = shot.panes.get(&p.id).map(Vec::as_slice).unwrap_or(&[]);
        let selected = Some(i) == hl;
        // the pane's last lines, clipped at the edge (see `preview`); the
        // selected one's scrolled `preview_scroll` lines back, clamped
        let scroll = if selected { app.preview_scroll } else { 0 };
        let (offset, body) = preview::window(lines, rect.height as usize, scroll);
        f.render_widget(Paragraph::new(body.to_vec()), *rect);
        if selected {
            app.preview_scroll = offset;
            app.preview_rows = rect.height as usize;
            if offset > 0 {
                let tag = format!(" ↑{offset} ");
                let w = (tag.chars().count() as u16).min(rect.width);
                f.render_widget(
                    Paragraph::new(Span::styled(
                        tag,
                        Style::default().add_modifier(Modifier::REVERSED),
                    )),
                    Rect {
                        x: rect.right() - w,
                        width: w,
                        height: 1,
                        ..*rect
                    },
                );
            }
        }
    }
    let buf = f.buffer_mut();
    for (x, y, c, outline) in preview::borders(inner, &rects, hl) {
        let style = if outline {
            Style::default().fg(Color::Magenta)
        } else {
            dim
        };
        buf[(x, y)].set_char(c).set_style(style);
    }
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
        buffer::Buffer,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind as K},
    };

    fn plain(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn shift(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::SHIFT)
    }

    /// Session alpha, the client on its first window: editor (`@0`, one
    /// pane `%0`) and a window with a long name (`@1`, `%1`).
    fn snap() -> Snapshot {
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
                active: true,
                current_command: "sh".into(),
                current_path: "/tmp".into(),
                title: "host".into(),
                width: 80,
                height: 24,
                window_width: 80,
                window_height: 24,
                ..Default::default()
            });
        }
        s.clients.push(Client {
            name: "c".into(),
            tty: "c".into(),
            session_id: "$0".into(),
        });
        s
    }

    fn app() -> App {
        app_of(&snap())
    }

    fn app_of(s: &Snapshot) -> App {
        let mut a = App::new();
        a.home = "/home/u".into();
        a.host = "host.example.org".into();
        a.set_rows(app::build_rows(s, Some("c"), "/home/u"));
        a.select_current();
        a
    }

    /// Every pane of the app's rows captured as `text`.
    fn shot_of(a: &App, text: &str) -> Shot {
        let lines = preview::parse(text);
        let mut shot = Shot::default();
        for p in a.rows.iter().flat_map(|r| &r.panes) {
            shot.panes.insert(p.id.clone(), lines.clone());
        }
        shot
    }

    fn render(a: &mut App, w: u16, h: u16, shot: &Shot) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, a, true, shot, NOW)).unwrap();
        t.backend().buffer().clone()
    }

    fn text_of(buf: &Buffer) -> String {
        let a = buf.area;
        (0..a.height)
            .map(|y| {
                (0..a.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(a: &mut App, w: u16, h: u16) -> String {
        let shot = shot_of(a, "line one\nline two");
        text_of(&render(a, w, h, &shot))
    }

    fn line(s: &str, pat: &str) -> String {
        s.lines()
            .find(|l| l.contains(pat))
            .unwrap_or_else(|| panic!("no {pat:?} in\n{s}"))
            .to_string()
    }

    /// The (char) column `pat` starts at in `l`.
    fn col(l: &str, pat: &str) -> usize {
        let b = l.find(pat).unwrap_or_else(|| panic!("no {pat:?} in {l:?}"));
        l[..b].chars().count()
    }

    const NOW: u64 = 1_790_000_000;

    /// Session "dev": an agent waiting on a permission prompt (`@0`), two
    /// running agents in one window (`@1`), a stale one (`@2`) and a plain
    /// shell (`@3`); the client is on `@3`.
    fn agent_app() -> App {
        app_of(&agent_snap())
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
            ("@pane_prompt", "add the tests"),
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
    fn git_badges_on_pane_rows_and_the_card() {
        let mut a = app_of(&git_snap());
        let s = screen(&mut a, 200, 50);
        // a pane row: command, path, then the badge
        let shell = line(&s, "~/d/a/src");
        assert!(shell.contains("fish"), "{s}");
        assert!(shell.contains("main +!? ⇡2 ⇣1 $1 ⚠2 ↻"), "{s}");
        assert!(col(&shell, "main") > col(&shell, "~/d/a/src"), "{s}");
        // the agent's pane: its worktree's badge, pinned copy and session
        // row alike
        assert_eq!(s.matches("fix/x (wt) ! |").count(), 2, "{s}");
        // a stale status ends in ~
        assert!(line(&s, "~/d/old").contains("legacy ~"), "{s}");
        // window rows carry no path or badge
        assert!(!line(&s, " shell").contains("main"), "{s}");
        // the card of the selected (current) window: its pane's repo
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
        // a stale repo's card says so, from its pane row too
        a.select_key("@2/%2");
        let s = screen(&mut a, 200, 50);
        assert!(s.contains("~ stale: the last check timed out"), "{s}");
        // filtering matches the branch
        let mut a = app_of(&git_snap());
        for c in "legacy".chars() {
            a.key(plain(KeyCode::Char(c)));
        }
        assert_eq!(a.counts().0, 1);
        assert_eq!(a.selected().unwrap().name, "old");
    }

    /// Narrow terminals drop the badge whole (it is a column), never part
    /// of a piece (`⚠12` must not become `⚠1`).
    #[test]
    fn badges_are_never_cut() {
        let mut snap = git_snap();
        snap.git.repos.get_mut("/home/u/dev/app").unwrap().stray = 12;
        let pieces = "main +!? ⇡2 ⇣1 $1 ⚠12 ↻";
        for w in (40u16..=200).step_by(4) {
            let mut a = app_of(&snap);
            let s = screen(&mut a, w, 50);
            let shell = s
                .lines()
                .find(|l| l.contains("└ .0") && l.contains("fish") && !l.contains("waiting"));
            let Some(shell) = shell else { continue };
            assert!(shell.chars().count() <= w as usize);
            let has = shell.contains("main");
            assert!(
                !has || shell.contains(pieces),
                "{w}: a partial badge: {shell:?}"
            );
            if w >= 160 {
                assert!(has, "{w}: room for it: {shell:?}");
            }
        }
    }

    #[test]
    fn help_has_the_keys_columns_and_badge_legend() {
        let mut a = app();
        a.key(plain(KeyCode::F(1)));
        let s = screen(&mut a, 100, 80);
        for want in [
            "^t              reopen",
            "prefix )  (     next / previous session page",
            "columns",
            "NM              window name / pane title",
            "MS              — / why it needs you",
            "git badge",
            "⚠n stray branches",
            "~ stale",
            "press any key",
        ] {
            assert!(s.contains(want), "no {want:?} in\n{s}");
        }
        assert!(!s.contains("s:<session>"), "{s}");
    }

    #[test]
    fn agent_rows_states_and_needs_you() {
        let mut a = agent_app();
        for (w, h) in [(200, 50), (120, 30)] {
            let s = screen(&mut a, w, h);
            let lines: Vec<&str> = s.lines().collect();
            // the tab strip: the client's session, one window needs you
            assert!(lines[0].contains("▶dev ●1"), "{s}");
            let needs = lines
                .iter()
                .position(|l| l.contains("─ NEEDS YOU ─"))
                .unwrap();
            assert!(lines[needs + 1].contains("fix"), "{s}");
            assert!(lines[needs + 1].contains("◐ waiting"), "{s}");
            assert!(lines[needs + 2].contains("└ .0"), "{s}");
            assert!(lines[needs + 2].contains("permission"), "{s}");
            assert!(lines[needs + 3].contains("─ dev ─"), "{s}");
            // the window: its most urgent state, the kind ×N; its panes:
            // each one's state and run time
            let api = line(&s, " api");
            assert!(
                api.contains("● running") && api.contains("claude ×2"),
                "{s}"
            );
            assert!(!api.contains("12m"), "no run time on a window: {s}");
            let api_panes: Vec<&str> = lines
                .iter()
                .skip_while(|l| !l.contains(" api"))
                .skip(1)
                .take(2)
                .copied()
                .collect();
            assert!(
                api_panes[0].contains("├ .0") && api_panes[1].contains("└ .1"),
                "{s}"
            );
            for p in &api_panes {
                assert!(p.contains("● running") && p.contains("12m"), "{s}");
            }
            assert!(line(&s, " old").contains("◐ waiting (ended?)"), "{s}");
            // the fix window is listed twice: pinned, and in its session
            assert_eq!(s.matches(" fix ").count(), 2, "{s}");
            assert!(line(&s, "> ").ends_with("4/4"), "windows, not lines: {s}");
        }
        // the prompt shows (dim) on a pane row when nothing is waiting
        let s = screen(&mut a, 200, 50);
        assert!(line(&s, "├ .0").contains("add the tests"), "{s}");
    }

    /// The column header names the columns over the cells they head; the
    /// cells line up from row to row.
    #[test]
    fn column_header_and_aligned_columns() {
        let mut a = app_of(&git_snap());
        let s = screen(&mut a, 200, 50);
        let head = s.lines().nth(2).unwrap().to_string();
        let labels = ["#", "NM", "CM", "ST", "T", "AG", "PA", "GT", "MS"];
        let words: Vec<(usize, &str)> = head
            .char_indices()
            .filter(|&(i, c)| c != ' ' && (i == 0 || head[..i].ends_with(' ')))
            .map(|(i, _)| {
                (
                    head[..i].chars().count(),
                    head[i..].split(' ').next().unwrap(),
                )
            })
            .collect();
        assert_eq!(words.iter().map(|w| w.1).collect::<Vec<_>>(), labels);
        let at: Vec<usize> = words.iter().map(|w| w.0).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "in order: {head:?}");
        // every window row's name, and every pane row's command, path and
        // state, starts under its label
        for name in ["fix", "api", "old", "shell"] {
            let l = line(&s, &format!(" {name}"));
            assert_eq!(col(&l, name), at[1], "{l:?}\n{s}");
        }
        for l in s
            .lines()
            .filter(|l| l.contains("└ .0") || l.contains("├ .0"))
        {
            let cmd = if l.contains("fish") {
                "fish"
            } else {
                "2.1.283"
            };
            assert_eq!(col(l, cmd), at[2], "{l:?}\n{s}");
            let path = if l.contains('~') { "~" } else { " / " };
            assert_eq!(
                col(l, path) + usize::from(path == " / "),
                at[6],
                "{l:?}\n{s}"
            );
        }
        for l in s.lines().filter(|l| l.contains("● running")) {
            assert_eq!(col(l, "●"), at[3], "{l:?}\n{s}");
        }
        let pane = line(&s, "~/d/a/src");
        assert_eq!(col(&pane, "main"), at[7], "{s}");
        let wait = s
            .lines()
            .find(|l| l.contains("permission") && l.contains("└"))
            .unwrap();
        assert_eq!(col(wait, "permission"), at[8], "{s}");
        // the header is dim
        let buf = render(&mut a, 200, 50, &Shot::default());
        assert!(buf[(4, 2)].modifier.contains(Modifier::DIM));
    }

    /// Narrow: columns go right to left (MS, GT, PA, T, …); # and NM stay.
    #[test]
    fn narrow_widths_drop_columns_right_to_left() {
        let head = |w| {
            let mut a = app_of(&git_snap());
            let s = screen(&mut a, w, 50);
            s.lines()
                .nth(2)
                .unwrap()
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let all = ["#", "NM", "CM", "ST", "T", "AG", "PA", "GT", "MS"];
        assert_eq!(head(200), all);
        let order = ["MS", "GT", "PA", "T", "AG", "CM", "ST"];
        let mut last = 0;
        for w in (30..200).rev().step_by(3) {
            let h = head(w);
            let gone: Vec<&str> = all
                .iter()
                .filter(|l| !h.contains(&l.to_string()))
                .copied()
                .collect();
            let n = gone.len();
            let mut want = order[..n].to_vec();
            want.sort_by_key(|l| all.iter().position(|a| a == l));
            assert_eq!(gone, want, "{w}: in order: {h:?}");
            assert!(n >= last, "{w}: columns come back: {h:?}");
            last = n;
        }
        assert_eq!(head(30), ["#", "NM"]);
        // NM keeps its minimum
        let mut a = app_of(&git_snap());
        let s = screen(&mut a, 30, 50);
        assert!(s.contains("shell"), "{s}");
    }

    #[test]
    fn the_tab_strip_marks_page_client_and_attention() {
        let mut s = agent_snap();
        for (sid, name) in [("$1", "beta"), ("$2", "zeta")] {
            s.sessions.push(Session {
                id: sid.into(),
                name: name.into(),
                attached: 0,
            });
            s.windows.push(Window {
                id: format!("@{sid}"),
                session_id: sid.into(),
                index: 0,
                name: "w".into(),
                automatic_rename: false,
                active: true,
            });
        }
        let mut a = app_of(&s);
        let buf = render(&mut a, 120, 30, &Shot::default());
        let t = text_of(&buf);
        let top = t.lines().next().unwrap();
        assert!(
            top.starts_with("  beta   ▶dev ●1   zeta"),
            "by name: {top:?}"
        );
        assert!(top.ends_with("F1 help"), "{top:?}");
        let x = col(top, "▶dev") as u16;
        assert!(
            buf[(x, 0)]
                .modifier
                .contains(Modifier::REVERSED | Modifier::BOLD)
        );
        assert!(
            !buf[(3, 0)].modifier.contains(Modifier::REVERSED),
            "beta isn't shown"
        );
        let dot = col(top, "●1") as u16;
        assert_eq!(buf[(dot, 0)].fg, Color::Yellow);
        // narrow: the page's tab stays in view
        let t = text_of(&render(&mut a, 24, 30, &Shot::default()));
        assert!(t.lines().next().unwrap().contains("dev"), "{t}");
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
        // a pane row: that pane's agent
        a.select_key("@1/%11");
        let s = screen(&mut a, 200, 50);
        assert!(s.contains("● running  ·  claude  ·  run 12m"), "{s}");
        assert!(!s.contains("2 agents"), "one pane's card: {s}");
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

    /// ^r (or ^x's confirm) from a pinned copy marks that copy's window
    /// row; from the session row, that row; from a pane row, its window's.
    #[test]
    fn the_editor_shows_on_the_selected_copy() {
        let underlined = |a: &mut App| -> Vec<u16> {
            let buf = render(a, 120, 20, &Shot::default());
            (0..20)
                .filter(|&y| (0..120).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED)))
                .collect()
        };
        let mut a = agent_app();
        a.key(ctrl('g'));
        assert_eq!(a.sel_key.as_deref(), Some("!@0"));
        a.key(ctrl('r'));
        assert!(matches!(&a.mode, Mode::Rename { wid, .. } if wid == "@0"));
        // tabs, prompt, column header, NEEDS YOU rule, then the copy (y=4)
        assert_eq!(underlined(&mut a), [4]);
        a.key(plain(KeyCode::Esc));
        // ... its pane (y=5), the dev rule (y=6), then fix in its session
        a.select_key("@0/%0");
        a.key(ctrl('r'));
        assert_eq!(underlined(&mut a), [7]);
    }

    #[test]
    fn no_agents_no_group() {
        let s = screen(&mut app(), 200, 50);
        assert!(!s.contains("NEEDS YOU") && !s.contains('●'), "{s}");
        // a pane title that is the host name isn't shown
        assert!(!s.contains("host"), "{s}");
    }

    #[test]
    fn preview_layout_by_size() {
        let at = |w, h, flip| {
            let (l, p) = preview_rect(Rect::new(0, 4, w, h - 5), h, flip);
            (l.height, p.map(|r| (r.y, r.width, r.height)))
        };
        // >= 30 rows: the bottom half, whatever the width
        assert_eq!(at(200, 50, false), (23, Some((27, 200, 22))));
        assert_eq!(at(120, 40, false), (18, Some((22, 120, 17))));
        // small: hidden until ^o, then the bottom half too
        assert_eq!(at(120, 20, false), (15, None));
        assert_eq!(at(120, 20, true).1.unwrap().1, 120);
        // ^o hides a shown preview
        assert_eq!(at(200, 50, true).1, None);
    }

    /// The preview is always below the list, never beside it, and shows
    /// the window's layout in a frame.
    #[test]
    fn the_preview_is_always_at_the_bottom() {
        for (w, h) in [(200, 50), (120, 40), (300, 30)] {
            let s = screen(&mut app(), w, h);
            let lines: Vec<&str> = s.lines().collect();
            let rule = lines
                .iter()
                .position(|l| l.chars().count() == w as usize && l.chars().all(|c| c == '─'))
                .unwrap_or_else(|| panic!("{w}x{h}: no rule\n{s}"));
            assert!(rule > 3 && rule < h as usize - 3, "{s}");
            assert!(lines[rule + 1].starts_with('┌'), "the frame: {s}");
            let one = lines.iter().position(|l| l.contains("line one")).unwrap();
            assert!(one > rule, "{s}");
            assert!(lines[..rule].iter().all(|l| !l.contains("line")), "{s}");
        }
        let s = screen(&mut app(), 120, 20);
        assert!(!s.contains("line two"), "hidden when small:\n{s}");
    }

    /// A window split in two, side by side: `%0` (100 columns) and `%5`.
    fn split_snap() -> Snapshot {
        let mut s = snap();
        s.panes[0].width = 100;
        s.panes[0].height = 50;
        let mut right = s.panes[0].clone();
        right.id = "%5".into();
        right.index = 1;
        right.active = false;
        right.left = 101;
        right.width = 99;
        right.current_command = "vim".into();
        right.title = "notes".into();
        s.panes.insert(1, right);
        s
    }

    /// The layout: each pane's lines in its place, a separator between;
    /// on a pane row that pane is outlined in magenta, on a window row
    /// nothing is.
    #[test]
    fn the_preview_lays_out_the_window_and_outlines_the_pane() {
        let mut a = app_of(&split_snap());
        let mut shot = Shot::default();
        shot.panes.insert("%0".into(), preview::parse("LEFT-PANE"));
        shot.panes.insert("%5".into(), preview::parse("RIGHT-PANE"));
        let buf = render(&mut a, 120, 40, &shot);
        let s = text_of(&buf);
        let l = line(&s, "LEFT-PANE");
        assert!(l.contains("RIGHT-PANE"), "side by side: {s}");
        let (left, right) = (col(&l, "LEFT"), col(&l, "RIGHT"));
        assert_eq!(left, 1);
        assert!((58..=62).contains(&right), "scaled: {l:?}");
        assert_eq!(l.chars().nth(right - 1), Some('│'), "{l:?}");
        let magenta = |buf: &Buffer| {
            let a = buf.area;
            (0..a.height)
                .flat_map(|y| (0..a.width).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    buf[(x, y)].fg == Color::Magenta && !["▌", " "].contains(&buf[(x, y)].symbol())
                })
                .collect::<Vec<_>>()
        };
        assert!(
            magenta(&buf).is_empty(),
            "a window row: no outline: {:?}",
            magenta(&buf)
        );
        // the right pane's row: its outline, and only its
        a.select_key("@0/%5");
        let buf = render(&mut a, 120, 40, &shot);
        let hl = magenta(&buf);
        let y = text_of(&buf)
            .lines()
            .position(|l| l.contains("RIGHT-PANE"))
            .unwrap() as u16;
        assert!(!hl.is_empty());
        assert!(hl.iter().all(|&(x, _)| x as usize >= right - 1), "{hl:?}");
        assert!(hl.contains(&(right as u16 - 1, y)), "the shared separator");
        assert!(hl.contains(&(119, y)), "the frame's right edge");
        // the pane rows: title shown, in index order under their window
        let s = text_of(&buf);
        assert!(line(&s, "├ .0").contains("sh"), "{s}");
        assert!(line(&s, "└ .1").contains("notes"), "{s}");
    }

    /// The preview in colour: SGR colours and attributes reach the cells.
    #[test]
    fn the_preview_renders_colours() {
        let mut a = app();
        let shot = shot_of(
            &a,
            "\x1b[31mred\x1b[0m \x1b[1;44mbold on blue\x1b[0m \x1b[7mrev\x1b[0m",
        );
        let buf = render(&mut a, 200, 50, &shot);
        let s = text_of(&buf);
        let y = s
            .lines()
            .position(|l| l.contains("red bold on blue rev"))
            .unwrap() as u16;
        let x = col(&line(&s, "red bold"), "red") as u16;
        assert_eq!(buf[(x, y)].fg, Color::Red);
        assert_eq!(buf[(x + 4, y)].bg, Color::Blue);
        assert!(buf[(x + 4, y)].modifier.contains(Modifier::BOLD));
        assert!(buf[(x + 17, y)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(x + 3, y)].bg, Color::Reset, "reset between spans");
    }

    /// Long lines are clipped at the pane's edge, not wrapped; wide
    /// characters and tabs keep the columns.
    #[test]
    fn preview_lines_are_clipped_not_wrapped() {
        let mut a = app();
        let long = format!("宽\tx{}END", "-".repeat(300));
        let shot = shot_of(&a, &format!("{long}\nlast"));
        let buf = render(&mut a, 200, 50, &shot);
        let s = text_of(&buf);
        let lines: Vec<&str> = s.lines().collect();
        let y = lines.iter().position(|l| l.contains('宽')).unwrap();
        assert!(
            lines[y + 1].starts_with("│last "),
            "not wrapped: {:?}",
            lines[y + 1]
        );
        assert_eq!(buf[(9, y as u16)].symbol(), "x", "the tab reaches column 8");
        assert!(!lines[y].contains("END"));
        assert_eq!(buf[(198, y as u16)].symbol(), "-", "clipped at the edge");
        assert_eq!(buf[(199, y as u16)].symbol(), "│", "the frame");
    }

    /// The wheel scrolls the selected pane, wherever the pointer is, and
    /// never the list; on a window row it does nothing. Moving the
    /// selection brings the preview back to the end.
    #[test]
    fn the_wheel_scrolls_only_the_selected_pane() {
        use ratatui::crossterm::event::MouseButton;
        let mut a = app_of(&split_snap());
        let mut shot = Shot::default();
        for (p, tag) in [("%0", "a"), ("%5", "b")] {
            let text: Vec<String> = (1..=100).map(|i| format!("{tag} {i}")).collect();
            shot.panes
                .insert(p.into(), preview::parse(&text.join("\n")));
        }
        let rows = |a: &mut App| text_of(&render(a, 200, 50, &shot));
        // a window row: nothing scrolls
        assert_eq!(a.mouse(wheel(K::ScrollUp, 150, 40)), Action::Redraw);
        a.key(shift(KeyCode::Up));
        assert_eq!(a.preview_scroll, 0);
        let s = rows(&mut a);
        assert_eq!(a.preview_rows, 0);
        assert!(s.contains("a 100") && s.contains("b 100"), "{s}");
        // the left pane's row
        a.key(plain(KeyCode::Down));
        assert_eq!(a.sel_key.as_deref(), Some("@0/%0"));
        let s = rows(&mut a);
        let n = a.preview_rows;
        assert!(n > 5, "{n}");
        let sel = a.sel;
        // over the list (x=10) and over the preview (x=150) alike
        for x in [10, 150] {
            assert_eq!(a.mouse(wheel(K::ScrollUp, x, 5)), Action::Redraw);
        }
        assert_eq!((a.sel, a.preview_scroll), (sel, 6));
        let s2 = rows(&mut a);
        assert!(s.contains("a 100") && !s2.contains("a 100"), "{s2}");
        assert!(
            s2.contains("a 94") && s2.contains("b 100"),
            "only the pane: {s2}"
        );
        assert!(
            line(&s2, " ↑6 ").starts_with("│a "),
            "the offset shows: {s2}"
        );
        a.mouse(wheel(K::ScrollDown, 150, 5));
        assert_eq!((a.sel, a.preview_scroll), (sel, 3));
        // past the top: clamped at the next draw
        for _ in 0..100 {
            a.mouse(wheel(K::ScrollUp, 150, 5));
        }
        rows(&mut a);
        assert_eq!(a.preview_scroll, 100 - n);
        // keys: S-↑/S-↓ a line, S-PgUp/S-PgDn half a page, list unmoved
        a.preview_scroll = 0;
        a.key(shift(KeyCode::Up));
        a.key(shift(KeyCode::Up));
        a.key(shift(KeyCode::Down));
        assert_eq!((a.sel, a.preview_scroll), (sel, 1));
        a.key(shift(KeyCode::PageUp));
        assert_eq!((a.sel, a.preview_scroll), (sel, 1 + n / 2));
        a.key(shift(KeyCode::PageDown));
        assert_eq!((a.sel, a.preview_scroll), (sel, 1));
        // a selection change resets the scroll; so does coming back
        a.key(plain(KeyCode::Down));
        assert_eq!(a.preview_scroll, 0);
        a.key(plain(KeyCode::Up));
        assert_eq!((a.sel, a.preview_scroll), (sel, 0));
        // a snapshot that keeps the selection keeps the scroll
        rows(&mut a);
        a.mouse(wheel(K::ScrollUp, 150, 5));
        let r = a.rows.clone();
        a.set_rows(r);
        assert_eq!(a.preview_scroll, 3);
        // a left click on a row selects it; on the preview, nothing
        rows(&mut a);
        assert_eq!(
            a.mouse(wheel(K::Down(MouseButton::Left), 150, 3)),
            Action::Redraw,
            "x=150 is still the list (it is full width)"
        );
        assert_eq!(a.sel, 0, "the click hit the first row (y=3)");
        a.mouse(wheel(K::Down(MouseButton::Left), 10, 6));
        assert_eq!(
            a.selected().unwrap().name,
            "a much longer window name than fits"
        );
        assert_eq!(
            a.mouse(wheel(K::Down(MouseButton::Left), 10, 45)),
            Action::None,
            "the preview"
        );
        // the rest of the mouse does nothing
        for k in [
            K::Down(MouseButton::Right),
            K::Moved,
            K::Up(MouseButton::Left),
        ] {
            assert_eq!(a.mouse(wheel(k, 10, 2)), Action::None);
        }
    }

    fn wheel(kind: K, column: u16, row: u16) -> ratatui::crossterm::event::MouseEvent {
        ratatui::crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// With the preview hidden the wheel does nothing.
    #[test]
    fn no_preview_no_scroll() {
        let mut a = app();
        a.key(plain(KeyCode::Down)); // a pane row
        screen(&mut a, 120, 20); // small: hidden
        assert_eq!(a.preview_rows, 0);
        a.mouse(wheel(K::ScrollUp, 10, 5));
        a.key(shift(KeyCode::Up));
        assert_eq!(a.preview_scroll, 0);
        a.key(ctrl('o'));
        screen(&mut a, 120, 20);
        assert!(a.preview_rows > 0);
    }

    /// The editors ignore the preview keys, and Shift-arrows in the list
    /// don't reach the filter.
    #[test]
    fn preview_keys_leave_the_editors_alone() {
        let mut a = app();
        a.key(plain(KeyCode::Down));
        screen(&mut a, 200, 50);
        a.key(ctrl('r'));
        for c in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            a.key(shift(c));
        }
        assert!(matches!(&a.mode, Mode::Rename { edit, .. } if edit.as_string() == "editor"));
        assert_eq!(a.preview_scroll, 0);
        a.key(plain(KeyCode::Esc));
        a.key(shift(KeyCode::Up));
        assert!(a.filter.is_empty());
        assert_eq!(a.preview_scroll, 1);
    }

    /// PgDn moves by at least one row whatever the list height.
    #[test]
    fn page_keys_move_at_least_one_row() {
        let mut a = app();
        screen(&mut a, 40, 5);
        a.key(plain(KeyCode::PageDown));
        assert_eq!(a.sel, 1);
        a.key(plain(KeyCode::PageUp));
        assert_eq!(a.sel, 0);
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        for (w, h) in [
            (20, 3),
            (10, 1),
            (1, 1),
            (3, 2),
            (200, 1),
            (5, 40),
            (200, 6),
        ] {
            for keys in [
                vec![],
                vec![ctrl('r')],
                vec![alt('n')],
                vec![ctrl('o')],
                vec![plain(KeyCode::Down), ctrl('o')],
                vec![plain(KeyCode::F(1))],
                vec![plain(KeyCode::Char('z')), plain(KeyCode::Char('z'))],
                vec![plain(KeyCode::PageDown), plain(KeyCode::PageUp)],
            ] {
                for snap in [snap(), split_snap(), git_snap()] {
                    let mut a = app_of(&snap);
                    a.notice = Some(" a long notice that does not fit anywhere".into());
                    for k in keys.clone() {
                        a.key(k);
                    }
                    screen(&mut a, w, h);
                    assert!(a.page >= 1);
                }
            }
            let mut a = app();
            a.close_plan(
                "@0",
                crate::ops::ClosePlan::Ask("close \"editor\"? (y/N) ".into()),
            );
            screen(&mut a, w, h);
        }
    }
}
