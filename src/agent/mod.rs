//! Agent awareness, read-only (SPEC §7). Pure: nothing here talks to tmux.
//!
//! The seam: the snapshot carries each pane's raw agent options
//! (`Pane::agent_opts`, the names in `OPTIONS`). An `AgentSource` turns
//! those into an `AgentState`, and decides whether the agent still looks
//! alive. The popup (and `ops`) only ever see `AgentState`, `PaneAgent`,
//! `WindowAgents` and `Tally`; they never read an option by name.
//!
//! Two sources, in order: tmux-home's own `@home_*` options (`home`,
//! written by `tmux-home hook`, through an `adapter::AgentAdapter` such as
//! `claude`), then tmux-agent-sidebar's `@pane_*` options (`sidebar`) as
//! the fallback for panes tmux-home's hooks haven't seen. `OPTIONS` is the
//! union of their names. Nothing in the UI knows which source answered.

pub mod adapter;
pub mod claude;
pub mod home;
pub mod sidebar;

use crate::tmux::snapshot::Pane;
use std::sync::LazyLock;

/// Sources in priority order: the first that recognises a pane wins.
pub const SOURCES: &[&dyn AgentSource] = &[&home::Home, &sidebar::Sidebar];

/// Every pane option any source reads, each once, in source order; the
/// snapshot fetches these.
pub static OPTIONS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let mut v: Vec<&'static str> = Vec::new();
    for o in SOURCES.iter().flat_map(|s| s.options()) {
        if !v.contains(o) {
            v.push(o);
        }
    }
    v
});

/// One provider of agent state.
pub trait AgentSource: Sync {
    /// The pane options this source reads.
    fn options(&self) -> &'static [&'static str];

    /// The pane's agent state, if this source has any for it.
    fn read(&self, pane: &Pane) -> Option<AgentState>;

    /// Staleness check: whether the agent the options describe still looks
    /// alive. The options are only cleaned up by the agent's own exit
    /// hook; a crash or `kill -9` leaves them behind on a pane that has
    /// moved on (back at its shell prompt, or running something else).
    fn looks_alive(&self, pane: &Pane, state: &AgentState) -> bool {
        state.kind.looks_alive(&pane.current_command)
    }

    /// Unix seconds this source is known to have last written the pane's
    /// options, if it can tell; for choosing between sources.
    fn updated(&self, _pane: &Pane) -> Option<u64> {
        None
    }
}

/// A later source must be this many seconds newer to win on time alone:
/// both sets of hooks fire on the same events, a second or so apart.
pub const NEWER_BY: u64 = 2;

/// A source whose session ID disagrees with a later source's, and that
/// hasn't written for this long, has stopped hearing from the agent.
pub const SESSION_GRACE: u64 = 30;

/// Whether `later`'s reading of a pane should replace `best`'s (from an
/// earlier source): it is clearly newer, or `best` disagrees on the
/// session and has gone quiet. `now` is Unix seconds.
pub fn later_wins(
    best: (&AgentState, Option<u64>),
    later: (&AgentState, Option<u64>),
    now: u64,
) -> bool {
    let best_t = best.1.unwrap_or(0);
    if later.1.is_some_and(|t| t > best_t + NEWER_BY) {
        return true;
    }
    match (&best.0.session_id, &later.0.session_id) {
        (Some(a), Some(b)) if a != b => now.saturating_sub(best_t) > SESSION_GRACE,
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    OpenCode,
    Other(String),
}

impl AgentKind {
    pub fn parse(s: &str) -> AgentKind {
        match s {
            "claude" => AgentKind::Claude,
            "codex" => AgentKind::Codex,
            "opencode" => AgentKind::OpenCode,
            o => AgentKind::Other(o.to_string()),
        }
    }

    /// Whether a pane running `cmd` (its `pane_current_command`) still runs
    /// this agent. Claude is checked positively: it runs as its version
    /// (`2.1.283`, the name of its versioned binary), or as `claude` or
    /// `node` (npm installs); anything else means it has gone. Other kinds
    /// are alive unless the pane is back at a shell.
    pub fn looks_alive(&self, cmd: &str) -> bool {
        match self {
            AgentKind::Claude => matches!(cmd, "claude" | "node") || is_version(cmd),
            _ => !crate::ops::is_shell(cmd),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::OpenCode => "opencode",
            AgentKind::Other(s) if s.is_empty() => "agent",
            AgentKind::Other(s) => s,
        }
    }
}

/// `^\d+\.\d+\.\d+$`.
fn is_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Agent status. The derived order is urgency: a window shows its most
/// urgent pane (error > waiting > running > background > idle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Unknown,
    Idle,
    Background,
    Running,
    Waiting,
    Error,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Error,
        Status::Waiting,
        Status::Running,
        Status::Background,
        Status::Idle,
        Status::Unknown,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Status::Unknown => "unknown",
            Status::Idle => "idle",
            Status::Background => "background",
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Error => "error",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Status::Unknown => "·",
            Status::Idle => "○",
            Status::Background => "◎",
            Status::Running => "●",
            Status::Waiting => "◐",
            Status::Error => "✕",
        }
    }

    /// Mid-run: closing the window would interrupt it.
    pub fn working(self) -> bool {
        matches!(self, Status::Running | Status::Waiting)
    }
}

/// Why an agent is waiting (or what went wrong).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// A permission prompt is open.
    Permission,
    /// A permission was denied (by a rule or the user); the run stopped.
    PermissionDenied,
    /// A question to the user (`elicitation_dialog`).
    Question,
    /// A teammate went idle; its name.
    TeammateIdle(String),
    /// The error text of a failed run.
    Error(String),
    /// Anything else, verbatim (`session_resumed`, …).
    Other(String),
}

impl WaitReason {
    pub fn label(&self) -> String {
        match self {
            WaitReason::Permission => "permission".into(),
            WaitReason::PermissionDenied => "permission denied".into(),
            WaitReason::Question => "question".into(),
            WaitReason::TeammateIdle(n) => format!("teammate idle: {n}"),
            WaitReason::Error(e) => format!("error: {e}"),
            WaitReason::Other(o) => o.replace('_', " "),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub name: String,
    pub branch: String,
}

/// What the UI knows about one pane's agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentState {
    pub kind: AgentKind,
    pub status: Status,
    /// The agent asked for attention (a notification, permission denied,
    /// a teammate going idle) and hasn't been answered yet.
    pub attention: bool,
    pub wait_reason: Option<WaitReason>,
    /// Unix seconds when the current run started; `None` between runs.
    /// Display time is computed from it at draw time, so the snapshot
    /// doesn't change every second.
    pub run_started: Option<u64>,
    pub prompt: Option<String>,
    /// `prompt` is the agent's last reply rather than the user's prompt.
    pub prompt_is_reply: bool,
    /// Active subagents, by type.
    pub subagents: Vec<String>,
    pub bg_cmd: Option<String>,
    pub permission_mode: Option<String>,
    pub worktree: Option<Worktree>,
    pub session_id: Option<String>,
}

/// A pane's agent and whether it looks stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneAgent {
    pub pane: String,
    pub state: AgentState,
    pub stale: bool,
}

impl PaneAgent {
    /// NEEDS YOU: waiting or error, or a pending notification on an agent
    /// that isn't running (the sidebar's TaskCompleted hook flags attention
    /// mid-run without changing the status); never when stale.
    pub fn needs_you(&self) -> bool {
        !self.stale
            && (matches!(self.state.status, Status::Waiting | Status::Error)
                || (self.state.attention && self.state.status != Status::Running))
    }

    pub fn live(&self) -> bool {
        !self.stale
    }
}

/// The agent of `pane`, from the freshest source that has one. Sidebar panes
/// (`@pane_role=sidebar`) are views, never agents.
pub fn pane_agent(pane: &Pane) -> Option<PaneAgent> {
    pane_agent_at(pane, now())
}

/// `pane_agent` at time `now`: the first source with state for the pane,
/// unless a later one's is fresher (`later_wins`).
pub fn pane_agent_at(pane: &Pane, now: u64) -> Option<PaneAgent> {
    if pane.role == "sidebar" {
        return None;
    }
    let mut best: Option<(&dyn AgentSource, AgentState, Option<u64>)> = None;
    for &src in SOURCES {
        let Some(state) = src.read(pane) else {
            continue;
        };
        let t = src.updated(pane);
        best = match best {
            Some((b, bs, bt)) if !later_wins((&bs, bt), (&state, t), now) => Some((b, bs, bt)),
            _ => Some((src, state, t)),
        };
    }
    let (src, state, _) = best?;
    let stale = !src.looks_alive(pane, &state);
    Some(PaneAgent {
        pane: pane.id.clone(),
        state,
        stale,
    })
}

/// The agents of one window's panes (in pane order); never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowAgents {
    pub panes: Vec<PaneAgent>,
}

impl WindowAgents {
    pub fn of<'a>(panes: impl IntoIterator<Item = &'a Pane>) -> Option<WindowAgents> {
        let panes: Vec<PaneAgent> = panes.into_iter().filter_map(pane_agent).collect();
        (!panes.is_empty()).then_some(WindowAgents { panes })
    }

    /// The pane the window is shown by (row, card, preview, ⏎): the one
    /// that needs you, if any; else the most urgent live agent (the first
    /// of equals); else the first stale one.
    pub fn lead(&self) -> &PaneAgent {
        self.needing().unwrap_or_else(|| {
            self.panes
                .iter()
                .filter(|p| p.live())
                .rev() // max_by_key keeps the last maximum
                .max_by_key(|p| p.state.status)
                .unwrap_or(&self.panes[0])
        })
    }

    /// Window status: the most urgent live agent's; `None` when every
    /// agent here is stale.
    pub fn status(&self) -> Option<Status> {
        self.panes
            .iter()
            .filter(|p| p.live())
            .map(|p| p.state.status)
            .max()
    }

    /// Every agent here looks stale.
    pub fn stale(&self) -> bool {
        self.panes.iter().all(|p| p.stale)
    }

    pub fn live_count(&self) -> usize {
        self.panes.iter().filter(|p| p.live()).count()
    }

    pub fn needs_you(&self) -> bool {
        self.panes.iter().any(PaneAgent::needs_you)
    }

    /// The pane that needs you (the first), if any.
    pub fn needing(&self) -> Option<&PaneAgent> {
        let mut v: Vec<&PaneAgent> = self.panes.iter().filter(|p| p.needs_you()).collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.state.status));
        v.first().copied()
    }

    /// A live agent here has status `s`.
    pub fn has_status(&self, s: Status) -> bool {
        self.panes.iter().any(|p| p.live() && p.state.status == s)
    }

    /// A live agent here is mid-run (running or waiting).
    pub fn working(&self) -> bool {
        self.panes
            .iter()
            .any(|p| p.live() && p.state.status.working())
    }
}

/// Live agents by status, for the header. Stale agents aren't counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    counts: [usize; 6],
}

impl Tally {
    pub fn add(&mut self, p: &PaneAgent) {
        if p.live() {
            self.counts[p.state.status as usize] += 1;
        }
    }

    pub fn get(&self, s: Status) -> usize {
        self.counts[s as usize]
    }

    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }

    /// `1 waiting · 1 running · 6 idle`, most urgent first; empty when
    /// there are no live agents.
    pub fn label(&self) -> String {
        Status::ALL
            .iter()
            .filter(|&&s| self.get(s) > 0)
            .map(|&s| format!("{} {}", self.get(s), s.word()))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Run elapsed for display: `42s`, `12m`, `3h05m`.
pub fn elapsed(started: u64, now: u64) -> String {
    let d = now.saturating_sub(started);
    match d {
        0..60 => format!("{d}s"),
        60..3600 => format!("{}m", d / 60),
        _ => format!("{}h{:02}m", d / 3600, (d % 3600) / 60),
    }
}

/// Unix seconds now.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A pane running `cmd` with these `@pane_*` options.
    pub fn pane(id: &str, cmd: &str, opts: &[(&str, &str)]) -> Pane {
        Pane {
            id: id.into(),
            window_id: "@0".into(),
            session_id: "$0".into(),
            index: 0,
            active: true,
            current_command: cmd.into(),
            current_path: "/".into(),
            title: String::new(),
            role: String::new(),
            home_role: String::new(),
            agent_opts: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            tty: String::new(),
        }
    }

    fn agent(id: &str, cmd: &str, status: &str) -> Pane {
        pane(
            id,
            cmd,
            &[("@pane_agent", "claude"), ("@pane_status", status)],
        )
    }

    #[test]
    fn no_options_no_agent() {
        assert_eq!(pane_agent(&pane("%1", "fish", &[])), None);
        assert_eq!(WindowAgents::of([&pane("%1", "nvim", &[])]), None);
    }

    #[test]
    fn sidebar_panes_are_never_agents() {
        let mut p = agent("%1", "tmux-agent-sidebar", "running");
        p.role = "sidebar".into();
        assert_eq!(pane_agent(&p), None);
    }

    #[test]
    fn a_shell_with_agent_options_is_stale() {
        for sh in ["fish", "-zsh", "bash", "sh", "/bin/sh"] {
            let a = pane_agent(&agent("%1", sh, "waiting")).unwrap();
            assert!(a.stale, "{sh}");
            assert!(!a.needs_you(), "stale never needs you");
        }
        // claude reports its version as the command
        let a = pane_agent(&agent("%1", "2.1.283", "waiting")).unwrap();
        assert!(!a.stale);
        assert!(a.needs_you());
    }

    #[test]
    fn window_status_is_the_most_urgent_live_pane() {
        let order = ["idle", "background", "running", "waiting", "error"];
        for (i, worst) in order.iter().enumerate() {
            let panes: Vec<Pane> = order[..=i]
                .iter()
                .enumerate()
                .map(|(n, s)| agent(&format!("%{n}"), "node", s))
                .collect();
            let w = WindowAgents::of(&panes).unwrap();
            assert_eq!(w.status().unwrap().word(), *worst);
            assert_eq!(w.lead().state.status.word(), *worst);
            assert_eq!(w.live_count(), i + 1);
        }
    }

    #[test]
    fn stale_panes_dont_count_toward_window_status() {
        let panes = [agent("%1", "fish", "error"), agent("%2", "node", "idle")];
        let w = WindowAgents::of(&panes).unwrap();
        assert_eq!(w.status(), Some(Status::Idle));
        assert_eq!(w.lead().pane, "%2");
        assert!(!w.stale() && !w.needs_you());
        assert_eq!(w.live_count(), 1);
        // all stale: no status, but the window is still an agent window
        let panes = [agent("%1", "fish", "running")];
        let w = WindowAgents::of(&panes).unwrap();
        assert_eq!(w.status(), None);
        assert!(w.stale());
        assert_eq!(w.lead().pane, "%1");
        assert!(!w.working());
    }

    #[test]
    fn needs_you_rules() {
        let needs = |opts: &[(&str, &str)]| {
            let mut o = vec![("@pane_agent", "claude")];
            o.extend_from_slice(opts);
            pane_agent(&pane("%1", "node", &o)).unwrap().needs_you()
        };
        assert!(needs(&[("@pane_status", "waiting")]));
        assert!(needs(&[("@pane_status", "notification")]));
        assert!(needs(&[("@pane_status", "error")]));
        // a notification with a live bg shell lands in background
        assert!(needs(&[
            ("@pane_status", "background"),
            ("@pane_attention", "notification")
        ]));
        assert!(!needs(&[
            ("@pane_status", "running"),
            ("@pane_attention", "clear")
        ]));
        assert!(!needs(&[("@pane_status", "idle")]));
    }

    /// The sidebar's TaskCompleted hook sets attention without touching
    /// the status: a running agent with a notification isn't waiting on you.
    #[test]
    fn running_with_a_notification_doesnt_need_you() {
        let p = |status| {
            pane_agent(&pane(
                "%1",
                "2.1.283",
                &[
                    ("@pane_agent", "claude"),
                    ("@pane_status", status),
                    ("@pane_attention", "notification"),
                ],
            ))
            .unwrap()
        };
        assert!(!p("running").needs_you());
        assert!(p("idle").needs_you());
        assert!(p("background").needs_you());
        assert!(p("waiting").needs_you() && p("error").needs_you());
    }

    /// The lead (card, preview, ⏎) is the pane that needs you, when one
    /// does, even if another pane is more urgent by status.
    #[test]
    fn lead_is_the_pane_that_needs_you() {
        let panes = [
            agent("%1", "node", "running"),
            pane(
                "%2",
                "node",
                &[
                    ("@pane_agent", "claude"),
                    ("@pane_status", "idle"),
                    ("@pane_attention", "notification"),
                ],
            ),
        ];
        let w = WindowAgents::of(&panes).unwrap();
        assert_eq!(w.status(), Some(Status::Running));
        assert_eq!(w.lead().pane, "%2");
        assert_eq!(w.needing().unwrap().pane, "%2");
    }

    /// Positive liveness per kind: Claude runs as its version (or
    /// `claude`/`node`); anything else in its pane means it has gone.
    #[test]
    fn liveness_by_kind() {
        let stale = |kind: &str, cmd: &str| {
            pane_agent(&pane(
                "%1",
                cmd,
                &[("@pane_agent", kind), ("@pane_status", "running")],
            ))
            .unwrap()
            .stale
        };
        for cmd in ["2.1.283", "10.0.1", "claude", "node"] {
            assert!(!stale("claude", cmd), "{cmd}");
        }
        for cmd in ["fish", "-zsh", "nvim", "ssh", "1.2", "2.1.x", "v2.1.3", ""] {
            assert!(stale("claude", cmd), "{cmd}");
        }
        assert!(!stale("codex", "nvim") && !stale("codex", "codex"));
        assert!(stale("codex", "zsh") && stale("opencode", "bash"));
    }

    #[test]
    fn options_are_every_sources_names_once() {
        for o in sidebar::OPTIONS.iter().chain(home::OPTIONS) {
            assert!(OPTIONS.contains(o), "{o}");
        }
        assert_eq!(OPTIONS[0], "@home_agent", "home's names come first");
        let mut v = OPTIONS.clone();
        v.sort();
        v.dedup();
        assert_eq!(v.len(), OPTIONS.len());
    }

    #[test]
    fn tally_counts_live_agents_by_status() {
        let panes = [
            agent("%1", "node", "waiting"),
            agent("%2", "node", "running"),
            agent("%3", "node", "idle"),
            agent("%4", "node", "idle"),
            agent("%5", "fish", "waiting"), // stale
        ];
        let mut t = Tally::default();
        for p in panes.iter().filter_map(pane_agent) {
            t.add(&p);
        }
        assert_eq!(t.total(), 4);
        assert_eq!(t.label(), "1 waiting · 1 running · 2 idle");
        assert_eq!(Tally::default().label(), "");
    }

    #[test]
    fn elapsed_format() {
        assert_eq!(elapsed(100, 100), "0s");
        assert_eq!(elapsed(100, 159), "59s");
        assert_eq!(elapsed(100, 160), "1m");
        assert_eq!(elapsed(0, 12 * 60 + 5), "12m");
        assert_eq!(elapsed(0, 3 * 3600 + 5 * 60), "3h05m");
        assert_eq!(elapsed(200, 100), "0s", "clock skew");
    }

    #[test]
    fn status_words_and_icons_are_distinct() {
        let mut icons: Vec<_> = Status::ALL.iter().map(|s| s.icon()).collect();
        icons.dedup();
        assert_eq!(icons.len(), 6);
        assert!(Status::Running.working() && Status::Waiting.working());
        assert!(!Status::Background.working() && !Status::Idle.working());
    }
}
