//! Drawing the sidebar: a narrow column (24–40 cells) in the popup's
//! compact look — status icon (never colour alone), name, and the git
//! badge clipped by whole pieces.

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

/// A window of the sidebar's session: `▌● 2 name   main +!`.
fn window_line(e: &Entry, width: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            if e.here { "▌" } else { " " },
            Style::default().fg(Color::Magenta),
        ),
        icon(e),
        Span::raw(format!(" {} ", e.index)),
    ];
    let used: usize = spans.iter().map(Span::width).sum();
    let room = width.saturating_sub(used);
    let badge = e.git.as_ref().map(git::badge_spans).unwrap_or_default();
    let bw: usize = badge.iter().map(Span::width).sum();
    let nw = e.name.chars().count();
    // the name keeps at least half the room; the badge drops whole pieces
    let keep = (room / 2).max(8);
    let name = clip(&e.name, nw.min(room.saturating_sub(bw).max(keep)).min(room));
    let name_w = Span::raw(name.as_str()).width();
    let name_style = if e.stale { dim() } else { Style::default() };
    spans.push(Span::styled(name, name_style));
    spans.extend(git::fit_badge(badge, room.saturating_sub(name_w)));
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
            for e in &m.windows {
                if e.here {
                    here = Some(lines.len());
                }
                let mut l = [window_line(e, width)];
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
            assert!(lines[8].starts_with(" ● 3 ed"), "{w}:\n{s}");
            assert_eq!(
                lines[13],
                clip(" 2s 4w · 1 error · 1 waiting · 1 running", w as usize),
                "{w}:\n{s}"
            );
        }
    }

    #[test]
    fn the_badge_gives_way_to_the_name_by_whole_pieces() {
        let m = model();
        let line = |w| screen(&m, w, 14, true).lines().nth(7).unwrap().to_string();
        // 24: "▌  2 " leaves 19; the name keeps half and the badge what fits
        assert_eq!(line(24), "▌  2 a-rather…  main !");
        assert_eq!(line(32), "▌  2 a-rather-long-w…  main ! ⇡1");
        assert_eq!(line(40), "▌  2 a-rather-long-window-na…  main ! ⇡1");
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
