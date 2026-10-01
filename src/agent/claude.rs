//! Claude Code's hooks as an `AgentAdapter`.
//!
//! Ported from tmux-agent-sidebar (MIT, (c) 2026 hiroppy; see `NOTICE`):
//! `src/adapter/claude/mod.rs` (payload fields), `src/cli/hook.rs` and
//! `src/cli/hook/handlers/{session,run,attention,subagent,
//! status_priority}.rs` (the state changes), `src/cli/hook/context/*`
//! (metadata, the subagent guard). Written against `@home_*` instead of
//! `@pane_*`, as pure functions returning `Change`s.
//!
//! Precedence: `running > permission > background > waiting > idle`.
//! Handled: SessionStart, UserPromptSubmit, Stop, StopFailure,
//! Notification, PermissionDenied, SessionEnd, SubagentStart,
//! SubagentStop. Not PostToolUse or Task* (daemon spec §5: kept off the
//! tool-call path); so no activity log and no TaskCompleted attention.

use super::AgentKind;
use super::adapter::{AgentAdapter, Change, Prior, Writes};
use super::home::Key;
use super::sidebar::MAX_PROMPT;
use serde_json::Value;

pub struct Claude;

/// The events this adapter handles, in Claude Code's names.
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "Notification",
    "PermissionDenied",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
];

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// The first non-empty of these string fields.
fn first<'a>(v: &'a Value, ks: &[&str]) -> &'a str {
    ks.iter()
        .map(|k| s(v, k))
        .find(|x| !x.is_empty())
        .unwrap_or("")
}

/// The sidebar's `sanitize_tmux_value` (newlines and `|` to spaces), plus
/// a cap: the popup never shows more than `MAX_PROMPT` characters.
fn sanitize(t: &str) -> String {
    t.replace(['\n', '|'], " ")
        .chars()
        .take(MAX_PROMPT)
        .collect()
}

/// Injected text, not something the user typed (`is_system_message`).
fn is_system_message(t: &str) -> bool {
    t.contains("<task-notification>") || t.contains("<system-reminder>") || t.contains("<task-")
}

/// Wait reasons that need the user to act: they stay `waiting` even with
/// a live background shell (`status_priority::is_permission_wait_reason`).
pub fn is_permission_wait_reason(r: &str) -> bool {
    matches!(
        r,
        "permission" | "permission_prompt" | "permission_denied" | "elicitation_dialog"
    )
}

/// Status Stop lands in (`resolve_stop_status`).
pub fn stop_status(bg_live: bool) -> &'static str {
    if bg_live { "background" } else { "idle" }
}

/// Status a Notification lands in (`resolve_notification_status`).
pub fn notification_status(reason: &str, bg_live: bool) -> &'static str {
    if bg_live && !is_permission_wait_reason(reason) {
        "background"
    } else {
        "waiting"
    }
}

/// `set_agent_meta`: the agent, and (unless subagents are running) its
/// permission mode, session id and worktree. The sidebar also records
/// `cwd`; tmux-home reads `pane_current_path` instead.
fn meta(w: &mut Writes, p: &Value) {
    w.set(Key::Agent, "claude");
    if !w.writes_allowed() {
        return;
    }
    let mode = s(p, "permission_mode");
    if !mode.is_empty() {
        w.set(Key::PermissionMode, mode);
    }
    w.set_or_unset(Key::SessionId, s(p, "session_id"));
    let wt = p.get("worktree").filter(|v| v.is_object());
    let (name, branch) = wt.map_or(("", ""), |o| (s(o, "name"), s(o, "branch")));
    w.set_or_unset(Key::WorktreeName, name);
    w.set_or_unset(Key::WorktreeBranch, branch);
}

/// `clear_run_state`.
fn clear_run(w: &mut Writes) {
    w.unset(Key::RunStarted);
    w.unset(Key::WaitReason);
}

/// The background shell `prior` knows of, mirrored into `@home_bg_cmd` so
/// the popup can show it; whether one is live.
fn bg(w: &mut Writes, prior: &Prior) -> bool {
    w.set_or_unset(Key::BgCmd, &prior.bg_cmd);
    !prior.bg_cmd.is_empty()
}

impl AgentAdapter for Claude {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn events(&self) -> &'static [&'static str] {
        EVENTS
    }

    fn on_hook(&self, event: &str, p: &Value, prior: &Prior, now: u64) -> Vec<Change> {
        let mut w = Writes::new(prior);
        match event {
            "SessionStart" => {
                meta(&mut w, p);
                w.unset(Key::Attention);
                clear_run(&mut w);
                w.unset(Key::Prompt);
                w.unset(Key::PromptSource);
                // the subagent list is kept: a subagent's own SessionStart
                // must not drop its parent's marker
                match s(p, "source") {
                    "resume" => w.set(Key::WaitReason, "session_resumed"),
                    "compact" => w.set(Key::WaitReason, "session_resumed_compact"),
                    _ => w.unset(Key::WaitReason),
                }
                w.status("idle");
            }
            "UserPromptSubmit" => {
                meta(&mut w, p);
                w.unset(Key::Attention);
                w.status("running");
                let prompt = first(p, &["prompt", "user_message"]);
                if !prompt.is_empty() && !is_system_message(prompt) {
                    w.set(Key::Prompt, sanitize(prompt));
                    w.set(Key::PromptSource, "user");
                }
                w.set(Key::RunStarted, now.to_string());
                w.unset(Key::WaitReason);
            }
            "Stop" => {
                meta(&mut w, p);
                w.unset(Key::Attention);
                let msg = s(p, "last_assistant_message");
                if !msg.is_empty() {
                    w.set(Key::Prompt, sanitize(msg));
                    w.set(Key::PromptSource, "response");
                }
                let bg_live = bg(&mut w, prior);
                // Task subagents are synchronous: at the parent's Stop none
                // can still run, so a leftover list is stale
                w.unset(Key::Subagents);
                if bg_live {
                    // the run goes on in the background: keep its start
                    w.unset(Key::WaitReason);
                } else {
                    clear_run(&mut w);
                }
                w.status(stop_status(bg_live));
            }
            "StopFailure" => {
                meta(&mut w, p);
                w.unset(Key::Attention);
                clear_run(&mut w);
                let err = first(
                    p,
                    &["error_type", "error", "error_message", "error_details"],
                );
                if !err.is_empty() {
                    w.set(Key::WaitReason, sanitize(err));
                }
                w.status("error");
            }
            "Notification" => {
                meta(&mut w, p);
                let reason = s(p, "notification_type");
                // "waiting for your input" after 60 s idle: metadata only
                if reason != "idle_prompt" {
                    let bg_live = bg(&mut w, prior);
                    w.status(notification_status(reason, bg_live));
                    w.set(Key::Attention, "notification");
                    w.set_or_unset(Key::WaitReason, reason);
                }
            }
            "PermissionDenied" => {
                meta(&mut w, p);
                w.status("waiting");
                w.set(Key::Attention, "notification");
                w.set(Key::WaitReason, "permission_denied");
            }
            "SessionEnd" => {
                // The subagent guard: subagents share the parent's pane, so
                // a SessionEnd while any run is (almost certainly) a
                // child's; wiping the pane would clobber the live parent.
                // A payload naming an `agent_id` is a subagent's outright.
                if w.writes_allowed() && s(p, "agent_id").is_empty() {
                    w.clear_all();
                }
            }
            "SubagentStart" => {
                let (ty, id) = (s(p, "agent_type"), s(p, "agent_id"));
                if !ty.is_empty() && !id.is_empty() {
                    let cur = w.subagents().to_string();
                    let entry = format!("{ty}:{id}");
                    let v = if cur.is_empty() {
                        entry
                    } else {
                        format!("{cur},{entry}")
                    };
                    w.set(Key::Subagents, v);
                }
            }
            "SubagentStop" => {
                let (ty, id) = (s(p, "agent_type"), s(p, "agent_id"));
                if !ty.is_empty() && !id.is_empty() {
                    let cur = w.subagents().to_string();
                    if let Some(v) = remove_subagent(&cur, id) {
                        w.set_or_unset(Key::Subagents, &v);
                    }
                }
            }
            _ => {}
        }
        w.changes
    }
}

/// `cur` without the entry for `id`; `None` if it has none.
fn remove_subagent(cur: &str, id: &str) -> Option<String> {
    let needle = format!(":{id}");
    let items: Vec<&str> = cur.split(',').filter(|e| !e.is_empty()).collect();
    let i = items.iter().position(|e| e.ends_with(&needle))?;
    let rest: Vec<&str> = items
        .iter()
        .enumerate()
        .filter(|&(j, _)| j != i)
        .map(|(_, e)| *e)
        .collect();
    Some(rest.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::adapter::apply;
    use crate::agent::tests::pane;
    use crate::agent::{Status, WaitReason, pane_agent};
    use std::collections::BTreeMap;

    macro_rules! fixture {
        ($name:literal) => {
            serde_json::from_str::<Value>(include_str!(concat!(
                "../../tests/fixtures/claude/",
                $name,
                ".json"
            )))
            .unwrap()
        };
    }

    const NOW: u64 = 1_790_000_000;

    /// `event` with `payload` on a pane holding `start`; the final options.
    fn run(
        start: &[(Key, &str)],
        bg_cmd: &str,
        event: &str,
        payload: &Value,
    ) -> BTreeMap<&'static str, String> {
        let prior = Prior {
            subagents: start
                .iter()
                .find(|(k, _)| *k == Key::Subagents)
                .map(|(_, v)| v.to_string())
                .unwrap_or_default(),
            bg_cmd: bg_cmd.into(),
        };
        apply(start, &Claude.on_hook(event, payload, &prior, NOW))
    }

    fn get<'a>(m: &'a BTreeMap<&'static str, String>, k: Key) -> Option<&'a str> {
        m.get(k.option()).map(String::as_str)
    }

    /// The popup's view of these options on a live claude pane.
    fn derived(m: &BTreeMap<&'static str, String>) -> Option<crate::agent::PaneAgent> {
        let opts: Vec<(&str, &str)> = m.iter().map(|(k, v)| (*k, v.as_str())).collect();
        pane_agent(&pane("%1", "2.1.283", &opts))
    }

    #[test]
    fn session_start() {
        let m = run(
            &[
                (Key::Prompt, "old"),
                (Key::WaitReason, "rate_limit"),
                (Key::RunStarted, "5"),
                (Key::Attention, "notification"),
            ],
            "",
            "SessionStart",
            &fixture!("session_start"),
        );
        assert_eq!(get(&m, Key::Agent), Some("claude"));
        assert_eq!(get(&m, Key::Status), Some("idle"));
        assert_eq!(get(&m, Key::PermissionMode), Some("default"));
        assert_eq!(
            get(&m, Key::SessionId),
            Some("00000000-0000-4000-8000-000000000001")
        );
        for k in [
            Key::Prompt,
            Key::WaitReason,
            Key::RunStarted,
            Key::Attention,
        ] {
            assert_eq!(get(&m, k), None, "{k:?}");
        }
        assert_eq!(derived(&m).unwrap().state.status, Status::Idle);
    }

    #[test]
    fn session_start_resume_and_kept_subagents() {
        let m = run(
            &[(Key::Subagents, "Explore:a1")],
            "",
            "SessionStart",
            &fixture!("session_start_resume"),
        );
        assert_eq!(get(&m, Key::WaitReason), Some("session_resumed"));
        assert_eq!(get(&m, Key::Subagents), Some("Explore:a1"));
        // subagents running: the session's metadata is left alone
        assert_eq!(get(&m, Key::SessionId), None);
        let p = serde_json::json!({"source": "compact"});
        let m = run(&[], "", "SessionStart", &p);
        assert_eq!(get(&m, Key::WaitReason), Some("session_resumed_compact"));
    }

    #[test]
    fn user_prompt_submit() {
        let m = run(
            &[
                (Key::WaitReason, "permission_prompt"),
                (Key::Attention, "notification"),
            ],
            "npm run dev",
            "UserPromptSubmit",
            &fixture!("user_prompt_submit"),
        );
        assert_eq!(get(&m, Key::Status), Some("running"));
        assert_eq!(
            get(&m, Key::Prompt),
            Some("Add a unit test for the parser and run it then report back")
        );
        assert_eq!(get(&m, Key::PromptSource), Some("user"));
        assert_eq!(get(&m, Key::RunStarted), Some("1790000000"));
        assert_eq!(get(&m, Key::WaitReason), None);
        assert_eq!(get(&m, Key::Attention), None);
        let a = derived(&m).unwrap();
        assert_eq!(a.state.status, Status::Running);
        assert_eq!(a.state.run_started, Some(NOW));
        assert!(!a.needs_you());
    }

    #[test]
    fn user_prompt_submit_skips_system_messages() {
        let m = run(
            &[(Key::Prompt, "earlier")],
            "",
            "UserPromptSubmit",
            &fixture!("user_prompt_submit_system"),
        );
        assert_eq!(get(&m, Key::Prompt), Some("earlier"));
        assert_eq!(get(&m, Key::Status), Some("running"));
    }

    #[test]
    fn stop_without_bg_shell_is_idle_and_clears_the_run() {
        let m = run(
            &[
                (Key::Status, "running"),
                (Key::RunStarted, "123"),
                (Key::Subagents, "general-purpose:x,Explore:y"),
            ],
            "",
            "Stop",
            &fixture!("stop"),
        );
        assert_eq!(get(&m, Key::Status), Some("idle"));
        assert_eq!(get(&m, Key::RunStarted), None);
        assert_eq!(get(&m, Key::Subagents), None, "stale subagents cleared");
        assert_eq!(
            get(&m, Key::Prompt),
            Some("Added the test; all 12 tests pass.")
        );
        assert_eq!(get(&m, Key::PromptSource), Some("response"));
        let a = derived(&m).unwrap();
        assert!(a.state.prompt_is_reply && !a.needs_you());
    }

    /// started_at is kept at Stop while a background shell is live.
    #[test]
    fn stop_with_bg_shell_is_background_and_keeps_the_start() {
        let m = run(
            &[(Key::RunStarted, "123"), (Key::WaitReason, "x")],
            "npm run dev",
            "Stop",
            &fixture!("stop"),
        );
        assert_eq!(get(&m, Key::Status), Some("background"));
        assert_eq!(get(&m, Key::RunStarted), Some("123"));
        assert_eq!(get(&m, Key::WaitReason), None);
        assert_eq!(get(&m, Key::BgCmd), Some("npm run dev"));
        assert_eq!(derived(&m).unwrap().state.status, Status::Background);
    }

    #[test]
    fn stop_failure_is_error_and_clears_the_start() {
        let m = run(
            &[(Key::RunStarted, "123"), (Key::Attention, "notification")],
            "npm run dev",
            "StopFailure",
            &fixture!("stop_failure"),
        );
        assert_eq!(get(&m, Key::Status), Some("error"));
        assert_eq!(get(&m, Key::WaitReason), Some("rate_limit"));
        assert_eq!(
            get(&m, Key::RunStarted),
            None,
            "cleared even with a bg shell"
        );
        assert_eq!(get(&m, Key::Attention), None);
        let a = derived(&m).unwrap();
        assert_eq!(
            a.state.wait_reason,
            Some(WaitReason::Error("rate_limit".into()))
        );
        assert!(a.needs_you());
        // legacy field names
        let m = run(
            &[],
            "",
            "StopFailure",
            &serde_json::json!({"error_details": "boom"}),
        );
        assert_eq!(get(&m, Key::WaitReason), Some("boom"));
    }

    #[test]
    fn notification_permission_is_waiting_even_with_bg_shell() {
        for bg in ["", "npm run dev"] {
            let m = run(
                &[(Key::Status, "running")],
                bg,
                "Notification",
                &fixture!("notification_permission"),
            );
            assert_eq!(get(&m, Key::Status), Some("waiting"), "bg={bg:?}");
            assert_eq!(get(&m, Key::Attention), Some("notification"));
            assert_eq!(get(&m, Key::WaitReason), Some("permission_prompt"));
            let a = derived(&m).unwrap();
            assert_eq!(a.state.wait_reason, Some(WaitReason::Permission));
            assert!(a.needs_you());
        }
    }

    #[test]
    fn notification_soft_reason_yields_to_bg_shell() {
        let p = fixture!("notification_auth");
        let m = run(&[], "cargo test", "Notification", &p);
        assert_eq!(get(&m, Key::Status), Some("background"));
        assert!(derived(&m).unwrap().needs_you(), "attention, not running");
        let m = run(&[], "", "Notification", &p);
        assert_eq!(get(&m, Key::Status), Some("waiting"));
    }

    /// idle_prompt is metadata only: status, attention, reason untouched.
    #[test]
    fn notification_idle_prompt_is_meta_only() {
        let m = run(
            &[(Key::Status, "idle")],
            "",
            "Notification",
            &fixture!("notification_idle"),
        );
        assert_eq!(get(&m, Key::Status), Some("idle"));
        assert_eq!(get(&m, Key::Attention), None);
        assert_eq!(get(&m, Key::WaitReason), None);
        assert_eq!(get(&m, Key::Agent), Some("claude"));
    }

    #[test]
    fn permission_denied() {
        let m = run(
            &[(Key::Status, "running")],
            "",
            "PermissionDenied",
            &fixture!("permission_denied"),
        );
        assert_eq!(get(&m, Key::Status), Some("waiting"));
        assert_eq!(get(&m, Key::Attention), Some("notification"));
        assert_eq!(get(&m, Key::WaitReason), Some("permission_denied"));
        let a = derived(&m).unwrap();
        assert_eq!(a.state.wait_reason, Some(WaitReason::PermissionDenied));
        assert!(a.needs_you());
    }

    #[test]
    fn session_end_clears_everything() {
        let start: Vec<(Key, &str)> = Key::ALL
            .iter()
            .filter(|k| **k != Key::Subagents)
            .map(|k| (*k, "x"))
            .collect();
        let m = run(&start, "", "SessionEnd", &fixture!("session_end"));
        assert!(m.is_empty(), "{m:?}");
        assert!(derived(&m).is_none());
    }

    /// A subagent's SessionEnd must not wipe its parent.
    #[test]
    fn subagent_guard() {
        let start = [
            (Key::Agent, "claude"),
            (Key::Status, "running"),
            (Key::RunStarted, "123"),
            (Key::Subagents, "Explore:agent-0001"),
        ];
        let m = run(&start, "", "SessionEnd", &fixture!("session_end"));
        assert_eq!(m, apply(&start, &[]), "untouched while subagents run");
        // a payload carrying agent_id is a subagent's, list or no list
        let start = [(Key::Agent, "claude"), (Key::Status, "running")];
        let m = run(&start, "", "SessionEnd", &fixture!("session_end_subagent"));
        assert_eq!(m, apply(&start, &[]));
        // and metadata from events during a subagent run is left alone
        let m = run(
            &[(Key::Subagents, "Explore:a"), (Key::SessionId, "parent")],
            "",
            "UserPromptSubmit",
            &fixture!("user_prompt_submit"),
        );
        assert_eq!(get(&m, Key::SessionId), Some("parent"));
    }

    #[test]
    fn subagent_start_and_stop() {
        let m = run(&[], "", "SubagentStart", &fixture!("subagent_start"));
        assert_eq!(get(&m, Key::Subagents), Some("Explore:agent-0001"));
        let m = run(
            &[(Key::Subagents, "Plan:p1")],
            "",
            "SubagentStart",
            &fixture!("subagent_start"),
        );
        assert_eq!(get(&m, Key::Subagents), Some("Plan:p1,Explore:agent-0001"));
        assert_eq!(derived(&m), None, "subagents alone aren't an agent");
        let m = run(
            &[(Key::Subagents, "Plan:p1,Explore:agent-0001")],
            "",
            "SubagentStop",
            &fixture!("subagent_stop"),
        );
        assert_eq!(get(&m, Key::Subagents), Some("Plan:p1"));
        let m = run(
            &[(Key::Subagents, "Explore:agent-0001")],
            "",
            "SubagentStop",
            &fixture!("subagent_stop"),
        );
        assert_eq!(get(&m, Key::Subagents), None);
        // unknown id, or no id: nothing
        let m = run(
            &[(Key::Subagents, "Plan:p1")],
            "",
            "SubagentStop",
            &fixture!("subagent_stop"),
        );
        assert_eq!(get(&m, Key::Subagents), Some("Plan:p1"));
        assert!(
            Claude
                .on_hook(
                    "SubagentStart",
                    &serde_json::json!({"agent_type": "Explore"}),
                    &Prior::default(),
                    NOW
                )
                .is_empty()
        );
    }

    #[test]
    fn unknown_events_change_nothing() {
        for e in ["PostToolUse", "TaskCompleted", "TaskCreated", "Bogus", ""] {
            assert!(
                Claude
                    .on_hook(e, &fixture!("stop"), &Prior::default(), NOW)
                    .is_empty(),
                "{e}"
            );
        }
    }

    /// running > permission > background > waiting > idle.
    #[test]
    fn precedence() {
        assert_eq!(stop_status(true), "background");
        assert_eq!(stop_status(false), "idle");
        for r in [
            "permission",
            "permission_prompt",
            "permission_denied",
            "elicitation_dialog",
        ] {
            assert!(is_permission_wait_reason(r));
            assert_eq!(notification_status(r, true), "waiting");
        }
        for r in ["auth_success", "", "session_resumed", "teammate_idle:a"] {
            assert!(!is_permission_wait_reason(r));
            assert_eq!(notification_status(r, true), "background");
            assert_eq!(notification_status(r, false), "waiting");
        }
    }

    #[test]
    fn long_values_are_capped_and_flattened() {
        let p = serde_json::json!({"prompt": format!("a\nb|{}", "x".repeat(5000))});
        let m = run(&[], "", "UserPromptSubmit", &p);
        let v = get(&m, Key::Prompt).unwrap();
        assert!(v.starts_with("a b "));
        assert_eq!(v.chars().count(), MAX_PROMPT);
    }

    #[test]
    fn every_event_is_handled() {
        for e in EVENTS {
            let p = serde_json::json!({"agent_type": "T", "agent_id": "i", "source": "startup"});
            let prior = Prior {
                subagents: if *e == "SubagentStop" {
                    "T:i".into()
                } else {
                    String::new()
                },
                bg_cmd: String::new(),
            };
            let p = if *e == "SessionEnd" {
                serde_json::json!({})
            } else {
                p
            };
            let start = [(Key::Subagents, prior.subagents.as_str())];
            let c = Claude.on_hook(e, &p, &prior, NOW);
            assert!(!c.is_empty(), "{e}");
            let _ = apply(&start, &c);
        }
    }
}
