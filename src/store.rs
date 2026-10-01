//! Closed-window stack: `<state_dir>/closed.json`, a 10-deep LIFO of window
//! shapes (not processes) written atomically under `state.lock`.

use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

pub const CLOSED_MAX: usize = 10;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ClosedWindow {
    pub session: String,
    pub index: u32,
    /// Window IDs of the old neighbours in its session ("-" for none).
    pub prev: String,
    pub next: String,
    pub automatic_rename: bool,
    /// Position of the active pane among `paths`.
    pub active: usize,
    /// `window_layout`, empty when it can't be replayed (a sidebar was dropped).
    pub layout: String,
    pub name: String,
    /// Each non-sidebar pane's cwd, in pane order.
    pub paths: Vec<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct ClosedFile {
    closed: Vec<ClosedWindow>,
}

pub struct Store {
    dir: PathBuf,
    /// The bash version's stack (`<state root>/closed`), imported once.
    legacy: Option<PathBuf>,
    /// Something the user should hear about (an unreadable stack was reset);
    /// the caller shows it (`take_notice`): the popup can't print while it
    /// owns the screen.
    notice: std::sync::Mutex<Option<String>>,
}

impl Store {
    pub fn new(dir: PathBuf, legacy: Option<PathBuf>) -> Store {
        Store {
            dir,
            legacy,
            notice: Default::default(),
        }
    }

    /// The pending notice, if any (cleared).
    pub fn take_notice(&self) -> Option<String> {
        self.notice.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn set_notice(&self, n: String) {
        *self.notice.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    /// The store for a tmux server's state dir, importing the bash stack from
    /// its parent (the state root, where bash kept `closed`).
    pub fn for_socket(tmux_socket: &Path) -> anyhow::Result<Store> {
        let dir = crate::paths::Paths::for_socket(tmux_socket)?.state_dir;
        let legacy = dir.parent().map(|p| p.join("closed"));
        Ok(Store::new(dir, legacy))
    }

    fn file(&self) -> PathBuf {
        self.dir.join("closed.json")
    }

    fn with_lock<T>(
        &self,
        f: impl FnOnce(&mut Vec<ClosedWindow>) -> anyhow::Result<(T, bool)>,
    ) -> anyhow::Result<T> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("state.lock"))?;
        lock.lock()?;
        let mut stack = self.read()?;
        let (out, dirty) = f(&mut stack)?;
        if dirty {
            self.write(&stack)?;
        }
        Ok(out)
    }

    fn read(&self) -> anyhow::Result<Vec<ClosedWindow>> {
        match std::fs::read(self.file()) {
            Ok(b) => match serde_json::from_slice::<ClosedFile>(&b) {
                Ok(f) => Ok(f.closed),
                // hand-edited, truncated or from an incompatible version:
                // keep it for inspection and start an empty stack (even if
                // it can't be moved aside: the next write replaces it)
                Err(_) => {
                    let name = corrupt_name(std::time::SystemTime::now());
                    self.set_notice(match std::fs::rename(self.file(), self.dir.join(&name)) {
                        Ok(()) => format!("closed.json was unreadable: kept as {name}, reopen stack reset"),
                        Err(e) => format!(
                            "closed.json is unreadable and could not be moved aside ({e}): reopen stack reset"
                        ),
                    });
                    Ok(vec![])
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let imported = self.import_legacy();
                if !imported.is_empty() {
                    self.write(&imported)?;
                }
                Ok(imported)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Reads the bash `closed` file (US-separated lines) and renames it to
    /// `closed.imported` so it is imported only once.
    fn import_legacy(&self) -> Vec<ClosedWindow> {
        let Some(p) = &self.legacy else { return vec![] };
        let Ok(text) = std::fs::read_to_string(p) else {
            return vec![];
        };
        let out: Vec<ClosedWindow> = text.lines().filter_map(parse_legacy).collect();
        let _ = std::fs::rename(p, p.with_file_name("closed.imported"));
        out
    }

    fn write(&self, stack: &[ClosedWindow]) -> anyhow::Result<()> {
        let tmp = self
            .dir
            .join(format!("closed.json.{}.tmp", std::process::id()));
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&ClosedFile {
            closed: stack.to_vec(),
        })?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, self.file())?;
        Ok(())
    }

    pub fn push(&self, w: ClosedWindow) -> anyhow::Result<()> {
        self.with_lock(|s| {
            s.push(w);
            let excess = s.len().saturating_sub(CLOSED_MAX);
            s.drain(..excess);
            Ok(((), true))
        })
    }

    pub fn pop(&self) -> anyhow::Result<Option<ClosedWindow>> {
        self.with_lock(|s| {
            let w = s.pop();
            let dirty = w.is_some();
            Ok((w, dirty))
        })
    }

    /// Put an entry back on top (a reopen that failed).
    pub fn unpop(&self, w: ClosedWindow) -> anyhow::Result<()> {
        self.push(w)
    }

    pub fn len(&self) -> anyhow::Result<usize> {
        self.with_lock(|s| Ok((s.len(), false)))
    }

    pub fn is_empty(&self) -> anyhow::Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// `closed.json.corrupt.<unix seconds>.<nanos>`: each unreadable file is
/// kept, none overwrites an earlier one.
fn corrupt_name(now: std::time::SystemTime) -> String {
    let d = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "closed.json.corrupt.{}.{:09}",
        d.as_secs(),
        d.subsec_nanos()
    )
}

/// `session index prev next auto active layout name path...`, US-separated.
fn parse_legacy(line: &str) -> Option<ClosedWindow> {
    let f: Vec<&str> = line.split('\x1f').collect();
    if f.len() < 8 {
        return None;
    }
    let mut paths: Vec<String> = f[8..].iter().map(|s| s.to_string()).collect();
    if paths.is_empty() {
        paths.push(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    }
    Some(ClosedWindow {
        session: f[0].into(),
        index: f[1].parse().ok()?,
        prev: f[2].into(),
        next: f[3].into(),
        automatic_rename: f[4] == "on",
        active: f[5].parse().unwrap_or(0),
        layout: f[6].into(),
        name: f[7].into(),
        paths,
    })
}
