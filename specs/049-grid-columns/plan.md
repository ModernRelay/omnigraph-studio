# 049: Column edges, widths and wrapped text

Status: in progress · 2026-11-25 · step 1 (who owns a column's edge, #181), step 2 (#180) and
step 3 (#182) in review.

## Problem

Columns can already be resized in the type table and a query's results, but near a column's
edge the resize handle and the buttons beside it get in each other's way. A press a pixel off
opens the column's menu or sorts the next column, and while the pointer is on the edge the
menu's slot hides the resize line. Beyond that, a long value can only be cut, the keyboard
cannot resize, and a double click resets a type table's column where a fit is expected.

## Principles

- **V.** A drag, fit or toggle changes only what was asked; turning the wrap on keeps the first
  row in view where it is.
- **VI.** The edge shows what it does (its line, the resize cursor); the menus name Fit, Reset
  and Wrap with their keys.
- **VIII.** Every new change is an action with a key: `studio_grid::WidenColumn`,
  `NarrowColumn`, `FitColumn`, `ResetColumns`, `ToggleWrap`.
- **IX.** Rows stay virtualized; a wrapped cell shows at most 6 lines; a fit measures at most
  1,000 rows.

## What exists today (checked)

| | Type table | Query results |
|---|---|---|
| Drag a header's right edge, 48 to 1,200 px | yes | yes |
| Double click on the edge | back to the type's width | back to the width fitted to the values (by character count) |
| Widths kept | with the tab (`"widths"`) | with the tab, by column name |
| Keys that resize, wrap, reset all | none | none |

No other view has columns under a header a person can act on: the inspector, the page and a
query row's inspector are name → value lists; History's commit list lines up fixed meta with no
header; the rest are one-column lists (surveyed 2026-11-25).

**The edge bug**, traced on `main` with a probe pressing and dragging at every pixel across a
type table's header edges (`edge-trace-main.txt` in the PR):

- No hit areas overlap and no event is taken by the wrong element: a press on the handle never
  opens the menu or sorts, a click on the menu's button never starts a resize, and a drag from
  the handle that ends over the button fires nothing.
- **Z-order.** The handle is the header cell's first child; the column menu's hover slot,
  absolute at the cell's end with an opaque fill, comes later. On the edge the slot covers the
  handle's line, so the edge reads as part of the button.
- **No margin.** The handle was the column's last 6 px; the button's hit area ended 7 px from
  the edge, touching it, and right of the rule the next column's header, a sort button, began at
  once. A press 1 px off either side fired a button.
- **A dead rule.** GPUI clips a cell's children inside its border (`Style::overflow_mask`,
  `crates/gpui/src/style.rs` at `v1.27.0`), so on a ruled header (the key column) the rule pixel
  was no one's.

## Design

### Step 1: one rule for a column's edge (`studio_ui::column_resize_handle`)

- The edge is `size::COLUMN_EDGE` (4 px) either side of the rule, in two halves: the column's
  end in its own header cell (`ColumnEdge::End`) and the next column's start in its cell
  (`ColumnEdge::Start`), given only where the column before is in view against it (not at a
  scrolled strip's or header's left edge).
- Each half is its cell's **last** child, so its line draws over any slot. A press of **any
  button** on it stops there, before the header cell around it hears it (no sort, no reorder
  drag, no column menu); it blocks nothing else, so a column dragged onto the header to move it
  still drops there. Which columns have a start half is decided from the scroll as GPUI will
  hold it this frame (`scrolled_within`, within the strip's own width, which beside a sidebar is
  less than the window's). Each half lights half
  of the drag line on its side of the rule; a drag lights both. A ruled cell's rule is drawn by
  its `End` half, not a border, so the rule is the edge.
- Controls keep out: a header's end control is a `ds::header_slot`, which stops `space::XS`
  short of the edge, and is left out where it would come within `space::XS` of the cell's start
  controls (the key column's select-all check, below 70 px); the header's right click still
  opens its menu.
- `resizable_grid_header` (query results) and the type table's header both place the two halves.

### Step 2: the column model, and query results on it

- `studio_ui::GridColumns` holds what the person chose (`studio_settings::GridLayout`: widths by
  key, `wrap`) over the grid's own; one `change(ColumnChange, …)` serves keys, menus and the
  edge. A double click on the edge **fits** the column to its widest text (header included, the
  first 1,000 rows); Reset width gives one column back; ⌥⇧0 gives every width and the wrap back.
- ⌥⇧→ / ⌥⇧← widen or narrow the selected column by 16 px; ⌥⇧F fits it; ⌥Z wraps.
- Wrapped, a text cell runs onto as many lines as its width needs, at most 6, the last cut with
  "…" (`CellText::wrap`); a row is as tall as its tallest cell and a one-line row is as tall as a
  cut one. Cut rows draw through `uniform_list` as today; wrapped rows through GPUI's `list`,
  which measures a row as it draws it. `studio_ui::GridRows` answers the line under the pointer,
  the first line in view and scrolling the same way for both. Turning the wrap on or off keeps
  the first line in view first and never moves the view to the selection. A wrapped row is as
  tall as its text cell with the most lines, drawn or not (`row_lines`), so it keeps its height
  while scrolled sideways. A value cut to what a cell keeps (512 characters) ends in "…".
- **One rule forgets wrap measurements** (`studio_ui::WrapLines`): a row's line count, and every
  height GPUI's list measured from it, holds for one measure, the frame's `wrap_key` (the cell
  text's font, size, weight and line height, and each wrapping column's key and room). A frame
  whose measure differs (a font size changed, a column shown, hidden, moved or resized) forgets
  them all and measures the list again; a row whose text changed is forgotten alone; other rows
  (a new result, a read) forget them all.
- Query results keep widths and wrap with the tab, as they kept widths; a new result closes a
  column menu opened on the last one.

### Step 3: the type table on the model

- Widths and wrap are the type's, per graph, in settings (`graphs.<graph>.columns.<Type>`); every
  tab of the type follows a change. A tab saved with widths hands them to the type once.
- Zed's cell around a type table cell does not stretch to its row, so a wrapped row's height is
  worked out from its drawn text cells' line counts and given to every cell.

## Decisions

| Decision | Rejected | Why |
|---|---|---|
| The edge straddles the rule, in two halves | A wider zone inside the column | Inside the column, a press just right of the rule still sorts the next column |
| The handle draws the rule | Keep the border, nudge the handle | A child can never take a clipped cell's border pixel |
| `uniform_list` cut, `list` wrapped | `list` for both | GPUI's `list` drops every height when its width changes and counts undrawn rows as 0 px: a cut table of 60 rows had a 13 px scroll range |
| A type's widths in settings | With the tab | Asked for; widths describe how to read a type, not a tab |
| ⌥Z wraps; ⌥⇧ arrows resize | ⌃⌥ arrows | VS Code's wrap key; GNOME and KDE take ⌃⌥ arrows |

## Deferred

Wrap per column; a row height a person sets; property lists' label column (the inspector, the
page). No engine change.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-25 | The edge bug, as above | Probe on `main`: a click and a 40 px drag at every pixel from 30 px left to 4 px right of each header rule of `Person` (two columns), and sub-pixel presses | Handle −6 to −1 px, menu button −24 to −7, sort left of that; on the key column the rule pixel did nothing and +0.5 px sorted the next column; drags from the handle never opened the menu |
| 2026-11-25 | The rule holds | `type_table` `a_column_edge_is_the_resize_handles_and_no_control_reaches_it`, `a_column_dropped_on_an_edge_moves`, `a_scrolled_strip_gives_no_edge_to_a_column_out_of_view`; `studio_ui` `a_press_at_a_rule_resizes_the_column_on_its_left` | Pass; the first fails on `main` at −0.5 px from the key column's rule. With the edge blocking the mouse (the first version of this fix), a column dropped on an edge did not move (review of #181) |
| 2026-11-25 | Query results wrap and size by key; cut rows keep `uniform_list` | `query_editor` `wrapped_results_grow_and_the_keys_size_the_selected_column`; with GPUI's `list` for cut rows too, `type_table` `a_sideways_wheel_scrolls_the_columns_not_the_rows` failed | Wrapped, a long note 6 lines and short rows a row; keys on the selected column; the tab keeps `"widths"` and `"wrap"`. With `list`, a cut table of 60 rows had a 13 px scroll range after its first frame |
| 2026-11-25 | A wrapped row keeps its height while scrolled sideways, and a run keeps no old heights | `query_editor` `wrapped_rows_keep_their_height_out_of_view_and_across_runs` (40 columns, the long one out of view) | Counted from the drawn cells only, the row was a row high until its tall cell scrolled in (review of #180); counted from every column, cached per row, it is tall from the start; a rerun's short row is a row |
| 2026-11-25 | The type table keeps its columns for the type, and wrapped rows grow without breaking selection | `type_table` `a_types_columns_are_kept_for_the_type_and_wrapped_rows_grow` | A saved tab's widths became the type's; wrapped, the long name's row grew and a press near its bottom is on it; a second tab of the type followed a change; Reset left no entry |
| 2026-11-25 | A wrapped row's cells run its height | Offscreen snapshot of `Note` wrapped, the cursor on "gpui-panes" | With each cell only as tall as its lines, the rules between short cells and the cursor's ring stopped after the first line; each cell now takes the row's height from its text cells' line counts |
| 2026-11-26 | Every button at an edge is the edge's; no end control over a start control | `type_table` `every_button_at_an_edge_is_the_edges`, `the_select_all_check_is_not_under_the_menu_at_the_narrowest_key_column`, both failing on `11942b0` | At `11942b0` a right press 3.5 px left of the key column's rule opened a menu, and at a 48 px key column the menu's button (16 to 34 px) covered the select-all check (16 to 30 px), so a click on the check opened the menu (Codex review of #181) |
| 2026-11-26 | Toggling the wrap keeps the view; a font change counts lines again; a new result closes a column menu | `query_editor` `toggling_the_wrap_never_jumps_to_the_selection`, `a_font_change_counts_wrapped_lines_again`, `a_new_result_closes_a_column_menu`, all failing on `1ef16d0`; `the_keys_menu_and_edge_work_as_a_person_uses_them` | At `1ef16d0` the toggle moved the view from row 40 to the selected row 0; at a 24 px rem a 6-line row was drawn 115 px tall instead of 173 px; a menu outlived its result (Codex and Bugbot reviews of #180). ⌥Z from the query does nothing, from the results wraps; a right press on a header opens its menu; a double click on an edge fits |
| 2026-11-25 | The edge shows its line | Offscreen snapshots of `Note`, the pointer 2 px left of slug's rule | `main`: the menu's button and no line, the slot's fill over it; now: the line, drawn over everything, and the button, which stops short of the edge |

## Corrections
