//! Drawing the sidebar: a narrow column (24–40 cells) in the popup's
//! compact look. Each window of the session is a stacked row: line 1 is
//! the marker, status icon (never colour alone), index and the name on
//! the full remaining width; the tags (the agent's state word, the git
//! badge's pieces) follow on lines indented to the name, wrapped by whole
//! pieces. The same at every width.

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

/// The width of the index field, the same for every row of the section
/// so the names line up.
pub fn index_width(windows: &[Entry]) -> usize {
    windows
        .iter()
        .map(|e| e.index.to_string().len())
        .max()
        .unwrap_or(1)
}

/// Where a row's name starts, and so where its tags are indented to:
/// after the marker, icon, space, index and space.
fn name_at(index: usize) -> usize {
    index + 4
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

/// A window of the sidebar's session: `▌● 2 name` with the name on the
/// full remaining width (clipped with `…` only if it still doesn't fit),
/// then its tags on further lines indented to the name's first column,
/// wrapped by whole pieces. A row without tags is one line. The here row
/// is highlighted to the edge on every line.
fn window_lines(e: &Entry, index: usize, width: usize) -> Vec<Line<'static>> {
    let at = name_at(index);
    let room = width.saturating_sub(at);
    let name = if room == 0 {
        String::new()
    } else {
        clip(&e.name, room)
    };
    let name_style = if e.stale { dim() } else { Style::default() };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            if e.here { "▌" } else { " " },
            Style::default().fg(Color::Magenta),
        ),
        icon(e),
        Span::raw(format!(" {:>w$} ", e.index, w = index)),
        Span::styled(name, name_style),
    ])];
    for t in wrap(tags(e), room) {
        let mut l = vec![Span::raw(" ".repeat(at))];
        l.extend(t);
        lines.push(Line::from(l));
    }
    if e.here {
        mark_here(&mut lines, width);
    }
    lines
}

/// Pads `lines` with blanks to `width`.
fn pad(lines: &mut [Line<'static>], width: usize) {
    for l in lines {
        let w = l.width();
        if w < width {
            l.spans.push(Span::raw(" ".repeat(width - w)));
        }
    }
}

/// The here row's highlight: one background across every cell of every
/// line, gaps included. Spans keep their foreground colours but give up
/// any background (or reversal) of their own.
fn mark_here(lines: &mut [Line<'static>], width: usize) {
    pad(lines, width);
    for s in lines.iter_mut().flat_map(|l| l.spans.iter_mut()) {
        s.style = s
            .style
            .remove_modifier(Modifier::REVERSED)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
    }
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

/// Shows `lines` as the selected row: every colour dropped and the whole
/// band reversed, bold, to the edge on every line, so it reads as one
/// uniform block whatever the spans' own colours were.
fn select(lines: &mut [Line<'static>], width: usize) {
    pad(lines, width);
    for s in lines.iter_mut().flat_map(|l| l.spans.iter_mut()) {
        s.style.fg = None;
        s.style.bg = None;
        s.style = s
            .style
            .remove_modifier(Modifier::DIM)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD);
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
    // No session header: tmux's own window status chips already name it.
    // A blank line keeps the windows apart from NEEDS YOU above them.
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    match &m.session {
        Some(_) => {
            let index = index_width(&m.windows);
            for e in &m.windows {
                if e.here {
                    here = Some(lines.len());
                }
                let mut l = window_lines(e, index, width);
                if is(false, e) {
                    select(&mut l, width);
                    picked = Some(lines.len());
                }
                lines.extend(l);
            }
        }
        // Not meaningless: this is the only "no session yet" signal.
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

    /// The screen lines from `from` on, as text.
    fn rows(m: &Model, w: u16, from: usize, n: usize) -> Vec<String> {
        screen(m, w, 16, true)
            .lines()
            .skip(from)
            .take(n)
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn renders_at_24_32_and_40_columns() {
        let m = model();
        for w in [24u16, 32, 40] {
            let s = screen(&m, w, 16, true);
            let lines: Vec<&str> = s.lines().collect();
            for l in &lines {
                assert!(l.chars().count() <= w as usize, "{w}: {l:?}");
            }
            assert!(lines[0].starts_with("─ NEEDS YOU ─"), "{w}:\n{s}");
            assert_eq!(lines[1], " ◐ main:1 api", "{w}:\n{s}");
            assert_eq!(lines[2], "   permission", "{w}:\n{s}");
            assert_eq!(lines[3], " ✕ ops:0 logs", "{w}:\n{s}");
            assert_eq!(lines[4], "   error: rate limited", "{w}:\n{s}");
            assert_eq!(lines[5], "", "{w}:\n{s}");
            assert!(lines[6].starts_with(" ◐ 1 api"), "{w}:\n{s}");
            assert_eq!(
                lines[15],
                clip(" 2s 4w · 1 error · 1 waiting · 1 running", w as usize)
            );
        }
    }

    #[test]
    fn rows_stack_the_name_then_the_tags_below_at_every_width() {
        let m = model();
        // the name takes the full remaining width; tags sit under its
        // first column on a line of their own
        assert_eq!(
            rows(&m, 24, 6, 6),
            [
                " ◐ 1 api",
                "     waiting",
                "▌  2 a-rather-long-wind…",
                "     main ! ⇡1",
                " ● 3 ed",
                "     running",
            ]
        );
        assert_eq!(
            rows(&m, 32, 6, 6),
            [
                " ◐ 1 api",
                "     waiting",
                "▌  2 a-rather-long-window-name",
                "     main ! ⇡1",
                " ● 3 ed",
                "     running",
            ]
        );
        assert_eq!(rows(&m, 40, 6, 6), rows(&m, 32, 6, 6));
    }

    #[test]
    fn a_row_without_tags_is_one_line_and_indexes_right_align() {
        let mut s = snap();
        s.windows[0].index = 10;
        s.windows[2].name = "edit".into();
        s.panes.retain(|p| p.id != "%3"); // ed: no agent, no repo
        let m = Model::build(&s, "%9", "/home");
        assert_eq!(index_width(&m.windows), 2);
        assert_eq!(
            rows(&m, 32, 6, 5),
            [
                "▌   2 web",
                "      main ! ⇡1",
                "    3 edit",
                " ◐ 10 api",
                "      waiting",
            ]
        );
    }

    #[test]
    fn tags_wrap_onto_further_lines_with_the_same_indent() {
        let m = model();
        // 14 columns: the here row's badge (`main ! ⇡1`, 9) can't share a
        // line with nothing else, but `main !` + `⇡1` still wrap by piece
        let r = rows(&m, 12, 0, 16);
        let i = r.iter().position(|l| l.starts_with("▌  2")).unwrap();
        assert_eq!(r[i + 1], "     main !", "{r:#?}");
        assert_eq!(r[i + 2], "     ⇡1", "{r:#?}");
        assert!(r[i + 3].starts_with(" ● 3"));
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
    fn a_tag_longer_than_the_line_is_clipped() {
        let m = model();
        // 10 columns: 5 left after the marker, icon and index; `running`
        // (7) is clipped
        let r = rows(&m, 10, 0, 16);
        let i = r.iter().position(|l| l.starts_with(" ● 3")).unwrap();
        assert_eq!(r[i + 1], "     runn…", "{r:#?}");
    }

    #[test]
    fn a_long_name_is_clipped_at_the_last_column_and_the_rest_stays_intact() {
        let mut s = snap();
        s.windows[1].name = "x".repeat(60);
        let m = Model::build(&s, "%9", "/home");
        let r = rows(&m, 24, 6, 6);
        // line 1: marker, icon, index, then the name to column 24 exactly
        assert_eq!(r[2], format!("▌  2 {}…", "x".repeat(18)));
        assert_eq!(r[2].chars().count(), 24);
        assert_eq!(r[3], "     main ! ⇡1", "tags still render below");
        // NEEDS YOU: the head `session:index name` clips the same way
        let mut s = snap();
        s.windows[3].name = "y".repeat(60);
        let m = Model::build(&s, "%9", "/home");
        let r = rows(&m, 24, 0, 5);
        assert_eq!(r[3], format!(" ✕ ops:0 {}…", "y".repeat(14)));
        assert_eq!(r[3].chars().count(), 24);
        assert_eq!(r[4], "   error: rate limited");
        // and a reason that doesn't fit
        s.windows[3].name = "logs".into();
        let m = Model::build(&s, "%9", "/home");
        assert_eq!(rows(&m, 16, 0, 5)[4], "   error: rate …");
    }

    #[test]
    fn here_is_highlighted_and_degraded_shows_the_chip() {
        let m = model();
        let mut t = Terminal::new(TestBackend::new(32, 16)).unwrap();
        t.draw(|f| draw(f, &m, false, None)).unwrap();
        let buf = t.backend().buffer().clone();
        assert_eq!(buf[(3, 8)].bg, Color::DarkGray, "the sidebar's window");
        assert_eq!(buf[(31, 8)].bg, Color::DarkGray, "to the edge");
        assert_ne!(buf[(3, 10)].bg, Color::DarkGray);
        let s = screen(&m, 32, 16, false);
        assert!(s.lines().nth(15).unwrap().starts_with(" ○ 2s 4w"), "{s}");
    }

    /// Every cell of lines `y0..y1` has the same background; returns it.
    fn uniform_bg(buf: &ratatui::buffer::Buffer, w: u16, y0: u16, y1: u16) -> Color {
        let bg = buf[(0, y0)].bg;
        for y in y0..y1 {
            for x in 0..w {
                assert_eq!(buf[(x, y)].bg, bg, "cell ({x},{y})");
            }
        }
        bg
    }

    #[test]
    fn the_here_highlight_is_one_band_over_every_line_of_the_row() {
        for w in [24u16, 32] {
            let mut t = Terminal::new(TestBackend::new(w, 16)).unwrap();
            t.draw(|f| draw(f, &model(), true, None)).unwrap();
            let buf = t.backend().buffer().clone();
            // the here row is lines 8 and 9 (name, tags)
            assert_eq!(uniform_bg(&buf, w, 8, 10), Color::DarkGray, "{w}");
            assert_ne!(buf[(0, 10)].bg, Color::DarkGray, "the next row is not");
            assert_ne!(buf[(0, 7)].bg, Color::DarkGray);
            for y in 8..10 {
                for x in 0..w {
                    assert!(!buf[(x, y)].modifier.contains(Modifier::REVERSED));
                }
            }
        }
    }

    #[test]
    fn spans_with_their_own_background_take_the_rows() {
        let mut lines = vec![
            Line::from(vec![
                Span::raw("ab"),
                Span::styled("cd", Style::default().bg(Color::Red).fg(Color::Green)),
            ]),
            Line::from(Span::styled(
                "e",
                Style::default().add_modifier(Modifier::REVERSED),
            )),
        ];
        mark_here(&mut lines, 8);
        for l in &lines {
            assert_eq!(l.width(), 8);
            for s in &l.spans {
                assert_eq!(s.style.bg, Some(Color::DarkGray));
                assert!(!s.style.add_modifier.contains(Modifier::REVERSED));
            }
        }
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Green), "fg kept");
    }

    #[test]
    fn the_selection_is_a_uniform_reversed_band_without_colours() {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("ab", Style::default().fg(Color::Green).bg(Color::Red)),
                Span::styled("c", Style::default().add_modifier(Modifier::DIM)),
            ]),
            Line::from("d"),
        ];
        select(&mut lines, 6);
        for l in &lines {
            assert_eq!(l.width(), 6);
            for s in &l.spans {
                assert_eq!((s.style.fg, s.style.bg), (None, None));
                assert!(s.style.add_modifier.contains(Modifier::REVERSED));
                assert!(s.style.add_modifier.contains(Modifier::BOLD));
                assert!(!s.style.add_modifier.contains(Modifier::DIM));
            }
        }
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
        let mut t = Terminal::new(TestBackend::new(32, 16)).unwrap();
        t.draw(|f| draw(f, &m, true, Some(&sel))).unwrap();
        let buf = t.backend().buffer().clone();
        let rev = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        for y in [10, 11] {
            assert!(rev(0, y) && rev(31, y), "both lines of ed, to the edge");
        }
        assert!(!rev(0, 9) && !rev(0, 12));
        let needs = Sel {
            needs: true,
            wid: "@4".into(),
        };
        t.draw(|f| draw(f, &m, true, Some(&needs))).unwrap();
        let buf = t.backend().buffer().clone();
        let rev = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!(rev(1, 3) && rev(1, 4), "both lines of ops:0 logs");
        assert!(!rev(1, 1) && !rev(1, 8) && !rev(1, 5));
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
