# 008: Branches, review and merge

Status: built on 0.13 · 2026-10-12 (`crates/review`, shared logic in `graph_store::changes` and
`graph_store::fixes`, cards in `studio_ui`). Approvals and comments are deferred (below).

## Problem

Branches are how changes to a graph get reviewed. Studio needs to create and switch branches,
show what a branch changed in a form a reviewer can read, fix mistakes in place, and merge with
conflicts explained.

## Principles

III (merge outcomes), IV (branch named; typed confirmation for delete), V (review items stay in
place), X (designed on 0.13; the better routes are deferred).

## What exists today (checked)

- Omnigraph `v0.13.0` HTTP: `GET /branches` (names only), `POST /branches {name, from}`,
  `DELETE /branches/{branch}`, `POST /branches/merge {source, target, delete_branch}` with
  outcome `already_up_to_date`, `fast_forward` or `merged`; a 409 carries `merge_conflicts[]`
  with kind `divergent_insert`, `divergent_update`, `delete_vs_update`, `orphan_edge`,
  `unique_violation`, `cardinality_violation` or `value_constraint_violation`.
- Changes: `GET /changes?branch=&start=after:<commit>&limit=&page_token=` (blocks of entity
  changes with `op` insert, update or delete and before/after images); `GET /commits/{id}/changes`
  (one commit against its first parent). `GET /commits?branch=` lists commits with parents.
- Not in 0.13.0: merge `as_commit`, commit publish and drop, a branch-to-branch diff, a merge
  preview, branch details (all in `docs/deferred-engine-changes.md`).
- An earlier web client's review: one summary sentence; changes to existing data first; groups
  of an entity plus the relationships created with it; fixes bound to the reviewed head and never
  retried.
- Zed `v1.23.2` `crates/git_ui/src/`: `project_diff.rs`, `commit_view.rs`, `conflict_view.rs`;
  `buffer_diff` and multibuffers present diffs as one scroll of excerpts.

## Design

### Branches

- Sidebar list with current branch marked; ⌘⇧B switch; create from the current branch or a
  commit. Fork point and last commit derived from `GET /commits?branch=`. The list draws only
  the rows in view, "New branch…" the row after the last, and builds a row's menu when it
  opens, so a graph of thousands of branches costs a frame a screenful of rows (since
  2026-10-21; before, every branch's row and menu were built at every frame). Each branch's
  head, actor and time are the shown graph's: a switch of graph drops the details and the reads
  still out for the previous one (since 2026-10-21). A head is read only for a row a shown list
  draws, once the frame that drew it is out, as History's panel does (spec 009): a list hidden
  in its dock reads none, and each row's is read once until a commit lands on its branch or the
  branch list changes; one that changes while the list is hidden is read when it is shown again
  (since 2026-10-22; before, the first 20 heads were read as the list was made and again at
  every commit or change of the list, shown or not).
- Built (start at a commit, E21, 2026-10-20): History's "New branch here…" (spec 009)
  dispatches `CreateBranchAt`, which opens the same new-branch dialog started at that commit:
  "New branch at <short id>", then who made it and when, read by its id as the dialog opens
  (`GET /commits/{id}`; a token refused that read sees the id alone), and the CLI line
  `omnigraph branch create <name> --from-commit <id>`. The session sends `from_commit` and
  no `from` (`graph_store::BranchStart`); the receipt's `from_commit_id` is the new branch's
  head, which the session records, and a commit start's outcome names it ("Create branch
  <name> at <short id>"). Success switches to the branch, as a start at a head does. A
  `branch_start_refusal` reads in words, nothing created: cleanup no longer keeps <type> as
  it was at the commit (410); written under an earlier schema; written before the storage
  upgrade; <branch>, which wrote it, was deleted; not published yet; an unattached snapshot
  (409); a reason this build does not know reads as the server's message. The notice is the
  commit's, so it stays while the name changes and Create stays off; nothing is sent again.
- Delete asks once, saying what goes with the branch, read from its merge base with `main` as
  the dialog opens: a branch whose head is in `main` loses nothing and is deleted with ↵; one
  with commits only it holds, or one the check could not read, is deleted by typing its name
  (constitution IV). The dialog holds that one sentence, the name field when it is needed,
  and Cancel and Delete; the CLI command is the Delete button's tooltip.

### Review (`review` crate)

- Opens as a tab "Review <branch> → <target>".
- Scope selector: since the fork (default), or one commit (its own changes, read from
  `GET /graphs/{g}/commits/{id}/changes`). The menu is a list that draws, and labels, only the
  commits in view, the keys moving through every row (since 2026-10-21). A scope change at the
  heads the review was read at, as far as Studio knows (no other writer's commit or schema
  seen, no fix landed, neither branch seen at another commit), takes the merge base and the
  commits since the fork it holds and reads only the new scope's changes; otherwise it reads
  them again, as a Refresh does. The commits since the fork and the changes they do not depend
  on are read together, and so are a page of each history once both are needed (since
  2026-10-21).
- Search: a field narrows the cards to those whose own change or a relationship on them has an
  id, type, relationship end, or before or after value containing the text; J/K move among
  the cards it leaves.
- Header: one sentence ("17 entities and 22 relationships changed in 3 commits by act-alice"),
  then per-type counts (+ added, ~ changed, − removed).
- Body: one scroll of entity cards (`graph_store::EntityGroups`) in two sections: changes to
  what the target has (changed, removed, or unchanged with relationships that changed), then
  what the branch added. Inside a section, cards run by type under one header counting what the
  run shows. A card is one node: its changed properties, before and after (a removed one stays
  as a real, searchable row), then the relationship changes that came with it. A changed list
  shows the elements it gained and lost and how many it kept; a changed text of eight words or
  more shows its changed words with three words of context on each side, the rest as "…",
  punctuation compared as its own mark (texts over 200 words show before → after).
- A relationship change is on one card, its home: the end the branch added, else an end it
  changed or removed, else its source, which then stands unchanged for its relationships.
  There it is counted, reviewed and reverted. When the other end has a card too, the
  relationship shows there as well, muted, "reviewed there", so either entity tells its whole
  story. A line reads as the relationship in words (`WorksAt` is "works at", `NoteAboutPerson`
  from Note to Person is "about"), an arrow for its direction and the other end by its label
  property, else its id; "new" marks an end the branch added, and a changed relationship's
  properties follow it. An end the review knows only by id, and a node standing unchanged for
  its relationships, is named by its label read at the reviewed head (`builder::node_labels`,
  500 ids a read, 5,000 nodes at most); until then, or without a label, its id shows. A
  relationship moved to another end, the one removal and one addition of an edge type that
  allows one target per source (`@card(..1)`) from the card's entity, is one line, "works at
  Lumen Labs → Northwind, moved", its rows comparing the old edge's values with the new one's;
  its Revert removes the new edge, then restores the old one, as two writes in that order. A
  relationship whose type the schema lacks stands alone as its own card. A card's header is
  the kind's symbol (+ ~ −, · for unchanged), the entity's name and its id; its type is the
  run's header's. Cards are hairline frames on the document, values in the UI font.
- Act in place: Revert (restore the before image), Edit, Mark reviewed (every change on the
  card; the card turns hollow but stays, and says how many are not reviewed when some are).
  Each relationship line has its own Revert, shown on hover. Keys: J/K between cards, ↵ edit,
  ⌘Y accept the card and go to the next, ⌥⌘Z revert the selected change (the card's own, or
  the relationship line clicked) and go to the next, ⌘R read the review again as its refresh
  button and every banner's Refresh do, each of which shows the key (since 2026-10-21).
- Edit (a node that is added or changed): a form of its editable properties filled with what
  the branch holds; Save writes only the fields that changed, each bound to that value, as one
  mutation on the reviewed branch bound to the reviewed head. After a fix the card's other
  fixes wait for Refresh.
- Revert of a relationship: an added one is deleted by `@id`; a removed one is inserted again
  between its before endpoints with its before values (an unkeyed one gets a new id: a `.gq`
  insert cannot supply one). A changed relationship is not reverted here: edges cannot be
  updated, and the delete and insert it needs cannot share one mutation, so it would publish
  twice. Its card says so and points to the query editor.
- A credential the access route refuses changes on the branch (spec 002) writes nothing from
  the review: Edit and every Revert are disabled with the refusal as their tooltip, and ↵ and
  ⌥⌘Z are not registered (since 2026-10-21). A new token reads the review again, its names and
  dry run included; nothing read with the old one shows meanwhile, nor after a read the new
  token is refused: the cards, the counts, the banners and the scope menu's commits go with the
  review read, and a scope change reads the merge base again. Reviewed marks stay, and a fix
  asked for and not back is still sent, bound as it was (since 2026-10-21).
- Every fix is a write on the branch, bound with `/mutate/if-graph-commit` to the reviewed head
  or to the commit of the fix before it; a 412 says the branch moved and offers Refresh. Fixes
  go one at a time, so a second one asked for quickly is bound to the first's commit, and each
  outcome finds its card by the change's key, also after a Refresh. A reviewed mark is kept by
  the change and its last commit: changed again since, it is reviewed again.
- Data: change feed from the fork, folded per entity (first before, last after), capped at
  20,000 changes with "Partial: showing the first 20,000 changes" when capped.
- A read of changes (a page of the feed, of a diff, of one commit's changes, and the dry run)
  waits up to five minutes, every other read one (`graph_client::CHANGE_READ_TIMEOUT`, since
  2026-10-26): the server works a page out whole before it answers, which on a large graph takes
  most of a minute (E35 to E38). A review still being read after 10 s says so, the header's
  line under "Reading what feature changed…" reading "On a large graph this can take a few
  minutes." and the body "The server is still working out the changes…". A page the server had
  not answered in five minutes reads "The server was still working out the changes after 5
  minutes, and Studio stopped waiting." with Refresh; a connection that did not open reads "Could
  not reach the server: the connection timed out". Nothing is sent again by itself.

### Merge

- "Merge into <target>" names both branches. Before merging, the button checks the branch head
  still equals the reviewed head. It merges only a review on screen, with no fix out: never
  while the review is being read (a head nobody has seen), and never from the branch list or
  the menu, which open the review instead.
- "Delete <branch>", beside the button and on by default, deletes the branch once the merge
  lands (`delete_branch`): the server reports the deletion on the answer and never undoes the
  merge for it, and the outcome says "Deleted <branch>." or why it was not. A session on the
  deleted branch moves to the target. Not offered for `main`.
- Before anyone merges, a review of the whole branch asks what a merge would do now with a
  dry run (`dry_run` on `POST /branches/merge`, E8): the server reads both branches, takes no
  write gate and writes nothing. A clean merge reads beside the fork, "merges cleanly as a
  fast-forward"; conflicts show in a banner with Show conflicts, the same list a merge's 409
  gives. A dry run that cannot be read says nothing; Merge stays available either way, since
  the merge itself decides.
- Outcome shown plainly: fast-forwarded, merged (with commit), already up to date.
- A 409 opens a conflicts view: one row per conflict with its kind in words ("Changed on both
  branches", "Deleted on main, changed here", "Relationship points to a deleted node"), the
  entity, and the message. Resolution happens on the source branch with ordinary edits; then
  merge again.
- After merging, Studio confirms the target's history contains the merge (a verified
  merge), reading it a page at a time from its head for at most its newest 10,000 commits, and
  only down to the target's head as Studio checked it just before sending the merge: a merge
  that lands publishes a commit of the target above that head, so a read that reaches it
  without the merge has read every commit the merge can be, and reads as not shown (since
  2026-10-21; before, a merge that never landed on a target of more than 10,000 commits read
  20 pages, "not reached", at every Refresh). A merge not in the pages read is not confirmed: "Not confirmed: the newest 10,000 commits of main's
  history do not show it, and Studio read no further; open History to check." A lost merge
  response read the same way stays unknown, never "merge again". Merge stays off while that
  history is read and while what was read cannot tell ("… Studio did not retry, and Merge stays
  off until main's history settles it: open History to check, or Refresh to read it again."):
  a second merge would cancel the read and be sent while the first may still land. Revert and
  Edit stay off with it, saying a merge may be running: a fix could land on either side of the
  merge (since 2026-10-21). Refresh reads the history again, never the merge. The lost merge is
  one entry of the session's outcome log, whose fields are the source and target, the source
  head the merge was sent for, the target's head just before it was sent and the outcome; its
  description is for display alone. Every review of the same branches in the session starts
  from the log's newest merge of them: one opened again, or open elsewhere and read again,
  reads the history for it, offering neither Merge nor a fix meanwhile (since 2026-10-21).
  What a read of the history shows, landed or not shown, is recorded on that entry, so the
  session's other reviews of the same branches take it without a Refresh of their own, and a
  merge shown to have landed stays so. The log keeps a merge that may still land past its
  newest 1,000 writes (since 2026-10-21). History read to its end without the merge lets
  Merge run again, saying the first may still land ("… does not show it yet, but the merge may
  still land. Studio did not retry; open History to check before merging again."), and a merge
  it shows ends the review's merging as a committed one does (since 2026-10-21).
- A fast-forward publishes a new target commit whose merged parent is the branch head, as a
  merge commit does: the head it took is that merged parent, so one of the reviewed head no
  longer says it took a commit made after the review (since 2026-10-21; before, every
  fast-forward said so).

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Review from the change feed | Wait for a diff route | Works on 0.13; the route is deferred |
| Stat cards removed from the header | Stat cards above the changes | They read as cluttered in an earlier design |
| A node's card carries its relationship changes, mirrored at the other end | Sections by type with edges in their own; a relationship on one card only | A reader joined a node and its edges by hand, from keys alone; a mirror lets either end tell the whole story while each change is counted and reverted once |
| Conflicts resolved by editing the source branch | Ours/theirs buttons | 0.13 has no resolution API; edits are ordinary writes |
| Reads of changes wait five minutes | Keep 60 s for every read; retry a read that timed out; no limit | On the personal graph one page took 37 to 60 s, so 60 s failed about as often as it answered. A read that timed out is not sent again by itself: the person's Refresh asks once more. Five minutes matches a write, and still ends a read whose server stopped answering |
| A branch its target was merged into is diffed to its head named by commit id | Name the branch (`to=<branch>`, or `GET /branches/diff`) and check the commit the first page resolved against the pinned head | Naming the branch saves 6 to 9 s a page (E37), but cannot keep the review exact for free. A page token binds every later page to the sides as the first page spelled them (its `sides` digest, Omnigraph `7711e4a` `prepare_diff`), so a branch that moved before the first page resolved it can be caught only by reading that page again by commit id: a whole page more (37 to 60 s) exactly when the review must stay exact. Taking the newer head instead would pair it with a merge base, commits since the fork and a fix binding read for the old head. `GET /branches/diff` reads the merge base again, which the review holds. Revisit if #972 stays open |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E6 paged branch-to-branch diff: `GET /diff` since 0.14. A review folds the change feed
    after the merge base, for each commit's author and the reviewed marks; a branch its target
    was merged into, whose base the feed refuses as a start, folds the net diff from the merge
    base to the reviewed head instead (both pinned), capped at 20,000 changes either way.
  - E7 merge base: `GET /branches/merge-base` since 0.14 (fork, head, and a merge's
    confirmation). A review of one commit reads the branch's history a page at a time down to
    the merge base. A branch its target was merged into, whose merge base is off its own
    chain, finds its commits since the fork from pages of both histories, read in turn: its
    first-parent chain down to its newest commit the target's pages hold, listed there or as a
    merged parent (`graph_store::ForkSearch`). No history is read whole: each is read at most
    20 pages of 500. A fork past them shows the branch's own commits read, down to the first
    one another branch published (`CommitOutput.graph_branch`, the branch that wrote it; the
    pages go on into history it shares with the branch it forked from), and a note that says
    which history stopped the read, counting only commits at or below the reviewed head (the
    pages also hold those the branch made after it): "More than 9,700 commits since the fork:
    …" when the branch's own commits go on past its pages; "The fork is past the newest 10,000
    commits of main's history: Studio read no further, and One commit lists the 3 commits made
    on feature." when the target's pages stopped it; "The fork is more than 10,000 commits down
    feature's history: …" when the branch's pages, below its own commits, did (since
    2026-10-21). The summary names no commit count or author ("17 entities changed"). A lost
    merge response reads the target's history a page at a time from its head, as far.
  - E8 merge preview with no effects: merge on request and show the 409 conflicts until then.
  - E9 branch details: derived from `GET /commits?branch=` until then.
  - E21 a branch from a commit (`from_commit`, Omnigraph #1016): built 2026-10-20 (Design,
    Branches). The agent's gateway still refuses a create started at a commit.
  - R2 unpublished commits: `as_commit` merge proposals. The minimum server (0.14) serves
    them; Studio does not use them yet.
  - E28 a merge bound to the reviewed source head: until then a commit landing between the
    head check and the merge is merged too, and the outcome says so.
  - E35 to E38, what makes a review slow on a large graph: a diff that reads whole tables
    (#969), a page that restarts its work (#970), a commit looked up in every branch (#972), and
    a feed page with no bound on its commits (#1157); the draft RFC #1171 answers the first and the
    last. Until then reads of changes wait five minutes and
    the review says the server is still working (Design, Review).
- Approvals and comments on reviews.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-26 | The review says when the server is slow and when Studio stopped waiting | `graph_client` `a_timeout_says_whether_the_server_was_reached` (reqwest's chains at `zed-reqwest` `33bc764` and `hyper-util` 0.1.17: a request's `operation timed out`, a connection's under `client error (Connect)`); `review` `a_slow_review_says_the_server_is_still_working` (the feed's page held past `SLOW_READ`, then answered, then read again), `a_review_that_timed_out_says_the_server_was_still_working` (the feed's page failing as a timed-out request does) | Before, a slow review showed "Reading what feature changed…" and nothing else, then "The review could not be read: Could not reach the server: the request timed out", though the server had taken the request. Now, after 10 s, "On a large graph this can take a few minutes." under the header and "The server is still working out the changes…" in the body, gone once the review is read; a page not answered reads "The server was still working out the changes after 5 minutes, and Studio stopped waiting." with Refresh; a connection that did not open reads "the connection timed out". Not yet seen in the running app against the personal graph |
| 2026-10-26 | Reads of changes wait five minutes; every other read one | The hosted personal graph (server `0.14.1-edge+7711e4a`, a read-only token), timed from the person's Mac over HTTPS on 2026-10-25: merge base 3.0 to 5.7 s; the feed from the fork of `review-commercial-profile-20261019` (9 commits, 366 changes) 54.4 s as one page; `GET /diff` merge base to head, 350 changes, 60.3 s cold and 37 to 43 s warm; the same with `type=Artifact` (about 100,000 rows, changed by 3 of the branch's commits) 47.6 s, any other single type 7.9 to 9.6 s; head to head, zero changes, head named by commit id, 12.7 to 13.6 s; one commit's changes 4.5 to 17.7 s, about 13 s for a one-change commit on the branch. `graph_client` `a_read_of_changes_waits_longer_than_other_reads` (each request's `RequestTimeout`) | Before, every read timed out at 60 s, so a review's feed or diff page failed about as often as it was read, and more often cold. Now the feed, a diff, a commit's changes and the dry run wait 300 s, and the twelve other reads the test makes 60 s. A read that times out is still not sent again by itself |
| 2026-10-26 | Naming the branch on a merged-in review's diff cannot keep the review exact without a second read | Omnigraph `7711e4a`: `crates/omnigraph/src/db/omnigraph.rs` `prepare_diff` (a continuation's `sides` digest must equal the request's) and `reopen_diff_continuation` (a continuation classifies its sides again and reaches only the token's commits); `crates/omnigraph-server/src/handlers.rs` `server_diff` ("The first page resolves both sides to exact commits and names them") | Not built (Decisions). A first page asked `to=feature` names the commit it resolved, but every later page must name `feature` too, and a head other than the pinned one is caught only by asking the first page again by commit id |
| 2026-10-29 | A review not drawn reads no diff | `review` `a_review_not_drawn_reads_no_diff` (a review in a window that draws a blank root: no read; another drawn: read and ready); every other review test, each drawn in its window | Pass. Before, a review restored behind another tab read its whole branch diff at start, and again after a new token; it now reads when first shown, as its dry run already did |
| 2026-10-22 | The branch list reads heads only while it is shown, for the rows in view | `review` `a_hidden_branch_list_reads_no_heads_until_shown` (three branches, the list cached in a stand-in dock, hidden, then shown; a fourth branch made while it is hidden again); `the_branch_list_builds_only_the_rows_in_view`, extended (2,000 branches, a 320 pt dock); both failing on the previous reads | Hidden, no `GET /commits?branch=` as it opens or when the branches change; shown, one per branch, and none when drawn again; the branch made meanwhile is read alone when it is shown again, the session answering the others from the heads it holds. Of 2,000 branches, the heads of the 12 rows in view, each once. The list asks after the frame that drew the rows, as History's panel does (spec 009). Before, it asked for the first 20 heads as it was made and at every commit or change of the list, shown or not; each head the session did not hold took 466 to 789 ms and one of its 8 read slots, beside the first screen's reads |
| 2026-10-21 | A lost merge is one entry of the session's outcome log, which every review of its branches reads | `review` `a_review_opened_after_the_one_that_merged_closed_starts_from_the_log` (the review that sent a merge answered 504 closed and released, main's history unreadable); `settling_a_lost_merge_in_one_review_lets_another_merge` (a review open since before the merge and one opened after it; the sender's Refresh reads main down to m2 without the merge); `a_lost_merge_keeps_merge_off_until_history_settles_it` and `a_lost_merge_is_looked_for_only_above_the_head_it_was_sent_over`, unchanged in what they assert; `graph_store` `a_merge_whose_deletion_was_refused_says_the_branch_stays` and `the_activity_log_keeps_the_newest_writes`, extended | The entry holds main, `feature`, f2 sent over m2 and the 504 as fields; the review opened after the first closed starts from it alone, reads main's history for f2 and sends neither the merge nor a fix. The sender's read records "not shown" on the entry: the review open since before offers Merge and its refused Revert again, the one opened after takes "not shown" without reading, and its merge is sent. A merge recorded as landed stays so; a lost merge not settled outlives 1,001 later writes, a settled one does not. Before, the review kept a second, per-session record of lost merges beside the log, whose entry named the branches only in its description |
| 2026-10-21 | A lost merge keeps the fixes off too, and a reopened review keeps Merge off | `review` `a_lost_merge_keeps_merge_off_until_history_settles_it`, extended: Revert while main's history cannot be read; a second review of feature into main opened after the lost merge; a Refresh after it settles | Revert is refused ("A merge is running, or may be") and no `/mutate` is sent; the reopened review starts from the lost merge, reads main's history for it, and sends neither the merge nor a fix; settled as not shown, fixes and Merge are offered again, and a Refresh after that forgets it, so a review opened since starts idle. Before, Revert stayed on while the banner said Merge stays off, and a reopened tab offered Merge at once |
| 2026-10-21 | A lost merge is looked for only above the target's head before it | `review` `a_lost_merge_is_looked_for_only_above_the_head_it_was_sent_over` (main of 10,501 commits, its head m10500 when the merge was checked, the merge answered 504 and never landed) | One page of main read, the merge not shown, Merge offered again. Before, 20 pages and "not reached", repeated at every Refresh, so Merge stayed off in that tab for good |
| 2026-10-21 | A new token leaves nothing the old one read | `review` `a_new_credential_reads_the_review_again`, extended: the new token refused the merge base | At once and after the refused read: no review data, cards, rows, scope-menu commits, names, dry run or bound head; the review reads as failed; the person's reviewed mark is kept; allowed again, a scope change reads the merge base. Before, the old token's counts, banners and scope menu stayed, for good after a refused read, and a scope change reused the old token's merge base and commits |
| 2026-10-21 | A lost merge is never sent twice | `review` `a_lost_merge_keeps_merge_off_until_history_settles_it` (a merge answered 504 through a gateway, main's history first unreadable, then readable without the merge) | While the history is read, and while it cannot tell (`Unread`), Merge is off and `merge` sends nothing; a Refresh reads main's history again and no second merge; read to its end without the merge, Merge runs again with "may still land". Before, Merge stayed on under "reading main's history…": a click replaced the merge task, cancelling the read, and sent a second merge |
| 2026-10-21 | A fast-forward of the reviewed head is not said to take more | `review` `a_merge_of_a_head_newer_than_the_review_says_so`, extended to `fast_forward` answers whose commit's merged parent is f2 (the reviewed head) or f3 | `beyond_review` false for f2, true for f3, as for a merge commit. Before, a fast-forward compared the new target commit's own id with the reviewed head, which never match: every fast-forward warned |
| 2026-10-21 | A switch of graph shows none of the previous graph's branch details | `review` `a_switch_of_graph_shows_none_of_the_previous_graphs_details` (graph A's head reads out when the list switches to graph B, whose branches are not read yet) | No details until B's branches are read, then B's main at b1 by act-b1. Before, A's read landed after the switch and named A's head, actor and time on B's rows |
| 2026-10-21 | A credential refused changes reverts and edits nothing | `review` `a_change_the_credential_may_not_make_is_not_offered` (the access route answering `read` and `branch_merge` only) | `RevertAndNext` and `EditChange` not available, `AcceptAndNext` available; `revert`, `revert_line`, `open_edit` send nothing, `save_edit` answers "Your credential may not change feature"; no `/mutate`. Before, the buttons and keys ignored the refusal the access answer held |
| 2026-10-21 | A new token reads the review again | `review` `a_new_credential_reads_the_review_again` (`SessionEvent::CredentialChanged` on a read review holding a name) | At once the review is reading, its names gone and its dry run unread; then the merge base and the feed are read again. Before, the review ignored the event: the old token's cards and names stayed |
| 2026-10-21 | The scope menu draws only the commits in view | `review` `the_scope_menu_builds_only_the_commits_in_view` (10,000 commits since the fork), `the_scope_menu_reviews_the_commit_chosen` | Fewer than 100 rows built of 10,000; Select Last, Next, Previous, First reach every row; ↵ reviews the highlighted commit and closes, and the menu opens again on it. Before, a context menu of one entry per commit was built and drawn at every frame |
| 2026-10-21 | A scope change at the same heads reads no merge base or fork again; the reads it needs overlap | `review` `a_scope_change_at_the_same_heads_reads_no_base_again`; `a_review_reads_the_merge_base_and_the_feed_after_it` (a branch main was merged into) | Choosing f1 reads one page of feature's history beside f1's changes, no merge base; f2 then reads only its changes; "Since the fork" only the feed; the commits are those a fresh read lists; after another writer's commit a scope change reads the merge base again. A branch main was merged into asks for the diff before either history page, and a page of each together. Before, every scope change read the merge base and searched the fork again, the diff waited for that search, and the two histories' pages were read one after the other |
| 2026-10-21 | ⌘R reads the review again | `review` `a_review_read_behind_the_schema_or_the_head_is_newer`, its second Refresh dispatched as `studio::Refresh` with the review focused | The review was read again (requests sent, the head bound as before, "Newer data" as the fake still serves f2). Before, nothing in the review handled `Refresh`: ⌘R reloaded the session at the workspace and the review's "Newer data · Refresh" stayed; its buttons showed no key |
| 2026-10-21 | The branch list draws only the rows in view | `review` `the_branch_list_builds_only_the_rows_in_view` (2,000 branches, a 320 pt dock, rows counted as built) | Fewer than 100 rows built of 2,001, more than none. Before, `BranchList::render` cloned the list and built a row and its right-click menu for every branch each frame |
| 2026-10-21 | A review short of the fork lists the branch's own commits and names the history that stopped it | `review` `a_fork_past_the_history_read_is_said_and_no_history_is_read_whole`, extended: fixtures name `graph_branch` as the server records it (`feature`, `dev`, none for main); a branch that moved 300 commits past the reviewed head; a branch forked from `dev`, whose 10,000 commits are past its pages; `graph_branch` at `7fd9cc9`: `GraphNamespacePublisher` and `GraphHeadExpectation` normalize main to `None`, `LineageIntent.branch` is the writing branch, `GET /commits?branch=` lists the first-parent chain (`commit_list_page`, `first_parent_page`) | Before, a branch main was merged into listed `["f2","fm","f1","m0"]`, main's m0 as the branch's, and every capped review said "More than 10,000 commits since the fork". Now `["f2","fm","f1"]` with "The fork is past the newest 10,000 commits of main's history: … the 3 commits made on feature."; the moved branch says "More than 9,700 commits" (9,900 since its fork, not 10,000); the `dev` fork lists `["f1","f0"]` and "The fork is more than 10,000 commits down feature's history: …". A fork reached is unchanged: its commits since it still include a parent branch's |
| 2026-10-21 | A merge whose deletion was refused is logged as such | `graph_store` `a_merge_whose_deletion_was_refused_says_the_branch_stays` (a merge with deletion answered `branch_deleted: false` and `branch_delete_error_details`, then one answered `true`) | "Merge feature into main; feature was not deleted: feature is protected", the session still on feature; then "Merge feature into main and delete feature", the session on main. Before, both read "…and delete feature", the activity log and the toast saying a branch that stayed was deleted |
| 2026-10-21 | Every write runs through one runner | `graph_store` `GraphSession::run_write` (count, send once, run to the outcome, log, follow-up) in place of six copies in `send_write`, `invoke_stored_write`, `send_blob_write`, `create_branch`, `delete_branch` and `merge`; the session, review, history, type table and query editor tests | All pass unchanged |
| 2026-10-11 | Merge outcomes and conflict kinds in 0.13.0 | parsed `openapi.json` at `v0.13.0` | As listed |
| 2026-10-12 | Edge mutations Studio builds compile on 0.13 | `gq_language` `writes_compile`, `graph_store` `fixes` tests type-check `insert_edge`/`delete_edge` against the 0.13 compiler; `docs/user/mutations/index.md` at `v0.13.0` (`from`/`to` assignments; no edge update; inserts and deletes cannot mix) | Pass |
| 2026-10-12 | Relationship reverts land on the branch | dev server: branch `review-edges` adds `WorksAt person-000000 → company-00003` and deletes `Knows person-000000 → person-000005`; review restored from a workspace file; ⌥⌘Z on both cards; queries on the branch | Two commits, each bound to the head before it; the `Knows` edge is back, the `WorksAt` edge is gone |
| 2026-10-12 | Edit writes only what changed, on the reviewed branch | dev server: `review-edges` retitles `person-000001`; ↵ on its card, " Jr" added to the name, ⏎ | One commit; the branch has "Donald Dijkstra Jr" and the retitled title unchanged; the card reads "Edited in ARZJVH2D" |
| 2026-10-12 | Search narrows the cards | `review` test `search_matches_ids_types_and_values_case_blind` | Ids, type names and before/after values, case-blind |
| 2026-10-11 | Change feed and commit list parameters | parsed `openapi.json` at `v0.13.0` | `/changes`: branch, cursor, start, page_token, limit, kind, type, op, set; `/commits`: branch |
| 2026-10-12 | Folding 20,000 changes stays under 1 s off the main thread | `graph_store` `fold_instrument` (ignored instrument), debug build, Apple M5 Pro, 48 GB: 20,000 changes into 15,000 entities | 175 ms |
| 2026-10-12 | Change-feed paging | live `omnigraph-server` 0.13.0: a terminal page carries `cursor` and `caught_up: false` but no `next_page_token` when more commits wait | `GraphClient::changes` takes no `cursor`, so review continues with `start=after:<last folded commit>` and stops at the reviewed head; a 20,500-change branch shows "Partial" at 20,000 |
| 2026-10-12 | Merge outcomes | live server: `feature-y` merged as a new commit; `feature-z` fast-forwarded; `feature-x` refused with 409 | 409 listed `divergent_update` and `delete_vs_update` with `entity_id`; nothing moved on main |
| 2026-10-12 | Target history after merges | live server: `GET /commits?branch=main` after both merges | Lists main's own chain only; merged branch commits appear only as `merged_parent_commit_id` |
| 2026-10-12 | Fast-forward shape | live server: main's head after fast-forwarding `feature-z` | A new main commit whose `merged_parent_commit_id` is the branch head, not the branch head itself |
| 2026-10-12 | A revert of several properties publishes once | live server, scratch branch: two `update` statements joined by `graph_store::fixes::compose` | One commit, `affected_nodes: 2` |
| 2026-10-12 | Fixes bound to the reviewed head | offscreen snapshot: Revert on `feature-x` `person-000002` through `/mutate/if-graph-commit` | Committed; the value read back as before; the card stays, hollow, "Reverted in …" |
| 2026-10-12 | Merge refuses a moved branch | offscreen snapshot: a second writer committed on `feature-v` after the review was read | "feature-v moved to …"; no merge request sent; main unchanged |
| 2026-10-18 | Relationships are reviewed with the entity they came with | `graph_store` `entity_groups` tests (home rule, mirrors, self relationships, a type the schema lacks, edge types in words); `review` test `a_search_keeps_the_cards_whose_changes_match_and_headers_count_what_shows` | Each change on exactly one card; mirrors name their card; headers count what shows |
| 2026-10-18 | Entity cards render | offscreen snapshot against a scratch dev server on edge `0a62bb7`: branch `review-demo` adds Person `person-900000` with `WorksAt` and `Knows`, retitles `person-000001` and adds its `Contributes`, removes `WorksAt person-000000 → company-00002`, adds `Knows person-000155 → person-000005` | Donald Dijkstra's card carries his new `contributes`; Theo Brandt's carries `works at` and `knows`, mirrored on `person-000000`'s card with the removed `works at` struck through; the scratch server and its data were deleted after |
| 2026-10-20 | No history is read whole: the fork, the commits since it and a merge's confirmation come from bounded pages | `review` tests `a_review_reads_the_merge_base_and_the_feed_after_it` (a branch main was merged into reads one page of each history, `limit=500`), `a_fork_past_the_history_read_is_said_and_no_history_is_read_whole` (a review of one commit on a branch of 10,500 commits since the fork; a branch main was merged into whose fork is past main's newest 10,000), `a_merge_past_the_history_read_is_not_confirmed` (main of 10,501 commits without the merge), `a_merge_is_confirmed_after_more_of_the_source_was_merged`; `graph_store` `ForkSearch` tests (`the_fork_is_the_newest_branch_commit_in_the_target`, `a_held_merge_base_is_the_fork_the_chain_reaches`, `a_branch_the_target_was_merged_into_forks_where_it_began`, `a_branch_merged_into_the_target_forks_at_its_merged_head`, `a_merged_branch_forks_at_its_own_head`), each history read in pages of 1, 2, 3 and 10; against fakes that page `GET /commits` by `limit` and `page_token` as `edge-20261020-7fd9cc9` does (`limit` 1 to 1,000, `crates/graph_client/openapi/omnigraph-0.14.0-edge.json`) and send the contract header | Pass: every listing names a `limit`; past the bound exactly 20 pages of a history are read, none of main's for a fork on the branch's own chain; the note shows and the summary reads "1 entity changed"; the merge reads `NotReached`, "Not confirmed: …" when committed and "The merge outcome is unknown (…): the newest 10,000 commits of main's history do not show it, and Studio read no further. Studio did not retry; open History before merging again." when lost. `GraphSession::commits` and `GraphClient::commits`, the whole-list reads, are gone: nothing else read them |
| 2026-10-20 | A branch starts at a commit with the commit alone, and each refusal reads in words | `graph_store` test `a_branch_starts_at_a_head_or_at_a_commit_never_both`; `review` test `a_branch_at_a_commit_says_why_it_was_refused_and_is_not_sent_again`, against fakes answering with the contract header as Omnigraph `edge-20261020-7fd9cc9` does (`BranchCreateRequest`, `BranchCreateOutput`, `BranchStartRefusalOutput` in `crates/graph_client/openapi/omnigraph-0.14.0-edge.json`) | Pass: the body names `from_commit` and no `from`; the receipt's commit is the head; every reason's notice shows, an unknown one as the server's message, and each start is sent once |

## Corrections

- 2026-10-12: a target's `GET /commits` holds only its own first-parent chain, and a fast-forward
  publishes a new target commit whose merged parent is the branch head. The fork point is the
  newest branch commit that is in the target's list *or* is a merged parent there
  (`graph_store::fork_point`); merge verification and settling an unknown outcome look for the
  merge commit or a commit whose merged parent is the reviewed head (`contains_merge_of`).
- 2026-10-12: relationships cannot be reverted from the review: `gq_language::builder` has no edge
  insert or delete builder. Their Revert is disabled with the reason.
- 2026-10-12: a revert that sets several properties joins the builder's statements into one
  mutation (`graph_store::fixes::compose`, renaming parameters). It belongs in
  `gq_language::builder` next to the builders it composes.
- 2026-10-12: the review's keys (j/k, ↑/↓, ⌘Y, ⌥⌘Z, ⌘⇧↩ for Merge) are bound in `review::init` in
  the `Review` context; they move to `assets/keymaps/` with the next keymap change.
- 2026-10-12: a fresh window focuses nothing, so window-level actions never reached the
  workspace's handlers. `review::init` focuses the workspace when nothing is focused; the fix
  belongs in `studio_workspace`.
- 2026-10-20: the fork of a branch its target was merged into, a fork past 20 pages of the
  branch's history, and a merge's confirmation past 20 pages of the target's read both whole
  lists (every commit since each branch began, main's before the fork included). They read
  bounded pages now and say when the bound stopped them (Deferred, E7); `fork_point` over two
  whole lists became `graph_store::ForkSearch` over pages.
- 2026-10-12: the change feed folds first-parent history, so a branch that merged its target in
  showed the merged-in changes too, each with Revert, which would undo the target's own change
  on merge. Since 2026-10-18 such a review folds the net diff from the merge base (E6) and says
  "Since main was last merged in at …".
- 2026-10-22: "Branches" did not say when a row's head is read, which `26129fd` changed: the
  list read the first 20 heads whether it was shown or not, and now reads only those of the rows
  a shown list draws.
