//! tmux-home's own agent options, `@home_*`, as an `AgentSource`.
//!
//! `tmux-home hook <agent> <event>` (`crate::hook`, via an `AgentAdapter`)
//! writes these per pane. They use the same value vocabulary as
//! tmux-agent-sidebar's `@pane_*` options (see `sidebar`), under
//! tmux-home's own names, so both sets of hooks can run side by side
//! during the migration without fighting over options (daemon spec §5,
//! §11). This source comes first in `SOURCES`; a pane without `@home_*`
//! options falls back to the sidebar's.

use super::sidebar::{Names, read_named};
use super::{AgentSource, AgentState};
use crate::tmux::snapshot::Pane;

/// One `@home_*` option.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// `claude` (the adapter's kind).
    Agent,
    /// `running` / `background` / `waiting` / `idle` / `error`.
    Status,
    /// `notification`, or unset.
    Attention,
    /// `permission_prompt`, `permission_denied`, `elicitation_dialog`,
    /// `session_resumed[_compact]`, … or the error text when status is
    /// `error`.
    WaitReason,
    /// Unix seconds when the current run started.
    RunStarted,
    /// The last prompt (`user`) or reply (`response`).
    Prompt,
    PromptSource,
    /// `Type:id,Type:id`.
    Subagents,
    BgCmd,
    PermissionMode,
    WorktreeName,
    WorktreeBranch,
    SessionId,
    /// Unix seconds of the last hook write (set on every write that sets
    /// anything); how fresh these options are against the sidebar's.
    Updated,
}

impl Key {
    pub const ALL: [Key; 14] = [
        Key::Agent,
        Key::Status,
        Key::Attention,
        Key::WaitReason,
        Key::RunStarted,
        Key::Prompt,
        Key::PromptSource,
        Key::Subagents,
        Key::BgCmd,
        Key::PermissionMode,
        Key::WorktreeName,
        Key::WorktreeBranch,
        Key::SessionId,
        Key::Updated,
    ];

    pub fn option(self) -> &'static str {
        match self {
            Key::Agent => "@home_agent",
            Key::Status => "@home_status",
            Key::Attention => "@home_attention",
            Key::WaitReason => "@home_wait_reason",
            Key::RunStarted => "@home_run_started",
            Key::Prompt => "@home_prompt",
            Key::PromptSource => "@home_prompt_source",
            Key::Subagents => "@home_subagents",
            Key::BgCmd => "@home_bg_cmd",
            Key::PermissionMode => "@home_permission_mode",
            Key::WorktreeName => "@home_worktree_name",
            Key::WorktreeBranch => "@home_worktree_branch",
            Key::SessionId => "@home_session_id",
            Key::Updated => "@home_updated",
        }
    }
}

pub const OPTIONS: &[&str] = &[
    "@home_agent",
    "@home_status",
    "@home_attention",
    "@home_wait_reason",
    "@home_run_started",
    "@home_prompt",
    "@home_prompt_source",
    "@home_subagents",
    "@home_bg_cmd",
    "@home_permission_mode",
    "@home_worktree_name",
    "@home_worktree_branch",
    "@home_session_id",
    "@home_updated",
];

const NAMES: Names = Names {
    agent: "@home_agent",
    status: "@home_status",
    attention: "@home_attention",
    wait_reason: "@home_wait_reason",
    started: "@home_run_started",
    prompt: "@home_prompt",
    prompt_source: "@home_prompt_source",
    subagents: "@home_subagents",
    bg_cmd: "@home_bg_cmd",
    permission_mode: "@home_permission_mode",
    worktree_name: "@home_worktree_name",
    worktree_branch: "@home_worktree_branch",
    session_id: "@home_session_id",
};

pub struct Home;

impl AgentSource for Home {
    fn options(&self) -> &'static [&'static str] {
        OPTIONS
    }

    fn read(&self, pane: &Pane) -> Option<AgentState> {
        read_named(&NAMES, pane)
    }

    fn updated(&self, pane: &Pane) -> Option<u64> {
        pane.agent_opts.get("@home_updated")?.trim().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::pane;
    use crate::agent::{AgentKind, Status, WaitReason, pane_agent, pane_agent_at};

    #[test]
    fn options_match_keys() {
        let keys: Vec<&str> = Key::ALL.iter().map(|k| k.option()).collect();
        assert_eq!(keys, OPTIONS);
    }

    #[test]
    fn reads_home_options() {
        let s = Home
            .read(&pane(
                "%1",
                "node",
                &[
                    ("@home_agent", "claude"),
                    ("@home_status", "waiting"),
                    ("@home_attention", "notification"),
                    ("@home_wait_reason", "permission_prompt"),
                    ("@home_run_started", "1790864748"),
                    ("@home_subagents", "Explore:a1"),
                ],
            ))
            .unwrap();
        assert_eq!((s.kind, s.status), (AgentKind::Claude, Status::Waiting));
        assert_eq!(s.wait_reason, Some(WaitReason::Permission));
        assert_eq!(s.run_started, Some(1790864748));
        assert_eq!(s.subagents, ["Explore"]);
        assert!(s.attention);
        // the sidebar's names mean nothing to this source
        assert_eq!(
            Home.read(&pane("%1", "node", &[("@pane_agent", "claude")])),
            None
        );
    }

    /// Both sources on one pane: `@home_*` wins while it is current; a
    /// stale `@home_*` (tmux-home's hooks no longer firing for this pane)
    /// loses to a fresher sidebar.
    #[test]
    fn stale_home_loses_to_a_fresh_sidebar() {
        let now = 1_790_000_000u64;
        let p = |home_updated: u64, home_sid: &str, pane_started: u64, pane_sid: &str| {
            let (hu, ps) = (home_updated.to_string(), pane_started.to_string());
            let opts = [
                ("@home_agent", "claude"),
                ("@home_status", "running"),
                ("@home_updated", hu.as_str()),
                ("@home_session_id", home_sid),
                ("@pane_agent", "claude"),
                ("@pane_status", "waiting"),
                ("@pane_started_at", ps.as_str()),
                ("@pane_session_id", pane_sid),
            ];
            pane_agent_at(&pane("%1", "node", &opts), now)
                .unwrap()
                .state
                .status
        };
        // same session, home written at or after the sidebar's run start
        assert_eq!(p(now - 5, "s1", now - 5, "s1"), Status::Running);
        assert_eq!(p(now - 5, "s1", now - 6, "s1"), Status::Running);
        // a second or two apart is the same event, not newer
        assert_eq!(p(now - 5, "s1", now - 4, "s1"), Status::Running);
        // the sidebar started a run well after home last wrote
        assert_eq!(p(now - 600, "s1", now - 10, "s1"), Status::Waiting);
        // sessions disagree: home loses once it has been quiet a while
        assert_eq!(p(now - 600, "old", now - 900, "new"), Status::Waiting);
        assert_eq!(p(now - 5, "old", now - 900, "new"), Status::Running);
        // no @home_updated at all (written by nothing current): any
        // sidebar run start is newer
        let opts = [
            ("@home_agent", "claude"),
            ("@home_status", "running"),
            ("@pane_agent", "claude"),
            ("@pane_status", "idle"),
            ("@pane_started_at", "1790000000"),
        ];
        assert_eq!(
            pane_agent_at(&pane("%1", "node", &opts), now)
                .unwrap()
                .state
                .status,
            Status::Idle
        );
    }

    /// `@home_*` wins over `@pane_*` on the same pane; a pane with only
    /// the sidebar's options still falls back to them.
    #[test]
    fn home_first_sidebar_fallback_per_pane() {
        let both = pane(
            "%1",
            "node",
            &[
                ("@home_agent", "claude"),
                ("@home_status", "running"),
                ("@pane_agent", "claude"),
                ("@pane_status", "idle"),
            ],
        );
        assert_eq!(pane_agent(&both).unwrap().state.status, Status::Running);
        let sidebar_only = pane(
            "%2",
            "node",
            &[("@pane_agent", "claude"), ("@pane_status", "waiting")],
        );
        assert_eq!(
            pane_agent(&sidebar_only).unwrap().state.status,
            Status::Waiting
        );
    }
}
