//! What the sidebar shows (spec §8), as pure data built from a snapshot:
//! NEEDS YOU across the server, then the windows of the session the
//! sidebar lives in, and a one-line server tally. Rows come from the
//! popup's `build_rows`, so status, NEEDS YOU and badges mean exactly what
//! they mean there.

use crate::agent::{Status, Tally};
use crate::git::badge::RepoStatus;
use crate::popup::app::{Row, build_rows};
use crate::tmux::snapshot::Snapshot;

/// One window line.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub wid: String,
    pub session: String,
    pub index: u32,
    pub name: String,
    /// The lead agent's status, if the window has an agent.
    pub status: Option<Status>,
    /// The agent has ended (its pane moved on): shown dimmed.
    pub stale: bool,
    /// Why the window needs you (NEEDS YOU entries).
    pub reason: Option<String>,
    pub git: Option<RepoStatus>,
    /// The window the sidebar lives in.
    pub here: bool,
}

impl Entry {
    fn of(r: &Row, here: &str) -> Entry {
        let reason = r.agents.as_ref().and_then(|a| a.needing()).map(|p| {
            p.state
                .wait_reason
                .as_ref()
                .map(|w| w.label())
                .unwrap_or_else(|| "needs you".into())
        });
        Entry {
            wid: r.wid.clone(),
            session: r.session.clone(),
            index: r.index,
            name: r.name.clone(),
            status: r.agents.as_ref().map(|a| a.lead().state.status),
            stale: r.agents.as_ref().is_some_and(|a| a.stale()),
            reason,
            git: r.git.as_ref().map(|(_, g)| g.clone()),
            here: r.wid == here,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    /// The sidebar's session; `None` while its pane isn't in the snapshot.
    pub session: Option<String>,
    /// Windows that need you, server-wide.
    pub needs: Vec<Entry>,
    /// The sidebar's session's windows, in index order.
    pub windows: Vec<Entry>,
    /// Live agents by status, server-wide.
    pub tally: Tally,
    pub sessions: usize,
    pub window_count: usize,
}

impl Model {
    /// The model for the sidebar in pane `me`.
    pub fn build(s: &Snapshot, me: &str, home: &str) -> Model {
        let mine = s.panes.iter().find(|p| p.id == me);
        let (sid, wid) = mine
            .map(|p| (p.session_id.as_str(), p.window_id.as_str()))
            .unwrap_or_default();
        let rows = build_rows(s, None, home);
        let mut m = Model {
            session: s
                .sessions
                .iter()
                .find(|x| x.id == sid)
                .map(|x| x.name.clone()),
            sessions: s.sessions.len(),
            window_count: rows.iter().filter(|r| !r.pinned).count(),
            ..Model::default()
        };
        for r in &rows {
            if r.pinned {
                m.needs.push(Entry::of(r, wid));
                continue;
            }
            for p in r.agents.iter().flat_map(|a| &a.panes) {
                m.tally.add(p);
            }
            if r.sid == sid {
                m.windows.push(Entry::of(r, wid));
            }
        }
        m
    }

    /// The bottom line: `2s 7w · 1 waiting · 2 running` (`no agents`
    /// without live agents).
    pub fn tally_line(&self) -> String {
        let agents = if self.tally.total() > 0 {
            self.tally.label()
        } else {
            "no agents".into()
        };
        format!("{}s {}w · {agents}", self.sessions, self.window_count)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent::tests::pane as agent_pane;
    use crate::git::badge::GitSection;
    use crate::tmux::snapshot::{Pane, Session, Window};

    fn win(id: &str, sid: &str, index: u32, name: &str) -> Window {
        Window {
            id: id.into(),
            session_id: sid.into(),
            index,
            name: name.into(),
            automatic_rename: false,
            active: index == 0,
        }
    }

    fn put(p: Pane, wid: &str, sid: &str, index: u32) -> Pane {
        Pane {
            window_id: wid.into(),
            session_id: sid.into(),
            index,
            ..p
        }
    }

    /// Sessions main ($1: api @1 waiting, web @2 with the sidebar and a
    /// repo, ed @3 running) and ops ($2: logs @4 errored).
    pub fn snap() -> Snapshot {
        let mut s = Snapshot {
            sessions: vec![
                Session {
                    id: "$1".into(),
                    name: "main".into(),
                    attached: 1,
                },
                Session {
                    id: "$2".into(),
                    name: "ops".into(),
                    attached: 0,
                },
            ],
            windows: vec![
                win("@1", "$1", 1, "api"),
                win("@2", "$1", 2, "web"),
                win("@3", "$1", 3, "ed"),
                win("@4", "$2", 0, "logs"),
            ],
            ..Snapshot::default()
        };
        let now = crate::agent::now().to_string();
        let waiting = agent_pane(
            "%1",
            "claude",
            &[
                ("@home_agent", "claude"),
                ("@home_status", "waiting"),
                ("@home_attention", "notification"),
                ("@home_wait_reason", "permission_prompt"),
                ("@home_updated", &now),
            ],
        );
        let running = agent_pane(
            "%3",
            "claude",
            &[
                ("@home_agent", "claude"),
                ("@home_status", "running"),
                ("@home_updated", &now),
            ],
        );
        let error = agent_pane(
            "%5",
            "claude",
            &[
                ("@home_agent", "claude"),
                ("@home_status", "error"),
                ("@home_wait_reason", "rate limited"),
                ("@home_updated", &now),
            ],
        );
        let mut side = put(agent_pane("%9", "tmux-home", &[]), "@2", "$1", 0);
        side.role = "sidebar".into();
        side.home_role = "sidebar".into();
        let mut web = put(agent_pane("%2", "zsh", &[]), "@2", "$1", 1);
        web.current_path = "/r".into();
        web.active = true;
        s.panes = vec![
            put(waiting, "@1", "$1", 0),
            side,
            web,
            put(running, "@3", "$1", 0),
            put(error, "@4", "$2", 0),
        ];
        let mut g = GitSection::default();
        g.paths.insert("/r".into(), "/r".into());
        g.repos.insert(
            "/r".into(),
            RepoStatus {
                branch: "main".into(),
                modified: 2,
                ahead: 1,
                upstream: Some("origin/main".into()),
                ..RepoStatus::default()
            },
        );
        s.git = g;
        s
    }

    #[test]
    fn needs_you_is_server_wide_windows_are_the_sidebars_session() {
        let m = Model::build(&snap(), "%9", "/home");
        assert_eq!(m.session.as_deref(), Some("main"));
        let needs: Vec<_> = m
            .needs
            .iter()
            .map(|e| (e.session.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(needs, [("main", "api"), ("ops", "logs")]);
        assert_eq!(m.needs[0].reason.as_deref(), Some("permission"));
        assert_eq!(m.needs[1].status, Some(Status::Error));
        let wins: Vec<_> = m.windows.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(wins, ["api", "web", "ed"], "ops' windows aren't listed");
    }

    #[test]
    fn the_sidebars_window_is_here_with_its_badge() {
        let m = Model::build(&snap(), "%9", "/home");
        let here: Vec<_> = m.windows.iter().filter(|e| e.here).collect();
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].name, "web");
        assert_eq!(here[0].status, None, "a sidebar pane is never an agent");
        assert_eq!(here[0].git.as_ref().unwrap().badge_text(), "main ! ⇡1");
        assert_eq!(m.windows[2].status, Some(Status::Running));
    }

    #[test]
    fn tally_counts_live_agents_and_windows() {
        let m = Model::build(&snap(), "%9", "/home");
        assert_eq!(m.sessions, 2);
        assert_eq!(m.window_count, 4, "pinned copies aren't counted twice");
        assert_eq!(m.tally_line(), "2s 4w · 1 error · 1 waiting · 1 running");
        let quiet = Snapshot {
            panes: vec![],
            ..snap()
        };
        assert_eq!(
            Model::build(&quiet, "%9", "/home").tally_line(),
            "2s 4w · no agents"
        );
    }

    #[test]
    fn an_unknown_pane_has_no_session_but_still_sees_needs_you() {
        let m = Model::build(&snap(), "%404", "/home");
        assert_eq!(m.session, None);
        assert!(m.windows.is_empty());
        assert_eq!(m.needs.len(), 2);
    }
}
