use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tmux-home", version)]
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
    /// R0 spike: measure control-mode side effects on a server and print a report.
    SpikeControl {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.cmd {
        Cmd::Daemon { socket, source } => rt.block_on(tmux_home::daemon::run(socket, source)),
        Cmd::Query { socket, json: _ } => rt.block_on(tmux_home::client::query(socket)),
        Cmd::SpikeControl { socket } => rt.block_on(tmux_home::tmux::source::spike_control(socket)),
    }
}
