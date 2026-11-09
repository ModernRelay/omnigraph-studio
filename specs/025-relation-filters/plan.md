# 025: Filters through relationships

Status: built 2026-11-08 (steps 1–3; step 4's timings pending) · draft 2026-11-08

## Problem

A type table filters by a property of a related record one hop away: "worksAt → city is
Berlin" (spec 004). Each such pill is its own search, so a table cannot say:

- **Two tests on the same related record.** "worksAt → city is Berlin" and "worksAt →
  employees > 100" keep a person who works at a small Berlin company and at a large company
  elsewhere. Most people mean one company that is both.
- **A property of the relationship itself.** "worked at Acme since 2020" or "role is CTO" is a
  property of the `WorksAt` edge, not of the person or the company. The table's menu does not
  offer edge properties.
- **A further hop.** "people whose company is in Germany" goes from Person to Company to City.
- **How many related records match.** None of them ("has no company in Berlin"), every one of
  them, at least two, or a total over them ("total hours on active projects over 100").

All four need "Condition (GQ)" today, which means writing GQ by hand. The filter from a
sentence (spec 019) also sends all four to that condition, so they arrive as GQ the person
cannot read or change with the pills.

This plan makes a relationship filter a **group**: a quantifier, the relationship, and the
tests one related record must pass. The tests can be on the record, on the edge, or another
group one hop further. The sentence filter answers in groups too.

## Principles

- I: the server judges every group. Studio never tests a group against the rows it holds.
- III: unchanged. Groups only read; editing a row keeps its row in place.
- V: rows stay put. After an edit, a row is kept until the next read. This is how relationship
  filters already behave (`model::row_matches`).
- VI: every group is a pill you can open, edit and remove, and its tooltip spells out the whole
  condition.
- IX: reads stay bounded. Groups nest at most three hops deep, and each one becomes one
  correlated block in the page query.
- X: designed against 0.14, and every shape below type-checks on the CLI today (Validation
  log). No engine change is needed.

## What exists today (checked)

Studio at `43176b80`; Omnigraph CLI `0.14.1-edge+269299b`.

- **Filter.** `Filter { property, op, value }` (`crates/gq_language/src/builder.rs:608`).
  `FilterOp` has the comparisons, the date presets, `HasAny`, `HasNone`, `RelatedTo` and `Expr`
  (`builder.rs:186`). Views save filters by `FilterOp::key`.
- **Keys.** A key is a property (`title`), a relationship (`out:WorksAt`), a neighbour's
  property (`out:WorksAt.city`) or an edge end's property (`from.title`), all parsed by
  `parse_group_key` (`builder.rs:455`). Property names hold no `.`, `:` or `@`.
- **Building.** `filter_clauses` (`builder.rs:694`) turns each filter into one clause. A
  neighbour's property becomes its own `exists { $r{i}: T  $n edge $r{i}  test }`, or a `not`
  block for "is empty" (`neighbour_property_clause`, `builder.rs:910`). Two such filters are
  two blocks, each with its own record. Edge properties can't be filtered from a node table.
- **Menu.** The filter menu nests each relationship's neighbour properties
  (`table.rs:695`, `:7638`). It doesn't list edge properties or the related type's own
  relationships.
- **Rows after an edit.** `row_matches` skips relationship and neighbour-property filters, so
  the engine judges them on the next read (`crates/type_table/src/model.rs:810`).
- **Sentence filter.** `filter_words` builds the answer's schema: `filters[]` with `property`
  as an enum of the keys offered (the type's properties, its relationships and
  `key.<property>`), `op` as an enum, `value` as a list of strings, and a free `predicate`
  (`crates/filter_words/src/filter_words.rs:80`). Its instructions tell the model to put "or,
  not, a count, two tests on one related record or more than one hop" in the predicate
  (`filter_words.rs:32`). The input lists each related type's properties once and stays under
  48 KiB. The personal graph's Artifact needs about 33 KiB (spec 019, 2026-11-01).
- **GQ at 0.14** binds an edge row with `$n $w:worksAt $r` and filters on `$w.since`. It nests
  `exists`, `not` and `count { } >= n` blocks, and takes `sum(...)`, `min`, `max` and `avg`
  over a block (Validation log).
- **Copy as stored query** (spec 024) builds its text with `filter_clauses`, so a filter the
  table can build can also be copied.

## Design

### The group

A group is one filter. Its op is the new `FilterOp::Where` (key `where_related`, label
"where"), its property is a relationship key relative to the record the group is tested on
(`out:WorksAt`), and its value is:

```json
{ "quantifier": "any", "count": null,
  "tests": [ { "property": "city", "op": "eq", "value": "Berlin" },
             { "property": "@since", "op": "gt", "value": "2020-01-01" },
             { "property": "out:LocatedIn", "op": "where_related",
               "value": { "quantifier": "any", "count": null,
                          "tests": [ { "property": "country", "op": "eq", "value": "Germany" } ] } } ] }
```

- **Tests** are ordinary filters relative to the related record. A key that starts with `@`
  names a property of the edge (`@since`), which is safe because no property name holds an
  `@`. A test can be another group, one hop further, or "has any", "has none" or "is related
  to". A group is at most three hops deep. Past that, the menu offers "Condition (GQ)".
- **Quantifiers:**

  | Key | Pill reads | GQ |
  |---|---|---|
  | `any` | worksAt → **any** Company where … | `exists { … }` |
  | `none` | worksAt → **no** Company where … | `not { … }` |
  | `every` | worksAt → **every** Company … | `exists { $n worksAt $_ }` and `not { … not { tests } }` |
  | `at_least` *n* | worksAt → **at least 2** Companies where … | `count { … } >= $n` |
  | `at_most` *n* | worksAt → **at most 2** Companies where … | `count { … } <= $n`, once validated |
  | `sum`/`avg`/`min`/`max` of a property, compared | worksOn → **total hours** of Projects where … **> 100** | `sum($r.hours) { … } > $v` |

  `every` holds only for a row with at least one related record. A row with none is not "every
  company in Berlin". An aggregate quantifier's value adds `"of"` (the property name, or an
  `@` edge property), `"compare"` (an op key) and `"than"` (the value), and is offered only on
  numbers, plus dates for `min` and `max`. Its key and title match spec 023's rollup columns,
  so a rollup column's "filter by this" makes the same group.
- **The old form stays.** `out:WorksAt.city is Berlin` is still read, built and saved as it is
  today, and it means the same as an `any` group with one test. A group of one test, `any`,
  with no edge property and no hop, is saved in the old form, so views written before this plan
  and views written after it compare equal, and E42 has one less shape to share.

### Building it

`filter_clauses` gains a `FilterOp::Where` arm that calls a new recursive
`builder::group_clause(schema, near_var, near_type, depth, filter, params)`:

- The related record is `$r{path}` (`$r0`, `$r0_1` for the first hop inside group 0, and so
  on), so the names never collide. The edge is bound as `$w{path}` only when a test names an
  `@` property, because a bound traversal returns one row per edge (GQ's rule), which changes
  nothing inside a block.
- Each test becomes a clause on the related record, built by the same `property_clause`,
  `relationship_clause` and `group_clause` code the row's own filters use. A parameter is named
  from its path, `$f{i}_{j}`.
- **"Is empty"** on a related record's property is `$r.p is null` inside the block. The one-hop
  form keeps its current meaning ("no related record has a value") because that form doesn't
  change.
- The page, the count, a section's count and grouping take the group as they take any filter,
  through `filter_clauses`. `filters_without` (`builder.rs:963`) drops a group by its key, as it
  drops any filter.
- **Copy as stored query** (spec 024) names a group's parameters from the path:
  `$works_at_city`, `$works_at_located_in_country`, with the same longest-first renaming.

### Adding and editing a group

- **Menu.** Each relationship's submenu in the filter menu reads, in order:
  1. The relationship itself: "has any", "has none", "is related to".
  2. "**worksAt itself**": the edge's properties (`since`, `role`), when it has any.
  3. "**Company**": the related type's properties, as today.
  4. "**Company's relationships ›**": the related type's relationships, each opening the same
     submenu one hop further. This entry is left out at the third hop.

  Picking a property adds a draft test exactly as today. A draft whose test is on an edge
  property or a further hop applies as a group of one.
- **The group pill.** It reads "worksAt → any Company where city is Berlin · employees > 100".
  Past the pill's width it is cut, with the full sentence in its tooltip. A click opens the
  group's editor below it, a popover built from the filter bar's own parts:
  - the quantifier as a dropdown (any, no, every, at least *n*, at most *n*, total, average,
    smallest or largest of a property);
  - the tests as pills, each removable and editable as a pill in the bar is, a nested group as
    a pill that opens its own popover;
  - "+ Add a test", which opens the relationship's submenu above, rooted at this record.

  ↵ applies the group and Esc puts it back as it was, as editing a pill does today
  (spec 004). Removing the last test removes the group.
- **Combining pills.** Two one-hop pills on the same relationship stay two records, the
  meaning they have today. Each pill's menu has "**Same Company as …**", which merges it into
  the other pill's group, and a group's test menu has "**Separate**", which moves the test back
  to the bar.

### The sentence filter (spec 019)

The answer can now say all four things in groups, so the predicate is left for `or` and
anything else the pills can't express.

- **The answer's schema** gains a `groups` list, whose items have:
  - `id`: a short name, such as `"g1"`;
  - `parent`: `""` for the row, or another group's `id`;
  - `relationship`: a relationship key relative to the parent's record;
  - `quantifier`: one of `any`, `none`, `every`, `at_least`, `at_most`, `sum`, `avg`, `min`,
    `max`;
  - `count`: the *n*, or null;
  - `of`, `compare`, `than`: the aggregate's property, comparison and value, or empty.

  Each filter gains `group`: `""` for a test on the row itself (today's filters), or the `id`
  of the group whose record it tests. The schema stays flat, with no `$ref` recursion, so the
  providers' strict modes take it. A filter's `property` stays an enum of the one-hop keys and
  gains the edge keys (`out:WorksAt.@since`). Inside a group, the filter names the full key
  from the row, and the group says which record it tests. A test two hops away
  (`out:WorksAt.out:LocatedIn.country`) is a string, checked when the answer comes back,
  because listing every two-hop key would pass the cap.
- **Instructions.** The sentence about the predicate becomes: "Put tests on one related record
  in one group; use a group's quantifier for none, every, at least, at most and totals, and a
  group inside a group for a further hop. Put only `or`, and anything groups cannot say, in
  `predicate`."
- **Input.** Each relationship line adds its edge's properties, said once per edge type.
  Each related type's relationships are listed by key and target type, once per type, after its
  properties. If the input would pass the cap, the related types' relationships are dropped
  first, and the input says so ("Further hops: not listed for this type"), as it already does
  for the related types' properties.
- **Checking the answer.** `filter_words::Answer` turns groups and their filters into `Where`
  filters through the same checks a typed filter goes through, at every level. A group whose
  relationship the record's type lacks, a hop past three, or a quantifier the property's type
  can't take is dropped and named ("Could not use: worksAt → located in"). A filter whose
  `group` names no group is dropped. A group left with no test is dropped too, except under
  `none` and `every`, where no tests means "has none" and "has any".
- **The tree** (spec 005) uses the same crate to filter a relationship's neighbours. It
  doesn't offer groups in this plan (Deferred), so its schema leaves `groups` out, as the
  schema already leaves out a part with no choices.

### Build order

1. `FilterOp::Where`, `group_clause` and the old form's equality: builder tests that every
   quantifier, an edge property, two and three hops, and the old form all type-check on the
   dev schema, and that copying as a stored query names the parameters.
2. The menu's edge properties and hops, the group pill and its popover, "Same Company as …"
   and "Separate".
3. The sentence filter's groups.
4. Measure on the personal graph (Validation, below) before shipping 2 and 3.

## Decisions

- **One group filter, not a set of linked filters.** A group is a single `Filter` whose value
  holds its tests, so the bar, views, `filters_without`, the tab's state and E42 handle it like
  any other filter. Rejected: a `group` id on each filter in the bar, which every place that
  removes, reorders or compares filters would have to keep consistent.
- **Two pills stay two records.** Merging them silently would change what an existing view
  returns. "Same Company as …" makes the change visible and deliberate.
- **The sentence answer is flat, with ids.** A recursive schema needs `$ref`, which not every
  strict mode takes. Ids and parents say the same thing in a shape every provider accepts, and
  the check rebuilds the tree.
- **Edge properties as `@name`.** They need a key distinct from the related type's properties
  (`WorksAt.role` beside `Company.role`). `@` can't be in a property name, and `@id` already
  reads as "the record's own".
- **Three hops.** Each hop is a nested block the engine runs per row. Three covers "person →
  company → city → country". Anything deeper is rare enough that the GQ condition covers it.
- **`every` excludes rows with none.** "Every company is in Berlin" being true of a person with
  no company surprises people. "has none" says that case.

## Deferred

- `or` between groups or tests. *Today:* "Condition (GQ)".
- Groups in an edge table (tests on an end and its relationships), and in the tree's
  relationship filter (spec 005).
- Grouping rows by a property two hops away. Grouping stays one hop (`out:WorksAt.city`).
- Lookup columns through a group (spec 023 waits on its own Part 1).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-08 | GQ at 0.14 takes every shape the design builds | `omnigraph lint --schema s.pg --query q.gq` (CLI `0.14.1-edge+269299b`) on Person, Company, City with `WorksAt { since: Date?, role: String? }` and `LocatedIn`: two tests on one `$r0` in an `exists`; a bound edge `$n $w0:worksAt $r0` with `$w0.since > $d` and `$w0.role = "CTO"`; a second hop as a nested `exists` and as two traversals in one block; `count { … } >= 2`; `not { … }` with a test; `every` as `exists { $n worksAt $_ }` plus `not { $r0 … not { $r0.city = $c } }`; `sum($r0.employees) { … } > 100` | All eight: OK, read queries, no warnings |
| 2026-11-08 | The filter, menu, row-matching and sentence-filter paths are as described | A read of `gq_language::builder`, `type_table::{table, model}` and `filter_words` at `43176b80`, citing file and line | As listed under "What exists today" |
| 2026-11-08 | Every group the builder writes type-checks, and refuses what it cannot say | `gq_language` `every_kind_of_group_compiles` (two tests on one company, the edge's `@since` and `@role` with a date preset, three hops, a hop as a neighbour's property beside a relationship test and `@id`, none, every with and without tests, at least 2, at most 1, the total of an edge property, the average of whole numbers against `50.5`, the latest date, the smallest fraction; each as a page, a count and a stored query, all checked by the compiler), `a_group_refuses_what_it_cannot_say` (four hops, a count without its n, a total of text, an unknown edge property, a condition inside), `a_group_of_one_test_is_kept_as_the_neighbour_property_it_means` | Pass. Two tests on one company are one `exists` with `$r0.city = $f0_0` and `$r0.employees > $f0_1`; the copy as a stored query names them `$works_at_city` and `$works_at_employees`; a group of one plain test reads back as `out:WorksAt.city`, and "is empty" as `none` of `is not empty` |
| 2026-11-08 | Drafts through relationships apply as groups in the table | `type_table` `drafts_through_relationships_apply_as_groups`: the edge's `@role`, a second test drafted into that group, its total of `@hours` (the op menu offering comparisons only), two hops, and a neighbour's own property kept flat; the page read with all of them; the pill reads "worksAt · total worksAt hours greater than 10 where · worksAt role is CTO · city is Berlin"; the group's menu opens | Pass. `the_filter_menu_nests_each_relationships_neighbour_properties` and `a_schema_changed_elsewhere_waits_for_refresh` updated: every relationship is now a submenu, since it holds its edge's properties and further hops |
| 2026-11-08 | A sentence's groups become filters on one related record | `filter_words` `an_answers_groups_test_one_related_record`: the schema offers `groups` and the names inside them (`city`, `country`, `@role`, `@hours`) only with the schema; an answer with three tests and a hop in one group, a total, at least 2, and five mistakes | Pass: three filters; refused, by name, a filter in no group, a property the company lacks, a relationship Person lacks, and at most with no count. Without the schema a grouped filter is refused |
| 2026-11-08 | The sentence input with groups fits the cap on the personal graph's 100 types | An uncommitted instrument over `omnigraph schema show --target personal --graph personal` (212 KB), building each type's choices as the table does | Listing every type three hops away filled 48 KiB on most types and still left 99 without it: the graph is connected enough that three hops reach nearly every type. With a 12 KiB budget for the hops, the widest input is Artifact's 36,018 B (32,709 before), Person's 30,093 B, and 7 of 100 types list further hops; the other 93 say groups go one relationship from the row and a further hop goes in the predicate |
| pending | `count { } <= n` keeps rows with no match | Run on the dev server: people with at most one company, compared with `not { }` plus `count = 1` | — |
| pending | A three-hop group and `every` page in about the time of a one-hop filter on a large type | Time the first page on the personal graph's Person, as "has none" was measured | — |
| pending | The live sentence filter answers in groups | A sentence on the personal graph's Event ("events at a company in Berlin I worked at since 2020") with a key stored | — |

## Corrections

- 2026-11-08, building it: the group's editor is the pill's menu, not a popover of the bar's
  parts. A click on a group pill (or a right-click on a neighbour's property pill) opens a menu:
  "Which of them" (any, none, every; "At least" 1, 2, 3, 5, 10 and "At most" 0, 1, 2, 3, 5;
  "Total of", "Average of", "Smallest", "Largest" by property), then each test as a submenu
  (Edit, "Separate into its own filter", Remove), "Add a test" (the relationship's submenu rooted
  at its record), "The same Company as" listing other pills on the relationship that any of its
  records passes, and "Remove filter". Editing a test, adding one or setting an aggregate opens
  the bar's draft, its key menu rooted at the group's record, applied into the group. A count
  is picked from the list, not typed.
- A nested group is edited through its path: a hop of one test on any of them edits as
  `out:LocatedIn.name`; a nested group with several tests or another quantifier (which only the
  sentence filter makes) is removed and added again.
- No `studio_table::FilterWhere` action: the Filter menu's relationship submenus and the pill's
  menu reach everything, so no key was needed.
- The sentence input lists further hops only within a 12 KiB budget (Validation log); past it,
  groups go one relationship from the row.
