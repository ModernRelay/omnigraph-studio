# 034 Omnigraph 0.15

**Status:** built 2026-11-18, for Studio 0.4.9 (PR #128). The pins moved to `v0.15.0` the day
before (contract 0.14 unchanged); this plan makes Studio use what 0.15 improved, without losing a
0.14.0 server.

## Problem

Omnigraph 0.15.0 (2026-11-09) keeps HTTP contract 0.14 and storage, and changes what a server
does: a `@card` minimum is checked on every write that adds or changes a source node, a ranked
read says how it ranked, the branch list can carry each branch's head and times, the access
route names the OAuth client, `omnigraph studio` opens Studio on a target, and diffs and merges
cost less. Studio only decoded the new fields. It had no way to say which server serves what: the
header must be exactly 0.14, later fields are optional, and what each release serves was written
only in `docs/deferred-engine-changes.md`. And one change makes Studio wrong: its new-node dialog
saves a node alone, which a 0.15 server refuses when the node's type needs an edge.

## Principles

- III: a write's outcome is the server's; an unknown one is settled from history, never retried.
  The `@card` refusal states `effects: unknown` though nothing was written (E54), so Studio keeps
  calling it unknown and only says why in words.
- X: Omnigraph `v0.14.0` and later. Every feature works against 0.14.0, falling back to what
  Studio did before, with a test for each side.
- No engine changes from this repo: E54 is added to spec 012.

## What exists today (checked)

- `GET /healthz` answers `HealthOutput.version`, kept on `Connection.health`
  (`graph_store/src/connection.rs`); nothing checked a feature against it, and a health check of
  a ready connection dropped the answer, so an upgrade in place was never seen.
- The CLI's `version --json` `protocols`, checked by number in `graph_store/src/cli_tokens.rs`,
  is the shape for a client checking what the other side drives.
- Omnigraph names an edge build after the release it leads to: `X.Y.Z-edge+<commit>`, which sorts
  before `X.Y.Z` (`.github/workflows/release-edge.yml`); main is `0.15.1` after `v0.15.0`.
- Release servers `v0.14.0` and `v0.15.0` (checksums verified), serving `script/dev-server`'s
  cluster with a `Member`, `Team` and `MemberOf: Member -> Team @card(1..)` added (Validation log).

## Design

### Server features

`graph_store::ServerFeatures` says which features a server serves, from one table, `FEATURES`:
each feature with the first release that serves it. It is worked out from the health answer's
version at the connect and at every health check, kept on the `Connection`, and emits
`ConnectionEvent::FeaturesChanged` when another release serves other features. Semver, with
prereleases sorting before their release (an edge build serves the release before it); an
unreadable version is `0.14.0`. Code asks `features.has(ServerFeature::…)`. The table is mirrored
in `docs/deferred-engine-changes.md` ("Server features by release"), and a test fails when a
feature or its release is missing there. Spec 003 ("Server features") and `docs/architecture.md`
(state model) describe it, and `AGENTS.md`'s rules forbid an ad hoc version check.

| Feature | Release | What changes in Studio |
|---|---|---|
| `RankedReadOutput` | 0.15.0 | The query editor shows the ranking (below) |
| `DetailedBranchList` | 0.15.0 | A list's heads in one request |
| `AccessClientId` | 0.15.0 | The OAuth client in the connection tooltip |
| `CardMinimumOnEveryWrite` | 0.15.0 | A new node is saved with the edges its minimum needs |
| `NotAdmittedCode` | 0.15.0 | A 403 `not_admitted` says so at the connect; a 401 to a held sign-in is its token refused |
| `UniqueOnScalarsOnly` | 0.15.0 | The schema view's note on a refused constraint |

`RankedReadOutput` and `AccessClientId` are read from the answer (the field is there or not);
their entries record where they come from, so the doc and the code list every 0.15 addition
Studio uses.

### 1. Correctness

**`@card` minimums on every write.** Every way Studio writes a node, checked
(`gq_language::builder`, `type_table`, `graph_store`, `review`, `agent_panel`):

| Path | One mutation with its edges? | Done |
|---|---|---|
| Type table's New row (`InsertRowModal`) | No: properties only | On a server with `CardMinimumOnEveryWrite`, the dialog has a field for each edge the type's minimums need (`SchemaModel::required_edges`, one per edge `@card(n..)` asks for), searched by name or id as the edge dialog's ends are (the shared `NodeFields`), and sends `insert_node` and each `insert_edge` composed into one mutation (`fixes::compose`). Nothing is sent until each is picked. A type with no single key, or whose required edge has a required property, cannot be inserted from the dialog there, and says to use a query. On a 0.14 server the dialog is as before |
| Undo of a delete (`deletes::undo_query`) | Yes, already | — |
| Cell edits, drafts (⌘S), bulk set, peek saves, text editor, Review's Edit | Property updates | A node already below its minimum is refused on 0.15 even so; the refusal says which bound (below) |
| Peek relationship edits | Edges alone, between existing nodes | Unchanged; removing the last required edge was refused before 0.15 too |
| Review's Revert of a removed node (`fixes::revert_query`) | No: the node alone, its removed edges as other cards | Left: see Deferred |
| Query editor, stored writes, the agent's CLI | The person's or agent's own text | The agent's skill says to send a node with its edges (4) |
| CSV or JSONL import | Studio has none | — |

A refused write's notice names the bound: `graph_store::CardViolation` reads the server's words
(`@card violation on edge MemberOf: source 'bo' has 0 edges (min 1)`), and the table's notice
says "The server answered that bo would have 0 edges of MemberOf, and the schema allows each one
at least 1, but not whether anything was saved. Check History before trying again; your changes
are still here." It stays an unknown outcome: the server answers `effects: unknown` (E54).

**Not admitted, for the whole server.** 0.15 answers an identity it admits no principal for
with 403 `not_admitted` on every authenticated route (`require_bearer_auth`), the graph list
included, where 0.14 answered 401. The connection read that 403 as "may not list graphs", so a
person signed in but not admitted saw "Connected" and an empty Open dialog. The connection now
owns the decision: a 403 `not_admitted` to the connect's graph list (or discovery) leaves it
`Unauthorized` and not admitted (`Connection::not_admitted`), labelled "Not admitted", with the
Open dialog's not-admitted text and its Sign in as someone else and Sign out, and none of a 401's
token handling. On a server with `NotAdmittedCode`, a 401 to a sign-in held reads as its token
refused (`HeldRefusal::Refused`, "Sign-in refused"), which a new sign-in may replace; from a 0.14
server it reads as before. A graph or read refused with `not_admitted` mid-session (the server's
admission snapshot can drop a principal at any time) keeps its own words (`SchemaFailure`, the
query editor).

**Schema diagnostics.** Studio has no `.pg` diagnostics; its schema view is read-only. It now
runs the compiler's admission parser over the served schema (`gq_language::admission_refusal`;
Studio reads schemas with the compatibility parser, so such a graph still opens) and, when it
refuses, says so under the toolbar: on a 0.15 server "This server refuses any schema change that
keeps this: @unique is not supported on list property D.tags. Drop the constraint in the schema
you apply."; on a 0.14 server, that 0.15 will and no write can set such a property.

**`omnigraph studio --target NAME`.** The 0.15.0 CLI (`crates/omnigraph-cli/src/commands/studio/`
and `docs/user/cli/studio.md` at the tag) sends `omnigraph-studio://open?target=&binding=&home=
[&graph=]`, which Studio's parser already took field for field. It refuses managed targets, so
it sends no managed-cluster link. It opens the link with `open -b com.modernrelay.omnigraph-studio`
on macOS (works), with the `x-scheme-handler/omnigraph-studio` desktop entry's `Exec` on Linux,
and through `HKCR\omnigraph-studio`'s URL protocol on Windows. On Linux and Windows nothing
worked: the desktop entry had no `%u` and no `MimeType`, the installer wrote no URL protocol, and
Studio read no link from its arguments (GPUI hands open-URL events on only on macOS). Now:
`Exec=omnigraph-studio %u` and `MimeType=x-scheme-handler/omnigraph-studio;` (the README says how
to register it from the tarball), the installer writes the `omnigraph-studio` URL protocol, and
`main` passes every `omnigraph-studio://` argument to the link handler
(`deep_link::links_in_args`). The comments that said the link came from "0.14.1" (there is no
such release) and that the CLI sends managed links, and spec 027's account of what the CLI looks
up on Windows, are corrected.

**Signed macOS CLI.** From 0.15 the CLI's release and edge builds are signed with a Developer ID
under `com.modernrelay.omnigraph` (#1293), so the Keychain knows an update and re-signing would
replace that identity. `script/sign-dev --cli` now leaves a Developer ID build alone and signs
only one signed ad hoc (built from source, or older); `AGENTS.md` says so.

### 2. The new data

- **Ranked reads.** A read's `ranking` is kept with its rows. The results' facts say "Ranked by
  nearest · lowest score first · score in d"; a score column's header reads "F64 · score"; and when
  `exhaustive` is false a warning fact, "Not exhaustive", whose tooltip says a nearer (or
  better-ranked) row than these may exist. An answer without a ranking shows none, as before.
- **Detailed branch list.** `GraphSession::head_commit`, which the branches list and History
  call for each branch they draw, joins one `GET /branches?detailed=true` on a server with
  `DetailedBranchList`: its answer gives every branch's head (kept in the head memo, as reads
  alone were) and its start and times (`GraphSession::branch_detail`). It fills the memo through
  a `memo::Fill` opened before it is sent, so a head forgotten while it was out (a commit landed
  on its branch) is not put back from an answer that may predate the commit, by the memo's own
  rule that a late answer changes nothing. A head forgotten since (a
  commit landed on its branch), a branch the details leave out, and a Refresh read one branch
  alone, as before; a 0.14 server reads each alone. The branches list's tooltip adds "made 3 days
  ago from main". The top bar's branch switcher shows names only and reads nothing per branch.
  Review's fork point stays the merge base: `from_commit_id` is where a branch started, which a
  later merge does not move.
- **`client_id`.** The connection chip's tooltip reads "as act-ada through studio-desktop" when
  the access route names a client; commits carry no client, so History shows none.

### 3. Workarounds 0.15 makes unnecessary

Found: `CHANGE_READ_TIMEOUT` (5 min) and the long-read slots (`graph_client`), the review's 10 s
"still working out the changes" note and 8,192-change pages (`review`), `REVIEW_CHANGE_CAP`,
History's 5,000-change commit pages, the fork search's 20 × 500 pages, the commit-changes memo.
0.15 resolves E9 and part of E35 (#1201: one history with no merge between its commits); E36,
E37 and E38 are open (#970, #972, #1157), and a diff across a merge or between two moved branches
still compares both tables. Every limit stays: each is an upper bound that costs nothing when the
server answers sooner, and a 0.14 server, or those diffs, still need it. Spec 012 marks E9 landed
and E35 partly, with the PRs. Nothing in Studio worked around #1240 (a retired merge base); the
history lanes' 404 stub still serves commits cleaned up from a deleted branch. The changes'
display is spec 033's (#130, merged 2026-11-18); this plan changed only the data path there.

### 4. The agent

- The vendored guide moves to `v0.15.0` (`crates/agent_panel/skill/upstream`, `SOURCE.md`):
  `index build`/`index rebuild` for `rebuild-full-text-indexes`, the 0.15 `@card` and `@unique`
  rules, the MCP `query` tool, `ranking`, `load csv`, `--format toon`. Studio's agent uses no
  Omnigraph MCP tool (only Studio's own UI tools), so the MCP rename needs nothing else.
- Studio's `SKILL.md` adds the rule that matters in Studio: save a new node with the edges its
  minimum needs in one `mutate`, and what the refusal looks like.
- `--format toon` stays the agent's choice, not its default: over 40 rows of six columns the CLI
  printed 5,538 bytes as `table` (the default), 5,346 as `json`, 5,323 as `toon` and 5,146 as
  `jsonl` (Validation log). The 4 % it saves over the table does not pay for a default that a
  0.14 CLI, which Studio still drives, refuses.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| One table of features by first release, from `/healthz`'s version | `/openapi.json` per feature; a field's absence; version checks where used | The `@card` rule is in no OpenAPI document; a missing field says nothing when the read did not call for it; one table stays in step with the docs (spec 003) |
| An edge build serves the release before it | Treat it as its release; read its OpenAPI | Its name says the release it leads to, not what it has yet; under-using one is safe, misreading one is not |
| The dialog requires a new node's edges only on a 0.15 server | Always; never | 0.14 takes the node alone, as before, which this plan keeps for every 0.14 server. The cost, from review: a node saved alone on 0.14 is below its minimum after an upgrade, so 0.15 refuses its next edit until the same mutation adds its edges; that refusal names the bound |
| A minimum above 1 refuses the same end picked twice | Send it and let the server answer | Two picks of one node make one edge (or two parallel ones), which a 0.15 server refuses as an unknown outcome |
| Not admitted is the connection's, from the 403 at the connect | Each surface's own 403 handling | One owner for a fact about the identity at the server; every surface reads it |
| A `@card` refusal stays unknown | Call it a refusal from its words | The disposition says `unknown`; III settles that from history (E54 asks for `none`) |
| Keep every change-read limit | Shorten timeouts or pages on 0.15 | An upper bound costs nothing when the server is fast, and 0.15 did not make every diff fast |
| Keep the agent's `table` output | `toon` as default | 4 % smaller, and refused by a 0.14 CLI |

## Deferred

- Review's Revert of a removed node with a `@card` minimum: the node and its removed edges as
  one fix (`fixes::revert_query` given the review's edge changes), with Review's fixes, which
  spec 033 redrew; a refused revert says which bound meanwhile.
- `U64` parameters above `i64::MAX` as decimal strings (#1102): the query editor and the cells
  still refuse them, as the engine did before 0.15; every input path would need the feature.
- The `@card` refusal as a refusal: E54 ([spec 012](../012-omnigraph-engine-changes/plan.md)).
- A Linux package that registers the scheme itself; the tarball's README steps do it by hand.
- One Studio process on Linux and Windows: a link opens in a new process there, as a second
  start does.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-18 | A 0.15 server refuses a node saved without its required edge, and takes it with the edge in the same mutation; a 0.14 server takes both | `curl` to `v0.14.0` and `v0.15.0` release servers (`/tmp` copy of `script/dev-server`'s cluster plus `MemberOf … @card(1..)`) | 0.14: both commit. 0.15: the lone node answers 400 `@card violation on edge MemberOf: source 'bo' has 0 edges (min 1)`, `effects: unknown, action: reconcile`; with the edge it commits |
| 2026-11-18 | A late detailed branch list does not bring back a forgotten head | `graph_store` `session::tests::a_late_detailed_list_does_not_bring_back_a_forgotten_head` and `memo::tests::a_fill_keeps_nothing_forgotten_since_it_opened`; the session test run with the fill's forgotten check turned off | Pass; with the check off it fails at "not put back", as review found (B2) |
| 2026-11-18 | A server that admits no one for the token says so at the connect, and a 0.15 401 to a sign-in held is its token refused | `graph_store` `connection::tests::a_server_that_admits_no_one_for_the_token_says_so_at_the_connect` (every route 403 `not_admitted`, decoded through the client) and `a_401_to_a_sign_in_held_is_read_by_the_servers_release` | Pass: `Unauthorized`, "Not admitted", `HeldRefusal::NotAdmitted`; a policy's 403 stays ready with no list; a 401 reads `Refused` on 0.15.0, `NotAdmitted` on 0.14.0 (B1) |
| 2026-11-18 | The agent's default output is already compact | `omnigraph query --format table|json|toon|jsonl` (CLI 0.15.0) on 40 `Person` rows of six columns | 5,538 / 5,346 / 5,323 / 5,146 bytes |
