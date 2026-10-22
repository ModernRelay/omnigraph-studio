# Specs

One numbered folder per feature, each with a `plan.md`. Add `validation.md` when the validation
log grows long. Keep folders after they land; they are the record of why Studio is the way it
is.

This is a lean form of GitHub Spec Kit: most features need one plan in the shape below, and only
large ones add Spec Kit's tasks, contracts or data-model files.
Spec Kit's layout is kept, so its commands still work: the constitution is in
`.specify/memory/constitution.md` and the plan template in `.specify/templates/`.

## Shape of a plan

Copy `.specify/templates/plan-template.md`. Sections, in order:

1. **Status**: draft, accepted, in progress, implemented (with date), or superseded.
2. **Problem**: what is wrong or missing, in the user's terms.
3. **Principles**: the constitution principles this touches.
4. **What exists today (checked)**: facts about Studio, Zed and Omnigraph that the design rests
   on, each with a path or command.
5. **Design**: what changes, with exact keys, states, copy, limits and timings.
6. **Decisions**: each choice and the alternatives rejected, with reasons.
7. **Deferred**: what waits, including engine changes (link
   `docs/deferred-engine-changes.md`).
8. **Validation log**: dated checks of every load-bearing claim, with results.
9. **Corrections**: what turned out wrong after implementation.

Add tasks, contracts or data-model files only for large features.

## Index

| # | Plan | Status |
|---|---|---|
| 001 | [Foundation](001-foundation/plan.md) | in progress |
| 002 | [App shell](002-app-shell/plan.md) | in progress |
| 003 | [Connections](003-connections/plan.md) | in progress |
| 004 | [Type tables](004-type-tables/plan.md) | in progress |
| 005 | [Relationship tree](005-relationship-tree/plan.md) | first slice implemented |
| 006 | [Graph canvas](006-graph-canvas/plan.md) | first slice implemented |
| 007 | [Query editor](007-query-editor/plan.md) | first slice landed |
| 008 | [Branches, review and merge](008-branches-review-merge/plan.md) | built on 0.13 |
| 009 | [History](009-history/plan.md) | built on 0.13 |
| 010 | [Inspectors](010-inspectors/plan.md) | built on 0.13 |
| 011 | [Agent panel](011-agent-panel/plan.md) | implemented |
| 012 | [Omnigraph engine changes](012-omnigraph-engine-changes/plan.md) | deferred |
| 013 | [The agent uses the Omnigraph CLI](013-agent-cli/plan.md) | implemented |
| 014 | [Node pages, editing in place, and long text](014-node-pages/plan.md) | part 1 built |
| 015 | [One read model](015-read-model/plan.md) | draft |
| 016 | [Studio as an OAuth client](016-oauth-client/plan.md) | draft |
| 017 | [Studio updates itself](017-auto-update/plan.md) | draft |

Each plan's first line holds its full status. 012 is not scheduled: it records every Omnigraph change Studio would use, what Studio does
without it, and which plan waits on it. Each feature plan's Deferred section names its IDs.
