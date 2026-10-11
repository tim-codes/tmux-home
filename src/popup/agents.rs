//! How agents look in the popup (SPEC §7): the state cells of the table
//! (and of the preview's pane bands). Status is always icon **and**
//! word, never colour alone; a stale agent is dimmed and says `(ended?)`.

use crate::agent::{PaneAgent, Status};
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
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

/// An agent state cell: icon and word, coloured by status; a stale
/// agent's dimmed, with `(ended?)`.
pub fn state_span(s: Status, stale: bool) -> Span<'static> {
    if stale {
        Span::styled(format!("{} {} (ended?)", s.icon(), s.word()), dim())
    } else {
        Span::styled(format!("{} {}", s.icon(), s.word()), status_style(s))
    }
}

/// Why `p` needs you: its wait reason, else `needs you`.
pub fn need_reason(p: &PaneAgent) -> String {
    p.state
        .wait_reason
        .as_ref()
        .map(|r| r.label())
        .unwrap_or_else(|| "needs you".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_cuts_with_an_ellipsis() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
    }
}
