# 048: Cards, a view of a type's nodes

Status: accepted 2026-11-23 · slices 1 to 5 built 2026-11-23 (1: Gallery is Cards: the names
and their aliases, a saved `gallery` tab restored, the uniform grid, ↵, ⌥↵ and G. 2: a card
shows fields under its title, the format with its Fields menu, kept by tab and by type; Cards
for every node type and from a tree of a set. 3 and 4: the table's filter bar in a file of its
own, behind `FilterHost`. 5: that bar and a Sort chip in Cards, a change read at the commit the
tab shows); slices 6 and 7 not built · supersedes
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
  commit**, keeps the picks by id, as the table keeps its selection (Corrections), and puts
  the cursor on the first card.
- A date preset ("today", "this week") is kept as the preset and resolved at each read, so it
  does not freeze at the day the tab opened. A filter on a property the schema no longer has
  is dropped, and the bar says which (Corrections: the table drops it and says nothing).
- **Sort** is a chip of its own with the table's menu (`ds::sort_menu`).
- **A tab of selected rows** shows a lead pill "3 selected rows ×", the words its title has,
  and the Filters and Sort chips disabled: "Showing 3 selected rows. Clear the pill to filter
  Note". Clearing it shows the type's rows. "Picked" is the word for cards picked in the tab.
- **Filter in words** (slice 7) asks the same model with the same question. In Cards its
  answer sets filters and order; a grouping or columns in the answer are left out.
- "Newer data · Refresh" and ⌘R as in every data view.

### Selection and the keyboard

Key context `Cards`; every key is an action in `studio_cards` unless named. Every key that
acts on cards is bound in `Cards && !editing`, so while the filter's value field has the
keyboard the letters are text, the arrows, Home and End move in it, ⌘A selects it and ⌘C
copies it. Esc, ⌘R, ⌘F and ⌘⌥⌫ are bound in `Cards` and act in both.

| Key (macOS; Ctrl for ⌘ elsewhere) | Action | Does |
|---|---|---|
| ← → | `MoveLeft`, `MoveRight` | the card before or after in row order, across line ends |
| ↑ ↓ | `MoveUp`, `MoveDown` | the card a line up or down; ↓ onto a short last line goes to the last card |
| Home, End | `MoveToFirst`, `MoveToLast` | the first card; the last card read |
| Space | `studio::ToggleInspector` | the inspector shows the cursor's node whole: every property, its files, its relationships |
| ↵, ⌘↵ | `OpenPage` | the node's page |
| ↵ while a filter is written | `studio_table::ConfirmEdit`, in `Cards && editing` | applies the draft, and opens no page |
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
  Slice 5 lifted what else the two tabs share: the test of a filter against a schema
  (`filter_bar::filter_stands`, out of the table's schema load), what rows can be ordered by
  and how a Sort chip words it (`model::sort_choices`, `can_sort_by`, `sort_label`), and a
  set's words (`model::filters_summary`, `set_title`). The trait gained `bar_lead`, what a
  host draws before the pills, for the pill of a listed set (`ds::set_pill`). Its follow-up
  made the draft's keyboard the bar's to place, which is the one thing the table now does
  differently (Corrections): the bar puts the keyboard in the value field or, for a test that
  takes no value, with the host (`focus_filter_draft`), takes it back from a field no longer
  drawn as it draws (`keep_filter_keys`), and tells a host when its own keys wait
  (`filter_draft_takes_keys`, each host's `editing`).
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

Slices 1, 2 and 5 have their rows below. Slices 3 and 4 were built on branches of their own
and reached this log's branch by a merge; their rows are theirs to add. Slices 6 and 7 are
not built.

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

What slice 1 left as Gallery had it, for the slices named: Cards is offered only for a type
with a file (2, built); the toolbar's File chip stays, named Cover, and F goes round the type's
files without None (2, built: the Fields menu); the set is fixed at opening (5, built).

### Slice 2 (2026-11-23)

Built on `feat/cards-fields`, on `6f917dee`. The same machine and toolchain.

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-23 | A format round-trips in the settings in each of its forms, and the default is no entry | `studio_settings` `card_formats_round_trip_and_the_default_is_no_entry`: three formats written and read as JSON (`{"property": …}`, `"none"`, the first file left out; `fields: []` kept; Medium left out), read from text written by hand, a size that is none of the three, the default set, the last entry of a graph cleared, and `document::edit` writing, changing and clearing one in a file with a comment and a node name | Pass. `cards` raises no "is not a setting" problem; a wrong `size` is set aside alone; a graph with nothing left is removed, and one with a node name stays; the comment and the name are kept |
| 2026-11-23 | The default rule, and a format read against the schema | `type_table` `card_format::tests` (4): a type with two files, one with none, one whose first properties are the key and the name with a vector after them, one named by another property, one with only a key; a format naming a field and a cover the type lost, a file as a field, a field twice, twelve fields; show, hide, move, the ninth field, reset; the cover round the files and none | Pass. Three properties that are not the name, the key, a file or a vector; the first file, or no cover; what the schema lacks is left out and the format is not changed by being read |
| 2026-11-23 | Every card is one height whatever its values hold | `cards::tests` `every_card_is_one_height_with_fields_whatever_they_hold`: twelve rows, half with a text of 1,000 characters on two lines, a list of five, an enum's member, a date and a number, half with none; the drawn bounds at three fields, at eight and with no cover | Pass: every card `card_height(width, cover, fields)`; five fields more are five `CARD_LINE` more; with no cover no file is read on a scroll to the end. A row's values are made once (`Rc::ptr_eq` across two keys) |
| 2026-11-23 | The Fields menu shows, hides, moves and resets, within eight | `the_fields_menu_shows_hides_moves_and_resets`: the menu opened and its rows clicked (a property not shown, None, the file, a property past eight, Reset), the moves and Hide as its submenus call them | Pass: it stays open and lists the card as it is; the ninth is refused; reset keeps the size. `studio_ui` `menu::tests::a_persistent_menu_stays_open_after_a_choice_in_its_submenu` (a click and ↵ in a submenu) for the submenus themselves |
| 2026-11-23 | A new tab takes the type's format, a restored tab keeps its own, and a change is written as the type's | `a_new_tab_takes_the_types_format_and_a_restored_tab_keeps_its_own`: five tabs of one type against settings held in memory | Pass: nothing is written by opening; a format naming a lost field and cover shows the rest and stays as written; a change in a tab is the type's at once and the next new tab's; tabs open keep theirs; the default card removes the entry and the graph's |
| 2026-11-23 | The same, in the settings file | The offscreen snapshot tool on a data folder of its own: `=` in Person's cards, then the file read; then files written by hand for Person, Project and a scratch Ticket type | `settings.json` gained `graphs."environment@http://127.0.0.1:8790/knowledge".cards.Person.size = "large"`; new tabs drew the fields, order and size the files named |
| 2026-11-23 | Old tabs restore | `a_saved_gallery_tab_comes_back_as_cards`, extended: two gallery tabs (`property`, `size`) and one saved by slice 1 (`cover`, `size`), through the workspace's opener | Pass: that cover, that size, the default fields; saved again with `format` and none of the three old keys |
| 2026-11-23 | F goes round the files and none | `the_arrows_move_in_row_order_and_by_lines` (two files): F, F, F | scan, none (no card has the box, nothing is wanted read), the first file again, kept as the default |
| 2026-11-23 | Every node table and a tree of a set list Cards | `type_table` `tree_holds_the_tables_rows_and_canvas_the_row_under_the_cursor` (Person, no file); `relationship_tree` `a_set_opens_closed_and_canvas_takes_the_selected_node` and `listed_nodes_are_the_set_and_a_set_is_known_by_what_it_holds`; `cards::tests` `a_type_with_no_file_has_cards_without_a_cover` | Pass: Table · Tree · Canvas · Cards with the two tooltips; the tree's action carries the set's filters or its listed ids, the commit it read and the selected row; a type with no file draws cards, reads no file and binds no F |
| 2026-11-23 | A format change draws the cards once; an arrow key still does with fields | `a_format_change_draws_the_cards_once`: six keys with three fields shown, then a field shown, moved, hidden, another size, no cover, the default card, and two changes that change nothing | Pass: one draw each; none for nothing. In memory: see Corrections for the settings file |
| 2026-11-23 | Cards with fields look right | Offscreen snapshots at 1440 × 900, read: Note at S, M and L; Topic and Person (no file); Person with its six properties; Project at Small; a scratch type `Ticket` (an enum, a second enum, a date, a time, a number, a list, a long nullable text, a file) with the default card and with eight fields; the Fields menu on both; the dark theme twice; a branch with a company added, changed and removed | Names in one column and values on one edge; long text cut with an ellipsis; chips, dashes, "Today", "3 h ago", "a, b, c +2"; cards of one height; no-cover cards on a faint fill. The first Topic picture showed titles with nothing around them, which the fill answers (Corrections) |
| 2026-11-23 | The workspace's checks | The same seven commands | All pass. Tests: 1,754 passed, 0 failed, 9 ignored in the 54 harnessed binaries (`type_table` 316 and 1 ignored, `studio_settings` 50, `studio_ui` 65, `relationship_tree` 17), and the 7 checks of `keymaps`. `layers ok: 24 crates; 64 UI tools in their crate`, `design tokens ok`, `brand ok: 544 files`, 8 release-file tests OK |

`cards::tests` holds 30 tests: slice 1's 25, of which `the_columns_fill_the_width`,
`the_arrows_move_in_row_order_and_by_lines` and `a_saved_gallery_tab_comes_back_as_cards` now
assert more (a card's height with fields and without a cover; F through none; the format a
restored tab has and saves), and 5 new. The table's strip test changed what it asserts: a table
of a type with no file listed three views and now lists four.

### Slice 5 (2026-11-23)

Built on `feat/cards-filters`, on the merge `769c0e55`, which holds slices 3 and 4. The same
machine and toolchain. The fake server of `type_table`'s tests answers a read at the commit it
names, or at the head another writer moved, and keeps every request.

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-23 | What the two tabs now share was moved, not changed | `cargo test -p type_table` after `filter_stands`, the sort choices and a set's words were lifted out of `table.rs`, before anything else changed | 322 passed, 1 ignored, the count reported for the merge; `a_schema_changed_elsewhere_waits_for_refresh`, which drops the table's filters under two schema changes, unedited |
| 2026-11-23 | A filter reads the set again at the commit the tab shows, not at the head | `cards::tests` `a_filter_reads_again_at_the_pin_and_not_at_the_head`: 12 notes read at `c1`; another writer commits `c2` and the head watch finds it; a filter written with ⌘F, typed, ↵; a second; ⌘⌥⌫; ⌘R | Pass: one read, asked with `snapshot: c1`, answered at `c1`, from the first page; the pin holds and Refresh is still offered; the cursor on the first card; the two picks kept by id and the range's start gone; the title "Cards: Note, 1" and the tab's key follow. ⌘⌥⌫ shows every note at `c1` with no request (the session holds what it read there); ⌘R reads `c2`. With the read made at the head instead, the test fails |
| 2026-11-23 | Sort orders the cards and reads at the pin | `sort_orders_the_cards_and_reads_at_the_pin`: the head moved; the chip's menu opened and `title` taken with the keyboard; descending; the same order again and one the type cannot give; the chip's × | Pass: `order { $n.title asc … }` then `desc`, each one read at `c1`, the cursor on the first card, the title unchanged; nothing read for the same order or a file's; × puts the ids' order back with no request |
| 2026-11-23 | A date preset is resolved at each read and saved as itself | `a_date_preset_is_resolved_at_each_read_and_saved_as_itself`: "due is today" written with ⌘F and ↵; ⌘R. `type_table` `a_date_preset_judges_edited_and_inserted_rows_by_its_days`, extended: what the table hands Cards. `relationship_tree.rs` read: its Cards action passes the set's filters as it holds them | Pass: each page is sent today's two days as `f0` and `f0_end`; the tab holds and saves `{"property": "due", "op": "Today", "value": null}`; the table's action carries the same, where it carried the days. With `filters` read in place of `resolved_filters()` the page is sent `null` and the test fails |
| 2026-11-23 | A filter the schema lost is dropped and said | `a_filter_the_schema_lost_is_dropped_and_said`: a saved tab with a filter on `topic`, which Note never had, one on `title`, and an order by `title`; another writer drops `title`; ⌘R; a new filter | Pass: read once, without it; "The schema no longer has topic: its filter was left out" in the bar, and not counted as the person's change; after the refresh the filter and the order on `title` are gone, the notice names `title`, the Filter menu no longer offers it; the next filter takes the notice away |
| 2026-11-23 | A tab of selected rows says so until its pill is cleared | `a_tab_of_selected_rows_says_so_until_its_pill_is_cleared` (named `…picked_rows…` until the follow-up): three listed notes; ⌘F, ⌘⌥⌫, a click on each chip, an order set in code; a click on the pill's ×; a click on each chip | Pass: the pill is drawn; no draft, no menu, no order, no page read, and the status bar offers no filter key; cleared, the 12 notes are read at `c1` under no filter, the title is "Cards: Note", the tab saves no ids, and the chips open the draft and the menu |
| 2026-11-23 | Keys typed in the value field are the text's | `keys_typed_in_the_value_field_are_the_texts` (the editor's own ←, Home, ⌘A, ⌘C and Esc bound as Zed's keymap binds them): ← and Home move the caret, → ↓ ↑ End move no cursor, ⌘A and ⌘C select and copy the text, G X F = − Space are typed; ↵; ↵ again. `studio` `keymaps` `the_cards_letter_keys_are_text_in_a_field`, rewritten: every Cards binding of the three keymaps by where it acts | Pass: the cursor, the picks and the size never change; ↵ applies the filter and opens no page; the next ↵ opens the first card's page. The keymaps bind 6 keys that type and 11 that do not (the arrows, Home, End, ↵, ⌘↵, ⌥↵, ⌘A, ⌘C) on the cards alone, Esc, ⌘R, ⌘F and ⌘⌥⌫ in both, and ↵ as `ConfirmEdit` in the field. With `editing` not set, the test fails |
| 2026-11-23 | Esc closes the draft first and the picks second | `esc_closes_the_draft_first_and_clears_the_picks_second`; then ⌘⌥⌫ over a filter and an open draft | Pass: the draft, then the picks, no read for either; after ⌘⌥⌫ the cards have the keyboard and → moves the cursor |
| 2026-11-23 | A draft with no value field takes ↵ and Esc | `a_draft_with_no_value_field_takes_enter_and_esc`: "due is empty" set as the menus leave it; → ↓ X G; Esc; again; ↵ | Pass: the cards have the keyboard, not the field that is not drawn; their keys wait; Esc closes the draft and leaves the picks; ↵ adds the filter and opens no page (Corrections) |
| 2026-11-23 | Typing a filter draws the cards no times | `typing_a_filter_draws_the_cards_no_times`: 300 tasks, no cover; four letters, ← and Home; ↓ ↓ ↑ through an enum's two members; ↵ with the page and the count each held | Pass: no draw of `CardsGrid` for the letters or the moves; one as the filter is applied, without its rows, one as the page lands, one as the count lands, and no more |
| 2026-11-23 | A preview tab the person changed is kept | `a_preview_tab_the_person_changed_is_kept`, through the workspace: a preview replaced by the next; then an order, a filter and a size, each in a new preview | Pass: untouched, one tab; after each change the tab is no longer the preview and the next preview opens beside it |
| 2026-11-23 | A saved tab comes back with its filters, order and format | `a_saved_tab_comes_back_with_its_filters_order_and_format`: two filters (one "due is this week"), an order, Large, a field hidden; saved; reopened in another window through the workspace's opener | Pass: the same filters, the preset a preset, the order, the format and the title "Cards: Task, Task, due is this week"; its first page sent the filter's text, the week's days and `$n.points desc`; not counted as changed |
| 2026-11-23 | A filter keeps what was read at its commit | `a_filter_keeps_the_covers_and_the_branchs_marks`: 12 notes with files on a branch that added one and removed one; a filter | Pass: no file is read again; the removed row first, the added one marked, the counts as they were; the cursor on the first card the branch still has; past the cap every preview of a row the filter no longer shows is let go and those shown stay |
| 2026-11-23 | The bar and the chips in the app | Offscreen snapshots of the `notes` graph at 1440 × 900 against `script/dev-server` on a port of its own (`omnigraph` 0.15.0), driven with keys and clicks through Studio's keymap, read: "written is in the last 30 days" from the menus and ↵; a filter through `filedUnder Topic`, "is related to", "de" typed, the offer picked with ↵; Sort by `written`, its menu open; three listed notes with the pointer on Filters; a saved filter on a property Note lacks; the dark theme twice | 7 of 13 notes under the preset, its pill and "Filters 1 ×"; "Design" offered under the field and, picked, the 7 notes filed under it, the tab "Cards: Note, design"; "Sort written ↑ ×" with the menu's two ticks; "3 picked rows ×", both chips dimmed, the tooltip as written; the notice after the kept pill. The toolbar reads view strip, type and count, Fields, Sort, Filters, S M L, Refresh |
| 2026-11-23 | The same draft in the table | The same tool on the Note table: `written`, "is in the last 30 days", ↵, Esc | The draft stays open and 13 rows show: no key reaches the table while its draft has no value field. As before this slice; not changed here (Corrections) |
| 2026-11-23 | The workspace's checks | The same seven commands | All pass. Tests: 1,772 passed, 0 failed, 9 ignored in the 54 harnessed binaries (`type_table` 334 and 1 ignored), and the 7 checks of `keymaps`; `PALETTE_ONLY` is still 33 names. `layers ok: 24 crates; 64 UI tools in their crate`, `design tokens ok`, `brand ok: 546 files`, 8 release-file tests OK |

`cards::tests` holds 42 tests: slice 2's 30, unedited but for the keys their `init` binds, now
as the keymaps bind them, and 12 new. The table's preset test asserts one thing more.

### Slice 5, follow-up (2026-11-23)

Built on `feat/cards-filters`, on `10949470`. The same machine and toolchain.

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-23 | In the table, a draft with no value field takes ↵ and Esc, and the table's keys wait meanwhile | `table::tests`, three added, keys bound as the keymaps bind them, a Person table with a Date column: `a_draft_with_no_value_field_takes_enter` (⌘F on the date opens "born is today"; ↓, J and Tab; ↵; ↓ ↓), `a_draft_with_no_value_field_takes_esc`, `clear_all_with_a_draft_open_gives_the_table_its_keys` (a filter, a draft with "Ad" typed, the bar's clear) | Pass: the table has the keyboard, not the field; the cursor does not move and ↵ opens no cell; ↵ adds `born` `Today`, Esc closes with nothing added; after the clear the table has the keyboard and ↓ moves. With the keyboard left where it was before (the field focused and never taken back), all three fail |
| 2026-11-23 | The same in a second host | `filter_bar::tests`, three added against the probe, which now has a key context and a key of its own (J): `a_second_hosts_draft_with_no_value_field_takes_enter` ("has any" on its relationship), `…_takes_esc` (and "is empty" taken from the test menu over a draft with a field), `clearing_a_second_hosts_filters_gives_it_the_keys_back` | Pass, and fail the same way with the old focus. The first failed once more before it passed: a host that already has the keyboard is not drawn again by a focus change when the draft is applied, so its key context kept `editing`; the bar now notifies as it applies |
| 2026-11-23 | Nothing else about the table changed | `git diff` of `table.rs`: two lines outside its tests (`editing` and Tab ask `filter_draft_takes_keys`), and tests only added. `cargo test -p type_table` | 340 passed, 1 ignored: the 334 of slice 5, none edited in `table.rs`, and 6 new |
| 2026-11-23 | Cards calls the same code | `cards.rs` lost its own guard, its `editing` rule and its ↵ for a draft with no field; `cards::tests` | The 42 pass. `a_draft_with_no_value_field_takes_enter_and_esc` spells out the check a removed helper made; nothing it asserts changed |
| 2026-11-23 | One word for the rows a tab holds | `a_tab_of_selected_rows_says_so_until_its_pill_is_cleared`, renamed, its tooltip's words changed | Pass: "Showing 3 selected rows. Clear the pill to filter Note" |
| 2026-11-23 | The table in the app, before and after | The offscreen snapshot tool on the Note table of the `notes` graph, through Studio's keymap: `written`, "is today"; ↓ and J; Esc; the draft again; ↵. Then Cards: a preset and ↵; three listed notes | Before (slice 5's row): the draft stays open after ↵ and Esc. After: the draft "written · is today" open with 13 rows; ↓ and J put no cursor on a row; Esc closes it; ↵ adds the pill "written is today" and the table reads "0 of 13". Cards as in slice 5, its pill "3 selected rows ×" under the title "Cards: Note, 3 selected rows" |
| 2026-11-23 | The workspace's checks | The same seven commands | All pass. Tests: 1,778 passed, 0 failed, 9 ignored in the 54 harnessed binaries (`type_table` 340 and 1 ignored), and the 7 checks of `keymaps`. `layers ok: 24 crates; 64 UI tools in their crate`, `design tokens ok`, `brand ok: 546 files`, 8 release-file tests OK |

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

Slice 2:

- 2026-11-23 · **A menu that stays open closed on a choice in its submenu.** The Fields menu's
  Move up, Move down and Hide are in submenus, and Zed's menu, which Studio's is a copy of,
  dismisses the whole menu when a submenu's row is chosen. The copy (`studio_ui/src/menu.rs`)
  now keeps a menu built with `build_persistent` open, builds it again and gives it the
  keyboard back. No other menu of Studio's is both persistent and has a submenu.
- 2026-11-23 · **The fields' names take one width**: the widest name's, and a third of the card
  at most, so every value of a card starts on one edge. "A card" says each name is cut to a
  third; alone, that leaves the values ragged.
- 2026-11-23 · **A card with no cover is drawn on the fill the cover's box has.** Without it a
  type with no file (Topic) drew as bare titles in a grid. Its check is at the title's end and
  its change bar along the card's own top, since both sat on the cover.
- 2026-11-23 · **Reset, and the chip's ×, leave the size.** They undo the fields and the cover;
  the size has its own control beside them, and a chip named Fields that offered a reset
  because the cards were Large would mislead. The size is still part of the format kept.
- 2026-11-23 · **What can be a field** is every property but a file and a vector: the key and
  the name can be chosen, and only the default rule leaves them out.
- 2026-11-23 · **What equals the default is kept as the default.** Fields put back as the
  default rule has them are kept as no list, and the type's first file chosen as the cover as
  no cover, so a card that looks like the default one is the default one: the × goes and the
  settings entry with it.
- 2026-11-23 · **Writing the settings file draws every window again when the write lands**
  (`studio_settings::update`, for every setting). In the app a format change, `=`, `−` and F
  among them, so draws the cards once at once and once more some milliseconds later. The
  guard holds for the change itself, with settings kept in memory.
- 2026-11-23 · **The `knowledge` graph has no type with an enum, a date and a nullable text,
  nor one with eight properties**: Person has six that can be fields, Project four. The
  pictures of eight fields are of a type made for them in a scratch graph on the slice's own
  development server; nothing of it is in the repository.
- 2026-11-23 · **An action that names a cover** (`ShowSetInCards.cover`, as the gallery's
  keymap entries could) still shows that file, for the tab it opens, over the type's format,
  and writes nothing. Studio itself no longer sets it.

Slice 5:

- 2026-11-23 · **The picks stay by id through a filter or order change**, shown or not, as the
  table's selection does (`TypeTable::clear_rows`): a card filtered out and back is still
  picked, and ⌘C copies it meanwhile. The card a ⇧-click would range from is a place in the
  old order, and goes. Filters and order said "keeps the picks that are still shown".
- 2026-11-23 · **The table says nothing when it drops a filter the schema lost**; its schema
  load only drops it. Cards drops it by the same test and says which in the bar, after the
  pills ("The schema no longer has topic: its filter was left out"), until the filters next
  change or the tab is refreshed. An order by a property that is gone is dropped unsaid, as
  the table drops it.
- 2026-11-23 · **A draft whose test takes no value takes no key in the table.** "is today",
  the test a date's draft starts on, "is empty" and "has any" draw no value field, the field
  keeps the keyboard, and a key sent to a field that is not drawn reaches nothing: ↵ and Esc
  do nothing, and the ✓ whose tooltip says ↵ is the only way on. Slice 5 answered it in Cards
  alone; the follow-up moved the answer into the bar, for both (below).
- 2026-11-23 · **A draft closed under its field left the field focused and not drawn**: ⌘⌥⌫,
  "Clear all" and the chip's × close the draft without moving the keyboard. Slice 5 gave it
  back to the cards; the follow-up does it in the bar, for both (below).
- 2026-11-23 · **Esc with one of the draft's menus open closes the menu and the draft**, in
  Cards as in the table: the menu passes the key on.
- 2026-11-23 · **"Reads again at the pin" is a read asked of the session at that commit.** The
  session sends a read at the commit it takes for the head as a read of the branch and reads
  again, pinned, when the answer is from another commit (spec 015); and what it read at a
  commit it does not ask for twice, so clearing the filters of a tab opened without any shows
  its rows with no request. Neither shows rows of another commit.
- 2026-11-23 · **Covers read at the commit are kept through a filter or order change**, as the
  table keeps its Blob cells: a card that is still shown does not read its file again. A
  preview of a row no longer shown is let go first once the previews pass their cap.
- 2026-11-23 · **Where a branch's removed rows stand first, the cursor goes to the first card
  the branch still has**, which is where an opening leaves it when the diff lands after the
  first page.
- 2026-11-23 · **A tab of listed rows keeps the title it opened with** ("Cards: Note, 3
  selected rows", the table's words). A filter on a related node words the title by the
  node's id ("Note, design") where the pill shows its name ("Design"): the table's words for
  a set, which the tree's tab has too.
- 2026-11-23 · **The toolbar has a Refresh button**, last, as the table's has.

Slice 5, follow-up:

- 2026-11-23 · **The draft's keyboard is the bar's to place, in the table as in Cards.** Two
  views with one bar must not differ in its keys. A draft whose test takes no value has the
  host's keyboard, and a draft closed by any path gives it back: the bar focuses the host
  where it has the window, and takes the keyboard from a field no longer drawn as the bar is
  drawn, which covers the paths that have none ("Clear all", the chip's ×, a host that
  replaces its filters). A host's `editing` comes from the bar's one answer. So the table
  changed in one way: with such a draft open ↵ adds it and Esc closes it, where neither did
  anything, and ↓, J, Tab and ↵ on a cell wait as they do while a value field has the
  keyboard. A click on a row does not end that wait; ↵ or Esc does. How it is built said the
  key contexts stay as they are.
- 2026-11-23 · **The rows a tab holds are "selected" in its pill and its tooltip too**, the
  word its title has from the table. The plan said "3 picked rows"; "picked" stays the word
  for cards picked in the tab.
