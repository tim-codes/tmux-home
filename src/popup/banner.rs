//! The preview's pane banners: up to three lines over each pane's capture
//! and a dim rule under them, all from what the daemon's snapshot already
//! holds (the pane row's agent state and git status; nothing is read
//! here). Most important first:
//!
//! 1. `.N cmd`, the agent's state (icon and word), its run time, its model
//! 2. git: branch `→ upstream` and sync (`≡`, `⇡n⇣n`), changes (`+n !n ?n`,
//!    `=n`), stashes `$n`, an operation in progress, `⊗`/`~`
//! 3. where: the cwd, `(wt) ← main worktree` or the linked count, `⚠n
//!    stray`, `⚑ mismatch`, `⊟`/`⊞`, the default branch
//!
//! Every banner of a layout has the same height (`height`), whatever its
//! pane is, so moving the selection never shifts a row: missing lines are
//! left blank. A pane without a repo shows its cwd on the second line.
//! Each line is fitted to the pane's width by dropping whole pieces, the
//! least important first; only the leading piece is ever cut.

use super::app::{PaneRow, short_path};
use super::{agents, git};
use crate::git::badge::{Part, RepoStatus};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// The tallest banner: three lines and the rule.
pub const MAX: u16 = 4;
/// The capture rows a pane keeps before its banner gives up rows.
const MIN_BODY: u16 = 3;

/// The banner height for a layout whose shortest pane is `rows` tall:
/// `MAX` down to one line as the panes get short, never more than a pane.
pub fn height(rows: u16) -> u16 {
    rows.saturating_sub(MIN_BODY).clamp(1, MAX).min(rows)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// A piece of a line: its spans, the separator before it (not before the
/// line's first) and when it goes (higher drops first; 0 never drops).
struct Piece {
    sep: &'static str,
    drop: u8,
    spans: Vec<Span<'static>>,
}

fn piece(sep: &'static str, drop: u8, spans: Vec<Span<'static>>) -> Piece {
    Piece { sep, drop, spans }
}

fn width_of(pieces: &[&Piece]) -> usize {
    pieces
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let sep = if i == 0 { 0 } else { p.sep.chars().count() };
            sep + p.spans.iter().map(Span::width).sum::<usize>()
        })
        .sum()
}

/// `pieces` in `width` columns: the droppable ones go, highest `drop`
/// (then rightmost) first, until the rest fit; then the first is cut.
fn fit(pieces: Vec<Piece>, width: usize) -> Vec<Span<'static>> {
    let mut keep: Vec<&Piece> = pieces.iter().collect();
    while width_of(&keep) > width {
        let Some(i) = (1..keep.len()).rev().max_by_key(|&i| (keep[i].drop, i)) else {
            break;
        };
        keep.remove(i);
    }
    let mut out = Vec::new();
    for (i, p) in keep.iter().enumerate() {
        if i > 0 {
            out.push(Span::raw(p.sep));
        }
        out.extend(p.spans.iter().cloned());
    }
    // only the leading piece is left and it is too wide: cut it
    let mut room = width;
    out.into_iter()
        .filter_map(|s| {
            let w = s.width();
            if w <= room {
                room -= w;
                Some(s)
            } else if room > 0 {
                let t = git::clip_to(&s.content, room);
                room = 0;
                Some(Span::styled(t, s.style))
            } else {
                None
            }
        })
        .collect()
}

/// Line 1: `.N cmd`, state, run time, model.
fn identity(p: &PaneRow, now: u64) -> Vec<Piece> {
    let mut v = vec![piece(
        "",
        0,
        vec![
            Span::styled(format!(".{}", p.index), dim()),
            Span::raw(format!(" {}", p.cmd)),
        ],
    )];
    if let Some(a) = &p.agent {
        v.push(piece(
            "  ",
            1,
            vec![agents::state_span(a.state.status, a.stale)],
        ));
        if let Some(t) = a.state.run_started.filter(|_| a.live()) {
            v.push(piece(
                "  ",
                2,
                vec![Span::raw(crate::agent::elapsed(t, now))],
            ));
        }
        if let Some(m) = a.state.model.as_deref().filter(|m| !m.is_empty()) {
            v.push(piece(
                "  ",
                3,
                vec![Span::styled(agents::short_model(m), dim())],
            ));
        }
    }
    v
}

fn part(p: Part) -> Style {
    git::part_style(p)
}

/// Line 2: branch, upstream and sync, changes, stashes, state.
fn git_line(g: &RepoStatus) -> Vec<Piece> {
    let mut head = Vec::new();
    if g.detached {
        head.push(Span::styled("⊘ ", part(Part::State)));
    }
    head.push(Span::styled(g.branch.clone(), part(Part::Branch)));
    let mut v = vec![piece("", 0, head)];
    if let Some(up) = &g.upstream {
        v.push(piece(
            " ",
            5,
            vec![Span::styled("→ ", dim()), Span::raw(up.clone())],
        ));
        let sync = if g.gone {
            Some(Span::styled("gone", Style::default().fg(Color::Red)))
        } else if g.ahead == 0 && g.behind == 0 {
            (g.phase != crate::git::badge::Phase::Head)
                .then(|| Span::styled("≡", part(Part::InSync)))
        } else {
            // as the badge: `⇡n`, `⇣n`, diverged `⇕⇡n⇣n`
            let s = match (g.ahead, g.behind) {
                (a, 0) => format!("⇡{a}"),
                (0, b) => format!("⇣{b}"),
                (a, b) => format!("⇕⇡{a}⇣{b}"),
            };
            Some(Span::styled(s, part(Part::Sync)))
        };
        if let Some(s) = sync {
            v.push(piece(" ", 1, vec![s]));
        }
    }
    let changes: Vec<String> = [(g.staged, '+'), (g.modified, '!'), (g.untracked, '?')]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, c)| format!("{c}{n}"))
        .collect();
    if !changes.is_empty() {
        v.push(piece(
            "  ",
            1,
            vec![Span::styled(changes.join(" "), part(Part::Dirty))],
        ));
    }
    if g.conflicts > 0 {
        v.push(piece(
            " ",
            1,
            vec![Span::styled(
                format!("={}", g.conflicts),
                part(Part::Conflict),
            )],
        ));
    }
    if g.stashes > 0 {
        v.push(piece(
            "  ",
            3,
            vec![Span::styled(format!("${}", g.stashes), part(Part::Stash))],
        ));
    }
    if let Some(op) = g.operation {
        v.push(piece(
            "  ",
            1,
            vec![Span::styled(format!("↻ {}", op.word()), part(Part::State))],
        ));
    }
    if g.limited {
        v.push(piece(
            "  ",
            2,
            vec![Span::styled("⊗ limited", part(Part::State))],
        ));
    }
    if g.stale {
        v.push(piece(
            "  ",
            2,
            vec![Span::styled("~ stale", part(Part::Stale))],
        ));
    }
    v
}

/// `~` for the home directory.
fn tilde(p: &str, home: &str) -> String {
    match p.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => p.to_string(),
    }
}

/// Line 3 (line 2 outside a repo): the cwd and the worktree.
fn place(p: &PaneRow, home: &str, width: usize) -> Vec<Piece> {
    let mut rest = Vec::new();
    if let Some((_, g)) = &p.git {
        if g.linked {
            let main = g
                .main_root
                .as_deref()
                .map(|m| format!(" ← {}", short_path(m, home)))
                .unwrap_or_default();
            rest.push(piece(
                "  ",
                1,
                vec![
                    Span::styled("(wt)", Style::default().fg(Color::Cyan)),
                    Span::styled(main, dim()),
                ],
            ));
        } else if g.worktrees > 0 {
            let n = g.worktrees;
            rest.push(piece(
                "  ",
                2,
                vec![Span::styled(
                    format!("{n} worktree{}", if n == 1 { "" } else { "s" }),
                    dim(),
                )],
            ));
        }
        if g.stray > 0 {
            rest.push(piece(
                "  ",
                1,
                vec![Span::styled(
                    format!("⚠{} stray", g.stray),
                    part(Part::Stray),
                )],
            ));
        }
        for (on, text) in [
            (g.mismatch, "⚑ mismatch"),
            (g.prunable, "⊟ prunable"),
            (g.locked, "⊞ locked"),
        ] {
            if on {
                rest.push(piece("  ", 2, vec![Span::styled(text, part(Part::State))]));
            }
        }
        if let Some(d) = g.default_branch.as_deref().filter(|d| *d != g.branch) {
            rest.push(piece(
                "  ",
                4,
                vec![Span::styled("default ", dim()), Span::raw(d.to_string())],
            ));
        }
    }
    // the whole cwd when the line fits, else its short form
    let full = tilde(&p.cwd, home);
    let path = |s: String| piece("", 0, vec![Span::raw(s)]);
    let mut all = vec![path(full)];
    all.extend(rest);
    if width_of(&all.iter().collect::<Vec<_>>()) > width {
        all[0] = path(short_path(&p.cwd, home));
    }
    all
}

/// Pane `p`'s banner in `width` columns and `rows` rows (`height`):
/// exactly `rows` lines. `selected`: drawn bright, else dimmed.
pub fn lines(
    p: &PaneRow,
    home: &str,
    now: u64,
    width: usize,
    rows: u16,
    selected: bool,
) -> Vec<Line<'static>> {
    let mut text: Vec<Vec<Piece>> = vec![identity(p, now)];
    if let Some((_, g)) = &p.git {
        text.push(git_line(g));
    }
    if !p.cwd.is_empty() {
        text.push(place(p, home, width));
    }
    let rule = rows == MAX;
    let n = rows as usize - usize::from(rule);
    let mut out: Vec<Line> = text
        .into_iter()
        .take(n)
        .map(|pieces| Line::from(fit(pieces, width)))
        .collect();
    out.resize(n, Line::default());
    if rule {
        out.push(Line::styled("┄".repeat(width), dim()));
    }
    if !selected {
        for l in &mut out {
            for s in &mut l.spans {
                s.style = s.style.add_modifier(Modifier::DIM);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_height_shrinks_with_the_panes() {
        assert_eq!(height(40), 4);
        assert_eq!(height(7), 4);
        assert_eq!(height(6), 3);
        assert_eq!(height(5), 2);
        assert_eq!(height(4), 1);
        assert_eq!(height(2), 1);
        assert_eq!(height(1), 1);
        assert_eq!(height(0), 0);
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn pieces_drop_whole_least_important_first() {
        let p = |sep, drop, t: &'static str| piece(sep, drop, vec![Span::raw(t)]);
        let mk = || {
            vec![
                p("", 0, "main"),
                p(" ", 5, "→ origin/main"),
                p(" ", 1, "⇡2"),
                p("  ", 3, "$1"),
            ]
        };
        assert_eq!(text(&fit(mk(), 80)), "main → origin/main ⇡2  $1");
        assert_eq!(text(&fit(mk(), 20)), "main ⇡2  $1");
        assert_eq!(text(&fit(mk(), 8)), "main ⇡2");
        assert_eq!(text(&fit(mk(), 5)), "main");
        assert_eq!(text(&fit(mk(), 3)), "ma…");
        assert_eq!(text(&fit(mk(), 0)), "");
    }

    #[test]
    fn tilde_only_at_a_path_boundary() {
        assert_eq!(tilde("/home/u/dev", "/home/u"), "~/dev");
        assert_eq!(tilde("/home/u", "/home/u"), "~");
        assert_eq!(tilde("/home/user2", "/home/u"), "/home/user2");
        assert_eq!(tilde("/tmp", ""), "/tmp");
    }
}
