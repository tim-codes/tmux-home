//! The filter line: tokens (`@attn @agent @running @waiting @idle @error
//! s:<session>`) plus free text.
//!
//! - Status tokens are alternatives (`@running @waiting`: either); every
//!   other token, and the text, must also hold.
//! - A status token matches a window with a live (non-stale) agent in that
//!   status; `@attn` one that needs you; `@agent` any agent window, stale
//!   ones included.
//! - `s:<session>` matches session names by case-insensitive prefix.
//! - An unknown `@word` is plain text.
//!
//! The text is matched fuzzily against session, index, name, command, path
//! and agent kind; against agent prompts it is matched word by word as
//! case-insensitive substrings, since a long prompt would fuzzily match
//! almost any short query.

use super::app::Row;
use crate::agent::Status;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub text: String,
    pub statuses: Vec<Status>,
    pub attn: bool,
    pub agent: bool,
    pub sessions: Vec<String>,
}

impl Query {
    pub fn parse(q: &str) -> Query {
        let mut out = Query::default();
        let mut text = Vec::new();
        for w in q.split_whitespace() {
            let status = match w {
                "@running" => Some(Status::Running),
                "@waiting" => Some(Status::Waiting),
                "@idle" => Some(Status::Idle),
                "@error" => Some(Status::Error),
                _ => None,
            };
            match (w, status) {
                (_, Some(s)) => out.statuses.push(s),
                ("@attn", _) => out.attn = true,
                ("@agent", _) => out.agent = true,
                _ => match w.strip_prefix("s:") {
                    Some(s) if !s.is_empty() => out.sessions.push(s.to_lowercase()),
                    _ => text.push(w),
                },
            }
        }
        out.text = text.join(" ");
        out
    }

    /// No tokens and no text: everything matches.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.statuses.is_empty()
            && !self.attn
            && !self.agent
            && self.sessions.is_empty()
    }

    /// The tokens hold for `r` (the text is matched by the caller).
    pub fn tokens_match(&self, r: &Row) -> bool {
        let a = r.agents.as_ref();
        if self.agent && a.is_none() {
            return false;
        }
        if self.attn && !a.is_some_and(|a| a.needs_you()) {
            return false;
        }
        if !self.statuses.is_empty()
            && !self
                .statuses
                .iter()
                .any(|&s| a.is_some_and(|a| a.has_status(s)))
        {
            return false;
        }
        let session = r.session.to_lowercase();
        self.sessions.iter().all(|s| session.starts_with(s))
    }

    /// Every text word is a substring of the row's (lowercased) prompts.
    pub fn prompt_match(&self, r: &Row) -> bool {
        !r.prompts.is_empty()
            && self
                .text
                .split_whitespace()
                .all(|w| r.prompts.contains(&w.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tokens_and_text() {
        let q = Query::parse("  @waiting dot @attn s:Main  fix @bogus @running s:");
        assert_eq!(q.statuses, [Status::Waiting, Status::Running]);
        assert!(q.attn && !q.agent);
        assert_eq!(q.sessions, ["main"]);
        assert_eq!(q.text, "dot fix @bogus s:");
        assert!(Query::parse("   ").is_empty());
        assert!(!Query::parse("@agent").is_empty());
    }
}
