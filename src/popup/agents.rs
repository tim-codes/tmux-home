//! How agents look in the popup (SPEC §7): the state cells of the table
//! (and of the preview's pane bands). Status is always icon **and**
//! word, never colour alone; a stale agent is dimmed and says `(ended?)`.

use crate::agent::Status;
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

/// A model ID as the banner shows it: `claude-opus-5-5` → `opus 5.5`
/// (a date suffix dropped, `[1m]` kept as ` 1m`; the old
/// `claude-3-5-sonnet-…` order too); anything else without its
/// `claude-` prefix.
pub fn short_model(id: &str) -> String {
    let id = id.trim();
    let (base, ctx) = match id.split_once('[') {
        Some((b, rest)) => (b, rest.trim_end_matches(']')),
        None => (id, ""),
    };
    let base = base.strip_prefix("claude-").unwrap_or(base);
    let mut parts: Vec<&str> = base.split('-').collect();
    if parts.len() > 1
        && parts
            .last()
            .is_some_and(|p| p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit()))
    {
        parts.pop();
    }
    let num = |p: &&str| !p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit());
    let word = |p: &&str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase());
    let short = match parts.split_first() {
        // opus-5-5
        Some((fam, ver)) if word(fam) && !ver.is_empty() && ver.iter().all(num) => {
            Some(format!("{fam} {}", ver.join(".")))
        }
        // 3-5-sonnet
        _ => match parts.split_last() {
            Some((fam, ver)) if word(fam) && !ver.is_empty() && ver.iter().all(num) => {
                Some(format!("{fam} {}", ver.join(".")))
            }
            _ => None,
        },
    };
    let mut out = short.unwrap_or_else(|| base.to_string());
    if !ctx.is_empty() {
        out = format!("{out} {ctx}");
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_cuts_with_an_ellipsis() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
    }

    #[test]
    fn model_ids_are_shortened() {
        for (id, short) in [
            ("claude-opus-5-5", "opus 5.5"),
            ("claude-sonnet-5-5", "sonnet 5.5"),
            ("claude-sonnet-4-5-20250929", "sonnet 4.5"),
            ("claude-opus-4-20250514", "opus 4"),
            ("claude-haiku-4-5", "haiku 4.5"),
            ("claude-3-5-sonnet-20241022", "sonnet 3.5"),
            ("claude-opus-4-6[1m]", "opus 4.6 1m"),
            ("claude-example-model", "example-model"),
            ("fable", "fable"),
            ("  claude-opus-5-5 ", "opus 5.5"),
        ] {
            assert_eq!(short_model(id), short, "{id}");
        }
    }
}
