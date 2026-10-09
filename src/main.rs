use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tmux-home", version = tmux_home::BUILD_ID)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon for one tmux server (normally started automatically).
    Daemon {
        #[arg(long)]
        socket: std::path::PathBuf,
        #[arg(long, value_enum, default_value = "poll")]
        source: tmux_home::tmux::source::SourceKind,
    },
    /// Print the current snapshot as JSON.
    Query {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// Print JSON (accepted for forward compatibility; JSON is
        /// currently the only output format).
        #[arg(long)]
        json: bool,
    },
    /// The full-window popup (run inside `display-popup -E`).
    Popup {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// The read-only sidebar (run in its own pane; `sidebar-toggle` makes
    /// one). `q` closes it; it ignores every other key.
    Sidebar {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// A pane tmux-home made (or resurrect restored): mark it as a
        /// sidebar, and let `q` close the pane.
        #[arg(long)]
        managed: bool,
    },
    /// Add or remove a sidebar in a window (default: $TMUX_PANE's).
    SidebarToggle {
        /// Every window of the window's session: on if any lacks one, else off.
        #[arg(long)]
        session: bool,
        /// The window (any tmux target, e.g. `$1:@3`).
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Recreate the most recently closed window; prints its ID (exit 4 if none).
    Reopen {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// One status-line token: ● daemon up, ○ down (nothing outside tmux).
    Status {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// An agent's hook: reads the event JSON on stdin and records it in the
    /// pane's `@home_*` options (pane from $TMUX_PANE). Always exits 0,
    /// silently. Events (claude): SessionStart, UserPromptSubmit, Stop,
    /// StopFailure, Notification, PermissionDenied, SessionEnd,
    /// SubagentStart, SubagentStop.
    Hook {
        /// `claude`.
        agent: String,
        /// The event name, as the agent calls it (`Stop`).
        event: String,
    },
    /// Save the panes running Claude Code under their `session:window.pane`
    /// (for `@resurrect-hook-post-save-all`). Always exits 0.
    AgentsSnapshot {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Resume the saved Claude Code sessions in the panes resurrect
    /// restored (for `@resurrect-hook-post-restore-all`; needs
    /// `@home-restore-agents on`). Once per server; returns at once, the
    /// launches are staggered in the background. Always exits 0.
    RestoreAgents {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// The background half (started by the command itself).
        #[arg(long, hide = true)]
        worker: bool,
    },
    /// R0 spike: measure control-mode side effects on a server and print a report.
    SpikeControl {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    // The hook bypasses clap: it must exit 0 whatever its arguments are.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("hook") {
        std::process::exit(tmux_home::hook::main(&args[1..]));
    }
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Cmd::Popup { socket } => return tmux_home::popup::run(socket),
        Cmd::Status { socket } => return tmux_home::client::status(socket),
        Cmd::Sidebar { socket, managed } => {
            return tmux_home::sidebar::run(socket, managed);
        }
        Cmd::SidebarToggle {
            session,
            window,
            socket,
        } => return tmux_home::sidebar::toggle(socket, session, window),
        Cmd::Query { socket, json: _ } => return tmux_home::client::query(socket),
        Cmd::AgentsSnapshot { socket } => return tmux_home::restore::snapshot_cli(socket),
        Cmd::RestoreAgents { socket, worker } => {
            return tmux_home::restore::restore_cli(socket, worker);
        }
        Cmd::Reopen { socket } => {
            let code = tmux_home::popup::reopen_cli(socket)?;
            std::process::exit(code);
        }
        c => c,
    };
    let rt = tokio::runtime::Runtime::new()?;
    match cmd {
        Cmd::Daemon { socket, source } => rt.block_on(tmux_home::daemon::run(socket, source)),
        Cmd::SpikeControl { socket } => rt.block_on(tmux_home::tmux::source::spike_control(socket)),
        Cmd::Popup { .. }
        | Cmd::Status { .. }
        | Cmd::Reopen { .. }
        | Cmd::Query { .. }
        | Cmd::Sidebar { .. }
        | Cmd::SidebarToggle { .. }
        | Cmd::AgentsSnapshot { .. }
        | Cmd::RestoreAgents { .. }
        | Cmd::Hook { .. } => {
            unreachable!()
        }
    }
}
