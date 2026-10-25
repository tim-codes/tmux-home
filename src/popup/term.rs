//! Terminal modes the popup turns on beyond ratatui's (mouse reporting),
//! and turning them off again on every way out — including a panic, which
//! ratatui's own hook (raw mode, alternate screen) doesn't cover. Also the
//! quiet section the preview parser runs in: a panic there is caught and
//! must print nothing over the TUI.

use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    queue,
};
use std::{
    cell::Cell,
    io::Write,
    panic::UnwindSafe,
    sync::{
        Once,
        atomic::{AtomicBool, Ordering},
    },
};

/// Mouse reporting is on (a `MouseGuard` is alive).
static MOUSE_ON: AtomicBool = AtomicBool::new(false);
/// modifyOtherKeys mode 2 is on (an `ExtendedKeys` that asked is alive).
static KEYS_ON: AtomicBool = AtomicBool::new(false);

/// Writes the sequence that resets modifyOtherKeys.
pub fn keys_off(out: &mut impl Write) {
    let _ = out.write_all(b"\x1b[>4m");
    let _ = out.flush();
}

thread_local! {
    /// Set while a `quietly` call runs on this thread.
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Writes the sequences that turn mouse reporting off.
pub fn mouse_off(out: &mut impl Write) {
    let _ = queue!(out, DisableMouseCapture);
    let _ = out.flush();
}

/// Mouse reporting on until the guard drops: every return from the event
/// loop (switch, quit, an error) goes through its `Drop`; a panic through
/// the hook `install_panic_hook` sets.
pub struct MouseGuard;

impl MouseGuard {
    pub fn on() -> MouseGuard {
        let mut out = std::io::stdout();
        let _ = queue!(out, EnableMouseCapture);
        let _ = out.flush();
        MOUSE_ON.store(true, Ordering::SeqCst);
        MouseGuard
    }
}

impl MouseGuard {
    /// Mouse reporting off now (idempotent: the drop after it writes
    /// nothing). For exits that leave the alternate screen before the
    /// guard drops (`⏎`'s switch).
    pub fn off() {
        if MOUSE_ON.swap(false, Ordering::SeqCst) {
            mouse_off(&mut std::io::stdout());
        }
    }
}

impl Drop for MouseGuard {
    fn drop(&mut self) {
        MouseGuard::off();
    }
}

/// The panic hook's own part: nothing for a panic inside `quietly` (it is
/// caught, and the popup carries on); otherwise the mouse off if it is
/// on, and modifyOtherKeys reset if it was asked for. True when the
/// previous hook should run.
pub fn on_panic(quiet: bool, mouse_on: bool, keys_on: bool, out: &mut impl Write) -> bool {
    if quiet {
        return false;
    }
    if mouse_on {
        mouse_off(out);
    }
    if keys_on {
        keys_off(out);
    }
    true
}

/// xterm's modifyOtherKeys mode 2 (`CSI > 4;2m`) while alive, reset
/// (`CSI > 4m`) on drop. tmux (`extended-keys on`) then reports every
/// modified key in its extended form, so Ctrl+Shift+R is no longer plain
/// ^r. Only asked for when tmux's `extended-keys-format` is `csi-u`
/// (`wanted`): crossterm reads `CSI 114;6u`, but drops the xterm form
/// (`CSI 27;6;114~`) — and in mode 2 that would be every Ctrl and Alt key.
/// Reset on drop, and by the panic hook (`on_panic`).
pub struct ExtendedKeys;

impl ExtendedKeys {
    /// tmux's `extended-keys` (`on`/`always`) and `extended-keys-format`
    /// allow mode 2 to be read.
    pub fn wanted(extended_keys: &str, format: &str) -> bool {
        matches!(extended_keys.trim(), "on" | "always") && format.trim() == "csi-u"
    }

    pub fn on(want: bool) -> ExtendedKeys {
        if want {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[>4;2m");
            let _ = out.flush();
            KEYS_ON.store(true, Ordering::SeqCst);
        }
        ExtendedKeys
    }
}

impl Drop for ExtendedKeys {
    fn drop(&mut self) {
        if KEYS_ON.swap(false, Ordering::SeqCst) {
            keys_off(&mut std::io::stdout());
        }
    }
}

/// Chains a hook in front of the current one, once per process. In the
/// popup it is installed right after `ratatui::init`, so a real panic
/// turns the mouse off, then ratatui restores the terminal and the default
/// hook prints; a `quietly` panic skips all three.
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let (quiet, mouse) = (QUIET.with(Cell::get), MOUSE_ON.load(Ordering::SeqCst));
            // a caught (`quietly`) panic leaves the mode on
            let keys = !quiet && KEYS_ON.swap(false, Ordering::SeqCst);
            if on_panic(quiet, mouse, keys, &mut std::io::stdout()) {
                prev(info);
            }
        }));
    });
}

/// Runs `f`, catching a panic silently (`None`). Installs the hook if
/// nothing has yet (tests; the popup has by the time it parses).
pub fn quietly<T>(f: impl FnOnce() -> T + UnwindSafe) -> Option<T> {
    install_panic_hook();
    let was = QUIET.with(|q| q.replace(true));
    let r = std::panic::catch_unwind(f);
    QUIET.with(|q| q.set(was));
    r.ok()
}

/// A test hook: `TMUX_HOME_TEST_PANIC=<at>` makes the popup panic at
/// `at` (`loop`: the event loop after its first draw; `parse`: every
/// preview parse), so the e2e tests can watch the terminal be restored.
pub fn test_panic(at: &str) -> bool {
    static AT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    AT.get_or_init(|| std::env::var("TMUX_HOME_TEST_PANIC").ok())
        .as_deref()
        == Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_keys_only_where_crossterm_reads_them() {
        assert!(ExtendedKeys::wanted("on", "csi-u"));
        assert!(ExtendedKeys::wanted("always", "csi-u\n"));
        assert!(!ExtendedKeys::wanted("on", "xterm"));
        assert!(!ExtendedKeys::wanted("off", "csi-u"));
        assert!(!ExtendedKeys::wanted("", ""));
    }

    #[test]
    fn the_hook_turns_the_mouse_off() {
        let mut out = Vec::new();
        assert!(on_panic(false, false, false, &mut out));
        assert!(out.is_empty(), "nothing when the mouse isn't on");
        assert!(on_panic(false, true, false, &mut out));
        let s = String::from_utf8(out).unwrap();
        for seq in ["\x1b[?1000l", "\x1b[?1002l", "\x1b[?1003l", "\x1b[?1006l"] {
            assert!(s.contains(seq), "no {seq:?} in {s:?}");
        }
    }

    #[test]
    fn a_quiet_panic_writes_nothing_and_skips_the_previous_hook() {
        let mut out = Vec::new();
        assert!(!on_panic(true, true, true, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn the_hook_resets_extended_keys() {
        let mut out = Vec::new();
        assert!(on_panic(false, false, true, &mut out));
        assert_eq!(out, b"\x1b[>4m");
    }

    #[test]
    fn quietly_catches_and_resets_the_flag() {
        assert_eq!(quietly(|| 1), Some(1));
        assert_eq!(quietly(|| -> i32 { panic!("boom") }), None);
        assert!(!QUIET.with(Cell::get));
    }
}
