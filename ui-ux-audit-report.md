# Omnigraph Studio UI/UX audit

Omnigraph Studio is a native desktop app for browsing and editing graph data on Omnigraph servers. It is built on the crates of Zed, a code editor, and its window follows an editor's layout. This audit asks why the interface is hard to understand and what should change.

Audited at commit `5cd5d53` on 2026-10-30. Evidence is code, specs and offscreen renders on a sample graph. No users were tested.

## Summary

**Studio has no interaction primitive**, meaning no basic rule for how its parts relate. A code editor has one rule that explains almost everything: pick something on the left and it opens in the center as a tab. Studio copied the editor's frame (docks, panes, tabs, status bar) but not that rule. Picking something can open a tab, fill a side panel, split the current tab, change the current tab's content, or change the scope of the whole window, depending on where you are. Screen regions have no fixed meaning, so results can't be predicted, and each surface has to be learned separately.

Under that sit four related failures:

1. **No object model.** No document says what Studio's objects are. The central object, the node, appears in five forms under different names.
2. **No single vocabulary.** Explore, Inspector, History, Nodes and Graph each name more than one thing.
3. **Navigation lives in the status bar.** It is unlabeled, and its drawings are reused for other meanings.
4. **Scope and severity are poorly signaled.** Server, graph and branch appear only in small text at the bottom, and a healthy setup shows a persistent red error while a refused connection shows no toast.

The capabilities and the write-safety design are sound and should stay. The fix is still large, because Explore, Nodes, History, the inspector and Branches would all change behavior. Start by writing the rule and the object model, before any restyling or renaming.

## Terms used in this report

| Term | Meaning in Studio today |
|---|---|
| Type, node, edge | A type is a kind of record defined by the schema, such as Person. A node is one record, such as one person. An edge is a relationship between nodes |
| Tab | A document in the center of the window |
| Dock panel | A panel on the left, bottom or right edge of the window |
| Rail | The row of unlabeled icon buttons in the bottom status bar, which choose the dock panel shown |
| Graph panel | The default left panel. It lists Ontology, Canvas, Schema, types and relationships |
| Ontology | A tab that draws the schema's types as a map |
| Schema | A tab that shows the schema's source text |
| Canvas | A tab that draws nodes, walked outward from a starting node |
| Explore panel | A left panel showing a node's relationships as a tree |
| Inspector | The right panel, which shows one selected node |
| Page tab | A tab showing one node as a document |
| Nodes panel | A bottom panel showing one type's table |
| History | A branch's commits, shown as either a bottom panel or a tab |
| Palette | The searchable list of commands and items, opened with ⌘K |
| CLI target | A server saved in the `omnigraph` command-line tool, which Studio reads its server list from |
| Scope | The server, graph and branch that the window is reading and writing |
| Constitution | The project's binding design rules, in `.specify/memory/constitution.md` |

## The core finding: no primitive

### Test: what happens when you pick something?

| You pick | Result | Follows the editor's rule? | Source |
|---|---|---|---|
| Type in the Graph sidebar | Center tab: one click previews, a double click keeps | **Yes** | `crates/type_table/src/types_list.rs` |
| Saved query | Center tab | **Yes** | `crates/query_editor/src/queries_panel.rs:108` |
| Conversation | Center tab | **Yes** | `crates/agent_panel/src/sessions_panel.rs:597` |
| Commit in the History *panel* | Center tab on one click | **Yes** | `crates/history/src/view.rs:1812` |
| Commit in the History *tab* | Split inside the same tab; a double click opens a separate tab | No | `crates/history/src/view.rs:1812` |
| Node in the Explore tree | One click selects; a double click re-roots the tree and fills the inspector; the center does not change | No | `crates/relationship_tree/src/relationship_tree.rs:1069`, `:81` |
| Row in a table | Right inspector; ⌘↵ opens a page tab | No | `assets/keymaps/default-macos.json`, `crates/type_table/src/table.rs:4271` |
| Type on Ontology | Same tab changes to a focused view with its own Back button | No | `crates/graph_canvas/src/ontology.rs:475`, `:695`, `:1290` |
| Branch in Branches | Nothing opens; the whole window's reads and edits change | No | `crates/review/src/branches.rs:230` |
| Rail icon | Only the sidebar swaps; the center does not react | No | `crates/studio_workspace/src/studio_workspace.rs:2725-2840` |

Four cases follow the rule, so it can be extended rather than invented. The rest each behave differently. In the clearest example, a node appears in the left tree and in the right inspector while the center says "No tab open".

### One gesture, several meanings

A double click means four different things:

| Where | Double click does |
|---|---|
| Type in the sidebar | Keeps the preview tab |
| Key cell in a table | Opens the node's page tab |
| Node in the Explore tree | Re-roots the tree on that node |
| Commit in the History tab | Opens the commit's own tab |

A single click differs too: on a commit row, it opens a tab from the History panel but fills a split in the History tab.

### Regions have no meaning

The same kind of content appears in different regions:

| Content | Where it can appear |
|---|---|
| A type's table | Center tab; bottom Nodes panel |
| A node | Table row (center); tree entry (left); inspector (right); page tab (center); canvas card (center) |
| Commit list | Center tab; bottom panel |
| A list you pick from | Left; center; bottom |

The Ontology page puts the conflict side by side: "Open table" opens a center tab and "Browse rows" opens the same table in the bottom dock (`crates/graph_canvas/src/ontology.rs:1040`, `:1054`). A region does not tell you what is in it, and picking does not tell you where the result will land. This is the most likely reason the app feels arbitrary, though no user testing confirms it.

The visual design hides the problem. Tabs and headers look uniform, which promises a consistency the behavior does not deliver.

### Cause

The specs define the frame by its mechanics: "A panel is a view with an id, a title and an icon" (`specs/002-app-shell/plan.md:573`). A search of that spec, `docs/architecture.md` and the constitution for hierarchy, primitives or an object model finds no rule about what a region or a tab means to the user. The recorded reasons for the layout are lineage: "Status bar, after Zed's", and panel buttons "as Zed's `PanelButtons`" (`specs/002-app-shell/plan.md:584-588`, `:105-106`).

In an editor, the frame works because of what it contains: one primary object (the file), one list of them (the tree) and one place to see one (the tab). Studio has 15 kinds of tab, 9 dock panels and 11 kinds of modal, with no equivalent statement.

### Limits of this finding

- **The rule is partial, not absent.** Types, saved queries, conversations and commits in the History panel already follow it.
- **The frame itself is consistent.** Docks and tabs behave predictably as containers. What varies is what gets put in them.
- **A graph is not a folder tree.** A node has many neighbors, so a strict hierarchy cannot run down to nodes. Following a relationship can work like go-to-definition in an editor: it moves between tabs without changing the rule.
- **Editors are not perfectly uniform either.** Search and source control have their own panels, but their results still open in a center tab. This comparison draws on general knowledge of VS Code and Zed and on their docs. It is not a controlled comparison.

## Related failures

### No object model

Johnson and Henderson's test needs no users: rebuild the objects-and-actions table from the running product ([Johnson & Henderson](https://courses.dubberly.com/frameworks_2024_fall/Conceptual_ModelsInteractionsPaper.pdf)). In Studio it stalls at the node. Their rule applies directly: "If it isn't in the conceptual model, the system should not require users to be aware of it." The difference between a panel and a tab means nothing in the user's domain, yet predicting where History opens requires knowing it.

**Type versus instance** is the distinction a graph tool must make plain, and Studio blurs it. Ontology and Schema show types and Canvas shows data, yet all three are peer rows under "Graph", and Ontology and Canvas both draw cards joined by dashed lines (`crates/type_table/src/types_list.rs:137-155`). Hutchins, Hollan and Norman name this weakness: "distinguishing the depiction of an individual element from a representation of a set or class of elements" ([Hutchins et al. 1985](https://vis.csail.mit.edu/classes/6.859/readings/pdfs/Hutchins-DirectManipulationInterfaces.pdf)).

**Hidden overlap.** Canvas and the Explore tree render one shared `Exploration` (`docs/architecture.md:58`), but the UI presents them as unrelated places. Schema and Ontology both show the schema. The History panel and the History tab reported the same commit's changes differently, as "+100 + more" and "+1,231".

Comfort among experienced users is not evidence that the model is sound. As Hutchins et al. put it, practice "gives the system credit for the work the user has done".

### Vocabulary

| Word today | What it labels | Proposed single meaning |
|---|---|---|
| Explore | Left tree; a button and `G` key that open the Canvas | Walking from a node, with Tree and Canvas as views |
| Ontology, Schema, Map | Tab title, source text, page header | Schema, with Map and Source as views |
| Inspector | Right dock; "Storage Inspector", which opens a tab titled "Storage" | Selection details only; the other becomes "Server storage" |
| History | Bottom panel; tab | A branch's commits, in one place |
| Nodes | Bottom panel; palette scope | Palette scope only |
| row, node | "200 rows" and "200 nodes" for one set; edges are rows too | Node and edge; "row" only for a table's literal row |
| Types, Relationships, edge | Sidebar headings say Types and Relationships; the palette and Storage tab say "edge" | Node types, edge types |
| Contributes, contributes | One edge in two casings in one window | The schema's spelling everywhere |
| Graph | Panel, menu, command prefix, the thing you open | The thing you open |
| server, CLI target, environment | One thing under three names | Server, which frees "environment" for production or staging |
| Agent, conversation | Button; tab and panel | Conversation is the object; the agent is who answers |

The word clicked often isn't the destination's title: "Explore" lands on Canvas, "Agent" on "New conversation", "Graph: open inspector" on "Storage", and "Ontology" on "Map". ⌘N makes a query, a row or a conversation depending on focus.

### Navigation

- **Mixed list.** The rail reads Graph, Explore, Queries, Branches, Conversations: a container, a verb, then three object lists. A sound list has one organizing principle ([NN/g](https://www.nngroup.com/articles/ia-vs-navigation/)).
- **Hierarchy inverted.** A button at the bottom swaps the column above it.
- **Unlabeled, with no keys.** Every rail button is icon-only, the action that opens a panel (`ActivatePanel`) has no key binding, and a highlighted button's tooltip stops naming the panel and says "Hide sidebar" (`crates/studio_workspace/src/studio_workspace.rs:2774-2792`). Searching the palette for "explore" finds nothing. NN/g calls labels "particularly critical" for navigation ([NN/g](https://www.nngroup.com/articles/icon-usability/)). Apple and JetBrains lean toward icons, but no vendor offers evidence that icon rails aid comprehension.
- **Reused drawings.** `<>` means Queries, Schema, New query and the Query tab. The branch drawing means Branches and the History button on the empty screen. The crosshairs drawing means Canvas and the "Explore" button (`crates/studio_theme/src/icons.rs`).
- **Misleading location signal.** The highlighted icon follows the dock, not the tab, so the Graph button stays highlighted while an Agent tab is open.

Many routes to one place are not the problem: [Linear](https://linear.app/docs/search) and [TablePlus](https://tableplus.com/docs/gui-tools/open-anything) have many too. The problem is that one label leads to different places, and no destination has a canonical home.

### Scope and safety signals

Server, graph and branch appear only in the bottom bar, and the spec records no reason beyond "no title bar" (`specs/002-app-shell/plan.md:100-101`). A connected environment fits NN/g's definition of a mode, which needs "at least two visual indicators" ([NN/g](https://www.nngroup.com/articles/modes/)). Apple warns against critical information in a bottom bar ([Apple](https://developer.apple.com/design/human-interface-guidelines/windows)), and Microsoft says "If users must see it, don't put it in a status bar" ([Microsoft](https://learn.microsoft.com/en-us/windows/win32/uxguide/ctrl-status-bars)).

[VS Code](https://code.visualstudio.com/docs/sourcecontrol/overview) and [TablePlus](https://tableplus.com/docs/gui-tools/the-interface/toolbar) also keep context at the bottom, and no study compares placements. But TablePlus adds a tinted bar, an environment tag and Safe Mode, while [DBeaver](https://dbeaver.com/docs/dbeaver/Connection-Types/) changes behavior by connection type. Studio has none of that:

- **No environment class.** "Production" appears in Studio's Rust only as a test fixture (`crates/graph_store/src/session.rs:7732`), so a production server looks the same as any other.
- **Confirmations lack scope.** The delete dialog names the branch but not the server or graph, so two windows on different servers, both on `main`, can show the same confirmation.
- **Spec gap.** The `write · <branch>` chip is listed as not built (`specs/002-app-shell/plan.md:201`), against constitution IV.

### Severity is inverted

A CLI target sharing an address with another produces a red toast that stays until dismissed, offers no action and covers the table's toolbar. From reading the code, it returns on each start and each new window. Its text describes internals: "Each target is an identity of its own, and Studio keeps one connection per address, so it does not connect as it" (`crates/graph_store/src/cli_config.rs:617-640`). A refused connection, which stops all work, shows inline with no toast. The inline handling is right, and the toast is the mistake. It also conflicts with constitution VII, "Loudness matches cost".

### Lower-order issues

These are real but follow from the above:

- Type color is a hash into 8 tints, so Person, Company, Owns and WorksAt all come out brick (`crates/studio_theme/src/studio_theme.rs:117-128`).
- Person and Project share the letter P, and one node reads R on the canvas and P in the tree.
- The right side of the rail shifts about 23 px when the inspector button appears, against constitution V, "Nothing moves under you".
- `String?` and `Ag` read as developer notation, and `I32?` and `Int?` label the same column type.
- Search, New query, History and Agent on the empty screen omit the obvious first step of opening a type.

## Scorecard

The table ranks established usability principles from most to least structural. A failure counts as structural if fixing it changes what exists or what things mean. No source ranks these principles and no study compares structural with surface failures, so the order is a judgment made for this audit. Numbers such as "Nielsen #2" refer to [Nielsen's ten usability heuristics](https://www.nngroup.com/articles/ten-usability-heuristics/). "Shneiderman rule" refers to his Eight Golden Rules.

| # | Principle | Main sources | Studio |
|---|---|---|---|
| 1 | Conceptual model and primitives | [Johnson & Henderson](https://courses.dubberly.com/frameworks_2024_fall/Conceptual_ModelsInteractionsPaper.pdf); Norman | **Broken** |
| 2 | Task mapping, no implementation leakage | Johnson & Henderson; [Hutchins et al.](https://vis.csail.mit.edu/classes/6.859/readings/pdfs/Hutchins-DirectManipulationInterfaces.pdf); Nielsen #2 | **Broken** |
| 3 | One word, one meaning | Johnson & Henderson; Nielsen #4; Shneiderman rule 1 | **Broken** |
| 4 | Behavioral consistency, modes | [Raskin, secondhand](https://encyclopedia.pub/entry/34903); [NN/g](https://www.nngroup.com/articles/modes/) | **Broken** |
| 5 | State visibility | Nielsen #1; Hutchins et al. | **Broken** for place and environment; sound for write outcomes |
| 6 | Reversibility, error prevention | Nielsen #3, #5; Shneiderman rules 5, 6 | **Strong**, with one gap (scope in confirmations) |
| 7 | Overview to detail, relate | [Shneiderman 1996](https://webspace.science.uu.nl/~telea001/uploads/VACourse/Shneiderman96.pdf); Nielsen #6 | Partly broken |
| 8 | Novice-to-expert path | [Scarr et al. 2011](https://csse.canterbury.ac.nz/andrew.cockburn/papers/blur.pdf) | Mixed |
| 9 | Signifiers, discoverability | [Norman 2008](https://jnd.org/signifiers-not-affordances/); Nielsen #6 | Broken, mostly as a symptom |
| 10 | Grouping, visual hierarchy | [Wagemans et al. 2012](https://ruccs.sas.rutgers.edu/images/archive/personal-manish-singh/papers/Wagemans_etal_2012_I.pdf) | Partly broken |
| 11 | Error wording, icon labels | [NN/g errors](https://www.nngroup.com/articles/error-message-guidelines/); [NN/g icons](https://www.nngroup.com/articles/icon-usability/) | Mixed |
| 12 | Target size, pointer travel | [MacKenzie 2018](https://www.yorku.ca/mack/hhci2018.pdf) | Not measured; nothing suggests it matters here |

Popular versions of some principles overstate their sources, which affects the advice. Hick's law and Miller's number don't justify cutting item counts ([Liu et al. 2020](https://researchportal.ip-paris.fr/en/publications/how-relevant-is-hicks-law-for-hci/), abstract only; [Miller 1956](https://psychclassics.yorku.ca/Miller/)), and density alone doesn't violate Nielsen #8, which is about irrelevant information competing with relevant information.

## What already works

Leave these alone:

- Destructive actions list what they remove, offer Undo, and require typing the key when Undo cannot restore everything (`crates/type_table/src/dialogs.rs:896-897`).
- Writes have three honest outcomes (constitution III).
- Drafts persist, and the Activity log is a durable record.
- The palette exists and is broad.
- The type table reads well, and sidebar counts are useful.
- Connection failures are reported inline.

## Recommendations

### The rule (proposal, to be tested)

| Region | Job | Never |
|---|---|---|
| Top | The scope: server › graph › branch, in text | Hidden, or only at the bottom |
| Left | Picks: lists what's in scope | Shows the content of the thing picked |
| Center | The thing you picked: one kind of tab per kind of object | Holds navigation lists that belong on the left |
| Right | Describes what's selected in the center | A destination; a place to pick from |
| Bottom | Reports what ran | Holds primary content |

One click on a left item always opens it in a preview tab, and a double click always keeps it. Following a relationship opens the target in the center, and Back returns.

### Where each object opens

| Object | Today | Under the rule |
|---|---|---|
| Node type, edge type | Tab or Nodes panel | Tab only |
| Node | Five forms in three regions | Page tab; inspector only for what's selected |
| Walking from a node | Tree on the left, canvas in the center, under different names | One tab, with Tree and Canvas as views |
| Schema | Ontology tab and Schema tab | One tab, with Map and Source as views |
| Commit list | Tab or panel | One place, per branch |
| Commit | Split in a tab, or its own tab | Its own tab |
| Branch | Row switches the window | Switching belongs to the scope at the top; a row opens the branch |
| Query, conversation | Tab | Unchanged |
| Server storage | Tab titled "Storage", reached as "inspector" | Tab, renamed |
| Write log | Bottom panel | Unchanged |

This table is a starting point for a spec, not a final design. The left side might read **Data** (Schema, node types, edge types), **Queries**, **Branches**, **Conversations**. Whether a branch deserves its own page is an open question.

### Order of work

| Order | Change | Reach |
|---|---|---|
| 1 | Write the rule, the object model and the glossary as a spec; add a constitution article with a named owner | Docs; small |
| 2 | Test the rule with the prediction test below, using a prototype or clickable mockups | Small |
| 3 | Rename sweep across UI, palette, menus and tooltips | Copy; moderate |
| 4 | Move surfaces to fit the rule: one Schema, one Explore, one History, no Nodes panel; make picking behave the same everywhere | Views and routes; large |
| 5 | One labeled navigation, out of the status bar | Shell; large |
| 6 | Scope at the top; per-server environment tag kept in Studio's settings; strongest marking for production; `write · <branch>`; confirmations naming server, graph and branch | Shell and modals; moderate |
| 7 | Message ladder: inline at source, persistent bar for state with an action, dialog only when work must stop, toast only for events | Moderate |
| 8 | Bindings and palette commands for every destination, with shortcut hints in labels | Small |
| 9 | Stable, collision-free type marks; a fixed rail | Small |

No item needs a change to the Omnigraph engine. A per-server environment tag should name every environment and let an unset one read as unset, so an unconfigured production server never looks safe.

### What not to do

- **Don't restyle first.** Polish on an unpredictable structure only hides the problem.
- **Don't cut items to hit a number.** Figma reversed minimalist choices because they slowed all-day users ([Figma](https://www.figma.com/blog/our-approach-to-designing-ui3/)).
- **Don't build a beginner mode.** Scarr et al. found expert paths form when the expert equivalent appears during ordinary use ([Scarr et al. 2011](https://csse.canterbury.ac.nz/andrew.cockburn/papers/blur.pdf)).
- **Don't rely on renaming alone.** Better labels can't make behavior predictable.

## How to test this

Each test can prove this audit wrong:

- **Prediction test (the main one).** Point at an item and ask where its result will appear before clicking. Score across all surfaces, on today's build and on the prototype. Under the proposed rule, a person who has learned one surface should predict the others.
- **Reconstruction test.** Rebuild the objects-and-actions table from the running product. It should give one row per object.
- **Screenshot test.** A stranger shown one screenshot names the object, server, graph and branch.
- **Card sorts and first-click tests** for the labels ([NN/g](https://www.nngroup.com/articles/navigation-ia-tests/)).
- **Icon test.** Ask what each drawing means, shown alone.

If people already predict today's build well, this diagnosis is wrong.

## Limits

- No users were tested. This is an expert reading of code, specs and renders, and the ranking is reasoned, not measured.
- Clicks were traced in code, and most were not exercised in the running app. The renders are offscreen.
- Not rendered: reviewing a real branch, populated saved queries, the toasts from a real write, the glass window style, split panes, and connecting to a server on an incompatible version.
- Norman's book, Morville and Rosenfeld, Krug and Raskin were read through secondary sources. Some guidance is dated (2014 web guidance on icons, Windows 7 guidance on status bars), and evidence on top versus bottom placement is by analogy.
- Quotations were captured through a limited fetch tool, so check them against the linked pages before reprinting.
- The rule, the object table and the glossary are proposals.
