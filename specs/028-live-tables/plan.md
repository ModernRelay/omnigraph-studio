# 028: The table in front follows its branch

Status: in progress · amendments approved and made (constitution 1.5.0) · step 1 built 2026-11-10

## Problem

Ragnor asked (2026-11-10) that the data in the tab in front refresh itself, instead of waiting
for a click on "Newer data · Refresh". Agents and other people write to the graph while he
looks at it, and the chip makes him do by hand what Studio already knows to do.

Studio already knows when a view is behind. The session asks for each branch's head every 30 s
while a window is active (`HEAD_POLL`), reads the change feed when a head moves, and each view
asks whether anything it reads changed (`GraphSession::scope_since`). A type table that is
behind moves on silently when nothing it shows changed, and otherwise shows the chip
(`TypeTable::compare_head`). It stops at the chip on purpose: constitution II says a view "does
not change under the reader", and V says new data comes as a chip, "never as a jump". Refresh
is also costly to your place: it clears the cursor, scrolls back to the top and shows the table
loading (`TypeTable::forget_rows`).

## Principles

II and V, amended below. IX (work is bounded: the table in front reads again, only what it has
loaded, and up to a limit). VII (silent: a refresh that lands says nothing).

## Amendments (constitution 1.4.0 → 1.5.0, minor; approved by Ragnor 2026-11-10)

**II. One snapshot per view**, the third sentence:

> ~~When the branch moves, the view offers Refresh; it does not change under the reader.~~
> When the branch moves, the view offers Refresh. A view that follows its branch, is in front
> and holds no draft of the person's may instead read the new commit in the background and
> take it in one frame, keeping the person's place (spec 028). A view pinned to a commit never
> moves.

**V. Nothing moves under you**, the last sentence:

> ~~New data announces itself as a chip, never as a jump.~~ New data announces itself as a
> chip, never as a jump. Where a view takes new data itself (II), the row under the cursor and
> the first row in view stay where they are.

Minor, not major: neither principle is reversed. A view still never changes while it is read
at a commit, and never moves the person's place.

## Design

- **When.** On the head move the table already compares (`compare_head`), when it would set
  `newer` today. No new timer: the session's 30 s poll is the trigger, and it already runs only
  while a Studio window is active.
- **Only the table in front.** The refresh is owed, as a branch change's reload is owed today,
  and read when the table is next drawn with its window active. A tab not drawn, or a window
  behind another app, reads nothing and keeps the chip until it is in front again.
- **Never when the person has work there.** The chip stays, as today, while any of these hold:
  the table is pinned to a commit (`at_commit`); a cell edit or a fill is open; one of its
  writes is out; it holds drafts on its branch; a file is still being stored; the schema moved
  (its columns would change); it is grouped (first step only); or it has loaded more rows than
  `LIVE_ROWS` (four of the graph's pages), so a long scroll never costs a large read every poll.
- **Read in the background, then take it in one frame.** One read at the head of as many rows
  as the table has loaded (at least its first page), with the old rows still on screen and
  nothing showing as loading. When it lands, the rows, the pin and the paging are swapped
  together, and the counts and what the branch changed are read again at the new pin, as after
  Refresh. A read that fails leaves the table as it was, with the chip.
- **Keep your place.** The cursor stays on its row by id, and on its column; a row gone takes
  the cursor to the row now in its place. Picked rows are already kept by id. The first row in
  view stays first when it is still there; otherwise the view keeps its offset. The peek and
  an open row page follow their own heads, as today.
- **Silent.** No toast and no chip for a refresh that landed. The tab's "newer" dot never
  shows for it.

### Build order

1. Ungrouped node and edge tables: owed refresh, background read, swap keeping the cursor and
   the first row in view, the guards above, tests. *Built 2026-11-10* (`TypeTable::follow_head`,
   `take_head`, `follows_live`).
2. Grouped tables, then the node page and peek, the relationship tree and the canvas, on the
   same rule. Query tabs only once a query can say what it reads (they have no scope today, so
   every commit counts as a change) and its read has a cost bound.

## Decisions

- **The poll, not a stream.** The server sends no changes; the 30 s head poll is what tells
  Studio. A shorter poll for the tab in front can come later as a setting.
- **Swap, not merge.** Rows are read again whole, not patched from the change feed: the feed
  names entities, not their place in a sorted, filtered page.
- **Rows gone disappear.** V keeps a row that stopped matching after the person's own edit;
  rows another writer removed go with the swap, as on Refresh.

- **The scope keeps what the old rows were read for.** A swap adds what the new rows were read
  for to the table's scope and keeps the old: the next head move may count a change to a row
  no longer shown, and read again for nothing, but never misses one. Refresh resets it.
- **Known gap.** Relationship and Blob cells are read again for the rows drawn after a swap, so
  they show empty until their reads land.

## Deferred

- A stream of commits from the server, so a table learns of a commit when it lands rather than
  at the next poll (the Commits panel, spec 026, defers the same).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-10 | The table in front takes its branch's head in one frame, keeping the cursor on its row; behind another window it offers Refresh and follows once active; an open edit keeps the chip | `type_table` `the_table_in_front_takes_its_branchs_head_keeping_its_place`; the `type_table` suite (no other test changed) | Pass. The scroll anchor (first row in view stays first) is not covered by a test, and not yet checked in the running app |

## Corrections
