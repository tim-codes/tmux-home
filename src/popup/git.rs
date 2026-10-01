//! How git looks in the popup: the compact badge on a window row (and on an
//! agent row, for the lead agent's worktree) and the git card in the
//! preview. Symbols and their meaning: `crate::git::badge`. A stale status
//! (its last check timed out or failed) is drawn dimmed, ending in `~`.

use super::app::short_path;
use crate::git::badge::{Part, Phase, RepoStatus};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
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

fn plural(n: u32, one: &str) -> String {
    format!("{n} {one}")
}

/// The git card: what the badge abbreviates, spelled out. `width` is the
/// preview's inner width; `home` shortens paths.
pub fn card(g: &RepoStatus, root: &str, width: usize, home: &str) -> Vec<Line<'static>> {
    let label = |k: &str| Span::styled(format!(" {k:<11}"), dim());
    let mut out = Vec::new();
    let mut row = |k: &str, v: Vec<Span<'static>>| {
        let mut l = vec![label(k)];
        l.extend(v);
        out.push(Line::from(l));
    };

    // branch → upstream, ahead/behind
    let mut head = vec![Span::styled(
        if g.branch.is_empty() {
            "?".to_string()
        } else {
            g.branch.clone()
        },
        part_style(Part::Branch),
    )];
    if g.detached {
        head.push(Span::styled(" (detached)", dim()));
    }
    match (&g.upstream, g.gone) {
        (Some(u), true) => head.push(Span::styled(format!(" → {u} (gone)"), dim())),
        (Some(u), false) => {
            head.push(Span::raw(format!(" → {u}")));
            if g.phase != Phase::Head {
                let ab = match (g.ahead, g.behind) {
                    (0, 0) => "  in sync".to_string(),
                    (a, 0) => format!("  ⇡{a} ahead"),
                    (0, b) => format!("  ⇣{b} behind"),
                    (a, b) => format!("  ⇡{a} ahead ⇣{b} behind"),
                };
                head.push(Span::styled(ab, part_style(Part::Sync)));
            }
        }
        (None, _) if g.phase == Phase::Refs && !g.detached => head.push(Span::styled(
            if g.has_remote {
                "  no upstream"
            } else {
                "  no remote"
            },
            dim(),
        )),
        _ => {}
    }
    row("git", head);

    if g.phase == Phase::Head {
        row("changes", vec![Span::styled("checking…", dim())]);
    } else if g.dirty() {
        let mut parts = Vec::new();
        for (n, w) in [
            (g.staged, "staged"),
            (g.modified, "modified"),
            (g.untracked, "untracked"),
        ] {
            if n > 0 {
                parts.push(plural(n, w));
            }
        }
        if g.conflicts > 0 {
            parts.push(plural(g.conflicts, "conflicted"));
        }
        row(
            "changes",
            vec![Span::styled(parts.join(" · "), part_style(Part::Dirty))],
        );
    } else {
        row("changes", vec![Span::styled("clean", dim())]);
    }
    if let Some(op) = g.operation {
        row(
            "state",
            vec![Span::styled(
                format!("↻ {} in progress", op.word()),
                part_style(Part::State),
            )],
        );
    }
    if g.stashes > 0 {
        row(
            "stash",
            vec![Span::styled(
                format!("${}", g.stashes),
                part_style(Part::Stash),
            )],
        );
    }
    if g.stray > 0 {
        let mut names = g.stray_names.join(", ");
        if (g.stray as usize) > g.stray_names.len() {
            names.push_str(", …");
        }
        row(
            "stray",
            vec![Span::styled(
                format!("⚠{} {names}", g.stray),
                part_style(Part::Stray),
            )],
        );
    }
    let mut wt = vec![Span::raw(short_path(root, home))];
    if g.linked {
        let main = g
            .main_root
            .as_deref()
            .map(|m| format!(" (linked; main {})", short_path(m, home)))
            .unwrap_or_else(|| " (linked)".into());
        wt.push(Span::styled(main, dim()));
    } else if g.worktrees > 0 {
        wt.push(Span::styled(format!(" (+{} linked)", g.worktrees), dim()));
    }
    let mut flags = Vec::new();
    for (on, w) in [
        (g.prunable, "⊟ prunable"),
        (g.locked, "⊞ locked"),
        (g.mismatch, "⚑ branch/path mismatch"),
    ] {
        if on {
            flags.push(w);
        }
    }
    if !flags.is_empty() {
        wt.push(Span::styled(
            format!("  {}", flags.join(" ")),
            part_style(Part::State),
        ));
    }
    row("worktree", wt);
    if let Some(d) = &g.default_branch {
        row("default", vec![Span::styled(d.clone(), dim())]);
    }
    if g.stale {
        let why = match &g.error {
            Some(e) => format!(
                "~ stale: {}",
                super::agents::clip(e, width.saturating_sub(22))
            ),
            None => "~ stale: the last check timed out".into(),
        };
        row("", vec![Span::styled(why, dim())]);
    }
    out.push(Line::styled("─".repeat(width), dim()));
    out
}
