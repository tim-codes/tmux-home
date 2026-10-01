//! Git badges in the real popup: a window in a temp repo, the popup opened
//! through tmux-home's binding on a throwaway server (whose daemon runs
//! the git task), read back with capture-pane.
mod common;
use common::*;

#[test]
fn badge_on_the_row_and_git_card_in_the_preview() {
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
    // the row: command, path, then the badge (pushed by the daemon)
    o.wait_for(r"^[ ▌]alpha +\d+  app .* main !\? \|");
    o.keys(&["Down", "Down"]);
    o.wait_cursor_on("app");
    // the card above the pane capture
    o.wait_for(r"git +main → origin/main  in sync");
    o.wait_for(r"changes +1 modified · 1 untracked");
    o.wait_for(r"default +main");
    // F1: the legend
    o.keys(&["F1"]);
    o.wait_for(r"⚠n stray branches");
    o.keys(&["Escape"]);
    o.wait_for(r"^> ");
}
