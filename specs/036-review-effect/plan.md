# 036: What a branch does to the graph's own questions

Status: built 2026-11-19 · proposal 1 of the cookbook study
([docs/ux/2026-11-18-cookbook-study.md](../../docs/ux/2026-11-18-cookbook-study.md), PR #143).
The graph declaring its own checks waits on E64.

## Problem

Every cookbook keeps "is the graph healthy" and "what matters now" as stored reads: beads'
`ready` queue, company-brain's nine hygiene checks, the hardware program's `budget_rollup` and
design-review gates, the development graph's `unheld_invariants`. Every reviewer of an agent's
branch asks what the branch does to those reads. The review answers which rows the branch
changed, never what that does to the reads:

- beads B4: closing `lk-acl` makes three issues ready. The review shows a status change, and
  the `ready` tab, refreshed after the merge, marks none of its new rows (study f03).
- company C2: `decisions_at_risk` says "run it on an agent's branch"; the query tab offers only
  main's head and commits (f04). Alice switched the whole window to the branch and compared
  from memory. The nine hygiene checks run nowhere before Merge.
- program P2: the review shows `p-mass-optics` 24.5 → 21.4; the engineer wants to know whether
  `dc-pl-mass` now closes (f08). P4: the cookbook checks its gates on an unpublished merge
  before publishing it; Studio cannot.
- dev D4: the review said "Merges cleanly" for a branch adding an owner edge main already had;
  after the merge the issue had the same owner twice.

## Principles

- II: every read of the Effect is pinned to a commit (each branch's head, or the held merge);
  a head that moves offers Refresh.
- III: the unpublished merge, its publish and its drop are writes sent once each, never again
  by themselves; a lost answer says to look at History.
- IV: every write names its branch; the buttons follow the access route
  (`branch_merge`, `commit_publish`, `commit_drop`), and a 403 shows as a refusal.
- VI, VIII: every control says what it does, its CLI line and its key; every action is in
  `studio_actions`, bound, and in the palette.
- IX: reads are bounded (four at once, each answer within the client's 32 MiB cap, at most
  5,000 rows of a side compared, 2,000 relationships checked, 50 rows of each kind listed).
- X: the graph cannot say which reads are checks (E64); the person marks them.
- XIII: the Effect has reads of its own, so it is a tab, a view of the branch beside its
  review and its history, not a toggle in the review.

## What exists today (checked)

- A stored read runs at a commit: `InvokeStoredQueryRequest.snapshot` (`omnigraph-0.15.0.json`,
  "Snapshot id to read from (read queries only …)"); `GraphSession::invoke_stored_read` takes
  it. Checked live: `ready` at main's head answered 8 rows, at `plan/sharing-v2`'s 10.
- A merge `as_commit` leaves the target's head alone and answers the unpublished commit
  (`BranchMergeRequest.as_commit`, R2); a stored read answers at that commit; `POST
  /commits/{id}/publish` publishes it; `DELETE /commits/{id}` drops it with a 204 and no body.
  All four checked against a `v0.15.0` server (validation log). `graph_client` had none of
  them before this plan.
- `GET /diff` takes `kind` and `op` filters (`omnigraph-0.15.0.json`, operation `diff`); an
  edge change carries its ends as `endpoints.from` and `endpoints.to`.
- An edge type without `@key(@src, @dst, …)` keeps every insert of the same ends as another
  edge ("without a key, repeated endpoint pairs remain distinct edges", the `omnigraph` skill's
  `references/schema.md`); `gq_language::EdgeType` did not say whether a type had a key.
- The access route answers `commit_publish` and `commit_drop` per branch
  (`AccessAction::CommitPublish`, `CommitDrop`). Beads' policy grants neither to anyone, so
  there `act-admin` may merge `as_commit` and may not drop the result (404, "not found");
  the hardware program's grants both to its writers.
- Cedar in 0.15 grants actions per graph and branch, never per type (`references/server-policy.md`,
  the action table), and the engine refuses an edge to a missing node (spec 012, E54's
  validation). An added edge's other end is therefore always readable to a reader of the branch.
- The catalog (`QueryCatalogEntry`) gives a stored query's name, description, instruction,
  `mutation` and parameters, never its file, tags or text (E43, E64).
- Saved views of stored queries keep their parameter values (`ViewQuery`, spec 024).
- The review holds its reviewed head (`ReviewView::bound_head`) and merges only from it
  (spec 008); the session keeps one entity per open graph and shares it with every view.

## Design

### Marks

A person marks a stored read for the graph's reviews as **watched** (a queue, a roll-up: the
rows it gains and loses matter) or a **check** (it should return nothing). The mark is kept in
the settings, per graph: `graphs.<target/graph>.effect`, a map from the read's key to
`"watched"` or `"check"`. The key is the stored read's name, or `view:<id>` for a saved view of
a stored query, which runs with the view's values, so a read with parameters can be watched
through a view.

Where a read is marked:

- The Queries panel's row menu for a stored read without required parameters: "Watch in
  reviews" and "Use as a check in reviews", each ticked when it is the mark; choosing the mark
  it has takes it away. The row's end says "watched" or "check".
- A stored read's tab: ⌘⌥E (`studio_effect::ToggleWatched`) and ⌘⌥K
  (`studio_effect::ToggleCheck`), registered only on a tab that can be marked; the toolbar
  shows a "watched in reviews" or "check in reviews" tag.
- The Effect tab's **Reads** menu (⌘⌥R, `studio_effect::PickReads`): every stored read without
  required parameters and every saved view of a stored query, each a submenu of "Watch: show the
  rows it gains and loses", "Check: it should return nothing" and "Leave out", the current one
  ticked. The menu stays open while marks change.

A read the catalog no longer lists, one that writes, or one needing parameters stays in the
list, greyed, saying why.

### The Effect (graph_store)

`graph_store::effect::Effect`, one per reviewed pair of branches, shared through the session
(`GraphSession::effect`), so the review's merge strip and the Effect tab read the same answers.

- Heads: one `GET /branches/merge-base` gives the branch's head, the target's head and the
  merge base. Every read runs as a stored read at those two commits (`snapshot`), at most
  [`READS_AT_ONCE`] (4) out at once. A view that wants only the checks (the review) asks for
  only those; the tab asks for all.
- Row diff (`graph_store::row_diff`): rows are matched by the column that names each row once
  on both sides: the first column holding whole nodes (by `@id`), else the first named `@id`,
  `id`, `slug`, `key` or `*_id`, else the first column, each only when every row on both sides
  holds a text there and no two rows of a side hold the same. Matched rows whose other cells
  differ are *changed* ("~ dc-pl-mass allocated 30.8 → 27.7 · violated true → false").
  Without such a column rows match whole, each row of a side used once. At most 5,000 rows of a
  side are compared ("Compared on the first 5,000 rows of each side."). The work runs off the
  main thread.
- A check *holds* the merge when it has rows at the branch head. `Effect::hold(head)` answers
  `Clear`, `Unread` (not read at that head), `Reading(n)` or `Rows(names)`.
- A head that moves after the reads marks the Effect `moved`: the tab offers "Newer data ·
  Refresh" and reads nothing by itself (constitution II).
- When the target moved since the fork (`merge_base ≠ target head`) the tab says that a row the
  target gained since reads as removed, and points to the merge result.

### Studio's own check

"Relationship already on main" runs on every review of a graph with an edge type without a
key. It pages through `GET /diff?from=<merge base>&to=<branch head>&kind=edge&op=insert` (1,000 a
page, the first 2,000 relationships), keeps the types without a key, and asks the target's head
for edges of each type between the same ends (`builder::edges_between`, 500 pairs a read, 5,000
edges an answer). Each pair the target has, or the branch adds more than once, is a row:
relationship, from, to, and "on main", "added 2 times here" or both. A merge keeps both edges,
so this is the dev D4 duplicate. Past 2,000 the row says "Studio checked the first 2,000
relationships the branch added".

The study's other two built-in checks are not built: "a new node with required-by-convention
fields left empty" needs proposal 5's convention; "a new edge whose other end is not readable"
cannot fire on 0.15 (What exists today).

### The Effect tab (review crate)

A view of the branch, `ViewKind::Effect`, opened from the view strip of the review and the
history (Review · History · Effect), from the palette and the Graph menu ("Branch Effect", ⌘⇧E,
`studio_effect::OpenEffect`, the window's branch against main), and from the review's "Show
effect". Tab title "Effect <branch> → main", saved and restored with the window.

```
Review  History  Effect                                           [Reads ▾]  ⟳
plan/sharing-v2 → main
⑂ main at S82HRWCZ   ⑂ plan/sharing-v2 at 1293TMH5
───────────────────────────────────────────────────────────────────────────────
Checks                                  main      plan/sharing-v2     Moved
▸ ⚠ Relationship already on main  Studio's check   —         1 row
▸ ✓ unsupported_decisions                0 rows    0 rows             same
Watched
▾ 👁 ready                               8 rows    10 rows            +3 −1
    + lk-share   Shared notebooks   epic   1
    + lk-share-api   Invite endpoint for shared notebooks   feature   2
    + lk-share-ui   Share dialog   feature   2
    − lk-acl   Per-notebook access control lists   task   1
───────────────────────────────────────────────────────────────────────────────
Check the merge result: merge as an unpublished commit, run these   [Check the merge result]
reads on it, then publish it onto main or drop it.
```

- Rows: chevron, the state (⚠ a check with rows, ✓ a clean check, a spinner while read, ✗ a
  failure, 👁 a watched read), the label (a view's name, "Studio's check" tag for the built-in
  one), the row count at each head, and what moved ("+3 −1 ~1", "same"). A check's count with
  rows is in the warning colour. A row's tooltip is the read's description, or why it does not
  run.
- Unfolded (click, Space): the rows the branch adds (+, created colour), removes (−) and
  changes (~ with each changed column before → after), at most 50 of each kind with "and N
  more"; a failure in words; the built-in check's rows.
- Keys: J/K and the arrows move among the reads, Space unfolds, ⌘R reads again, ⌘⌥R the Reads
  menu, ⌘⌥M checks the merge result, ⌘⇧↵ publishes it, ⌘⌥⌫ drops it.
- Empty: "No read is marked for this graph's reviews", with why the marks are the person's
  (E64) and a Pick reads button.
- The status bar says "1 check has rows" or "3 reads", and lists the tab's keys.

### The merge result (R2)

"Check the merge result" (⌘⌥M) merges the branch `as_commit`. The target does not move. Every
runnable read then runs at the unpublished commit too, a third column ("Merge result") and,
unfolded, "main → the merge result" beside "main → this branch". The footer says "Merge result
<id> is held, unpublished: 1 check has rows on it. main has not moved." with **Drop** (⌘⌥⌫,
`omnigraph commit drop <id>`) and **Publish onto main** (⌘⇧↵, `omnigraph commit publish
<id>`). Outcomes:

- Published: "Published <id> onto main." The session's write log makes it main's head.
- Dropped: "Dropped the merge result; main is as it was."
- Already merged: "<branch> is already in main: there is no merge to check."
- Refused publish (main moved, so the commit expired): "The server refused to publish the
  merge result: … It is still held; drop it, then check again."
- Refused merge or drop: "The server refused to <merge|drop> it: …. Nothing changed."
- Lost answer: "The answer to the <step> was lost (…). Studio did not send it again; open
  History to see whether it landed." Nothing is offered until a Refresh.

The button is offered only when the access route does not refuse `branch_merge` into the target
and `commit_drop` on it: a held commit the credential could not drop would stay on the server
("Your credential may not drop an unpublished commit on main, so a merge result Studio held
could not be let go"). Publish is disabled with the refusal when `commit_publish` is refused.
The branch is never deleted by this path (`as_commit` refuses `delete_branch`).

### The review's merge strip

The review asks the Effect for the checks at its reviewed head once the whole branch is read
(not for one commit's scope). The merge strip leads with the checks' line when one holds:
"⚠ 1 check has rows on plan/sharing-v2: unsourced_signals  [Show effect]". The first Merge
press (button or ⌘⇧↵) only arms the merge: "… Merge again to merge anyway."; the second
merges. While checks are still read the line says "2 checks running…", and a first press says
"2 checks are still running. Merge again to merge without waiting.". Checks not read at the
reviewed head (another head) hold nothing; the merge's own head check refuses a moved branch
anyway (spec 008).

### Query tabs

A query tab that runs the same read again (Run, ⌘R, Refresh after "Newer data"; same text or
stored query, same values, same page) compares the new rows with the last ones, off the main
thread, as the Effect does. A row new to the answer reads "+12" in its number gutter, in the
created colour; a changed row "~3", in the modified colour. The footer adds "+4 −1 since the
last run" or "Same rows as the last run". A first run, a write, a statement or a plan compares
nothing.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Effect is a tab, a view of the branch | A third segment inside the review | Constitution XIII: it has reads, a selection and writes of its own; the study's sketch put it beside Review and History, which are already tabs of the branch |
| One `Effect` per branch pair in `graph_store`, shared | The review and the tab each read the checks | The merge strip and the tab must agree on what holds the merge; one read set, two readers |
| Marks in the settings, per graph, by stored name or saved view | Tags in the graph (E64); a marks file beside `cluster.yaml` | The catalog has no tags today and Studio never writes `cluster.yaml` (spec 024); views already carry parameter values, so a read with parameters is marked through a view |
| Rows matched by a naming column, else whole | Always whole rows; ask the person for a key | A changed roll-up row should read as one change ("~ dc-pl-mass …"), not a removal and an addition. Without E43 the column names are all there is; the rule takes a column only when it names every row once on both sides, so it never matches two rows to one |
| A check holds Merge behind one more press | Disable Merge; nothing | A check is the graph's advice, not policy (constitution IV: Cedar decides); a conflict does not block a merge today either. One more press makes the person see it |
| Checks still read also take one more press | Let Merge run while they read | A press while the checks read would merge past a check that may fail; the second press is the person's decision to go without them |
| The merge result is offered only when the credential may drop it | Offer it whenever merge is allowed | Beads lets an admin merge `as_commit` but no one drop it; Studio would leave an unpublished commit it cannot let go |
| The built-in duplicate-edge check always runs where a type has no key | A mark to turn it on | It finds a defect a merge cannot undo (dev D4) and costs one filtered diff and a read a type |
| Comparison with the target's head, not the merge base | Compare with the fork | The reviewer's question is "what will main's reads say", and the merge result answers it exactly; a target that moved is said in a notice |
| The query tab's branch picker is left to proposal 2 | Offer branches in the query tab's commit picker here | Proposal 2 moves the branch to the tab; the Effect answers C2's "run it on the agent's branch" without it |

## Deferred

- **E64** ([#1474](https://github.com/ModernRelay/omnigraph/issues/1474)): a stored query's
  source file and tags in its catalog entry, so a graph declares its checks once for every
  client (`@tag("check")`). Until then the person marks them, per graph.
- **E43**: a stored query's result columns with their variables would let the row diff match by
  the node a row names exactly instead of by column names.
- **E35**: a diff across a merge reads both tables, so the built-in check's diff is slow on a
  large graph whose branch merged its target in.
- **E48**: a precondition scoped to what a write touches would keep a held merge result
  publishable while unrelated commits land on the target; today any commit to main expires it.
- The study's "required-by-convention fields left empty" check waits on proposal 5's convention.
- A view of a type (filters, not a stored query) cannot be marked: it would run Studio's own
  query text, not the graph's question.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | A stored read runs at a branch head, at main's head and at an unpublished merge | `omnigraph-server` `v0.15.0` (release tarball, sha256 checked) serving the eight cookbooks as one cluster (study Appendix C); `POST /graphs/beads/queries/ready` with `{"snapshot": …}`, `act-writer` | main's head 8 rows; `plan/sharing-v2` 10 (lk-share, lk-share-api, lk-share-ui in, lk-acl out); the `as_commit` merge `hb1.…01M7S8A49QMRVFXDHBHPSYQ1QA` 10, its `target.snapshot` echoing the commit |
| 2026-11-19 | A merge `as_commit` is held and the target does not move; drop answers 204 | `POST /graphs/program/branches/merge {"source":"trade-optics-light","target":"main","as_commit":true}` as `act-writer`; `budget_rollup` at the answer's commit; `DELETE /graphs/program/commits/<id>` | Outcome `fast_forward`, `commit.kind: "unpublished"`, `published: false`; `budget_rollup` there: `dc-pl-mass` 27.7, `violated: false` (main 30.8, true); drop `204 No Content` with the contract header and no body |
| 2026-11-19 | Beads lets an admin merge `as_commit` but nobody drop it | Same server; `GET /graphs/beads/access?branch=main` as `act-admin`; `DELETE /commits/<id>` as `act-admin` and `act-writer` | Access lists `branch_merge` and neither `commit_publish` nor `commit_drop`; both drops answer 404 `not_found`, `effects: none` (a denial reads as an unknown id). The commit stays unpublished on that scratch server |
| 2026-11-19 | A diff filters to inserted edges and names their ends | `GET /graphs/dev/diff?from=<merge base>&to=<review/dup-owner head>&kind=edge&op=insert` | One `IssueOwnedBy` insert, `after.endpoints: {"from": "iss-safari-login", "to": "act-priya"}`, no node changes |
| 2026-11-19 | Row matching, the counts, the cap | `graph_store` `row_diff::tests` (a queue's rows in and out, a roll-up's cells moved, whole-row matching with repeats, a node column by `@id`, the 5,000-row cap, cells by column) | Pass |
| 2026-11-19 | The Effect compares both heads, finds the duplicate edge, and checks the merge result | `review` `effect::tests::the_effect_compares_both_heads_and_checks_the_merge_result` (fake server) | Built-in check first with one row, "on main, and added 2 times here"; `ready` +1 −1; `hold(f2)` is `Rows`, `hold(f1)` `Unread`; merged once with `as_commit` and without `delete_branch`; the reads at `u1`; published once |
| 2026-11-19 | A check with rows holds Merge behind one more press | `review` `view::tests::a_check_with_rows_holds_merge_behind_one_more_press` | With a row at `f2`: the first press sends no merge, the second sends one; with none, the first press merges |
| 2026-11-19 | The running app against the cookbooks | Studio's offscreen tool (`crates/studio/examples/snapshot.rs`, debug build, Linux, lavapipe) on the same `v0.15.0` server as `act-writer` (company as `act-alice`); marks made with ⌘⌥E and ⌘⌥K in stored-query tabs, which wrote `graphs."environment@http://127.0.0.1:8787/beads".effect` | beads `plan/sharing-v2`: `ready` 8 → 10, "+3 −1" (lk-share, lk-share-api, lk-share-ui in, lk-acl out), `blocked` "+1 −1", the check `blocked_by_ancestor` 2 → 0, Check the merge result disabled (beads grants `branch_merge` to admins only). program `trade-optics-light`: `budget_rollup` "~2" (`dc-pl-mass` allocated 30.8 → 27.7, violated true → false; `req-lv-mass` 142.6 → 139.5), the merge result held, the same at it, then dropped, main unmoved. dev `review/dup-owner`: Studio's check, 1 row `IssueOwnedBy iss-safari-login → act-priya: on main`, under a dry run that says "Merges cleanly … as a fast-forward"; the first Merge press armed it. company `agent/notes-oct-14`: `decisions_at_risk` 0 → 1. A beads `ready` tab run, `close` of lk-acl by the CLI, run again: "+3 −1 since the last run", rows +3, +5, +6 |
| 2026-11-19 | A stored read's tab took no key before its first rows | The same tool: a fresh `ready` tab, `studio_query::Run` dispatched | Nothing ran: the tab's focus is its result grid's, which is drawn only with rows. The result area now holds that focus whatever it shows, so ⌘↵, ⌘⌥E and ⌘⌥K work on a fresh tab (study B1) |
| 2026-11-19 | The Effect's list draws once a key | `review` `effect::tests::moving_through_the_reads_draws_the_list_once_a_key` | J/K and Space draw the cached list once each (`docs/drawing.md`) |

## Corrections

None yet.
