//! The preview pane's text: a `capture-pane -e` capture parsed into styled
//! lines (colours, bold, dim, italic, underline, reverse kept), and the
//! window of it the preview shows at a scroll offset.
//!
//! Lines are clipped at the preview's right edge, never wrapped: the capture
//! is already laid out at the pane's width (tmux wraps it; `-J` is not
//! used), so one captured line is one preview row. That keeps a scroll
//! offset a plain line count, a TUI's columns lined up, and the bottom of
//! the preview the bottom of the pane.

use ansi_to_tui::IntoText;
use ratatui::text::{Line, Span};

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
    let mut lines = match std::panic::catch_unwind(|| clean.into_text()) {
        Ok(Ok(t)) => t.lines,
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
