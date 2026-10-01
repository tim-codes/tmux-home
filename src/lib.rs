pub mod agent;
pub mod client;
pub mod daemon;
pub mod git;
pub mod hook;
pub mod ipc;
pub mod ops;
pub mod paths;
pub mod popup;
pub mod store;
pub mod tmux;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build identity the daemon handshake compares: the package version
/// plus the git commit (and a source hash when the tree is dirty or has no
/// git); see `build.rs`. A rebuild at the same version gets a new ID, so it
/// still replaces a running daemon.
pub const BUILD_ID: &str = env!("TMUX_HOME_BUILD_ID");

#[cfg(test)]
mod tests {
    #[test]
    fn build_id_extends_the_package_version() {
        let rest = super::BUILD_ID
            .strip_prefix(&format!("{}+", super::VERSION))
            .expect("BUILD_ID starts with VERSION+");
        assert!(
            rest.starts_with('g') || rest.starts_with("src."),
            "{}",
            super::BUILD_ID
        );
        assert!(rest.len() >= 13, "{}", super::BUILD_ID);
    }
}
