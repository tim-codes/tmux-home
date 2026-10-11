//! The filter line: tokens (`@attn @agent @running @waiting @background
//! @idle @error`) plus free text. It applies to the session page shown;
//! paging (not a token) picks the session.
//!
//! - Status tokens are alternatives (`@running @waiting`: either); every
//!   other token, and the text, must also hold.
//! - A status token matches a window with a live (non-stale) agent in that
//!   status; `@attn` one that needs you; `@agent` any agent window, stale
//!   ones included.
//! - An unknown `@word` is plain text.
//!
//! The text is matched fuzzily against a window's index, name and agent
//! kinds, and each of its panes' command, path, title and branch; against agent prompts it is matched word by word as
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
                "@background" => Some(Status::Background),
                "@error" => Some(Status::Error),
                _ => None,
            };
            match (w, status) {
                (_, Some(s)) => out.statuses.push(s),
                ("@attn", _) => out.attn = true,
                ("@agent", _) => out.agent = true,
                _ => text.push(w),
            }
        }
        out.text = text.join(" ");
        out
    }

    /// No tokens and no text: everything matches.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.statuses.is_empty() && !self.attn && !self.agent
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
        self.statuses.is_empty()
            || self
                .statuses
                .iter()
                .any(|&s| a.is_some_and(|a| a.has_status(s)))
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
        let q = Query::parse("  @waiting dot @attn s:Main  fix @bogus @running");
        assert_eq!(q.statuses, [Status::Waiting, Status::Running]);
        assert!(q.attn && !q.agent);
        assert_eq!(q.text, "dot s:Main fix @bogus", "s: is no token any more");
        assert!(Query::parse("   ").is_empty());
        assert!(!Query::parse("@agent").is_empty());
    }
}
