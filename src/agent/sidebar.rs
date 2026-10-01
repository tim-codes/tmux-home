//! tmux-agent-sidebar's `@pane_*` options as an `AgentSource`.
//!
//! The sidebar's hooks (`src/cli/hook/handlers/*.rs` in that repo) write
//! these per pane; see its `docs/state-management.md`. Values, as written:
//! - `@pane_agent`: `claude` / `codex` / `opencode`;
//! - `@pane_status`: `running` / `background` / `waiting` / `idle` /
//!   `error` (older builds: `notification`, shown as waiting);
//! - `@pane_attention`: `notification` or `clear`;
//! - `@pane_wait_reason`: `permission`, `permission_prompt` (a prompt is
//!   open), `permission_denied` (PermissionDenied: the run was refused), `elicitation_dialog`, `teammate_idle:<name>[:why]`,
//!   `session_resumed[_compact]`, or the error text when status is `error`;
//! - `@pane_started_at`: Unix seconds at UserPromptSubmit, cleared at
//!   Stop (unless a bg shell lives on) and StopFailure;
//! - `@pane_prompt` / `@pane_prompt_source`: the last prompt (`user`) or
//!   the last reply (`response`), newlines and `|` replaced by spaces;
//! - `@pane_subagents`: `Type:id,Type:id`;
//! - `@pane_bg_cmd`, `@pane_permission_mode`, `@pane_worktree_name`,
//!   `@pane_worktree_branch`, `@pane_session_id`.

use super::{AgentKind, AgentSource, AgentState, Status, WaitReason, Worktree};
use crate::tmux::snapshot::Pane;

pub const OPTIONS: &[&str] = &[
    "@pane_agent",
    "@pane_status",
    "@pane_attention",
    "@pane_wait_reason",
    "@pane_started_at",
    "@pane_prompt",
    "@pane_prompt_source",
    "@pane_subagents",
    "@pane_bg_cmd",
    "@pane_permission_mode",
    "@pane_worktree_name",
    "@pane_worktree_branch",
    "@pane_session_id",
];

/// Prompts are cut to this many characters when read: the sidebar stores
/// whole prompts (or replies), and the popup shows a few lines at most.
pub const MAX_PROMPT: usize = 2000;

pub struct Sidebar;

impl AgentSource for Sidebar {
    fn options(&self) -> &'static [&'static str] {
        OPTIONS
    }

    fn read(&self, pane: &Pane) -> Option<AgentState> {
        let get = |k: &str| {
            pane.agent_opts
                .get(k)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
        };
        let agent = get("@pane_agent");
        let status_raw = get("@pane_status");
        if agent.is_none() && status_raw.is_none() {
            return None;
        }
        let status = match status_raw.unwrap_or("") {
            "running" => Status::Running,
            "background" => Status::Background,
            "waiting" | "notification" => Status::Waiting,
            "idle" => Status::Idle,
            "error" => Status::Error,
            _ => Status::Unknown,
        };
        let wait_reason = get("@pane_wait_reason").map(|r| parse_reason(r, status));
        let worktree = match (get("@pane_worktree_name"), get("@pane_worktree_branch")) {
            (None, None) => None,
            (n, b) => Some(Worktree {
                name: n.unwrap_or("").into(),
                branch: b.unwrap_or("").into(),
            }),
        };
        Some(AgentState {
            kind: AgentKind::parse(agent.unwrap_or("")),
            status,
            attention: get("@pane_attention") == Some("notification"),
            wait_reason,
            run_started: get("@pane_started_at").and_then(|s| s.parse().ok()),
            prompt: get("@pane_prompt").map(|p| p.chars().take(MAX_PROMPT).collect()),
            prompt_is_reply: get("@pane_prompt_source") == Some("response"),
            subagents: get("@pane_subagents")
                .map(|s| {
                    s.split(',')
                        .filter(|e| !e.trim().is_empty())
                        .map(|e| e.split(':').next().unwrap_or(e).trim().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            bg_cmd: get("@pane_bg_cmd").map(str::to_string),
            permission_mode: get("@pane_permission_mode").map(str::to_string),
            worktree,
            session_id: get("@pane_session_id").map(str::to_string),
        })
    }
}

fn parse_reason(r: &str, status: Status) -> WaitReason {
    match r {
        "permission" | "permission_prompt" => WaitReason::Permission,
        "permission_denied" => WaitReason::PermissionDenied,
        "elicitation_dialog" => WaitReason::Question,
        _ => {
            if let Some(rest) = r.strip_prefix("teammate_idle:") {
                let name = rest.split(':').next().unwrap_or(rest);
                WaitReason::TeammateIdle(name.to_string())
            } else if status == Status::Error {
                WaitReason::Error(r.to_string())
            } else {
                WaitReason::Other(r.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::pane;

    fn read(opts: &[(&str, &str)]) -> Option<AgentState> {
        Sidebar.read(&pane("%1", "node", opts))
    }

    #[test]
    fn needs_agent_or_status() {
        assert_eq!(read(&[]), None);
        assert_eq!(read(&[("@pane_prompt", "x")]), None);
        assert_eq!(read(&[("@pane_agent", "")]), None);
        let s = read(&[("@pane_status", "idle")]).unwrap();
        assert_eq!(s.kind.name(), "agent");
        let s = read(&[("@pane_agent", "codex")]).unwrap();
        assert_eq!((s.kind, s.status), (AgentKind::Codex, Status::Unknown));
    }

    #[test]
    fn reads_a_full_set_of_options() {
        let s = read(&[
            ("@pane_agent", "claude"),
            ("@pane_status", "running"),
            ("@pane_attention", "clear"),
            ("@pane_started_at", "1790864748"),
            ("@pane_prompt", "fix the build"),
            ("@pane_prompt_source", "user"),
            ("@pane_subagents", "Explore:sub-1,Plan:sub-2"),
            ("@pane_bg_cmd", "npm run dev"),
            ("@pane_permission_mode", "plan"),
            ("@pane_worktree_name", "fix-x"),
            ("@pane_worktree_branch", "fix/x"),
            ("@pane_session_id", "abc"),
        ])
        .unwrap();
        assert_eq!(s.kind, AgentKind::Claude);
        assert_eq!(s.status, Status::Running);
        assert!(!s.attention);
        assert_eq!(s.run_started, Some(1790864748));
        assert_eq!(s.prompt.as_deref(), Some("fix the build"));
        assert!(!s.prompt_is_reply);
        assert_eq!(s.subagents, ["Explore", "Plan"]);
        assert_eq!(s.bg_cmd.as_deref(), Some("npm run dev"));
        assert_eq!(s.permission_mode.as_deref(), Some("plan"));
        assert_eq!(
            s.worktree,
            Some(Worktree {
                name: "fix-x".into(),
                branch: "fix/x".into()
            })
        );
    }

    #[test]
    fn long_prompts_are_cut_at_read() {
        let long = "é".repeat(5000);
        let s = read(&[("@pane_agent", "claude"), ("@pane_prompt", &long)]).unwrap();
        assert_eq!(s.prompt.unwrap().chars().count(), MAX_PROMPT);
        assert_eq!(MAX_PROMPT, 2000);
    }

    #[test]
    fn statuses_and_reasons() {
        let st = |v| read(&[("@pane_agent", "claude"), ("@pane_status", v)]).unwrap();
        assert_eq!(st("notification").status, Status::Waiting);
        assert_eq!(st("bogus").status, Status::Unknown);
        let r = |status, reason| {
            read(&[
                ("@pane_agent", "claude"),
                ("@pane_status", status),
                ("@pane_wait_reason", reason),
            ])
            .unwrap()
            .wait_reason
            .unwrap()
        };
        assert_eq!(r("waiting", "permission_prompt"), WaitReason::Permission);
        assert_eq!(
            r("waiting", "permission_denied"),
            WaitReason::PermissionDenied
        );
        assert_ne!(
            WaitReason::PermissionDenied.label(),
            WaitReason::Permission.label()
        );
        assert_eq!(r("waiting", "elicitation_dialog"), WaitReason::Question);
        assert_eq!(
            r("waiting", "teammate_idle:alice:tokens"),
            WaitReason::TeammateIdle("alice".into())
        );
        assert_eq!(
            r("error", "rate limited"),
            WaitReason::Error("rate limited".into())
        );
        assert_eq!(
            r("idle", "session_resumed").label(),
            "session resumed",
            "other reasons are shown verbatim"
        );
        let s = read(&[
            ("@pane_agent", "claude"),
            ("@pane_attention", "notification"),
            ("@pane_started_at", "not a number"),
            ("@pane_prompt_source", "response"),
        ])
        .unwrap();
        assert!(s.attention && s.prompt_is_reply);
        assert_eq!(s.run_started, None);
    }
}
