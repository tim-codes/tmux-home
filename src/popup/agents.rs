//! How agents look in the popup (SPEC §7): the status cell of a window row
//! and the agent card the preview shows. Status is always icon **and**
//! word, never colour alone; a stale agent is dimmed and says `(ended?)`.

use crate::agent::{PaneAgent, Status, WindowAgents, elapsed};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

fn status_style(s: Status) -> Style {
    let st = Style::default();
    match s {
        Status::Running => st.fg(Color::Green),
        Status::Waiting => st.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Status::Error => st.fg(Color::Red).add_modifier(Modifier::BOLD),
        Status::Background => st.fg(Color::Blue),
        Status::Idle => st,
        Status::Unknown => st.add_modifier(Modifier::DIM),
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// First `n` characters of `s`, with `…` if cut.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// The status cell of an agent window's row, in place of command + path:
/// `● running    12m  claude ×2  (wt) feat/x  plan  <reason or prompt>`.
pub fn row_cell(a: &WindowAgents, now: u64) -> Vec<Span<'static>> {
    let lead = a.lead();
    let st = &lead.state;
    let mut v = Vec::new();
    if a.stale() {
        v.push(Span::styled(
            format!("  {} {} (ended?)", st.status.icon(), st.status.word()),
            dim(),
        ));
        v.push(Span::styled(format!("  {}", st.kind.name()), dim()));
        return v;
    }
    let s = st.status;
    v.push(Span::styled(
        format!("  {} {:<10}", s.icon(), s.word()),
        status_style(s),
    ));
    let run = st.run_started.map(|t| elapsed(t, now)).unwrap_or_default();
    v.push(Span::raw(format!(" {run:>5}")));
    let n = a.live_count();
    let count = if n > 1 {
        format!(" ×{n}")
    } else {
        String::new()
    };
    v.push(Span::raw(format!("  {}{count}", st.kind.name())));
    if let Some(wt) = &st.worktree {
        v.push(Span::styled(format!("  (wt) {}", wt.branch), dim()));
    }
    if st.permission_mode.as_deref() == Some("plan") {
        v.push(Span::styled("  plan", Style::default().fg(Color::Cyan)));
    }
    if let Some(p) = a.needing() {
        let why = p
            .state
            .wait_reason
            .as_ref()
            .map(|r| r.label())
            .unwrap_or_else(|| "needs you".into());
        v.push(Span::styled(
            format!("  {}", clip(&why, 60)),
            Style::default().fg(Color::Yellow),
        ));
    } else if let Some(p) = &st.prompt {
        v.push(Span::styled(format!("  {}", clip(p, 60)), dim()));
    }
    v
}

/// Greedy word wrap to `width` columns, at most `max` lines (the last
/// ends in `…` if text was left over).
pub fn wrap(text: &str, width: usize, max: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        loop {
            let need = cur.chars().count() + usize::from(!cur.is_empty()) + word.chars().count();
            if need <= width {
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(&word);
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            // a word longer than a line: hard-break it
            let head: String = word.chars().take(width).collect();
            word = word.chars().skip(width).collect();
            lines.push(head);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max {
        lines.truncate(max);
        if let Some(l) = lines.last_mut() {
            *l = format!("{}…", clip(l, width.saturating_sub(1)));
        }
    }
    lines
}

/// The agent card: the preview's top for an agent window. `width` is the
/// preview's inner width.
pub fn card(a: &WindowAgents, now: u64, width: usize) -> Vec<Line<'static>> {
    let p: &PaneAgent = a.lead();
    let st = &p.state;
    let label = |k: &str| Span::styled(format!("{k:<11}"), dim());
    let mut out = Vec::new();

    // status line
    let mut head = vec![Span::raw(" ")];
    if p.stale {
        head.push(Span::styled(
            format!("{} {} (ended?)", st.status.icon(), st.status.word()),
            dim(),
        ));
    } else {
        head.push(Span::styled(
            format!("{} {}", st.status.icon(), st.status.word()),
            status_style(st.status),
        ));
    }
    head.push(Span::raw(format!("  ·  {}", st.kind.name())));
    if let Some(t) = st.run_started {
        head.push(Span::raw(format!("  ·  run {}", elapsed(t, now))));
    }
    let n = a.live_count();
    if n > 1 {
        head.push(Span::raw(format!("  ·  {n} agents")));
    }
    out.push(Line::from(head));
    if p.stale {
        out.push(Line::styled(
            " the pane is back at its shell: these are left-over options",
            dim(),
        ));
    }

    let mut row = |k: &str, v: String, style: Style| {
        out.push(Line::from(vec![
            Span::raw(" "),
            label(k),
            Span::styled(v, style),
        ]));
    };
    if let Some(r) = &st.wait_reason {
        let k = if p.needs_you() { "needs you" } else { "reason" };
        row(k, r.label(), Style::default().fg(Color::Yellow));
    } else if p.needs_you() {
        row(
            "needs you",
            "notification".into(),
            Style::default().fg(Color::Yellow),
        );
    }
    if !st.subagents.is_empty() {
        row(
            "subagents",
            format!("+{} ({})", st.subagents.len(), st.subagents.join(", ")),
            Style::default(),
        );
    }
    if let Some(bg) = &st.bg_cmd {
        row("background", bg.clone(), Style::default());
    }
    if let Some(wt) = &st.worktree {
        let v = match (wt.name.is_empty(), wt.branch.is_empty()) {
            (false, false) => format!("{} ({})", wt.name, wt.branch),
            (true, _) => wt.branch.clone(),
            (_, true) => wt.name.clone(),
        };
        row("worktree", v, Style::default());
    }
    if let Some(m) = &st.permission_mode {
        row("mode", m.clone(), Style::default());
    }
    if let Some(pr) = &st.prompt {
        let k = if st.prompt_is_reply {
            "reply"
        } else {
            "prompt"
        };
        let text = wrap(pr, width.saturating_sub(13), 6);
        for (i, l) in text.into_iter().enumerate() {
            out.push(Line::from(vec![
                Span::raw(" "),
                if i == 0 {
                    label(k)
                } else {
                    Span::raw(" ".repeat(11))
                },
                Span::raw(l),
            ]));
        }
    }
    out.push(Line::styled("─".repeat(width), dim()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_and_clip() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
        assert_eq!(
            wrap("one two three four", 9, 5),
            ["one two", "three", "four"]
        );
        assert_eq!(wrap("one two three four", 9, 2), ["one two", "three…"]);
        assert_eq!(wrap("abcdefghijkl", 8, 5), ["abcdefgh", "ijkl"]);
        assert!(wrap("", 20, 3).is_empty());
    }
}
