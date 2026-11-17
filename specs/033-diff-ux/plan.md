# 033: One way to show a change

Status: built 2026-11-17 · `change_cards` holds the vocabulary; the review, a commit's tab,
History, the Commits panel, the inspector, the type table on a branch and the query editor's
write result use it

## Problem

Studio shows a change in seven places, and each grew its own way of saying it. The same change
reads "1 changed in Person" in the review's feed and "~1" beside it in the split; a changed row
is blue in the review and green in the type table; an old value is grey and struck through in
the review and red-marked in the table; a commit's sections are "Changes to existing entities"
and "Added" where the review's are "Changes to what main has" and "Added on the branch"; a
schema change is a red error with a "Try again" that can never work, and a blank cell in
History. A reviewer of a 3,000-change branch learns from the header that "1,650 entities and
1,500 relationships changed", not that 1,500 people were added. A commit's tab selects nothing,
so the inspector stays empty and there is no way from a change to its node. This plan starts
from who looks at a change and why, and gives every screen one vocabulary built from that.

## Who looks at a change, and why

Five jobs, in the order of how often Studio is opened for them.

| Job | Sees first | Compares | How big | Next |
|---|---|---|---|---|
| **J1. Review a branch before merging it**, an agent's above all: an agent writes on `agent/<stamp>` (spec 011, E20) and a person decides whether it lands | Whether it is safe to merge: how much changed, of which types, whether anything is removed, whether it merges cleanly or conflicts | Each value on the branch against the target's at the fork | One property to an agent's bulk load of thousands of nodes and their relationships | Merge; fix in place (Edit, Revert); mark reviewed; open the node; ask for a summary |
| **J2. Find out what one commit did**, or who changed a value and when | Who, when, on which branch, and what it changed, by type | The commit against its parent | One value; a load of thousands; a schema change, whose data changes Omnigraph does not list | Open the node as the commit left it; open a table or a query at the commit; follow to the parent; start a branch there |
| **J3. Check that a write did what was meant**, from the query editor's result, a toast's "View commit", or a table on a branch | Exactly the values written, and anything written that was not meant (a load that cleared a property, a relationship removed with its node) | Before against after, value by value | One cell to a load | Revert or edit; open the node; write again |
| **J4. Understand a conflict** | Which nodes stop the merge, and what each branch did to them | The branch's change against the target's (Studio holds only the branch's side; E53) | One node to many | Fix on the branch with ordinary edits, then merge again |
| **J5. Watch what lands**, across branches, live (spec 026) | Who wrote, on which branch, and how much | Nothing yet: a glance, then J2 | Any | Open the commit (J2) |

What the jobs share decides most of the design: every one starts with *how much, of what* (a
count by type and kind), then reads values side by side, then goes to the node. None starts
with a sentence.

## Principles

- V (nothing moves under you): a run that starts folded is folded when the change is first
  shown, never after; a review read again keeps the person's folds.
- VI (every control explains itself): the kind's mark and every count say their words in a
  tooltip; the open button names ⌘↵.
- VIII (keyboard first): opening a change's node is `studio_changes::OpenChanged` (⌘↵ in the
  review and a commit), unfolding a commit's change `studio_changes::ToggleChange` (Space), and
  a commit's J/K are `menu::SelectNext`/`SelectPrevious`, as History's.
- IX (work is bounded): a long run starts folded; a listed commit is still counted from one
  page of 100 changes (E44).
- X: a schema change's own diff and a conflict's two sides need engine changes (E52, E53); the
  design says what it cannot show rather than working around them.
- XIII: a click selects and the inspector follows, in a commit's tab as in the review.

## What exists today (checked)

Surveyed 2026-11-17 on `main` at `b2b47fa` against the dev server (Omnigraph `0.15.1`) with a
fixture of its own (Validation log): a schema change and three bios on main; `small-edit` (a
retitle with a list and a long-text edit, a new person with three relationships, a removed
relationship, a moved city, a removed project with its six relationships); `bulk-import` (1,500
new people, each where they work, 150 retitled, a load that also cleared their bios); `empty`;
`conflict` (a title and a city changed on both sides). Each screen was snapshotted offscreen
(`crates/studio/examples/snapshot.rs`, 1440×900); the before and after pairs are on the pull
request.

| Place | Code | Before |
|---|---|---|
| Review, feed | `crates/review/src/view.rs` `feed_section_row`, `feed_entity_row`; `change_cards::run_row`, `entity_row` | A run is a sentence, "1 changed in Person", led by a status dot of the run's first kind (blue even when the run also adds); entity rows have no kind mark; a removed name is struck through |
| Review, split | `split_group_row`, `split_item_row` | A run is the type and "~1"; an entity is a coloured dot and its name |
| Review, header | `render_header` | "4 entities and 10 relationships changed in 5 commits by act-dev": no type, no kind |
| Review, empty and states | `Render` | "Nothing to review"; "Reading changes…" |
| Review, conflicts | `render_conflicts`, merge strip | The strip says "2 conflicts stop a merge" (and "1 conflict stop"); the cards they are on say nothing |
| A commit's tab | `crates/history/src/commit_view.rs` | Sections "Changes to existing entities" and "Added"; "4 changes" in the facts; no selection, so the inspector reads "Nothing selected" and no way to a node; a schema change is the error state with "Try again" (Ctrl-Alt-N), which gets the same refusal |
| History's rows and hover card | `crates/history/src/view.rs` row, `commit_card` | "~1"; "+100 + more"; "first commit"; a schema change's cell blank |
| Inspector, a commit | `crates/history/src/commit_note.rs` | "Changes ~1", "+ more"; no types |
| Commits panel | `crates/history/src/commits_panel.rs` | "~1", "+100 +" (the partial mark a second plus); the label "09V5S…" cuts the commit id to fit the actor |
| Type table on a branch | `crates/type_table/src/table.rs` `change_color`, `render_diff_counts` | A changed row's bar green, as an added row's; "+1 ~1" with "~" grey |
| Query editor, a write's result | `crates/query_editor/src/view.rs`, `results.rs` | "Committed · 1 nodes, 0 edges"; the commit id is text, not a way to its changes |
| Property rows (shared) | `crates/studio_ui/src/changes.rs` | A short change: old grey and struck, new plain; a list: gains and losses on green and red marks; a text: lines on tints, each word an element of its own, so a changed word sent the rest of its line to a new line, past the tint |
| Dead code | `studio_ui::ChangeCard`, `change_group_header` | A second card design, used nowhere |

The inconsistencies, by kind:

1. **Colour.** Changed is `Color::Modified` (blue) in the review, History and the panel, green
   in the table's bar, grey in the table's count.
2. **Marks.** The kind is a dot (feed run, split item), a symbol (relationship lines, counts),
   or nothing (feed entity rows, commit rows): colour alone in two places.
3. **Old and new values.** Grey struck text (review), red mark struck beside a green mark
   (table cells, list elements), tinted lines (text).
4. **Counts.** A sentence (feed), symbols (split, History, panel, table), a total ("4
   changes"), entities and relationships ("4 entities and 10 relationships"); a partial count
   "+ more" (History, inspector) or "+" (panel).
5. **Grouping words.** "Changes to what main has" / "Added on the branch" against "Changes to
   existing entities" / "Added".
6. **States.** "Nothing to review" / "No entity changes"; "Reading changes…" twice; a schema
   change an error in a commit and blank in History.
7. **Affordances.** The review selects, unfolds, edits and reverts; a commit's tab unfolds only,
   selects nothing and opens nothing; a write's commit opens nothing.
8. **Layout bugs.** A changed word broke its line; the panel cut the id it exists to show.

## Design

### The vocabulary (`change_cards`, on `studio_ui`)

- **Kind.** Added `+` in `Color::Created`, changed `~` in `Color::Modified`, removed `−` in
  `Color::Deleted`, and `·` muted for a node shown only for its relationships. Always the symbol
  in its colour, in a 12 px slot, its word in the tooltip (`change_cards::kind_mark`): the
  feed's and the split's entity rows, a commit's rows, relationship lines. No dot.
- **Counts.** "+3 ~2 −1", each part in its kind's colour, zeros left out
  (`studio_ui::change_counts`): runs, the summary line, History, the panel, the inspector, the
  table's toolbar. A listed commit's counts are `change_cards::commit_counts` over
  `CommitCounts`: the counts, with a muted "…" when only the first 100 changes were counted;
  "no changes"; "first commit"; "schema change"; "cleaned up"; "not comparable" (another
  comparison the server refuses, its reason in the tooltip). Each says its words in its tooltip
  ("100 added, counted from its first 100 changes; it has more"). One function reads a refused
  read of a commit's changes (`history::counts::lasting`), for the commit's tab and every list
  alike, so they cannot say two things of one commit.
- **Summary line** (`change_cards::summary_line`). Under the heading of a review and a commit,
  and in the inspector's commit: each type with a change, its mark, name and counts, node types
  before relationship types, wrapping. Its tooltip says the counts in words. The review's facts
  keep who made the commits ("5 commits by act-dev") and the fork; a commit's keep who, when,
  where and its parents. "so far" follows a commit's line while more changes are to read.
- **Runs.** A run's row is led by its type: chevron, the type's mark and name, its counts, then
  what else it needs (a run of nodes standing only for their relationships, which has no counts,
  "2 nodes with changed relationships"), and when, in the review (`change_cards::run_row`). The
  same row in the feed and in a commit.
- **Sections.** "Existing" (changes to what the target at the fork, or the parent, held) and
  "New" (`change_cards::section_words`), in the review and in a commit.
- **Values.** A short changed value: what the change took away struck through on the removed
  colour's mark, beside what it brought on the added colour's (`studio_ui::changed_value`), as a
  list's elements and the table's cells already read. A long text: its lines, a changed pair
  "−" and "+" on faint tints with the words each side alone has on its mark, now drawn as one
  text that wraps as prose does. An added or removed node's values are plain: its mark says
  what happened to all of them.
- **Long changes.** A run of more than 50 entities (`change_cards::FOLD_PAST`) starts folded
  to its row when the change holds other runs, the first time it is shown; one run alone shows
  open. The summary line and the folded rows read as the change's shape; J and K skip folded
  runs, as they did.
- **States.** Reading: `reading_state` ("Reading the changes…", and in a slow review "The
  server is still working out the changes. On a large graph this can take a few minutes.").
  Nothing: `no_changes_state` ("No changes", and against what: "small-edit has no commits of its
  own.", "This commit changed no node or relationship against its parent."). A schema change:
  `schema_change_state` ("Schema change", the type it names, that Omnigraph does not list a
  commit's data changes across one, and that the schema as it is now is in Schema), with no
  retry. The first commit: `first_commit_state`. History cleaned up, and another comparison
  refused ("Not comparable", with the server's reason), the same way: `lasting_state` maps every
  lasting refusal to its state, none with a retry. A failure stays `ds::error_state` with its
  recovery.

### Affordances

- **Select, then open.** In a commit's tab, as in the review: the first change is selected and
  unfolded as it opens, a click selects an entity and unfolds it (a second folds it), J/K move (`menu::SelectNext`/`SelectPrevious`), Space unfolds
  (`studio_changes::ToggleChange`), and the inspector shows the selected node held at the commit,
  or at its parent for a node the commit removed.
- **Open the node** where the change left it, read-only: ⌘↵ (`studio_changes::OpenChanged`,
  Ctrl+Enter on Linux and Windows) in the review and a commit, and the arrow button at the end
  of an entity's row, shown while the row is hovered or selected (`change_cards::open_button`).
  The review opens at the reviewed head, or at the fork for a removed node; a commit at itself,
  or at its parent. A relationship of its own has no page; its ends do.
- **Follow to the commit.** A write's result in the query editor links its commit to the
  commit's tab (a click previews, a second keeps it, and the query's tab is kept, never
  replaced by the preview), so a write is checked where it was made.
- **Conflicts on their cards.** A card whose node a merge would stop on says "Conflicts with
  main" in `Color::Conflict` in the feed, and carries the conflict mark in the split's list,
  beside the conflicts list the strip opens. The conflicts are held with their index by type and
  entity (`Conflicts`, made where a dry run or a merge answers), so a drawn card finds its own
  in one lookup. `studio_changes::ToggleChange` unfolds a change in the review too.

### Drawing

The review's and a commit's lists of changes are views of their own, drawn cached
(`ReviewRows`, `CommitRows`) and again when their tab notifies, as History's rows are; both tabs
answer `Item::caches_its_parts`. A key typed in the review's search no longer draws the list;
the search landing draws it once.

### Where it lives

`change_cards` (layer 4) holds the vocabulary and now depends on `studio_actions` (layer 0) for
`OpenChanged`; `studio_ui` (layer 2) keeps the value rows and counts it draws with, and its
`SidebarRow` gains a `detail` that is cut before its label (the Commits panel's actor beside the
whole id). Feature crates still share nothing directly.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The kind is a symbol in its colour everywhere, with its word in the tooltip | Keep the feed's dot; colour alone | J1 and J5 are scans: a mark has to read at a glance in a column, and for a reader who cannot tell red from green. The symbol was already the counts' and the relationship lines' |
| Changed is `Color::Modified` everywhere, the table included; old and new values are red and green marks | Changed in green in the table (spec 004's "green and red only"); old values grey in the review | Two colours meant one thing: green said "added" on one screen and "changed" on the next. At the value level, red and green say "taken away" and "brought", which is the same in every place, and J3 reads the written value first, which a grey struck value hid |
| Counts as "+3 ~2 −1"; the feed's sentence goes | "1 changed in Person" in the feed and symbols elsewhere; sentences everywhere | The same count had two forms on one screen. The symbols fit History's column and the panel; the tooltip has the words (spec 002, "Layout, not sentences") |
| A summary line by type and kind under the heading | The entities and relationships sentence; stat cards (rejected in spec 008) | J1 and J2 start with *how much of what*: "Person +1,500 ~150 · WorksAt +1,500" says what an agent's load did before any card; "1,650 entities" did not |
| A run is led by its type, the same in the feed, the split and a commit | Type last, after a sentence | The summary line, the split's list and the type table are all led by type; a reader matches a run to the line above it by its mark |
| "Existing" and "New" in both | Keep each view's own words | J2 reads a commit the way J1 reads a branch: what existed and what is new. The heading already names the branches |
| Runs past 50 entities start folded when there are others | Fold every run; fold none; a cap on rows | A load of thousands buried the 150 retitles, the changes that need reading (J1). One run alone stays open, since its row would say no more than the summary line |
| A listed commit's counts are one element with its states, the partial mark "…" | "+ more", "+"; a blank for a schema change | "+" also means added. A blank cell says nothing of why: J5's reader cannot tell a schema change from a read still out |
| A schema change is a state, not an error, with no retry | The error state with Try again; reading both schemas and diffing them | The server's refusal is lasting, so a retry only repeats it. Studio cannot read a schema at a commit (`GET /schema` takes no commit), so it says what it knows and E52 asks for the rest |
| A commit's tab selects, the inspector follows, ⌘↵ opens the node | Unfold only | J2's next step is the node: "who changed this value" ends on the node, as it was |
| A write's commit links to its changes | A toast only | J3 starts where the write was made |
| Conflicts are said on their cards | Only in the conflicts list | J4 reads the card it is fixing, and the conflicts list has no values |
| One `studio_changes` namespace for opening and unfolding, the review keeping its own J/K | Move the review's actions to the new namespace | A person's keymap names `studio_review::*`; renaming them breaks it for nothing |
| The lists drawn as cached views of their own | Leave them in the tab's draw | `docs/drawing.md`: a field (the review's search) is never drawn inside a large view's draw |
| Delete `studio_ui::ChangeCard` and `change_group_header` | Keep them | A second card design, used nowhere, is a second answer to the question this plan settles |

## Deferred

- Engine changes ([spec 012](../012-omnigraph-engine-changes/plan.md),
  [index](../../docs/deferred-engine-changes.md)):
  - **E52** a commit's schema change: the schema at a commit, or a commit's schema diff, so a
    schema change shows what it added, changed and removed (J2). Today: "Schema change" and the
    type the refusal names.
  - **E53** a merge conflict with both sides' values and the base's, so a card can show what the
    target did beside what the branch did (J4). Today: the conflict's kind in words and the
    server's message.
  - **E44**, extended: a commit's counts by type in its listing, so History, the panel and the
    inspector need not read a page of changes. Today: one page of 100 per drawn commit.
- In Studio: the summary line's types as links to their runs; an edge's own page from a
  relationship's card; the review's split record's own Open button beside Edit and Revert (⌘↵
  and the inspector's Page serve it); counts on the branch list's rows.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-17 | The survey above | Offscreen snapshots of every screen on `main` at `b2b47fa`, debug build, Linux (gVisor, lavapipe), 1440×900, against `omnigraph-server` `0.15.1` serving the fixture (`/tmp/diffux/fixture.py`, a schema change applied with `cluster apply --as act-dev` after granting `schema_apply` in the fixture's own policy) | As listed. Also found: "1 conflict stop a merge" |
| 2026-11-17 | A changed line is one text with its words marked | `studio_ui` `a_changed_line_is_one_text_with_its_words_marked` | Pass; "spring," and "autumn," keep their comma and the rest of the line |
| 2026-11-17 | A listed commit's counts say what they counted or why there are none | `change_cards` `a_commits_counts_say_what_they_counted_or_why_there_are_none`, `a_types_row_leaves_the_properties_to_its_entities` (updated) | Pass |
| 2026-11-17 | A commit's tab moves among its changes, one draw a key, and the inspector follows | `history` `moving_through_a_commits_changes_draws_the_list_once_a_key` | Pass: p1 selected and unfolded as it opens; J, J, J at the end, K: p2, p3, p3, p2, each at c9; Space unfolds |
| 2026-11-17 | A schema change is said as one and not offered again | `history` `a_schema_change_is_said_as_one_and_not_offered_again` (a 409 `schema_boundary` at Person) | Pass: `SchemaChange(Some("Person"))`, nothing to try again |
| 2026-11-17 | Typing in the review's search leaves the list of changes | `review` `typing_in_the_search_leaves_the_changes_as_they_were`, with reduced motion: a dry run the fake server never answers keeps a spinner turning, which draws the review a few times a second | Pass: two keys draw the list 0 times; the search landing once. Without reduced motion the spinner's frame counted as one more |
| 2026-11-17 | The first commit's note says it is the first, whatever is held for it | `history` `the_inspector_shows_the_selected_commit`, changed: it inserted counts for the first commit, which no read makes | Pass |
| 2026-11-17 | A commit's selection follows its entity when a later page or a schema makes the groups anew (review comment) | `history` `the_selection_follows_its_entity_across_pages`, since replaced by the test below that goes through the real read | Pass: p2 stays selected with the groups in another order; gone from them, nothing is selected and the inspector is told. Before, the selection was an index, and a page that sorted a change before it moved it onto another node |
| 2026-11-17 | A card a merge would stop on finds its conflict in one lookup (review comment) | `review` `a_card_a_merge_would_stop_on_says_so` | Pass: "Conflicts with main" in one lookup; conflicts that replaced them with none there mark nothing. Before, each drawn card scanned every conflict; then a hand-kept index, now `Conflicts` holding list and index together |
| 2026-11-17 | A commit's tab and its rows read a refusal alike: a cleaned-up commit (410) is a state with nothing to try again, an unknown 409 "not comparable" rather than a blank (review of #130, B1) | `history` `a_refused_comparison_says_why` (rewritten over `counts::lasting`: parentless, schema change with and without a type, another 409 with its reason, 410, a 503 that may pass), `a_cleaned_up_commit_says_so_and_offers_no_retry` | Pass. Before, the tab showed the error state with Try again for a 410, and a 409 of an unknown reason left History's cell blank for good |
| 2026-11-17 | The selection stays on its node across a later page that sorts first, through the real read (review of #130, O3) | `history` `the_selection_stays_on_its_node_when_a_later_page_sorts_first` (5,000 Person inserts, then a Company update on page 2), the reviewer's test | Pass; it failed at `fa76ad1` (the selection moved to p00000), per the review |
| 2026-11-17 | Every screen after | The same snapshots on this branch | On the pull request |

## Corrections
