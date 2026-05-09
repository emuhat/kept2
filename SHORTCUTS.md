# Keyboard shortcuts

`Ctrl` here means **Ctrl on Linux / Windows, Cmd (⌘) on macOS** unless otherwise noted. Where Mac differs in a non-obvious way, it's called out per row.

## Panes

The pane chord: press `Ctrl+W`, then within ~2 seconds press one follow-up key.

| Sequence | Action |
|---|---|
| `Ctrl+W` then `h` | Switch active pane → left |
| `Ctrl+W` then `l` | Switch active pane → right |
| `Ctrl+W` then `v` | Split: clone the active pane to its right; new pane becomes active |
| `Ctrl+W` then `q` | Close active pane (no-op when only one remains) |
| `Ctrl+W` then `=` | Reset divider to 50/50 |
| `Ctrl+W` then `Esc` | Cancel chord |

Drag the divider with the mouse to resize. Click anywhere in a pane to make it active.

## Opening things in the *other* pane

| Trigger | Behavior |
|---|---|
| `Alt+click` on a sidebar entry | Opens that view in the other pane (splits first if there's only one). |
| `Alt+click` on a cell | Opens the cell in the other pane in **focus mode** (cell fills the pane). Splits if needed. Clicks anywhere on the cell — title, body, whitespace. Suppressed when the click is on a link or `#tag` inside the cell, since those keep their own Alt+click semantic ("open the link/tag in the other pane"). |
| `Alt+Enter` in the search popup | Lands the result in the other pane (splits if needed). |

Alt-opens never auto-switch the active pane — the destination receives the view (and, for cell opens, the focus + focus-mode), but your keyboard / typing stays where it was. Useful for "show that there while I keep working here."

Plain click / plain Enter open in the active pane as usual.

## Navigation

| Key | Action |
|---|---|
| `Ctrl+K` | Open search |
| `Ctrl+[` | History back |
| `Ctrl+]` | History forward |
| `Ctrl+Shift+D` | Jump to today |
| `Ctrl+Shift+Up` / `Down` | Sidebar context — newer / older |
| `Ctrl+Up` / `Down` (view mode) | Move focus to the previous / next visible cell |
| `Ctrl+F` | Focus mode (enlarge focused cell to fill the pane) |
| `Esc` | Exit focus mode → exit edit mode → close menus |

In **view mode** (no caret), plain `Up` / `Down` arrow keys also move focus between cells. In **edit mode**, arrow keys move the caret; arrowing past the top / bottom of the cell crosses into the adjacent cell.

## Editing

| Key | Action |
|---|---|
| `Enter` (view mode, on a focused cell) | Enter edit mode |
| `Esc` (edit mode) | Exit edit mode |
| `Ctrl+Z` | Undo |
| `Ctrl+Shift+Z` | Redo |
| `Ctrl+A` | Select all (in cell) |
| `Ctrl+C` / `X` / `V` | Copy / cut / paste |
| `Ctrl+T` | Toggle title slot on the focused cell |

## Creating cells

| Key | Cell type |
|---|---|
| `Ctrl+N` | Plain |
| `Ctrl+O` | Outline |
| `Ctrl+P` | PopPop calculator |
| `Ctrl+Shift+N` | Plain, starts in title-edit mode |
| `Ctrl+Shift+O` | Outline, starts in title-edit mode |
| `Ctrl+Shift+P` | PopPop, starts in title-edit mode |
| `Ctrl+Shift+R` | Rotate context (start a new context "now") |

A new cell is inserted after the currently focused cell and goes straight into edit mode. No-op if the currently focused cell is empty (so the shortcut doesn't pile up blanks).

The `Ctrl+Shift+<letter>` variants pre-attach an empty title and put the cursor in it, equivalent to `Ctrl+<letter>` followed by `Ctrl+T`.

## References

| Key | Action |
|---|---|
| `Ctrl+E` | Envelope the focused Reference cell — wraps it in an outline so you can write notes around the embed. No-op on non-Reference cells. |

"Unwrap envelope" lives only on the right-click menu of an envelope outline. Bullet notes are dropped on unwrap; `Ctrl+Z` restores them.

## View

| Key | Action |
|---|---|
| `Ctrl+=` or `Ctrl++` | Zoom in |
| `Ctrl+-` | Zoom out |

Zoom affects the whole document (font scale is global, not per-pane).

## Search popup

While the popup is open:

| Key | Action |
|---|---|
| `Esc` | Cancel |
| `Enter` | Open the highlighted result in the active pane |
| `Alt+Enter` | Open the highlighted result in the other pane (split if needed) |
| `Up` / `Down` | Move highlight |
| `Tab` | Same as Enter |
| `Ctrl+C` / `X` / `V` / `A` | Clipboard / select-all on the search input |

Other `Ctrl+letter` combos are swallowed while the popup is open so app-level shortcuts don't fire behind it.

## Within a cell (text editing)

Standard text-editing keys behave as expected:

| Key | Action |
|---|---|
| `Left` / `Right` | Move caret one char |
| `Up` / `Down` | Move caret one line |
| `Home` / `End` | Move to line start / end |
| `Backspace` / `Delete` | Delete previous / next char |
| `Enter` | Insert newline |
| Hold `Shift` with any of the above | Extend selection |

Word-jump and word-delete use the platform's word modifier:

- **Linux / Windows:** `Ctrl+Left` / `Ctrl+Right` for word jump; `Ctrl+Backspace` / `Ctrl+Delete` for word delete.
- **macOS:** `Option+Left` / `Option+Right` and `Option+Backspace` / `Option+Delete`.

Mac line-edge nav: `Cmd+Left` / `Cmd+Right` jumps to line start / end (with `Shift` to extend selection). Off-Mac, use `Home` / `End`.

### Outlines & tables

Inside outline cells:

| Key | Action |
|---|---|
| `Tab` | Indent bullet |
| `Shift+Tab` | Outdent bullet |
| `Enter` | Split bullet at caret |
| `Backspace` at start of bullet | Merge into previous |
| `Shift+Up` / `Shift+Down` (in bullet selection) | Extend selection across bullets |

Inside tables, `Tab` advances cells; `Backspace` / `Delete` clear the focused cell when bullet/cell selection is active.

### Multi-cursor

`Shift+Alt+click` inside a cell adds a secondary caret/selection at the click position. (Plain `Alt+click` is reserved for "open the cell in the other pane.")

## Right-click menus

| Target | Menu |
|---|---|
| Cell | Timestamps + Delete / Surface / Envelope / Unwrap / **Mark inactive** (or Mark active) — and **Mark sub-outline inactive/active** when the click landed on a bullet in this cell |
| Sidebar tag (with no cells) | Delete tag |
| People-page row | Rename / Delete person |

Inactive cells and bullets are hidden from views by default. The **Show inactive** toggle at the bottom of the sidebar surfaces them globally — they render dimmed (about 40% alpha) instead of disappearing. Bullet-level inactive cascades by ancestry: marking a sub-outline root inactive hides its whole subtree.

`Esc` dismisses any menu.

## Mouse

| Gesture | Action |
|---|---|
| Wheel | Scroll the pane under the cursor (independent of which pane is active) |
| Drag the scrollbar thumb | Direct scrolling; flick-and-release coasts via kinetic decay |
| `Alt` + drag | Drag from anywhere in a pane as if you were dragging the scrollbar thumb — cursor y maps absolutely to scroll position (so the gain matches what scrollbar drag would feel like). Flick-and-release coasts via kinetic decay. Disambiguated from `Alt`+click (open in other pane / multi-cursor add) by a small drag threshold; short drags stay clicks, longer drags become pans |
| Drag the divider | Resize the split (clamped between 15% and 85%) |
| Click | Activate the pane that was clicked, then dispatch normally |
