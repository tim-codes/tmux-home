//! Git badges in the real popup: a window in a temp repo, the popup opened
//! through tmux-home's binding on a throwaway server (whose daemon runs
//! the git task), read back with capture-pane.
mod common;
use common::*;

#[test]
fn badge_on_the_row_and_in_the_preview_band() {
    git_env(); // before the server starts: its daemon inherits it
    let t = TempDir::new("e2e");
    let repo = t.repo("app");
    with_origin(&repo);
    write(&repo, "todo.txt", "x\n");
    write(&repo, "README", "edited\n");
    let (_env, s, o) = popup_fixture();
    s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "alpha:",
        "-n",
        "app",
        "-c",
        repo.to_str().unwrap(),
    ]);
    o.open_popup();
    // the app window's pane row: command, path, then the badge (pushed
    // by the daemon)
    o.wait_for(r"^ +└ \.0 .*/app +main !\? ≡");
    o.keys(&["Down", "Down", "Down", "Down"]);
    o.wait_cursor_on("app");
    // the pane's meta band atop its capture, no git card
    o.wait_for(r"│\.0 \S+ +main !\? ≡");
    assert!(!o.screen().contains("main → origin/main"), "{}", o.screen());
    // F1: the legend
    o.keys(&["F1"]);
    o.wait_for(r"⚠n stray branches");
    o.keys(&["Escape"]);
    o.wait_for(r"^> ");
}
