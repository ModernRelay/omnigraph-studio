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

## Guards

Every costly view counts its draws (`studio_perf::count!("draw", …)`; some older guards still
keep a `#[cfg(test)] renders` field), and a headless test drives the interaction and asserts the
count:

| Guard | Holds |
| --- | --- |
| `type_table` `typing_in_a_field_leaves_the_grid_as_it_was` | The grid follows the table's `notify`; typing in the filter field or a cell does not draw it (counted with `studio_perf::capture`) |
| `type_table` `rows_draw_only_the_columns_in_view` | A 21-column type draws the pinned column and those in view; scrolled to the end, the last; the cursor's column always; a line lays out one gap for the rest, as wide as they are; widened, every column in the frame of the resize |
| `query_editor` `a_wide_result_draws_only_the_columns_in_view` | A 60-column result draws the columns in view and one gap for the rest, as wide as they are; scrolled to the end, the last |
| `query_editor` `typing_in_the_query_leaves_the_results_as_they_were` | The result area follows the tab's `notify`; typing in the query does not draw it |
| `query_editor` `typing_leaves_the_problems_as_they_were` | The problems strip is a cached view of its own; typing that leaves the problems unchanged does not draw it |
| `type_table` `opening_a_table_reads_each_relationship_once` | Opening a table reads its page once and each relationship column's counts and neighbours once: one read per column and kind, never per row, none twice (`studio_perf::capture`) |
| `studio_workspace` `a_tab_that_caches_its_parts_keeps_them_while_a_field_changes` | A tab that caches its parts keeps them while a field changes; a cached tab draws them again, which is the GPUI behavior the opt-out exists for |

Draw counts are exact, so they run in CI. Times are not: the machine's load moves them several
times over.

## Measuring

Score with counts first and times second: counts are exact on any machine, times move with its
load (constitution XII asks for the machine and the data size with every time).

`studio_perf` (layer 0) is the one way Studio measures; the design, and what it takes from Zed,
Chromium, Tracy, puffin, tracing, Bevy and divan, is in `docs/perf/20261106-report.md`.

```rust
studio_perf::count!("draw", "TableGrid");            // a counter declared where it counts
static BYTES: Counter = Counter::new("received", "GET /graphs", Unit::Bytes);
BYTES.add(n);                                         // a counter kept in a table

let step = studio_perf::step("step", "open a type's table"); // a measured span
// ... the interaction; requests, draws and allocations count on any thread ...
let record = step.end();                              // what each counter added, attached
let _decode = studio_perf::span("decode", "ReadOutput");     // a span for the trace only

let reads = studio_perf::capture();                   // a test: this thread's counts
assert_eq!(reads.count("query", "studio_page"), 1);
```

- Counters count only while measuring is on (`OMNIGRAPH_STUDIO_PERF` or
  `OMNIGRAPH_STUDIO_TRACE`, the snapshot tool, a test's `capture`): off, adding is one relaxed
  load; on, an atomic add and a thread-local add, with no lock and no allocation, so an allocator
  may count.
- `graph_client` counts every request by method and route (`GET /graphs/{graph_id}/snapshot`,
  never its path), the bytes of each answer (`received`), and each `/query` by the name Studio
  gave the query it built (`studio_page`; a person's own is `query`, its name unsaid). The costly
  views count their draws (`draw`, by view). The snapshot tool counts allocations (`alloc`) and
  the heap.
- A `step` carries what every counter added while it was open, on every thread and on its own;
  it allocates what it needs before it reads them, so it counts none of its own work.
- Ended spans go to one collector while it collects: `Records::table` prints them,
  `Records::chrome_trace` writes Chrome trace JSON (slices, async request tracks, a counter
  track per group) that `ui.perfetto.dev` opens. With `OMNIGRAPH_STUDIO_TRACE=<file>` Studio
  writes it there when it quits.
- Guard tests assert on the counters through `capture`, which counts on the test's thread only:
  a `#[gpui::test]` runs its work there, so tests beside it add nothing.

`crates/studio/examples/snapshot.rs` draws Studio offscreen against a server. After each step it
prints a `step` line: how long until the step's last request ended (`settled`), the main
thread's allocations, the requests by route with their bytes, Studio's queries, the views drawn,
and the frame drawn after it. `report:<name>` names the next actions as one step. `measure:<n>`
draws n frames and prints wall time, the main thread's CPU time per frame, and per frame the
main thread's allocations and the views drawn: `,type` (a key into the focused field),
`,scroll=<dy>` (`,scroll=<dx>:<dy>` sideways) or `,drag=<dx>:<dy>`, from the window's centre or
`,at=<x>:<y>`. A frame is one update that delivers the input; GPUI's test mode draws what it
changed as the update ends, once, as an app's next frame would, and the frame is timed from
outside so that draw is in it.

Headless, every change draws the window, where an app draws at most once a display frame: a
step's views drawn are an upper bound.

`script/measure-frames` runs the common frames against the development graph
(`script/dev-server`) on `release-fast`: a key in the table's filter field, in a cell, in the
query editor and in the Go To palette, a table scroll, a canvas pan and zoom, a sidebar resize.
`--memory` prints the heap every thread holds (the snapshot tool's `heap`) after each of ten
opens and closes of a table and five of a query, which should level off once the caches fill.
`--scenarios` runs whole interactions (start to first paint, open a type's table, sort, filter,
open a row's page, the tree, the canvas, a query, switch branch, history, review, merge) and
prints one line each; `--all` both. With `script/dev-big-server` running (port 8788: a
95-column type of 100,000 rows, 500,000 edges, 340 commits, 40 branches) both add its cases.
For a slow network put `script/latency-proxy.py` in front of a server (`--rtt 100`,
`--bandwidth`, `--drop`) and point `OMNIGRAPH_STUDIO_SERVER` (or `STUDIO_BIG_SERVER`) at it.

```bash
script/dev-server &                 # and script/dev-big-server & for the large graph
script/measure-frames --all
script/latency-proxy.py --rtt 100 & # port 9787, in front of 8787
OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:9787 script/measure-frames --scenarios
```

On 2026-10-18 (Apple M5 Pro, load average 8) each key cost 1.2 to 2.0 ms of main-thread CPU and a
table scroll 4.3 ms; those key times were measured before the frame was timed from outside, so
they leave out the key's draw. On 2026-11-06 (Linux under gVisor, 2 cores, no GPU, lavapipe) a
key in a table's field cost 2.0 ms and 4,600 allocations, a table scroll 6.2 ms and 22,000
allocations, a canvas pan 1.2 ms. gVisor's thread clock ticks in
10 ms, so there read the mean over all frames, not the median.

In the running app, `studio.show_performance` puts Studio's memory and CPU at the end of the
footer, and a build with `--features perf` (GPUI's `profiler`) has GPUI's frame-time overlay on
⌘⌥⇧F (`studio::ToggleFrameOverlay`): frame times, then their distribution, then hidden.

```bash
cargo build --profile release-fast -p studio --example snapshot
OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token \
  OMNIGRAPH_STUDIO_GRAPH=knowledge OMNIGRAPH_STUDIO_TRACE=/tmp/trace.json \
  target/release-fast/examples/snapshot /tmp/out.png \
  'report:open Person' 'studio_graph::OpenType={"type_name":"Person"}' \
  'studio_table::AddFilter' 'measure:200,type'
```

To see where a frame goes, run a long measure and sample the process: on macOS,
`/usr/bin/sample <pid> 8 -file out.txt`, then read the main thread's heaviest frames; on Linux,
attach `gdb -batch -ex 'thread 1' -ex bt` repeatedly (valgrind does not start under gVisor). To
find what a part of a frame costs, take it out behind an environment variable and compare the
allocations per frame, which are exact.
