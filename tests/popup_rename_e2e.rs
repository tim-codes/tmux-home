//! Renaming from the popup: ^r names the pane (`@home_pane_name`, shown
//! over the title an agent keeps rewriting), ^S-r / M-R the window. And
//! the key protocol: Ctrl+Shift+R is only told apart from ^r when tmux
//! reports extended keys in the csi-u format, which the popup asks for
//! (modifyOtherKeys mode 2). Ctrl+Shift+R is pressed on the inner
//! server's client with `send-keys -K` (the outer terminal of the test
//! sends it as plain ^R); other keys through the outer client as usual,
//! so they too reach the popup re-encoded in mode 2. (`-K` delivers only
//! modified keys to a popup.)
mod common;
use common::*;

/// The inner server's (only) client.
fn client(s: &TestServer) -> String {
    s.tmux(&["list-clients", "-F", "#{client_name}"])
        .lines()
        .next()
        .unwrap()
        .to_string()
}

/// A key pressed on the inner client (tmux key name, `C-S-r`).
fn press(s: &TestServer, key: &str) {
    let c = client(s);
    s.tmux(&["send-keys", "-c", &c, "-K", key]);
    std::thread::sleep(KEY_GAP);
}

#[test]
fn ctrl_r_names_the_pane_over_its_title() {
    let (_env, s, o) = popup_fixture();
    let pane = fmt(&s, "alpha:editor", "#{pane_id}");
    s.tmux(&["select-pane", "-t", &pane, "-T", "✳ Agent title"]);
    o.open_popup();
    o.wait_cursor_on("editor");
    // a window of one pane: ^r names that pane, from its title
    o.keys(&["C-r"]);
    o.wait_for("^rename pane › ✳ Agent title");
    o.keys(&["C-u"]);
    o.typed("api work");
    o.keys(&["Enter"]);
    wait_until("the pane named", || {
        fmt(&s, &pane, "#{@home_pane_name}") == "api work"
    });
    o.wait_for("api work");
    // the agent rewrites its title: the name stays
    s.tmux(&["select-pane", "-t", &pane, "-T", "✳ Another title"]);
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(o.screen().contains("api work"), "{}", o.screen());
    assert!(!o.screen().contains("Another title"), "{}", o.screen());
    assert_eq!(
        fmt(&s, &pane, "#{window_name}"),
        "editor",
        "window untouched"
    );
    // empty clears it: the title shows again
    o.keys(&["C-r"]);
    o.wait_for("^rename pane › api work");
    o.keys(&["C-u"]);
    o.keys(&["Enter"]);
    o.wait_for("Another title");
    assert_eq!(fmt(&s, &pane, "#{@home_pane_name}"), "");
    // M-R renames the window from the same line
    o.keys(&["M-R"]);
    o.wait_for("^rename › editor");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
}

/// extended-keys on, csi-u: the popup turns on mode 2 and reads ^S-r as
/// the window key, ^r as the pane key; typing, Enter and Esc still work.
#[test]
fn ctrl_shift_r_is_the_window_key_with_csi_u_extended_keys() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["set-option", "-s", "extended-keys", "on"]);
    s.tmux(&["set-option", "-s", "extended-keys-format", "csi-u"]);
    let target = wid(&s, "alpha:editor");
    o.open_popup();
    o.wait_cursor_on("editor");
    press(&s, "C-S-r");
    o.wait_for("^rename › editor");
    o.keys(&["C-u"]);
    o.typed("via ctrl shift r");
    o.keys(&["Enter"]);
    wait_until("renamed by ^S-r", || {
        fmt(&s, &target, "#{window_name}") == "via ctrl shift r"
    });
    // ^r still the pane
    o.keys(&["C-r"]);
    o.wait_for("^rename pane ›");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
    // and M-R, and list keys with modifiers (C-n moves)
    press(&s, "M-R");
    o.wait_for("^rename › via ctrl shift r");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
    o.keys(&["C-n"]);
    o.wait_cursor_on_pane("");
    o.typed("win");
    o.wait_for(r"^> win .*1/2");
}

/// The xterm format (`CSI 27;6;114~`) is one crossterm can't read, so
/// the popup leaves extended keys off: ^S-r arrives as ^r (the pane), and
/// M-R is the window key.
#[test]
fn with_the_xterm_format_ctrl_shift_r_is_ctrl_r_and_m_r_renames_the_window() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["set-option", "-s", "extended-keys", "on"]);
    s.tmux(&["set-option", "-s", "extended-keys-format", "xterm"]);
    o.open_popup();
    o.wait_cursor_on("editor");
    press(&s, "C-S-r");
    o.wait_for("^rename pane ›");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
    press(&s, "M-R");
    o.wait_for("^rename › editor");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*2/2");
    // other keys unaffected
    o.keys(&["C-n"]);
    o.wait_cursor_on_pane("");
}
