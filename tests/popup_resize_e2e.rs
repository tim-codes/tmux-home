//! M-j / M-k / M-= in the real popup: they reach it through tmux even
//! with root-table `M-j`/`M-k` bindings (the usual `resize-pane` ones),
//! move the list/preview divider, and the size outlives the popup in
//! `@home-preview-pct`.
mod common;
use common::*;

/// The screen row of the preview's top rule inside the popup.
fn rule_row(o: &Outer) -> Option<usize> {
    let rule = regex::Regex::new(r"─{60,}").unwrap();
    // the popup's own border is the first such line
    o.screen()
        .lines()
        .enumerate()
        .filter(|(_, l)| rule.is_match(l))
        .map(|(i, _)| i)
        .nth(1)
}

fn wait_rule(o: &Outer, what: &str, f: impl Fn(usize) -> bool) -> usize {
    let mut at = None;
    wait_until(what, || {
        at = rule_row(o);
        at.is_some_and(&f)
    });
    at.unwrap()
}

fn pct(s: &TestServer) -> String {
    s.tmux(&["show-options", "-gqv", "@home-preview-pct"])
        .trim()
        .to_string()
}

fn pane_heights(s: &TestServer) -> String {
    s.tmux(&["list-panes", "-t", "alpha:editor", "-F", "#{pane_height}"])
}

#[test]
fn alt_j_k_resize_the_preview_despite_root_bindings() {
    let (_env, s, o) = popup_fixture();
    // the operator's pane-resize keys, in the root table
    s.tmux(&["bind-key", "-n", "M-j", "resize-pane", "-D", "5"]);
    s.tmux(&["bind-key", "-n", "M-k", "resize-pane", "-U", "5"]);
    // two panes, so a root binding firing would show in their heights
    s.tmux(&["split-window", "-d", "-v", "-t", "alpha:editor"]);
    let heights = pane_heights(&s);

    o.open_popup();
    o.wait_cursor_on("editor");
    let auto = wait_rule(&o, "the preview", |_| true);
    assert_eq!(pct(&s), "", "auto by default");

    o.keys(&["M-k"]);
    let up = wait_rule(&o, "M-k: the divider up", |r| r < auto);
    let set: u16 = pct(&s).parse().expect("M-k saves the size");
    o.keys(&["M-j"]);
    o.keys(&["M-j"]);
    let down = wait_rule(&o, "M-j: the divider down", |r| r > up);
    assert!(down > auto, "{}", o.screen());
    assert_eq!(pct(&s), (set - 10).to_string());
    assert_eq!(pane_heights(&s), heights, "the root bindings didn't fire");

    // the size outlives the popup
    o.keys(&["Escape"]);
    wait_until("the popup closed", || !o.popup_open());
    o.open_popup();
    wait_rule(&o, "the saved size", |r| r == down);

    // M-=: auto again, and the option is gone
    o.keys(&["M-="]);
    wait_rule(&o, "M-=: auto", |r| r == auto);
    wait_until("the option unset", || pct(&s).is_empty());
    assert_eq!(pane_heights(&s), heights);

    // control: outside the popup the same key does reach the root binding
    o.keys(&["Escape"]);
    wait_until("the popup closed", || !o.popup_open());
    o.keys(&["M-k"]);
    wait_until("the root M-k resized a pane", || {
        pane_heights(&s) != heights
    });
}
