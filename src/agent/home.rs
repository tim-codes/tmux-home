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
}

impl Key {
    pub const ALL: [Key; 13] = [
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::pane;
    use crate::agent::{AgentKind, Status, WaitReason, pane_agent};

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
