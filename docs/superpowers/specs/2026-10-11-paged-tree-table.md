# Paged sessions, window/pane tree, tabular rows

Status: approved direction (operator request 2026-10-11), details by judgement.

## Popup (overview)

### Session pages
- The list shows **one session at a time**. Opens on the client's session
  (no client: first session by name).
- Header row becomes a **tab strip**: every session name; the current page
  reversed/bold, the client's session marked `▶`, and a per-session
  attention count (`●N`, yellow) when it has NEEDS YOU windows.
- Paging is deliberately **not** on a convenient bare key (Tab etc.): it is a
  tmux-prefixed chord, `<prefix> )` next / `<prefix> (` previous (wrap) —
  tmux's own next/previous-session keys — recognised by the popup the same
  way it already recognises the open-key chord. Never paged implicitly by
  list navigation: Up/Down wrap *within* the page.
- Filter is **scoped to the page**. Paging keeps the filter text and
  re-applies it. `s:` token is dropped (paging replaces it).
- NEEDS YOU group shows only the current page's attention windows. `^g`
  jumps to the next attention window, paging to its session if needed (an
  explicit action, so crossing sessions is intended).

### Window rows with pane rows beneath
- Every window is a row; **every non-sidebar pane is a row beneath it**
  (always, including single-pane windows, so the structure is uniform).
- Selection moves over both kinds. Enter on a window: switch to the window
  (lead agent pane selected, as today). Enter on a pane: switch to the window
  and select that pane.
- Window-row actions (rename, close, swap, new) work from a pane row too,
  acting on its window.
- Filtering: a window is shown if it or any of its panes match; when shown,
  all its panes are shown.

### Table
Column header line under the filter, dim, htop-style 1–2 char labels:

| col | meaning                                   | window row          | pane row              |
|-----|-------------------------------------------|---------------------|-----------------------|
| `#` | index                                     | window index        | `├ .N` / `└ .N`       |
| `NM`| name                                      | window name         | pane title if not hostname, else blank |
| `CM`| command                                   | —                   | current command       |
| `ST`| agent state (icon + word)                 | worst pane state    | pane agent state      |
| `T` | elapsed run time                          | —                   | agent elapsed         |
| `AG`| agent kind                                | `kind ×N` rollup    | kind                  |
| `PA`| path (short, `~`)                         | —                   | cwd                   |
| `GT`| git badge                                 | —                   | badge for pane cwd    |
| `MS`| wait reason / prompt (rest of the width)  | —                   | reason (yellow) / prompt (dim) |

Column widths are computed per page from content (capped), so values align
vertically. Narrow widths drop columns right-to-left (MS, GT, PA, T first),
never truncating `#`/`NM` below their minimum. F1 help lists the legend.

### Preview
- **Always at the bottom.** Shown when height ≥ 30 (50% of the list area);
  hidden below that unless toggled with `^o` (then 50%).
- Renders the **whole window's pane layout**: each pane's visible content
  (`capture-pane -e`, no history) placed in its rectangle scaled to the
  preview area, with tmux-style separators.
- Window row selected: all panes, no highlight.
- Pane row selected: same layout, that pane outlined with a highlighted
  (magenta) border.
- Agent/git cards stay above the layout, for the lead pane (window row) or
  the selected pane (pane row).
- Wheel/Shift-scroll: scrolls the highlighted pane's content when a pane is
  selected; no-op on window rows.
- Needs pane geometry in the snapshot: `pane_left/top/width/height`,
  `window_width/height` added to PANE_FMT.

## Sidebar
- **Fixed-width first column** (status icon, index, name), the same width
  for every row; width = longest name in the session, capped to ~half the
  sidebar.
- **Second column: tags stacked**, one or more per line, wrapping onto
  continuation lines aligned under column 2 rather than being dropped.
  Missing fields collapse (no blank cells).
- Fix: focusing the sidebar pane must allow navigation and actions.
