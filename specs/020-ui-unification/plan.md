# 020: UI/UX unification

Status: in progress · 2026-11-01

Follows [the audit](../../ui-ux-audit-report.md) and replaces its proposals where they differ (see
[What this changes](#what-this-changes)).

**In one line:** the sidebar lists things, a tab is one view of one thing, and everything around
the tab follows it.

## Problem

Studio's sidebar mixes two questions. Ontology, Canvas and Schema answer "how do I see it?", while
Person and Knows answer "what do I see?". They sit in one list, under one heading, as if they were
the same kind of row. The audit found the same fault across the app: picking something can open a
tab, fill a side panel, split a tab, change a tab or change the whole window, and nothing tells a
person which one will happen.

Studio copied Zed's frame but not the rules that make it work. This spec adopts those rules, and
says where a graph needs something different.

## Principles

| Article | How this touches it |
|---|---|
| I. The engine is the authority | A tree of a set reads its rows from the server; rows ticked are a list of nodes, never treated as true |
| II. One snapshot per view | Every table, page and walk reads one commit. Tree and Canvas on one walk share that commit |
| IV. Every write names its branch | Scope moves to the top bar, always visible |
| V. Nothing moves under you | Opening a view never changes or closes the tab it came from. Refreshing a shared walk shows in both of its tabs |
| VI. Every control explains itself | A view that can't open stays visible, disabled, with the reason |
| VII. Loudness matches cost | Opening a view is silent |
| VIII. Keyboard first | Every view has an action and a key; the palette lists only those available |
| IX. Work is bounded | A set in Tree is paged; Canvas keeps its cap and honest exit |
| XI. Zed lineage | The model is Zed's; the deviations are listed under Decisions |
| XII. Evidence over assumption | The prediction test gates the surface moves |

A new constitution article stating the rule follows acceptance. Its owner isn't named yet.

## What exists today (checked)

Studio, at `local/updated-studio` (`b3510a2`). This branch carries the cuts that aren't merged yet,
so check line numbers again before building on `main`.

**Sidebar and tabs**

- The sidebar's first panel, titled "Graph", lists Ontology, Canvas and Schema, then types, then
  relationships (`crates/type_table/src/types_list.rs:149-165`, `type_table.rs:135-139`).
- Ontology, Canvas and Schema are three separate tabs. Ontology is `OntologyPage`
  (`graph_canvas/src/ontology.rs`), Schema is `SchemaView` in `query_editor`, and Canvas is
  `GraphCanvas`. The comment at `types_list.rs:93` saying Ontology and Canvas are one tab is out of
  date.
- Tree is a left panel (`relationship_tree.rs:65`). History is a tab and a bottom panel
  (`history.rs:53`). Scope sits in the status bar (`studio_workspace.rs:3726-3760`).
- `Item` has `dedupe_key` and `opened_by`, but no notion of what a tab shows
  (`studio_workspace/src/item.rs:58-110`).
- `ds::toolbar` is called from the table, query editor, History and text editor, and not from
  Canvas, Ontology, Schema or the node page (by search). `ds::segmented` already exists
  (`ds.rs:2349`).

**Walks and commits**

- There is one Canvas tab per graph, keyed by graph id (`graph_canvas.rs:134`). There is one walk
  per session (`Exploration::for_session`, `exploration.rs:1331`), with one root, one selection
  (`ExploreState`, `exploration.rs:381-397`) and one pin (`exploration.rs:1278`). Tree and Canvas
  draw that one walk. Canvas also keeps its own `selected` (`graph_canvas.rs:594`).
- A walk starts from the branch head (`exploration.rs:1352`). `refresh` resets the whole walk's
  reads (`exploration.rs:1818-1829`), and a branch change starts it over at the same root
  (`exploration.rs:1386`).
- `OpenType` and `OpenNodePage` carry `at_commit`. `ShowNodeOnCanvas` and `ShowResultsOnCanvas` don't
  (`studio_actions.rs:168`, `:205`, `:277`, `:317`). Spec 014 already requires a surface holding a
  commit to open its peek and page held at that commit (`specs/014-node-pages/plan.md:86-87`).
- One head table exists per type, and one per type and commit (`table_key`, `type_table.rs:246`).
- A table holds several selected rows and a cursor (`table.rs:532`). `G` sends the cursor's node to
  the shared Canvas (`table.rs:6477`).

**Sets on Canvas**

- A results canvas is its own tab, gets a new sequence number each time it's shown, and holds only
  a title, nodes and links (`ResultsData`, `graph_canvas.rs:146-153`). It isn't restored after a
  restart (`graph_canvas.rs:419`) and doesn't walk (`Mode::Results`).
- Canvas stops at 2,000 nodes with "Too many nodes to draw" (`model.rs:14`, `graph_canvas.rs:2455`).

**Saved Canvas layouts**

- A saved Canvas holds hand-placed positions and no root (`pins_to_json`, `graph_canvas.rs:279`;
  `serialize`, `:419`). Positions are keyed by node or relationship pill, not by root (`pin_key`,
  `:251`).
- From reading the code (not run): restoring applies positions to a Canvas with no root, so they
  take effect only when those nodes are walked again (`graph_canvas.rs:73-77`, `:2028`).

**Inspector**

- A table builds its inspector as `PeekSubject::Node` or `PeekSubject::Edge`, with the row's values,
  the table's snapshot and whether it holds a commit (`show_peek`, `table.rs:4793-4833`;
  `peek.rs:109`).
- `set_inspector` replaces the one inspector view and drops the old one
  (`studio_workspace.rs:2142-2168`). Nothing remembers an inspector per tab.

**Panes and following**

- Each pane has one preview tab that the next preview replaces (`pane.rs:118-132`), and its own Back
  and Forward history, which skips closed tabs (`pane.rs:51-87`). `navigate` only steps the pane it's
  given (`studio_workspace.rs:1661`).
- `open_item` looks for a key in the active pane, then in the others, and activates it where it is
  (`find_key`, `studio_workspace.rs:1448`; `open_item_with`, `:1475-1495`). A split copies its tab
  with this turned off, so two panes can hold one key, but opening never makes a second copy.
- Showing a tab of another pane ends a pane's zoom (`activate_in`).
- `WorkspaceEvent` has only `SessionChanged` (`studio_workspace.rs:254`). The fork has no toolbar
  items, no status items and no active-item event (a search for `ToolbarItemView`,
  `StatusItemView` and `ActiveItemChanged` under `crates/` finds nothing).

**Keys**

- `cmd-alt-1` to `9`, `cmd-shift-1` to `9`, and their `ctrl` forms on Linux and Windows are unbound
  in all three keymaps (by search). Whether window managers on those platforms take them hasn't
  been checked.

Zed, at the pinned commit (`c4273271092b`). This comes from reading the source, not from running
Zed.

- A second view of the same file is a second tab. Markdown, SVG and table previews open from one
  Eye button in the toolbar, and Alt-click opens them to the side
  (`zed/src/zed/quick_action_bar/preview.rs:44-89`).
- Panels are tools. Project lists files, Git lists changes, and Outline shows the active file's
  structure (`zed/src/zed.rs:780-786`). None lists views. A panel is opened by an icon button
  (`workspace/src/dock.rs:1536`).
- The surroundings follow the active tab. Toolbar and status items receive `set_active_pane_item`
  (`workspace/src/toolbar.rs:25`, `status_bar.rs:44`), and a toolbar item hides itself when it
  doesn't apply (`ToolbarItemLocation::Hidden`). Outline listens for `ActiveItemChanged`
  (`outline_panel.rs:1097`), and Project reveals the active file (`project_panel.rs:738`).
- A view can follow too: a Markdown preview in Follow mode tracks the active editor, and its reuse
  looks only in the active pane (`markdown_preview_view.rs:92`, `:217-226`).
- Project and branch sit in the title bar (`title_bar/src/title_bar.rs:798`, `:930`).
- One same-tab toggle: unified or split diff, a toolbar control over the `diff_view_style` setting
  (`settings_content/src/editor.rs:281`). It only redraws.

Omnigraph: nothing here needs an engine change.

## Design

### The four questions

Every interaction answers four questions, and each answer has one home.

| Question | Examples | Lives in | Who chooses |
|---|---|---|---|
| **Which?** | Server, graph, branch | Top bar | The person, rarely |
| **What?** | Schema, type, node, query, branch | Left sidebar | The person, constantly |
| **How?** | Table, Tree, Canvas, Page, Map | The view strip, which opens a tab | The person |
| **Where?** | Region: center, right, bottom | Fixed by the rule below | Nobody |
| | Pane: this one or beside it | Same pane by default | The person |

A tab can also hold an earlier commit. That's part of the tab's read context, shown on the tab, and
not a change to the window's scope.

### The regions

| Region | Job | Never |
|---|---|---|
| Top | Which: server › graph › branch, and environment | Hidden, or only at the bottom |
| Left | What: lists things to pick | Lists views; shows a picked thing's content |
| Center | One view of one thing, in a tab | Holds lists that belong on the left |
| Right | Describes what's selected in the active tab | A destination; a place to pick from |
| Bottom | Reports what ran (Activity) | Holds primary content |

**Test for a sidebar row:** it must answer "what?". If it answers "how?", it's in the wrong place.

### What counts as the same thing

Opening a view can change more than the view, so every transition follows these definitions. They
are rules, not five new Rust types.

| Concept | Meaning | Rule |
|---|---|---|
| **Subject** | The schema, type, node, edge, query or branch being viewed | Sets the view strip. A view that needs one node narrows a type to that node, and the tab's title says so |
| **Read context** | The session and branch, and either "head" or a held commit | Always carried to the destination, and part of a tab's identity. A held view is read-only |
| **Set** | A type's rows under filters, or nodes listed | Fixed at opening: what a tree reads. Never follows its source |
| **Tab identity** | The key that decides whether an open tab is reused | Subject, view and read context; for a set, also what it reads |
| **Shared walk** | The expansion, selection and commit behind a Tree and a Canvas | One per session, root and read context. Refreshes together |

### What each view holds

Each view answers one question, and holds only what can answer it.

| View | Question | Holds |
|---|---|---|
| Table | Which of these? | A set |
| Tree | What are these connected to? | A set, or one node |
| Canvas | What's around this one? | One node |
| Page | What is this one? | One node |

If a view can't hold what the tab has, it narrows to the selected node. With nothing selected it's
disabled, with the reason. So the strip follows the subject: a set has `Table · Tree · Canvas` and a
node has `Page · Tree · Canvas`. Canvas never draws a set or the whole graph; Map is the overview.

### Objects and their views

The first view is what a pick opens.

| What | Listed in | Opens as | View strip |
|---|---|---|---|
| Schema | Data | Map | Map · Source |
| Node type, or a set of its rows | Data | Table | Table · Tree · Canvas |
| Edge type | Data | Table | None |
| Node | Nowhere; reached from a row, a link or ⌘P | Page | Page · Tree · Canvas |
| Query | Queries | Editor with results | None; results offer Tree, and Graph when they link |
| Branch | Branches | Review (`main`: History) | Review · History (`main`: Review disabled) |
| Commit | History | Its changes | None |

A graph has no tree of nodes, so nodes have no sidebar list. The audit's point stands: following a
relationship works like go-to-definition, moving between tabs without changing the rule.

### The sidebar

```
Data
  Schema
NODE TYPES
  Person     230k
  Company     12k
EDGE TYPES
  Knows       81k
```

- Every row is a thing. Canvas and Tree have no row.
- One click previews and a double click keeps, as types do today.
- A row is lit while the active tab shows that thing, in any view. Schema stays lit on Map and on
  Source.
- The panel's title becomes Data. Panels are switched from the status bar's icons, as before, and
  also by ⌘⇧1, ⌘⇧2, ⌘⇧3.

### The view strip

A view strip is a row of links to the current thing's views. It's a list of sibling tabs, not a mode
inside the tab.

```
Person ×   Tree: Person, Berlin ×
Table · [Tree] · Canvas
```

**Where:** the first thing in the tab's toolbar, drawn with `ds::segmented`. Canvas, Ontology,
Schema and the node page gain a `ds::toolbar` to hold it. A thing with one view shows no strip. Each
link's tooltip says what it will open: "Walk from Radia Knuth", "Show the 5 selected rows".

**Behavior:**

| Gesture | Result |
|---|---|
| Click | Activates that view's tab wherever it's open, and focus moves there; otherwise opens it in this pane as a preview |
| ⌥-click | Opens it in a pane to the right; an open tab is activated where it is |
| ⌘⌥1 to ⌘⌥3 (`ctrl-alt` on Linux and Windows) | The strip's first, second and third view (`OpenViewAt`) |
| ⌘⌥⇧1 to ⌘⌥⇧3 | The same, in a pane to the right |
| Back | This pane's previous tab |

**Rules:**

1. **The source tab is never replaced.** Opening a view from a preview tab first keeps that tab. If
   it didn't, a filtered Person table would vanish when its Canvas took the preview slot.
2. **Working in a preview tab keeps it.** That means a draft, a filter, an expanded row or a moved
   node. An untouched preview can still be replaced, since it holds no work and is cheap to reopen.
3. **A view that can't open is disabled, with the reason.** It's never hidden, so the strip doesn't
   shift.
4. **The read context always carries.** A view opened from a held tab is held at the same commit.
5. **Selection carries once, at opening.** After that, each tab keeps its own, except Tree and Canvas
   on one walk.
6. **Opening a view is silent.** No toast.

**What each view starts from:**

| From | View | Opens | If it can't |
|---|---|---|---|
| Table | Tree | The selected rows; with none, the rows under the filters, read a page at a time | Disabled with no rows: "No rows to show" |
| Table | Canvas | A walk from the row under the cursor; rows ticked don't change that | Disabled with no row: "Select a row to walk from it" |
| Tree of a set | Table | The type's table: its open tab as it stands, or a new one with these filters | Always available |
| Tree of a set | Canvas | A walk from the selected node, or from the node a selected relationship belongs to | Disabled with none: "Select a node to walk from it" |
| Tree of one node | Canvas | The same walk | Always available |
| Tree of one node, Canvas | Page | The selected node, else the root | Always available |
| Canvas | Tree | The same walk | Always available |
| Page | Tree, Canvas | This node | Always available |
| Map | Source | The selected type's definition, else the top of the file | Always available |
| Source | Map | The whole map | Always available |
| Query results | Tree | The result's nodes | No button without nodes |
| Query results | Graph | A picture of the nodes that share a row | No button without links |

Everywhere a node is focused, ↵ opens its Page and `G` opens its Canvas.

### Scenarios

| You want to | You do | What happens |
|---|---|---|
| See what's around someone | Filter Person, select Ada, click **Canvas** | A walk from Ada; Back returns to the filtered table |
| Check Canvas with nothing selected | Open a table, look at **Canvas** | Disabled: "Select a row to walk from it" |
| Compare connections across a set | Filter to Berlin, click **Tree** | "Tree: Person, Berlin": rows closed, paged with "Show more" |
| Work with a handful | Tick 5 rows, click **Tree** | Exactly those 5 |
| Tick 5 rows but click **Canvas** | The same | A walk from the row under the cursor, named in the tooltip |
| Go from the set to one person | In that Tree, select Grace, click **Canvas** | A walk from Grace; the Tree stays open |
| Go back to the set | In that Tree, click **Table** | The source table |
| See one walk both ways | Tree on Ada, click **Canvas** | The same walk: what's open stays open |
| Read everything about one record | ⌘↵ on a row, or ⌘P and a name | Its Page, with `Page · Tree · Canvas` |
| Re-centre on another node | "Canvas from here" | A new tab; Back returns |
| See the whole graph | Click **Schema** | The Map of types: Canvas never tries to draw all the data |
| Look at an old commit | Open a view from a held tab | Every view stays at that commit |

### Walks

Zed lets two tabs share one buffer: they have the same text and separate cursors. Studio does the
same with a walk.

**One walk per session, root and read context.** `Exploration::for_walk` looks up all three, in a
weak registry. Each tab holds its walk strongly, so closing every tab on a walk ends it.

| Tab | Key |
|---|---|
| Tree at head | `tree:<graph>:<type>:<id>` |
| Tree held | `tree:<graph>:<type>:<id>@<commit>` |
| Tree of a set | `tree-set:<graph>:<hash of type, commit, filters and listed nodes>` |
| Canvas at head | `canvas:<graph>:<type>:<id>` |
| Canvas held | `canvas:<graph>:<type>:<id>@<commit>` |

**Example:** Ada exists at C1 and is deleted at C2. From Ada's page held at C1, opening Canvas opens
a held Canvas at C1, even if a head Canvas for Ada is already open. The two never share data or edit
availability.

**Sharing on one walk:**

- Expanding a group in Tree shows on Canvas, and the other way round.
- Selection stays in step. Canvas's own `selected` has to be reconciled with `ExploreState.selection`.
- Both tabs read one commit, so neither promises a snapshot the other could break.

**Refresh and branch changes:**

| Walk | Refresh | Branch change |
|---|---|---|
| Head | Updates the whole walk. Both tabs offer it together and update together; a hidden tab catches up when shown | Starts over at the same root, as today |
| Held | Not offered; the commit is the point | Ignored; stays at its commit, read-only |

`Exploration` needs a way to start at a commit, since it starts at the head today. `ShowNodeOnCanvas`
gains `at_commit`, and a new `ShowNodeInTree` carries it too.

**Also:**

- Each tab saves its root and read context.
- A hidden tab doesn't rebuild on every answer. It uses Canvas's `owe_layout` and `read_owed`
  pattern, and the Tree tab gets a draw-count test (`docs/drawing.md`).
- Re-rooting opens the new root as a tab, in the preview slot, and Back returns to the old one in
  the same pane. This replaces the tab's breadcrumb.
- `OpenCanvas` with no thing behind it goes away. Canvas always starts from a node or a set, so no
  blank canvas exists.

### A set in Tree

A Tree of a set holds its rows under an anchor that's never drawn, so each row is a path of its own
and everything below a row works as in any walk. It's one walk with one commit, shared with no other
tab.

- **What it reads.** Rows under filters are read as `builder::node_page`, keyset-paged with the tree's
  page size, so the set is the whole filter and never "200 of 230,000 loaded". Rows ticked are listed
  in the action with their names, and read nothing.
- **What it costs closed.** The page alone: a closed row reads nothing until it opens.
- **Identity.** The key is a hash of the type, commit, filters and listed nodes.

| Then | Result |
|---|---|
| Click Tree again, with the same filter | Activates the open tab |
| Change the filter from Berlin to London, then click Tree | A new tab for London. Berlin stays as it was, unless it's an untouched preview |
| Change the filter and don't click Tree | The Berlin tab doesn't change. It's its own set, not a following view |

**The Table link.** Studio allows one head table per type, so a separate Berlin table can't exist
beside a London one. The link activates the open table as it stands, even if it now shows London; if
it has closed, a new one opens with the filters.

**Query results.** Tree shows the result's nodes as a listed set. **Graph** draws the nodes that share
a row, and exists only when some do: a one-column result links nothing, and a set with no links is
just a grid. It's the one picture of more than one node, kept because it shows how a result connects,
and named Graph because it isn't a Canvas: it doesn't walk, and clicking a node opens its Page.

### Saved Canvas layouts

A person may have placed nodes and pills by hand, and an upgrade mustn't lose that.

- On first restore, the old `canvas` tab's positions move into a **per-graph store** in the graph's
  saved state. The upgrade doesn't delete them.
- Each new Canvas on that graph copies matching positions when it opens, so a Canvas on Ada places
  that node where the person left it. These positions don't depend on a root, since they're keyed by node
  or pill.
- After that, each tab owns its copy and saves it with its root, so moving a node in one tab can't
  move it in another. Reset layout clears only that tab's copy.
- No legacy tab is restored. It would be the blank Canvas this spec removes, and it shows nothing
  today until those nodes are walked again.

### The inspector

The inspector describes what's selected in the active tab, and the workspace remembers one per tab.

- `set_inspector` records its view against the active tab, in place of replacing the single view. A
  finished view already holds its node or edge, values, commit and saving state, so no shared
  descriptor is needed and the layer rule holds.
- When the active tab changes, the workspace shows that tab's remembered view. With none, it reads
  "Nothing selected".
- A selection change inside a tab already goes through `set_inspector`, so it updates at once.
- A held selection stays read-only and shows its commit's values.
- Closing a tab drops its view, and a session change drops them all. A hidden inspector reads
  nothing, since it reads only the groups in view.

**Example:** select an edge in its table, switch to a node page, then return. The inspector shows
the edge's properties and endpoints again.

### What Item gains

```rust
/// What this tab shows. The sidebar lights its row from this.
fn thing(&self, cx: &App) -> Option<Thing>;

/// This thing's views, in strip order. Asked at every draw, so it builds no action.
fn views(&self, cx: &App) -> Vec<ViewLink>;

/// What opens one of them, with the tab's read context, built when it's asked for.
fn view_action(&self, kind: ViewKind, cx: &App) -> Option<Box<dyn Action>>;
```

- `Thing`, `ViewKind` and `ViewLink` live in `studio_actions`, so the layer rule holds.
- `ViewLink` holds a kind, whether it's the current view, an optional reason it's disabled, and what
  it will open in words.
- Tabs build the actions themselves, so feature crates still don't depend on each other. Existing
  actions cover most links: `OpenType`, `OpenNodePage`, `ShowNodeOnCanvas`, `ShowResultsOnCanvas`.
- `OpenViewAt { index, beside }` is what the strip and the keys dispatch.
- `OpenAsTable`, `OpenAsPage`, `OpenAsTree`, `OpenAsCanvas`, `OpenAsMap`, `OpenAsSource`,
  `OpenAsReview` and `OpenAsHistory` name the same thing for the palette. Each is registered only
  while the active tab offers it, so the palette stays accurate.
- `opened_by` goes away, since `thing` replaces it.
- `WorkspaceEvent` gains `ActiveItemChanged`. The sidebar, inspector and top bar follow it. The
  irrefutable `match event` sites, such as `types_list.rs:52`, need updating.

### What follows the active tab

| Surface | Follows by |
|---|---|
| Sidebar | Lights the row for `thing` |
| Inspector | Shows the tab's remembered view; with none, "Nothing selected" |
| View strip | Each tab draws its own from `views` |
| Status bar | The tab's facts, as today |

### Same tab or new tab

Use this test for any new "how" control:

- **New tab** if the view has its own state, selection or reads. Table, Tree, Canvas, Map and Source
  are all new tabs.
- **Toggle in the toolbar** if it only redraws the same state, like Zed's diff style.

### Words

| Use | For | Replaces |
|---|---|---|
| Schema | The graph's model | |
| Map | The schema drawn | Ontology |
| Source | The schema's text | Schema, as a tab name |
| Node type, edge type | Sidebar headings | Types, Relationships |
| Node, edge | Records | "Row", except a table's literal row |
| Table, Tree, Canvas, Page | Views | Explore (the word goes), Nodes |
| Data | The sidebar panel | Graph, as a panel name |
| Graph | The thing you open | Panel, menu and command names |
| Server | Where a graph lives | CLI target, environment |
| Environment | Production, staging and so on | |
| Inspector | Selection details only | |

Tab titles name the view and the thing: "Tree: Ada", "Canvas: Person, Berlin", "Map", "Source". A
table stays "Person". A held tab also names its commit. Renamed actions keep
`#[action(deprecated_aliases = [...])]` so saved keymaps still work.

### Saved tabs

A restored session mustn't lose tabs, lose work or show errors.

| Saved today | Becomes |
|---|---|
| `ontology` tab | Map tab, with its focus |
| `schema` tab | Source tab |
| `canvas` tab with pins and no root | No tab; its positions move to the per-graph store |
| `explore` or `history` panel | Falls back as any unknown panel id does |

## What this changes

| Earlier position | Now | Why |
|---|---|---|
| Audit: Walking is one tab, with Tree and Canvas as views | Tree and Canvas are tabs that share a walk | Zed's model; side by side comes free |
| Audit: Schema is one tab, with Map and Source as views | Map and Source are sibling tabs | Same reason |
| Plan PR 11: a Tree row beside Canvas | No row; Tree is reached from the strip | The sidebar lists things only |
| Plan PR 11: Tree bound to one session, one tab | Tree bound to a session, root and read context | One walk per root and commit |
| Plan PR 14: an `Item` hook so Back steps inside a tab | Removed | Views are tabs, so each pane's history covers Back |
| Plan PR 15: move `schema_view` into `graph_canvas` | Removed | Sibling tabs can stay in their crates |
| Plan PR 16: an `Item::inspector()` hook | The workspace remembers each tab's inspector | Peeks can be requested from other crates, and no new `Item` method is needed |
| Conversation: a switcher inside the tab | Dropped | See Decisions |
| First build: a table's rows drawn on Canvas | Rows go to Tree; Canvas takes one node | Rows have no relationships loaded, so Canvas drew a grid of unconnected chips |
| First build: labeled navigation rows in the sidebar | The status bar's icons, as before | The requester's call: the selector was not the problem |

The rest of the plan's rule stands: pick behavior, one History, double click means keep, the top
bar, environment marking and the message ladder.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Each view is a tab | A switcher inside one tab; one shared Canvas that the strip jumps to | The switcher needs several views' state in each tab and in-tab Back, and it blocks side by side. A shared Canvas loses its walk each time it's re-rooted |
| Share the walk, not selection in general | All selection synced across tabs; nothing shared | Follows Zed's buffer and cursors. Sharing everything makes one tab's click move another; sharing nothing makes Tree and Canvas disagree |
| A walk's identity includes its commit | Session and root only | A held view and a head view answer different questions |
| Refresh updates the whole walk, visibly | One pin per tab on a shared walk | One commit per walk is what makes sharing coherent, and it matches article II. Separate pins would double the reads and defeat the sharing |
| Canvas is always of something | A free-standing blank Canvas | Each tab keeps a clear "what". A collecting canvas can return as its own thing |
| Saved positions move to a per-graph store | Drop them; keep a legacy tab; convert them to a collection | Dropping loses work. A legacy tab is a blank Canvas. Positions are keyed by node, so a store keeps them with no new concept |
| Each tab copies positions at opening | All Canvas tabs share live positions | Moving a node in one tab mustn't move it in another (article V) |
| The workspace remembers each tab's inspector | A shared selection descriptor that rebuilds the inspector; an `Item::inspector()` hook | The view already holds values, commit and saving state, which a descriptor would have to carry through the layers |
| Keep reusing tabs across panes, and define Back per pane | Reuse only in the current pane (Zed's preview does); record the origin for a return | Studio assumes a key is unique in the window, so per-pane reuse would allow two Person tables. Recording origins adds state for little gain, since the source pane stays on screen. Deliberate deviation |
| A view holds only what can answer its question | Every view takes any subject | Canvas of unconnected rows answers nothing. Tree of a set does what a set needs |
| Tree of a set keeps its filter and pages | A copy of the loaded rows | The whole filter, at one commit, with nothing partial to explain |
| Rows ticked stay listed | Rewrite them as a filter | They're exactly what was picked, read or not |
| A set's identity is what it reads | A new tab each time; reuse and replace the contents | Reopening the same set shouldn't pile up tabs, and a changed filter mustn't overwrite earlier work |
| The Table link goes to the source table | A separate table for the set | Studio allows one head table per type |
| Graph only for results that link | Keep Canvas for every result; drop the picture | A result's links are the one picture a table can't give |
| An untouched preview tree can be replaced | Never replace a set's tree | It holds no work and reopens from the same set, and otherwise each filter change adds a tab |
| Keep the source tab when a view opens | Let the preview slot take it | The filter and drafts would vanish |
| Disabled with a reason, not hidden | Hide what doesn't apply, as Zed's toolbar does | Article VI, and the strip stays still. Zed hides whole items; this hides nothing inside a strip |
| No movable panels | Zed's `set_position` | A fixed region is what makes a result predictable. Deliberate deviation |
| Keep Tree in the center | Zed's Outline pattern, a side panel that follows | The person's direction. ⌥-click gives the same arrangement when wanted |
| Keys by strip position | One letter per view | `cmd-alt-t` is taken, and the digits are free on all three platforms. A digit's view varies by thing, but the strip shows it |

## Order of work

The cuts that are already done aren't repeated here.

| # | Change | Gate |
|---|---|---|
| 1 | Accept this spec; add the constitution article; move it to `specs/` | Owner named |
| 2 | Clickable mockup and prediction test | Thresholds below |
| 3 | `ActiveItemChanged`, `Item::thing`; per-tab inspector; sidebar and inspector follow | Test passed |
| 4 | Sidebar: things only, Data panel, new headings; Map and Source reachable from Schema | Item 3 |
| 5 | `Item::views`, `ds` view strip, `OpenAs*` actions and keys; keep-on-open rules; read context on the opening actions | Item 4 |
| 6 | One walk per session, root and read context; Tree becomes a tab; saved positions move to the store; Explore panel and the shared Canvas go | Item 5, and the tree baseline (plan PR 9) |
| 7 | Tree of a set, paged; Canvas of one node; Graph for results that link | Item 5 |
| 8 | One History; pick and double-click rules everywhere | Item 5 |
| 9 | Top bar | Real Linux and Windows builds checked first |
| 10 | Environment marking; message ladder; copy sweep | Item 9 |

Tree performance work (plan PRs 9, 10 and 12) continues as planned, with one thing to recheck: more
than one walk can now be alive, so the baseline should measure two walks open at once.

**Prediction test (the thresholds are a proposal, not a standard):** 5 people who don't use Studio
daily point at about 12 items, on today's build and on the mockup, and predict where each result
lands. The items include a view opened from a held tab, a view already open in another pane, and
Canvas after a changed filter.

- **Proceed** if the mockup beats today's build by at least 25 percentage points and scores at
  least 80% on surfaces not shown beforehand.
- **Revise and retest** if it doesn't.
- **Stop items 3 to 9** if people already score 80% or better on today's build, since that would
  prove the audit's diagnosis wrong.

## How this gets verified

- **Rule:** a test walks every sidebar row and asserts it names a thing, not a view.
- **Picks:** `#[gpui::test]` cases for click, double click, ⌥-click and Back on every list.
- **Source tab:** a filtered table with a draft, opened as a preview, survives opening Canvas, and
  Back returns to it unchanged.
- **Disabled views:** each row of the "starts from" table has a test for its reason.
- **Walks:** expanding in Tree shows on Canvas; two roots don't affect each other; closing both tabs
  ends the walk; a hidden tab rebuilds nothing.
- **Read context:**
  - The same root opened at C1 and C2 keeps its data and edit availability apart.
  - Navigating and inspecting from the C1 view stays at C1 after a delete at head and after a branch
    change.
  - Refresh updates both visible and hidden tabs on a head walk, and isn't offered on a held one.
- **Inspector:**
  - An edge selection returns across a tab switch with its properties and endpoints.
  - A held selection stays read-only with its commit's values.
  - A selection change inside a tab updates the inspector without another tab activation.
- **Pane navigation:** Back and Forward after opening in the same pane, reusing a tab in another pane
  and opening beside, including a source tab closed since. The source's filter, cursor and drafts
  survive whenever it stays open.
- **Sets in Tree:**
  - Rows open closed and read nothing below; the page is all that's read.
  - Ticked rows are exactly the set; another filter, list or commit is another tab; a tree comes back
    from what it saves.
  - Canvas waits for a selected node and walks from that one; Table goes back with the filters.
- **Bounds:** a set over the cap shows the honest exit.
- **Saved state:**
  - A fixture restores an old `workspaces/*.json` with each row of the saved-tabs table, with no
    error toast and other tabs intact.
  - Hand-placed node and pill positions survive the upgrade and another save and restart.
- **Keys:** the keymap test covers every `OpenAs*` action on all platforms.
- **Every change:** the checks in `AGENTS.md`, plus snapshots of the sidebar, each strip and a split.

## Deferred

| What | Why it waits |
|---|---|
| **Following views.** A Canvas beside a table that tracks its selection or filter, like Zed's Follow mode | Needs item 3 first; the copy at opening may be enough |
| **Saved canvases.** A canvas that collects nodes across types and appears in the sidebar as its own thing | No usage data shows the need. Today's shared Canvas is the nearest thing to it, and this spec removes it, so check this in the test |
| **⌘K.** What it lists, and whether it splits as Zed's does (`file_finder`, `command_palette`, `tab_switcher`, `outline`) | Set aside by the person for now. Its row test should match this spec's sidebar test |

No engine changes are needed, so nothing goes in `docs/deferred-engine-changes.md`.

## Open questions

1. **Who owns the constitution article?**
2. **Does a person lose anything without a blank Canvas?** Removing the sidebar row and the shared
   Canvas is the largest behavior change here. The test should include "show me the graph".
3. **Is "Map" the right word?** The audit proposed it, but "Ontology" is in use today. The card sort
   should decide.
4. **Do many tabs become a burden?** The preview slot should limit them. Watch this in the test.
5. **Do Linux and Windows window managers take `ctrl-alt-1` to `3`?** If so, the keys change there.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-01 | Zed has no in-tab view switcher; a second view is a tab | Read `quick_action_bar/preview.rs`, `markdown_preview_view.rs`, `tabular_data_preview.rs` at `c4273271092b` | Confirmed |
| 2026-11-01 | Zed's surroundings follow the active tab | Read `toolbar.rs`, `status_bar.rs`, `outline_panel.rs`, `project_panel.rs` | Confirmed |
| 2026-11-01 | Studio's fork has no follow mechanism | Searched `crates/` for `ToolbarItemView`, `StatusItemView`, `ActiveItemChanged`; read `WorkspaceEvent` | Confirmed |
| 2026-11-01 | A walk is one per session, with one root and one pin, and refreshes as a whole | Read `exploration.rs:381`, `:1278`, `:1331`, `:1818` | Confirmed |
| 2026-11-01 | The opening actions to Canvas carry no commit | Read `studio_actions.rs:205`, `:317` | Confirmed |
| 2026-11-01 | A saved Canvas holds positions, keyed by node or pill, and no root | Read `graph_canvas.rs:73-77`, `:251`, `:279`, `:419`, `:2028` | Confirmed by reading; a restored Canvas not run |
| 2026-11-01 | An inspector view carries its subject, values and commit | Read `table.rs:4793-4833`, `peek.rs:109`, `type_table.rs:254-287` | Confirmed by reading; per-tab memory not built |
| 2026-11-01 | Tab reuse crosses panes, and Back doesn't | Read `studio_workspace.rs:1475-1495`, `:1661`; `pane.rs:51-87` | Confirmed |
| 2026-11-01 | One head table exists per type | Read `type_table.rs:246` | Confirmed |
| 2026-11-01 | Digit keys are free | Searched all three keymaps | Confirmed; window managers not checked |
| 2026-11-01 | Tree and Canvas selection stay in step | Read `ExplorationEvent::SelectionChanged`, which both follow | Confirmed by reading; not exercised with both tabs open |
| 2026-11-01 | The rules hold in the code | `cargo test --workspace`: 1,173 passed, 0 failed, and the keymap test (`crates/studio/tests/keymaps.rs`, its own harness); clippy `-D warnings`, fmt, `cargo deny`, layers, brand, design tokens, release files all pass | New tests: each tab keeps its inspector; opening a view keeps its source; beside opens one pane and reuses it; a disabled view does not open; every sidebar row is a thing; a held walk stays at its commit; a hidden tree makes no rows; a captured set is known by what it holds; an environment goes by address; every destination has a key |
| 2026-11-01 | The frame and views draw as designed | Offscreen snapshots on `script/dev-server`: sidebar, table with its strip, Map, a set on Canvas, Tree beside a table, Branches with History | As designed. Clicks ran through the snapshot tool; the running app was not driven by hand |
| 2026-11-01 | An old session upgrades without loss | A real saved `workspaces/*.json` with a root-less `canvas` tab and `explore` and `history` panels, restored twice by the snapshot tool | The tab went, its positions stayed in `canvas_positions` through both restarts, and the panels fell back to `types` and `activity`, with the table tab intact |
| 2026-11-01 | Tree holds a set and Canvas one node, on real reads | `relationship_tree` `a_set_opens_closed_and_canvas_takes_the_selected_node`, `listed_nodes_are_the_set_and_a_set_is_known_by_what_it_holds`; `type_table` `tree_holds_the_tables_rows_and_canvas_the_row_under_the_cursor`; `graph_store` `a_set_is_paged_and_its_rows_open_on_their_own`; offscreen snapshots on `script/dev-server` | Person filtered to Founder opened as "Tree: Person, Founder", rows closed, a row opened to its relationships with counts; selecting Edsger Hamilton, Canvas opened a walk from that one node with both tabs kept |
| | The top bar is right on Linux and Windows | Real builds | **Not run.** Compiled by CI only; whether those platforms draw window controls in it is unknown |
| | The rule improves prediction | Prediction test | **Not run. No users have seen any of this** |
| | Two walks perform acceptably | Tree baseline | **Not run** |

## Corrections

The first four record changes to the draft before it was built.

- 2026-11-01 · A source review found five gaps in the first draft: read context through navigation
  and sharing, saved Canvas layouts, the inspector's selection, Back across panes, and the identity
  of captured sets. Each is now settled in Design.
- 2026-11-01 · The first draft dropped saved canvases silently, which would have lost hand-placed
  positions. They now move to a per-graph store.
- 2026-11-01 · The first draft's `selected_node` couldn't describe an edge or keep a held view's
  values. The workspace now remembers each tab's inspector.
- 2026-11-01 · The first draft promised Back to the source tab across panes, which pane history can't
  do. Back now means this pane's previous tab.
- 2026-11-01 · Built with the prediction test not run, at the requester's call. The spec's gate on
  items 3 to 9 was passed over, so its stop condition can still undo this work.
- 2026-11-01 · A table's rows drawn on Canvas were a grid of unconnected chips. The requester's rule
  replaced it: Tree holds sets, Canvas one node, and the captured-set code went. Several roots in a
  walk moved from Deferred to the core of Tree.
- 2026-11-01 · The sidebar's labeled navigation rows were reverted at the requester's call: the
  status bar's icons were not the problem. The ⌘⇧ keys stayed.
- 2026-11-01 · Item actions take `views` and `view_action`, not one `views` holding actions: the strip
  asks at every draw, and a set's action copies every loaded row.
- 2026-11-01 · A key is not strictly unique in the window, since a split copies its tab, so the
  decision against per-pane reuse rests on opening never making a second copy.

## What this branch leaves open

| What | State |
|---|---|
| Message ladder | One notice fixed: the CLI target notice is no longer a red toast that stays, and says it in plain words. `warning_bar` is not promoted to `ds`, the other toast callers are not classified, and the certificate notices are not rewritten |
| Scope in confirmations | Table dialogs, branch delete and the query confirm bar name server, graph and branch. The review's field edit names only the branch, and merge has no dialog to name it in |
| `write · <branch>` | Field editors say it; there is no standing chip in the status bar |
| Sidebar lighting | Data and Branches light from `thing`; Queries keeps its own |
| Tree of a set | Follows the table's filter but not its sort: rows come by `@id`. A cursor row past the first page is not selected until its page arrives |
| Cuts not in this branch | The agent panel (`refactor/remove-agent-panel`) and AI calls, so Conversations is still a sidebar row |
| Also still there | `Exploration::reroot` and `back_to` and the breadcrumb state, which no view calls now; panel moving; `window_style` |
