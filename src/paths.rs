use std::path::{Path, PathBuf};

pub struct Paths {
    pub sock: PathBuf,
    pub lock: PathBuf,
    pub state_dir: PathBuf,
}

pub fn server_key(tmux_socket: &Path) -> String {
    let digest = sha1_smol::Sha1::from(tmux_socket.as_os_str().as_encoded_bytes()).digest();
    let hex = digest.to_string();
    hex[..12].to_string()
}

fn runtime_root() -> PathBuf {
    if let Some(d) = std::env::var_os("TMUX_HOME_RUNTIME_DIR") {
        return d.into();
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| std::env::var_os("TMPDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into());
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc_getuid() };
    base.join(format!("tmux-home-{uid}"))
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn state_root() -> PathBuf {
    if let Some(d) = std::env::var_os("TMUX_HOME_STATE_DIR") {
        return d.into();
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    base.join("tmux-home")
}

impl Paths {
    pub fn for_socket(tmux_socket: &Path) -> anyhow::Result<Paths> {
        let key = server_key(tmux_socket);
        let rt = runtime_root();
        Ok(Paths {
            sock: rt.join(format!("{key}.sock")),
            lock: rt.join(format!("{key}.lock")),
            state_dir: state_root().join(key),
        })
    }
}
