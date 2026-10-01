mod common;
use std::path::Path;
use tmux_home::paths::{Paths, server_key};

#[test]
fn key_is_stable_12_hex() {
    let k = server_key(Path::new("/private/tmp/tmux-501/default"));
    assert_eq!(k.len(), 12);
    assert!(
        k.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_eq!(k, server_key(Path::new("/private/tmp/tmux-501/default")));
    assert_ne!(k, server_key(Path::new("/private/tmp/tmux-501/other")));
}

#[test]
fn paths_honour_env_overrides() {
    let env = common::TestEnv::new();
    let p = Paths::for_socket(Path::new("/tmp/x/sock")).unwrap();
    let key = server_key(Path::new("/tmp/x/sock"));
    assert_eq!(p.sock, env.runtime.join(format!("{key}.sock")));
    assert_eq!(p.lock, env.runtime.join(format!("{key}.lock")));
    assert_eq!(p.state_dir, env.state.join(&key));
}
