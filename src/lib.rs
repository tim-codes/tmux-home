pub mod client;
pub mod daemon;
pub mod ipc;
pub mod paths;
pub mod tmux;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
