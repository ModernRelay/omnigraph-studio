# 048: Cards, a view of a type's nodes

Status: accepted 2026-11-23 · slice 1 built 2026-11-23 (Gallery is Cards: the names and their
aliases, a saved `gallery` tab restored, the uniform grid, ↵, ⌥↵ and G; a card is its cover and
its title, offered where Gallery was); slices 2 to 7 not built · supersedes
[035](../035-gallery/plan.md), whose Gallery left the app with slice 1 · Studio's claims checked
against `7f3e7e92` on 2026-11-23; Omnigraph's are spec 035's, checked against `v0.15.0` on
2026-11-18 and not rechecked

## Problem

A node type is read today as a table: one line a node, a column a property. That is the right
shape for comparing one property down many nodes and the wrong one for reading a node as a
thing: its name, the few values that matter, its picture. Notes, people, companies, test runs
and artifacts are scanned as things.

Gallery (spec 035) was a first answer and covers one case of it. It is offered only for a type
with a file property, a tile shows the file and the node's name and nothing else, and its
filters are fixed when it opens, so narrowing what is shown means going back to the table and
opening another gallery. The person who asked for this plan tried it on 2026-11-23 and said it
"doesn't make any sense" as it stands, and asked for:

- nodes shown as cards;
- a card format that can be changed;
- each card shown whole in the inspector;
- a canvas made from a card;
- the same filters as a table.

This plan replaces Gallery with **Cards**: a view of every node type, where a card shows the
properties the person chooses and can show a file as its cover. What Gallery showed is then one
card format among others.

## Principles

- II: a Cards tab reads one commit: the commit of the table it was opened from, or the head.
  Every card's values, cover and relationships are read at that commit. A filter or an order
  changed in the tab reads again at the same commit.
- V: cards never move. Every card of a tab shows the same fields, each on one line, and the
  cover sits in a box of one shape, so every card is one size and a value, a picture or a
  relationship landing fills its place. Rows a later page brings are placed after the last one.
- VI: every control shows its key; the Fields chip, the size control and each state say what
  they are in their tooltips. A control that cannot act says why.
- VII: cards appear as they are read, without motion; the cursor moves instantly.
- VIII: every Cards action is in `studio_actions` with a key in every keymap.
- IX: only the cards meeting the view are laid out; rows are paged a page ahead; a cover is read
  only for a card shown, at most four at once, with a cap on what is read, decoded and held;
  relationship fields are read for the cards near the view, a hundred rows a read.
- X: designed against contract 0.14. E42 (a view registry) and E55 to E59 (what the engine
  could say of a Blob) record what the engine could do instead.
- XIII: Cards is a view of a set, so it is a tab, opened from the view strip of the tab it
  comes from, which stays. It has state (its filters, order and format), a selection and reads
  of its own, which is the article's test for a tab and not a toggle. The article's list of
  what each view holds gains "Cards a set" (constitution 1.6.1).

## What exists today (checked)

Studio at `7f3e7e92`, before slice 1: what Gallery was. Read, not run, unless a row of the
validation log says otherwise.

- **Gallery** (spec 035, PR #144, merged 2026-11-20).
  - `crates/type_table/src/gallery.rs` (2,599 lines): `TypeGallery`, a tab of a node type's
    rows; its body is a cached child view (`GalleryGrid`).
  - It reads rows as the table does (`builder::node_page`, or `builder::nodes_by_ids` for
    listed rows; `model::Paging` pinned to the first page's commit), keeps a cursor and picks,
    reads the branch's diff (`table::read_branch_diff_from`) and marks the tiles it changed.
  - Files: one `HEAD` per tile (`GraphSession::blob_meta_in`), then the first 64 KiB
    (`tile::FIRST_READ`), then the rest of a drawn kind up to 8 MiB, decoded and scaled off the
    main thread (`tile::look`); at most four reads at once (`READS_AT_ONCE`), started 120 ms
    after scrolling pauses (`READ_AFTER`), 64 MiB of previews held (`PREVIEWS_HELD`).
  - Placement is a masonry (`masonry.rs`, 269 lines): a tile is placed once its file's shape is
    known, so shapes are read a screen ahead of the view.
  - Offered only when the type has a Blob property (`TypeTable::gallery_property`,
    `table.rs:8234`), as the strip's fourth entry: Table · Tree · Canvas · Gallery, ⌘⌥4.
  - Its filters and order are those of the table at opening (`ShowSetInGallery`); its toolbar
    (`gallery.rs:1684`) has no filter control. A date preset is resolved by the table before
    it is handed over (`gallery_action`, `table.rs:8253`), so "today" is the day of opening.
  - ↵ opens the file and ⌘↵ the node's page; there is no key for a canvas. Spec 020 says
    "Everywhere a node is focused, ↵ opens its Page and `G` opens its Canvas".
  - Tests: 20 in `gallery/tests.rs`, among them the draw-count guard
    `a_key_draws_the_grid_once_and_previews_land_together` (`docs/drawing.md`).
- **The table's filters** are not a part of their own. About 1,600 lines of `table.rs` (20,018
  lines) are methods on `TypeTable`: the pills and the draft row (`render_filter_bar`, :10021),
  adding and editing (`open_filter_draft` :8670 to `apply_filter_draft`), value suggestions,
  and filters through relationships (spec 025, `open_group_menu`). Every change ends in
  `reload(true, cx)`. ↵ and Esc are single table actions that choose their target by the
  field in focus. The suggestions deliberately do not `notify` the table, so typing does not
  draw the grid (`typing_in_a_field_leaves_the_grid_as_it_was`). What already stands alone:
  `Filter`, `FilterOp` and `Sort` (`gq_language::builder`), `model::filter_parts` and
  `filter_sentence`, `suggest.rs`, `offers::Offers`, the `filter_words` crate, and the `ds`
  parts (`filter_pill`, `filter_draft`, `chip_button`, `control_chip`, `sort_menu`).
- **Cells.** `TypeTable::render_cell`, `render_relation_cell` and `render_blob_cell` read the
  grid's state and cannot be called by another view. Free functions another view can call:
  `model::display_value`, `relative_value`, `chips_that_fit`, `node_label`;
  `studio_ui::cell_text`, `tags`, `format_value`; `ds::chip`, `ds::tag`.
- **Relationship cells** are read apart from the page: `TypeTable::load_relation_groups`
  (`table.rs:5096`) sends `builder::page_degrees` and `page_neighbours` for each relationship
  drawn, a hundred rows a read (`RELATION_READ_CHUNK`).
- **Columns.** `model::node_columns` gives the key, the properties without vectors, then a
  column a relationship; `ColumnLayout { hidden, order, widths }` is saved with the tab and in
  a saved view, and nowhere by type. `GraphOverrides` (`studio_settings.rs`) keeps per graph
  the page sizes, `views` and `labels` (the property that names a type's nodes).
- **A node's name** is `NodeType::label_property` (`gq_language/src/schema.rs:105`): the
  property chosen for the graph, else `name`, `title`, `display_name` or `label`, else the key.
- **Tabs.** A saved tab is restored by its kind's opener (`register_item_kind`); a kind nothing
  registers is dropped without a word (`studio_workspace/src/persistence.rs`,
  `Workspace::restore_saved`). There are no aliases for kinds. Actions do have them
  (`#[action(deprecated_aliases = […])]`).
- **Views.** `OpenViewAt { index }` opens a strip's entry by position, bound to ⌘⌥1 to ⌘⌥4.
  A tree of a set lists Table · Tree · Canvas (`relationship_tree.rs`, `views`). A saved view
  (spec 024) keeps a `shape`, which nothing reads: every saved view opens as a table.
- **A canvas** walks from one node (`ShowNodeOnCanvas`). The one canvas of listed nodes, a
  query's results (`ShowResultsOnCanvas`), has no strip and is not saved with the workspace.
- **Seen running** (2026-11-23, `script/dev-server`'s `notes` graph, a build of `1e375487`):
  the Note table's strip reads Table · Tree · Canvas · Gallery; Topic's stops at Canvas.

Omnigraph `v0.15.0`, as spec 035 checked it: a Blob is read one cell at a time (`HEAD`, `GET`
with one `Range`), is never projected, filtered or ordered by, and answers no media type; no
route lists a type's Blobs or makes a preview. Nothing in the contract or in the CLI's
configuration describes a view, a layout or a card.

## Design

### When it is offered

- Every node table's strip gains **Cards** as its last entry: Table · Tree · Canvas · Cards,
  ⌘⌥4 (Ctrl-Alt-4), ⌘⌥⇧4 beside, and `studio_graph::OpenAsCards` in the palette. Tree and
  Canvas keep ⌘⌥2 and ⌘⌥3, as on a node's page.
- A tree of a set of one type lists it too: Table · Tree · Canvas · Cards.
- Edge tables have no strip and no cards (Deferred).
- Its tooltip says what it opens: "Show these rows as cards", or "Show the 3 selected rows as
  cards".

### What it opens

- The set the table shows: its filters and order, at its commit. A table at the head hands
  over the commit it read, so the first pages are the table's own, answered from the session's
  memo. A table held at a commit opens Cards held there, read-only.
- Rows selected in the table: those rows alone, in the table's order. The table's cursor row
  is the cursor card.
- From then on the tab keeps **its own** filters, order and format. It never follows the table
  it came from, and the table does not follow it (spec 020, amended: a set is carried once).
- The tab is titled "Cards: Note", or "Cards: Note, design" with its filters as they are now,
  and comes back after a restart (`cards` item kind) at the commit it held, or at the head.

### A card

```
┌──────────────────────────┐
│ ▓▓▓▓▓▓▓▓ cover ▓▓▓▓▓▓▓▓▓ │  optional: one file property, fitted inside a box of one shape
│ ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ │
│ Harbour at dusk          │  title: the node's name (`NodeType::label_property`), else its key
│ body     Cover image fo… │  fields: the property's name, muted, then its value; one line each
│ written  2026-11-02      │
│ filedUnder  Design       │  a relationship is a field too (slice 6): its neighbours as tags
└──────────────────────────┘
```

- **The title** is the node's name in the heading weight, on one line. A node whose name is
  null shows its key.
- **A field** is one line: the column's title in the muted colour, cut to a third of the
  card's width, then the value, cut with an ellipsis. A value is drawn as the table's cell
  draws it: text, numbers and dates through `studio_ui::cell_text` (a date in relative words
  where the table uses them), an enum member as `ds::chip`, a list as "a, b, c +N", a null as
  a dash in the disabled colour, a relationship as `studio_ui::tags` with "+N" for the rest.
  The whole value is in the inspector; a card has no tooltip of its own per field.
- **The cover** is a box 4 wide by 3 high across the card. A picture is fitted inside it,
  centered, never drawn past twice its pixels, with `surface::image_outline`. The other kinds
  are Gallery's states, unchanged: an SVG drawn; a PDF's first page on macOS; text's first
  lines in the code face; other bytes, an image or PDF over 8 MiB or 40 megapixels, an
  external reference and a failed read as an icon; an empty value "Empty"; a null a dash. With
  the cover set to None no card has the box.
- **One size.** A card's height is the cover box, the title line and one line a field, from
  `ds` tokens. Every card of a tab is that height, whatever it holds.
- The cursor's card has the focus ring; a picked card the picked fill and a check in its
  corner; both as tiles have them today.

### The grid

- Columns of one width: at least Small, Medium (default) or Large (`ds::size::CARD_SMALL`,
  `CARD`, `CARD_LARGE`: 180, 240 and 340 pixels, settled by snapshot in slice 1; Corrections),
  as many as fit the tab less `space::EDGE` either side and `space::LG` between, grown to fill
  it.
- Cards are placed in row order, left to right, then down. Card *i* of *c* columns is in line
  *i ÷ c*, column *i mod c*. Nothing is measured before a card is placed.
- One scroll area as tall as its lines. A frame lays out only the cards of the lines that meet
  the view and half a screen either side.
- `=` and `−` (and the toolbar's S · M · L) change the size; a resize or a size change flows
  the columns again with no read.

### Format

A format is which fields a card shows and in what order, its cover, and its size.

| Part | Values | Default |
|---|---|---|
| Fields | Column keys in order: a property's name, or a relationship's key (`out:FiledUnder`) from slice 6. At most 8 | The first three properties in schema order, leaving out the name property, the key, files and vectors |
| Cover | None, or one of the type's file properties | The type's first file property; none when it has no file property |
| Size | Small, Medium, Large | Medium |

- The default needs no setup and holds for any graph, which matters because a format is
  personal (Limits). No relationship is a field by default: each costs two reads a hundred
  cards.
- **Where it is kept.** The tab holds its format and saves it with itself. A change is also
  written as the type's format, per graph, in `settings.json`
  (`graphs."<target>/<graph>".cards."<Type>"`), so the next Cards tab of that type opens the
  same way. A restored tab keeps its own.

  ```json
  "cards": { "Note": { "fields": ["body", "written"], "cover": { "property": "attachment" }, "size": "large" } }
  ```

  `cover` is `"none"`, `{ "property": "<name>" }`, or absent for the type's first file property.
  `fields` absent is the default rule. A field or a cover the schema no longer has is left out
  when the format is read, and the file is not rewritten for it.
- **The Fields chip** (toolbar, `ds::chip_button` in a `ds::control_chip`, as Columns is) opens
  a menu that stays open while it is used:
  - the shown fields, in card order, each with a submenu: Move up, Move down, Hide;
  - the hidden ones, in schema order; choosing one shows it last. Past eight shown, they are
    disabled: "A card shows at most 8 fields";
  - **Cover**: None, and each file property, the current one checked;
  - "Reset to the default".
  The chip's × is the reset, with the tooltip "Show the default card". `F` shows the next
  cover (None after the last), when the type has a file property.

### Filters and order

- **The same filter bar as the table, from the same code.** Pills, the draft row with its key
  and operator menus and its value suggestions, and filters through relationships (spec 025)
  move out of `table.rs` into a part both tabs use (How it is built). ⌘F adds a filter, ⌘⌥⌫
  clears them, ↵ applies the draft and Esc closes it, as in the table.
- A filter or order change reads the set again from its first page **at the tab's pinned
  commit**, keeps the picks that are still shown, and puts the cursor on the first card.
- A date preset ("today", "this week") is kept as the preset and resolved at each read, so it
  does not freeze at the day the tab opened. A filter on a property the schema no longer has
  is dropped with the toolbar notice the table gives.
- **Sort** is a chip of its own with the table's menu (`ds::sort_menu`).
- **A tab of selected rows** shows a lead pill "3 picked rows ×" and the Filters chip disabled:
  "Showing 3 picked rows. Clear the pill to filter Note". Clearing it shows the type's rows.
- **Filter in words** (slice 7) asks the same model with the same question. In Cards its
  answer sets filters and order; a grouping or columns in the answer are left out.
- "Newer data · Refresh" and ⌘R as in every data view.

### Selection and the keyboard

Key context `Cards`; every key is an action in `studio_cards` unless named. The single-letter
keys are bound in `Cards && !editing`, so they are text in the filter's field.

| Key (macOS; Ctrl for ⌘ elsewhere) | Action | Does |
|---|---|---|
| ← → | `MoveLeft`, `MoveRight` | the card before or after in row order, across line ends |
| ↑ ↓ | `MoveUp`, `MoveDown` | the card a line up or down; ↓ onto a short last line goes to the last card |
| Home, End | `MoveToFirst`, `MoveToLast` | the first card; the last card read |
| Space | `studio::ToggleInspector` | the inspector shows the cursor's node whole: every property, its files, its relationships |
| ↵, ⌘↵ | `OpenPage` | the node's page |
| ⌥↵ | `OpenCover` | the cover's file in its Blob tab (spec 004), at the tab's commit |
| G | `ShowOnCanvas` | a canvas walking from the cursor's node, as the strip's Canvas |
| X | `TogglePick` | picks the cursor's card, or takes it out |
| ⌘A | `PickAll` | picks every card read |
| Esc | `Cancel` | closes the filter draft; else takes every pick out |
| ⌘C | `CopyIds` | the picked nodes' ids, a line each, else the cursor's |
| F | `ShowNextCover` | the next file property as the cover, then none |
| = and − | `LargerCards`, `SmallerCards` | the card size |
| ⌘F, ⌘⌥⌫ | `studio_table::AddFilter`, `ClearFilters` | as in the table |
| ⌘R | `Refresh` | reads the set again: from the head, or at the held commit |

A click puts the cursor on a card; a second click opens its page, as ↵; ⌘-click picks it;
⇧-click picks from the last card picked to it. The strip's Tree holds the picked nodes, or the
set under the filters; Canvas walks from the cursor's node; Table opens the type's table.

Cards writes nothing. A value is changed in the inspector or on the node's page, which name
their branch as they do today.

### Reading

- **Rows** as Gallery reads them: `builder::node_page` by the tab's order, keyset pages pinned
  to the first page's commit, a page ahead of the cards wanted; listed rows by id.
- **Covers** as Gallery reads a tile's file, without the shapes read ahead: for the cards
  meeting the view, 120 ms after scrolling pauses, at most four at once; a `HEAD`, the first
  64 KiB, then the rest of a drawn kind up to 8 MiB and 40 megapixels; scaled to at most
  480 px off the main thread; 64 MiB of previews held, those farthest from the view freed
  first. A cover read for a card scrolled away is dropped.
- **Relationship fields** (slice 6), only for the relationships the format shows and the cards
  near the view: `builder::page_degrees` and `page_neighbours`, a hundred rows a read, as the
  table reads its relationship cells.
- Reads go through a scope of their own (`ReadView::Cards`, spec 030) while the tab follows its
  branch, so the ledger names them.
- With no cover and no relationship field a screen of cards costs no read beyond its page.

### On a branch

A branch's tab reads the branch's diff once, as the table does, and marks its cards with the one
change mark (spec 033): a bar in the change's colour along the top, the mark beside the title,
an added card's wash, a removed card dimmed with its title struck, shown before the others.
The toolbar counts them ("+3 ~2 −1"). Nothing is placed until the diff is read.

### States

| State | Shows |
|---|---|
| Reading the first page | "Reading…" (`ds::loading_state`) |
| No rows | "No rows", or "No rows match" under filters or a list |
| A page that failed | the error, and Retry (⌘R) |
| Held at a commit | "Read only" and the commit, in the toolbar, as the table |
| The head moved | "Newer data · Refresh" |

### Drawing

The grid is a cached view of its own, made with the tab (`CardsGrid`; `Item::caches_its_parts`
is true). A card has one element id and one click handler; a field has none. Values, covers
and relationship fields landing within 30 ms are drawn together. The guards
(`docs/drawing.md`): an arrow key draws the grid once; a screen of covers landing together
draws it once; a screen of relationship fields landing draws it once; a key typed in the
filter's field, and a move through its suggestions, draws it no times.

### How it is built

- **Cards grows out of Gallery's code.** `gallery.rs` becomes `cards.rs`. Kept: the reads and
  paging, the listed set, the cursor and picks, the branch diff, the cover reads and their
  caps, the preview drawing, the `Item` implementation and the cached grid. Removed: the
  masonry, the shapes read ahead, the waiting bands, the File chip, `tile::shape`. New: the
  uniform grid (a pure module, tested alone), the format, the fields, the Fields menu.
- **The filter bar is shared through a host trait.** The table's filter methods move to
  `crates/type_table/src/filter_bar.rs` as the provided methods of `FilterHost`, with the
  draft, its field, its menus and its choices in a `FilterBar` each host holds. A host keeps
  its own `filters`, answers what the bar asks (the schema, the type, the session, the pinned
  commit, where focus returns) and re-reads in `filters_changed`. The bodies, the notify
  pattern, ↵ and Esc, and the key contexts stay as they are, so the table behaves as before.
- **Relationship reads** move out of `TypeTable` into `relations.rs`, which both tabs call.
- **Names.** Old names keep working:

  | Was | Becomes | Kept working by |
  |---|---|---|
  | `ViewKind::Gallery`, `OpenAsGallery`, `ShowSetInGallery` | `ViewKind::Cards`, `OpenAsCards`, `ShowSetInCards` | `deprecated_aliases`; `cover` reads a saved `property` |
  | `studio_gallery::*`, key context `Gallery` | `studio_cards::*`, `Cards` | `deprecated_aliases`, so a person's keymap still loads. `OpenFile` becomes `OpenCover`, `OpenRowPage` `OpenPage`, `ClearPicks` `Cancel`, `ShowNextFile` `ShowNextCover`, `LargerTiles` and `SmallerTiles` `LargerCards` and `SmallerCards` |
  | saved tab kind `gallery` | `cards` | `gallery` stays registered to the same opener: the same set, that file as the cover, its size |
  | `ReadView::Gallery`; `ds::size::TILE_*`, `icon::tile` | `ReadView::Cards`; `CARD_*`, `icon::cover` | nothing outside Studio reads them |

  A person's `enter` in a `Gallery` context of their own keymap would now open the page, not
  the file; the release note says so.

### Limits

- Omnigraph has no notion of a view, a layout or a card. A format is a preference kept on this
  computer, as the property that names a type and saved views are. A teammate on the same
  graph sees the default card. Sharing waits on E42.
- A cover is still one read a card and a decode on this computer (E55 to E58).

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Cards replaces Gallery | Keep both in the strip; keep Gallery for file types only | The requester's call (2026-11-23). A card with a cover shows what a tile showed, so two views would answer one question |
| A uniform grid | A masonry, as Gallery is (spec 035's decision of 2026-11-19, reversed) | The requester's call. One size needs no shape read ahead and no waiting band, arrows go straight, and nothing can move (V) |
| A format is fields, their order, a cover and a size | Per-field options (label, lines); a free-form template | The requester's call for the first version. The others are Deferred |
| Its own tab | A rows-or-cards switch inside the table | The requester's call, and XIII: it has filters, a format, a selection and reads of its own. A closed branch (`feat/ux-p9-type-layouts`) drew cards as a toggle of the table |
| Last in the strip | Second, beside Table | ⌘⌥2 stays Tree and ⌘⌥3 Canvas on a table as on a node's page; ⌘⌥4 is where Gallery was |
| Offered for every node type | Only where the table shows few columns, or only with a file | "Only types with a Blob" is why Gallery was missing where it was looked for, and a strip that differs by type shifts |
| A set is carried once, then the tab keeps its own filters | Fixed at opening, as Gallery and Tree are; following the table | "The same filters as tables" asks for the bar. A tab that followed its source would change under the person (V) |
| The filter bar shared through a host trait | A separate filter-bar view; a second copy for Cards; Cards as a mode of `TypeTable` | A view of its own would need `filters` moved (19 uses outside the filter code) and changes Esc's order. A copy drifts: the tree already has a second, smaller one (`NeighbourFilters`). A mode would let the table's write actions reach cards |
| Evolve `gallery.rs` | Write Cards new | Its paging, picks, diff marks, cover reads and 20 tests are what Cards needs; ten of those tests pin paging, retry and cursor cases a rewrite would have to find again |
| One line a field | As many lines as the text | One height for every card (V); the whole value is one key away in the inspector |
| The default is three properties in schema order | The table's shown columns; every property | It must be good with no setup on any graph and stay short; a format remembered by type should not depend on which table tab opened it |
| ↵ opens the page, ⌥↵ the file, G the canvas | ↵ opens the file, as Gallery | Spec 020: ↵ opens a node's page and G its canvas wherever a node is focused |
| A format by graph and type in settings | Only with the tab; in a saved view | The next tab of the type should open the same way. A saved view cannot open as Cards yet (spec 024, step 2) |
| A canvas from one card | A canvas of the picked cards | XIII: a canvas holds one node. The one canvas of a list has no strip and is not restored |
| A tab of selected rows turns Filters off | Filter within the list | `nodes_by_ids` takes no filter today; Deferred |

## Order of work

Each slice is a pull request that stands alone.

| # | Slice | Proves it |
|---|---|---|
| 0 | This plan; 035 superseded; spec 020 and the constitution's wording | — |
| 1 | Gallery becomes Cards: the names and their aliases, a saved `gallery` tab restored as Cards, the uniform grid, ↵, ⌥↵ and G. A card is its cover and title; offered where Gallery is | Grid tests in place of the masonry's; Gallery's tests adapted; a saved `gallery` tab comes back as Cards; letters typed in a field are text; the draw counts |
| 2 | Fields and the format; every node type; the tree's link | The format's round trip in settings; the default on a type with and without files; the strips |
| 3 | The table's filter code moves to a file of its own, unchanged | A pure move: the table's 98 tests pass unedited |
| 4 | `FilterHost` | The table's 19 filter tests; a probe host; typing draws the grid no times |
| 5 | The filter bar and Sort in Cards | A change reads at the pin; a preset is resolved a read; typing draws the cards no times |
| 6 | Relationship fields | The table's relationship tests; a screen of them draws the grid once; two reads a relationship a hundred cards |
| 7 | Filter in words in Cards | Its tests against both hosts |

## Deferred

- Grouping in Cards: a grid by section needs a header line between sections.
- A saved view that opens as Cards (spec 024, step 2: `ViewShape` is kept and never read).
- A canvas of several picked cards (XIII, and a canvas of a list is not restored).
- Lines a field, a label that can be hidden, a free-form layout.
- Filters within a list of selected rows (`@id in` in the query builder).
- Cards of an edge type.
- Bulk actions on picked cards: they stay in the table.
- Offering Cards to agents through the UI tools (spec 032).
- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md) and
  [docs/deferred-engine-changes.md](../../docs/deferred-engine-changes.md):
  - **E42** a view registry a client may write, so a format is shared with everyone on the graph.
  - **E55** a server-made preview of a Blob cell.
  - **E56** a Blob's metadata in GQ, so a page says what each cover holds.
  - **E57** a validator that names a Blob's bytes, so a cover read at one commit stands at the next.
  - **E58** a media type and an image's size answered by `HEAD`.
  - **E59** diff images that describe a Blob instead of carrying it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-23 | Gallery is offered only for a type with a file | `script/dev-server`'s `notes` graph in a build of `1e375487`: the strips of the Note and Topic tables | Note: Table · Tree · Canvas · Gallery. Topic: Table · Tree · Canvas |
| 2026-11-23 | Gallery's parts are as named, and its filters are fixed at opening | Read `gallery.rs`, `masonry.rs`, `tile.rs`, `gallery/tests.rs`, `type_table.rs` and `table.rs` at `7f3e7e92` | As written in What exists today. 20 tests counted in `gallery/tests.rs` |
| 2026-11-23 | The table's filter code is methods on `TypeTable`, and its tests guard a move | Read `table.rs`; `grep -c` of its test attributes | 98 tests. 19 touch filter state or methods, by reading; not run for this plan |
| 2026-11-23 | A saved tab of an unregistered kind is dropped, and kinds have no aliases | Read `persistence.rs` and `Workspace::restore_saved` | Confirmed by reading |
| 2026-11-23 | A saved view's shape is never read; a results canvas is not saved | Read `studio_settings.rs`, `type_table.rs` (`open_saved_view`), `graph_canvas.rs` (`serialize`) | Confirmed by reading |

Slice 1 is built; slices 2 to 7 are not. Each slice adds its rows.

### Slice 1 (2026-11-23)

Built on `feat/cards-view`, on `028574f4`. Apple M5 Max, macOS 27.0.1, Rust 1.98.1, debug builds.

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-23 | The gallery's tests passed before the change | `cargo test -p type_table gallery` on `028574f4` | 20 passed |
| 2026-11-23 | Card `i` of `c` columns is at line `i ÷ c`, column `i mod c`; the arrows move as the table of keys says | `type_table` `card_grid::tests` (4): `a_card_is_placed_by_its_index_alone`, `the_columns_fill_the_width`, `the_cards_of_the_lines_meeting_a_span_are_found`, `the_arrows_move_in_row_order_and_by_lines` | Pass. They take the place of `masonry.rs`'s four |
| 2026-11-23 | Every card is one size and is placed before its cover is read; a cover landing moves none | `cards::tests` `every_card_is_one_size_whatever_its_cover`, `a_card_is_placed_before_its_cover_and_stays_there` (drawn bounds, one file's bytes held) | Pass |
| 2026-11-23 | Covers are read for the cards a frame lays out and no others, four at once; a read for a card scrolled away is dropped | `covers_are_read_for_the_cards_laid_out_and_no_others` (400 rows, 120 files over 64 KiB, the fake server's log of reads) | Pass: 36 cards laid out and read in the test's 1920 × 1080 window, none after them; four reads out at once; after a jump to the end no read for a card near the start was sent |
| 2026-11-23 | A frame lays out only the lines near the view | `a_frame_lays_out_only_the_lines_near_the_view`: 5,000 rows read, the cards built at the end, the middle and the start | Pass: the lines meeting the view and half a screen either side, under 200 cards of 5,000 |
| 2026-11-23 | A window widened at the cards' end still shows cards | `a_widened_window_at_the_end_still_draws_the_last_cards`: 150 rows, scrolled to the end, the window made twice as wide | Failed first: the frame chose its cards from the old offset, past the end, and drew none until the next event, as the gallery did. Pass with the offset held within the new range (`studio_ui::scrolled_within`, as the table's columns) |
| 2026-11-23 | An arrow key draws the grid once; a screen of covers landing draws it once | `a_key_draws_the_cards_once_and_covers_land_together` (↓ ↑ ← → End Home; every cover of the cards laid out read again) | Pass: one draw a key, no draw as the covers land, one draw `LANDING_DELAY` after |
| 2026-11-23 | ↵ and ⌘↵ open the page, ⌥↵ the cover's file, G the canvas the strip opens; a second click opens the page | `enter_opens_the_page_alt_enter_the_cover_and_g_the_canvas`, `a_click_selects_and_a_second_opens_the_page` (the actions dispatched) | Pass |
| 2026-11-23 | A saved `gallery` tab comes back as Cards | `a_saved_gallery_tab_comes_back_as_cards`: two tabs as 0.4.12 wrote them (`property`, `size`; one filtered and sorted, one of listed rows held at a commit), reopened through the workspace's opener for their kind | Pass: the same set, that file the cover, that size; saved again as kind `cards` with `cover`; a kind nothing registers opens nothing |
| 2026-11-23 | The old names load from a person's keymap | `studio` `keymaps` `a_keymap_naming_the_gallery_still_loads`: the sixteen `studio_gallery::…` names, `studio_graph::OpenAsGallery` and `ShowSetInGallery` with `property`, in a `Gallery` context | Pass: each bound to the action that took its place; the view keeps the `Gallery` key context beside `Cards`, so such bindings act (`the_arrows_move_in_row_order_and_by_lines`) |
| 2026-11-23 | Letters typed in a field are text | `keymaps` `the_cards_letter_keys_are_text_in_a_field`: G, X, F, =, − and Space are bound in `Cards && !editing` on every platform, the other thirteen keys in `Cards` | Pass. No field is in the tab until slice 5, which sets `editing` |
| 2026-11-23 | The card widths | Offscreen snapshots of the `notes` graph's Note cards at 1440 × 900, against `script/dev-server` on a port of its own (`omnigraph` 0.15.0) | 180, 240 and 340 drew 4, 3 and 2 columns between the sidebar and the inspector (a tab of 836 px), and 240 drew 4 with the inspector closed (1,176 px), where 180 and 340 give 6 and 3 by the grid's arithmetic, not drawn. 260 drew 3 in the 836 px tab, and 200 gives 3 there by the same arithmetic (Corrections) |
| 2026-11-23 | Cards look right at each size, with picks and with the inspector | The same snapshots, read: Small, Medium, Large; the cursor moved and two cards picked; the inspector closed and open again with Space; a branch `fix` with a note added, one retitled and one deleted | Cards of one height in straight lines; covers fitted and centered in boxes of one shape (pictures, an SVG, a PDF's page, text, bytes, an empty value, a null); titles on one line; nothing overlaps or is cut. On the branch the deleted note's card is first, dimmed, its title struck; the added and the changed card carry their bar and mark; the toolbar counts +1 ~1 −1 |
| 2026-11-23 | The workspace's checks | `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --no-fail-fast`; `script/check-layers.py`, `check-design-tokens.py`, `check-brand.py`, `test_release_files.py` | All pass. Tests: 1,742 passed, 0 failed, 9 ignored in the 54 harnessed test binaries (`type_table` 307, 1 ignored), and the 7 checks of `studio`'s `keymaps`, which has no harness; `PALETTE_ONLY` is still 33 names. `layers ok: 24 crates; 64 UI tools in their crate`, `design tokens ok`, `brand ok: 543 files`, 8 release-file tests OK |

`cards::tests` holds 25 tests: the gallery's 20 and 5 new (the frame's lines, the keys that open,
the clicks, the saved tab, the widened window). Of the gallery's 20, 15 were adapted (names and the
grid's in place of the masonry's) and 3 replaced by their uniform-grid equivalents:
`a_tile_is_as_tall_as_its_files_shape` by `every_card_is_one_size_whatever_its_cover`,
`a_placed_tile_keeps_its_place_when_later_shapes_arrive` by
`a_card_is_placed_before_its_cover_and_stays_there`, and
`the_tiles_shown_are_drawn_and_the_rows_below_measured` by
`covers_are_read_for_the_cards_laid_out_and_no_others`. Two were adapted in what they assert:
`the_arrows_move_within_and_across_columns` (now row order and lines; a size change reads no cover
twice, where it read nothing) and `previews_past_the_cap_are_let_go_farthest_first` (no first bytes
are held for rows measured ahead; a preview of a file that is no longer the cover goes first).

What slice 1 leaves as Gallery had it, for the slices named: Cards is offered only for a type
with a file (2); the toolbar's File chip stays, named Cover, and F goes round the type's files
without None (2, the Fields menu); the set is fixed at opening (5).

## Corrections

- 2026-11-23 · **The card widths are 180, 240 and 340**, not 200, 260 and 340. Columns grow to
  fill the tab, so a size is seen only as a column count: in a 1440 px window with the sidebar
  and the inspector open the tab is 836 px, where 200 and 260 both give three columns of 260
  (and 260 sits exactly on the edge of two).
- 2026-11-23 · **A later page that failed is retried from the status bar**, as in the table. The
  gallery said "Not read · Retry" in the band under its shortest column, and the bands are
  gone; States names only ⌘R, which reads the set again from its first page. With no row
  shown the tab's body still says the error and offers the same Retry.
- 2026-11-23 · **Covers are read for the cards a frame lays out**: the lines meeting the view,
  first, then half a screen below and above. Reading says "the cards meeting the view"; read
  only for those, a scroll of less than half a screen would show cards drawn with empty covers.
- 2026-11-23 · **A preview of at most 480 px is soft on a Large card.** 480 was twice the
  gallery's large tile (240 px). A Large card's cover is 330 to 400 px wide, so on a 2x display
  it is drawn from 480 px over as many as 800. Left at 480: at 680 a preview holds twice the
  bytes, and a screen of Small cards already holds about what `PREVIEWS_HELD` allows. A choice
  for the person who owns this plan.
- 2026-11-23 · **A card keeps one tooltip**: the node's name, which its title may cut, and under
  it what the cover's file is, why there is none, or that the branch changed it, which the
  gallery's meta line said. A field still has none.
- 2026-11-23 · **Home, End and the arrows show the cursor's card** even when the cursor does not
  move (Home on the first card, scrolled away): the gallery left the view where it was.
- 2026-11-23 · **The view keeps the key context `Gallery` beside `Cards`.** Names says the old
  action names are kept working by their aliases; a binding a person wrote in a `Gallery`
  context would still load and never act, since a context has no alias. The tab answers both.
- 2026-11-23 · **The tab scrolls to the card it opened at once the grid is measured**, in the
  first frame that knows its width. The gallery scrolled when the tile was placed, from a
  width it guessed.
