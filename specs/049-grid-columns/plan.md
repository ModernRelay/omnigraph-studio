# 049: Column edges, widths and wrapped text

Status: in progress · 2026-11-25 · step 1 (who owns a column's edge) in review; steps 2 and 3
stacked on it.

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
- Each half is its cell's **last** child, so its line draws over any slot. A press on it stops
  there, before the header cell around it hears it (no sort, no reorder drag); it blocks nothing
  else, so a column dragged onto the header to move it still drops there. Each half lights half
  of the drag line on its side of the rule; a drag lights both. A ruled cell's rule is drawn by
  its `End` half, not a border, so the rule is the edge.
- Controls keep out: a header's end control is a `ds::header_slot`, which stops `space::XS`
  short of the edge.
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
  the first line in view and scrolling the same way for both.
- Query results keep widths and wrap with the tab, as they kept widths.

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
| 2026-11-25 | The edge shows its line | Offscreen snapshots of `Note`, the pointer 2 px left of slug's rule | `main`: the menu's button and no line; now: the line, no button |

## Corrections
