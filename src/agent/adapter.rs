//! The hook seam (daemon spec §5): an agent's hook payload in, changes to
//! the pane's `@home_*` options out. Pure; `crate::hook` reads the pane's
//! prior state and applies the changes. Adding an agent is one module
//! implementing `AgentAdapter` plus an entry in `ADAPTERS`.

use super::AgentKind;
use super::home::Key;
use serde_json::Value;

/// One change to a pane's `@home_*` options, applied in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Set(Key, String),
    Unset(Key),
}

/// What a hook needs to know about the pane before it writes: the parts
/// of its state the rules depend on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prior {
    /// `@home_subagents` (`Type:id,…`); non-empty while subagents run.
    pub subagents: String,
    /// tmux-agent-sidebar's `@pane_bg_cmd`: its PostToolUse hook's live
    /// background shell, when the sidebar runs alongside.
    pub sidebar_bg_cmd: String,
    /// `@home_bg_cmd`: the background shell the last Stop reported.
    pub home_bg_cmd: String,
    /// `@home_status`, `@home_attention`, `@home_wait_reason`: what a
    /// resolution event (a tool finishing after a permission prompt) needs
    /// to decide whether there is a wait to end.
    pub status: String,
    pub attention: String,
    pub wait_reason: String,
}

pub trait AgentAdapter: Sync {
    /// The agent this adapter speaks for; the name used on the command
    /// line (`tmux-home hook <name> <event>`) is `kind().name()`.
    fn kind(&self) -> AgentKind;

    /// The events `on_hook` acts on; any other is a no-op, decided before
    /// tmux is asked anything.
    fn events(&self) -> &'static [&'static str];

    /// Events whose payload `on_hook` never looks at (per-tool-call
    /// events, whose payload carries the tool's whole output): the hook
    /// drains stdin without parsing it and passes an empty object.
    fn payload_unused(&self, _event: &str) -> bool {
        false
    }

    /// The changes `event` (with its stdin `payload`) makes to a pane in
    /// state `prior`; `now` is Unix seconds. Unknown events: no changes.
    fn on_hook(&self, event: &str, payload: &Value, prior: &Prior, now: u64) -> Vec<Change>;
}

pub const ADAPTERS: &[&dyn AgentAdapter] = &[&super::claude::Claude];

/// The adapter for `name` (`claude`).
pub fn adapter(name: &str) -> Option<&'static dyn AgentAdapter> {
    ADAPTERS.iter().copied().find(|a| a.kind().name() == name)
}

/// Accumulates changes, tracking the subagent list as it changes.
pub struct Writes {
    pub changes: Vec<Change>,
    subagents: String,
}

impl Writes {
    pub fn new(prior: &Prior) -> Writes {
        Writes {
            changes: Vec::new(),
            subagents: prior.subagents.clone(),
        }
    }

    pub fn set(&mut self, k: Key, v: impl Into<String>) {
        let v = v.into();
        if k == Key::Subagents {
            self.subagents = v.clone();
        }
        self.changes.push(Change::Set(k, v));
    }

    pub fn unset(&mut self, k: Key) {
        if k == Key::Subagents {
            self.subagents.clear();
        }
        self.changes.push(Change::Unset(k));
    }

    /// Set when non-empty, else unset.
    pub fn set_or_unset(&mut self, k: Key, v: &str) {
        if v.is_empty() {
            self.unset(k)
        } else {
            self.set(k, v)
        }
    }

    pub fn subagents(&self) -> &str {
        &self.subagents
    }

    /// The sidebar's `set_status`: `running` and `idle` also clear
    /// attention.
    pub fn status(&mut self, s: &str) {
        self.set(Key::Status, s);
        if matches!(s, "running" | "idle") {
            self.unset(Key::Attention);
        }
    }

    /// Every `@home_*` option unset (the SessionEnd teardown).
    pub fn clear_all(&mut self) {
        for k in Key::ALL {
            self.unset(k);
        }
    }
}

/// `changes`, stamped with `@home_updated = now` when they set anything
/// (a pure teardown leaves no stamp behind).
pub fn stamped(mut changes: Vec<Change>, now: u64) -> Vec<Change> {
    if changes.iter().any(|c| matches!(c, Change::Set(..))) {
        changes.push(Change::Set(Key::Updated, now.to_string()));
    }
    changes
}

/// The value of `changes` applied to an empty pane (or to `start`), for
/// tests: the final value of each key.
pub fn apply(
    start: &[(Key, &str)],
    changes: &[Change],
) -> std::collections::BTreeMap<&'static str, String> {
    let mut m: std::collections::BTreeMap<&'static str, String> = start
        .iter()
        .map(|(k, v)| (k.option(), v.to_string()))
        .collect();
    for c in changes {
        match c {
            Change::Set(k, v) => {
                m.insert(k.option(), v.clone());
            }
            Change::Unset(k) => {
                m.remove(k.option());
            }
        }
    }
    m
}
