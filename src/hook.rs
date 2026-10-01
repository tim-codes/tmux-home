//! `tmux-home hook <agent> <event>`: an agent's hook. Reads the event
//! payload (JSON) from stdin, finds the pane from `$TMUX_PANE` and the
//! server from `$TMUX`, and writes the pane's `@home_*` options through
//! the agent's `AgentAdapter`. It talks to no daemon: the daemon's poll
//! picks the options up.
//!
//! It always exits 0, quickly and silently: the agent must never be held
//! up or shown an error by it. Anything that goes wrong (no `$TMUX_PANE`,
//! tmux unreachable, bad JSON) is a no-op, noted in `<state_dir>/hook.log`
//! when the server (and so the state dir) is known.

use crate::agent::adapter::{AgentAdapter, Change, Prior, adapter, stamped};
use crate::tmux::Tmux;
use std::io::{Read, Write};
use std::path::Path;

/// `hook.log` is moved to `hook.log.1` once it reaches this size.
pub const LOG_CAP: u64 = 64 << 10;

// A panic must unwind into `main`'s catch_unwind: with `panic = "abort"`
// it would kill the process with SIGABRT, which Claude Code reads as a
// failed hook.
#[cfg(panic = "abort")]
compile_error!("tmux-home hook needs panic = \"unwind\"");

/// The whole hook. Never panics outward and never prints.
pub fn main(args: &[String]) -> i32 {
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| run(args));
    0
}

fn run(args: &[String]) {
    let (agent, event) = match args {
        [a, e, ..] => (a.as_str(), e.as_str()),
        _ => return,
    };
    let socket = crate::client::current_socket(None);
    let log = |msg: &str| {
        if let Some(s) = &socket {
            log(s, &format!("{agent} {event}: {msg}"));
        }
    };
    let Some(ad) = adapter(agent) else {
        return log("unknown agent");
    };
    if !ad.events().contains(&event) {
        return; // not ours: don't even read stdin
    }
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    if !pane.starts_with('%') {
        return log("no $TMUX_PANE");
    }
    let Some(socket) = socket.clone() else {
        return; // no $TMUX: nowhere to write, nowhere to log
    };
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        return log(&format!("stdin: {e}"));
    }
    let payload: serde_json::Value = if input.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        match serde_json::from_str(&input) {
            Ok(v @ serde_json::Value::Object(_)) => v,
            Ok(_) => return log("payload is not a JSON object"),
            Err(e) => return log(&format!("bad JSON: {e}")),
        }
    };
    let t = Tmux::new(socket);
    if let Err(e) = apply_event(&t, &pane, ad, event, &payload, crate::agent::now()) {
        log(&format!("{pane}: {e:#}"));
    }
}

/// Reads the pane's prior state, runs the adapter, writes its changes:
/// two tmux commands at most.
pub fn apply_event(
    t: &Tmux,
    pane: &str,
    ad: &dyn AgentAdapter,
    event: &str,
    payload: &serde_json::Value,
    now: u64,
) -> anyhow::Result<()> {
    let prior = read_prior(t, pane)?;
    let changes = stamped(ad.on_hook(event, payload, &prior, now), now);
    write(t, pane, &changes)
}

/// The pane's subagent list and known background shells, in one call:
/// tmux-agent-sidebar's `@pane_bg_cmd` (set by its PostToolUse hook,
/// cleared by its refresh sweep; tmux-home stays off the tool-call path,
/// spec §5) and `@home_bg_cmd` (from the last Stop's `background_tasks`).
pub fn read_prior(t: &Tmux, pane: &str) -> anyhow::Result<Prior> {
    let out = t.run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{@home_subagents}\x1f#{@pane_bg_cmd}\x1f#{@home_bg_cmd}",
    ])?;
    let out = out.strip_suffix('\n').unwrap_or(&out);
    let mut f = out.split('\x1f').map(|v| v.trim().to_string());
    Ok(Prior {
        subagents: f.next().unwrap_or_default(),
        sidebar_bg_cmd: f.next().unwrap_or_default(),
        home_bg_cmd: f.next().unwrap_or_default(),
    })
}

/// All `changes` as one tmux invocation (`set -p … ; set -pu …`).
pub fn write(t: &Tmux, pane: &str, changes: &[Change]) -> anyhow::Result<()> {
    let args = write_args(pane, changes);
    if args.is_empty() {
        return Ok(());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    t.run(&args).map(|_| ())
}

/// The argv for `write`. A value ending in `;` gets that `;` escaped, or
/// tmux would take it for a command separator.
pub fn write_args(pane: &str, changes: &[Change]) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    for c in changes {
        if !a.is_empty() {
            a.push(";".into());
        }
        match c {
            Change::Set(k, v) => {
                let v = match v.strip_suffix(';') {
                    Some(head) => format!("{head}\\;"),
                    None => v.clone(),
                };
                a.extend(["set-option", "-p", "-t", pane, k.option()].map(String::from));
                a.push(v);
            }
            Change::Unset(k) => {
                a.extend(["set-option", "-p", "-u", "-t", pane, k.option()].map(String::from));
            }
        }
    }
    a
}

/// Appends a line to `<state_dir>/hook.log` (state dir created 0700),
/// rotating it to `hook.log.1` at `LOG_CAP`. Errors are dropped.
fn log(socket: &Path, msg: &str) {
    let _ = (|| -> std::io::Result<()> {
        let dir = crate::paths::Paths::for_socket(socket)
            .map_err(std::io::Error::other)?
            .state_dir;
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
        }
        let path = dir.join("hook.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() >= LOG_CAP) {
            std::fs::rename(&path, dir.join("hook.log.1"))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{} {msg}", crate::agent::now())
    })();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::home::Key;

    #[test]
    fn writes_are_stamped() {
        use crate::agent::adapter::stamped;
        let c = stamped(vec![Change::Unset(Key::Attention)], 7);
        assert_eq!(
            c,
            [Change::Unset(Key::Attention)],
            "a teardown leaves no stamp"
        );
        let c = stamped(vec![Change::Set(Key::Status, "idle".into())], 7);
        assert_eq!(c.last(), Some(&Change::Set(Key::Updated, "7".into())));
        assert!(stamped(vec![], 7).is_empty());
    }

    #[test]
    fn write_args_chain_and_escape() {
        let a = write_args(
            "%3",
            &[
                Change::Set(Key::Prompt, "do it;".into()),
                Change::Unset(Key::Attention),
                Change::Set(Key::Status, "-idle".into()),
            ],
        );
        assert_eq!(
            a,
            [
                "set-option",
                "-p",
                "-t",
                "%3",
                "@home_prompt",
                "do it\\;",
                ";",
                "set-option",
                "-p",
                "-u",
                "-t",
                "%3",
                "@home_attention",
                ";",
                "set-option",
                "-p",
                "-t",
                "%3",
                "@home_status",
                "-idle",
            ]
        );
        assert!(write_args("%3", &[]).is_empty());
    }
}
