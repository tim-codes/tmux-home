pub mod client;
pub mod daemon;
pub mod ipc;
pub mod ops;
pub mod paths;
pub mod popup;
pub mod store;
pub mod tmux;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
