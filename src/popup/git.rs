//! How git looks in the popup: the compact badge in a pane row's `GT`
//! cell, the preview's pane bands and the sidebar's lines. Symbols and
//! their meaning: `crate::git::badge`. A stale status
//! (its last check timed out or failed) is drawn dimmed, ending in `~`.

use crate::git::badge::{Part, RepoStatus};
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn part_style(p: Part) -> Style {
    let s = Style::default();
    match p {
        Part::Branch => s.fg(Color::Magenta),
        Part::Worktree | Part::InSync | Part::Stale => dim(),
        Part::Dirty => s.fg(Color::Yellow),
        Part::Conflict => s.fg(Color::Red).add_modifier(Modifier::BOLD),
        Part::Sync => s.fg(Color::Cyan),
        Part::Stash => s.fg(Color::Blue),
        Part::Stray => s.fg(Color::Red),
        Part::State => s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
    }
}

/// The badge as spans, each piece preceded by `sep` (two spaces before the
/// first, one between the rest).
pub fn badge_spans(g: &RepoStatus) -> Vec<Span<'static>> {
    g.badge()
        .into_iter()
        .enumerate()
        .map(|(i, (text, part))| {
            let style = if g.stale { dim() } else { part_style(part) };
            Span::styled(format!("{}{text}", if i == 0 { "  " } else { " " }), style)
        })
        .collect()
}

/// The badge as table-cell pieces: one space between them, none before
/// the first.
pub fn badge_pieces(g: &RepoStatus) -> Vec<Span<'static>> {
    let mut v = badge_spans(g);
    if let Some(first) = v.first_mut() {
        first.content = first.content.trim_start().to_string().into();
    }
    v
}

/// The badge spans that fit in `room` columns: whole pieces, dropped from
/// the right (the branch goes last), never part of one.
pub fn fit_badge(mut spans: Vec<Span<'static>>, room: usize) -> Vec<Span<'static>> {
    while spans.iter().map(Span::width).sum::<usize>() > room {
        spans.pop();
    }
    spans
}

/// `s` in at most `n` columns: cut with `…`, or nothing when `n` is 0.
pub fn clip_to(s: &str, n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        super::agents::clip(s, n)
    }
}
