//! The real popup managing windows: close (idle, busy, last of a session,
//! last on the server), reopen, reorder, new window, live redraw, and the
//! daemon going away (bash suite port, part 2).
mod common;
use common::*;
use std::time::Duration;
use tmux_home::{paths::Paths, store::Store};

/// Filters to `name`, closes it with ^x (it must close at once) and clears
/// the filter.
fn close_idle(o: &Outer, name: &str) {
    o.typed(name);
    o.wait_for(&format!(r"^> {name} .*1/"));
    o.keys(&["C-x"]);
    o.wait_for(&format!(r"^> {name} .*0/"));
    o.keys(&["Escape"]);
}

fn store(s: &TestServer) -> Store {
    Store::for_socket(&s.socket).unwrap()
}

#[test]
fn close_idle_current_and_cursor() {
    let (_env, s, o) = popup_fixture();
    for n in ["qone", "qtwo", "qnext"] {
        s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", n]);
    }
    s.tmux(&[
        "send-keys",
        "-t",
        "alpha:qnext",
        "echo QNEXT-MARKER",
        "Enter",
    ]);
    // not -d: the client's current window
    s.tmux(&["new-window", "-t", "alpha:", "-n", "qcur"]);
    s.wait_settled();
    let qcur = wid(&s, "alpha:qcur");
    o.open_popup();
    o.wait_for(&format!(
        "tmux-home +alpha ▸ {}",
        fmt(&s, &qcur, "#{window_index}")
    ));
    o.wait_for(r"^▌alpha ▶ +\d+  qcur");

    // the client's current, idle window closes at once; the client moves
    // to another alpha window and the popup stays
    o.keys(&["C-x"]);
    o.wait_for(r"^> +.*7/7");
    assert!(!window_ids(&s).contains(&qcur));
    assert!(
        !s.tmux(&["list-clients", "-F", "#{client_name}"])
            .trim()
            .is_empty(),
        "client still attached"
    );
    let at = client_at(&s);
    assert!(at.starts_with("alpha:") && at != "alpha:qcur", "{at}");
    let cur = s
        .tmux(&["list-clients", "-F", "#{window_id}"])
        .trim()
        .to_string();
    o.wait_for(&format!(
        "tmux-home +alpha ▸ {}",
        fmt(&s, &cur, "#{window_index}")
    ));
    assert!(o.popup_open());
    assert_eq!(store(&s).len().unwrap(), 1, "closing pushed its shape");

    // an idle window closes at once, the filter stays
    let qone = wid(&s, "alpha:qone");
    o.typed("qone");
    o.wait_for(r"^> qone .*1/7");
    o.keys(&["C-x"]);
    o.wait_for(r"^> qone .*0/6");
    assert!(!window_ids(&s).contains(&qone));
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*6/6");

    // closing a row leaves the cursor on the next one
    o.keys(&["Down", "Down"]);
    o.wait_cursor_on("qtwo");
    o.keys(&["C-x"]);
    o.wait_for(r"^> +.*5/5");
    o.wait_cursor_on("qnext");
    o.wait_for("QNEXT-MARKER");
    assert!(!window_names(&s, "alpha").contains(&"qtwo".to_string()));

    // ^x does nothing while the rename editor is open
    o.keys(&["C-r"]);
    o.wait_for("^rename › qnext");
    o.keys(&["C-x"]);
    std::thread::sleep(Duration::from_millis(500));
    assert!(o.screen().contains("rename › qnext"), "{}", o.screen());
    assert!(window_names(&s, "alpha").contains(&"qnext".to_string()));
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*5/5");
}

#[test]
fn close_asks_when_busy() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "alpha:",
        "-n",
        "qbusy",
        "sleep 1000",
    ]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qside"]);
    s.tmux(&[
        "split-window",
        "-d",
        "-h",
        "-t",
        "alpha:qside",
        "sleep 1000",
    ]);
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "alpha:qside.1",
        "@pane_role",
        "sidebar",
    ]);
    s.wait_settled();
    let qbusy = wid(&s, "alpha:qbusy");
    o.open_popup();
    o.typed("qbusy");
    o.wait_for(r"^> qbusy .*1/6");
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "qbusy"\? running: sleep \(y/N\)"#);
    o.typed("n");
    o.wait_for(r"^> qbusy .*1/6");
    assert!(window_ids(&s).contains(&qbusy), "kept after n");
    for cancel in ["Escape", "Enter"] {
        o.keys(&["C-x"]);
        o.wait_for(r#"^close "qbusy"\?"#);
        o.keys(&[cancel]);
        o.wait_for(r"^> qbusy .*1/6");
        assert!(window_ids(&s).contains(&qbusy), "kept after {cancel}");
    }
    assert!(o.popup_open());
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "qbusy"\?"#);
    o.typed("y");
    o.wait_for(r"^> qbusy .*0/5");
    assert!(!window_ids(&s).contains(&qbusy));
    o.keys(&["Escape"]);
    // a sidebar pane running a program doesn't make the window busy
    o.typed("qside");
    o.wait_for(r"^> qside .*1/5");
    o.keys(&["C-x"]);
    o.wait_for(r"^> qside .*0/4");
}

/// A job stopped with ^z leaves the shell in the foreground, so the pane
/// looks idle; ^x must still ask.
#[test]
fn close_asks_when_a_job_is_stopped() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qstop"]);
    s.wait_settled();
    s.tmux(&["send-keys", "-t", "alpha:qstop", "sleep 1000", "Enter"]);
    wait_until("sleep running", || {
        fmt(&s, "alpha:qstop", "#{pane_current_command}") == "sleep"
    });
    s.tmux(&["send-keys", "-t", "alpha:qstop", "C-z"]);
    wait_until("sleep stopped", || {
        fmt(&s, "alpha:qstop", "#{pane_current_command}") != "sleep"
    });
    let w = wid(&s, "alpha:qstop");
    o.open_popup();
    o.typed("qstop");
    o.wait_for(r"^> qstop .*1/5");
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "qstop"\? running: sleep \(stopped\) \(y/N\)"#);
    o.keys(&["Escape"]);
    o.wait_for(r"^> qstop .*1/5");
    assert!(window_ids(&s).contains(&w));
}

#[test]
fn reopen_restores_and_selects() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qnext"]);
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "qside"]);
    s.tmux(&[
        "split-window",
        "-d",
        "-h",
        "-t",
        "alpha:qside",
        "sleep 1000",
    ]);
    s.tmux(&[
        "set-option",
        "-p",
        "-t",
        "alpha:qside.1",
        "@pane_role",
        "sidebar",
    ]);
    s.wait_settled();
    o.open_popup();
    close_idle(&o, "qside");
    close_idle(&o, "qnext");
    o.wait_for(r"^> +.*4/4");
    let before = client_at(&s);

    // ^t reopens the last closed (qnext), selects it, doesn't switch
    o.keys(&["C-t"]);
    o.wait_for(r"^> +.*5/5");
    assert!(window_names(&s, "alpha").contains(&"qnext".to_string()));
    assert_eq!(client_at(&s), before, "client not switched");
    o.wait_cursor_on("qnext");

    // from a filter that hides it: the filter clears, the cursor is on it
    o.typed("build");
    o.wait_for(r"^> build .*1/5");
    o.keys(&["C-t"]);
    o.wait_for(r"^> +.*6/6");
    o.wait_cursor_on("qside");
    // a reopened window has fresh shells: the sidebar is not rebuilt
    assert_eq!(fmt(&s, "alpha:qside", "#{window_panes}"), "1");

    close_idle(&o, "qnext");
    close_idle(&o, "qside");
    o.wait_for(r"^> +.*4/4");
    let st = store(&s);
    while st.pop().unwrap().is_some() {}
    o.keys(&["C-t"]);
    o.wait_for("nothing to reopen");
    assert_eq!(window_ids(&s).len(), 4, "nothing created");
    o.typed("x");
    o.wait_gone("nothing to reopen");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
}

#[test]
fn close_last_window_of_client_session() {
    let (_env, s, o) = popup_fixture();
    s.tmux(&["new-session", "-d", "-s", "qdelta", "-n", "qd"]);
    let client = s
        .tmux(&["list-clients", "-F", "#{client_name}"])
        .lines()
        .next()
        .unwrap()
        .to_string();
    s.tmux(&["switch-client", "-c", &client, "-t", "qdelta"]);
    s.wait_settled();
    o.open_popup();
    o.wait_for("tmux-home +qdelta ▸ 0");
    o.keys(&["C-x"]);
    o.wait_for(r#"^close "qd"\? — session "qdelta" will end \(y/N\)"#);
    o.typed("y");
    o.wait_for(r"^> +.*4/4");
    assert!(!sessions(&s).contains(&"qdelta".to_string()));
    assert!(
        !s.tmux(&["list-clients", "-F", "#{client_name}"])
            .trim()
            .is_empty(),
        "client still attached"
    );
    let at = client_at(&s);
    assert!(at.starts_with("alpha:") || at.starts_with("beta:"), "{at}");
    o.wait_for(r"tmux-home +(alpha|beta) ▸ \d");
    assert!(o.popup_open());

    // and ^t brings the session back, without switching the client
    o.keys(&["C-t"]);
    o.wait_for(r"^> +.*5/5");
    o.wait_cursor_on("qd");
    assert!(sessions(&s).contains(&"qdelta".to_string()));
    assert_eq!(client_at(&s), at);
}

#[test]
fn close_refuses_the_last_window_on_the_server() {
    let env = TestEnv::new();
    let s = TestServer::start();
    s.tmux(&["set-option", "-g", "escape-time", "50"]);
    install_binding(&s);
    let o = Outer::attach(&s, "alpha", 200, 50);
    let w = wid(&s, "alpha:");
    o.open_popup();
    o.keys(&["C-x"]);
    o.wait_for("can't close the last window on the server");
    assert_eq!(window_ids(&s), [w]);
    assert!(o.popup_open());
    assert!(store(&s).is_empty().unwrap(), "nothing pushed");
    drop(o);
    drop(s);
    drop(env);
}

#[test]
fn reorder_and_new_window() {
    let (_env, s, o) = popup_fixture();
    let editor = wid(&s, "alpha:editor");
    o.open_popup();
    o.keys(&["M-Down"]);
    wait_until("editor at index 1", || {
        fmt(&s, &editor, "#{window_index}") == "1"
    });
    o.wait_for(r"^▌alpha ▶ +1  editor");
    assert_eq!(
        s.tmux(&["list-clients", "-F", "#{window_id}"]).trim(),
        editor,
        "the client stays on its window"
    );
    // M-↓ at the session's edge does nothing
    o.keys(&["M-Down"]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fmt(&s, &editor, "#{window_index}"), "1");
    o.keys(&["M-Up"]);
    wait_until("editor back at index 0", || {
        fmt(&s, &editor, "#{window_index}") == "0"
    });
    o.wait_for(r"^▌alpha ▶ +0  editor");

    o.keys(&["M-n"]);
    o.wait_for("^new window ›");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*4/4");
    assert_eq!(window_ids(&s).len(), 4, "Esc creates nothing");
    o.keys(&["M-n"]);
    o.wait_for("^new window ›");
    o.typed("fresh #1");
    o.keys(&["Enter"]);
    wait_until("fresh #1 after editor", || {
        window_names(&s, "alpha") == ["editor", "fresh #1", "win two"]
    });
    o.wait_cursor_on("fresh #1");
    o.wait_for(r"^> +.*5/5");
    assert_eq!(
        s.tmux(&["list-clients", "-F", "#{window_id}"]).trim(),
        editor,
        "a new window does not take the client"
    );
}

#[test]
fn live_redraw_keeps_the_editor() {
    let (_env, s, o) = popup_fixture();
    o.open_popup();
    o.keys(&["C-r"]);
    o.wait_for("^rename › editor");
    o.typed("-x");
    s.tmux(&["new-window", "-d", "-t", "beta:", "-n", "pushed"]);
    o.wait_for("pushed");
    o.wait_for("^rename › editor-x");
    o.keys(&["Escape"]);
    o.wait_for(r"^> +.*5/5");
    o.keys(&["Down"]);
    o.wait_cursor_on("win two");
    o.keys(&["C-r"]);
    o.wait_for("^rename › win two");
    s.tmux(&["kill-window", "-t", "alpha:1"]);
    o.wait_for("that window has gone");
    o.wait_for(r"^> +.*4/4");
}

fn kill_daemon(s: &TestServer) {
    let pat = format!("tmux-home daemon --socket {}", s.socket.display());
    let _ = std::process::Command::new("pkill")
        .args(["-KILL", "-f", &pat])
        .status();
    let sock = Paths::for_socket(&s.socket).unwrap().sock;
    wait_until("daemon gone", || {
        std::os::unix::net::UnixStream::connect(&sock).is_err()
    });
}

/// The daemon dies under an open popup: the popup reads tmux itself (the
/// header says so), still sees changes, and its own writes show at once.
#[test]
fn daemon_death_falls_back_to_polling() {
    let (_env, s, o) = popup_fixture();
    let sock = Paths::for_socket(&s.socket).unwrap().sock;
    wait_until("daemon up", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });
    // the popup must not be able to start a replacement
    s.tmux(&[
        "set-environment",
        "-g",
        "TMUX_HOME_BIN",
        "/nonexistent/tmux-home",
    ]);
    o.open_popup();
    // live first: the header has no "(direct)"
    std::thread::sleep(Duration::from_millis(600));
    assert!(!o.screen().contains("(direct)"), "{}", o.screen());
    kill_daemon(&s);
    o.wait_for(r"\(direct\)");
    s.tmux(&["new-window", "-d", "-t", "beta:", "-n", "later"]);
    o.wait_for(r"^> +.*5/5");
    o.wait_for("later");
    // its own write shows without waiting for a poll
    o.keys(&["M-n"]);
    o.typed("mine");
    o.keys(&["Enter"]);
    o.wait_cursor_on("mine");
    o.wait_for(r"^> +.*6/6");
}

/// No daemon at all and none can start: the popup works on direct reads.
#[test]
fn degraded_mode_without_a_daemon() {
    let (_env, s, o) = popup_fixture();
    let sock = Paths::for_socket(&s.socket).unwrap().sock;
    wait_until("daemon up", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });
    s.tmux(&[
        "set-environment",
        "-g",
        "TMUX_HOME_BIN",
        "/nonexistent/tmux-home",
    ]);
    kill_daemon(&s);
    o.open_popup();
    o.wait_for(r"\(direct\)");
    o.wait_for(r"^> +.*4/4");
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "polled"]);
    o.wait_for(r"^> +.*5/5");
    close_idle(&o, "polled");
    o.wait_for(r"^> +.*4/4");
    o.keys(&["C-t"]);
    o.wait_cursor_on("polled");
    assert!(sessions(&s).len() == 2 && window_ids(&s).len() == 5);
}
