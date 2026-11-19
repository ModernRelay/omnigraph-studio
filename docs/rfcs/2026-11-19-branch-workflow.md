---
rfc: "2026-11-19-branch-workflow"
title: "Branch workflow"
track: maintainer
status: draft
implementation: not-started
authors:
  - aaltshuler
created: 2026-11-19
updated: 2026-11-19
discussion: null
supersedes: []
superseded_by: []
blocked_on: []
---

# RFC: Branch workflow

**Prototypes:** [Branch workflow, today and proposed](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26).
One pair of frames for each surface below. Add `#surface-1` to `#surface-7` to the link to open
one surface, or `#open-question` for the choice this RFC leaves open.

Studio has had no RFC before. This one follows the shape of
[Omnigraph's RFCs](https://github.com/ModernRelay/omnigraph/blob/main/docs/rfcs/README.md), with
the [constitution](../../.specify/memory/constitution.md) in the place of the engine's invariants.
Once accepted, the design moves to a plan under `specs/` with the next free number. The plan
carries the exact keys, copy, limits and the validation log, as every spec does.

## Summary

One rule decides where a window reads and writes:

> **A window reads any branch. It writes to one, and the top bar names it.**

Three things follow from it.

1. **One write target.** The top bar's branch is where this window writes. Every editor, menu and
   dialog repeats it where the person saves, in the same words.
2. **Looking is read-only.** A tab that shows another branch, or an earlier commit, cannot edit.
   It says why, and offers the step that can.
3. **A review fixes its branch.** A review's fixes land on the branch it reviews, and each one
   says so. This is the one write a window makes off its own branch.

Seven surfaces change so that the rule can be seen: the top bar, the saves that do not name their
branch, the Branches list, read-only views, the query tab's read picker, the merge strip and the
conflict card. None adds a panel, a view or state that Studio owns.

Not changed: the engine, the target of a review and a merge (`main`), the three outcomes of a
write, and how a branch is made, deleted, reviewed and merged.

One question is open: whether the write target belongs to the window, as above, or to each tab.
See [Unresolved questions](#unresolved-questions).

## Motivation

A change reaches `main` through a branch, a review and a merge, whether a person or an agent made
it (specs [008](../../specs/008-branches-review-merge/plan.md) and
[033](../../specs/033-diff-ux/plan.md), job J1). Studio has every step. What it lacks is what a
person needs to know between the steps: where this window writes, how a branch stands against
`main`, why a view will not edit, and what to do about a conflict.

Observed on Studio at `98c7cead` against a local Omnigraph 0.15.0, on 2026-11-19:

| # | Today | What a person cannot tell |
|---|---|---|
| 1 | The top bar names the branch and says nothing about it | Whether the branch is ahead of `main`, already merged or empty, without opening its review |
| 2 | Five writes save without naming the branch first: the inspector's value menu and its on/off switch, a relationship's Remove, a Blob's Clear value, and the review's Revert | Where the save goes, at the moment of saving (article IV) |
| 3 | Every row of the Branches list looks alike | Which branches are done and which hold work |
| 4 | An inspector or page opened from a review is held at the reviewed commit and cannot edit. It shows a commit id and no reason | Why it will not edit, and how to edit that node (article VI) |
| 5 | A query tab reads the window's branch and its commits only | How to run a read on another branch without moving the window there |
| 6 | With a conflict known, the filled button is still "Merge into main" | That the next step is the conflict, not the merge (article VII) |
| 7 | A conflict card gives a kind, an id and the server's message | What each branch did to the node, and what clears the conflict (spec 033, job J4) |

### Prior work

Nine pull requests built the proposals of the cookbook study (#143), each from the same `main`.
Five of them changed how branches work:

| PR | Title |
|---|---|
| #147 | the branch belongs to the tab; a protected branch takes proposals |
| #150 | a tab carries its branch, and an edit on main starts a proposal |
| #151 | conflicts show three sides and a resolution |
| #153 | agent work as lanes: compare branches, group them, mark them merged |
| #148 | what a branch does to the graph's own questions |

They changed the same files and gave different answers to "where does a save go". All nine were
closed on 2026-11-19 so that the branch workflow is decided once, in one place. This RFC covers the
branch parts of those five. The other four (#145, #146, #152, #154) are about how a change, a
node's history, a stored mutation and a page are shown. They are out of scope here.

### The test this design holds itself to

Each surface is specified for every state it can be in, and not for one walk through it. The
states that must work: a person working alone, a graph with no agents, a branch with no commits, a
branch already merged, a branch the credential may not change, a read that failed, and a server
on 0.14. Each section below has a table of states for that reason.

## User-visible behavior

### The loop, and where each step lives

| Step | Where | What happens |
|---|---|---|
| Start | Top bar or Branches | New branch, by name. The window moves because the person asked |
| Work | Top bar | The bar shows the branch and how far it is from `main` |
| Review | Center tab | ⌘⇧R, or a click in Branches. The window stays where it is |
| Settle | In the review | A conflict shows both sides. A fix writes to the reviewed branch |
| Merge | In the review | Merge into `main`, delete the branch, and the window returns to `main` |

Every step exists today. The surfaces below fill in what each step does not yet say.

### 1. The top bar says how the branch stands

[Prototype](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#surface-1)

Off `main`, the bar adds one fact after the branch name, in the place `read only` has today. The
fact opens the branch's review, which ⌘⇧R already does.

| State | The bar shows after the branch name |
|---|---|
| On `main` | Nothing |
| The branch has commits `main` lacks | `8 ahead of main · Review ⌘⇧R` |
| More than one page of them | `100+ ahead of main · Review ⌘⇧R` |
| No commits of its own | `no commits yet` |
| Every commit is already in `main` | `merged into main` |
| The read is out | Room for the fact, empty |
| The read failed | `Review ⌘⇧R` alone. A count is never guessed |
| The credential may not change the branch | `You can read main, not change it`, with the reason in the tooltip as today, and `New branch…` when the server's access answer allows `branch_create` |

Room for the fact is held from the moment the window is on a branch other than `main`, so its
arrival moves nothing to its right (article V).

### 2. Every save names its branch before it writes

[Prototype](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#surface-2)

Most saves already do. A typed edit, the table's value menu, an inspector field, and adding or
replacing a relationship end their hint line with `write · <branch>`. Dialogs carry
`<server> › <graph> › <branch>`. Five writes do not:

| Write | Today | Proposed |
|---|---|---|
| The inspector's value menu | A pick saves at once. No branch is shown | The menu opens with the line `A pick writes to <branch>`, as the table's menu has |
| The inspector's on/off switch | A click saves at once. No branch is shown | While the switch has focus or the pointer, its row shows `space switch · write · <branch>` |
| A relationship's Remove | The first press asks, the second removes. The tooltip names the relationship, not the branch | Once asked, the row shows `press again to remove · write · <branch>` |
| A Blob's Clear value | Clears at once from the key hints | The hint reads `Clear value · write · <branch>` |
| The review's Revert | The branch is in the tooltip only | The card's action row shows `write · <branch>` beside Edit and Revert |

The review's line matters most. With the window on `main` and a review of `q4-hires` open, the top
bar says `main` and Revert writes to `q4-hires`.

### 3. The Branches list tells live from done

[Prototype](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#surface-3)

| State | The row shows | Known from |
|---|---|---|
| The window's branch | `current`, as today | The session |
| No commits of its own | `no commits yet` | Its head equals `from_commit_id` in the detailed list (`DetailedBranchList`, 0.15) |
| Every commit is in `main` | `merged into main` | Its head is a commit on the newest page of `main`'s history, or the merged parent of one. One read for the whole list |
| Otherwise | Who last wrote and when, as today | The detailed list |

Switch keeps its place on every row, and a click on a row still opens its review. Deleting a
merged branch is already confirmed with ↵ (article IV).

Two limits, stated on purpose. From a 0.14 server the list has no `from_commit_id`, so
`no commits yet` is not shown: Studio does not guess from a missing field. A branch merged longer
ago than one page of `main`'s history shows no state.

### 4. A read-only view says why, and what would edit

[Prototype](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#surface-4)

A table held at a commit already says `Read only · <commit>`. An inspector or a page in that state
shows only a commit id. All four views gain one line with the same words, and one step.

| Why the view cannot edit | The line | The step offered |
|---|---|---|
| Opened from a review | `Read-only: q4-hires as reviewed, at MJ6HMZA2` | Open q4-hires in a new window |
| Held at a commit, from History | `Read-only: main at 7CAVF299` | Open at the branch head |
| It shows a branch the window has left | `Read-only: q4-hires. This window writes to main` | Open q4-hires in a new window |
| The credential may not change the branch | `Read-only: you can read main, not change it` | New branch…, when allowed |
| The branch was deleted | `Read-only: q4-hires was deleted` | None |

"Open in a new window" leaves this window alone (spec 020: looking at a branch is not a move to
it). Switching the window stays where it is today: the top bar, Switch in the Branches list and
⌘⇧B.

A text editor with unsaved text and a Blob view already keep, and save to, the branch they were
opened on after the window moves. They protect unsaved work, so they stay as they are and gain
the same line.

### 5. A query can read another branch

[Prototype](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#surface-5)

The read picker offers the head of the window's branch and that branch's commits. It gains a
section, `Read another branch`, with the branches most recently written to and `Find a branch…`.
A stored read can then run on an agent's branch while the window stays on `main`.

| State | Behavior |
|---|---|
| The tab reads another branch | The tab says `Reads q4-hires`, where `Read only · <commit>` is shown today |
| A mutation is run there | It does not run. `This tab reads q4-hires. The window writes to main, so a mutation does not run here.` |
| The window changes branch | The tab keeps the branch it reads. It is part of the tab's identity (spec 020, read context) |
| The branch moves ahead | The next run reads its new head, and the result names its commit as today (article II) |
| The branch is deleted | The next run shows the server's answer. The last result stays, with its commit |

### 6. The merge strip leads with what you can do

[Prototype](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#surface-6)

| What the dry run said | The strip | Filled button |
|---|---|---|
| `Merges cleanly into main` | As today | Merge into main |
| Conflicts | `1 conflict stops a merge into main.` | Show conflict. Merge is outlined |
| `Already merged into main` | As today | As today |
| Not read yet, or the read failed | As today | Merge into main |

Merge stays available beside a known conflict. The dry run is a read of an earlier moment, and
the merge itself decides.

### 7. A conflict shows both sides and the one fix there is

[Prototype](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#surface-7)

The card gains the values: for each property that differs, what it held at the fork, what the
branch holds and what `main` holds now. The first two are in the review's diff already. The third
is one read of the node at `main`'s head for each conflict drawn.

On Omnigraph 0.15.0 a conflict is per node, not per property (see
[Evidence](#evidence-and-tests)). The only fix that clears it from the source branch is to give
the branch `main`'s version of that node. The card says so and offers it:

- **Take main's version** writes `main`'s values for that node to the reviewed branch, bound to
  the reviewed head as Edit and Revert are.
- Before it runs, the card says what it drops: `That drops "title: CTO" from the branch; you can
  set it again after the merge.`
- Studio holds the branch's values, so the write is confirmed with ↵ and its outcome offers Undo
  (article IV).

| Kind of conflict | The card shows | Fix offered |
|---|---|---|
| Changed on both branches | Three values for each property that differs | Take main's version |
| Added on both with different values | Both sides | None until it is measured. Edit on the branch |
| Deleted on one, changed on the other | Both sides, and which side deleted | None until it is measured. Edit or Revert on the branch |
| A relationship to a deleted node, a shared unique value, too many relationships, a value that breaks a constraint | Today's words | Today's: fix on the branch |

"Keep this branch's" is not offered. The server cannot do it today, and writing to `main` outside
a merge to imitate it would change the target before anyone merged.

## Design

**State.** Studio gains no state of its own (article X). `GraphSession.branch` stays the window's
write target, one session per graph per window. A tab's read context is already "the session and
branch, and either head or a held commit" (spec 020). Surface 5 lets a query tab's read context
name another branch at its head. It is saved with the tab, as a held commit is.

**Two lines, worded once.** `write · <branch>` is formatted in six places today. It becomes one
function in `studio_ui::ds`, used by every save in surface 2. The read-only line of surface 4 is a
second one, taking a reason and a step. A view passes the reason as a fact and does not word it.

**Reads added.** Each is bounded (article IX), and none repeats per row.

| Surface | Read | Bound |
|---|---|---|
| 1 | The merge base of the window's branch and `main`, and the commits since it: the reads a review already makes | One page, once per window, shared with an open review of that branch |
| 3 | The newest page of `main`'s history | One read for the whole list |
| 5 | The query's own | As today |
| 7 | The node at `main`'s head | One for each conflict drawn |

**Server features.** `no commits yet` asks `features.has(ServerFeature::DetailedBranchList)`.
Nothing is decided by a version check or by a missing field.

**Actions.** One new action, `OpenBranchInNewWindow`, in `studio_actions`, with a default binding
and a palette entry. Today `NewWindow` opens a window and asks for a graph, so opening one on the
same server and graph at a named branch is new work. `SwitchBranch` (⌘⇧B) and `ReviewBranch`
(⌘⇧R) stay as they are.

**Policy.** Whether a branch takes edits comes from the server: `GET /graphs/{g}/access?branch=`
lists the actions the credential has there. Studio never decides that `main` is protected
(article IV).

## Principles

No article is amended by the recommended design. The alternative in
[Unresolved questions](#unresolved-questions) amends article XIII.

| Article | How the design holds it |
|---|---|
| II. One snapshot per view | A tab off the window's branch reads one branch at one commit, and names it |
| III. A write has three outcomes | No write path changes. "Take main's version" is a write like Revert, with the same outcomes |
| IV. Every write names its branch; Cedar decides | Surface 2 closes the five gaps. Read-only comes from the server's access answer. "Take main's version" says what it drops, is confirmed with ↵ and offers Undo |
| V. Nothing moves under you | The window's branch changes only where the person says so. Room for the top bar's fact is held. Switch keeps its place |
| VI. Every control explains itself | Surface 4: a view that cannot edit says why, and names the step |
| VII. Loudness matches cost | Surface 6: the filled button is the next useful step |
| VIII. Keyboard first | The new action has a binding. Review keeps ⌘⇧R |
| IX. Work is bounded | The table of reads above |
| X. The engine moves at its own pace | Designed against 0.14 and 0.15 as they are. Needs are listed under Compatibility and wait in spec 012 |
| XII. Evidence over assumption | The server's merge behavior was measured before the conflict card was designed |
| XIII. One rule for what a pick does | The top still says which server, graph and branch. A tab off that branch is a view of it and never a second place to write |

## Compatibility and reversibility

**Servers.** Nothing needs an engine change. On a 0.14 server `no commits yet` is not shown;
everything else works, since the merge base, the dry run and the access answer are there.

**Saved state.** One addition: a query tab that reads another branch saves that branch's name. A
saved tab whose branch is gone opens on the window's branch and says so, as a window whose branch
is gone falls back to `main` today.

**Reverting.** Each step in [Rollout](#rollout) lands alone and can be reverted alone. Studio
writes nothing new to a graph except "Take main's version", which goes through the mutation
endpoint Revert uses.

**Engine needs.** None blocks this RFC. On acceptance they go to
[spec 012](../../specs/012-omnigraph-engine-changes/plan.md):

| Need | Effect here |
|---|---|
| E53, a conflict's base, source and target images (already listed) | Removes surface 7's read of `main` |
| E28, a merge bound to the reviewed source head (already listed) | Unchanged by this RFC |
| New: a merge that can keep the source's version of a node in conflict, or that merges per property | Makes "Keep this branch's" possible |
| New: how far each branch is from its target, and whether it is merged, in the detailed list | Removes the reads of surfaces 1 and 3 |

## Alternatives

**A branch per tab.** What #147 and #150 began: a page, an inspector or a query tab carries a
branch of its own and writes to it. Everything stays in one window, and a fix found in a review
is made in the next tab. The costs: one window then holds several write targets, so "where does
this save go" has an answer per tab; each table, tree and canvas needs counts and live reads per
branch, where a session holds one branch today; and article XIII's "the top says which branch"
stops being true and must be amended. This is the open question below.

**Do nothing, or land the closed PRs one after another.** They conflict in the same files, and
two of them set the write target in different ways. Landing them in order would decide this
RFC's question by merge order.

**An edit on `main` starts a branch by itself.** It moves the window without being asked
(article V, spec 020). It also has Studio enforce a rule the server did not state: whether `main`
takes edits is the policy's answer (article IV). Surface 1 covers the need: when the server
refuses edits, the bar says so and offers `New branch…`.

**"Keep this branch's", done by writing to `main`.** It changes the target outside a merge, with
no review of that write. It waits for the engine.

**Checks on stored reads before a merge.** The graph cannot yet declare which reads are checks, so
each person would mark them by hand, per graph. Surface 5 covers the need by hand: run the read
on the branch.

**Grouping branches by name prefix.** It depends on how one team names its branches. The list's
search and surface 3's states do not.

## Evidence and tests

**Measured** on a local Omnigraph 0.15.0, 2026-11-19, on a development graph with branches made
for it (`q4-hires`, 8 commits ahead with one conflict; `fix-title`, merged and kept;
`scratch-empty`, no commits):

| Question | Result |
|---|---|
| Is a conflict per property or per node? | Per node. The branch changed `email` and `main` changed `title` of one person: `divergent_update` |
| What clears it from the source branch? | The branch holding `main`'s values for that node. The dry run then answered `merged` |
| Does taking one of `main`'s values clear it? | No. The branch took `main`'s `title` and kept its own `email`: still a conflict |
| Does merging `main` into the branch avoid it? | No. It conflicts the same way |
| Can Studio learn from the server that a branch takes no edits? | Yes. `GET /access?branch=` lists `change` or leaves it out, per branch |
| Can the list tell a branch with no commits? | Yes on 0.15: its head equals its `from_commit_id` |
| Can the list tell a merged branch from one read? | Yes. For both merges tried, the branch's head is the `merged_parent_commit_id` of a commit on `main`, and the merge base equals the head |

**Checked in the code** at `98c7cead`, and again at `e112c795` after #142 merged, where none of it
had changed: the window's branch and what a switch does
(`graph_store/src/session.rs`, `set_branch`); drafts kept per branch across a switch; the top bar's
`read only` (`studio_workspace/src/studio_workspace.rs`, `branch-read-only`); which saves name
their branch (`studio_ui/src/modal.rs` `write_target`, `type_table/src/cell_select.rs`,
`table.rs`, `peek.rs`) and the five that do not (`peek.rs` value menu, switch and `render_remove`;
`table.rs` `on_clear_blob`; `review/src/view.rs` Revert); the read picker
(`query_editor/src/view.rs`); the merge strip and its words (`review/src/view.rs`); the conflict
kinds (`review/src/conflicts.rs`); the fixed target (`review/src/branches.rs`, `TARGET`).

**Not checked yet.** Each is a gate for the step that needs it.

- Two windows on one graph, each on a different branch. Step 3 depends on it.
- Conflicts of the other kinds, and conflicts on relationships. Until measured, their cards offer
  no one-step fix.
- A 0.14 server. The fallbacks above are read from the code, not run.
- The cost of the top bar's read on a large graph. It reads commits, not a diff, so E35 should not
  apply; this is to be timed on the 230,000-person seed.
- The "Today" frames in the prototypes are redrawn from screenshots by hand.

**Tests to add.** For each surface, `#[gpui::test]` cases over its table of states, with
`FakeHttpClient` behind the session. A snapshot of each surface in each state. A draw-count test
for the top bar's fact, which sits in a view that redraws often (`docs/drawing.md`). A test that
every write surface shows the shared line, so a new one cannot leave it out.

## Rollout

Four steps. Each can ship alone, and what the later ones add stays absent until then.

| Step | Surfaces | New reads | Ships |
|---|---|---|---|
| 1. Words | 2 and 6 | None | The five saves name their branch; the merge strip leads with the conflict |
| 2. Standing | 1 and 3 | One page of commits for the window's branch; one page of `main`'s history for the list | The top bar's fact and the list's two states |
| 3. Looking | 4 and 5 | None beyond a query's own | Read-only views that say why, a window opened on a branch, queries on another branch |
| 4. Conflicts | 7 | One read of the node on `main` for each conflict drawn | Both sides on the card, and "Take main's version" |

Step 1 does not depend on the open question. Steps 3 and 4 do.

## Unresolved questions

1. **Does the write target belong to the window or to the tab?**
   [Both, drawn](https://claude.ai/artifact/QtcQ5TvVvQ5xRx9v7CBF26#open-question). This RFC
   recommends the window. Spec 020 already says a view off the window's branch is held and
   read-only, so nothing is amended, and work on a second branch happens in a second window
   beside the first. The cost is that a fix beyond the review's own buttons takes a new window. A
   branch per tab keeps everything in one window at the costs listed under Alternatives. Surfaces
   4 and 5 are written for the window; under the other answer they become "this tab writes to
   q4-hires" in place of "read-only".
2. **Does "Take main's version" ship before the engine can keep the branch's side?** It drops the
   branch's change to that node, which a person may find worse than editing by hand. Recommended:
   yes. It is the only fix measured to clear the conflict, it says what it drops, and Undo puts it
   back.
3. **Is one page of `main`'s history enough for `merged into main`?** A branch merged longer ago
   shows no state. Recommended: yes, until the detailed list can say it.

## Decision log

- 2026-11-19: Drafted. The nine cookbook-study PRs were closed the same day in favor of one
  design. The server's conflict and merge behavior was measured on Omnigraph 0.15.0, and today's
  surfaces were checked in the code at `98c7cead` and again at `e112c795`.
