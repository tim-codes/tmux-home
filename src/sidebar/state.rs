//! The sidebar's state that outlives a tmux server, in its state dir
//! (`<state root>/<server key>/`; `TMUX_HOME_STATE_DIR` overrides the
//! root, as for `closed.json`):
//!
//! - `pins`: the names of the pinned sessions, one per line. A session is
//!   pinned by name, so a restored session with the same name is pinned
//!   again (the daemon re-applies the file once a restore is done).
//! - `sidebar-width`: the width last set by hand, `N%` of the window.
//!
//! Both are rewritten whole (a temp file renamed over), under `sidebar.lock`.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub struct State {
    dir: PathBuf,
}

impl State {
    pub fn new(dir: PathBuf) -> State {
        State { dir }
    }

    /// The state of the tmux server at `tmux_socket`.
    pub fn for_socket(tmux_socket: &Path) -> anyhow::Result<State> {
        Ok(State::new(
            crate::paths::Paths::for_socket(tmux_socket)?.state_dir,
        ))
    }

    fn pins_file(&self) -> PathBuf {
        self.dir.join("pins")
    }

    fn width_file(&self) -> PathBuf {
        self.dir.join("sidebar-width")
    }

    /// The pinned session names; none if the file is missing or unreadable.
    pub fn pins(&self) -> Vec<String> {
        std::fs::read_to_string(self.pins_file())
            .map(|s| parse_pins(&s))
            .unwrap_or_default()
    }

    /// Adds (`on`) or removes `name`.
    pub fn set_pin(&self, name: &str, on: bool) -> anyhow::Result<()> {
        self.edit_pins(|pins| {
            pins.retain(|p| p != name);
            if on {
                pins.push(name.to_string());
            }
        })
    }

    /// A pinned session was renamed: `old` → `new`.
    pub fn rename_pin(&self, old: &str, new: &str) -> anyhow::Result<()> {
        self.edit_pins(|pins| {
            pins.retain(|p| p != old && p != new);
            pins.push(new.to_string());
        })
    }

    fn edit_pins(&self, f: impl FnOnce(&mut Vec<String>)) -> anyhow::Result<()> {
        self.locked(|| {
            let mut pins = self.pins();
            let before = pins.clone();
            f(&mut pins);
            if pins == before {
                return Ok(());
            }
            let mut text = String::new();
            for p in &pins {
                text.push_str(p);
                text.push('\n');
            }
            self.replace(&self.pins_file(), &text)
        })
    }

    /// The stored width, a percentage; none if unset or not `N%`.
    pub fn width(&self) -> Option<u32> {
        let s = std::fs::read_to_string(self.width_file()).ok()?;
        parse_pct(&s)
    }

    pub fn set_width(&self, pct: u32) -> anyhow::Result<()> {
        self.locked(|| self.replace(&self.width_file(), &format!("{pct}%\n")))
    }

    /// Runs `f` holding `sidebar.lock` (state dir created 0700).
    fn locked<T>(&self, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("sidebar.lock"))?;
        lock.lock()?;
        f()
    }

    /// Writes `path` whole: a temp file beside it, renamed over it.
    fn replace(&self, path: &Path, text: &str) -> anyhow::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Names, one per line; blank lines dropped, duplicates once.
fn parse_pins(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in s.lines() {
        let l = l.trim_end_matches('\r');
        if !l.is_empty() && !out.iter().any(|p| p == l) {
            out.push(l.to_string());
        }
    }
    out
}

/// `N%` with 1 ≤ N ≤ 99.
fn parse_pct(s: &str) -> Option<u32> {
    let n: u32 = s.trim().strip_suffix('%')?.parse().ok()?;
    (1..=99).contains(&n).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (State, PathBuf) {
        let d = std::env::temp_dir().join(format!(
            "th-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        (State::new(d.join("srv")), d)
    }

    #[test]
    fn pins_are_names_one_per_line() {
        let (s, d) = temp();
        assert!(s.pins().is_empty(), "no file");
        s.set_pin("main", true).unwrap();
        s.set_pin("work two", true).unwrap();
        s.set_pin("main", true).unwrap();
        assert_eq!(s.pins(), ["work two", "main"]);
        assert_eq!(
            std::fs::read_to_string(d.join("srv/pins")).unwrap(),
            "work two\nmain\n"
        );
        s.rename_pin("main", "home").unwrap();
        assert_eq!(s.pins(), ["work two", "home"]);
        s.set_pin("work two", false).unwrap();
        assert_eq!(s.pins(), ["home"]);
        assert_eq!(parse_pins("a\n\na\r\nb\n"), ["a", "b"]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn width_is_a_percentage() {
        let (s, d) = temp();
        assert_eq!(s.width(), None);
        s.set_width(23).unwrap();
        assert_eq!(s.width(), Some(23));
        assert_eq!(
            std::fs::read_to_string(d.join("srv/sidebar-width")).unwrap(),
            "23%\n"
        );
        for bad in ["", "23", "0%", "100%", "x%", "-3%"] {
            assert_eq!(parse_pct(bad), None, "{bad}");
        }
        assert_eq!(parse_pct(" 40%\n"), Some(40));
        let _ = std::fs::remove_dir_all(d);
    }
}
