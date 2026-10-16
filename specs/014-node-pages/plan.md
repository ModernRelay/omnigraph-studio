# 014: Node pages, editing in place, and long text

Status: in progress; part 1 (the page and opening it) and markdown reading built · 2026-10-16

## Problem

A node has no page. "Opening" one from a table means one of three paths into the right dock's
inspector, which read different commits, and the inspector is read-only. Editing happens only in
table cells, ends five different ways, and cannot touch relationships: adding one means opening
the relationship's own table and typing the node you came from again, and changing one is a
delete and an insert in two dialogs. Long text is edited in a one-line cell editor, so a
description or a note written in markdown can be neither read nor written comfortably.

People want to open a node from a table in one move, read it as a page, edit its properties
and relationships where they see them, and expand a text property into a full markdown editor.

## Principles

- II: the inspector and the page read the commit of the surface they were opened from; an edit
  never mixes commits.
- III: every save keeps its three outcomes; a draft is kept until the outcome is known.
- IV: every editing surface names the branch it writes to.
- V: a page does not refresh under the person; "Newer data · Refresh" as everywhere.
- VI, VIII: one meaning per key across the table, tree, canvas and page; every control has a
  tooltip with its key.
- IX: a page reads its relationships in pages, not all at once; long text is bounded.
- X: designed against the 0.13 contract; what the engine should own is filed (Deferred).
- XI: the markdown editor is built from Zed's `editor` and `markdown` crates, not
  `markdown_preview`, which needs Zed's `workspace` and `project`.

## What exists today (checked)

Studio at `5b88186`; Zed `v1.23.2` (`~/.cargo/git/checkouts/zed-a70e2ad075855582/c019950`);
OmniGraph `v0.13.0`.

- **No node tab.** Tab kinds are blob, type table, canvas, storage, query, review, history,
  commit, agent and settings (`register_item_kind` in `type_table.rs:36,51`,
  `graph_canvas.rs:54`, `inspectors.rs:16`). The node inspector is `NodePeek`
  (`type_table/src/peek.rs`, 1,179 lines), placed in the right dock by
  `Workspace::set_inspector` (`studio_workspace.rs`, "Selection shows the inspector but never
  reopens a dock the user closed").
- **Three paths fill the inspector at different commits.** The table's own peek reads at the
  table's pin and reuses the table's relationship reads (`table.rs:2774-2850`); `PeekNode` and
  `OpenNode` read at the branch head (`type_table.rs:220-232`; `peek.rs:198-245`).
- **The inspector is read-only**: properties copy on click (`peek.rs:754-828`); a relationship
  group shows at most 8 neighbours, then "+N more" with no action (`peek.rs:23-24, 959-965`).
- **Table keys** (`assets/keymaps/default-macos.json:144-187`): ↵ edits (a non-editable cell
  peeks), Space toggles the peek, G shows the row on the canvas, ⌘N inserts, ⌘⌫ deletes. A
  single click on a value cell edits at once; there is no double-click handling in the table.
  A click on a relationship cell opens the other type filtered to this row
  (`table.rs:2746-2764`); its chips have no handler of their own (`table.rs:5253-5257`).
- **An edit ends five ways**: ↵ and Tab save now, ⌥↵ and clicking away keep a draft, ⌘S saves
  the drafts as one commit (`table.rs:2898-2916, 3066-3236`). Drafts live in each table's
  `cells` map, in memory, cleared on a branch or session switch (`table.rs:69-80, 548-571`).
  `docs/architecture.md` describes a `Draft` entity in `graph_store` that does not exist.
- **Values are typed as text** (`gq_language/src/input.rs:18-102`): enums by name, booleans as
  `true/yes/false/no`, dates `YYYY-MM-DD`, lists comma-separated (an element cannot hold a
  comma), empty text is null for a nullable property.
- **Relationships are written only from edge tables**: `InsertEdgeModal` with two empty
  endpoint searches (`dialogs.rs:394-895`), delete with ⌘⌫ bound to the table's pin
  (`dialogs.rs:1018-1021`). There is no edge update builder (`builder.rs:1669-1945`).
- **OmniGraph 0.13**: edge types have no update; a keyed edge changes by inserting its key
  again, an unkeyed one by delete and insert (`docs/user/mutations/index.md:90-92` at OmniGraph
  main). One mutation is constructive (insert, update) or destructive (delete), never both
  (`docs/dev/writes.md:376-379`, rule D2). Multi-statement mutations publish one commit.
  `@card(min..max)` restricts an edge type's count; `@description("…")` annotates types and
  properties; unknown annotations "are retained as metadata but have no built-in behavior"
  (`docs/user/schema/index.md:81-139`). The 0.13 compiler keeps a property's annotations
  (`PropDecl.annotations`, `crates/omnigraph-compiler/src/schema/ast.rs:74-78` at `v0.13.0`);
  Studio's `gq_language::Property` does not carry them yet (`gq_language/src/schema.rs:29-38`).
- **Zed**: `markdown` (GPL-3.0-or-later) renders markdown and is already Studio's renderer for
  agent replies (`agent_panel/src/panel.rs`, `Markdown::new`, `MarkdownElement::new`).
  `markdown_preview` pairs an editor with a preview but depends on Zed's `workspace` and
  `project` (`crates/markdown_preview/Cargo.toml`). Zed highlights markdown with
  `tree-sitter-md` (`Cargo.toml:898`).

## Design

Four parts, built in this order; each lands on its own.

### 1. One node view, two places; one way to open it

- **The node view** is one component in two densities: the **peek** in the right dock (as
  today, now editable) and the **node page**, a tab (item kind `node`, keyed by graph, branch,
  type and id, and by commit when it holds one, so opening it again activates it). A surface
  that holds a commit (a table or history opened at a commit) opens the peek and the page held
  at that commit (`PeekNode.at_commit`, `OpenNodePage.at_commit`); a surface that reads the
  head opens them at the head.
- **The page**, top to bottom: the type mark, the node's label and `#id`, the commit and
  branch, "Newer data · Refresh"; the actions Explore (G), Open in table, Delete; the
  properties, each a labelled field with its `@description` as help; long text and markdown at
  full height; Blobs with Open and Save; every relationship as a section with its count, read
  in pages of 50 (IX), with "Show more"; for an edge, its endpoints and properties. Back and
  Forward walk pages as any tab.
- **Keys, the same on every surface**:
  - Space peeks, and shows the right dock when it is hidden (an explicit request, unlike
    passive selection, which still never reopens a dock the person closed).
  - ↵ is the surface's own action, as today: edit in a table, open in the tree, expand on the
    canvas.
  - ⌘↵ opens the node page, from a table row, a tree node and a canvas node; the peek's header
    has an Open button (↗), and the footer's key hints read "⌘↵ Open". On a tree's
    relationship group, which has no page, ⌘↵ opens its table as before; "Open in table" for a
    node moves to ⌥↵ (the tree has no row menu).
  - In the peek and on the page a neighbour or an endpoint is a link: a click peeks it, ⌘-click
    opens its page. The peek's "+N more" reads "+N more · Open page" and opens the page, which
    lists a group's neighbours 50 at a time with "+N more" reading the next.
  - In a table, a click selects a cell; ↵ or a double click edits it (as a spreadsheet;
    typing to edit is deferred). A double click on the key cell opens the page.
  - In a relationship cell each chip is a link: a click peeks that node, ⌘-click opens its
    page; the count ("+N") opens the related table, as the cell does today.

### 2. Edit where you read: typed fields and one draft per branch

- **One draft per branch**, the `graph_store::Draft` that `docs/architecture.md` describes:
  entries keyed by type, id and property (or by edge), each holding the value it was read at
  (so saves stay value-bound) and the new value. Table cells, peek fields and page fields read
  and write the same draft, so an edit made in the page shows in the table and the reverse.
- **One rule for ending an edit**: ↵ saves that field now; Tab saves and moves to the next
  field or cell; leaving a changed field keeps it in the draft, dotted; ⌘S saves the whole
  draft as one commit; Esc puts the field back to what it held before this edit (a draft
  entry is not lost by Esc). A surface holding draft entries shows the pill it shows today,
  "N changes · Discard · Save as one commit"; the status bar shows only their count.
- **Typed editors**: an enum is a menu of its values; a boolean a switch; a date and a
  datetime a field with a calendar; a list editable chips (so an element may hold a comma); a
  nullable property a clear button (×) that sets null, so an empty string can also be stored;
  numbers right-aligned, with `@range` bounds shown and checked before saving; `@check` regexes
  checked before saving. The server stays the authority; these checks only save a round trip.
- Keys and ids stay read-only (the engine cannot change them).

### 3. Long text and markdown

- **Every String value is read as markdown.** Studio parses it as CommonMark (the parser of
  Zed's `markdown`) and renders what it finds. Plain text has no markup, so it renders as
  itself; a value with headings, lists, links, emphasis or code renders them. Keys, ids and
  enum values are not parsed. Soft line breaks render as line breaks, so plain text keeps its
  lines. What is stored is always the raw text: rendering never rewrites a value, and an edit
  saves exactly what was typed.
- **In a table** a value shows its first line, markup removed by the same parse; ↵ edits a
  one-line value in the cell; for a value with a line break or markup, ↵ opens the editor
  (below) and the cell shows the draft's first line with a dot.
- **On the page** a value renders (Zed's `markdown`, as agent replies render). A click on it or
  ↵ edits it in place: Zed's `Editor` in full mode, soft-wrapped, growing with its text,
  highlighted with `tree-sitter-md`; ⌘↵ or leaving it ends the edit into the draft. A segmented
  Edit · Preview · Split switch sits on the field's corner.
- **Expand** (⤢ on a text field, or ⌘⇧↵) opens the property as its own tab, the editor with a
  live preview beside it, as Zed's markdown preview to the side. It writes to the same draft
  entry; closing it keeps the entry.
- Bounds: a value over 256 KB opens read-only with a notice, and every editor saves through the
  ordinary value-bound update.

### 4. Relationships from the node

- Each relationship section on the page and in the peek has **+ Add**: a search over the other
  type (by label and id, as `InsertEdgeModal` searches today) with this node already the
  endpoint; picking a node adds a draft entry, and an edge type with properties asks for them
  in a small form. Each neighbour row has a remove button (×).
- An edge type with `@card(..1)` is a **field**, not a section: one picker that sets, replaces
  or clears the target. `@card(1..)` refuses removing the last edge before saving, with the
  reason.
- **Edge properties** edit like node properties. A keyed edge saves by inserting its key again
  (an upsert); an unkeyed edge saves as delete and insert, and says that its id changes.
- **In a table**, ↵ on a relationship cell opens the same picker as a popover: the current
  neighbours with ×, and a search to add.
- **Saving a draft that adds and removes**: rule D2 makes that two commits. Studio saves the
  constructive part, then the destructive one, except that a replacement on a `@card(..1)`
  edge deletes first; the outcome names each commit, and a failure of the second leaves the
  first in place, says so, and keeps the rest of the draft. "Save as one change" instead
  creates a branch from the pin, writes both parts there and merges it (spec 008), slower
  (seconds on a hosted server) but one commit on the branch.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| A node page tab and the peek, one component | Inspector only, zoomed to full size | A page takes the pane's back and forward, splits and restore like every document; zoom is a view of the dock, not a place |
| ⌘↵ opens the page everywhere; ↵ keeps each surface's own action | ↵ opens the page in tables | ↵ editing a cell is the spreadsheet convention people bring; changing it would break every table edit for one open |
| Click selects, ↵ or a double click edits | Click edits (today) | Click-to-edit makes selecting, copying and opening a chip impossible with the mouse, and causes accidental edits |
| One draft per branch, ↵ saves the field now | Every change saves on its own; drafts only | Quick single edits stay one keystroke; a mixed edit across the table and the page still publishes once; nothing is written by leaving a field |
| Every String renders as markdown, parsed from its text | `@format("markdown")` in the schema; a Studio setting per property | Plain text renders as itself, so parsing everything costs nothing for text without markup; 0.13 refuses adding an annotation to an existing property, so a schema marker would reach only properties declared with it from the start; a setting lives on one machine |
| Zed's `editor` and `markdown`, not `markdown_preview` | `markdown_preview` | It needs Zed's `workspace` and `project`, which Studio replaced |
| D2 saves as two ordered commits, with "Save as one change" through a branch | Refuse mixed drafts | People edit relationships as one thought; the engine's documented way to make mixed work one result is a branch |

## Deferred

- E25: one mutation that may hold inserts, updates and deletes (lift D2 for one write), so a
  relationship replacement is one commit without a branch.
- E26: edge update (T16), so an unkeyed edge's properties change without a new id.
- R2 (on OmniGraph main, not 0.13): unpublished commits could hold "Save as one change" for
  review before it reaches the branch.
- Typing into a table's selected cell needs the keymap to leave printable keys to the table;
  j, k, g and Space are table keys today, so typing to edit waits on moving those to modifiers
  or on edit-on-↵ alone.
- Node history on the page (the commits that touched this node) waits on a per-entity change
  query (E6).
- An opt-out for text that must not be read as markdown (identifiers with `*` or `_`, code):
  `@format("text")` on a property, which 0.13 accepts on a new property or type but refuses to
  add to an existing one (Validation log). E27 would let annotation-only changes apply.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-16 | Text values read as markdown | `studio_ui` `text` tests (plain values such as `foo_bar_baz` and `3 < 4` stay plain; headings, emphasis, lists, line breaks, links and code are markup; a cell's first line drops the markup); `type_table` `a_text_cell_shows_its_markdowns_first_line` (keys, enums and lists are not parsed); offscreen snapshots of the `notes` graph after giving two notes a markdown and a multi-line body (`dev/notes-seed.jsonl`) | The page renders the heading, bold, inline code, the list and the link, headings near the body's size (Zed's agent style); the peek keeps the plain body's three lines; capitals measure the same as the labels beside them once the size is set on the element around the markdown, since GPUI's text runs carry no size; the table cell reads "Keyset paging…" |
| 2026-10-16 | Part 1: a node's page and the ways to open it | `type_table` `a_nodes_page_reads_it_and_names_its_tab` (label as the tab title, one tab per node and commit, remembered and restored, a held page never newer); `studio_workspace` `an_asked_for_peek_shows_the_inspector_that_selection_leaves_hidden`; offscreen snapshots of Radia Knuth's page and of a peek | The page lists every property and opens every non-empty relationship group; the tab reads "Radia Knuth"; the peek has ↗ beside Explore and the footer "⌘↵ Open"; a peek asked for while the right dock was hidden shows it |
| 2026-10-16 | The flows above are today's | A read of `type_table`, `relationship_tree`, `graph_canvas`, `inspectors`, `review` and the keymaps, citing file and line | As listed under "What exists today" |
| 2026-10-16 | 0.13 keeps unknown annotations and the compiler exposes them per property | `docs/user/schema/index.md:139` at OmniGraph main; `git show v0.13.0:crates/omnigraph-compiler/src/schema/ast.rs` | "retained as metadata"; `PropDecl { name, prop_type, annotations }` |
| 2026-10-16 | D2 and edge update rules | `docs/dev/writes.md:376-379`, `docs/user/mutations/index.md:90-92` at OmniGraph main | One mutation constructive or destructive; edges have no update |
| 2026-10-16 | 0.13 keeps a property annotation it does not know, but only from the start | `omnigraph` 0.13.0 on a scratch graph: `init` with `body: String? @format("markdown")`, `schema show`; `schema plan` adding the annotation to an existing property, then a new property and a new type carrying it | `init` and `schema show` keep it verbatim; adding it to `Note.body`: "supported: no … changing annotations beyond @description/@instruction is not supported in schema migration v1"; a new property or type with it: "supported: yes" |
| 2026-10-16 | Plain text keeps its lines when rendered as markdown | `crates/markdown/src/markdown.rs:3292-3295` at Zed `v1.23.2` | `MarkdownStyle::soft_break_as_hard_break` renders a soft break as a line break |
| 2026-10-16 | `markdown_preview` cannot be used as it is | `crates/markdown_preview/Cargo.toml` at Zed `v1.23.2` | Depends on `workspace` and `project` |

## Corrections
