//! Claude Code's hooks as an `AgentAdapter`.
//!
//! Ported from tmux-agent-sidebar (MIT, (c) 2026 hiroppy; see `NOTICE`):
//! `src/adapter/claude/mod.rs` (payload fields), `src/cli/hook.rs` and
//! `src/cli/hook/handlers/{session,run,attention,subagent,
//! status_priority}.rs` (the state changes), `src/cli/hook/context/*`
//! (metadata). Written against `@home_*` instead of `@pane_*`, as pure
//! functions returning `Change`s, and checked against Claude Code
//! 2.1.284's hook payloads, which differ from the sidebar's assumptions in
//! places (noted inline).
//!
//! Precedence: `running > permission > background > waiting > idle`.
//! Handled: SessionStart, UserPromptSubmit, Stop, StopFailure,
//! Notification, PermissionDenied, SessionEnd, SubagentStart,
//! SubagentStop. Not PostToolUse or Task* (daemon spec §5: kept off the
//! tool-call path); so no activity log and no TaskCompleted attention.
//!
//! Subagents share their parent's `$TMUX_PANE`. A payload from inside a
//! subagent carries `agent_id` (and only then); SessionStart, SessionEnd
//! and Notification always come from the main context.

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

/// The payload comes from inside a subagent.
fn from_subagent(p: &Value) -> bool {
    !s(p, "agent_id").is_empty()
}

/// The sidebar's `sanitize_tmux_value` (`|` to a space), with every
/// control character (newline, `\r`, ESC, …) a space too, and a cap: the
/// popup never shows more than `MAX_PROMPT` characters.
fn sanitize(t: &str) -> String {
    t.chars()
        .map(|c| if c == '|' || c.is_control() { ' ' } else { c })
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
        "permission"
            | "permission_prompt"
            | "permission_denied"
            | "elicitation_dialog"
            | "elicitation_url_dialog"
    )
}

/// Notification types that mean Claude is blocked on the user. Every
/// other type (`idle_prompt`, `auth_success`, `elicitation_complete`,
/// `agent_completed`, `quota_auto_resume_*`, …) is metadata only. (The
/// sidebar exempts only `idle_prompt`.)
pub fn notification_waits(t: &str) -> bool {
    matches!(
        t,
        "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input"
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

/// `set_agent_meta`: the agent, and (for the main context only) its
/// permission mode, session id and worktree. The sidebar guards on its
/// subagent list; the payload's `agent_id` says it exactly. The sidebar
/// also records `cwd`; tmux-home reads `pane_current_path` instead.
fn meta(w: &mut Writes, p: &Value) {
    w.set(Key::Agent, "claude");
    if from_subagent(p) {
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

/// A Stop's still-running background tasks (`background_tasks`, absent
/// before Claude Code reported them): whether there are any, and the
/// first shell's command.
fn background_tasks(p: &Value) -> (bool, &str) {
    let live: Vec<&Value> = p
        .get("background_tasks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|t| {
                    !matches!(
                        s(t, "status"),
                        "completed" | "failed" | "killed" | "stopped" | "cancelled"
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let cmd = live
        .iter()
        .find(|t| s(t, "type") == "shell")
        .map_or("", |t| s(t, "command"));
    (!live.is_empty(), cmd)
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
                let source = s(p, "source");
                // subagents never fire SessionStart, so a fresh or cleared
                // session has none; the sidebar keeps the list
                if matches!(source, "startup" | "clear") {
                    w.unset(Key::Subagents);
                }
                match source {
                    "resume" => w.set(Key::WaitReason, "session_resumed"),
                    "compact" => w.set(Key::WaitReason, "session_resumed_compact"),
                    _ => w.unset(Key::WaitReason),
                }
                w.status("idle");
            }
            "UserPromptSubmit" if !from_subagent(p) => {
                meta(&mut w, p);
                w.unset(Key::Attention);
                w.status("running");
                // `source` (2.1.284): the prompt is the user's only from
                // `user` or `sdk`; wakeups and polls start a run but
                // don't replace what the user last asked
                let typed = matches!(
                    p.get("source").and_then(Value::as_str),
                    None | Some("user" | "sdk")
                );
                let prompt = s(p, "prompt");
                if typed && !prompt.is_empty() && !is_system_message(prompt) {
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
                // Background work: Claude Code's own `background_tasks`,
                // or the sidebar's `@pane_bg_cmd` while it runs too.
                let (tasks_live, task_cmd) = background_tasks(p);
                let bg_live = tasks_live || !prior.sidebar_bg_cmd.is_empty();
                let cmd = if task_cmd.is_empty() {
                    prior.sidebar_bg_cmd.as_str()
                } else {
                    task_cmd
                };
                w.set_or_unset(Key::BgCmd, &sanitize(cmd));
                // A subagent still working in the background reports here
                // as a task, and stops with its own SubagentStop; the list
                // (subagents as SubagentStart/Stop saw them) is only
                // stale when nothing is left running.
                if !tasks_live {
                    w.unset(Key::Subagents);
                }
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
                let err = first(p, &["error_type", "error", "error_details"]);
                if !err.is_empty() {
                    w.set(Key::WaitReason, sanitize(err));
                }
                w.status("error");
            }
            "Notification" => {
                meta(&mut w, p);
                let reason = s(p, "notification_type");
                if notification_waits(reason) {
                    let bg_live = !prior.sidebar_bg_cmd.is_empty() || !prior.home_bg_cmd.is_empty();
                    w.status(notification_status(reason, bg_live));
                    w.set(Key::Attention, "notification");
                    w.set(Key::WaitReason, reason);
                }
            }
            "PermissionDenied" => {
                // The model carries on after a denial (auto mode), so the
                // status stays as it is (the sidebar sets `waiting`); the
                // denial and its reason are recorded for the card.
                meta(&mut w, p);
                w.set(Key::Attention, "notification");
                let why = sanitize(s(p, "reason"));
                w.set(
                    Key::WaitReason,
                    if why.is_empty() {
                        "permission_denied".to_string()
                    } else {
                        format!("permission_denied:{why}")
                    },
                );
            }
            "SessionEnd" => {
                // Only the main context ends a session, so this wipes the
                // pane even with subagents listed (the sidebar skips it
                // then, leaving a stale `running`); a payload naming an
                // `agent_id` is still never trusted to.
                if !from_subagent(p) {
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
    use serde_json::json;
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

    type Opts = BTreeMap<&'static str, String>;

    /// `event` with `payload` on a pane holding `start`, with the sidebar's
    /// `@pane_bg_cmd` = `sidebar_bg`; the final options.
    fn run(start: &[(Key, &str)], sidebar_bg: &str, event: &str, payload: &Value) -> Opts {
        let find = |k| {
            start
                .iter()
                .find(|(x, _)| *x == k)
                .map(|(_, v)| v.to_string())
                .unwrap_or_default()
        };
        let prior = Prior {
            subagents: find(Key::Subagents),
            sidebar_bg_cmd: sidebar_bg.into(),
            home_bg_cmd: find(Key::BgCmd),
        };
        apply(start, &Claude.on_hook(event, payload, &prior, NOW))
    }

    fn get(m: &Opts, k: Key) -> Option<&str> {
        m.get(k.option()).map(String::as_str)
    }

    /// The popup's view of these options on a live claude pane.
    fn derived(m: &Opts) -> Option<crate::agent::PaneAgent> {
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
                (Key::Subagents, "Explore:stale"),
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
            Key::Subagents,
        ] {
            assert_eq!(get(&m, k), None, "{k:?}");
        }
        assert_eq!(derived(&m).unwrap().state.status, Status::Idle);
        let m = run(
            &[(Key::Subagents, "Explore:stale")],
            "",
            "SessionStart",
            &fixture!("session_start_clear"),
        );
        assert_eq!(get(&m, Key::Subagents), None, "clear drops subagents");
    }

    #[test]
    fn session_start_resume_and_compact() {
        let m = run(
            &[(Key::Subagents, "Explore:a1")],
            "",
            "SessionStart",
            &fixture!("session_start_resume"),
        );
        assert_eq!(get(&m, Key::WaitReason), Some("session_resumed"));
        assert_eq!(
            get(&m, Key::Subagents),
            Some("Explore:a1"),
            "kept on resume"
        );
        let m = run(&[], "", "SessionStart", &json!({"source": "compact"}));
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

    /// A wakeup or poll starts a run but isn't the user's prompt; `sdk`
    /// is; no `source` (older Claude Code) is taken as the user's.
    #[test]
    fn user_prompt_submit_source() {
        let m = run(
            &[(Key::Prompt, "earlier")],
            "",
            "UserPromptSubmit",
            &fixture!("user_prompt_submit_wakeup"),
        );
        assert_eq!(get(&m, Key::Prompt), Some("earlier"));
        assert_eq!(get(&m, Key::Status), Some("running"));
        for src in ["system", "loop_wakeup", "schedule_wakeup", "poll_event"] {
            let m = run(
                &[],
                "",
                "UserPromptSubmit",
                &json!({"prompt": "x", "source": src}),
            );
            assert_eq!(get(&m, Key::Prompt), None, "{src}");
        }
        let m = run(
            &[],
            "",
            "UserPromptSubmit",
            &json!({"prompt": "x", "source": "sdk"}),
        );
        assert_eq!(get(&m, Key::Prompt), Some("x"));
        let m = run(&[], "", "UserPromptSubmit", &json!({"prompt": "x"}));
        assert_eq!(get(&m, Key::Prompt), Some("x"));
        // no `user_message` fallback: that field doesn't exist
        let m = run(&[], "", "UserPromptSubmit", &json!({"user_message": "x"}));
        assert_eq!(get(&m, Key::Prompt), None);
    }

    #[test]
    fn user_prompt_submit_from_a_subagent_is_ignored() {
        let p = json!({"prompt": "x", "agent_id": "a1", "agent_type": "Explore"});
        assert!(
            Claude
                .on_hook("UserPromptSubmit", &p, &Prior::default(), NOW)
                .is_empty()
        );
    }

    #[test]
    fn stop_without_background_is_idle_and_clears_the_run() {
        let m = run(
            &[
                (Key::Status, "running"),
                (Key::RunStarted, "123"),
                (Key::Subagents, "general-purpose:x,Explore:y"),
                (Key::BgCmd, "old shell"),
            ],
            "",
            "Stop",
            &fixture!("stop"),
        );
        assert_eq!(get(&m, Key::Status), Some("idle"));
        assert_eq!(get(&m, Key::RunStarted), None);
        assert_eq!(get(&m, Key::Subagents), None, "stale subagents cleared");
        assert_eq!(get(&m, Key::BgCmd), None, "no task reported: none left");
        assert_eq!(
            get(&m, Key::Prompt),
            Some("Added the test; all 12 tests pass.")
        );
        assert_eq!(get(&m, Key::PromptSource), Some("response"));
        let a = derived(&m).unwrap();
        assert!(a.state.prompt_is_reply && !a.needs_you());
    }

    /// Claude Code's own `background_tasks`: background, the run's start
    /// kept, the shell's command shown; the subagent list survives while a
    /// background subagent runs.
    #[test]
    fn stop_with_background_tasks() {
        let m = run(
            &[
                (Key::RunStarted, "123"),
                (Key::WaitReason, "x"),
                (Key::Subagents, "Explore:task-0001"),
            ],
            "",
            "Stop",
            &fixture!("stop_background"),
        );
        assert_eq!(get(&m, Key::Status), Some("background"));
        assert_eq!(get(&m, Key::RunStarted), Some("123"));
        assert_eq!(get(&m, Key::WaitReason), None);
        assert_eq!(get(&m, Key::BgCmd), Some("npm run dev"));
        assert_eq!(get(&m, Key::Subagents), Some("Explore:task-0001"));
        let a = derived(&m).unwrap();
        assert_eq!(a.state.status, Status::Background);
        assert_eq!(a.state.bg_cmd.as_deref(), Some("npm run dev"));
        // a subagent alone: background, no command
        let p = json!({"background_tasks": [{"id": "t", "type": "subagent", "status": "running", "description": "d"}]});
        let m = run(&[], "", "Stop", &p);
        assert_eq!(get(&m, Key::Status), Some("background"));
        assert_eq!(get(&m, Key::BgCmd), None);
        // finished tasks don't count; an empty list is none
        for tasks in [
            json!([{"id": "t", "type": "shell", "status": "completed", "command": "make"}]),
            json!([]),
        ] {
            let m = run(
                &[(Key::RunStarted, "1")],
                "",
                "Stop",
                &json!({"background_tasks": tasks}),
            );
            assert_eq!(get(&m, Key::Status), Some("idle"));
            assert_eq!(get(&m, Key::RunStarted), None);
        }
    }

    /// Alongside the sidebar, its `@pane_bg_cmd` counts too.
    #[test]
    fn stop_with_the_sidebars_bg_shell() {
        let m = run(
            &[(Key::RunStarted, "123")],
            "npm run dev",
            "Stop",
            &fixture!("stop"),
        );
        assert_eq!(get(&m, Key::Status), Some("background"));
        assert_eq!(get(&m, Key::RunStarted), Some("123"));
        assert_eq!(get(&m, Key::BgCmd), Some("npm run dev"));
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
        // error_type → error → error_details
        let e = |p: Value| get(&run(&[], "", "StopFailure", &p), Key::WaitReason).map(String::from);
        assert_eq!(
            e(json!({"error_type": "a", "error": "b"})).as_deref(),
            Some("a")
        );
        assert_eq!(e(json!({"error_details": "boom"})).as_deref(), Some("boom"));
        assert_eq!(e(json!({"error_message": "ignored"})), None);
    }

    /// The four blocking types wait, with attention; permission-class
    /// ones even over a live background shell.
    #[test]
    fn notification_types_that_wait() {
        for t in [
            "permission_prompt",
            "elicitation_dialog",
            "elicitation_url_dialog",
            "agent_needs_input",
        ] {
            let p = json!({"notification_type": t, "message": "m"});
            let m = run(&[(Key::Status, "running")], "", "Notification", &p);
            assert_eq!(get(&m, Key::Status), Some("waiting"), "{t}");
            assert_eq!(get(&m, Key::Attention), Some("notification"), "{t}");
            assert_eq!(get(&m, Key::WaitReason), Some(t));
            assert!(derived(&m).unwrap().needs_you(), "{t}");
            let bg = run(&[], "npm run dev", "Notification", &p);
            let want = if t == "agent_needs_input" {
                "background"
            } else {
                "waiting"
            };
            assert_eq!(get(&bg, Key::Status), Some(want), "{t} with a bg shell");
        }
        let m = run(
            &[],
            "",
            "Notification",
            &fixture!("notification_permission"),
        );
        assert_eq!(
            derived(&m).unwrap().state.wait_reason,
            Some(WaitReason::Permission)
        );
        // a background shell the last Stop reported counts too
        let p = json!({"notification_type": "agent_needs_input"});
        let m = run(&[(Key::BgCmd, "npm run dev")], "", "Notification", &p);
        assert_eq!(get(&m, Key::Status), Some("background"));
    }

    /// Everything else is metadata only: status, attention and reason
    /// untouched.
    #[test]
    fn notification_types_that_are_metadata_only() {
        let mut cases: Vec<Value> = [
            "idle_prompt",
            "auth_success",
            "elicitation_complete",
            "elicitation_response",
            "agent_completed",
            "quota_auto_resume_fired",
            "quota_auto_resume_stale",
            "quota_auto_resume_disabled",
            "",
        ]
        .iter()
        .map(|t| json!({"notification_type": t}))
        .collect();
        cases.push(fixture!("notification_idle"));
        cases.push(fixture!("notification_auth"));
        for p in cases {
            let m = run(&[(Key::Status, "idle")], "npm run dev", "Notification", &p);
            assert_eq!(get(&m, Key::Status), Some("idle"), "{p}");
            assert_eq!(get(&m, Key::Attention), None, "{p}");
            assert_eq!(get(&m, Key::WaitReason), None, "{p}");
            assert_eq!(get(&m, Key::Agent), Some("claude"));
        }
    }

    /// The model carries on after a denial: status unchanged; attention
    /// and the reason recorded.
    #[test]
    fn permission_denied_leaves_the_status() {
        for status in ["running", "idle"] {
            let m = run(
                &[(Key::Status, status)],
                "",
                "PermissionDenied",
                &fixture!("permission_denied"),
            );
            assert_eq!(get(&m, Key::Status), Some(status));
            assert_eq!(get(&m, Key::Attention), Some("notification"));
            assert_eq!(
                get(&m, Key::WaitReason),
                Some("permission_denied:classifier")
            );
            let a = derived(&m).unwrap();
            assert_eq!(a.state.wait_reason, Some(WaitReason::PermissionDenied));
            // needs_you ignores attention while running
            assert_eq!(a.needs_you(), status != "running");
        }
        let m = run(&[], "", "PermissionDenied", &json!({}));
        assert_eq!(get(&m, Key::WaitReason), Some("permission_denied"));
    }

    #[test]
    fn session_end_clears_everything() {
        let start: Vec<(Key, &str)> = Key::ALL.iter().map(|k| (*k, "x")).collect();
        let m = run(&start, "", "SessionEnd", &fixture!("session_end"));
        assert!(m.is_empty(), "{m:?}");
        assert!(derived(&m).is_none());
    }

    /// Subagents share the pane: their events carry `agent_id` and must
    /// not overwrite the parent's session metadata; and since only the
    /// main context ends a session, SessionEnd clears even with subagents
    /// still listed (no stale `running`).
    #[test]
    fn subagent_guard() {
        let start = [
            (Key::Agent, "claude"),
            (Key::Status, "running"),
            (Key::SessionId, "parent"),
            (Key::PermissionMode, "default"),
            (Key::Subagents, "Explore:agent-0001"),
        ];
        let m = run(
            &start,
            "",
            "PermissionDenied",
            &fixture!("permission_denied_subagent"),
        );
        assert_eq!(get(&m, Key::SessionId), Some("parent"));
        assert_eq!(get(&m, Key::PermissionMode), Some("default"));
        assert_eq!(get(&m, Key::Status), Some("running"));
        let m = run(
            &start,
            "",
            "StopFailure",
            &json!({"agent_id": "a", "session_id": "child"}),
        );
        assert_eq!(get(&m, Key::SessionId), Some("parent"));
        // the main context's events do update it, subagents or not
        let m = run(&start, "", "Stop", &fixture!("stop"));
        assert_eq!(
            get(&m, Key::SessionId),
            Some("00000000-0000-4000-8000-000000000001")
        );
        let m = run(&start, "", "SessionEnd", &fixture!("session_end"));
        assert!(m.is_empty(), "{m:?}");
        // a SessionEnd naming an agent_id is never trusted to wipe
        let m = run(&start, "", "SessionEnd", &json!({"agent_id": "a"}));
        assert_eq!(m, apply(&start, &[]));
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
                    &json!({"agent_type": "Explore"}),
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
            "elicitation_url_dialog",
        ] {
            assert!(is_permission_wait_reason(r));
            assert_eq!(notification_status(r, true), "waiting");
        }
        for r in ["auth_success", "", "agent_needs_input", "teammate_idle:a"] {
            assert!(!is_permission_wait_reason(r));
            assert_eq!(notification_status(r, true), "background");
            assert_eq!(notification_status(r, false), "waiting");
        }
    }

    #[test]
    fn values_are_capped_and_control_chars_flattened() {
        let p = json!({"prompt": format!("a\nb|c\rd\x1b[31me\tf{}", "x".repeat(5000))});
        let m = run(&[], "", "UserPromptSubmit", &p);
        let v = get(&m, Key::Prompt).unwrap();
        assert!(v.starts_with("a b c d [31me f"), "{v:?}");
        assert!(!v.chars().any(char::is_control));
        assert_eq!(v.chars().count(), MAX_PROMPT);
    }

    #[test]
    fn every_event_is_handled() {
        for e in EVENTS {
            let p = if e.starts_with("Subagent") {
                json!({"agent_type": "T", "agent_id": "i"})
            } else {
                json!({"source": "startup", "notification_type": "permission_prompt"})
            };
            let prior = Prior {
                subagents: if *e == "SubagentStop" {
                    "T:i".into()
                } else {
                    String::new()
                },
                ..Prior::default()
            };
            assert!(!Claude.on_hook(e, &p, &prior, NOW).is_empty(), "{e}");
        }
    }
}
