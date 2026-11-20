# A node's history: a design study

2026-11-20 · Studio `main` at `74eedf31` (0.4.11) · measured against `omnigraph-server`
`edge-20261030-1a3e7ed` (reports `0.14.1-edge`, contract 0.14) serving Studio's development
graph.

This is a design study, not a plan. It asks how Studio should answer *what happened to this
node: what changed, who changed it, when, and why*, and how to put a value back. It builds on
the closed [#146](https://github.com/ModernRelay/omnigraph-studio/pull/146) (spec 040 on
`feat/ux-p5-node-history`) and on proposal 4 of the cookbook study
([#143](https://github.com/ModernRelay/omnigraph-studio/pull/143),
`docs/ux/2026-11-18-cookbook-study.md` on `docs/ux-cookbook-study`). It is written to fit the
branch workflow RFC ([#158](https://github.com/ModernRelay/omnigraph-studio/pull/158),
`docs/rfcs/2026-11-19-branch-workflow.md`), which is still open. When the design is accepted, it
moves to a plan under `specs/` that carries the exact keys, copy and tests.

## Summary

A node's history is a **History tab**, the fourth view of a node beside Page, Tree and Canvas
(⌘⌥4). It is the one place that reads history. The page and the inspector only point to it.

1. **It opens on "Now".** The first block lists every property's current value, who set it,
   when, and in which commit. This is blame, and it answers the most common question in one
   look.
2. **Below that, changes, newest first.** Each entry shows who made it and how: a person, an
   agent or Studio, with the operation once E60 lands. It shows where: the branch, or "via merge
   of *branch*". It shows when and the commit. Under that come the property's before and after,
   relationship lines, and Blobs by size. Consecutive small edits by one author fold into one
   entry with their net change.
3. **Above "Now", work not merged yet.** Branches that change this node list as one line each,
   read-only, with their review one key away. When main has also changed the node since the
   branch started, the line says the branch will conflict.
4. **Compare and restore never write by themselves.** Two entries compare as the node at two
   commits. "Restore" puts the old value into the page's draft on the window's branch. The
   person saves it the way they save any edit: it names its branch (#158, surface 2), and the
   server's policy decides.

Today's API can do all of this, with two limits. A read costs what the node's *types* wrote,
not what the node did. A merge names the person who merged, not the person who made the
change. E62 removes both limits and E60 adds the "why". The measurements are below.

## Who asks, and what they need first

The cookbook study (#143) found the question in seven of its eight graphs. It comes in five
forms:

| Job | Asked as | Needs first | Then |
|---|---|---|---|
| **H1. Who set this value?** | "Who changed this deal's stage?" (vcos), "where did this title come from?" | The value's setter, commit and time, beside the value | The commit, or the change before it |
| **H2. What happened to this node?** | "When did the agent last touch `lk-queue`?" (beads B2) | The newest changes, by author and operation | One entry's before and after |
| **H3. Was it like this last week?** | "What did this record say before the import?" | The node at two commits, side by side | Put a value back |
| **H4. Is anyone changing it now?** | "Is an agent about to rewrite this?" | Unmerged branches that change the node | Their review |
| **H5. Where did it go?** | A removed node, or one created again | When it was removed, by whom, and what went with it | Restore it on a branch |

H1 is the most frequent and the cheapest to answer once the history is read. That is why the tab
opens on it.

## What today's API answers (measured)

### The routes

Checked in `crates/graph_client/openapi/omnigraph-0.15.0.json`:

| Route | Gives a node's history | Limit |
|---|---|---|
| `GET /commits?branch=&limit=&page_token=` | The branch's first-parent commits, newest first, paged, with `actor_id`, `created_at`, parents | No operation or message (E60). No entity filter |
| `GET /commits/{id}/changes?type=&kind=&op=` | One commit against its first parent, with before and after images | One request per commit. No `id` filter (E62) |
| `GET /changes?branch=&start=after:<id>&type=&op=` (the change feed) | One block per first-parent commit, each with its `cause` (actor, `authored_at`, `authored_branch`, parents) and images | Up to 128 commits a page (E38). Starts at a commit but has no stop, so the head is the poll's own. No `id` filter (E62) |
| `GET /diff?from=&to=&type=` | The net change between any two commits | Net only: a reverted update or an equal re-insert is not listed. Costs what the types changed between the two commits |
| `GET /branches/diff?source=&target=&type=` | What a branch changed since its merge base | One per branch. May read whole tables across a merge (E35) |
| `POST /query` with `snapshot` | The node exactly as it was at any commit | Cheap: one point read |
| `GET /blob?…&snapshot=` | A Blob as it was at a commit | Reads the whole file |

Three facts shape the design:

- **An image is the whole entity.** `ChangeImageOutput.properties` holds every property, so
  Studio works out which properties changed by comparing them. A Blob in an image is its bytes
  as `base64:` text (E59). Since #138, `change_cards` shows a Blob by its size.
- **A merge is one block, by the merger.** The change made on `featmerged` reads in main's feed
  as `act-dev`'s change, `authored_branch: main`, with `merged_parent_commit_id` set. The
  branch's own commit is not in main's feed. `GET /commits/{merged_parent}` names only the
  branch head's actor, which need not be the person who changed the node.
- **Deleting a node removes its relationships with it**, and inserting the key again brings
  back none of them. The type's opaque `id` stays the same; it changes only when the
  *type* is dropped and added again. To Studio, the same type and key are the same node.

### Cost

The fixture, on the development graph (`script/dev-server` on port 8797, with its own data
directory):

- The seed was loaded.
- 400 single-property commits: 10 of them change `person-000007` (title, email or tags), 3 add
  or remove one of its `Knows` relationships, and `person-000099` is removed and then created
  again.
- A branch, `featmerged`, changes the node's title and is merged after main moves (a true merge).
- A branch, `agentpending`, holds two unmerged commits that change the node.
- Then, for the second phase, 50,000 new `Person` nodes in 10 loads of 5,000, and 50 more small
  commits, 5 of them on the node.

The final graph has 464 first-parent commits on main. The client was a Python script on the
same machine (Apple M5 Pro, load average 5 to 6, local disk), so the times leave out network
latency. Bytes and request counts carry over to a remote server.

| What was read | Requests | Bytes | Time | Changes to the node found |
|---|---|---|---|---|
| *Before the bulk loads (404 commits)* | | | | |
| List all commits | 1 | 140 KiB | 0.02 s | — |
| Walk the newest 200 commits, `GET /commits/{id}/changes`, filtered by type | 200 | 140 KiB | 0.78 s | 7 |
| Walk all 404 commits, filtered by type | 404 | 550 KiB | 1.40 s | 22 |
| Feed from the beginning, the node's type only | 4 | 336 KiB | 0.81 s | 12 |
| Feed from the beginning, its type and its 3 relationship types | 4 | 553 KiB | 0.81 s | 22 |
| Diff from the middle commit to the head, `Person` | 1 | 84 KiB | 0.04 s | 1 (net) |
| The node at one commit (`POST /query`, `snapshot`) | 1 | < 1 KiB | 0.02 s | — |
| Pending: branch list and one branch diff per branch (2 branches) | 3 | 2 KiB | 0.02 s | 1 on `agentpending` (two commits, netted) |
| *After 50,000 new `Person` in 10 loads (464 commits)* | | | | |
| Feed window, the newest 60 commits | 7 | 13.2 MiB | 0.66 s | 6 |
| Walk the newest 60 commits | 60 | 13.2 MiB | 0.62 s | 6 |
| Feed window, the newest 200 commits | 8 | 13.3 MiB | 1.04 s | 10 |
| Walk the newest 200 commits | 200 | 13.3 MiB | 1.06 s | 10 |
| Feed from the beginning, 4 types | 10 | 13.7 MiB | 1.66 s | 28 |
| Feed from the beginning, the 3 relationship types only | 4 | 0.3 MiB | 0.35 s | 10 |
| Feed from the beginning, 4 types, `op=update&op=delete` | 4 | 0.3 MiB | 0.98 s | 18 (no inserts) |
| Diff from before the loads to the head, `Person` | 7 | 13.2 MiB | 0.76 s | 1 (net) |
| The node at 20 commits, 20 point reads | 20 | < 20 KiB | 0.39 s | — |

What the numbers say:

- **The feed and the walk read the same bytes.** The feed takes 8 requests for 200 commits where
  the walk takes 200. On a remote server at 50 ms a round trip, the walk adds about 10 s if run
  one after another, and over 1 s even 8 at a time. #146 was right to choose the feed.
- **A read costs what the type wrote, not what the node did.** Six changes to one person cost
  13.2 MiB, because the window held a load of 50,000 people and every insert carries its whole
  image. This is the one cost #146 did not measure: its cookbook had no bulk load.
- **The `op` filter is the cheap way past a load.** Reading only updates and deletes cuts the
  read 45 times (0.3 MiB) and keeps 18 of the 28 changes. What it misses are inserts: the node's
  own creation, and relationships added. Relationship types read alone are cheap here (0.3 MiB),
  because the loads wrote no relationships. That stays true only until a load does.
- **Point reads are cheap at any depth.** One read of the node at a commit takes about 20 ms. A
  binary search over 464 commits takes 9 reads (about 0.2 s) to find when the node first existed.
  The same search works for a value that changed once. It is not exact for a value that went back
  to an earlier value.
- The server is the 2026-10-30 edge build, before 0.15.0. Omnigraph #1201 in 0.15.0 makes a
  diff along one history read only the changed rows (E35, partly landed). It changes nothing in
  the feed's images.

### The read plan for today

The read plan follows from the measurements. Each step is bounded (article IX) and runs off the
main thread.

1. **A window.** The window's branch's newest 200 first-parent commits: one `GET /commits` page,
   as History reads (spec 009). Every later window answers for the same `head_commit_id`
   (article II).
2. **Updates and deletes first.** The feed from just before the window's oldest commit, filtered
   to the node's type and every relationship type with an end of that type, `op=update&op=delete`.
   The window is bounded by the commit ids its listing named, not by a block the feed may not
   send (the fix made in review of #146).
3. **Then inserts, under a cap.** The same feed with `op=insert`, under a byte cap of 8 MiB for
   each window. If a page would pass the cap, the window stops there and the entry says so (see
   States). The node's own creation comes from this pass. If the cap stopped it, the creation
   comes from a binary search of point reads.
4. **Pending branches.** Read when the tab is first shown, and not again until Refresh: up to 20
   branches, those written most recently (the detailed branch list on 0.15, as History's tags on
   0.14). Each gets one `GET /branches/diff` with the node's types, and Studio keeps only the
   changes to this node.
5. **New commits** come from the session's change log (spec 028, `GraphSession::change_logs`),
   which follows each watched branch's head for the types its views read. The tab adds its
   types to its scope. New changes to the node then arrive with no read of their own, and a chip
   announces them (article V).

The pages are kept in the session's read memo by commit, so the page's blame, the inspector's
line and the tab share one read.

## How other tools do it

Read from each vendor's own help pages (sources at the end). None of them shows a structured
record's history at the property level as fully as Omnigraph's images allow. The closest are
Airtable's record activity and Google Sheets' cell history.

| Tool | Where it lives | One entry | Grouping | Compare and restore |
|---|---|---|---|---|
| Notion page history | A window from the page's menu | A snapshot of the page, with what changed highlighted and who changed it | A version every 10 minutes while editing, and one 2 minutes after the last edit | No side-by-side view. Restore previews first, and a restore can itself be restored. Part of a page is restored by copying. Retention by plan, 7 days to unlimited |
| Figma version history | A right sidebar | Name, description, time, the main contributor | A checkpoint every 30 minutes. Autosaves fold under one row that expands | One version at a time. Restore adds checkpoints and loses nothing |
| Linear issue activity | Inline under the issue, with its comments | "Changed priority", with the old value in a tooltip | Similar events in a row are grouped. Older activity between comment threads folds | None |
| GitHub file history and blame | A Blame view of the file, and the file's own commit list | Each line under the commit that last touched it, with author, message and date | Lines group by commit | "View blame prior to this change" steps back. Compare is `base...compare`. `.git-blame-ignore-revs` hides noisy commits, with a banner saying so |
| `git log` | — | `-L` traces one range or function; `-S` finds when a string appeared; `--follow` continues past renames | — | — |
| Airtable record history | The expanded record's activity feed | Who or what made the change: a person, an automation, a sync, the API or a form | — | A filter for all activity, comments only or revisions only. One record at a time. Retention by plan, 2 weeks to 3 years |
| Google Docs and Sheets | A right panel from "Last edit" | Who and when, with the changes highlighted | Versions group, and "Expand" shows them one by one | Only whole-document restore, or a copy of a version. Named versions, with a filter for them. In Sheets, right-click a cell and "Show edit history" |

**Patterns worth taking:**

1. **History from the value itself** (Sheets "Show edit history", `git log -L`). A property row
   on the page offers "History of *title*", which opens the tab filtered to that property.
2. **Blame beside the value, and "before this change"** (GitHub). The "Now" block shows who set
   each value, and every entry offers "Open as it was before".
3. **Fold a run of small edits by one author** (Notion's 10-minute version, Figma's autosaves,
   Linear's grouped events). The folded row shows the net change and expands to the commits.
4. **Name the source, not only the account** (Airtable names an automation, a sync, the API or a
   form). An entry says a person, an agent or Studio, and the operation once E60 gives it.
5. **Fold the noise, and say that you did** (GitHub's ignore-revs banner). A bulk load reads as
   one muted line ("Load of 50,000 Person"). The setting that hides loads says how many it hid.
6. **A restore is a new change, previewed first** (Notion, Figma). In Studio a restore fills a
   draft, and saving the draft makes a commit. Nothing in history is lost or rewritten.
7. **Compare any two points** (GitHub `base...compare`). Pick two entries, or one entry and now.
8. **Old value in a tooltip** (Linear), for the compact line the inspector shows.
9. **Say how far back it goes** (Notion and Airtable show retention). Studio says "Read back to
   K1GB3NS7, 3 weeks ago" and "History before X was cleaned up".
10. **Filter by kind of activity** (Airtable). Filters for a property, an actor, and
    relationships only.

**Not taken:** a highlighted whole-document snapshot as the main view (Notion, Docs). A node is
structured, so a property row already says exactly what changed. Named versions (Figma, Docs):
Omnigraph has no tags, and a branch or a commit id already names a point. Deleting history
(Docs): history belongs to the engine (article I).

## Prior work, read critically

#146 (spec 040) built a node's History tab, History rows that say what a commit was, a History
search that finds a node, and review cards that name who set each value. Its parts and what this
study does with them:

| #146 | Keep | Change |
|---|---|---|
| A tab, the node's fourth view, ⌘⌥4 (article XIII: it has reads and state of its own) | Yes. ⌘⌥4 is already bound on main for the fourth view place | — |
| Windows of 200 commits from the type-filtered feed, "Read the 200 commits before" | The windows and the honest exit | Reads updates and deletes first, then inserts under a byte cap. Measured: a bulk load in the window cost 13 MiB for 6 changes, which #146's cookbook fixture never met |
| One entry per commit | — | Fold a run of one author's small edits into one entry with its net change |
| Setters only on review cards (`set_by`) | Keep on the review | Blame for the node itself, in the "Now" block, from the same fold |
| A merge shown as the merger's commit | Yes, as what the feed says | Add "via merge of *branch*", and on demand, who changed it on the branch (bounded, below) |
| Disabled for a page held at a commit | — | Open it, and mark the held commit with a "You are viewing" line. History is most wanted from a commit |
| Studio's own words for its own writes this session | Yes, until E60 | — |
| History search finds a node | Yes, as the way from the branch's history | — |
| No compare, no restore, no pending branches | — | All three are added here |

The branch workflow RFC (#158) sets the rules a restore must follow: *a window reads any branch
and writes to one*, *looking is read-only*, and *a review fixes its branch*. So:

- A pending change on another branch is read-only here. Its line opens that branch's review,
  where fixes to that branch are made, or the branch in a new window (`OpenBranchInNewWindow`,
  from the RFC).
- A restore is an edit on the window's branch. If the credential may not change that branch,
  the RFC's read-only line applies ("You can read main, not change it"), with New branch… when
  the server allows it.

## Design

### Where it lives

```
┌ Node page: Ada Lovelace ──────────────────────────────────────────────────────┐
│ ● Person  Ada Lovelace  #person-000007        main · K1GB3NS7                  │
│ [Page] [Tree] [Canvas] [History ⌘⌥4]                                           │
│                                                                               │
│ ⌁ title     Lead Engineer                    ← pointer on the row:            │
│ ✉ email     ada@example.org                    "Set by act-dev · 3 d ago ·    │
│ # tags      [research] [x]                      History of title ⌥H"          │
│                                                                               │
│ Last changed by act-dev, 3 days ago · History ⌘⌥4                             │
└───────────────────────────────────────────────────────────────────────────────┘
```

| Place | What it shows | Reads |
|---|---|---|
| **History tab** (the node's fourth view) | Everything below | All of the read plan |
| **Node page** | One line under the properties, "Last changed by *who*, *when* · History ⌘⌥4", only once the node's history is held in the session. A property row's tooltip and menu gain "Set by *who* · *when*" (when held) and **History of *property*** | None of its own. It shows what the tab or an earlier open read. Opening a page never reads history |
| **Inspector** | The same one line, and a **History** button beside Page, Tree and Canvas | None. J/K through a table moves the inspector row by row, so a read for each selection would be unbounded (IX) |
| **Branch History** | Search finds a node (#146) | As today |
| **A commit's tab, a review card** | ⌘⌥4 on a selected change opens that node's History, with the commit marked | — |

Rejected: **a section at the end of the node page.** Every page open would read history. Its
entries would grow under the person as they landed, and it would push relationships down
(articles V and IX). **A list in the inspector**: the reads would follow every selection. **The
branch's History filtered to the node**: it lists commits, not the node's values, and it cannot
show blame or pending work.

### The tab

```
┌ History · Ada Lovelace ───────────────────────────────────────────────────────┐
│ [Page] [Tree] [Canvas] [History]   12 changes on main, newest 200 commits  ⟳ ⌘R│
│ Filter: [All properties ▾] [Anyone ▾] [☐ Hide loads]          ⌘F              │
│                                                                               │
│ NOT MERGED                                                                    │
│ ⑂ agentpending  title, tags · act-dev · 1 h ago · Will conflict with main     │
│                                              Review ⌘⇧R · Open in new window  │
│                                                                               │
│ NOW                                                                           │
│ title  Set on featmerged     act-dev · via merge of featmerged · 2 h · HV2P   │
│ email  t300@x.org            act-dev · 3 d · 9QKD                             │
│ tags   [tag340] [x]          act-dev · 3 d · 7M1C                             │
│ name   Ada Lovelace          act-dev · created · 3 wk · 01M7                  │
│ knows → 3 people             last: + Grace Hopper · act-dev · 4 d             │
│                                                                               │
│ CHANGES                                                                       │
│ ~ act-dev · via merge of featmerged · 2 h ago                     HV2P ↗      │
│     title   T title 380 → Set on featmerged                 Restore ⌘⌥Z       │
│ ~ act-dev · 3 changes · 14:02–14:09, 3 d ago                  ⌄ 3 commits     │
│     title   T title 140 → T title 380                                         │
│     tags    [tag220] → [tag340]  [x]                                          │
│ − act-dev · 4 d ago                                               3QD9 ↗      │
│     − knows → Grace Hopper                                                    │
│ · Load of 50,000 Person · act-dev · 5 d ago (this node unchanged)  hidden     │
│ + act-dev · created · 3 wk ago                                    01M7 ↗      │
│     name Ada Lovelace · title Founder · email … (6 values)                    │
│                                                                               │
│ Read back to the graph's first commit.                                        │
└───────────────────────────────────────────────────────────────────────────────┘
```

From top to bottom: what is pending, what is true now, and how it got there.

- **Not merged** (H4) only appears when a branch changes the node. Each branch is one line: the
  properties and relationships it changes, who last wrote it, and when. The line says "Will
  conflict with main" when main also changed the node after the branch's merge base. The RFC
  measured that a conflict is per node on 0.15.0, so this is a fact, not a guess, as long as the
  branch and main both moved. It opens the review (⌘⇧R, the RFC's step) or the branch in a new
  window. Nothing here writes. With the window on a branch other than main, the section
  compares against main instead and says "main has changed this since *branch* started".
- **Now** (H1, blame) lists each property and relationship type of the node: its current value,
  who set it, how (the operation once E60 lands, else "via merge of …", "created", Studio's own
  words), when, and the commit. A property not changed in the commits read reads "set before
  K1GB3NS7 · Read older ⌘⌥N". A click on a row filters Changes to that property.
- **Changes** (H2), newest first. Each one is described under *One entry*.
- **The end row** says how far the read went: "Read back to K1GB3NS7, 3 weeks ago. Read the 200
  commits before ⌘⌥N", or "Read back to the graph's first commit."

The tab keeps #146's identity: "History · *name*", key `node-history:<type>:<id>`, restored with
the workspace, the inspector showing the node at the head read.

### One entry

```
 ~  act-dev  ·  agent on agent/20261120-1402  ·  close  ·  via merge  ·  2 h ago   HV2P ↗
 │   who            what kind of author          op (E60)   where          when     commit
 ├─ status     open → closed                                         Restore
 ├─ priority   1 → 0                                                 Restore
 ├─ summary    −  The queue drains every night.                       (long text: lines)
 │             +  The queue drains every hour.
 ├─ cover      file replaced · 1.2 MB → 1.4 MB                       Restore
 ├─ + blocked by ← lk-share-api
 └─ ~ works at → Acme · role: Engineer → Lead
```

- **The mark** (`change_cards::kind_mark`) is the node's own change: `+` created, `~` changed,
  `−` removed. A `·` marks an entry that changed only relationships.
- **Who.** The actor, as the feed's `cause` names it. Its kind comes from what Studio knows:
  - "agent", when the commit was authored on an `agent/<stamp>` branch or Studio's outcome log
    names the agent's CLI (spec 011). E20 will name which agent.
  - "Studio", when Studio made the write in this session.
  - A person otherwise. Never guessed beyond that.
- **What.** The operation and message, once E60 lands: "close", "load", "verify_claim · 'source
  confirmed'". Until then, Studio's own words for its own writes this session ("Set priority of
  lk-acl"), else nothing. Never invented from the values.
- **Where.** Nothing when the commit was made on the window's branch. "via merge of *branch*"
  for a merge block. "on *branch*, before it was merged" once the person has asked who changed
  it there (below).
- **When.** Relative, with the UTC time and the commit's full id in the tooltip. The short id
  opens the commit's tab (spec 009).
- **Values** come from the review's rows (`change_cards`, spec 033): a short value struck on the
  removed mark beside the new one on the added mark; a long text as changed lines; a list as
  elements gained and lost; a Blob by size, "file replaced · 1.2 MB → 1.4 MB" (#138). An
  unchanged property is not listed.
- **Relationships** use the review's lines. An edge whose end moved names both ends ("moved from
  Cleo"). An edge property change is shown under its line.
- **Actions** (in the row menu, and the selected entry's keys): Open as it was (⌘↵, the page held
  at that commit), Open as it was before (at its parent), Compare with now, the commit, and
  Restore on each value row.

**Folding runs.** Entries fold into one when all of these hold:

- they are consecutive in the list;
- they have the same actor, the same authored branch and the same kind of author;
- each is less than 10 minutes after the one before (Notion's window);
- none is a merge, a creation or a removal.

The folded row shows the net change of the run (the first before and the last after for each
property) and "3 changes · 14:02–14:09". Space unfolds it to its commits. A run starts folded
when it holds more than one commit. Each save in Studio is its own commit (spec 014), and an
agent's run is many, so this keeps them from flooding the list.

**Loads.** A commit that changed the node's type far more than the node itself still appears
when it changed the node: it is real history. When it did not change the node, it never
appears, because the read keeps only this node's changes. "Hide loads" folds entries from commits
whose own counts (E44, or the row counts History reads) pass 1,000 changes into one muted line.
The filter says how many it hid. This is GitHub's ignore-revs, made visible.

### Merges: who changed it on the branch

The feed names the merger. When asked, Studio finds the commit on the merged branch that
changed the node:

- The row's **Who on featmerged?** walks the merged branch's first-parent commits from
  `merged_parent_commit_id` back to the merge base of the two parents.
- It reads each commit's changes filtered to the node's type, at most 50 commits, 8 at once.
- The entry then reads "act-alice on featmerged, merged by act-dev". If the walk passes 50
  commits it says so and stops, and offers no guess.

On a deleted branch the latest commits may answer 404 (spec 009's validation of
`resolve_commit`). Then the entry says the branch's commits are no longer served. E62 replaces
all of this with the original commit and author.

### Compare two points (H3)

```
┌ Ada Lovelace · K1GB3NS7 (5 d ago) → now ──────────────────────── ⎋ ─┐
│              5 d ago                       now                     │
│ title        T title 140                   Set on featmerged   ↺   │
│ email        t180@x.org                    t300@x.org          ↺   │
│ tags         [tag100] [x]                  [tag340] [x]        ↺   │
│ name         Ada Lovelace                  Ada Lovelace            │
│ knows        − Grace Hopper                                    ↺   │
│ 3 values and 1 relationship differ · Restore all from 5 d ago      │
└─────────────────────────────────────────────────────────────────────┘
```

- **Choosing the points.** Select an entry and press ⌘D (Compare with now). Or select one entry
  and ⇧-click or ⇧J/⇧K to a second. A filled row marks each end. The comparison opens in the
  tab's lower split. It is part of the tab's state (article XIII), so it needs no tab of its
  own.
- **What it reads.** The node at each commit, one point read each (about 20 ms), and its
  relationships at each, read with the page's own relationship reads held at the commit. It
  does not use `GET /diff`. The measurements show a diff costs what the whole type changed in
  between (13.2 MiB across the load, for one changed node). Two point reads show the same net
  change at the cost of one node.
- **What it shows.** Only rows that differ, with "Show all" for the rest. Both sides use the
  review's value forms. A long text shows as a line diff.

### Restore a value through the branch

The rules come from article IV and the branch workflow RFC: every write names its branch, and
the server's policy decides.

1. **↺ Restore** on a value row, an entry's value, or "Restore all from *point*" puts the old
   value into the **page's draft for the window's branch** (spec 014, one draft per branch),
   bound to the value read now. Restore writes nothing. The page opens, or comes to the front,
   with the draft's dot on each restored row and the save line that already names its branch:
   "⌘S Save 3 values · write · main" (RFC surface 2).
2. **Saving** is an ordinary value-bound update, with its three outcomes (article III). The
   commit it makes is the newest entry in the history. The old entry stays where it was, so a
   restore is itself a change and can be put back the same way (Figma, Notion).
3. **When the window's branch takes no edits** (the access answer lacks `change`), the draft is
   still made, and the page's read-only line says so with the RFC's step: "You can read main,
   not change it · New branch…". The restore is not lost. Whether the draft moves to the new
   branch is an open question.
4. **What a restore covers.** A property: an update. A list: the whole old list, so that the
   person reads what they get. A relationship removed: an insert of the edge, with its old
   properties; a keyed edge upserts by its key. A relationship added: its removal, with
   relationship Remove's own confirm. A Blob: Studio reads the old bytes with `GET /blob` at the
   entry's commit and stages them as a Choose File would. A node removed (H5): **Create it
   again…** opens the new-node dialog filled with the before image's values. It lists the
   relationships that went with it, each checked to be added in the same mutation where the
   `@card` rules allow (spec 014's new-node dialog). It names the branch.

Rejected: **a restore that writes at once.** A save would not name its branch at the moment it
writes, which is gap 2 in the RFC. **Revert a whole commit from the node's history**: that is the
review's Revert (spec 008), and it acts on every node the commit touched.

### Pending work (H4)

Read in step 4 of the read plan. Only branches that change this node are listed. When none do,
the section is not drawn: no empty heading, and nothing moves when the read lands, because the
section's place is held only while the read is out (article V).

| Case | The line |
|---|---|
| A branch changes the node, and main has not since the merge base | `⑂ agentpending  title, tags · act-dev · 1 h ago · Review ⌘⇧R` |
| Both changed it | `… · Will conflict with main` in `Color::Conflict` |
| An unpublished commit or an unattached snapshot targets the branch (R2) | Not read. Studio writes none itself (spec 004). Left to a later plan |
| More than 20 branches | `Read 20 of 34 branches · Read the rest` |
| A branch diff failed or was refused (403) | The branch is named with the reason. A count is never guessed |

### Removed and created again (H5)

```
 + act-dev · created again · 2 d ago                                 4PZC ↗
     name Reborn  (1 value; the 3 relationships it had are not back)
 ┄┄ not on main for 10 commits, 4 min ┄┄
 − act-dev · removed · 2 d ago                                       77HE ↗
     name Yukihiro Wirth · title Title 99 · …
     − works at → Allen Labs   − knows → … (3 relationships went with it)
```

- The type and key are the node. A removal and a later insert of the same key are two entries
  of one history, with a dashed line saying how long the node was gone.
- **The node as it is now does not exist.** The tab opens from a commit's tab, a review card, or
  a search (it has no page at the head). Then "Now" is replaced by "Removed by act-dev in 77HE,
  2 days ago", with **Create it again…** and **Open as it was before ⌘↵**.
- The type's own `id` changes only when the type is dropped and added again. Across that, the
  history stops with "History stops at a schema change to Person" (below).

### Keys

Proposed. The plan settles them against the keymap test.

| Key | Action | Where |
|---|---|---|
| ⌘⌥4 | Open the node's History (`OpenViewAt` place 4, bound today) | Page, tree, canvas, table row, inspector |
| ⌥H | History of the focused property | Page, inspector |
| J / K | Next or previous entry (`menu::SelectNext`/`SelectPrevious`) | History tab |
| ⇧J / ⇧K | Extend the selection to compare | History tab |
| Space | Unfold or fold an entry or a run (`studio_changes::ToggleChange`) | History tab |
| ⌘↵ | Open as it was (`studio_changes::OpenChanged`) | History tab |
| ⌥⌘↵ | Open as it was before | History tab |
| ⌘D | Compare with now | History tab |
| ⌘⌥Z | Restore the selected value into the draft | History tab, compare pane |
| ⌘⌥N | Read the 200 commits before (`ReadOlderCommits`, as #146) | History tab |
| ⌘F | Focus the filter | History tab |
| ⌘R | Read again at the head | History tab |
| ⌘⇧R | Review the selected pending branch | History tab |

Every action goes in `studio_actions`, with a binding in `assets/keymaps/` and a palette entry
(AGENTS.md).

### States

The tab draws nothing until its first window is read. Entries arrive oldest first from the feed,
so drawing them as they arrived would push the newest down under the reader. A window of 60
commits is read first, then the rest of the 200, each drawn in one frame: about 0.7 s and 1 s
measured locally. New commits announce themselves as a chip, never as a jump (article V).

| State | What the tab says | Next step |
|---|---|---|
| Reading | `change_cards::reading_state`: "Reading the history…" | — |
| Slow (> 10 s) | "The server is still reading the history. A load in these commits makes this slower." | Wait, or Stop |
| Nothing in the window | "No commit in the newest 200 on main changed Ada Lovelace." | Read the 200 commits before ⌘⌥N |
| Nothing since creation | "Ada Lovelace has not changed since act-dev created it, 3 weeks ago." | — |
| The insert pass hit the byte cap | On the end row: "These commits hold a load of 50,000 Person. Their new nodes were not read, so a relationship added in them may be missing. Reading them costs about 13 MB." | Read them anyway |
| A schema change in the window | "History stops at a schema change to Person in X. Omnigraph does not compare across one." (409 `schema_boundary`; E52) | Read before the change, which starts a new feed after the boundary |
| History cleaned up | "History before X was cleaned up." (410) | None |
| Failed | `ds::error_state`: "The node's history could not be read", the reason, Try again. Entries already shown stay | Try again |
| Credential changed | Read again under the new token, as every view (architecture: `GraphSession`) | — |
| Opened from a page held at a commit | Read at the branch head as usual. A "You are viewing K1GB3NS7" line sits at that commit's place, and entries newer than it are dimmed | — |
| The window is on a branch | History is that branch's first-parent chain. A "*branch* started here" line marks the fork | — |

### Drawing

- The entries are a cached view of their own, made with the tab (`docs/drawing.md`), as
  `CommitRows` and `ReviewRows` are. The filter field draws beside it, never inside.
- A commit on another branch draws nothing. A commit that did not change the node draws only the
  toolbar's count.
- The tab answers `Item::caches_its_parts`.
- A draw-count test for J/K (one draw a key), for the filter (typing draws the list zero times,
  the filter landing once), and for a window landing (once).

## What waits on the engine

| ID | With it | Today |
|---|---|---|
| **E62** ([#1472](https://github.com/ModernRelay/omnigraph/issues/1472)) the commits that changed one entity, paged, merged-in commits with their own actors | One paged request per node with no window and no byte cap. A merged change names who made it on the branch. Blame is exact at any depth | Windows of 200 commits, two feed passes, a byte cap with its honest exit, and a bounded walk for "who on the branch" |
| E62, extended (to propose on #1472) | Each change names the properties it changed. A `property=` filter, like `git log -L`. The newest change of each property as one answer (blame) | Studio compares the two images. Blame comes from the fold, and says "set before X" past the window |
| **E60** ([#1470](https://github.com/ModernRelay/omnigraph/issues/1470)) a commit's operation and message, and who created a branch | "close", "load", "verify_claim" and a writer's message on every entry. "Opened by" on a pending branch | Studio's own words for its own writes this session. Otherwise nothing |
| **E20** ([#978](https://github.com/ModernRelay/omnigraph/issues/978)) delegated agent credentials, recorded beside the actor | "orion (agent) for act-ragnor" | "agent" only from an `agent/<stamp>` branch or Studio's own log |
| E44 counts in the commit listing | "Hide loads" knows a load from the listing | One page of 100 changes per drawn commit, as History reads |
| E59 a Blob in an image by size and digest | A Blob change costs bytes for its metadata, not its file | Images carry the file as `base64:` text, so a Blob-heavy type makes the feed heavy |
| E52 the schema at a commit | History continues across a schema change | It stops there and says so |
| E35, E38 | Faster pending reads across merges; a feed page bounded below 128 commits | As measured |

E60 and E62 are filed as Omnigraph issues and cited in the cookbook study. On `main` they are
not yet in spec 012 or `docs/deferred-engine-changes.md`, since #146 closed. The plan that builds
this adds them there, with the extension to E62.

## Principles

| Article | How the design holds it |
|---|---|
| I | Every read goes through the session and `graph_client`. Studio keeps no history of its own beyond the read memo and this session's outcome log |
| II | Each window answers for the head its first listing page resolved. Compare reads two named commits |
| III, IV | A restore is a draft. Its save is an ordinary write that names its branch, and the policy decides |
| V | No entries before a window is whole. New commits arrive as a chip. The "Not merged" section's place is held while it is read |
| VI | Each disabled action says why. The byte cap says what it skipped and what reading it costs |
| VIII | Every action has a key and a palette entry |
| IX | 200-commit windows, an 8 MiB cap for each window, at most 20 pending branches, at most 50 commits in a merge walk |
| X | Designed against 0.14 and 0.15. The waits are E60, E62 and an extension of E62 |
| XIII | One tab, a view of one node. The page and inspector only point to it. Compare is state of the tab |

## Open questions

1. **Blame on the page itself.** Should the page draw a "who set it" column for every property
   while a toggle is on (GitHub's Blame mode)? It would need the history read, and article XIII
   says a toggle may only redraw state the view already has. This study puts blame in the tab's
   "Now" block, with a line and tooltips on the page only once the read is held.
2. **Does a restore draft follow "New branch…"?** Drafts are kept per branch (spec 014). When
   main refuses the save and the person makes a branch, should the draft move with them? That
   depends on the RFC's open question (the write target per window or per tab).
3. **The fold window.** Is 10 minutes, one actor and one branch right for agents that write
   hundreds of commits an hour? An agent's run could fold by its `agent/<stamp>` branch instead.
4. **The byte cap.** Is 8 MiB per window right? Measure it on the 230,000-person seed and a
   remote server before the plan sets it.
5. **Pending branches on a busy graph.** With dozens of agent branches, is "the 20 most recently
   written" the right bound? Should the section wait for E35's fix rather than read a diff per
   branch?
6. **Binary search for creation and blame.** It costs 9 point reads for 464 commits, but it is
   not exact when a value went back to an earlier value. Use it only for "created", or also for
   blame past the window, labelled as "a commit that set this value"?
7. **Relationship history from the other end.** "Knows" from Grace to Ada is in both histories.
   Should an edge change name which end made it, given that the image does not say?

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-20 | The routes and their parameters | `crates/graph_client/openapi/omnigraph-0.15.0.json`: paths and the schemas `ChangeFeedOutput`, `ChangeBlockOutput`, `ChangeCauseOutput`, `EntityChangeOutput`, `ChangeImageOutput`, `ChangeTypeOutput`, `CommitOutput`, `DiffOutput` | As stated under *The routes*. No `id` filter on any change route. `CommitOutput` has no operation or message |
| 2026-11-20 | The cost table | The scripts in [`2026-11-20-node-history/`](2026-11-20-node-history/) (`fixture.py`, `measure.py`, `measure_bulk.py`) against `script/dev-server` with `STUDIO_DEV_DATA` and `STUDIO_DEV_BIND=127.0.0.1:8797` (port checked free with `lsof`), `omnigraph-server` `edge-20261030-1a3e7ed`, a Python client over HTTP with `dev-token`; fixture as described; Apple M5 Pro, load average 5 to 6; the server stopped afterwards | As in the table. 464 first-parent commits; 28 changes to `person-000007` across its type and three relationship types |
| 2026-11-20 | A merge reads as the merger's block | The feed's block for the merge of `featmerged`: `actor_id: act-dev`, `authored_branch: main`, `merged_parent_commit_id` set, the title change inside it | As stated |
| 2026-11-20 | A removal takes the node's relationships, and a new insert of its key brings none back | The feed for `person-000099`: `insert`, `delete` (its relationships removed with it), `insert`, with the same `type.id` throughout | 3 `Person` changes, 21 with relationships |
| 2026-11-20 | A branch diff nets a branch's commits into one change | `GET /branches/diff?source=agentpending&target=main`: two commits (title, then tags) | One `update` to the node, carrying both |
| 2026-11-20 | The detailed branch list | `GET /branches?detailed=true` on this edge server | Plain names (the feature landed in 0.15.0), so the pending read here listed every branch |
| 2026-11-20 | Other tools' history UIs | Each vendor's help page; Linear's from its changelog, since its docs had no history page | As in the table. Not checked in the running products |
| — | Not checked | A 0.15.0 server, a remote server's latency, the 230,000-person seed, a Blob-heavy type, R2 commits | Open questions 4 and 5 |

### Sources

- Notion, [Duplicate, delete and restore content](https://www.notion.com/help/duplicate-delete-and-restore-content)
- Figma, [View a file's version history](https://help.figma.com/hc/en-us/articles/360038006754-View-a-file-s-version-history)
- Linear, [Collapsed issue history](https://linear.app/changelog/2025-04-03-collapsed-issue-history) (changelog, 2025-04-03)
- GitHub, [Viewing and understanding files](https://docs.github.com/en/repositories/working-with-files/using-files/viewing-and-understanding-files) (blame) and [Differences between commit views](https://docs.github.com/en/pull-requests/committing-changes-to-your-project/viewing-and-comparing-commits/differences-between-commit-views)
- Git, [`git log`](https://git-scm.com/docs/git-log) (`-L`, `-S`, `--follow`)
- Airtable, [Record-level revision history](https://support.airtable.com/docs/record-level-revision-history-overview)
- Google, [Find what's changed in a file](https://support.google.com/docs/answer/190843)
