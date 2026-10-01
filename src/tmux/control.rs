//! tmux control-mode (`tmux -C`) line classification. The daemon only needs
//! to know *that* something changed; it then re-reads the whole snapshot.

pub const STRUCTURAL: &[&str] = &[
    "window-add",
    "window-close",
    "window-renamed",
    "unlinked-window-add",
    "unlinked-window-close",
    "unlinked-window-renamed",
    "sessions-changed",
    "session-changed",
    "session-renamed",
    "session-window-changed",
    "layout-change",
    "window-pane-changed",
    "pane-mode-changed",
    "subscription-changed",
    "client-session-changed",
];

#[derive(Debug)]
pub enum Line {
    BlockStart,
    BlockEnd { error: bool },
    Notify(Notification),
    Other,
}

#[derive(Debug)]
pub enum Notification {
    Changed(&'static str),
    Exit(Option<String>),
    Output,
}

pub fn parse_line(line: &str) -> Line {
    let Some(rest) = line.strip_prefix('%') else {
        return Line::Other;
    };
    let (name, tail) = rest.split_once(' ').unwrap_or((rest, ""));
    match name {
        "begin" => Line::BlockStart,
        "end" => Line::BlockEnd { error: false },
        "error" => Line::BlockEnd { error: true },
        "exit" => Line::Notify(Notification::Exit(
            (!tail.is_empty()).then(|| tail.to_string()),
        )),
        "output" | "extended-output" => Line::Notify(Notification::Output),
        _ => match STRUCTURAL.iter().find(|s| **s == name) {
            Some(s) => Line::Notify(Notification::Changed(s)),
            None => Line::Other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks() {
        assert!(matches!(
            parse_line("%begin 1727600000 12 1"),
            Line::BlockStart
        ));
        assert!(matches!(
            parse_line("%end 1727600000 12 1"),
            Line::BlockEnd { error: false }
        ));
        assert!(matches!(
            parse_line("%error 1727600000 12 1"),
            Line::BlockEnd { error: true }
        ));
    }

    #[test]
    fn structural_and_subscription_are_changes() {
        for l in [
            "%window-add @3",
            "%window-close @3",
            "%unlinked-window-add @4",
            "%window-renamed @3 new name",
            "%sessions-changed",
            "%session-window-changed $1 @3",
            "%layout-change @3 b25f,80x24,0,0,2 b25f,80x24,0,0,2 *",
            "%window-pane-changed @3 %5",
            "%subscription-changed th $1 @3 0 %5 : fish",
            "%session-renamed $1 x",
        ] {
            assert!(
                matches!(parse_line(l), Line::Notify(Notification::Changed(_))),
                "{}",
                l
            );
        }
    }

    #[test]
    fn exit_and_output() {
        assert!(matches!(
            parse_line("%exit"),
            Line::Notify(Notification::Exit(None))
        ));
        assert!(matches!(
            parse_line("%exit server exited"),
            Line::Notify(Notification::Exit(Some(_)))
        ));
        assert!(matches!(
            parse_line("%output %1 hello"),
            Line::Notify(Notification::Output)
        ));
        assert!(matches!(parse_line("some command output"), Line::Other));
        assert!(matches!(
            parse_line("%client-detached /dev/ttys001"),
            Line::Other
        ));
    }
}
