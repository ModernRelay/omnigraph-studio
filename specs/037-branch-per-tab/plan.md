# 037: The branch belongs to the tab; a protected branch takes proposals

Status: step 1 built 2026-11-19 · proposal 2 of the
[cookbook study](../../docs/ux/2026-11-18-cookbook-study.md) (PR #143). Waits on E63 for the
policy's protected branches.

## Problem

A window has one branch, and every tab in it reads and writes that branch. A review, a query tab
and a page each need their own:

- **Fixing an agent's branch from its review takes a window switch.** Removing one off-topic
  edge during a review (industry intel, S3) meant opening Branches, switching the window to the
  agent's branch, opening the node's page again, pressing × twice, going back to the review and
  pressing Refresh. The review's inspector, and the page opened from it, were held at the
  reviewed commit and read-only.
- **A check on an agent's branch moves everything.** `decisions_at_risk` says "Run it on an
  agent's branch", and the query tab offered only main's head and commits
  ([f04](../../docs/ux/2026-11-18-cookbook-study/f04-company-query-picker-main-only.png)). Alice
  switched the window, which made every tab and every edit target the agent's branch until she
  moved back (company brain, C2). A claim opened from the review showed branch data under a top
  bar that said `main` (C1).
- **One click commits to main.** As `act-writer`, a click on a decision's status enum committed
  straight to `main` ([f11](../../docs/ux/2026-11-18-cookbook-study/f11-pharma-one-click-commit-to-main.png),
  pharma R3). Every cookbook's README says "review on a branch" and every policy declares
  `protected_branches: [main]`, but the access route does not say which branches are protected,
  so Studio cannot tell (E63). Starting a planning or weekly-review branch took a dialog and a
  window switch each time (beads B3, second brain N2).

## Principles

- IV. Every write names its branch: every surface that writes names where the edit lands, which
  on a protected branch is "a new proposal from main" until the proposal exists, and its name
  after. Routing an edit to a proposal is the person's setting, on by default and off with one
  key; it is not a policy Studio enforces: a write the setting sends to `main` is the server's to
  allow, and a proposal the policy will not let the credential start is refused by the server,
  with the draft kept.
- III. A write has three outcomes: a proposal's start is one more write, sent once. Its refusal
  or lost answer sends no edit; the edit's own outcome is reported as it is.
- II. One snapshot per view: a view on a branch of its own pins to that branch's head as any
  view pins to its window's, and offers Refresh when it moves.
- XIII. One rule for what a pick does: amended (v1.6.0). The top bar names the window's branch,
  which a new tab takes; a tab on a branch of its own says so in its own header.
- VIII. Keyboard first: two new actions with bindings, in the palette and the Graph menu.
- X. The engine moves at its own pace: E63 is added to spec 012; Studio treats `main` as
  protected without it.

## What exists today (checked)

- Studio `98c7cea` (origin/main, 2026-11-19):
  - `GraphSession` writes to an explicit branch: `write_on`, `write_properties_on`,
    `write_request`, `invoke_stored_write`, `blob_put_on`, `blob_clear_on`
    (`crates/graph_store/src/session.rs`), each sent once through `run_write`. Drafts are kept by
    branch, heads are watched per branch (`watch_head`), and access is read per branch
    (`read_access`).
  - A read with no commit goes to the session's branch (`GraphSession::read`), and a scope
    already records the branch its view reads (`Scope::branch`, `crates/graph_store/src/scope.rs`).
  - `NodePeek` keeps the branch it reads in `branch`, which follows the session's on
    `BranchChanged`; a few reads and labels still used the session's branch directly
    (`peek.rs` `send_node_read`, `compare_head`, `forget_shown`, the keys and drafts lines).
  - The review's inspector node and the page it opens are held at the reviewed head
    (`ReviewView::selected_node`, `open_selected_page`, `crates/review/src/view.rs`).
  - The query tab's snapshot picker lists "Head of <branch>" and the branch's 15 latest commits
    (`picker_entries`, `crates/query_editor/src/view.rs`; spec 007).
  - A Blob tab already writes to the branch it was saved with (`saved_blob`,
    `crates/type_table/src/type_table.rs`): the one precedent for a tab with a branch of its own.
- Omnigraph `v0.15.0` (contract 0.14), `crates/graph_client/openapi/omnigraph-0.15.0.json`:
  - `AccessOutput` has `actor_id`, `graph_wide`, `branch`, `target_branch`, `actions`,
    `client_id`, and nothing about protected branches. `GET /branches` lists names (details
    with `?detailed=true`), and nothing about protection.
  - `BranchCreateRequest` takes `from` (a branch, its head) or `from_commit` (E21), not both.
  - There is no branch rename route (`/graphs/{id}/branches` has `get` and `post`;
    `/branches/{name}` has `delete`).
  - A policy's `protected_branches: [main]` is in every cookbook's `graph.policy.yaml`; the
    pharma cookbook's lets `act-writer` change and create branches on `branch_scope: any`.

## Design

### A tab carries its branch

A view opened from something on a branch is on that branch: it reads that branch's head, pins
to it (constitution II), writes there, and stays there when the window moves. The window's
branch is the default for new tabs and for every view not opened from one.

- **`GraphSession`.** A scope's read with no commit goes to the scope's own branch
  (`read_in` → `read_from`), which is the session's unless the view put itself elsewhere;
  `invoke_stored_read_on` runs a stored read at a named branch's head.
- **Pages and the inspector (`NodePeek::on_branch`).** A peek or page put on a branch before
  its first read reads that branch's head, watches it (`watch_head`), reads what the credential
  may do there, and ignores the window's `BranchChanged`. Its head comparisons, reset scopes and
  write labels use its own branch. The pages it opens and the related node it selects carry the
  branch (`OpenNodePage.on_branch`, `InspectedNode.branch`, both new, both defaulting to the
  window's).
- **Node page.** `NodePage` takes `on_branch`; its tab is its own per branch
  (`node:<type>:<id>~<branch>`), it is saved and restored with it, and its toolbar shows
  "⑂ on <branch>" with the tooltip "This page reads and edits <branch>, whatever branch the
  window is on". A page asked for on the window's own branch follows the window, as before.
- **Review.** The selected change's node is on the reviewed branch at the reviewed head
  (`snapshot`, not a held commit): the inspector edits it there, with Remove and + on its
  relationships, and Open page opens it there. A node the branch removes stays held at the fork,
  read-only, as before. The review's own head watch sees the fix land and offers Refresh, as it
  does for any commit on the branch.
- **Query tab.** The snapshot picker adds "Read on another branch": the window's branch
  ("main · the window's") and every other listed branch. Choosing one puts the tab on it
  (`QueryEditor::set_on_branch`): reads and stored reads go to that branch's head, its rows
  open in the inspector on it, a write's confirm bar names it, and it is saved with the tab.
  The picker then reads "Head of <branch>", its tooltip "This tab reads and writes <branch>,
  whatever branch the window is on". Choosing a commit still pins it read-only; choosing the
  window's branch follows the window again. The branch list is read when the picker opens and
  none is held.

### Proposals

An edit on a branch the graph protects starts a proposal: a branch of its own, made at the
commit the edit was read at, where the edit lands.

- **Which branches.** `GraphSession::protects(branch)`: until E63 the server cannot say, so
  `main` is protected in a window's session (`open_session`, `open_session_behind` call
  `follow_proposal_setting`) while the graph's `propose_changes` setting is on. It is on by
  default (`studio.propose_changes`, per graph under `graphs.<target>/<graph>`, shown in the
  settings' Editing section). A session made outside a window (a test's) writes where told.
- **Where an edit lands.** `GraphSession::lands_on(branch)` is `Branch(branch)` when it is not
  protected, `Proposal(name)` when the session is on a proposal of it, else
  `NewProposal { from }`. `write_target(branch)` words it: `main`, `proposal/act-writer/1119-1`,
  or "a new proposal from main". Every write surface's line uses it: the node view's keys line
  ("↵ save · esc cancel · write · a new proposal from main"), its relationship search, drafts bar
  and Blob slots, the table cell's keys line and select, `write_scope` in the new-node and edit
  dialogs and the query tab's confirm bar, and the query tab's "Run asks before it writes to …".
- **The first edit.** Every write entry point (`write_on`, `write_properties_on`,
  `write_request` with a branch, `invoke_stored_write`, `blob_put_on`, `blob_clear_on`) goes
  through `GraphSession::routed`. For a new proposal it:
  1. names it `proposal/<actor>/<MMDD>-<n>` (the actor from the access route, made a branch word;
     the date in UTC; `n` the first free in the branch list; `proposal/<MMDD>-<n>` with no actor);
  2. creates it with `from_commit` set to the commit the edit is bound to, or `from: main` for an
     edit bound by value (a property write) or not at all (a Blob write);
  3. sends the edit there, bound to the same commit, so it means on the proposal what it meant
     on `main`;
  4. once it commits (or its outcome is unknown), moves the session onto the proposal: the views
     that follow the window read it, and show the edit.
  Edits sent while the start is out wait for it and go to the same proposal. A start refused
  (403, a name taken) refuses the edit with the server's words and sends nothing; one with no
  answer sends nothing and says the Branches panel lists the proposal if it was made. An edit
  refused on a proposal just started leaves the branch as a spare, which the next edit from the
  same commit takes rather than starting another.
- **Edits offered.** On a protected branch an edit is offered even where the access route
  refused `change` on it (`GraphSession::edit_refusal`): the proposal's start is what is sent
  there, and the server judges it.
- **On a proposal.** `GraphSession::proposal()` is the session's branch when it is named
  `proposal/…`, proposing to `main`, with the changes Studio made on it since the session moved
  there. The top bar's breadcrumb names `main`, then "Proposing on ⑂ proposal/act-writer/1119-1
  · 3 changes", a Review button (⌘⇧R, the existing Review Branch, which reviews the window's
  branch against main) and × (Stop proposing). The breadcrumb's tooltip says edits land on the
  proposal. On `main` while proposals are on, it reads "edits propose" after the branch name, its
  tooltip naming ⌥⌘P.
- **Ending.** Merging the proposal into `main` from its review returns the window to `main`
  (deleting the branch, the default, did already). Stop proposing (⌥⌘⇧P) returns to `main` and
  leaves the branch listed. Switching to another branch leaves the proposal too.
- **Restored.** A window saved on a proposal comes back on it, and proposing, since the state is
  the branch's name; its count starts again at 0 and the chip shows no count until one is made.

### Actions and keys

| Action | macOS | Linux, Windows | Palette, menu |
|---|---|---|---|
| `studio_graph::ToggleProposeChanges` | ⌥⌘P | Ctrl-Alt-P | Graph › Propose Changes to Main |
| `studio_graph::StopProposing` | ⌥⇧⌘P | Ctrl-Alt-Shift-P | Graph › Stop Proposing; offered only on a proposal |

Toggle writes `graphs.<key>.propose_changes` for the window's graph. Review is the existing
`ReviewBranch` (⌘⇧R).

### Polish from the study's appendix (pharma R3)

An enum on a node's page or in the inspector writes on a click: its menu now starts with "Writes
to <where it lands>", and its row has no hover tooltip, which covered the menu.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The session moves onto the proposal once its first edit lands | Keep the window on `main` and send every edit to the proposal while views read `main` | Views would show `main` without the person's own edits. On the proposal every view shows them, and the breadcrumb still names `main` |
| Move after the edit lands, not when the branch is made | Move at the branch's creation | Views would read the proposal at its start, without the edit, and then call the edit's commit newer data |
| The proposal starts at the commit the edit is bound to | Start at `main`'s head and send the edit unbound | The edit keeps its `If-Graph-Commit` meaning: it lands only on what it was read against |
| A proposal is any `proposal/…` branch, to `main` | Keep proposal state in Studio | Studio keeps no state about a branch the server does not; a window restored on it proposes again |
| Edits offered on a protected branch whatever `change` says there | Disable them as read only | The credential may well start a proposal (company brain's agents may not write `main` but may branch); the server decides, and the draft stays on a refusal |
| A refused first edit keeps its branch as a spare | Delete the empty branch | Deleting is a second write Studio would make on its own; the spare is taken by the next edit |
| `main` protected by default, the person's setting per graph | Off by default; protected only when the policy says | The study: every cookbook means "review on a branch", and E63 is not served. A person who writes `main` on purpose turns it off once per graph |
| Pages, the inspector and the query tab carry a branch in step 1 | Every view (tables, trees, canvases) at once | Those three are the study's workflows (S3, C1, C2, R3); a table on a branch of its own needs its counts, filters and live reads per branch (spec 028) |
| Reuse ⌘⇧R for the proposal's review | A new Review Proposal action | Review Branch already reviews the window's branch against main, which on a proposal is the proposal |

## Deferred

- **E63** ([#1473](https://github.com/ModernRelay/omnigraph/issues/1473)): which branches the
  policy protects, in `AccessOutput` or the branch list. With it, `protects` asks the server, and
  the setting only turns proposals off. Listed in spec 012 and `docs/deferred-engine-changes.md`.
- **Renaming a proposal.** No rename route; a rename would be a branch made at the proposal's
  head and the old one deleted, two writes. Later.
- **"← main: upcoming" beside a changed value.** The study's sketch shows `main`'s value next to
  the proposal's on a page. It needs a second read per page at `main`'s head; later, with the
  review's three sides (proposal 3, E53).
- **Tables, trees and canvases on a branch of their own.** They follow the window. Their view
  strip from a page on a branch opens them on the window's branch.
- **Drafts typed on `main` before the proposal started.** The edit that starts the proposal
  lands there; other unsaved values typed on `main` meanwhile wait on `main`, as a branch's
  drafts do. Moving them with the window needs the save that is out to settle on the branch it
  was typed on first.
- **A pinned view's edit after the move.** A view held on `main` (not following the window)
  that edits once the session is on the proposal sends its edit there bound to the `main`
  commit it read; once the proposal has moved, the server refuses it (412) and the draft stays.
- **A proposal from a branch other than `main`.** Waits on E63.
- **Typed "Propose changes" per window.** The setting is per graph; a window-only toggle was not
  needed by any workflow walked.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | The first edit on `main` starts `proposal/<actor>/<MMDD>-1` at the edit's commit and lands there bound to it; later edits go to it with no second start; Stop returns to `main` | `graph_store` `session::proposal::tests::the_first_edit_on_main_starts_a_proposal_and_lands_there` (requests recorded by the fake server) | Passed: create `from_commit: c7`, then the mutation on the proposal with `If-Graph-Commit: c7`, then two unbound mutations on it, no second create; changes counted 3 |
| 2026-11-19 | A start the policy refuses sends no edit | `…::a_refused_start_sends_nothing` (403 on `POST /branches`) | Passed: refused with the server's words, no mutation sent, session on `main` |
| 2026-11-19 | Without proposals an edit writes `main` | `…::without_proposals_an_edit_writes_main` | Passed |
| 2026-11-19 | A proposal's name is its actor and UTC day | `…::a_proposal_is_named_by_its_actor_and_day` | Passed (`1119`, `0101`, `0229` for 2024-02-29; `user:ada@example.com` → `user-ada-example.com`) |
| 2026-11-19 | A page on a branch of its own stays there when the window moves, opens pages there, and writes there | `type_table` `peek::tests::a_page_on_a_branch_of_its_own_edits_there_and_stays` | Passed: the write's body names `agent/x` after the window moved to `fix` |
| 2026-11-19 | The review's inspector node is on the reviewed branch at the reviewed head; a removed node is held at the fork | `review` `the_inspector_shows_the_selected_changes_node_where_the_review_reads_it` | Passed |
| 2026-11-19 | A query tab on a branch of its own reads there and stays; back on the window's it follows it | `query_editor` `a_tab_on_a_branch_of_its_own_reads_there` (request bodies recorded) | Passed |
| 2026-11-19 | The top bar says "edits propose" on `main` even where `change` is refused, names a proposal with its review, and reads "read only" again once proposals are off | `studio_workspace` `the_bar_says_edits_propose_and_names_the_proposal` | Passed |
| 2026-11-19 | No view this adds is costly to draw | The proposal chip and the page's branch chip are a few elements in bars already drawn per frame; no new list or grid | No draw-count guard added (`docs/drawing.md`) |
| 2026-11-19 | A table keeps its cursor when the window moves onto a proposal of its branch | `type_table` `the_cursor_stays_on_its_row_when_the_window_moves_onto_a_proposal` | Passed; a move to another branch still starts the table afresh |
| 2026-11-19 | It works against a real `v0.15.0` server on a cookbook graph | `omnigraph-server` `v0.15.0` (release build, linux x86_64, sha256 checked), the pharma-intel cookbook (omnigraph-cookbooks `ce31d40`) applied and its seed loaded (313 entities), Studio debug build under Xvfb 1600×1000 as `act-writer`, driven with `xdotool`; every server state read back with curl | R3: `dec-vanquish-interim-readout`'s status menu read "Writes to a new proposal from main"; choosing `decided` made `proposal/act-writer/1119-1` and committed there (main still `upcoming`, the proposal `decided`); the bar read "Proposing on … · 1 change · Review"; the table kept its row and the inspector showed `decided`; Review → Merge fast-forwarded main, deleted the proposal and put the window back on main. S3/R2: an agent branch `ci/orforglipron-approval` with a signal linked to two compounds; its review's inspector removed the wrong `aboutCompound` edge in two presses, committed to the branch with the window on main (the branch then held one link, main none); the page opened from the review read "on ci/orforglipron-approval". C2: a query tab put on the agent's branch from its picker read the branch's new signal first. The first run, before the table kept its cursor, showed the inspector emptied by the move; fixed and run again |

## Corrections

- 2026-11-19, from review of PR #150 (Devin, Cursor Bugbot): an own-branch view still used the
  window's branch in places, and two quick edits could split.
  - A peek on its own branch read and typed drafts on the window's branch; they are its own
    branch's now (`draft_of`, `node_drafts`, `put_draft`).
  - The Blob and text editors it opens wrote the window's branch; `OpenBlob` and
    `OpenTextEditor` carry `on_branch`, and the text editor stays there when the window moves.
  - Its relationship search read the window's head; it searches at its own branch's head.
  - A query tab's row note opened nodes on the window's branch; it carries the tab's.
  - A stored-query tab restored with both a branch and a pin lost the pin; the pin is put back
    after the branch.
  - An edit sent after a proposal was made but before its first edit landed started a second
    proposal; such an edit joins the one opening (`opening_proposal`), and its change counts.
    Guarded by `edits_sent_together_go_to_one_proposal`. An edit that joins it bound to the
    commit it was read at on `main` (a list, a relationship replaced) is refused once the first
    edit has moved the proposal (412), and its draft stays to be saved again there: the binding
    is what keeps it from writing over a value that changed. Property edits are bound by value
    and join without it.
  - A text editor opened from a page on its own branch followed the window when the window was
    on that branch too; it keeps the page's branch.
