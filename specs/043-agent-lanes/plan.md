# 043: Agent work as lanes

Status: built 2026-11-19 · the cookbook study's proposal "Agent work as lanes: compare branches,
see who opened them, see when they are merged" (`docs/ux/2026-11-18-cookbook-study.md`, its
seventh ranked proposal; PR #143).

## Problem

In five of the eight cookbooks, agents propose on branches of their own and people review them.
The study found three gaps in how Studio shows that work:

- **Two agents answer one question differently** (company C4, program P2, spike S4). The notes
  agent proposes a successor launch date of Nov 20, the Slack agent Nov 27. A review compares a
  branch only with its target ("Since the fork", or one commit), so the two proposals could only
  be read one after the other, from memory (friction log: "company | C4 compare two agents | no
  branch-against-branch review").
- **The branch list does not say what a branch is.** A merged branch that was kept
  (`tentative/scout-helix`, `agent/a`) looks like a live one (vcos V4, screenshot f06). Agents'
  branches (`agent/`, `tentative/`, `capture/`, `intake/`, `ci/`) are mixed in with people's, and
  nothing says whose each one is or how much work is on it.
- **A new branch shows late** (beads B5). An agent's branch appeared only at the next 30 s poll,
  and the outside agent's `graph_open_branch` answered "not a branch" until then.

## Principles

- **II (one snapshot per view).** A comparison reads both heads once and pins its diff to them.
  When a compared branch moves, it offers Refresh; a compared commit never moves.
- **IV (every write names its branch).** A comparison writes nothing: Edit, Revert and Merge are
  refused with the reason, and their keys are not registered. Delete on a merged row opens the
  delete dialog, which reads the merge base again and confirms with ↵, since nothing is lost.
- **V (nothing moves under you).** Groups come from the names alone, or, by author, from one
  detailed list read before the rows are drawn. A lane's mark lands in place and changes no row's
  position. A lane read again to the same answer does not draw the list.
- **VI, VIII.** Every new control shows its key; Compare, Refresh, the grouping and Swap are
  actions with bindings, in the palette.
- **IX (work is bounded).** Lanes are read only for the rows drawn, one page of 100 commits at
  most; past it a row says "100+ ahead". A comparison is capped as a review is (20,000 changes,
  "Partial").
- **X.** Designed against contract 0.14 as `v0.15.0` serves it. Grouping by author needs the
  detailed branch list (`ServerFeature::DetailedBranchList`, 0.15); on a 0.14 server the toggle
  is disabled with the reason, and the list groups by prefix.
- **XIII.** A comparison is a tab of two things. It has no view strip (no one branch's other
  views follow it) and names itself "Compare".

## What exists today (checked)

- Studio (`main` at `98c7cea`):
  - `crates/review/src/branches.rs`: the Branches panel is a `uniform_list` of every branch in
    name order, `main` first (`graph_store::session::branch_order`). A drawn row's head is read
    after the frame (`note_drawn`, `read_drawn`); from a 0.15 server one detailed list answers
    them all (`GraphSession::head_commit`, `detailed_branches`).
  - `DeleteBranchModal` already reads the merge base with `main` and deletes a merged branch with
    ↵, any other by typing its name.
  - `crates/review/src/view.rs`: `ReviewView` takes any `target`, reads the merge base, then the
    branch's feed after it, or `GET /diff` from the base when the feed refuses it
    (`read_review_since_merge`). A review of `agent/b` against `agent/a` shows what `agent/b`
    did since the two forked, not how the two differ.
  - `crates/ui_tool_server/src/tools.rs` `check`: `Need::Branch` refuses a branch the session's
    last list does not hold.
- Omnigraph `v0.15.0` (`crates/graph_client/openapi/omnigraph-0.15.0.json`):
  - `GET /graphs/{id}/diff?from=&to=` (E6): "a branch name (its head now) or a graph commit
    id" on each side; `DiffOutput` names both commits it resolved.
  - `GET /branches/merge-base?source=&target=` answers the base and both heads in one read.
  - `GET /branches?detailed=true` (E9): `BranchDetailOutput { name, head: CommitOutput,
    updated_at, created_at, from, from_commit_id }`. No creator (E60).
  - `CommitOutput.graph_branch` is the branch a commit was written on, `None` for main.
  - CLI: `omnigraph diff --from <a> --to <b>` reads the same net changes.

## Design

### Branches panel: lanes

- **Grouping** (`studio.branch_grouping`, "Group branches by" in Settings › Browsing; ⌘⌥⇧G
  `studio_graph::ToggleBranchGrouping`, and the panel's title-line button):
  - **Prefix** (default). `main` first; then each branch whose prefix (the part of its name
    before the first `/`) no other branch shares, by name; then one group for each prefix two or
    more branches share, by prefix (`agent/`, `tentative/`). A grouped branch is named without
    its prefix ("notes-oct-14") and inset under its header; its tooltip names it whole.
  - **Author**. `main` first; then one group per author of each branch's newest commit, by
    name, every branch in one, "Author not known" last (a branch the server did not detail). It
    needs every head at once, so it is offered only where the server details its branches
    (Omnigraph 0.15); elsewhere the button is disabled with "Grouping by author needs a server
    that lists every branch's head (Omnigraph 0.15 or later)". While that list is read the panel
    says "Reading who wrote each branch…" and draws no rows, so none moves to its group as heads
    land. Until E60, the author is who wrote the newest commit, and the group's tooltip says
    so: "Branches whose newest commit act-notes-bot wrote".
  - A group's header is a row of the list (the same height, so the list stays uniform): a
    chevron, the prefix or author, and its count. A click folds or shows its branches, kept per
    window while the graph is shown; ⌘⌥⇧K (`studio_graph::FoldBranchGroups`) folds every group,
    or shows them all when every one is folded, and the header's tooltip names the key.
  - By author, a branch's author is the detailed list's, unless a head the panel read since (a
    commit seen on the branch) is newer: then that head's writer, and the branch moves to its
    group when that head is read. A new credential forgets the details the old one read
    (`GraphSession::forget_reads`), and the panel forgets its rows' heads and lanes, so no
    group names a writer the new credential may not read.
- **Lanes.** For each branch row drawn other than `main`, after the frame: its merge base with
  `main`. When the base is the branch's head, the row is **merged**: its name dims, a `merged`
  tag replaces its time, and **Delete** replaces Switch (disabled with the refusal when the
  credential may not delete it). Delete opens the delete dialog, which reads the merge base again
  and confirms with ↵. Otherwise one page of the branch's history (100 commits) is counted down
  to the base, or to the first commit written on main (which main holds), and the row says
  "3 ahead" before its time; a full page with more says "100+ ahead". The row's tooltip adds
  "merged into main" or "3 commits not in main". A lane that cannot be read leaves the row
  unmarked. Lanes are read again when the branch commits (`SessionEvent::Committed`), every
  drawn lane when main moves (a commit or a head seen at the poll), and at Refresh; the marks
  shown stay until the new answers land.
- **Refresh** (⌘⌥⇧B `studio_graph::RefreshBranches`, the title line's ↻): forgets every
  branch head the session holds (`GraphSession::refresh_branches`) and reads the list again;
  the drawn rows read their heads (from a 0.15 server, in one detailed read) and lanes again.
- **Compare with…** in a row's menu, and the title line's compare button (⌘⌥⇧R
  `studio_graph::CompareBranches`, for the window's branch).

### Compare

- **The dialog** ("Compare <branch> with…"): a field that narrows the other branches (every
  word typed, case-blind), a list of them that scrolls (8 rows shown), ↑ ↓ to move and ↵ to
  compare. Text that names no branch is read as a commit id: the list says "No branch is named
  <text>: it is read as a commit id." and the button reads "Compare with commit". The button's
  tooltip shows `omnigraph diff --from <a> --to <b>`. Nothing is written.
- **The tab** (`studio_graph::OpenCompare { from, to }`; saved as `compare` and opened again
  with the window): a review tab in its read-only mode (`ReviewView::compare`), with the
  review's cards, search, Feed and Split, Mark reviewed, and the inspector. Its title is
  "Compare agent/notes-oct-14 · agent/slack-oct-14"; its kicker "Compare", its heading
  "agent/notes-oct-14 ↔ agent/slack-oct-14".
  - **Read.** Two branches: their merge base (one read: both heads and the commit they share),
    then `GET /diff` from the first's head to the second's, page by page to the review's cap.
    Any other pair (a commit side, or sides the branch list has not said yet, as for a tab
    opened again with the window before the list is read) is named to `GET /diff` as it is,
    since the merge base takes branches only; its first page names the commits it resolved, which
    the comparison shows and the later pages' token holds to. A side is a branch once the list
    holds it, and is watched from then: a branch made after the list the comparison opened with
    is taken for a commit, then for a branch once a list holds it. A compared branch deleted
    since stays named as a branch, is not opened again with the window, and is read again by
    name, never through the merge base. A diff names no commit, so each difference is the
    second head's.
  - **Words.** A card's kind is relative to the first side: + only on the second, − only on
    the first, ~ differs, as the facts line says ("+ only on agent/slack-oct-14 · − only on
    agent/notes-oct-14 · ~ differs"). The facts give each side's commit ("agent/notes-oct-14 at
    8F6WX93") and the commit both hold ("both from 4K2…"). The summary reads "3 entities and
    4 relationships differ"; identical sides read "They hold the same data" and the body
    "<a> and <b> hold the same data." The feed's sections read "On <first>" and "Only on
    <second>".
  - **Read-only.** Edit, every Revert, Merge and Summarize are not offered. The cards' actions
    are disabled with "A comparison is read-only: change <second> from its own review", and
    their keys are not registered. No dry run, no merge strip, no scope menu.
  - **Swap** (⇧⌘X `studio_review::SwapSides`, the header's Swap chip): the sides change places
    and the comparison is read again in the same tab; reviewed marks are of the other direction
    and are cleared.
  - **Newer data.** A compared branch whose head moves offers "Newer data: <a> or <b> changed
    after this comparison was read." with Refresh, whichever side it is, and also when it moved
    while the comparison was being read.
- The Effect view of the study's proposal 1 lands in the review; a comparison is a review tab,
  so it has it where proposal 1 builds it for every review tab.

### An agent naming a new branch

A UI tool that names a branch (`graph_open_branch`, `graph_open_branch_history`) the window's
last list does not hold reads the list once, as Refresh does, and waits for it (at most the
tools' `WAIT`, 15 s) before it judges the call. A branch still not listed is refused as before.
`ui_tool_server` sends no request itself: it asks the session (`GraphSession::reload_branches`,
`branches_reading`).

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| A comparison is the review tab in a read-only mode | A tab of its own in `history`, as the commit view; a review of one branch against the other as target | The study asks for "the review's cards and Effect view": one surface keeps search, layouts, marks and proposal 1's Effect. A review against another branch as target shows what one did since the fork, not how the two differ |
| Read-only, no Revert to "take the other side" | Revert restores the first side's value on the second branch | A diff between two moved branches is not a branch's own change: reverting it would write the other agent's whole entity, unbound to what the person read. Proposal 3's resolution (E61) is where taking a side belongs |
| A side not listed as a branch is a commit id | A separate commit picker | `GET /diff` takes either; the branch list says which. History already opens a commit; a pasted id is enough for the rare case |
| Group by prefix by default, author as a choice | Both at once (prefix groups inside author groups); a filter | The study's prefixes (`agent/`, `tentative/`, `capture/`, `intake/`, `ci/`) are how agents name their work today, known from names alone and stable. Two levels cost a row of indentation the sidebar does not have |
| A prefix shared by one branch is not a group | Every prefixed branch grouped | A header over one row adds a row and hides nothing |
| Author = newest commit's actor, on 0.15 only | Read every branch's head on 0.14 (thousands of reads); author of the oldest own commit (a walk per branch) | One detailed read answers it; the honest label says what it is until E60 gives the creator |
| Lanes for drawn rows only, two reads each | Read every branch's standing at once | A graph may hold thousands of branches (spec 008's list draws only rows in view); E66 would make it free |
| "Ahead" counts down to the merge base or the first commit written on main | Down to the merge base only | A branch main was merged into reaches its base only through a merged parent; the first-parent walk would pass it and count main's own history. A commit written on main is in main |
| Merged rows offer Delete in place of Switch | A menu entry only | The study: "marks a branch merged … offering Delete (↵, since nothing is lost)". Switching to a merged branch is rare; its menu still has it |

## Deferred

- **E60** ([#1470](https://github.com/ModernRelay/omnigraph/issues/1470)): the actor that
  created a branch, in the detailed list. With it, grouping by author groups by creator, and a
  0.14 server keeps today's behaviour. A server feature (`graph_store::FEATURES`) when it ships.
- **E66** (spec 012, not filed): a branch's standing against a target in its details, so lanes
  cost no read of their own.
- **E35**: a diff between two branches that both moved reads both tables, so a comparison of
  two agents' branches on a large graph takes as long as a merged-in review does today. The
  review's slow-read words and its five-minute wait apply.
- Proposal 1's Effect view, in the review tab a comparison is.
- A branch's own commits as a lane in History (the study's "lanes" as drawn there already
  exist for merged-in branches, spec 009).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | The branch list lays out by prefix and by author as designed | `cargo test -p review --lib branches::tests::branches_group` | By prefix: `main`, `fix-titles`, `plan/sharing-v2`, `agent/ (2)` with `notes-oct-14` and `slack-oct-14`, `tentative/ (2)`, `New branch…`; folded, the header stays. By author: `act-alice (1)`, `act-notes-bot (2)`, `Author not known (1)` last |
| 2026-11-19 | "Ahead" stops at the base or at main's first commit; a full page says more | `branches::tests::a_branch_is_ahead_by_its_commits_down_to_main` | 3 ahead for a branch of a branch; 1 after a merge at s2; `more` on a full page |
| 2026-11-19 | Lanes are read for drawn rows only, again when main moves, and answers alike draw nothing | `branches::tests::drawn_branches_are_marked_merged_or_ahead_and_read_again_when_main_moves` | `feature` merged, `spike` 2 ahead, no lane for `main`; 2 merge-base reads, no history read for the merged branch; a commit to main: 4 merge-base reads, and the lane landings asked for no draw (2 before, 2 after) |
| 2026-11-19 | Refresh reads the list, the drawn heads and lanes again | `branches::tests::refresh_reads_the_branches_their_heads_and_lanes_again` | One more `GET /branches`, the heads read again, two more merge-base reads |
| 2026-11-19 | Grouped, the list builds only the rows in view | `branches::tests::a_grouped_list_builds_only_the_rows_in_view` (`docs/drawing.md`, Guards) | 2,000 branches under four prefixes: 2,005 rows laid out, fewer than 100 built in a 320 pt dock, fewer than 100 lanes read |
| 2026-11-19 | A comparison reads one merge base and one diff between the heads, and writes nothing | `view::tests::a_comparison_reads_one_diff_between_two_heads_and_changes_nothing` | `merge-base` once, `GET /diff?from=a1&to=b1` once, no feed, no dry run; Edit, Revert and Merge refused with the read-only words; no footer, no view strip; saved as `compare` `{from, to}`; swapped, `GET /diff?from=b1&to=a1` |
| 2026-11-19 | A commit side is named to the diff as it is | `view::tests::a_comparison_with_a_commit_names_both_sides_to_the_diff` | No merge-base; `GET /diff?from=a1&to=agent%2Fb`; the heads shown are the ones the first page resolved; the commit is named by its short id |
| 2026-11-19 | A comparison made before the branch list is read learns its sides | `view::tests::a_comparison_made_before_the_branches_are_read_learns_its_sides` | Both sides unknown, nothing watched, read by name; once the list is read, both branches, both watched |
| 2026-11-19 | The merge base takes branches only | `curl` against the `v0.15.0` server: `GET /branches/merge-base?source=<commit id>&target=agent/notes-oct-14`, and `GET /diff?from=agent/notes-oct-14&to=<commit id>` | 500 `internal error while reading changes`; the diff answers, naming both commits it resolved. A comparison restored before its list was read sent the first and failed, in the running app: corrected the same day (below) |
| 2026-11-19 | The dialog narrows the branches and reads other text as a commit | `compare::tests::the_dialog_narrows_the_branches_and_reads_other_text_as_a_commit` | `main`, `agent/slack-oct-14`, `plan/q4` offered (not the branch compared from); ↓ moves; "SLACK oct" leaves one; an id is picked as typed; the branch itself is not |
| 2026-11-19 | Grouping by author is offered where the server details branches | `branches::tests::grouping_by_author_waits_for_one_detailed_list_where_it_is_served` | On a 0.14 server the list stays grouped by prefix and asks for no detailed list; with 0.15's features, one `detailed=true` read, groups `act-notes-bot` and `act-slack-bot` |
| 2026-11-19 | An agent's branch made since the last list opens | `studio` `ui_tools_tests::graph_open_branch_reads_the_branches_for_one_just_made` | `agent/new`, served after the window's list: opened as Review; `agent/none`: `not_available` |

Checked in the running app on 2026-11-19, as the study did: a debug build of this branch under
Xvfb (1600×1000, Mesa lavapipe) on a 2-core Linux box, against `omnigraph-server` `v0.15.0`
serving the company-brain cookbook (seed loaded with `load --mode append`) behind the
cookbooks' TLS gateway; the agents wrote through the CLI with their own targets
(`local-notes` as `act-notes-bot`, `local-slack` as `act-slack-bot`), Alice reviewed with her
token (`act-alice`). The notes agent ran the README's walk on `agent/notes-oct-14` (a meeting,
a claim, a contradiction, a successor for Nov 20); the Slack agent a rival on
`agent/slack-oct-14` (Nov 27); `agent/notes-oct-07` was merged by Alice and kept;
`plan/q4-launch` is Alice's.

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | The panel shows the agents' work as lanes | Branches panel, grouped by prefix; the same graph in `main`'s build for the before | Before: five flat rows, the merged `agent/notes-oct-07` reading as live. After: `main`; `plan/q4-launch` 1 ahead; `agent/` (3): `notes-oct-07` dimmed, `merged`, Delete; `notes-oct-14` 4 ahead; `slack-oct-14` 4 ahead. A row's tooltip: "agent/notes-oct-14 · Head 69T7SP95 · act-notes-bot · 4 min ago · made 4 min ago from main · 4 commits not in main" |
| 2026-11-19 | Grouping by author reads one detailed list | The title line's person button | `act-alice` (1), `act-notes-bot` (2: the merged one and the live one), `act-slack-bot` (1); back to prefix with the same button |
| 2026-11-19 | Two agents' rival proposals read side by side | "Compare with…" on `agent/notes-oct-14`, "slack", ↵; then the search "checkout-launch" | One tab, "Compare agent/notes-oct-14 · agent/slack-oct-14": "Launch Checkout v2 on Nov 20" only on the notes branch, "… on Nov 27" only on the Slack branch, each source, claim and edge; Revert disabled; the inspector shows the selected decision at its side's head; "both from XXAYM5WM" |
| 2026-11-19 | A branch an agent just made shows at Refresh | `agent/slack-oct-21` made with the CLI as `act-slack-bot` while Studio ran | Not listed until the panel's ↻; listed at once after it, 1 ahead. `main`'s build showed it only at the next poll |
| 2026-11-19 | A merged branch is deleted with ↵ | Delete on `agent/notes-oct-07` | "Everything on agent/notes-oct-07 is in main, so deleting it loses nothing." with no name to type; ↵ deleted it |
| 2026-11-19 | A comparison with a commit, and both tabs again after a restart | "Compare with…", the seed commit's id typed; Studio quit and started again | "Compare agent/notes-oct-14 · XXAYM5WM": the notes agent's 3 nodes and 6 edges, only on the branch (the README's "3 nodes and 6 edges"). After the restart both comparisons were open again and read |

| 2026-11-19 | The review's findings on #153 hold, and are fixed | Devin Review and Cursor Bugbot on the first push; `branches::tests::every_group_folds_and_shows_from_the_keyboard`, `view::tests::a_compared_side_listed_later_is_a_branch_and_one_deleted_is_read_by_name`, `graph_store` `session::tests` (the detailed-list test: `forget_reads` leaves no detail) | Folded and shown by the key; a side listed later becomes a watched branch, a deleted one is read by name with no merge-base; a new credential leaves no branch detail |

## Corrections

- 2026-11-19, before merge: a comparison decided its sides from the branch list as it was made,
  taking every side for a branch while no list was held. Restored with the window before the
  list was read, a comparison with a commit sent the commit to `GET /branches/merge-base`, which
  `v0.15.0` answers with a 500. A side is now unknown until the list says, nothing is watched
  meanwhile, and only two known branches are read through the merge base; any other pair is
  named to `GET /diff` as it is.
- 2026-11-19, from the review of #153: a branch taken for a commit stayed one when a later
  list held it; a deleted compared branch was still read through the merge base; a move of the
  side compared from during a read offered no Refresh; grouping by author kept a writer the
  detailed list named after a newer head was read, and kept the old credential's details after
  a new token; groups folded only by pointer. Each is fixed as the Design says.
