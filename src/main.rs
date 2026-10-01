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
    /// R0 spike: measure control-mode side effects on a server and print a report.
    SpikeControl {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Cmd::Popup { socket } => return tmux_home::popup::run(socket),
        Cmd::Status { socket } => return tmux_home::client::status(socket),
        Cmd::Query { socket, json: _ } => return tmux_home::client::query(socket),
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
        Cmd::Popup { .. } | Cmd::Status { .. } | Cmd::Reopen { .. } | Cmd::Query { .. } => {
            unreachable!()
        }
    }
}
