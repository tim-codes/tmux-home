//! Drawing the sidebar: a narrow column (24–40 cells) in the popup's
//! compact look. The session's windows are a two-column table: a
//! fixed-width first column (status icon, never colour alone; index;
//! name) and the tags (the agent's state word, the git badge's pieces),
//! wrapped by whole pieces onto lines aligned under the second column.

use super::model::{Entry, Model, Sel};
use crate::agent::Status;
use crate::popup::{agents::clip, git};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn status_style(s: Status) -> Style {
    let st = Style::default();
    match s {
        Status::Running => st.fg(Color::Green),
        Status::Waiting => st.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Status::Error => st.fg(Color::Red).add_modifier(Modifier::BOLD),
        Status::Background => st.fg(Color::Blue),
        Status::Idle => st,
        Status::Unknown => dim(),
    }
}

/// The status icon cell: the icon (dimmed when the agent has ended), or a
/// blank for a window without an agent.
fn icon(e: &Entry) -> Span<'static> {
    match e.status {
        Some(s) if e.stale => Span::styled(s.icon(), dim()),
        Some(s) => Span::styled(s.icon(), status_style(s)),
        None => Span::raw(" "),
    }
}

/// A rule with a label: `─ main ────`.
fn rule(label: &str, width: usize, style: Style) -> Line<'static> {
    let label = clip(label, width.saturating_sub(4));
    let used = label.chars().count() + 3;
    Line::from(vec![
        Span::styled("─ ", dim()),
        Span::styled(label, style),
        Span::styled(
            format!(" {}", "─".repeat(width.saturating_sub(used))),
            dim(),
        ),
    ])
}

/// The session list's first column, the same for every row: the here
/// marker, the status icon, the index (right-aligned) and the name,
/// padded to the longest name (at most half the sidebar, and room for 8
/// columns of tags, but never capped below 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cols {
    pub index: usize,
    pub name: usize,
}

impl Cols {
    pub fn of(windows: &[Entry], width: usize) -> Cols {
        let index = windows
            .iter()
            .map(|e| e.index.to_string().len())
            .max()
            .unwrap_or(1);
        let longest = windows
            .iter()
            .map(|e| Span::raw(e.name.as_str()).width())
            .max()
            .unwrap_or(0);
        // at most half the sidebar, and leave the tags 8 columns
        let cap = (width / 2).min(width.saturating_sub(index + 13)).max(8);
        // never wider than the line leaves after the marker, icon and index
        let room = width.saturating_sub(index + 4);
        Cols {
            index,
            name: longest.min(cap).min(room),
        }
    }

    /// Where the second column starts.
    pub fn tags_at(&self) -> usize {
        self.index + 4 + self.name + 1
    }
}

/// A row's tags, the second column's pieces: the agent's state word, then
/// the git badge's pieces. Missing ones are simply absent.
fn tags(e: &Entry) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    if let Some(st) = e.status {
        let style = if e.stale { dim() } else { status_style(st) };
        out.push(Span::styled(st.word(), style));
    }
    for sp in e.git.as_ref().map(git::badge_spans).unwrap_or_default() {
        out.push(Span::styled(sp.content.trim_start().to_string(), sp.style));
    }
    out
}

/// Lays `pieces` out in `room` columns: as many whole pieces per line as
/// fit, one space apart, the rest on further lines; a piece wider than
/// `room` gets a line of its own, clipped.
fn wrap(pieces: Vec<Span<'static>>, room: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut used = 0;
    for p in pieces {
        let w = p.width();
        match lines.last_mut() {
            Some(l) if used + 1 + w <= room => {
                l.push(Span::raw(" "));
                l.push(p);
                used += 1 + w;
            }
            _ if room == 0 => break,
            _ => {
                let p = if w > room {
                    Span::styled(clip(&p.content, room), p.style)
                } else {
                    p
                };
                used = p.width();
                lines.push(vec![p]);
            }
        }
    }
    lines
}

/// A window of the sidebar's session, as a table row:
/// `▌● 2 name      running main` with the tags that don't fit on further
/// lines, aligned under the second column.
fn window_lines(e: &Entry, c: Cols, width: usize) -> Vec<Line<'static>> {
    let name = clip(&e.name, c.name);
    let pad = c.name.saturating_sub(Span::raw(name.as_str()).width());
    let name_style = if e.stale { dim() } else { Style::default() };
    let head = vec![
        Span::styled(
            if e.here { "▌" } else { " " },
            Style::default().fg(Color::Magenta),
        ),
        icon(e),
        Span::raw(format!(" {:>w$} ", e.index, w = c.index)),
        Span::styled(name, name_style),
        Span::raw(" ".repeat(pad + 1)),
    ];
    let at = c.tags_at();
    let mut tag_lines = wrap(tags(e), width.saturating_sub(at)).into_iter();
    let mut first = head;
    first.extend(tag_lines.next().unwrap_or_default());
    let mut lines = vec![first];
    for t in tag_lines {
        // the marker's cell apart, as on the first line
        let mut l = vec![Span::raw(" "), Span::raw(" ".repeat(at - 1))];
        l.extend(t);
        lines.push(l);
    }
    lines
        .into_iter()
        .map(|mut spans| {
            if e.here {
                for s in spans.iter_mut().skip(1) {
                    s.style = s.style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
                }
                let w: usize = spans.iter().map(Span::width).sum();
                spans.push(Span::styled(
                    " ".repeat(width.saturating_sub(w)),
                    Style::default().bg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// A window that needs you: `◐ main:1 api`, then its reason, indented.
fn needs_lines(e: &Entry, width: usize) -> [Line<'static>; 2] {
    let st = e.status.map(status_style).unwrap_or_default();
    let head = clip(
        &format!("{}:{} {}", e.session, e.index, e.name),
        width.saturating_sub(3),
    );
    let reason = e.reason.clone().unwrap_or_default();
    [
        Line::from(vec![
            Span::raw(" "),
            icon(e),
            Span::styled(format!(" {head}"), st),
        ]),
        Line::styled(
            format!("   {}", clip(&reason, width.saturating_sub(3))),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        ),
    ]
}

/// Shows `lines` as the selected row: reversed, to the edge.
fn select(lines: &mut [Line<'static>], width: usize) {
    for l in lines {
        let w = l.width();
        if w < width {
            l.spans.push(Span::raw(" ".repeat(width - w)));
        }
        for s in &mut l.spans {
            s.style = s.style.add_modifier(Modifier::REVERSED);
        }
    }
}

/// The body lines (everything above the tally) and the line to keep in
/// view: the selected row's, else the sidebar's own window line, if listed.
pub fn body(m: &Model, width: usize, sel: Option<&Sel>) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines = Vec::new();
    let mut picked = None;
    let is = |needs: bool, e: &Entry| sel.is_some_and(|s| s.needs == needs && s.wid == e.wid);
    if !m.needs.is_empty() {
        lines.push(rule(
            "NEEDS YOU",
            width,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        for e in &m.needs {
            let mut l = needs_lines(e, width);
            if is(true, e) {
                select(&mut l, width);
                picked = Some(lines.len());
            }
            lines.extend(l);
        }
    }
    let mut here = None;
    match &m.session {
        Some(s) => {
            lines.push(rule(
                s,
                width,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            let cols = Cols::of(&m.windows, width);
            for e in &m.windows {
                if e.here {
                    here = Some(lines.len());
                }
                let mut l = window_lines(e, cols, width);
                if is(false, e) {
                    select(&mut l, width);
                    picked = Some(lines.len());
                }
                lines.extend(l);
            }
        }
        None => lines.push(Line::styled(" …", dim())),
    }
    (lines, picked.or(here))
}

/// Draws the sidebar with `sel` highlighted; `live` is false while it
/// reads tmux directly (no daemon), shown as `○` before the tally.
pub fn draw(f: &mut Frame, m: &Model, live: bool, sel: Option<&Sel>) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let h = area.height.saturating_sub(1) as usize;
    let (lines, here) = body(m, width, sel);
    // keep the selection, else the sidebar's own window, in view in a
    // short pane
    let top = here.map_or(0, |i| i.saturating_sub(h.saturating_sub(1)));
    let shown: Vec<Line> = lines.into_iter().skip(top).take(h).collect();
    f.render_widget(
        Paragraph::new(shown),
        Rect {
            height: h as u16,
            ..area
        },
    );
    let chip = if live { "" } else { "○ " };
    f.render_widget(
        Paragraph::new(Span::styled(
            clip(&format!(" {chip}{}", m.tally_line()), width),
            dim(),
        )),
        Rect {
            y: area.bottom() - 1,
            height: 1,
            ..area
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::model::tests::snap;
    use ratatui::{Terminal, backend::TestBackend};

    fn screen(m: &Model, w: u16, h: u16, live: bool) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, m, live, None)).unwrap();
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

    fn model() -> Model {
        let mut s = snap();
        s.windows[1].name = "a-rather-long-window-name".into();
        Model::build(&s, "%9", "/home")
    }

    #[test]
    fn renders_at_24_32_and_40_columns() {
        let m = model();
        for w in [24u16, 32, 40] {
            let s = screen(&m, w, 14, true);
            let lines: Vec<&str> = s.lines().collect();
            for l in &lines {
                assert!(l.chars().count() <= w as usize, "{w}: {l:?}");
            }
            assert!(lines[0].starts_with("─ NEEDS YOU ─"), "{w}:\n{s}");
            assert_eq!(lines[1], " ◐ main:1 api", "{w}:\n{s}");
            assert_eq!(lines[2], "   permission", "{w}:\n{s}");
            assert!(lines[3].starts_with(" ✕ ops:0 logs"), "{w}:\n{s}");
            assert!(lines[5].starts_with("─ main ─"), "{w}:\n{s}");
            assert!(lines[6].starts_with(" ◐ 1 api"), "{w}:\n{s}");
            assert!(lines[7].starts_with("▌  2 a-rather"), "{w}:\n{s}");
            // at 24 the here row's last tag wraps onto a line of its own
            let ed = if w == 24 { 9 } else { 8 };
            assert!(lines[ed].starts_with(" ● 3 ed"), "{w}:\n{s}");
            assert_eq!(
                lines[13],
                clip(" 2s 4w · 1 error · 1 waiting · 1 running", w as usize),
                "{w}:\n{s}"
            );
        }
    }

    #[test]
    fn rows_are_a_table_name_column_then_tags() {
        let m = model();
        let rows = |w| {
            screen(&m, w, 14, true)
                .lines()
                .skip(6)
                .take(if w == 24 { 4 } else { 3 })
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        // the name column is as wide as the longest name allows (half the
        // sidebar, leaving 8 for tags); tags start in one column
        assert_eq!(
            rows(24),
            [
                " ◐ 1 api        waiting",
                "▌  2 a-rather-… main !",
                "                ⇡1",
                " ● 3 ed         running",
            ]
        );
        assert_eq!(
            rows(32),
            [
                " ◐ 1 api              waiting",
                "▌  2 a-rather-long-w… main ! ⇡1",
                " ● 3 ed               running",
            ]
        );
        assert_eq!(
            rows(40),
            [
                " ◐ 1 api                  waiting",
                "▌  2 a-rather-long-windo… main ! ⇡1",
                " ● 3 ed                   running",
            ]
        );
    }

    #[test]
    fn the_name_column_fits_the_longest_name_and_right_aligns_indexes() {
        let mut s = snap();
        s.windows[0].index = 10;
        s.windows[2].name = "edit".into();
        s.panes.retain(|p| p.id != "%3"); // ed: no agent, no repo
        let m = Model::build(&s, "%9", "/home");
        assert_eq!(Cols::of(&m.windows, 32), Cols { index: 2, name: 4 });
        assert_eq!(Cols::of(&m.windows, 32).tags_at(), 11);
        let out = screen(&m, 32, 14, true);
        let rows: Vec<&str> = out.lines().skip(6).take(3).collect();
        assert_eq!(
            rows,
            ["▌   2 web  main ! ⇡1", "    3 edit", " ◐ 10 api  waiting"],
            "missing tags collapse: nothing after the name\n{out}"
        );
        // never capped below 8, never past the line
        let long = |n: usize| {
            let mut e = m.windows[0].clone();
            e.name = "x".repeat(n);
            vec![e]
        };
        assert_eq!(Cols::of(&long(30), 24).name, 10, "8 left for tags");
        assert_eq!(Cols::of(&long(30), 40).name, 20, "half");
        assert_eq!(Cols::of(&long(30), 20).name, 8, "never below 8");
        assert_eq!(Cols::of(&long(30), 10).name, 5, "but never past the line");
        assert_eq!(Cols::of(&[], 32), Cols { index: 1, name: 0 });
    }

    #[test]
    fn tags_wrap_by_whole_pieces() {
        let p = |t: &str| Span::raw(t.to_string());
        let text = |ls: Vec<Vec<Span<'static>>>| {
            ls.iter()
                .map(|l| l.iter().map(|s| s.content.to_string()).collect::<String>())
                .collect::<Vec<_>>()
        };
        let pieces = || vec![p("running"), p("main"), p("+!"), p("⇡12"), p("$3")];
        assert_eq!(text(wrap(pieces(), 30)), ["running main +! ⇡12 $3"]);
        assert_eq!(text(wrap(pieces(), 12)), ["running main", "+! ⇡12 $3"]);
        assert_eq!(text(wrap(pieces(), 7)), ["running", "main +!", "⇡12 $3"]);
        assert_eq!(text(wrap(pieces(), 4)), ["run…", "main", "+!", "⇡12", "$3"]);
        assert!(wrap(pieces(), 0).is_empty());
        assert!(wrap(vec![], 10).is_empty());
    }

    #[test]
    fn here_is_highlighted_and_degraded_shows_the_chip() {
        let m = model();
        let mut t = Terminal::new(TestBackend::new(32, 14)).unwrap();
        t.draw(|f| draw(f, &m, false, None)).unwrap();
        let buf = t.backend().buffer().clone();
        assert_eq!(buf[(3, 7)].bg, Color::DarkGray, "the sidebar's window");
        assert_eq!(buf[(31, 7)].bg, Color::DarkGray, "to the edge");
        assert_ne!(buf[(3, 8)].bg, Color::DarkGray);
        // a wrapped here row is highlighted on every line
        let mut t = Terminal::new(TestBackend::new(24, 14)).unwrap();
        t.draw(|f| draw(f, &m, true, None)).unwrap();
        let buf = t.backend().buffer().clone();
        assert_eq!(buf[(16, 8)].symbol(), "⇡");
        assert_eq!(buf[(1, 8)].bg, Color::DarkGray, "its continuation line");
        assert_eq!(buf[(23, 8)].bg, Color::DarkGray);
        let s = screen(&m, 32, 14, false);
        assert!(s.lines().nth(13).unwrap().starts_with(" ○ 2s 4w"), "{s}");
    }

    #[test]
    fn a_short_pane_keeps_its_own_window_in_view() {
        let m = model();
        let s = screen(&m, 32, 4, true);
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines[2].starts_with("▌  2 a-rather"), "{s}");
        assert!(lines[3].starts_with(" 2s 4w"), "{s}");
    }

    #[test]
    fn the_selection_is_reversed_and_kept_in_view() {
        let m = model();
        let sel = Sel {
            needs: false,
            wid: "@3".into(),
        };
        let mut t = Terminal::new(TestBackend::new(32, 14)).unwrap();
        t.draw(|f| draw(f, &m, true, Some(&sel))).unwrap();
        let buf = t.backend().buffer().clone();
        let rev = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!(rev(0, 8) && rev(31, 8), "the ed row, to the edge");
        assert!(!rev(0, 7) && !rev(0, 6));
        let needs = Sel {
            needs: true,
            wid: "@4".into(),
        };
        t.draw(|f| draw(f, &m, true, Some(&needs))).unwrap();
        let buf = t.backend().buffer().clone();
        let rev = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!(rev(1, 3) && rev(1, 4), "both lines of ops:0 logs");
        assert!(!rev(1, 1) && !rev(1, 8));
        // a 4-row pane scrolls to the selection, not to its own window
        let mut t = Terminal::new(TestBackend::new(32, 4)).unwrap();
        t.draw(|f| draw(f, &m, true, Some(&sel))).unwrap();
        let top: String = (0..10)
            .map(|x| t.backend().buffer()[(x, 2)].symbol().to_string())
            .collect();
        assert!(top.starts_with(" ● 3 ed"), "{top:?}");
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        let m = model();
        for (w, h) in [(1, 1), (2, 3), (5, 1), (24, 2), (0, 0)] {
            if w > 0 && h > 0 {
                screen(&m, w, h, true);
            }
        }
    }
}
