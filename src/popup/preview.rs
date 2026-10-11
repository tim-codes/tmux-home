//! The preview: the selected window's pane layout, each pane's
//! `capture-pane -e` capture parsed into styled lines (colours, bold, dim,
//! italic, underline, reverse kept) and placed in its rectangle scaled to
//! the preview, with tmux-style borders; and the window of a capture the
//! preview shows at a scroll offset.
//!
//! Lines are clipped at the preview's right edge, never wrapped: the capture
//! is already laid out at the pane's width (tmux wraps it; `-J` is not
//! used), so one captured line is one preview row. That keeps a scroll
//! offset a plain line count, a TUI's columns lined up, and the bottom of
//! the preview the bottom of the pane.

use ansi_to_tui::IntoText;
use ratatui::{
    layout::Rect,
    text::{Line, Span},
};
use std::collections::HashMap;

/// The captures the preview draws from, by pane ID: each pane's visible
/// part (a selected pane's with history above it).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shot {
    pub panes: HashMap<String, Vec<Line<'static>>>,
}

/// Where each pane goes in `area`: the panes' bounding box (in window
/// cells; a sidebar left out leaves no gap) scaled into `area` less a
/// one-cell frame. Neighbours share their one-cell separator, as in tmux,
/// so the rectangles tile the frame's inside exactly. Panes without a
/// geometry (all zero) are stacked top to bottom instead.
pub fn place(rects: &[(u32, u32, u32, u32)], area: Rect) -> Vec<Rect> {
    if rects.is_empty() || area.width < 3 || area.height < 3 {
        return vec![Rect::default(); rects.len()];
    }
    let stacked: Vec<(u32, u32, u32, u32)>;
    let rects = if rects.iter().all(|r| r.2 == 0 || r.3 == 0) {
        stacked = (0..rects.len() as u32)
            .map(|i| (0, i * 25, 80, 24))
            .collect();
        &stacked[..]
    } else {
        rects
    };
    let (iw, ih) = (area.width as u32 - 2, area.height as u32 - 2);
    let minx = rects.iter().map(|r| r.0).min().unwrap_or(0);
    let miny = rects.iter().map(|r| r.1).min().unwrap_or(0);
    let maxx = rects.iter().map(|r| r.0 + r.2).max().unwrap_or(1);
    let maxy = rects.iter().map(|r| r.1 + r.3).max().unwrap_or(1);
    let (bw, bh) = ((maxx - minx).max(1), (maxy - miny).max(1));
    // one axis: [start, end) in the inside, sharing separators
    let span = |lo: u32, len: u32, min: u32, max: u32, b: u32, inner: u32| {
        let s = |v: u32| v.saturating_sub(min) * inner / b;
        let a = if lo <= min { 0 } else { s(lo - 1) + 1 };
        let z = if lo + len >= max { inner } else { s(lo + len) };
        (a, z.max(a))
    };
    rects
        .iter()
        .map(|&(l, t, w, h)| {
            let (x0, x1) = span(l, w, minx, maxx, bw, iw);
            let (y0, y1) = span(t, h, miny, maxy, bh, ih);
            Rect::new(
                area.x + 1 + x0 as u16,
                area.y + 1 + y0 as u16,
                (x1 - x0) as u16,
                (y1 - y0) as u16,
            )
        })
        .collect()
}

/// The border cells of `area` around the placed `panes`: every cell no
/// pane covers, as a box-drawing character joined to its neighbours, and
/// whether it outlines pane `hl` (drawn highlighted).
pub fn borders(area: Rect, panes: &[Rect], hl: Option<usize>) -> Vec<(u16, u16, char, bool)> {
    let inside = |x: u16, y: u16| {
        panes
            .iter()
            .any(|r| x >= r.x && x < r.right() && y >= r.y && y < r.bottom())
    };
    let border = |x: i32, y: i32| {
        x >= area.x as i32
            && y >= area.y as i32
            && x < area.right() as i32
            && y < area.bottom() as i32
            && !inside(x as u16, y as u16)
    };
    let outline = |x: u16, y: u16| {
        hl.and_then(|i| panes.get(i)).is_some_and(|r| {
            let (x0, y0) = (r.x as i32 - 1, r.y as i32 - 1);
            let (x1, y1) = (r.right() as i32, r.bottom() as i32);
            let (x, y) = (x as i32, y as i32);
            (x0..=x1).contains(&x)
                && (y0..=y1).contains(&y)
                && (x == x0 || x == x1 || y == y0 || y == y1)
        })
    };
    let mut out = Vec::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let (xi, yi) = (x as i32, y as i32);
            if !border(xi, yi) {
                continue;
            }
            let (u, d) = (border(xi, yi - 1), border(xi, yi + 1));
            let (l, r) = (border(xi - 1, yi), border(xi + 1, yi));
            let c = match (u, d, l, r) {
                (true, true, true, true) => '┼',
                (true, true, false, true) => '├',
                (true, true, true, false) => '┤',
                (false, true, true, true) => '┬',
                (true, false, true, true) => '┴',
                (false, true, false, true) => '┌',
                (false, true, true, false) => '┐',
                (true, false, false, true) => '└',
                (true, false, true, false) => '┘',
                (true, _, false, false) | (_, true, false, false) => '│',
                (false, false, _, _) => '─',
            };
            out.push((x, y, c, outline(x, y)));
        }
    }
    out
}

/// Tab stops every 8 columns, as in a terminal.
const TAB: usize = 8;

/// Parses a capture into lines. Escape sequences other than SGR are
/// dropped (OSC 8 hyperlinks, which tmux 3.4+ emits with `-e`, among
/// them); tabs become spaces to the next stop and other control characters
/// go, so nothing in a line has a width the layout can't see. A capture the
/// parser rejects falls back to its plain text. Trailing blank lines are
/// trimmed (the empty rows below a shell's prompt).
pub fn parse(capture: &str) -> Vec<Line<'static>> {
    let clean = strip_osc(capture);
    // a parser panic is caught without a word (it would print over the TUI)
    let parsed = super::term::quietly(|| {
        if super::term::test_panic("parse") {
            panic!("TMUX_HOME_TEST_PANIC=parse");
        }
        clean.into_text()
    });
    let mut lines = match parsed {
        Some(Ok(t)) => t.lines,
        _ => strip_ansi(&clean)
            .lines()
            .map(|l| Line::raw(l.to_string()))
            .collect(),
    };
    for l in &mut lines {
        sanitize(l);
    }
    while lines
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        lines.pop();
    }
    lines
}

/// The rows the preview shows: `height` lines ending `offset` lines above
/// the last one. `offset` is clamped to the top; the clamped value is
/// returned with the rows.
pub fn window<'a>(
    lines: &'a [Line<'static>],
    height: usize,
    offset: usize,
) -> (usize, &'a [Line<'static>]) {
    let max = lines.len().saturating_sub(height);
    let offset = offset.min(max);
    let end = lines.len() - offset;
    (offset, &lines[end.saturating_sub(height)..end])
}

/// Removes OSC sequences (`ESC ] … BEL` or `ESC ] … ESC \`). ansi-to-tui
/// only knows the BEL ending; an ST-ended one would take the rest of its
/// line with it.
fn strip_osc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\x1b]") {
        out.push_str(&rest[..i]);
        let body = &rest[i + 2..];
        // an unterminated OSC ends at the end of its line
        let end = body
            .char_indices()
            .find_map(|(j, c)| match c {
                '\x07' => Some(j + 1),
                '\x1b' if body[j + 1..].starts_with('\\') => Some(j + 2),
                '\n' => Some(j),
                _ => None,
            })
            .unwrap_or(body.len());
        rest = &body[end..];
    }
    out.push_str(rest);
    out
}

/// Plain text: every escape sequence removed (the parse fallback).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if it.peek() == Some(&'[') {
            it.next();
            // CSI: parameters, then one final byte in @..~
            for c in it.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            it.next();
        }
    }
    out
}

/// Tabs to spaces (by display column), other control characters dropped.
fn sanitize(line: &mut Line<'static>) {
    if !line
        .spans
        .iter()
        .any(|s| s.content.chars().any(char::is_control))
    {
        return;
    }
    let mut col = 0;
    for span in &mut line.spans {
        let mut text = String::with_capacity(span.content.len());
        for c in span.content.chars() {
            if c == '\t' {
                let n = TAB - col % TAB;
                text.extend(std::iter::repeat_n(' ', n));
                col += n;
            } else if !c.is_control() {
                text.push(c);
                let mut b = [0; 4];
                col += Span::raw(&*c.encode_utf8(&mut b)).width();
            }
        }
        span.content = text.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    /// A 100x30 window split left/right at 59|40, the right half split
    /// top/bottom at 14|15, placed in a 52x17 area (inside: 50x15).
    #[test]
    fn panes_are_scaled_into_the_frame_sharing_separators() {
        let rects = [(0, 0, 59, 30), (60, 0, 40, 14), (60, 15, 40, 15)];
        let area = Rect::new(10, 5, 52, 17);
        let got = place(&rects, area);
        assert_eq!(got[0], Rect::new(11, 6, 29, 15));
        // the separator column is x=40: the right panes start after it
        assert_eq!(got[1].x, 41);
        assert_eq!(got[1].right(), 61);
        assert_eq!(got[1].y, 6);
        assert_eq!(got[2].y, got[1].bottom() + 1, "one separator row");
        assert_eq!(got[2].bottom(), 21);
        // the borders: a frame, the separators, joined
        let b = borders(area, &got, None);
        let at = |x, y| b.iter().find(|c| (c.0, c.1) == (x, y)).map(|c| c.2);
        assert_eq!(at(10, 5), Some('┌'));
        assert_eq!(at(40, 5), Some('┬'));
        assert_eq!(at(40, got[1].bottom()), Some('├'));
        assert_eq!(at(61, got[1].bottom()), Some('┤'));
        assert_eq!(at(40, 21), Some('┴'));
        assert_eq!(at(20, 20), None, "inside a pane");
        assert!(b.iter().all(|c| !c.3), "nothing highlighted");
        // the bottom-right pane highlighted: its outline only
        let b = borders(area, &got, Some(2));
        let hl: Vec<_> = b.iter().filter(|c| c.3).collect();
        assert!(hl.iter().all(|c| c.0 >= 40 && c.1 >= got[1].bottom()));
        assert_eq!(
            hl.len(),
            2 * (got[2].width as usize + 2) + 2 * got[2].height as usize
        );
    }

    /// A sidebar left out of the layout leaves no gap: the rest fills the
    /// frame; panes without geometry are stacked.
    #[test]
    fn the_bounding_box_fills_the_frame() {
        let got = place(&[(31, 0, 169, 50)], Rect::new(0, 0, 40, 10));
        assert_eq!(got, [Rect::new(1, 1, 38, 8)]);
        let got = place(&[(0, 0, 0, 0), (0, 0, 0, 0)], Rect::new(0, 0, 40, 11));
        assert_eq!(got[0].y, 1);
        assert_eq!(got[1].y, got[0].bottom() + 1);
        assert_eq!(got[1].bottom(), 10);
        assert_eq!(
            place(&[(0, 0, 9, 9)], Rect::new(0, 0, 2, 2)),
            [Rect::default()]
        );
    }

    fn plain(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn colours_and_attributes_are_kept() {
        let l = parse(
            "\x1b[31mred\x1b[39m \x1b[1;4mbu\x1b[0m \x1b[2;3mdi\x1b[0m \x1b[7mrev\x1b[0m \x1b[48;5;22;38;2;1;2;3mx\x1b[0m",
        );
        assert_eq!(l.len(), 1);
        let style_of = |t: &str| l[0].spans.iter().find(|s| s.content == t).unwrap().style;
        assert_eq!(style_of("red").fg, Some(Color::Red));
        let bu = style_of("bu").add_modifier;
        assert!(bu.contains(Modifier::BOLD | Modifier::UNDERLINED));
        let di = style_of("di").add_modifier;
        assert!(di.contains(Modifier::DIM | Modifier::ITALIC));
        assert!(style_of("rev").add_modifier.contains(Modifier::REVERSED));
        let x = style_of("x");
        assert_eq!(
            (x.fg, x.bg),
            (Some(Color::Rgb(1, 2, 3)), Some(Color::Indexed(22)))
        );
    }

    /// tmux carries the pen across lines: a colour set on one line holds
    /// on the next until reset.
    #[test]
    fn style_carries_across_lines() {
        let l = parse("\x1b[32mone\ntwo\x1b[0m\nthree");
        assert_eq!(l[1].spans[0].style.fg, Some(Color::Green));
        assert_ne!(l[2].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn tabs_hyperlinks_and_controls_do_not_break_the_layout() {
        let l = parse("ab\tc\x1b[31m\td\x1b[0m e\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ end\x07!");
        assert_eq!(plain(&l[0]), "ab      c       d elink end!");
        // a wide character counts two columns towards the tab stop
        let l = parse("宽\tx");
        assert_eq!(plain(&l[0]), "宽      x");
        assert_eq!(l[0].width(), 9);
        // a BEL-ended OSC (window title) goes too
        assert_eq!(plain(&parse("a\x1b]0;title\x07b")[0]), "ab");
    }

    #[test]
    fn malformed_escapes_do_not_panic_and_keep_the_text() {
        for bad in [
            "\x1b[31",
            "\x1b[38;5mx",
            "\x1b[38;2;1;2mx",
            "\x1b[999;999;999mok",
            "\x1b",
            "a\x1b]8;;unterminated",
            "\x1b[4:3mcurly\x1b[0m",
            "\x1b[\x1b[\x1b[",
            "x\x1bY\x1b(B\x1b[?25ly",
        ] {
            let l = parse(bad);
            let text: String = l.iter().map(plain).collect();
            assert!(!text.contains('\x1b'), "{bad:?} -> {text:?}");
        }
        assert!(plain(&parse("\x1b[4:3mcurly\x1b[0m")[0]).contains("curly"));
        assert!(plain(&parse("\x1b[999;999;999mok")[0]).contains("ok"));
        assert_eq!(plain(&parse("a\x1b]8;;unterminated\nb")[1]), "b");
    }

    #[test]
    fn the_fallback_strips_escapes() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m \x1b(plain"), "red plain");
    }

    #[test]
    fn trailing_blank_lines_are_trimmed() {
        let l = parse("$ ls\nfile\n\x1b[0m\n   \n\n");
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn the_window_ends_offset_lines_above_the_bottom() {
        let lines: Vec<Line<'static>> = (1..=10).map(|i| Line::raw(i.to_string())).collect();
        let rows = |h, o| {
            let (o, w) = window(&lines, h, o);
            (o, w.iter().map(plain).collect::<Vec<_>>().join(","))
        };
        assert_eq!(rows(3, 0), (0, "8,9,10".into()));
        assert_eq!(rows(3, 2), (2, "6,7,8".into()));
        assert_eq!(rows(3, 99), (7, "1,2,3".into()), "clamped to the top");
        assert_eq!(
            rows(20, 5),
            (
                0,
                (1..=10)
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        );
        assert_eq!(rows(0, 3), (3, String::new()));
    }
}
