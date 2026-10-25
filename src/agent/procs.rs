//! Agents found by process rather than by hooks. A pane's
//! `pane_current_command` is its foreground process group's leader, so an
//! agent started by a wrapper (`sh -c "claude-kickoff …; exec $SHELL"`)
//! that leaves it in its shell's group reads as `fish`; the process tree
//! under `pane_pid` still has it. Claude Code runs as its versioned
//! binary (`…/versions/2.1.310`, so a process named like a version) or as
//! `…/claude`.
//!
//! `annotate` runs once per snapshot read: one listing of the process
//! table (`ProcTable::read`), every pane's subtree walked in it, and for an
//! agent found whose account isn't otherwise known, its environment read
//! once per process (`CLAUDE_CONFIG_DIR`, cached by pid and start time) to
//! tell Kimi (Claude Code on the OpenRouter account) from Claude.

use crate::tmux::snapshot::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};

/// A Claude Code process found under a pane.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct ProcAgent {
    pub pid: u32,
    /// `CLAUDE_CONFIG_DIR` from its environment: read only when the pane
    /// carries neither a launcher marker (`@agent`) nor the hooks' account
    /// (`@home_config_dir`); `None` when unread, unset or unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

/// One process: its parent, name and start time (0: unknown).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Proc {
    ppid: u32,
    name: String,
    start: u64,
}

/// The process table: every process's parent and name, and each parent's
/// children.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcTable {
    procs: HashMap<u32, Proc>,
    children: HashMap<u32, Vec<u32>>,
}

impl ProcTable {
    /// From `(pid, ppid, name)` triples.
    pub fn from_entries(it: impl IntoIterator<Item = (u32, u32, String)>) -> ProcTable {
        ProcTable::with_starts(it.into_iter().map(|(p, pp, n)| (p, pp, n, 0)))
    }

    fn with_starts(it: impl IntoIterator<Item = (u32, u32, String, u64)>) -> ProcTable {
        let mut t = ProcTable::default();
        for (pid, ppid, name, start) in it {
            if pid != ppid {
                t.children.entry(ppid).or_default().push(pid);
            }
            t.procs.insert(pid, Proc { ppid, name, start });
        }
        for v in t.children.values_mut() {
            v.sort_unstable();
        }
        t
    }

    /// `ps -A -o pid=,ppid=,comm=` output (the name may hold spaces);
    /// unparseable lines are skipped.
    pub fn parse_ps(out: &str) -> ProcTable {
        ProcTable::from_entries(out.lines().filter_map(|l| {
            let l = l.trim_start();
            let (pid, rest) = l.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (ppid, name) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            Some((
                pid.parse().ok()?,
                ppid.parse().ok()?,
                name.trim().to_string(),
            ))
        }))
    }

    /// The live table; `None` if it can't be read.
    pub fn read() -> Option<ProcTable> {
        #[cfg(target_os = "macos")]
        {
            macos::table()
        }
        #[cfg(not(target_os = "macos"))]
        {
            let out = std::process::Command::new("ps")
                .args(["-A", "-o", "pid=,ppid=,comm="])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| ProcTable::parse_ps(&String::from_utf8_lossy(&out.stdout)))
        }
    }

    /// The nearest agent process at or under `root` (breadth-first, lowest
    /// pid first among equals).
    pub fn agent_under(&self, root: u32) -> Option<u32> {
        let mut seen = HashSet::new();
        let mut q = VecDeque::from([root]);
        while let Some(pid) = q.pop_front() {
            if !seen.insert(pid) {
                continue;
            }
            if self.procs.get(&pid).is_some_and(|p| is_agent_name(&p.name)) {
                return Some(pid);
            }
            q.extend(self.children.get(&pid).into_iter().flatten());
        }
        None
    }

    fn start(&self, pid: u32) -> u64 {
        self.procs.get(&pid).map_or(0, |p| p.start)
    }

    #[cfg(test)]
    fn parent(&self, pid: u32) -> Option<u32> {
        self.procs.get(&pid).map(|p| p.ppid)
    }
}

/// A process name (or executable path) that is Claude Code's: `claude`,
/// or a version (`2.1.310`, the name of its versioned binary, which it
/// also sets as its process title).
pub fn is_agent_name(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    // a login shell's `-` prefix never applies to claude, but costs nothing
    let base = base.trim_start_matches('-');
    base == "claude" || super::is_version(base)
}

/// The variable that names Claude Code's account directory.
pub const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";

/// `CLAUDE_CONFIG_DIR` of each process looked at, by (pid, start time):
/// a process's environment doesn't change after it starts.
type EnvCache = HashMap<(u32, u64), Option<String>>;

static ENV_CACHE: LazyLock<Mutex<EnvCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Finds each work pane's agent process in one read of the process table.
/// Sidebar panes (views, not work) are never walked. A process found
/// gets its account from its environment only when the pane's options
/// don't already say (`@agent`, `@home_config_dir`).
pub fn annotate(s: &mut Snapshot) {
    if !s.panes.iter().any(|p| p.pid != 0 && p.role != "sidebar") {
        return;
    }
    let Some(t) = ProcTable::read() else {
        return;
    };
    annotate_with(s, &t, &mut |pid| read_env(pid, CONFIG_DIR_VAR));
}

/// `annotate` against table `t`, reading `CLAUDE_CONFIG_DIR` through `env`
/// (cached; a pid's entry goes once the process is gone).
pub fn annotate_with(s: &mut Snapshot, t: &ProcTable, env: &mut dyn FnMut(u32) -> Option<String>) {
    let mut cache = ENV_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(pid, start), _| t.start(*pid) == *start && t.procs.contains_key(pid));
    for p in s.panes.iter_mut().filter(|p| p.role != "sidebar") {
        p.proc_agent = None;
        if p.pid == 0 {
            continue;
        }
        let Some(apid) = t.agent_under(p.pid) else {
            continue;
        };
        let known = [super::MARKER, super::CONFIG_DIR]
            .iter()
            .any(|k| p.agent_opts.get(*k).is_some_and(|v| !v.is_empty()));
        let config_dir = if known {
            None
        } else {
            cache
                .entry((apid, t.start(apid)))
                .or_insert_with(|| env(apid))
                .clone()
        };
        p.proc_agent = Some(ProcAgent {
            pid: apid,
            config_dir,
        });
    }
}

/// Variable `name` in process `pid`'s environment; `None` when unset or
/// unreadable (another user's process).
pub fn read_env(pid: u32, name: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        env_from_procargs(&macos::procargs(pid)?, name)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        env_from_list(raw.split(|&b| b == 0), name)
    }
}

/// `name`'s value among `NAME=value` byte strings.
fn env_from_list<'a>(vars: impl Iterator<Item = &'a [u8]>, name: &str) -> Option<String> {
    let key = format!("{name}=");
    vars.filter_map(|v| v.strip_prefix(key.as_bytes()))
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .next()
}

/// `name`'s value in a `KERN_PROCARGS2` buffer: `argc` (a native-endian
/// int), the executable path, NUL padding, `argc` arguments, then the
/// environment, each NUL-terminated.
pub fn env_from_procargs(buf: &[u8], name: &str) -> Option<String> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let mut rest = &buf[4..];
    // the executable path, then its padding
    let end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[end..];
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    let mut items = rest.split(|&b| b == 0);
    for _ in 0..argc.max(0) {
        items.next()?;
    }
    env_from_list(items.take_while(|v| !v.is_empty()), name)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::ProcTable;
    use std::mem::size_of;

    /// Every process the caller may see: pid, parent, name, start time.
    pub fn table() -> Option<ProcTable> {
        // SAFETY: proc_listallpids with a null buffer returns the count;
        // the second call fills at most `cap` pids into a buffer that long.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return None;
        }
        let cap = n as usize + 64;
        let mut pids = vec![0 as libc::pid_t; cap];
        let bytes = (cap * size_of::<libc::pid_t>()) as libc::c_int;
        let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if got <= 0 {
            return None;
        }
        pids.truncate((got as usize).min(cap));
        let entries = pids.into_iter().filter(|&p| p > 0).filter_map(|pid| {
            // SAFETY: an all-zero proc_bsdinfo is valid; proc_pidinfo
            // writes at most its size.
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let r = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
            };
            if r != size {
                return None;
            }
            // `pbi_name` is the longer (32-byte) name; `pbi_comm` the
            // executable's, 16 bytes
            let name = cstr(&info.pbi_name);
            let name = if name.is_empty() {
                cstr(&info.pbi_comm)
            } else {
                name
            };
            Some((pid as u32, info.pbi_ppid, name, info.pbi_start_tvsec))
        });
        Some(ProcTable::with_starts(entries))
    }

    fn cstr(b: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = b
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The `KERN_PROCARGS2` buffer of `pid` (arguments and environment);
    /// `None` for another user's process.
    pub fn procargs(pid: u32) -> Option<Vec<u8>> {
        let mut max: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: sysctl writes at most `len` bytes into each buffer.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&raw mut max).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 || max <= 0 {
            return None;
        }
        let mut buf = vec![0u8; max as usize];
        let mut len = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return None;
        }
        buf.truncate(len);
        Some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::snapshot::Pane;

    /// fish (10, the pane's) → claude (11) → its own children; a version-
    /// named claude (21) straight under a login shell (20); a bare shell
    /// (30) running nvim (31); a sh → fish → claude chain (40 → 41 → 42).
    const PS: &str = "    1     0 launchd
   10     1 fish
   11    10 /Users/u/.local/bin/claude
   12    11 npm exec @mantine/mcp-server
   13    11 /bin/zsh
   20     1 -fish
   21    20 2.1.310
   30     1 fish
   31    30 nvim
   40     1 sh
   41    40 fish
   42    41 claude
   50     1 bash
   51    50 /Applications/My App.app/Contents/MacOS/my app
";

    fn table() -> ProcTable {
        ProcTable::parse_ps(PS)
    }

    #[test]
    fn parses_ps_with_spaces_in_names() {
        let t = table();
        assert_eq!(t.parent(51), Some(50));
        assert_eq!(
            t.procs[&51].name,
            "/Applications/My App.app/Contents/MacOS/my app"
        );
        assert_eq!(t.parent(1), Some(0));
        assert!(ProcTable::parse_ps("garbage\n  x y z\n").procs.is_empty());
    }

    #[test]
    fn agent_names() {
        for n in [
            "claude",
            "/Users/u/.local/bin/claude",
            "2.1.310",
            "10.0.1",
            "-claude",
        ] {
            assert!(is_agent_name(n), "{n}");
        }
        for n in [
            "fish",
            "node",
            "claude-kickoff",
            "2.1",
            "v2.1.3",
            "/bin/claude.sh",
            "",
        ] {
            assert!(!is_agent_name(n), "{n}");
        }
    }

    #[test]
    fn finds_the_agent_under_a_pane() {
        let t = table();
        assert_eq!(t.agent_under(10), Some(11), "claude, a child of fish");
        assert_eq!(t.agent_under(20), Some(21), "version-named process");
        assert_eq!(t.agent_under(40), Some(42), "sh → fish → claude");
        assert_eq!(t.agent_under(42), Some(42), "the pane's own process");
        assert_eq!(t.agent_under(30), None);
        assert_eq!(t.agent_under(999), None, "no such pid");
    }

    #[test]
    fn a_parent_cycle_ends() {
        let t = ProcTable::from_entries([(5, 6, "a".into()), (6, 5, "b".into())]);
        assert_eq!(t.agent_under(5), None);
    }

    fn pane(id: &str, pid: u32, opts: &[(&str, &str)]) -> Pane {
        let mut p = crate::agent::tests::pane(id, "fish", opts);
        p.pid = pid;
        p
    }

    #[test]
    fn annotate_marks_agents_and_reads_the_account_only_when_unknown() {
        let mut s = Snapshot {
            panes: vec![
                pane("%1", 10, &[]),
                pane("%2", 40, &[("@agent", "kimi")]),
                pane("%3", 20, &[("@home_config_dir", "/u/.claude-x")]),
                pane("%4", 30, &[]),
                pane("%5", 0, &[]),
            ],
            ..Default::default()
        };
        let mut sidebar = pane("%6", 10, &[]);
        sidebar.role = "sidebar".into();
        s.panes.push(sidebar);
        let mut asked = Vec::new();
        annotate_with(&mut s, &table(), &mut |pid| {
            asked.push(pid);
            Some(format!("/env/{pid}"))
        });
        let got: Vec<_> = s.panes.iter().map(|p| p.proc_agent.clone()).collect();
        assert_eq!(
            got,
            [
                Some(ProcAgent {
                    pid: 11,
                    config_dir: Some("/env/11".into())
                }),
                Some(ProcAgent {
                    pid: 42,
                    config_dir: None
                }),
                Some(ProcAgent {
                    pid: 21,
                    config_dir: None
                }),
                None,
                None,
                None, // a sidebar pane is never walked
            ]
        );
        assert_eq!(asked, [11], "env read only where the pane doesn't say");
    }

    #[test]
    fn procargs_env() {
        let mut b = 2i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/usr/bin/claude\0\0\0\0claude\0--resume\0HOME=/u\0CLAUDE_CONFIG_DIR=/u/.claude-exxo-openrouter\0\0junk=1\0");
        assert_eq!(
            env_from_procargs(&b, CONFIG_DIR_VAR).as_deref(),
            Some("/u/.claude-exxo-openrouter")
        );
        assert_eq!(env_from_procargs(&b, "HOME").as_deref(), Some("/u"));
        assert_eq!(env_from_procargs(&b, "junk"), None, "after the env's end");
        // an argument that looks like a variable is not one
        let mut b = 1i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/x\0CLAUDE_CONFIG_DIR=/arg\0HOME=/u\0");
        assert_eq!(env_from_procargs(&b, CONFIG_DIR_VAR), None);
        assert_eq!(env_from_procargs(&[1, 2], CONFIG_DIR_VAR), None);
    }

    /// The live reader sees this test process, its parent, and its own
    /// environment.
    #[test]
    fn reads_the_live_table_and_own_env() {
        let t = ProcTable::read().expect("process table");
        let me = std::process::id();
        assert!(t.procs.contains_key(&me));
        assert_eq!(t.parent(me), Some(std::os::unix::process::parent_id()));
        let path = std::env::var("PATH").unwrap_or_default();
        assert_eq!(read_env(me, "PATH").unwrap_or_default(), path);
    }
}
