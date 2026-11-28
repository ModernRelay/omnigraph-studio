# 035: A gallery of a type's files

Status: superseded 2026-11-23 by [048](../048-cards/plan.md), which replaces Gallery with Cards (a
file becomes a card's cover); the gallery is gone from the app as of 048's slice 1, built
2026-11-23: `gallery.rs` became `cards.rs`, `masonry.rs` was deleted, `tile.rs` makes a cover's
preview, a saved `gallery` tab restores as Cards and the `studio_gallery::…` action names are
aliases · was: draft; step 1 (the gallery tab, which writes nothing itself) built as a prototype
2026-11-18 as a uniform grid, reworked 2026-11-19 as a masonry (`crates/type_table/src/gallery.rs`,
`masonry.rs`, `tile.rs`) · drafted 2026-11-18, claims checked against Studio `50a495c` and
Omnigraph `v0.15.0` the same day. What follows describes the gallery as it was

## Problem

A node type whose rows carry files (a `Blob` property: photos, screenshots, decks, reports,
diagrams) is browsed today one cell at a time. The table shows "Blob · 2.1 KB · Preview" in a
column, and a file is seen only in the peek of the row under the cursor or in a tab of its own
(spec 004). Finding "the deck with the orange chart", checking the twelve screenshots an agent
uploaded on its branch, or picking a cover image means pressing ↓ and reading the inspector
once per row. People recognise a picture faster than its name; a table makes them read names.

## Who looks at a type's files, and why

The development graph and the cookbooks (`ModernRelay/omnigraph-cookbooks` at `main`,
2026-11-18) hold four kinds of Blob type:

| Graph | Type and property | Holds | Who browses it, and why |
|---|---|---|---|
| `script/dev-server` `notes` | `Note.attachment` (`Blob?`) | a PNG, an SVG, a PDF, text, bytes, an empty value, nulls (13 notes since this plan) | Studio's own work: every kind of tile and state in one type |
| `vc-os` | `Artifact.blob` with `kind` (deck, screenshot, image, doc, …) and `mime_type` | pitch decks, screenshots, transcripts | An investor finds "the deck with the market slide" among a company's artifacts: recognition, not recall |
| `second-brain` | `Artifact.blob` with `kind` (photo, document, recording, voice-note, …) | photos, scanned documents, recordings | A person looks for a photo or a scan by what it looks like, filtered to `kind = photo` |
| `hardware-program` | `TestRun.report` (`Blob?`), with `outcome` | a PDF report per test run, stored by `blob put` in a second commit | An engineer reviews the evidence of a test campaign: which runs have their report, and what each shows |

Four jobs follow, each a reason a list of names is the wrong shape:

1. **Find a file by how it looks.** Filter the type (`kind = deck`), then scan tiles. Wins over
   the table as soon as more than a screen of rows has files.
2. **Check what an agent uploaded on its branch.** The agent's `blob put`s are commits on
   `agent/<stamp>` (spec 011). On that branch the gallery marks each tile the branch added or
   changed (spec 033's one way of showing a change), so a person sees all twelve new screenshots
   at once and opens the one that looks wrong.
3. **Compare a row's file before and after.** On a branch, a changed tile can show what the
   target holds beside what the branch holds; the review of the branch links to the same.
4. **Pick one.** A cover image, the report to attach: move across the columns, Space to see it in
   the inspector, ↵ to open it, ⌘↵ to open its row.

## Principles

- II: the gallery reads one commit: the commit of the table it was opened from, or the head
  when opened at the head with nothing read yet. Every tile's file is read at that commit.
- V: tiles never move. A tile is placed only once its shape is known and every row before it
  is placed, in row order, so a preview landing, failing or being freed, or a later shape
  arriving, changes no placed tile. Rows a later page brings are placed after the last one.
- VI: every control shows its key; the File chip, the size control and each tile's states say
  what they are in their tooltips.
- VII: tiles appear as they are read, without motion; the cursor moves instantly.
- VIII: every gallery action is in `studio_actions` with a key in every keymap.
- IX: only the tiles meeting the view are laid out, paged a page ahead of the rows wanted,
  reads only for the tiles shown and the rows a screen below once scrolling pauses, at most
  four files read at once, a cap on what is read and decoded per tile, and a cap on the
  previews and first bytes held.
- X: designed against contract 0.14; E55 to E59 record what the engine could do instead.
- XIII: Gallery is a view of a set, so it is a tab, opened from the view strip of the table it
  comes from, which stays. It has reads (each tile's file) and a selection of its own.

## What exists today (checked)

- Studio `50a495c`:
  - Views. `studio_actions::ViewKind` (Table, Page, Tree, Canvas, Map, Source, Review,
    History); `Item::views` and `Item::view_action`; `studio_workspace::view_strip`;
    `OpenViewAt` on ⌘⌥1 to ⌘⌥3 (Ctrl-Alt elsewhere), and `OpenAs*` per kind, palette-only
    (`crates/studio/tests/keymaps.rs` `PALETTE_ONLY`). A node table's strip is Table · Tree ·
    Canvas (`table.rs` `TypeTable::views`); Tree opens the set through `ShowSetInTree` with the
    table's filters, or the picked rows.
  - Blob cells (spec 004). A Blob is never projected; each drawn cell is read on its own with
    `HEAD` at the page's commit (`GraphSession::blob_meta`, kept per commit in `pinned_blobs`,
    so the table, the peek and the Blob tab share one read), once scrolling pauses for 120 ms
    (`BLOB_READ_AFTER`), at most four at once (`BLOB_READS_AT_ONCE`), and a read for a cell
    scrolled away is dropped (`TypeTable::want_blob_metas`). Dropping the task that holds a
    session read stops its request (`GraphSession::counted_read_as`).
  - Previews. `blob::value_kind` judges a value by its first 4 KiB (`KIND_PREFIX`): PNG,
    JPEG, GIF, WebP, SVG (text whose first element is `<svg`), PDF, text, bytes; the peek, the
    Blob tab and a saved name share it. The peek reads the prefix, then the whole of a drawn
    kind up to 8 MiB (`IMAGE_MAX`), and decodes off the main thread into a `ShownImage` it
    holds and frees (`ShownImage::release`, since 2026-10-13: GPUI's asset cache kept every
    decoded image, about 100 MB for an 8 MB JPEG). A PDF's page is drawn by CoreGraphics on
    macOS only (`pdf.rs`); elsewhere `pdf::render` refuses.
  - Paging. `model::Paging` (keyset pages pinned to the first page's commit, each page twice
    the last up to 8 pages, read a page ahead: `wants_more`); `builder::node_page`.
  - Branches. A table on a branch reads the net diff from the merge base
    (`table.rs` `read_branch_diff`, `branch_diff::BranchDiff`) and marks rows as spec 033 says.
  - The ledger (spec 030) names a read's asker by the scope it was read through
    (`ledger::ReadView`); `blob_bytes` was counted under `"blob_head"`.
- Omnigraph `v0.15.0` (`crates/graph_client/openapi/omnigraph-0.15.0.json`; requests sent
  with curl to a release server on the development cluster, 2026-11-18):
  - `GET`/`HEAD /graphs/{id}/blob` by `entity`, `type`, `id`, `property`, `branch` or
    `snapshot`: one cell. Managed values answer `ETag`, `Content-Length`, `Accept-Ranges:
    bytes` and one `Range`; external ones a `302` to their URI, never fetched. No route lists a
    type's Blobs, answers several cells, or makes a preview; no thumbnail anywhere.
  - `Content-Type` is `application/octet-stream` for every value: no media type is kept.
  - An `ETag` names the type's table version, not the bytes: after a write to another Note's
    title on a branch, an unchanged `lance-versions` attachment answered a new ETag there
    (`"27571470…"` then `"10657842…"`) while `main` kept `"ffa16d8d…"`.
  - GQ cannot read a Blob: `return { $n }` leaves it out, and even `$n.attachment is not null`
    is refused (`T7: blob comparisons in filters are not supported`). So a page cannot say
    which rows have a file, and no filter keeps only those.
  - `GET /branches/diff` carries a changed Blob whole, as `base64:` text, in both the `before`
    and the `after` image (a 690-byte PNG replaced by a 110-byte SVG on a scratch branch).
- The Omnigraph skill's Blob reference (`omnigraph skill show references/blobs.md`, CLI
  `0.15.0`): Blobs cannot be projected, filtered, ordered or aggregated; reads are one cell at
  a time; the server reads managed values in 4 MiB pieces; an `ETag` is an opaque validator,
  not a content hash.

## Design

### When it is offered

- A node table whose type has at least one Blob property gains **Gallery** as the last entry
  of its view strip: Table · Tree · Canvas · Gallery, ⌘⌥4 (Ctrl-Alt-4), ⌘⌥⇧4 beside, and
  `studio_graph::OpenAsGallery` in the palette. A type with no Blob shows no Gallery: the
  schema fixes it for the tab, so the strip does not shift.
- Its tooltip says what it opens: "Show these rows as tiles of their attachment", or "Show the
  3 selected rows as tiles of their attachment".
- Edge tables have no strip, and no gallery (Deferred).

### What it opens

- The set the table shows, fixed at opening (spec 020): its filters and order, at its commit.
  A table at the head hands over the commit it read (`snapshot`), so the gallery's first
  pages are the table's own, answered from the session's memo, and its tiles' `HEAD`s are the
  table's cells'. A table held at a commit opens a gallery held there.
- Rows picked in the table: the gallery holds those rows alone, in the table's order. The
  cursor's row is the gallery's cursor tile.
- Which Blob: the property the table's cursor is on when it is a Blob, else the first Blob
  property in schema order. The toolbar's **File** chip lists the type's Blob properties,
  checked while shown; `F` shows the next one. The choice is saved with the tab.
- The tab is titled "Gallery: Note", or "Gallery: Note, design" with its filters, and comes
  back after a restart (`gallery` item kind) at the commit it held, or at the head.

### Layout: a masonry

Three layouts were compared. On 2026-11-18 the uniform grid was chosen; on 2026-11-19 Ragnor
asked for the gallery to be "more Pinterest style", and the masonry's two costs turned out to
be answerable within today's API (Decisions):

| | Uniform grid | Justified rows (Flickr, Google Photos) | Masonry (Pinterest) |
|---|---|---|---|
| Needs before placing a tile | nothing | each file's shape | each file's shape |
| Where the shape comes from | — | the first bytes, which every tile reads anyway | the same |
| A shape arriving late | — | moves every later tile on the line, unless lines wait | moves nothing, when tiles are placed strictly in row order |
| ↑ and ↓ | the tile above or below | the nearest by x | the tile above or below in its column |
| Mixed kinds (PDF, text, bytes, none) | one square each | a shape per kind | a shape per kind |
| A picture | cropped or boxed in a square | its own shape | its own shape |

**Chosen: a masonry.** Columns of one width; each tile as tall as its file's shape needs and
its caption under the picture; rows placed in row order, each into the shortest column (the
first of equal ones), `space::LG` apart (`masonry::Masonry`).

- **Shapes from the bytes already read.** A tile's first read, its first 64 KiB
  (`FIRST_READ`), is made for every tile anyway: it judges the kind and holds most icons and
  texts whole. Its header gives a picture's size: a PNG's IHDR (bytes 16 to 24) and a GIF's
  screen descriptor (bytes 6 to 10) read directly, a JPEG's frame header and a WebP's through
  the `image` decoder, past whatever metadata comes first; an SVG's from its root element's
  `viewBox`, or its `width` and `height` (`tile::Facts::shape`). Measuring therefore adds no
  request: the preview's read goes on from the bytes kept. E58 (the media type and an image's
  size answered by `HEAD`) would make the shape known from the `HEAD` alone, before any byte.
- **A shape for every kind** (`tile::shape`), so every row places as soon as its kind is
  known: a picture its own height over width, held between 0.4 and 2.2
  (`masonry::MIN_RATIO`, `MAX_RATIO`; the picture fits inside); a picture whose size is not in
  its first bytes a square; a PDF a letter page upright (11 ÷ 8.5); text a square; other bytes
  4 : 3 landscape; no file, an empty value, an external reference or a failed read a short
  band, 1 : 2.
- **Placed tiles never move.** A row is placed when its shape is known and every row before it
  is placed. A shape that lands early waits, kept; one that lands late places itself and the
  rows waiting behind it. On a branch, nothing is placed until the branch's diff is read, since
  the rows it removed go first. Rows still waiting show as a quiet band at the end of each
  column (`ds::size::TILE_BAND`, the faint highlight), "Reading…" in the shortest one where the
  next tile goes; never as a placeholder that later resizes.
- **Measured ahead.** The rows a screen below the last placed one, a screen's worth of square
  tiles and a column more, have their first bytes read while the shortest column ends within a
  screen of the view's bottom, so a scroll rarely meets the band. Their first bytes are kept
  for their preview's read.
- **Column width** follows the tile size: at least Small 128, Medium 176 (default), Large 240
  pixels (`ds::size::TILE_SMALL`, `TILE`, `TILE_LARGE`), as many columns as fit the document
  less `space::EDGE` either side and `space::LG` between, grown to fill it. A resize or a size
  change (`=` and `−`, the toolbar's segmented control) flows the columns again from the shapes
  kept, with no read; another file shown (`F`) places them again from the first.
- **A tile**: the preview well (`surface::well`), its corner concentric within the tile's
  (`radius::within(radii.md, ring + XS)`), the picture filling it edge to edge with the same
  corner and a `surface::image_outline` hairline; a picture held to the bounds, or a small one
  never drawn past twice its pixels, sits centered and square-cornered. Below it the caption
  (`size::TILE_CAPTION`): the row's name (its label, else its key) in body text, then a meta
  line, the file's kind, pixel size and size ("PNG · 640 × 400 · 2.6 KB"), cut with an ellipsis
  and whole in the tooltip.
- The cursor's tile has the focus ring (`surface::focus`, `size::ACCENT_LINE`); a picked tile
  a check in its preview's corner and the picked fill (`surface::picked`).

### What a tile shows

| The cell holds | Preview | Meta line |
|---|---|---|
| Not read yet | not placed: the band at the column's end | — |
| Measured ahead, not shown yet | placed, at its shape; the empty well until shown | the kind and size |
| PNG, JPEG, GIF, WebP up to 8 MiB and 40 megapixels | the image, scaled to at most 480 px on its long side off the main thread | "JPEG · 3024 × 4032 · 2.4 MB" |
| SVG up to 8 MiB | the drawing, as GPUI draws an SVG | "SVG · 1.1 KB" |
| PDF up to 8 MiB | its first page on macOS; elsewhere the document icon | "PDF · 766 B" |
| A larger image or PDF | the kind's icon | "JPEG · 14 MB · open to see it" |
| Text | its first lines, in the code face | "Text · 122 B" |
| Other bytes | the file icon and "Bytes" | "Bytes · 1 KB" |
| An empty value | "Empty" | "0 B" |
| Null | a dash in the disabled colour | "No attachment" in the disabled colour |
| An external reference | the link icon and its host; the URI in the tooltip, never fetched | "External · s3 host" |
| A read that failed | the warning icon | "Could not read"; the reason in the tooltip |

The image's pixel size comes from its header in the first bytes read, so it shows before
the image is decoded, and for one too large to decode.

### Selection and the keyboard

Key context `Gallery`; every key is an action in `studio_gallery` unless named:

| Key (macOS; Ctrl for ⌘ elsewhere) | Action | Does |
|---|---|---|
| ↑ ↓ | `MoveUp`, `MoveDown` | the tile above or below in the cursor's column |
| ← → | `MoveLeft`, `MoveRight` | the tile in the next column whose vertical middle is nearest the cursor tile's, the higher of two as near; nothing past the edge |
| Home, End | `MoveToFirst`, `MoveToLast` | the first tile; the last tile placed |
| Space | `studio::ToggleInspector` | the inspector shows the cursor's row, as the table's peek: every property, its relationships, its files |
| ↵ | `OpenFile` | the file in its Blob tab (spec 004) at the tile's commit; a null or empty cell opens the tab's empty state with Choose File… |
| ⌘↵ | `OpenRowPage` | the row's page |
| X | `TogglePick` | picks the cursor's tile, or takes it out |
| ⌘A | `PickAll` | picks every tile read |
| Esc | `ClearPicks` | takes every pick out |
| ⌘C | `CopyIds` | the picked rows' ids, a line each, else the cursor's |
| F | `ShowNextFile` | the next Blob property on the tiles |
| = and − | `LargerTiles`, `SmallerTiles` | the tile size |
| ⌘R | `Refresh` | reads the set again: from the head, or at the held commit |

A click puts the cursor on a tile; a second click opens it, as ↵; ⌘-click picks it; ⇧-click
picks from the last tile picked to it. The view strip's Tree holds the picked rows, or the set;
Canvas walks from the cursor's row; Table returns to the table, which stayed.

Bulk actions that write (delete, set a property) stay in the table, where their dialogs are:
the gallery's picks carry to a table opened from its strip (step 2). The gallery writes
nothing in step 1; ↵ opens a Blob tab, which writes as spec 004 says.

### Paging and virtualization

- Rows are read as the table reads them (`builder::node_page`, keyset by the table's order,
  `Paging` at the gallery's commit, the graph's page size), a page ahead of the rows wanted:
  those placed and those measured ahead. A gallery of picked rows reads them by id, a page at a
  time, moving on in the list only once a page is read, so Retry reads a failed one again. A
  page that shows no row and is not the last reads on at once (sorted, the rows with a value ran
  out; listed, its rows are gone), and the gallery says "Reading…" until a row shows or the set
  ends. At a commit made before a property was added, rows are read as columns without it, as
  the table reads them.
- The cursor opens on the table's cursor row: pages are read on until it is read, every row
  before it is measured and placed, and the columns scroll to it. A listed set on a branch reads
  the branch's diff too, marking its tiles and showing the listed rows the branch removed; read
  whole, it counts the rows it shows.
- The columns are one scroll area whose content is as tall as the tallest column (and the band);
  each frame lays out only the tiles that meet the view and half a screen either side, found by
  a search in each column (`Masonry::meeting`), so a frame costs what it shows, not what is
  placed. The count reads as the table's ("13 rows", "40 of 13,402 read").

### Reading the files

- Reads start once scrolling has paused for 120 ms: first the tiles shown, in row order, then
  the rows measured ahead; at most four tiles at once (`gallery::READS_AT_ONCE`), which leaves
  the connection's other four slots to pages, counts and the inspector. A shape landing that
  places tiles asks for what the view wants next at once.
- Each tile: its `HEAD` at the commit (shared with the table's cells and the peek); then its
  first 64 KiB (`FIRST_READ`), which judges its kind, gives an image's size and holds a text's
  first lines; then, for a tile shown, an image, an SVG or a PDF up to 8 MiB whose header fits
  40 megapixels, the rest in the server's 4 MiB ranges. A row measured ahead stops after its
  first read, its bytes kept for the rest; a file whole in its first read has its preview made
  at once. Nothing else is read for a tile.
- The bytes are decoded and scaled off the main thread to at most 480 px on the long side
  (`THUMB_EDGE`, twice the large tile), then dropped: a tile keeps only its preview, and a
  text's first lines.
- A read for a tile the view no longer wants (scrolled away) is dropped (its request stops)
  and asked again when it is.
- The previews and kept first bytes are capped at 64 MiB together (`PREVIEWS_HELD`, about
  seventy previews at the cap's 480 px); past it, those farthest from the span shown are freed
  (`ShownImage::release`), rows not placed first, and read again if they come back. The tiles
  shown keep theirs even past the cap, so a screen that alone holds more is never read again
  and again. Every shape is kept, at a few bytes a row. Closing the tab or reading again frees
  them all.
- Reads go through a scope of their own (`ReadView::Gallery`, spec 030) while the gallery
  follows its branch, so the ledger names them (`GraphSession::blob_bytes_in` for ranges). A
  range is now counted as `blob_bytes`; before this plan the peek's and the Blob tab's ranges
  were counted as `blob_head`.
- Cost of a screen. A 1440 × 900 window with the sidebar open shows about six columns of Medium
  tiles, some 24 tiles of mixed shapes. With every `HEAD` already read by the table: 24 first
  reads, and for photos of about 3 MB one more each, about 48 requests and 72 MB; the rows a
  screen below cost one first read each (64 KiB), which their previews then go on from. The
  masonry adds no request to the grid's. A server thumbnail (E55) would make it 24 requests
  and well under a megabyte; E58 would let a row be placed from its `HEAD`.

### On a branch

On a branch other than `main` the gallery reads the branch's net diff from the merge base, as
the table does (`read_branch_diff`, the review's cap), and marks tiles as every change is
marked (spec 033):

- Added: the kind's mark `+` in `Color::Created` before the name, the caption on the added
  wash, and a bar of the colour along the preview's top edge.
- Changed: `~` in `Color::Modified` and the bar; when the shown file is among the changed
  properties the meta line reads "File changed" in the modified colour.
- Removed: the rows the branch removed come first, their name struck through and dimmed; their
  file is read where it last was, at the merge base. They cannot be picked, the inspector shows
  nothing for them, and ↵ opens their file held at the merge base, read-only.
- The toolbar counts them, "+3 ~2 −1", as the table's.
- Before and after (step 2): `\` (`ShowTarget`) turns every changed tile whose file changed to
  the target's file, read at the merge base, and back; the caption says "on main". The review's
  card of a changed Blob links to the same.

### States

| State | Shows |
|---|---|
| Reading the first page | `ds::loading_state("Reading…")` |
| No row | `ds::empty_state(Images, "No rows")`; filtered: "No rows match" |
| The first page failed | `ds::error_state("Could not read Note", reason)` with Retry (⌘R) |
| The Blob property is gone after a schema change | "Note has no files now" and Open as Table |
| Rows waiting for their shapes or their page | the band under each column, "Reading…" in the shortest |
| A later page failed | the shortest column's band says "Not read", the reason in its tooltip, and Retry |
| Newer data | "Newer data · Refresh" in the toolbar, as the table's |

### Drawing

The columns are a view of their own (`GalleryGrid`), made with the tab and drawn cached; the tab
answers `Item::caches_its_parts`. A tile's read changes its state at once but draws nothing:
the grid is drawn 30 ms after the first landing not yet drawn (`LANDING_DELAY`, the command
palette's), with every other landing meanwhile, so a screen of 24 tiles landing in a burst draws
the grid once rather than 48 times (measured in the test: 48 draws for 40 tiles before). A key
moves the cursor with one draw of the grid, and a scroll draws it once a frame, laying out only
the tiles near the view. Tests count the grid's draws (`docs/drawing.md`).
Each tile carries an element id for its click and tooltip; a screen holds tens of tiles, not the
table's hundreds of cells.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| A masonry: columns of one width, tiles as tall as their files' shapes, placed in row order into the shortest column (since 2026-11-19) | A uniform grid of square tiles (built first, 2026-11-18); justified rows | Ragnor, 2026-11-19: "more Pinterest style". The grid was chosen because placing by shape seemed to need a read per row before layout and to move tiles as shapes landed. Neither holds: the shape is in the first 64 KiB that every tile reads anyway, so measuring adds no request, only reads it a screen early; and placing strictly in row order, each tile only once every row before it is placed, never moves a placed tile (constitution V; `a_placed_tile_keeps_its_place_when_later_shapes_arrive`). Kinds without a shape take one each, so nothing waits on them. Justified rows would make a late shape move the tiles beside it, or hold a whole line back for its slowest file |
| Left and right go to the next column's tile whose middle is nearest | The next tile in row order; the tile at the same top | In columns of different heights a row's neighbour in order is anywhere on screen; the same top rarely exists. The nearest middle is where the eye goes |
| Rows waiting show as a band at the columns' end | A placeholder tile at a guessed shape | A guess resizes when the shape lands, which moves everything under it |
| A tab of its own, opened from the view strip | A toggle that swaps the table's grid for tiles | Constitution XIII: it has reads (every tile's file) and a selection of its own. The table's 19,600 lines stay untouched |
| Its own pages, pinned to the table's commit | Drawing the table's loaded rows | A view never follows its source (spec 020, "Set"); pinned to the table's commit its first pages are answered from the session's memo, so the shared rows cost nothing |
| Gallery listed only for types with a Blob | Listed and disabled everywhere | The schema fixes whether it applies, for the tab's life; a disabled Gallery on every table would say nothing useful |
| Previews decoded and scaled by Studio, from the whole file up to 8 MiB | Reading only a prefix (an embedded JPEG thumbnail); no previews past a smaller cap | The API has no preview (E55); the peek's 8 MiB cap is one rule for one person's attention. A JPEG's own thumbnail is optional and often missing |
| A first read of 64 KiB, also for rows measured ahead | The peek's 4 KiB; a smaller probe for the shape | One request for most icons, diagrams and short texts, and the image header nearly always in it: a camera JPEG's frame header follows its metadata, often tens of kilobytes. A smaller probe would need a second request for such a JPEG, and its bytes would be read again for the preview; the 64 KiB a row below costs are those its preview needs first |
| Previews kept at 480 px, whatever the tile size | Decoding at the shown size | Changing the tile size draws what is held, with no read |
| Bulk writes stay in the table | Delete and set from the gallery in step 1 | One dialog for each destructive action (constitution IV); the delete dialog is bound to the table (`DeleteRowModal` holds a `TypeTable`) |

## Open questions

1. Should rows without a file be hidden? They are short bands now. Without E56 a gallery of a
   large type where few rows have a file pages through them; leaving them out of the columns
   is possible (their `HEAD` is read before they are placed anyway) but makes the set's count
   and the gallery's disagree.
2. Which properties should a caption show beyond the name? An enum such as `kind` or `outcome`
   as a tag would help the cookbooks' types; a Caption menu, or the table's first visible
   columns?
3. Is 8 MiB per tile right? A screen of 8 MiB photos is 192 MB of reads. A lower gallery cap
   (2 MiB) with "open to see it" past it would bound a screen at about 50 MB.
4. Should the tile size be per tab, as built, or one setting for every gallery?
5. Should a gallery's picks carry back to the table it came from, so a bulk delete is one
   step? Spec 020 says each tab keeps its own selection.
6. Edge types with Blob properties have no strip; would a gallery of edges help anyone?
7. Should very tall pictures be cropped to the bound, as Pinterest crops them, rather than
   boxed whole inside it, as built?

## Deferred

- Step 2: before and after on a branch (`ShowTarget`), picks carried to the table for its bulk
  actions, Save files… for the picked tiles into a folder, and a caption property.
- The UI tools (spec 032) do not offer the gallery to agents yet.
- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md) and
  [docs/deferred-engine-changes.md](../../docs/deferred-engine-changes.md):
  - **E55** a server-made preview of a Blob cell (a small image of a picture, a PDF's first
    page), cacheable by content.
  - **E56** a Blob's metadata in GQ: whether a cell holds a value, its size, kind and digest, in
    a projection and a filter, so one page says what each tile holds.
  - **E57** a validator that names a Blob's bytes, stable across commits that did not change
    them, so a preview read at one commit stands at the next.
  - **E58** a media type and, for an image, its pixel size kept with a Blob and answered by its
    `HEAD`: a row's tile would be placed from its `HEAD`, with no first read for the rows below
    the view, and a picture past 64 KiB of metadata would not fall back to a square.
  - **E59** diff and change-feed images that describe a Blob by size and digest instead of
    carrying it whole.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | Cursor Bugbot's and Devin Review's findings on #144 | Each checked against the code; a test for each real one, run against the code before the fix (with only `retry_page` stubbed as the old Retry and `absent` empty) and after: `an_empty_page_that_is_not_the_last_reads_on`, `a_listed_batch_of_rows_gone_reads_the_next`, `retry_reads_the_listed_batch_that_failed`, `a_commit_before_a_property_was_added_reads_the_rest`, `the_cursor_opens_on_its_row_past_the_first_page`, `the_cursor_opens_on_a_removed_row_once_it_is_placed`, `a_resize_reads_the_tiles_it_brings_into_view`, `a_listed_gallery_on_a_branch_marks_its_rows` | All eight failed before and pass after. A decoded preview dropped with its cancelled read is no leak: never drawn, it holds no texture and no asset (`a_preview_never_drawn_holds_nothing_of_gpuis`, GPUI `Image::is_asset_cached`) |
| 2026-11-19 | A placed tile never moves, and the columns are placed in row order into the shortest | `masonry::tests` `a_placed_tile_keeps_its_place_when_later_shapes_arrive` (row 3's shape unknown: three placed, none after; once known, the first three where they were, the same as placing all at once), `rows_go_into_the_shortest_column_in_their_order`, `the_tiles_meeting_a_span_are_found_in_each_column`; `gallery::tests` `a_placed_tile_keeps_its_place_when_later_shapes_arrive` (the fake server holding n003's bytes: three placed while later shapes land and wait, a band under the columns; let go, all eight placed and the first three unmoved) | Pass |
| 2026-11-19 | Shapes come from the first bytes, and every kind has one | `tile::tests` `a_tiles_shape_comes_from_the_first_bytes_or_its_kind` (a 300 × 450 JPEG past 20 KB of metadata, from its first 21 KB; a PNG from 32 bytes; an SVG's `viewBox`, its `width` and `height` with `px`, and a `100%` one; a PDF, text, bytes); `gallery::tests` `a_tile_is_as_tall_as_its_files_shape` (200 × 400 and 400 × 100 PNGs, a PDF, no file) | Pass. The `png` decoder's `into_dimensions` refused 32 bytes (it reads to the first image data), so a PNG's and a GIF's sizes are read from their headers directly |
| 2026-11-19 | The arrows move within and across columns; a size change reads nothing | `masonry::tests` `the_arrows_move_within_and_across_columns`; `gallery::tests` `the_arrows_move_within_and_across_columns` (40 notes of three shapes: ↓ stays in column 0, → lands on column 1's tile whose middle is nearest, checked against every tile there, ← past the first column stays; X, ⌘A, Esc; − − then = = with the fake server's read log unchanged by the reflow; F) | Pass |
| 2026-11-19 | Shown tiles are drawn, rows a screen below measured, nothing further read, pages read as the columns near their end | `gallery::tests` `the_tiles_shown_are_drawn_and_the_rows_below_measured` (80 notes of a 300 × 300 noise PNG past 64 KiB among 400: each tile shown read in its first range and the rest; the first row below in its first range alone, its 64 KiB kept; nothing 50 rows past the last placed; reads for tiles scrolled away before they were sent ask nothing), `pages_are_read_as_the_columns_near_the_end` (5,000 notes: fewer than all read at opening, then all of them, none twice, as the view scrolls to the columns' end), `previews_past_the_cap_are_let_go_farthest_first`, `a_branch_marks_its_tiles_and_places_removed_rows_first`, `a_key_draws_the_grid_once_and_previews_land_together` | Pass |
| 2026-11-19 | The masonry costs no more requests than the grid | The 2026-11-18 measurement again (offscreen `reads:`, `OpenType Note` then `OpenViewAt 3`, the `v0.15.0` release server on `script/dev-server`'s 13 notes) | The gallery's step: its page `Held(Read)`, no `HEAD`, 10 `blob_bytes` as `View(Gallery)` for the 10 stored values, the peek's one: as the grid's. A first run sent 11: a shape read already on the wire was dropped when a landing shrank the rows measured ahead, and sent again; shape reads out now finish |
| 2026-11-19 | The masonry draws every kind and state, light and dark, and a branch's changes | Offscreen snapshots at 1440 × 900, 2x, of the notes graph after the rework: Medium in Studio Light and Studio Dark, the cursor moved by → and ↓; the branch `agent/20261118-gallery` (now with a 320 × 480 poster added); Small with three tiles picked | Each picture at its shape (square icon, wide charts, a portrait poster), the PDF a letter page, text square, bytes 4 : 3, empty and null cells short bands; → went to the next column's nearest tile, ↓ down its column; the removed note first and struck, `~` and `+` with their bars and wash, "File changed"; "3 picked" |
| 2026-11-18 | The Blob route makes no preview and keeps no media type | `HEAD` and `GET` with `Range: bytes=0-15` on `notes` `Note gpui-panes attachment`, a `v0.15.0` release server on `script/dev-server`'s cluster; the OpenAPI document's `blobGet`, `blobStat` parameters and headers | 200 and 206 with `etag`, `content-length`, `content-range`, `accept-ranges`, and `content-type: application/octet-stream`; no route or parameter for a preview, a listing or several cells |
| 2026-11-18 | An ETag changes when another row of the type is written | `HEAD` of `lance-versions` on `main` and on a scratch branch, before and after `update Note set { title: "Probe" } where slug = "merge-preview"` there | `main` `"ffa16d8d…"` both times; the branch `"27571470…"`, then `"10657842…"`: a preview cannot be kept by ETag across commits (E57) |
| 2026-11-18 | GQ cannot tell which rows have a file | `POST /query` `match { $n: Note  $n.attachment is not null }`; `return { $n } limit 2` | 400 `T7: blob comparisons in filters are not supported`, `effects: none`; the rows carry no `attachment` key (E56) |
| 2026-11-18 | The gallery's keys, paging, reads and drawing | `type_table` `gallery::tests`: `the_keys_move_across_the_grid_and_pick` (13 notes: ← → across line ends, ↑ ↓ by the line, Home, End, X, ⌘A, Esc, = and −, F), `pages_are_read_as_the_lines_drawn_near_the_end` (500 notes: 200 read, then the rest once line 150 ÷ columns is drawn, none twice), `a_gallery_of_listed_rows_holds_them_in_their_order`, `only_the_tiles_drawn_are_read_and_those_scrolled_away_are_dropped` (400 notes: a `HEAD` per tile drawn and none far below; four reads at once; reads for lines scrolled past before they were sent ask nothing), `previews_past_the_cap_are_let_go_farthest_first`, `a_branch_marks_its_tiles_and_shows_removed_rows_first`, `a_key_draws_the_grid_once_and_previews_land_together`; `tile::tests` (a 1200 × 600 PNG held as 480 × 240, its size read from 64 bytes; text's first 12 lines; caps) | All pass. Before `LANDING_DELAY` the landing test drew the grid 48 times for 40 tiles |
| 2026-11-18 | Opening Gallery from a table costs only the files' bytes | Offscreen, `crates/studio/examples/snapshot.rs` with `reads:` (spec 030) against a `v0.15.0` release server on `script/dev-server` (the 13 notes of `dev/notes-seed.jsonl`), Linux 4.19 under gVisor, lavapipe, debug build: `OpenType Note`, then `OpenViewAt 3` | The table's step sent 13 `blob_head` and its page. The gallery's step: its page `Held(Read)` from the memo, no `HEAD` (the table's, kept at the commit), 10 `blob_bytes` as `View(Gallery)` for the 10 stored values (2 nulls and an empty value read nothing more), and one by the peek |
| 2026-11-18 | The gallery draws every kind and state, light and dark, and a branch's changes | Offscreen snapshots at 1440 × 900, 2x, of the notes graph: Medium tiles in Studio Light and Studio Dark with the cursor moved by → twice; on a local branch `agent/20261118-gallery` (an added note with a PNG, a note's PNG replaced through `PUT /blob`, a title changed, a note removed); Small tiles with three tiles picked by X | PNG, transparent PNG, SVG, text, bytes, empty, null and PDF (the icon: pages are drawn on macOS only) tiles; the removed note first, struck through and dimmed; `~` with the modified bar, `+` with the added wash and bar, "File changed" on the replaced file; "+1 ~2 −1"; "3 picked" in the status bar; the inspector follows the cursor |
| 2026-11-18 | A branch diff carries a changed Blob whole | `PUT /blob` of a 110-byte SVG over the 690-byte PNG on a scratch branch, then `GET /branches/diff?source=…&target=main` | One `update` whose `before.properties.attachment` and `after.properties.attachment` are the whole values as `base64:` text (E59). The branch was deleted after |

## Corrections

- 2026-11-19 · The layout was a uniform grid of square tiles, chosen because a masonry seemed to
  need a read per row before layout and to move tiles as shapes landed. Ragnor asked for "more
  Pinterest style"; both costs turned out to be avoidable (Decisions), and the gallery is now a
  masonry. The grid's tests were replaced by the masonry's; the 2026-11-18 rows of the log name
  the grid's tests as they were.
