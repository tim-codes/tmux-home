//! This process's own binary, as it was on disk when the process started,
//! and whether a build has since replaced it there.
//!
//! The binary on disk is the source of truth for which build should run.
//! A long-running process (the sidebar, the daemon) still runs the image
//! it started from after a rebuild replaced the file, so it compares that
//! file now with what it was at start: a different file there means this
//! process is the stale build. Clients tell the daemon so (`current` in
//! the request envelope): a stale client never makes the daemon restart,
//! and a sidebar replaces itself with the new binary (`reexec`).

use std::{
    ffi::OsString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// What tells one file at a path from another written there later: a
/// build writes a new file (a new inode, a new mtime), or at least new
/// contents (a new size or mtime).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl FileId {
    fn of(path: &Path) -> Option<FileId> {
        let m = std::fs::metadata(path).ok()?;
        m.is_file().then(|| FileId {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
        })
    }
}

/// A binary at `path`, as it was when this was made.
#[derive(Clone, Debug)]
pub struct Exe {
    path: PathBuf,
    at_start: Option<FileId>,
}

impl Exe {
    pub fn at(path: impl Into<PathBuf>) -> Exe {
        let path = path.into();
        let at_start = FileId::of(&path);
        Exe { path, at_start }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Another file is at the path now. A missing or unreadable one (a
    /// build halfway through putting it there) is not a replacement yet:
    /// there is nothing to run instead.
    pub fn replaced(&self) -> bool {
        match (self.at_start, FileId::of(&self.path)) {
            (Some(then), Some(now)) => then != now,
            _ => false,
        }
    }
}

static SELF: OnceLock<Exe> = OnceLock::new();

/// Records this process's binary. Called first thing in `main`, before a
/// rebuild can have replaced it; until then (a library user, a test
/// process) this process counts as never replaced.
pub fn init() {
    if let Ok(p) = std::env::current_exe() {
        let _ = SELF.set(Exe::at(p));
    }
}

/// This process's binary as recorded by `init`.
pub fn get() -> Option<&'static Exe> {
    SELF.get()
}

/// Whether a build has replaced this process's binary since it started:
/// this process is then an older build than the one on disk.
pub fn replaced() -> bool {
    get().is_some_and(Exe::replaced)
}

/// Replaces this process with the binary now on disk at its path, with the
/// same arguments and environment (same PID, so a pane running it keeps
/// running). Returns only on failure.
pub fn reexec() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let Some(exe) = get() else {
        return std::io::Error::other("own binary not recorded");
    };
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap_or_else(|| OsString::from(exe.path()));
    std::process::Command::new(exe.path())
        .arg0(arg0)
        .args(args)
        .exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(path: &Path, bytes: &[u8]) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn a_new_file_at_the_path_is_a_replacement() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bin");
        install(&p, b"build one");
        let exe = Exe::at(&p);
        assert!(!exe.replaced(), "the same file");
        install(&p, b"build one");
        assert!(exe.replaced(), "a new file, even with the same bytes");
    }

    #[test]
    fn a_missing_binary_is_not_a_replacement_yet() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bin");
        install(&p, b"build one");
        let exe = Exe::at(&p);
        std::fs::remove_file(&p).unwrap();
        assert!(!exe.replaced(), "nothing to run instead");
        install(&p, b"build two");
        assert!(exe.replaced());
    }

    #[test]
    fn a_binary_unknown_at_start_never_counts_as_replaced() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bin");
        let exe = Exe::at(&p);
        install(&p, b"build two");
        assert!(!exe.replaced());
    }

    #[test]
    fn a_process_without_init_is_never_replaced() {
        // the test harness never calls `init`
        assert!(!replaced());
    }
}
