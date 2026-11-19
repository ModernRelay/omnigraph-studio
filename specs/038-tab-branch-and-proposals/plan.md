# 038: The branch belongs to the tab; a protected branch takes proposals

Status: step 1 built 2026-11-19 · constitution 1.6.0 (XIII amended) · waits on E63 for the
graph's own list of protected branches

## Problem

A window has one branch, and every tab in it reads and writes that branch. The cookbook study
(`docs/ux/2026-11-18-cookbook-study.md`, proposal "The branch belongs to the tab, not the
window; a protected branch takes proposals", ranked second; 8 of 8 cookbooks) found three costs:

- **A view needs a branch the window is not on.** Removing one off-topic edge during a review
  took six steps and a window-wide switch (spike S3): the review's inspector, and the page opened
  from it, were held at the reviewed commit and read-only. Running a check on the agent's branch
  meant moving the whole window there, so every tab and every edit went to the agent's branch
  until Alice moved back (company C2, f04: the query tab's picker offered only main's head and
  commits). A claim opened from a review showed branch data under a top bar that said `main`
  (company C1).
- **One click commits to `main`.** As `act-writer`, one click on an enum committed straight to
  `main` (pharma R3, f11). Every cookbook README says "review on a branch", and every policy
  declares `protected_branches: [main]`, but Studio cannot tell: `AccessOutput` says what the
  actor may do, not which branches are protected.
- **Starting a branch to work on** took a dialog and a window switch each time (beads B3, brain
  N2).

## Principles

- **XIII** (one rule for what a pick does), amended below: the top says the window's branch,
  the default for new tabs; a tab on another branch says its own.
- **IV** (every write names its branch): every editor names where a save lands, the proposal
  included, and a write never lands somewhere its surface did not name.
- **III** (three outcomes): a proposal that cannot be made sends nothing, so the edit is refused
  with why and its draft stays; nothing is retried.
- **I, X** (the engine is the authority; it moves at its own pace): Studio does not keep its own
  list of protected branches as truth. Until the server says (E63) it treats `main` as protected
  by default, and the person can turn that off.
- **VI, VIII**: the proposal mode is visible, has an action with a binding, and is in the palette.

## Amendment (constitution 1.5.2 → 1.6.0, minor)

**XIII. One rule for what a pick does**, the second paragraph:

> ~~The top says which server, graph and branch; the left lists what there is; …~~ The top says
> which server and graph, and the window's branch, which new tabs read; a tab on another branch
> says its own in its header (spec 038). The left lists what there is; …

and *In practice* gains: "A view opened from a tab on a branch of its own reads that branch, or
holds the commit the tab shows where it cannot read another branch."

Minor, not major: nothing is reversed. The top still says where the window is; a tab on its
own branch is a new kind of read context, as a held commit already was (spec 020, "What counts
as the same thing": "the session and branch, and either head or a held commit").

## What exists today (checked)

At `origin/main` `98c7cea`, Omnigraph `v0.15.0` (contract 0.14).

- `GraphSession` holds one `branch` (`crates/graph_store/src/session.rs`), and every view takes
  it at birth and follows `SessionEvent::BranchChanged` (`NodePeek::build`, `TypeTable`,
  `QueryEditor`). Writes already name their branch (`write_on`, `write_properties_on`,
  `blob_put_on`, `blob_clear_on`, `invoke_stored_write`; spec 011), drafts are kept by branch
  (`drafts: HashMap<String, …>`), and the session watches heads of branches its window does not
  show while a view holds a `HeadWatch`.
- A read scope already records its branch (`scope::Scope::branch`), but `GraphSession::read`
  with no snapshot reads the session's branch.
- The review held its inspector's node and the page it opened at the reviewed head
  (`ReviewView::selected_node`, `open_selected_page`), read-only.
- The query tab's snapshot picker offered the head of the window's branch and its last commits
  (`QueryEditor::picker_entries`); `invoke_stored_read` read the session's branch.
- `AccessOutput` (`crates/graph_client/openapi/omnigraph-0.15.0.json`) has `actor_id`,
  `client_id`, `branch`, `target_branch`, `graph_wide` and `actions`: nothing names a protected
  branch. The policy's protection shows only as actions missing on `main` for some actors.
- `POST /branches` takes `from` (a branch head) or `from_commit` (E21; every supported server).
  There is no branch rename route: the document's branch paths are `GET`/`POST /branches`,
  `/branches/diff`, `/branches/merge`, `/branches/merge-base` and `DELETE /branches/{name}`.
- Branch names with `/` are served (`agent/<stamp>`, spec 013).
- `ReviewBranch` (⌘⇧R) opened the window's branch against `main`.

## Design

### A tab carries its branch

A tab reads either the window's branch (it *follows* the window, as every tab did) or a branch
of its own, which it keeps whatever the window shows. The window's branch is the default for new
tabs. Built for the three views the study names:

- **Node page.** `OpenNodePage` gains `branch: Option<String>` (`#[serde(default)]`; `None`
  follows the window, and a name equal to the window's branch is taken as `None`). The page's
  peek (`NodePeek::on_branch`) reads and writes that branch: its read scope is on it, it holds a
  `HeadWatch` for it, and it reads what the credential may do there (`read_access`). It ignores
  `BranchChanged`. Its toolbar shows `⑂ on <branch>` while the window shows another, with the
  tooltip "This page reads and edits `<branch>`, whatever the window shows." Its key is
  `node:<type>:<id>#<branch>`, so a page on a branch and the same node's page following the
  window are two tabs. It is saved with its branch and comes back on it. Its neighbours and its
  edges' ends open on the same branch. Its Tree and Canvas, which read the window's branch,
  hold the commit the page shows instead (read-only), so they never show the window's data under
  the page's branch.
- **Review.** A review of the whole branch (`scope` none, branch not merged and deleted) gives
  the inspector the selected change's node *on the reviewed branch*, read at the reviewed head
  (`InspectedNode { branch, snapshot }`); Space and ⌘↵ open its page on the branch. Both are
  editable there, with Remove and + on relationships, because they are the page's view. A node
  the branch removes stays held at the fork; one commit's review holds every node at that
  commit, as before. `InspectedNode` gains `branch`, and `type_table`'s node inspector puts the
  peek on it.
- **Query tab.** The snapshot picker lists, after the head and the last commits, "Read another
  branch": the window's branch first ("Head of main · the window's"), then the graph's others in
  the session's order, at most 12 (`PICKER_BRANCHES`), then "N more in the Branches panel". A
  branch picked becomes the tab's own (`QueryEditor::set_own_branch`): Run, a stored read
  (`invoke_stored_read_on`), Refresh, the commits the picker offers and a write's confirm bar all
  use it; a held commit, a write waiting to be confirmed and the commits read go, being the other
  branch's. The picker's label stays "Head of `<branch>`", in the tab's own header, and its
  tooltip says "Reads and writes run at the head of `<branch>`, whatever the window shows". The
  window's branch picked again makes the tab follow the window. Saved with the tab
  (`"branch"`), for a file, a scratch query and a stored query alike. A result's node is
  inspected on the tab's branch.

The session's scoped reads honour their scope's branch: `read_in` with no snapshot reads the
scope's branch when it is not the session's (`read_on`). Later reads pin to the commit the first
answered, as every view's do.

### Proposals

A write asked of a branch lands where `GraphSession::write_target` says:

| The window | A write asked of `main` (protected) | Of another branch |
|---|---|---|
| Proposals on for `main` (the default), no proposal open | starts a proposal: `NewProposal { base: main }` | goes there |
| On a proposal `P` into `main` | goes to `P` | goes there |
| Proposals turned off for `main` | goes to `main` | goes there |
| Proposals turned on for a branch `b` | goes there | `b`: starts a proposal from `b` |

- **Which branches are protected.** `GraphSession::protects`: `main`
  (`PROTECTED_WITHOUT_POLICY`) until the server says (E63). Studio proposes on a protected
  branch unless the person turns proposals off there, and on another branch only where they turn
  them on. The default is `graph_store::ProposeProtected`, which `graph_store::init` sets, so
  the app proposes and a test that makes its own session writes where it asks unless it sets it.
- **Starting one.** The first write makes the branch, then sends the write there once
  (`GraphSession::routed_write`, under every write entry point: mutations, property writes, Blob
  put and clear, stored mutations; a statement names its own branches and is sent as it is).
  The branch is `proposal/<actor>/<MMDD>-<n>` (`proposal_name`): the actor's name lower-cased
  with anything but letters, digits, `.`, `_` and `-` as `-` (`me` with none), the local date,
  and the first `n` that no listed branch and no proposal this session named holds. It starts at
  the commit the write is bound to (`If-Graph-Commit`), so the binding holds there, and otherwise
  at the base's head. Writes asked while it is being made wait for it and go there too (one
  `Shared` task per base). A branch that could not be made sends nothing: the write is `NotSent`
  with "The proposal branch … could not be made: …", which every editor shows as a refusal that
  wrote nothing, its draft kept (III). A creation whose outcome is unknown says so and sends
  nothing.
- **The window moves to it.** Once the write went through, a window still on the base moves to
  the proposal (`set_branch`) and records it (`Proposal { base, branch }`). Every tab following
  the window reads the proposal, so what was just edited stays in view; a page on a branch of
  its own stays where it is. The window leaves the proposal when it leaves the proposal's branch
  (the top bar, Switch, the branch deleted by its merge), or with Stop proposing. The next edit
  of the base after that starts another proposal.
- **The top bar.** `local / pharma / ⑂ main ⌄ → ⑂ Proposing on proposal/act-writer/1119-1 ⌄
  · 3 edits · Review`. The first chip keeps naming the base, its menu switching the window as
  before. The proposal's menu offers Review proposal (⌘⇧R), "Stop proposing; show main" (⌘⌥⇧P)
  and "Edit main directly" (⌘⌥P). "N edits" counts the session's committed writes to the
  proposal (a window reopened on one counts from then, and shows "Review" alone until its first).
  Before the first edit, a quiet "proposes edits" button beside the branch says it, with the
  tooltip "Edits start a proposal branch, since main is treated as protected. Turn proposals
  off  ⌘⌥P".
- **Every surface names it.** `GraphSession::writes_to(branch)` is what an editor's keys say
  after "write ·": `main`, `proposal/act-writer/1119-1`, or "a new proposal from main". The
  table cell, the select, the page's fields, its relationship search, its unsaved-changes bar,
  the new-node and edit dialogs (`write_scope`), the query tab's confirm bar ("add changes data
  on a new proposal from main.", "Run on a new proposal from main") all use it. The outcome's
  toast and the activity log name the branch the write went to, as they always did.
- **Review.** `ReviewBranch` on a proposal opens it against its base.
- **Saved.** A window's graph saves `proposing_into` (when on a proposal), `propose_off` and
  `propose_on` (`SavedGraph`), and comes back with them.

### Actions

| Action | Binding (macOS / Linux, Windows) | Where |
|---|---|---|
| `studio_graph::ToggleProposeChanges`: "Turn proposals on or off for edits of the window's branch" | ⌘⌥P / Ctrl+Alt+P | Always, with a graph; on a proposal it is its base's switch, and turning it off leaves the proposal |
| `studio_graph::StopProposing`: "Leave the proposal for the branch it proposes into" | ⌘⌥⇧P / Ctrl+Alt+Shift+P | Only while the window is on a proposal (`register_action_when`) |
| `studio_graph::ReviewBranch` (existing, ⌘⇧R) | | On a proposal, against its base |

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Treat `main` as protected by default until E63 | Read protection from actions missing on `main` | A missing action means "this actor may not", not "the policy protects it": an admin allowed on `main` would get no proposals, and a reader would be told to propose where it may not write at all |
| Route the write in the session, under every write entry point | Each view starts the proposal before it writes | One place, one rule; every editor, dialog, Blob write and stored mutation gets it, and none can forget (IV) |
| The window moves to the proposal after the first write lands | Keep the window on `main` and read the proposal in each tab | Following tabs then show what was just written, with no second read model; the top bar still names `main` as the base |
| Start the proposal at the commit the write is bound to | Always at the base's head | A bound write would otherwise be refused (412) whenever `main` moved after the person read it |
| A failed creation sends nothing (`NotSent`) | Fall back to writing on `main` | The person asked for a proposal; a silent write to the protected branch is the study's worst case |
| Tree and Canvas from a page on another branch hold the page's commit | Give them branches of their own now | Their walks are shared per session and branch (`Exploration`); a held commit is correct and read-only, and their own branch is step 2 |
| The query picker shows at most 12 other branches | The whole list | An agent can leave thousands (IX); the Branches panel lists them all |
| No rename in step 1 | Rename as create-at-head then delete | Omnigraph has no rename route; two writes that can fail between them need their own design (step 2) |

## Deferred

- **E63** ([#1473](https://github.com/ModernRelay/omnigraph/issues/1473)): which branches the
  policy protects, in `AccessOutput` or the branch list. With it, `protects` asks the server and
  `PROTECTED_WITHOUT_POLICY` goes. See `docs/deferred-engine-changes.md`.
- **Step 2.** Tables, trees and canvases on a branch of their own (the view strip carries the
  branch, not the held commit); a proposal renamed (create at its head, then delete it, which
  destroys no data, confirmed with ↵); "New branch" opening the new branch's tabs rather than
  switching the window (beads B3, brain N2); the review taking Studio's own edits on its branch
  without "Newer data" (study, Appendix A, spike S3).
- **UI tools** (spec 032): a tool that opens a view on a branch, and one that switches a
  window's branch (study, "Agents outside Studio").

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | Nothing in contract 0.14 names a protected branch | `AccessOutput`, `BranchListOutput` and `BranchDetailOutput` in `omnigraph-0.15.0.json`; `GET /graphs/pharma/access?branch=main` as `act-writer` against `v0.15.0` serving the pharma cookbook, whose policy declares `protected_branches: [main]` | Actions `read, export, change, branch_delete` on `main`; no field about protection. Filed as E63 (#1473) |
| 2026-11-19 | No branch rename route | Paths of `omnigraph-0.15.0.json` containing `branch` | `GET`/`POST /branches`, `/branches/diff`, `/branches/merge`, `/branches/merge-base`, `DELETE /branches/{branch_name}` |
| 2026-11-19 | The first edit of `main` makes one proposal, every edit lands there, the window moves to it, and `main` never moves; turned off, `main` takes the edit | `graph_store` `an_edit_of_main_lands_on_a_proposal_the_window_moves_to` (FakeHttpClient) | One `POST /branches` (`proposal/me/<MMDD>-1` from `main`), three `POST /mutate` on it, two of them asked before it existed; window on the proposal, three edits counted |
| 2026-11-19 | A proposal that cannot be made sends no edit | `graph_store` `a_proposal_that_cannot_be_made_sends_nothing` | 403 on `POST /branches`: the write is `NotSent`, no `/mutate`, window on `main` |
| 2026-11-19 | A page on its own branch reads and writes it and ignores the window's moves | `type_table` `a_page_on_a_branch_of_its_own_reads_and_writes_it` | `studio_node` read with `branch: agent/x`; none on `fix` after the window moved there; the save sent with `branch: agent/x` |
| 2026-11-19 | A query tab on its own branch reads it, a stored read too, and follows the window again when the window's branch is picked | `query_editor` `a_tab_on_a_branch_of_its_own_reads_it_whatever_the_window_shows` | Both requests carry `branch: agent/x`; saved with `"branch": "agent/x"` |
| 2026-11-19 | The review's inspector is on the reviewed branch; one commit's review holds it | `review` `the_inspector_shows_the_selected_changes_node_where_the_review_reads_it` | `InspectedNode { branch: feature, snapshot: f2 }`; with a commit scope, `at_commit: f2` |
| 2026-11-19 | In the running app against `v0.15.0` with the pharma cookbook, an edit of a decision's status as `act-writer` lands on a proposal and `main` keeps its value | "Running app" below; `GET /branches` and a read of `Decision.status` on `main` with curl after | `proposal/act-writer/1119-1` made and holds `decided`; `main` still `upcoming` |
| 2026-11-19 | The review's inspector, the page it opens and a query tab read the agent's branch while the window stays on `main` | "Running app" below, on `agent/decision-sweep` (one commit by `act-writer` setting `dec-oral-phase3-start` to `deferred`, made with curl) | Inspector and page show `deferred` and the new outcome with the status menu; the page says "on agent/decision-sweep"; `decisions_upcoming` there answers 1 row where `main` answers 2 |
| 2026-11-19 | The enum's own tooltip no longer covers its menu (study, Appendix A, pharma R3) | The status menu opened in the running app, before and after | Before: "upcoming / enum(…)" over the menu's first rows; after: the menu alone, headed "Saves to a new proposal from main" |

### Running app

2026-11-19: the debug build's offscreen snapshot tool (`crates/studio/examples/snapshot.rs`,
1440×900 drawn at 2x, lavapipe) against `omnigraph-server` `v0.15.0` serving the pharma and beads
cookbooks from one cluster, as the study's Appendix C sets up, with `act-writer`'s token. Steps:
open `dec-vanquish-interim-readout`'s page on `main` (the bar reads "main · proposes edits");
open its status menu (headed "Saves to a new proposal from main"); pick `decided` (the toast:
"Set status of dec-vanquish-interim-readout · committed to proposal/act-writer/1119-1"; the bar:
"main → Proposing on proposal/act-writer/1119-1 · 1 edit · Review"); ⌘⇧R (the review of the
proposal against `main`, "Merges cleanly into main as a fast-forward"). Then, on a fresh window
on `main`: the review of `agent/decision-sweep`, its card selected (the inspector on the branch),
⌘↵ (its page, "on agent/decision-sweep"); `decisions_upcoming`, its picker, "Head of
agent/decision-sweep", Run. The frames are in the PR.

## Corrections

None yet.
