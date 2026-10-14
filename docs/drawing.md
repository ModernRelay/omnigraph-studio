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
  (spec 004, table performance).

## Guards

Every costly view counts its draws in tests (`#[cfg(test)] renders`), and a headless test drives
the interaction and asserts the count:

| Guard | Holds |
| --- | --- |
| `type_table` `typing_in_a_field_leaves_the_grid_as_it_was` | The grid follows the table's `notify`; typing in the filter field or a cell does not draw it |
| `studio_workspace` `a_tab_that_caches_its_parts_keeps_them_while_a_field_changes` | A tab that caches its parts keeps them while a field changes; a cached tab draws them again, which is the GPUI behavior the opt-out exists for |

Draw counts are exact, so they run in CI. Times are not: the machine's load moves them several
times over.

## Measuring

`crates/studio/examples/snapshot.rs` draws Studio offscreen against a server and measures frames:
`measure:<n>` with `,type` (a key into the focused field), `,scroll=<dy>` or `,drag=<dx>:<dy>`.
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
