//! Git badges in the real popup: a window in a temp repo, the popup opened
//! through tmux-home's binding on a throwaway server (whose daemon runs
//! the git task), read back with capture-pane.
mod common;
use common::*;

#[test]
fn badge_on_the_row_and_the_preview_banner() {
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
    // the app window's row (one pane: the pane's columns): command,
    // path, then the badge (pushed by the daemon)
    o.wait_for(r"^ +\d+ +app +\S+ .*/app +main !\? ≡");
    o.keys(&["Down", "Down"]);
    o.wait_cursor_on("app");
    // the pane's banner atop its capture: its command, then git spelled
    // out (branch → upstream, in sync, counts), then where it is
    o.wait_for(r"│\.0 \S+");
    o.wait_for(r"│main → origin/main ≡  !1 \?1 ");
    o.wait_for(r"│\S*/app +");
    // F1: the legend
    o.keys(&["F1"]);
    o.wait_for(r"⚠n stray branches");
    o.keys(&["Escape"]);
    o.wait_for(r"^> ");
}
