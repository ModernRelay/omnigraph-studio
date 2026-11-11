# Drawing cost

How GPUI decides what to draw again, the rules Studio follows so a keystroke or a scroll does
not lay out more than it changed, and how to measure and guard them.

## What GPUI draws again

Measured against Zed v1.23.2 (`crates/gpui/src/window.rs`, `view.rs`):

- `cx.notify()` on a view marks that view and every view around it for drawing
  (`Window::mark_view_dirty`). It also calls the entity's `observe` callbacks.
- A view drawn with `AnyView::cached(style)` replays its last frame while it is not marked and
  its bounds, clip and text style are the same.
- A cached view that draws again draws every cached view inside it again too: `prepaint_view`
  sets `window.refreshing` while it draws. Caching only helps a view whose surroundings are
  not cached, or not drawn.
- Typing in an editor marks the editor, so every view around the field draws again, and with
  them every cached view inside the nearest cached one.
- An entity made while a frame is drawn counts as read before its own cached draw, so GPUI
  leaves it out of what that view depends on and stops redrawing it when it notifies. Make
  views that are drawn cached when their owner is made, not on its first draw.
- The first key after mouse input switches GPUI to keyboard input, which draws the whole
  window once (`Window::dispatch_event`).

## Rules

- A field (editor) is never drawn inside a large view. Draw it beside it, or over it with
  `studio_ui::OverlaySlot`: the large view marks the place while it draws, the field is drawn
  there from outside.
- A large part of a tab (a grid, a canvas) is a view of its own, drawn cached, made with the
  tab. The tab answers `Item::caches_its_parts` with `true`, so its pane does not cache the
  whole tab and draw the part again with it.
- `notify` only for state the view draws. A handler that runs on every key (`BufferEdited`)
  does not notify a view the field is not inside; the view around the field draws again
  anyway.
- Work done for each drawn row or cell scales with what is shown, never with what is loaded
  (spec 004, table performance). A grid wider than its view draws only the columns in view,
  one either side, and the ones the cursor or an editor needs; each run of the others is one
  empty gap as wide as they are, so a line lays out a handful of cells, its header the same
  ones, and the scroll range does not change. The columns are picked as the grid draws, from
  the scroll offset and the window's width, which bounds the view's.
- The type table and the query results pick their columns with `studio_ui::columns_in_view` and
  `studio_ui::grid_places`.
- A grid cell's plain value is `studio_ui::CellText`, not a `Label`: the cell's box is fixed by
  its column, so the text asks for no measurement and is cut only when it does not fit. A
  truncated label is measured on every layout pass of every frame, with a line wrapper taken
  from GPUI's pool each time (`TextLayout::layout`); in a table that was a fifth of a scroll
  frame.
- A cell has no element id and no click handler of its own. GPUI keeps an element state and a
  hitbox for every element with an id, each frame, for each of a frame's hundreds of cells; the
  grid takes the click and finds the cell under it (`TypeTable::row_cell_at`), and a control in
  a cell stops the click. A cell takes an id only for a tooltip. On 2026-11-10 this took a
  table scroll from 4.7 to 4.4 ms.
- An element's request-layout and prepaint states are small: GPUI moves them, with the element,
  from step to step of every frame. A `ShapedLine` is about 3 KB, its runs held inline, so a
  cell keeps its line boxed (`studio_ui::CellText`, guarded by
  `a_cell_moves_a_pointer_not_its_line`). Unboxed, the copies were a tenth of a table scroll
  frame; boxing took it from 5.1 to 4.7 ms.

## Guards

Every costly view counts its draws in tests (`#[cfg(test)] renders`), and a headless test drives
the interaction and asserts the count:

| Guard | Holds |
| --- | --- |
| `type_table` `typing_in_a_field_leaves_the_grid_as_it_was` | The grid follows the table's `notify`; typing in the filter field or a cell does not draw it |
| `type_table` `rows_draw_only_the_columns_in_view` | A 21-column type draws the pinned column and those in view; scrolled to the end, the last; the cursor's column always; a line lays out one gap for the rest, as wide as they are; widened, every column in the frame of the resize |
| `query_editor` `a_wide_result_draws_only_the_columns_in_view` | A 60-column result draws the columns in view and one gap for the rest, as wide as they are; scrolled to the end, the last |
| `query_editor` `typing_in_the_query_leaves_the_results_as_they_were` | The result area follows the tab's `notify`; typing in the query does not draw it |
| `query_editor` `typing_leaves_the_problems_as_they_were` | The problems strip is a cached view of its own; typing that leaves the problems unchanged does not draw it |
| `studio_workspace` `a_tab_that_caches_its_parts_keeps_them_while_a_field_changes` | A tab that caches its parts keeps them while a field changes; a cached tab draws them again, which is the GPUI behavior the opt-out exists for |

Draw counts are exact, so they run in CI. Times are not: the machine's load moves them several
times over.

## Measuring

`script/measure-frames` runs the common cases against the development graph (`script/dev-server`)
on `release-fast`: a key in the table's filter field, in a cell, in the query editor with six
result columns, and in the Go To palette; a table scroll; the whole window drawn again, as each
frame of an animation draws it (a dialog or a toast coming up, the connection dot); and a
scroll of a node page, the inspector, its tree and History, and a pan of its canvas. On 2026-10-18 (Apple M5 Pro, load
average 8) each key cost 1.2 to 2.0 ms of main-thread CPU and a table scroll 4.3 ms. On the
personal graph's 92-column Person type a table scroll costs 4.0 ms, and a 63-column query
result 2.2 ms.

On 2026-11-10 (Apple M5 Pro, load average 6 to 20), the median of eight alternating runs of the
same two builds: a table scroll 5.1 ms before the two rules above and 4.4 ms after; every other
case 1 to 2.5 ms, and the whole window redrawn about what a table scroll costs, since the table
is most of the window. What is left of a table frame is GPUI's own work per element: a quad for
each cell's rule (about a tenth of the frame), Taffy's layout and each element's style. Drawing
the column rules once per column, over the rows, would take the quads; it is not done, since the
rules would then draw over the cursor's ring and the edit field.

On 2026-11-11 (load average 35 to 47) the inspector's scroll cost 4.3 ms on a company of the
development graph and 1.4 to 6.5 ms on a person of the personal graph, with its 60 relationship
groups, and read nothing as it scrolled. Turning off the re-measure of the items each draw
changed nothing beyond the machine's noise.

In the running app, `studio.show_performance` puts Studio's memory and CPU at the end of the
footer, and a build with `--features perf` (GPUI's `profiler`) has GPUI's frame-time overlay on
⌘⌥⇧F (`studio::ToggleFrameOverlay`): frame times, then their distribution, then hidden.

`crates/studio/examples/snapshot.rs` draws Studio offscreen against a server and measures frames:
`measure:<n>` with `,type` (a key into the focused field), `,scroll=<dy>` (`,scroll=<dx>:<dy>` sideways) or `,drag=<dx>:<dy>`.
It prints wall time and the main thread's CPU time per frame; compare CPU times, in the same
session, on the `release-fast` profile:

```bash
cargo build --profile release-fast -p studio --example snapshot
OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token \
  OMNIGRAPH_STUDIO_GRAPH=knowledge \
  target/release-fast/examples/snapshot /tmp/out.png \
  'studio_graph::OpenType={"type_name":"Person"}' 'studio_table::AddFilter' 'measure:200,type'
```

To see where a frame goes, run a long measure and sample the process: on macOS,
`/usr/bin/sample <pid> 8 -file out.txt`, then read the main thread's heaviest frames.
