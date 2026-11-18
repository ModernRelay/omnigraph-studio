# 031: Finding a node by what people call it

Status: built 2026-11-16 · drafted and measured the same day

## Problem

Ragnor reported (2026-11-16) that "the search of nodes seems to often not retrieve correctly".
An investigation that ran real searches on three local graphs and, read-only, on the
ModernRelay operating graph found five causes, each confirmed in the running app (the scenario
runner, real HTTP):

1. ⌘P read only the first 8 node types (`TYPES_PER_SEARCH`); the rest waited for "Search more
   types". The schema model lists types by name, so on the operating graph the first 8 were
   ActionItem to File, and Org and Person came in the third batch, two clicks away: 0 of 183
   Org and Person names typed exactly as stored were found at the first pause.
2. Matching was case-sensitive (`contains` with four spellings of the whole text) and needed
   the words in order: "bluequill" missed "BlueQuill", "mcallister" "McAllister", "acme corp" "ACME
   Corp", "garcia jose" "José García".
3. Five rows per type ordered by `@id`, with no ranking: "acme" never showed ACME Corp, whose id
   sorts after six `acme-*` ids.
4. Only the property naming the type was searched: choosing email to name people made names
   unsearchable, and emails, websites and statements were never searched.
5. A type whose read failed showed as "No node matches".

The AI filter's "related to <name>" also took the first match by id: "Smith" became Adam
Smithson.

## Principles

- I: the server stays the authority. A type's name index is made of an answer read at a
  commit, and stands at another only when the change log says no node of its type changed
  between them (spec 015's carry); with no head known, for one head poll at most.
- II: a search reads the branch's head, or the commit of the view that asks (a picker).
- V: rows found later come below those shown; a batch of answers lands ranked in one draw.
- VII: a failed type says so, with Retry; nothing is retried by itself.
- IX: bounded: an index is at most 5,000 nodes and all of them together at most 32 MiB of text
  (each text kept to 1,000 characters); a larger type is searched on the server, 25 rows at a
  time.
- X: designed against today's API; E49 to E51 record what the engine could do instead.

## What exists today (checked)

- Studio `main` at `ab19cfd4`: `command_palette.rs` `schedule_node_search`, `read_types`,
  `TYPES_PER_SEARCH = 8`, `NODES_PER_TYPE = 5`, `search_order`; `found.rs` narrowing;
  `gq_language::builder::node_search`, `text_match`, `spellings` (four spellings of the whole
  text); `exploration.rs` `search_group` through `builder::neighbours`; `type_table::suggest`
  (the add-relationship dialog, Peek's add, the filter's value field) and `table.rs` (the AI
  filter's "related to", first row taken).
- Omnigraph 0.14 and 0.15 (`omnigraph skill show references/queries.md`, "Filter Operators";
  `query.pest` at `v0.14.0` and main `4f7c2076`): `starts_with` and `contains` are exact and
  case-sensitive; nothing folds case. `search`, `fuzzy`, `match_text` and `bm25` exist since
  v0.14.0, fold case and stem, and need a one-column `@index` or `@key` on the String (`T27`
  otherwise); `search` words are OR'd, so it is useful only ranked by `bm25`; it matches whole
  words, not a word's middle. `Property::full_text` already tells Studio which properties have
  the index.
- The change feed is asked only about the types views read through a scope
  (`GraphSession::read_names`); a read through no scope is not carried to a later commit.

## Design

### What a search matches

`builder::searched_properties`: the property chosen in Settings to name the type's nodes, the
schema's name for them (`name`, `title`, `display_name`, `label`), then properties whose name
holds `email`, `mail`, `website`, `domain`, `url`, `handle` or `username`; at most 4, never the
key (the id is always matched).

Text is folded (`graph_store::node_search::fold`): decomposed, accents and apostrophes dropped,
lowercase, every other run of non-letters one space. A node matches when its id or a searched
property, folded, ranks; best first:

| Rank | When |
|---|---|
| Exact | the text is the property's text |
| Prefix | the property starts with the text |
| WordStart | every word typed starts a word of the property, in any order |
| Substring | every word typed is in the property, or each in one of the node's texts |
| FullText | only the engine's full-text search found it |

Ties: a match on the name or id before one on another property, then the shorter text, then
the id.

### Where the matching happens

- **A type of at most `INDEX_MAX_NODES` = 5,000 nodes** is matched in memory against its name
  index: every node's id and searched properties, read with `builder::node_index`
  (`studio_search_index`, `limit 5001`) at the commit searched. The rows are read fresh
  (`GraphSession::read_fresh`, through the search scope) and kept nowhere but in the index made
  of them: not also in the read memo. Reads of one index at one commit are shared while out. An
  index held at one commit stands for another when the branch's change log says no node of its
  type changed between them. With no head known, the commit a head read answered at stands for
  the head for `HEAD_UNKNOWN_HOLD` = 30 s, one head poll, so pauses meanwhile read nothing.
- **The budget.** Indexes are held by type and commit, at most `INDEX_BUDGET_BYTES` = 32 MiB in
  all (ids and texts, each kept as read and folded, plus 128 bytes a node; an index several
  commits share counts once), the least recently used first out. An index that alone would
  exceed the budget is never held: its type is searched on the server. A type found too large
  holds a marker that counts 256 bytes, so markers are bounded with the indexes. Each text keeps
  `INDEX_TEXT_CHARS` = 1,000 characters, so a long title still matches whole.
- **Forgetting.** A new token or schema forgets every index and every index read still out
  (a generation they carry moves): a read that lands after it changes nothing. The session
  already answers a read sent under another token with `stale_credential`.
- **A larger type** (by its count, or an index read answering more than 5,000 rows) is searched
  on the server with `builder::node_search` (`studio_find`): each word, in any of four spellings
  (as typed, lowercase, Capitalized, UPPERCASE), in the id or a searched property; 25 rows by
  id, ranked as above. Text of two words or more is also asked for whole
  (`builder::node_phrase_search`, `studio_find_phrase`, with Each Word Capitalized too): each
  word alone matches many more nodes than the phrase, and the 25 by id would leave a name typed
  in full out. Its rows come first; every row the server matched is listed, those the ranking
  cannot place last.
- **Full-text.** Beside either, the engine's `search` ordered by `bm25`
  (`builder::node_text_search`, `studio_find_text`, 25 rows) runs on properties with a
  full-text index: for a held type, the first such property its index does not hold (a
  statement, a description); for a larger type, its indexed searched properties, then its key,
  at most 2. Its hits come after the ranked matches, each node once. A full-text read that fails
  is left out; the type still answers.
- **Failure.** A type fails only when its index read or its server search fails: the palette
  lists "<Type> could not be searched", which Retry searches again (a read).

### The palette (⌘P)

- Every node type is searched, except those counted empty; no "Search more types".
- After the 150 ms pause, one `find_nodes` per type. Answers land together, ranked by rank, then
  the shorter name whatever its type, then by type (named types first), within 30 ms of each
  other (`LANDING_DELAY`), or at once when the last type answers; rows already shown keep their
  place.
- Each type shows its best `NODES_PER_TYPE` = 5; one with more offers "More in <Type>…", which
  shows up to `NODES_EXPANDED` = 25 and keeps the palette open, the highlight on the first row it
  brought. At most `NODES_SHOWN` = 60 rows, besides those of a type the person asked more of.
- A row matched on a property other than the name ends with "<Type> · <property>", and the
  footer says "Matched <property>: <text>".
- While open, the palette holds a read scope (`ReadView::Search`), so the change feed is asked
  about the types searched and their indexes are carried across commits that leave them alone.
- Request budget: the first search on a graph reads one index per type (8 at a time, the
  client's slots); every later text sends nothing for a held type but its one full-text read
  where it has an indexed property the index does not hold, and for a larger type its search,
  its phrase (two words or more) and at most two full-text reads. A second palette at the same
  commit reads no index. On the operating graph: 35 index reads once a commit (630 KB of rows,
  7,869 nodes; the largest, Span, 2,625), then 6 full-text reads a pause (ActionItem, Claim,
  Decision, ExternalID, File, Span), where the old search sent 8.

### Pickers and the relationship tree

- The add-relationship dialog, Peek's add and the filter's value field use the same search,
  8 rows, at their view's commit.
- The tree's search inside a relationship group uses the server form (`builder::neighbours`
  with `node_search`'s predicate): each word on its own, in the id or the searched properties.

### The AI filter's "related to"

A name names a node only when one matches, or one alone is it exactly by its id or name, not by
an email or handle that happens to equal it (`suggest::named`). When
several match, nothing is filtered by it: its filter opens as a draft with the name in its
value field and the matches offered, and the sentence's note says "N Org nodes match "Acme":
pick one in the filter." When no draft can open (a key the filter bar cannot test "related to"
on, one reached through a relationship's group), the note names the choices, by name and id,
and says to name one exactly or add the filter by hand (`suggest::pick_note`). A node only
full-text search found is never taken alone; it is offered as a choice. When none match, or the
search fails, the note says so.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Match in memory, per type, from an index read once per commit | Wait for E49 or E51; more spellings | The engine has no case-insensitive test; every ModernRelay type is under 5,000 nodes; one read per type and commit costs less than a read per type per pause did |
| ~~Read the index through the read memo~~ Read it fresh and carry it by the change log (review of #124) | The read memo, which also kept the rows | The memo held every index's rows beside the index made of them, outside the index budget; the change log answers the same question the memo's carry asked |
| Hold the head read's commit for one head poll when no head is known | Read every pause; serve the last index whatever its age | Every view goes as long without asking; a pause's reads were otherwise all sent again |
| A scope while the palette is open | A session-wide scope | A session-wide scope would ask the feed about every type on every head move; the palette's lasts only while it is open |
| Full-text after the ranked matches | Interleaved by score | `bm25` scores do not compare with the folding ranks; the folded matches are what the person typed |
| Full-text only where an index is declared | Call it everywhere and catch `T27` | A refused call costs a read for nothing |
| 5,000 nodes per index, 32 MiB in all, 1,000 characters a text | Unbounded; a node budget; 200 characters a text | 5,000 covers every type of the operating graph (the largest, Span, has 2,625). A budget in bytes bounds memory whatever the texts' length. 200 characters cut 45 of 725 ActionItem titles, and typing one of them in full then found nothing (measured: 284 of 300 whole titles found at 200, 300 at 1,000) |
| Ask for a multi-word text whole too on the server | Words alone | Each word alone matched more rows than the 25 read; a whole title on the dense graph's 7,500 Mentions was found 296 of 300 times by words alone, 300 with the phrase |
| A row matched inside a word ranks below one matched at a word's start | Id order within a rank band | The person typed a word's start far more often than its middle; measured, a four-letter middle loses one or two finds a type on the operating graph, and every other kind of query gains |
| Answers land in batches | Each as it comes | A type answering a few ms after another would push the best match below weaker ones |
| "More in <Type>…" shows more in the palette | Opening the type's table filtered | Keeps the keyboard flow; the table's filter is exact and would not rank |
| An ambiguous "related to" opens the filter with the matches | Take the first; refuse it | A name the model was given names one record; the person knows which |

## Deferred

- [E49](../012-omnigraph-engine-changes/plan.md): a substring test that ignores case and
  accents, so a type too large to hold matches as a held one does.
- [E50](../012-omnigraph-engine-changes/plan.md): one request that searches across types.
- [E51](../012-omnigraph-engine-changes/plan.md): full-text search without an index per
  property, and inside words.

(`docs/deferred-engine-changes.md` lists all three.)

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-16 | The five causes above happen in the app | The scenario runner (`crates/studio/examples/snapshot.rs`, `studio::GoTo`, `type:`, `key:enter`) against a CRM-shaped graph served by `omnigraph-server` (main `4f7c2076`, contract 0.14); a proxy answering 503 to one type's search | "bluequill": "0 nodes · Search 7 more types", BlueQuill only after the click; "acme": five `acme-*` organisations and no ACME Corp; "acme corp", "mcallister", "jose garcia", "bluequill-partner": No node matches; Person named by email: "Sam Okafor" No node matches; the 503: No node matches |
| 2026-11-16 | Findability before and after, per type | Every node's id and name read once (read-only on the operating graph), each searched for in eight ways; before: a port of `builder::node_search` checked against the server's own answers (14 of 14 the same); after: `graph_store::node_search` itself over the same rows | See "Before and after" |
| 2026-11-16 | Full-text ranks what `contains` misses | 25 random ActionItem titles on the operating graph, `search` + `bm25`, top 5 | Lowercase title 25/25 (contains 7); words reversed 24/24 (0); four letters inside a word 0/22 (15) |
| 2026-11-16 | Every operating-graph type fits an index | `count($n)` per type, read-only | Largest: Span 2,625, ExternalID 1,378, Source 1,030 |
| 2026-11-16 | The new search works in the app | The scenario runner against the CRM-shaped graph, first pause, no click | "bluequill": BlueQuill first; "acme": ACME Corp first, then the Acme companies, and "More in Org…"; "acme corp", "mcallister", "garcia jose", "bluequill partner", "smith" (both Smiths before Smithson): each found; "anna.woehler@example.com": Anna Wöhler, "Person · email"; a 503 on Org's index: "Org could not be searched · Retry" among the other types' rows |
| 2026-11-16 | Names in this plan | — | The companies, people and addresses named in the problem and the log are fictional stand-ins for the fixture's, with the same case, accents, punctuation and word order |
| 2026-11-16 | Review of #124: an index read out when the token changes is kept | `counted_read` (`session.rs`) answers a read sent under another token with `stale_credential`, so its rows were never kept; `node_search` `an_index_read_out_when_the_token_changes_lands_nowhere` | The finding's kept rows could not happen; its landing did remove the new token's read of the same index from those marked out, so the next pause read it again. Fixed with a generation; the test fails without it |
| 2026-11-16 | Review of #124: with no head known, every pause read every index | `with_no_head_known_a_pause_after_reads_nothing` (two pauses, one read; past the hold, a second) | Confirmed and fixed |
| 2026-11-16 | Review of #124: the budget was not strict | `an_index_over_the_budget_is_searched_on_the_server`, `markers_count_against_the_budget`, `an_index_is_held_once_not_also_in_the_read_memo` | Confirmed: an index over budget was kept, markers counted nothing, and the memo kept each index's rows too. Fixed; the carry test `an_index_is_read_once_a_commit_and_carried_past_other_types` passes through the change log |
| 2026-11-16 | Review of #124: the note said "pick one" with no draft open; a full-text-only match was taken | `type_table` `a_name_that_names_several_nodes_asks_which` (a key with no "related to" opens none), `suggest` `a_note_names_the_choices_when_no_filter_opens`, `a_name_names_one_node_only_when_one_matches` | Fixed |
| 2026-11-16 | Before and after on four graphs | A harness outside the repository: each type's `studio_search_index` rows read once, the old predicate applied to them, and a temporary test running `NodeIndex::find` and the palette's ordering over the same rows | See "Before and after" |

### Before and after

Every node of each measured type (300 sampled on the dense graph) searched for in each way; a
find is the node among its type's five shown. "Palette" is the operating and CRM graphs' whole
⌘P at the first pause: before, the first 8 types only; after, every type ranked together, 60
rows. Before: the old predicate (a port checked against the server's own answers, 14 of 14);
after: `graph_store::node_search` itself over the same rows (`NodeIndex::find`, and for the
dense graph's Mention, 7,500 nodes, the server path's rows).

ModernRelay operating graph (read-only; Org, Person, Opportunity, ActionItem (300 sampled),
Meeting, Conversation, Product, Initiative):

| Typed | In its type, before | After | Palette, first pause, before | After |
|---|---|---|---|---|
| exactly as stored | 770/770 | 770/770 | 357/770 (46%) | 770/770 |
| whole name, lowercase | 321/770 (42%) | 770/770 | 58/770 (8%) | 770/770 |
| first word, lowercase | 545/770 (71%) | 718/770 (93%) | 200/770 (26%) | 718/770 (93%) |
| last word, lowercase | 560/769 (73%) | 616/769 (80%) | 210/769 (27%) | 615/769 (80%) |
| first word, UPPERCASE | 573/770 (74%) | 718/770 (93%) | 209/770 (27%) | 718/770 (93%) |
| four letters inside a word | 425/621 (68%) | 463/621 (75%) | 184/621 (30%) | 460/621 (74%) |
| words reversed | 0/640 | 640/640 | 0/640 | 640/640 |
| accents and punctuation left out | 1/17 | 16/17 | 0/17 | 16/17 |
| email (Person) | 0/39 | 39/39 | 0/39 | 39/39 |
| domain, the key (Org) | 95/95 | 95/95 | 0/95 | 95/95 |
| **All** | **3,290/5,261 (63%)** | **4,845/5,261 (92%)** | **1,218/5,261 (23%)** | **4,841/5,261 (92%)** |

A CRM-shaped graph built for the measurement (15 types, as the operating graph has them, with 57 organisation names and 30 people with accents, apostrophes and emails): in its type 588/761
→ 756/761; palette at the first pause 36/761 (5%) → 756/761 (99%).

Development graph (`script/dev-server`): 1,000/1,895 → 1,434/1,895; email 0/128 → 128/128,
words reversed 0/265 → 265/265. Dense graph (`script/dense-server`, names drawn from small
pools, so a first name matches hundreds of nodes and the cut is a lottery before and after):
2,874/10,086 → 4,289/10,086; words reversed 11/1,397 → 1,286/1,397, whole name lowercase
1,255/1,500 → 1,334/1,500.

What stays missed: a single common word (Conversation names all end in the same person's name;
"lowercase last word" 8/57 before and after), and the dense graph's duplicated names. "More in
<Type>…" shows 25 of a type.

## Corrections
