# OmniGraph Studio constitution

Version 1.1.0 · ratified 2026-10-11 · amended 2026-10-17 (IV: ↵ and Undo for what Studio can
put back)

OmniGraph Studio is a native desktop app for exploring and changing OmniGraph graphs: type
tables, a relationship tree, a graph canvas, a `.gq` query editor, branches, review, merge and
history. These principles bind every plan and every change. A plan names the principles it
touches; a change that breaks one amends this file first.

## I. The engine is the authority; Studio speaks HTTP

Studio reads and writes graphs only through an OmniGraph server's HTTP API. It never opens
graph storage, never links the engine, and never keeps a copy of graph data that it treats as
true. The one engine crate it links is `omnigraph-compiler`, for parsing schemas and queries
offline; parsing is language intelligence, not data access.

*In practice:* one client crate owns every request. Every request sends the
`omnigraph-http-api` contract header and every response is checked for it. A response without
it has an unknown effect.

## II. One snapshot per view

Every view reads one commit. It records the `graph_commit_id` its first read returned and pins
later reads to it. When the branch moves, the view offers Refresh; it does not change under the
reader. A write carries the commit it was read at (`Omnigraph-If-Graph-Commit`).

## III. A write has three outcomes

A write is **committed**, **refused** (and nothing was written), or **unknown** (the response
was lost). Studio never retries a write by itself. An unknown outcome is settled by reading
history, never by sending the write again. The person's draft is kept until the outcome is
known.

## IV. Every write names its branch; Cedar decides

Every surface that writes shows the branch it writes to. A destructive action says exactly what
it destroys, including what goes with it, before it runs. When Studio can put it all back, it
is confirmed with ↵ and its outcome offers Undo; when it cannot, it is confirmed by typing the
name or key of what is destroyed. Studio reflects the server's policy and never
enforces its own: a hidden button is a courtesy, a 403 is shown as a denial, and the draft
stays.

## V. Nothing moves under you

Layout is stable. A row that stops matching a filter after an edit stays in place, dimmed,
until the list is read again. Columns only grow while results stream. Tabs, buttons and modal
positions keep their size and place. New data announces itself as a chip, never as a jump.

## VI. Every control explains itself

A control shows its keyboard shortcut. A disabled control says why. Where an `omnigraph` CLI
command does the same thing, Studio shows it.

## VII. Loudness matches cost

Routine success is silent. A toast appears only after a durable publish, with one follow-up
action. An error says what happened and what to do next. Motion follows frequency: actions done
many times a minute never animate; rare ones get a short ease-out; reduced motion keeps fades
only.

## VIII. Keyboard first

Everything Studio can do is an action with a binding. Menus, tooltips and the command palette
are generated from actions. The palette lists only actions available where the person is.

## IX. Work is bounded

Every list is virtualized and paged. Every fetch has a page size or a cap, and every cap has an
honest exit ("Too many nodes to draw · Open as table"). Expensive work runs off the main
thread. A frame does no I/O.

## X. The engine moves at its own pace

Studio targets the released OmniGraph HTTP contract (0.13 today). A feature that would be better
with an engine change is designed against today's API, and the change is recorded in
`docs/deferred-engine-changes.md`. Studio never works around a missing route by keeping its own
authoritative state. Newer routes are used only when the server's own OpenAPI document
advertises them.

## XI. Zed lineage, GPL boundary

Studio is GPL-3.0-or-later and is built on Zed's crates from git, pinned to one Zed release
tag. Files copied from Zed keep Zed's copyright notice and record the Zed commit they came from.
No AGPL, SSPL or BSL dependencies. No GPUI Kit or other copy of GPUI: there is exactly one
`gpui` in the build.

## XII. Evidence over assumption

Every plan has a validation log. Claims about Zed, GPUI or OmniGraph are checked against source
at the pinned revisions and cited by path. A performance claim needs a measurement, recorded
with the machine and the data size.

## Governance

Amend this file in a pull request that states the change and bumps the version: major for a
removed or reversed principle, minor for a new one, patch for wording. Plans in `specs/` cite
principles by number.
