# 009: History

Status: built on 0.13 · 2026-10-12 (`crates/history`): a History tab with the lane graph and a
commit detail split, `OpenCommit` as its own tab, a History panel in the bottom dock, search by
id prefix or actor, and type tables and query editors pinned to a commit.

## Problem

People need to see how a graph got here: commits by branch, merges, who changed what, and the
exact changes of any commit.

## Principles

II (a commit is an immutable snapshot), VI, IX (virtualized and paged), XI (lane logic forked
from Zed with notices).

## What exists today (checked)

- Omnigraph `v0.13.0`: `GET /commits?branch=` returns every commit (no paging) with
  `graph_commit_id`, `parent_commit_id`, `merged_parent_commit_id`, `actor_id`, `created_at`;
  `GET /commits/{id}`; `GET /commits/{id}/changes` (diff against the first parent; 409
  `change_diff_refusal` for parentless or schema-boundary commits; 410 when history was cleaned
  up).
- Zed `v1.23.2` `crates/git_ui/src/git_graph.rs`: lanes assigned as commits stream in, curves
  for merges, rendered with a `canvas` beside a virtualized table, with property tests over random
  commit graphs.

## Design

- Bottom dock panel and a full tab: a lane graph left, then message-free rows (commits have no
  messages), built from `studio_ui::ds` like every list: the short id (code text), the change
  counts in a fixed column, branch heads as tags with a merge mark, then the actor and the
  relative time, muted and right-aligned. No column header; rows are inset pills at the data
  row height. The graph draws the branch's own lane in the accent and merged lanes in grey.
  The tab's toolbar is the shared one: title, then branch and commit count as meta, search and
  a refresh icon.
- Selecting a commit opens its changes (spec 008's entity cards) in a detail split, headed like
  the inspector: a small "Commit" label, the short id as the title (click copies the full id),
  then one meta row (actor, time, branch, parent and merged parent as links, change count).
  The per-type breakdown is the group headers over the cards, not a second summary. An edge
  a load upserted with another end (the change's before and after images name different
  ends) is one change on the card of the end it stayed at, "~ knows ← Ana → Bob, moved", and
  mirrored on the old end's card; with both ends moved, on its card where it is now, naming
  who held that end ("moved from Cleo"); one whose type the schema lacks shows a `from` or
  `to` row. A review shows it alike (since 2026-10-21; before, "~ knows Bob" with no rows, the
  old end never named). When both moved and one end took the other's place (Cleo knows Dan
  becomes Dan knows Eve), it is on that end's card, which it stayed joined to, whichever ends
  the commit changed, and mirrored on the card of the end that left it, pointing as it did
  (since 2026-10-22; before, Cleo's card said nothing, and the edge could sit on Eve's). A commit
  held off every branch names what it is in the branch's place ("Unpublished, on no branch
  until published to main", "Unattached snapshot, on no branch"), and a commit still being read
  names no branch until its kind is known (since 2026-10-21). The graph's first commit has no
  parent, so the server cannot compare it (a 409, `parentless_commit`): listed, its changes
  are never asked for, its row reads "first commit" in the changes column, and its detail says
  it is the graph's first commit. History opens the commits it lists in their own tabs with
  the commit as listed (a click in the panel, a double click, ↵, the row menu's "Open in a
  tab"), which is not read again. A commit named by its id alone (a restored tab, a toast's
  "View commit", a parent link) asks for its changes beside its own read, not after it, and
  the server's `parentless_commit` refusal shows it as the first (since 2026-10-21; before, it
  waited for its own read, a round trip more on every commit tab). A commit tab's "Load more
  changes", and "Try again" after a read of them failed, is ⌘⌥N (`LoadMoreChanges`). Another
  refused comparison says what stopped it by the refusal's typed reason
  (`ErrorOutput.change_diff_refusal`), then the server's reason as it came: a schema change
  between the commit and its parent names the type at it, and no other reason claims a schema
  change (since 2026-10-21; before, every 409 was said to be a schema change).
- Search by id prefix or actor with an i/N counter; ↵ and ⇧↵ step through the matches, and
  typing in the field never moves the selection.
- Any commit can open a type table or query editor pinned to it (read-only chip): the commit
  detail's Table menu (the commit's types first) dispatches `OpenType { at_commit }`, its Query
  button `OpenQueryAt`. A pinned table reads that snapshot for every page and count, ignores the
  branch and its head, and refuses edits, inserts and deletes with the reason; a pinned query
  reads the snapshot and refuses writes. Both come back after a restart.
- The bottom-dock panel follows the window's branch and graph; rows only, and a click opens the
  commit in its own tab. Hidden (another panel shown in its dock, or the dock closed), it asks
  the server nothing: what a commit, a change of branch or a changed branch list asks of it is
  read when it is next drawn, once for all of them, keeping the pages read after commits. A
  dock draws only the panel it shows, so its view reads them from its own draw, after the
  frame (since 2026-10-21; before, a panel shown once read its first page at every commit to
  its branch, about twenty an hour on the personal graph, for the rest of the run). A read of
  the branch left that is still out when the window's branch changes ends there, and the
  panel is being read until the new branch is, so neither the old branch's commits nor why
  they could not be read show under the new one's name (since 2026-10-22; before, a read out
  then landed on the emptied panel).
- Built (paging, E10, 2026-10-20): History reads its branch's newest 200 commits, and the
  next page when the rows drawn come within a screen of the list's end; every later page
  answers for the head the first page resolved (`head_commit_id`), so the pages stay one
  history while the branch moves. A commit landing on the branch reads the first page again:
  when it reaches the head the pages were read from, the new commits go on top and the pages
  read stay, else it replaces them. The rows being read stay where they are: a list scrolled
  down scrolls by the rows that came above its top row (merged-in commits read since
  included), and one at its top shows the new commits there (since 2026-10-21). The toolbar's
  counts are grouped ("12,480 commits so far · 1,200 merged in"), and ⌘R reads the history
  again as its refresh button does, which shows the key. A server that pages no history answers the whole list,
  read as one page. When a later page fails, the list's last row says why and offers Try
  again, which reads the listing from its first page when the server refused the page's
  continuation. Search finds the commits read so far, and a page read finds them again.
  A first page read again (a commit landing, a Refresh) that fails while commits are shown
  keeps them as read (constitution V): beside their count the toolbar says "The newest commits
  could not be read", the reason in its tooltip, and offers Try again, which reads the first
  page again from the head and keeps the pages read below it when it reaches their head, as a
  commit landing does; the branch list read again does the same. Meanwhile the pages below and
  the commits merged into them go on being read. Only a first read with no commit shown gives
  the list's place to the error (since 2026-10-21; before, any failed first page replaced every
  row with it, stopped paging and merged reads, and its recovery dropped the pages read).
  A read of the first page changes nothing until it is read: a Refresh forgets which merged-in
  commits the server did not have only then, so one that fails leaves each stub saying why,
  and its Try again asks for them again as the Refresh was to. A read that takes the place of
  one still under way, as a commit landing during a Refresh does, does what both were to: it
  keeps the pages read only when both would, and asks again for those commits when either
  would (since 2026-10-21; before, the commit's read dropped the Refresh's ask). A commit
  landing after a Refresh's read failed reads the page as Try again would, keeping the pages
  read and asking again for what the Refresh was to (since 2026-10-22; before, it dropped that
  ask too). Once the
  server refused the listing's continuation, Try again reads the listing from its first page
  rather than keep the pages going on from that continuation (since 2026-10-21; before, a
  failed Refresh had forgotten why the stubs were stubs, and Try again kept the refused
  continuation, which took a second Try again).
- Built (row counts): a drawn row without counts reads its commit's first 100 changes and nets
  them as the detail does ("+ more" when there are more); answers are kept, since a commit's
  changes never change, and the detail's full count replaces a first-page one. A commit
  without a first parent shows none. (500 until 2026-10-26: each change carries its before and
  after images, so 40 rows drawn asked for up to 20,000 of them, and pushed the detail's pages
  out of the shared memo.)
- Built (a branch at a commit, E21, 2026-10-20): a published commit's row menu offers "New
  branch here…", and its detail a Branch button beside Table and Query; `b`
  (`NewBranchHere`) does the same for the selected commit in the list, the panel and a
  commit's tab. Each dispatches `CreateBranchAt`, which opens spec 008's new-branch dialog
  started at that commit. A proposal or an unattached snapshot (`kind` other than
  `published`) offers none of them, and the key is not available there. A commit named by its
  id alone reads itself by id once (`GET /commits/{id}`) for its kind and branch; one History
  lists, beside it or in its own tab, is not read again.

Other branches' tags come from each branch's newest commit, read alone (a page of one) and
kept by the session until a commit lands on that branch (spec 003), for up to 20 branches; a
Refresh reads them again for every view. A branch's listing names a merged-in commit only as a
`merged_parent_commit_id`, so History reads each by id (`GET /commits/{id}`), then the parents
it names, as far back as the oldest commit of the pages read: at most 200 in one walk, 8 at
once, the rows drawn first. A walk follows each page read and starts again while a drawn row
names one not read; a read that failed is asked again 5 s later, while its row is drawn. That
route checks `read` on no branch, so it refuses (403) a token whose read grant names branches:
History then reads the newest 1,000 commits of up to 20 other branches, one page each (the
server's most), once until the next load or Refresh, and never a listing whole. A branch whose
page could not be read is kept out of those read: one that failed in passing (a timeout, a 5xx,
a 429) is read again, it alone, 5 s after it failed while a row naming a commit it may hold is
drawn, and one the server refused (another 4xx: a branch the credential may not read, one
deleted since) at the next load or Refresh (since 2026-10-21; before, a page that failed
counted as one without the commit, until the next load; and, the same day, the 5 s ran from
when the page was asked for, so one failing after them, at a gateway's 30 s timeout, was asked
for again as it failed). A merged parent not read yet, not had, or in none of
those pages draws as a stub, and the merge mark's tooltip says which: the server does not serve
it (a deleted branch's latest commits, history cleaned up); it is not among the newest 1,000
commits of any branch read and the credential may not read a commit by its id; it is not among
those of the branches read and the lists named could not be read, so it may be there, asked
for again while the row shows or at a Refresh; its read failed and is asked again while the row
shows; it is being read; or it is older than the commits read so far.
A row's change count is read once scrolling has paused for 120 ms, and reads for rows scrolled
away are dropped.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Fork Zed's lane assignment and its property tests | Write lanes from scratch | GPL accepted; the tests are the specification |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E10 paged commit list (Omnigraph #1013): built 2026-10-20 (Design). Two parts differ from
    the plan here before: a commit landing reads the first page again and keeps the pages it
    reaches, rather than showing "N newer commits · Refresh"; and merged-in commits are read by
    id rather than from other branches' paged lists, which start at those branches' heads now.
  - E21 a branch from a commit (Omnigraph #1016): built 2026-10-20 (Design).
  - E9 branch details: branch chips derived from per-branch commit lists until then.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-22 | An edge whose end took the other's place is mirrored on the end it left | `graph_store` `entity_groups::a_relationship_whose_end_moved_names_the_end_it_had`, extended: Knows from Gus to Hal upserted from Hal to Ivy, Gus and Ivy changed in the review, Hal not; failing with the mirror undone | On Hal's card, "hal → ivy", "moved from gus", mirrored on Gus's pointing at Hal as it did and on Ivy's as the other end; each change reviewed once. Undone, Gus's card had no mirror; placed as before, the edge sat on Ivy's card, whose mirror on Gus would have named Ivy, whom Gus never knew |
| 2026-10-22 | The panel shows nothing of the branch left under the new one's name | `history` `a_panel_shows_nothing_of_the_branch_left_under_the_new_ones_name` (main's first page out, the panel hidden and the window moved to feature; then again with that page failing), failing with `forget_branch` leaving the read running | Both times no commit showed under feature and the history was being read; shown again, it read feature's two commits. Undone, main's three commits showed under feature, and the failure took the list's place under feature's name |
| 2026-10-22 | A commit after a failed Refresh keeps the Refresh's ask | `history` `merged_in_commits_are_read_by_their_ids`, extended: a Refresh's page fails, then a commit lands; failing with only a read under way carried | The commit's read kept the pages and asked again for the merged commit the server did not have, a fifth read of it. Undone, the read was `KEEP` alone and the stub was not asked for again |
| 2026-10-21 | An edge moved to another end names both | `graph_store` `changes::an_edge_with_another_end_is_changed_and_names_both`, `entity_groups::a_relationship_whose_end_moved_names_the_end_it_had` (Knows from Ana to Maya upserted from Bob, and from Cleo to Dan upserted from Eve to Finn, Ana and Dan changed in the review); `change_cards` `a_moved_relationship_names_the_end_it_had`; `carry.rs` `Logged::add`, which already took both images' ends | The first is on Maya's card, "Ana → Bob", "moved", and mirrored on Ana's; the second on Eve's, "Dan → Finn", "moved from Cleo", with no mirror on Dan's, whose Dan was joined to Cleo; each change is reviewed once, and Maya, Bob, Eve, Finn and Cleo are read for their names. Before, both were homed by their new ends alone, read "~ knows Bob" and "~ knows Finn" with no rows, and the old ends were neither named nor read. Not yet seen in the running app |
| 2026-10-21 | A commit landing during a Refresh keeps the Refresh's ask | `history` `merged_in_commits_are_read_by_their_ids`, extended: a Refresh, then a commit to the branch before the Refresh's page is read | The read in its place is `State::Loading { read: FirstPage::REFRESH }`, and the merged commit the server does not have is asked for once more. Before, `committed` sent `load(FirstPage::KEEP)`, which replaced the Refresh's task and its `ask_missing`, so the stub was not asked for again until the next Refresh (found in review of `0ecb3a0`) |
| 2026-10-21 | A hidden History panel reads nothing until shown | `history` `a_hidden_panel_reads_nothing_until_shown_again` (the panel drawn cached by a stand-in dock, as `studio_workspace` `panel_body` draws a bottom panel; hidden, two commits land; shown; then a third) | One listing as first drawn; none for the two commits while hidden; one when shown again, the five commits in order; one more for the third, while shown. Before, `HistoryView::panel` read the first page at each `Committed` whether drawn or not. Not yet seen in the running app |
| 2026-10-21 | A refused comparison is read from a typed field | `graph_client` `a_refused_commit_comparison_names_its_reason`, extended (the whole refusal with its type, and a reason a later build sends); `history` `a_refused_comparison_says_why`; the contract test tables (`ErrorOutput` now lists `change_diff_refusal`, checked against `openapi/omnigraph-0.14.0-edge.json`) | `ErrorOutput` carries `change_diff_refusal` as `ChangeDiffRefusalOutput`, and `ApiErrorSummary` keeps it whole, boxed so a read's error stays under clippy's `result_large_err` bound; before, it was parsed out of `ErrorOutput::other`. A schema boundary on `Person` says it is "across a change to Person's schema", then the server's reason; another 409 gives the server's reason and claims no schema change |
| 2026-10-11 | `git_graph.rs` exists at the pin | `git ls-tree -r v1.23.2 crates/git_ui/src` | Present |
| 2026-10-12 | Lanes connect and never collide | `history::lanes` property test over 2,000 seeded random commit graphs (up to 60 commits, merges, missing parents): every edge is one line, rows join edge to edge, no two lines share a lane, width bounded | Passes; a mutation that leaves ending lanes busy fails at seed 0 |
| 2026-10-12 | Merges draw against a live server | offscreen snapshot of main after merging `feature-y` and fast-forwarding `feature-z` | Both merged lanes join main; heads shown as branch chips |
| 2026-10-12 | Parentless commit | live server: `GET /commits/{genesis}/changes` | 409 "has no first parent"; the detail says the commit cannot be compared |
| 2026-10-12 | Pinned tables and queries read their commit | offscreen snapshot restoring `Person @ 4S3Y6CKK`, `Person @ EP12CTBE` and a query at `4S3Y6CKK`, with the History panel open | Tabs named with the commit, a "Read only · 4S3Y6CKK" chip, footer hints reduced to Peek; the query's result reads "people on commit 4S3Y6CKK" |
| 2026-10-12 | Change counts on rows | offscreen snapshot of a scratch branch with two edits on top of main | `~3` and `~1` on the edits, `+500 + more` on the seed load, none on the first commit |
| 2026-10-12 | Search matches by id prefix or actor | `history` test `search_finds_commits_by_id_prefix_or_actor` | An id matches only by prefix; actors match anywhere, case-blind |
| 2026-10-20 | History reads a page at a time, merged-in commits by id | `history` tests `history_reads_its_branch_a_page_at_a_time`, `an_old_server_answers_the_whole_history_as_one_page`, `a_commit_landing_on_the_branch_keeps_the_pages_read`, `try_again_after_a_refused_page_starts_from_the_first`, `merged_in_commits_are_read_by_their_ids`, `a_merged_branch_is_read_as_far_as_the_pages_read`, `a_long_merged_branch_is_read_as_its_rows_are_drawn`, `merged_reads_go_to_the_rows_drawn_first`, `a_failed_merged_read_is_asked_again_after_a_while`, `a_token_refused_commits_by_id_reads_merges_from_the_listings`, `tags_sit_at_each_branchs_newest_commit`, `a_commit_to_another_branch_moves_its_tag_alone`, `a_search_finds_matches_a_later_page_brings`, against a fake server that pages as #1013 does | Pass; no listing is read whole but a 0.14 server's that pages none, and the other branches' after a 403 by id |
| 2026-10-20 | A token refused commits by id reads one page of each other branch, and a stub says why it is one | `history` tests `a_token_refused_commits_by_id_reads_merges_from_the_listings` (a third branch of 1,102 commits whose `z1`, merged into main, lies below its newest 1,000), `merged_in_commits_are_read_by_their_ids` (a merged parent the server answers 404 for); `history::merged` tests `listed_commits_answer_a_merged_chain_at_once`, `a_merge_from_a_commit_the_server_does_not_have_stays_a_stub` (`Absent::NotListed`, `Absent::NotHad`); against the `Served` fake paging as `edge-20261020-7fd9cc9` does and sending the contract header | Pass: each other branch is read as `branch=<name>&limit=1000`, once per load and again after Refresh; every listing names a `limit`; `z1` draws as a stub whose tooltip reads "The merged commit is not among the newest 1,000 commits of any branch read, and this credential may not read a commit by its id."; the 404 one "The server does not serve the merged commit, as for a deleted branch's latest commits and history cleaned up." |
| 2026-10-20 | A deleted branch's commits by id | Omnigraph `GraphCoordinator::resolve_commit` at `0a62bb7` (source read) | `GET /commits/{id}` serves them once they have moved into the graph's `__history`; its latest commits, still buffered in the branch's retired `__manifest`, answer 404 and draw as stubs |
| 2026-10-20 | "New branch here" is offered only at a published commit | `history` tests `new_branch_here_is_offered_only_at_a_published_commit` (the selected row's key and the row menu, over published, unpublished and unattached commits) and `a_branch_starts_here_only_at_a_published_commit` (a commit's own tab reads its kind; a listed one is not read), against the `Served` fake and a fake sending the contract header | Pass: only the published commit makes the key available, shows the menu entry and starts `CreateBranchAt` |
| 2026-10-21 | A held commit's view names what it is, not main | `history` `a_branch_starts_here_only_at_a_published_commit`, extended: own tabs of a published, an unpublished and an unattached commit, each read by id, and `branch_fact` for a commit still being read and for one that could not be read | Before (found in the running app on the edge server, after a write `as commit`), the meta line showed "main", the cause's `authored_branch`. Now: published, "main"; unpublished, "Unpublished, on no branch until published to main"; unattached, "Unattached snapshot, on no branch"; still being read, no branch; unreadable, the cause's branch. Not yet seen in the running app |
| 2026-10-21 | A commit tab makes no read wait for another | `history` `a_commit_named_by_its_id_reads_its_changes_at_once` (the commit by id held 5 s of the test clock, its changes answered at once; the first commit `c0` refused with `change_diff_refusal.reason: parentless_commit`, as `omnigraph-server` `commit_changes_parentless_commit_is_typed_409` at `7fd9cc9` answers), `a_listed_commit_opens_its_tab_without_reading_itself` (in a workspace's window: no `/commits/c7`, one `/commits/c7/changes`), `a_branch_starts_here_only_at_a_published_commit` (the first commit listed asks nothing, in its own tab once); `graph_client` `a_refused_commit_comparison_names_its_reason` | Before, `load_more` waited for `GET /commits/{id}`, so every History click (each opens a tab), toast "View commit", parent link and restored tab made two reads in turn, and `OpenCommit` dropped the commit History held. Now c9's changes are in while its own read is out, c0 shows "The graph's first commit" from the refusal while its own read is out, and a listed commit is not read again. Not yet seen in the running app |
| 2026-10-21 | The rows being read stay where they are when a commit lands | `history` `a_commit_landing_on_the_branch_keeps_the_pages_read`, extended: scrolled to row 50 (m399 at the top), m450 lands; then at the top, m451 lands | Before, `take` put the new rows on top with no scroll adjustment, moving m399 down a row under the reader. Now the offset moves by the row that came above, m399 stays at the top of the rows shown (row 51); at the top, m451 shows there |
| 2026-10-21 | ⌘R refreshes History, and a commit tab's Load more has a key | `history` `merged_in_commits_are_read_by_their_ids` (⌘R dispatched as `studio::Refresh` with the view focused: the first page and the missing merged parent read again); `studio` `keymaps` (`studio_history::LoadMoreChanges` bound on every platform, ⌘⌥N / Ctrl+Alt+N in `CommitView`) | Before, nothing in History handled `Refresh`, so ⌘R went to the workspace and "Newer data" stayed; the refresh button showed no key, and Load more and Try again had no action |
| 2026-10-21 | The toolbar's counts are grouped | `history` `the_summary_groups_its_counts` | "12,480 commits so far · 1,200 merged in"; before, "12480 commits" |
| 2026-10-21 | The graph's first commit is named, and its changes never asked for | `history` `the_first_commits_changes_are_never_asked_for` (a three-commit chain, row counts read for every row) and `a_branch_starts_here_only_at_a_published_commit` (the first commit listed and in its own tab) | Before (found in the running app on the edge server), each History open sent `/commits/{root}/changes` and the server answered 409; the row showed nothing and the detail an error. Now no `/commits/m0/changes` is sent, the row reads "first commit", and both views are in their `First` state: "The graph's first commit", with no read. Not yet seen in the running app |
| 2026-10-21 | A first page read again that fails keeps the commits shown | `history` `a_failed_read_again_keeps_the_commits_shown` (two pages read, m450 lands, its first page answers 503 until cleared: the rows stay, the next page is read, the branch list read again fails again, then Try again) and `a_first_read_that_fails_says_why_in_place_of_the_list` (the first read answers 503 with nothing shown); both check the drawn window for the toolbar's note and the error state | Before (found in review), `first_page` set `Failed` even for a commit landing, so the error state took every row's place, paging and merged reads stopped, and the branch list read again ran `load(false)`, which dropped the pages read. Now the 400 rows stay with "The newest commits could not be read" in the toolbar and no error state, the third page brings 450, and Try again puts m450 on top of them (451, `m0` last) with no listing read again but the first page; with nothing shown, the error state takes the list's place, and the branch list read again reads it. A mutation that always sets `Failed` fails the first test |
| 2026-10-21 | A listing that could not be read is not taken for one without the commit | `history` `a_listing_that_could_not_be_read_is_read_again_alone` (a token refused by id; `feature`'s page answers 503 once, `secret`'s 403); `history::merged` `a_commit_a_page_not_read_may_hold_is_answered_once_it_is_read` | Before (found in review), `read_listed` dropped a failed page and `take_from` marked every commit it may hold `NotListed`, kept until the next load, its tooltip saying it was among the newest 1,000 commits of no branch read. Now b2 is `Absent::Unlisted`, its tooltip naming the lists of `feature` and `secret`; 5 s later, with the row drawn, `feature`'s page alone is read again (not `fix`'s, nor the refused `secret`'s), b2 and b1 draw, and z9's tooltip names `secret` alone, which a Refresh reads again. A mutation that drops the failure fails the test |
| 2026-10-21 | A listing that fails slowly waits from its failure | `history` `a_listing_that_could_not_be_read_is_read_again_alone`, extended: `feature`'s page fails only after 30 s of the test clock (`Served::fail_after`), then a drawn row asks at once, 1 ms before the 5 s and at them | Before (found in review of `9e8cfdd`), `failed_at` was the time the reads were planned, so a page failing 30 s later was due at once and the next redraw sent the same 1,000-commit page again, back to back. Now it is the time the reads ended, as for a read by id: nothing is sent at the failure nor 1 ms short of 5 s, and `feature`'s page alone at 5 s. A mutation that dates the failure 30 s back fails the test |
| 2026-10-21 | A failed Refresh keeps why each stub is one | `history` `merged_in_commits_are_read_by_their_ids`, extended: a Refresh whose first page answers 503, then Try again, then ⌘R | Before (found in review of `9e8cfdd`), `refresh` forgot the missing commits before its page was read, so after the 503 `gone` read as "being read" or "older". Now it stays `Absent::NotHad` with the server-does-not-serve tooltip and is not asked for; Try again (`State::Stale { read: FirstPage::REFRESH }`) asks for it once, as the Refresh was to, keeping the rows, and ⌘R once more. A mutation that forgets before the read fails the test |
| 2026-10-21 | One Try again after a failed restart | `history` `try_again_after_a_refused_page_starts_from_the_first`, extended: the continuation `m449_200` refused (`Served::refused_tokens`), m450 lands, the restart's first page answers 503, then the toolbar's Try again and the next page | Before (found in review of `9e8cfdd`), Try again read the first page keeping the pages, which it reached, so the refused `m449_200` stayed the continuation and the next page was refused again: a second Try again. Now a refused continuation makes the read replace the pages: 200 rows from m450, and the next page is `m450_200`, read; `m449_200` is never sent again. (The session, available again after the 503, reads main's head and reports m450 as landed, one more first page that keeps the pages.) A mutation that keeps the pages fails the test with 201 rows |

## Corrections

- 2026-10-21: a commit opened in its own tab without its listing no longer waits for its own
  read before asking for its changes, which the row "The graph's first commit is named" below
  had it do: it asks for both at once, and the server's `parentless_commit` refusal names the
  graph's first commit (Design).
- 2026-10-12: lanes are a fresh implementation of the idea in Zed's `GraphData::add_commits`, not a
  fork: each row records the lines through it, ending at its commit and leaving it, so every
  row of the virtualized list draws itself with a `canvas`. The property tests are Studio's own.
- 2026-10-12: `GET /commits?branch=` lists one branch's first-parent chain; merged-in commits appear
  only as `merged_parent_commit_id`. History also reads up to 20 other live branches' lists and
  adds every commit reachable through a merged parent (`history::merged`). Merges from deleted
  branches draw as a short stub. Superseded on 2026-10-20 (below).
- 2026-10-20: History reads merged-in commits by id, not from the other branches' lists, which
  it reads only for a token refused commits by id (Design). A merge from a deleted branch draws
  as a lane once its commits are in the graph's `__history`; its latest commits, still buffered
  in the branch's retired `__manifest`, answer 404 and draw as a stub.
- 2026-10-20: for a token refused commits by id, History read the other branches' whole
  listings, every commit since each began. It reads the newest page of each (1,000 commits),
  and the stubs say why the commits merged are not drawn (Design).
