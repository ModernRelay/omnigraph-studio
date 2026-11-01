# 019: Model calls through OpenRouter

Status: built 2026-10-30; the three uses wait for a live check with a key

## Problem

Studio has places where a sentence of the person's is worth a structured answer from a model in
about a second: "dinners last month" as a table filter, a branch of 294 changes as a headline and
five lines for its reviewer, a column of a hundred rows filled from another column. The agent
panel (spec 011) cannot do this: an agent turn is the agent's whole loop, four to six seconds at
best, with no way to demand a shape for the answer (measured below), and it would start a
conversation for each. The headless CLIs (`claude -p --json-schema`, `codex exec
--output-schema`) enforce a shape but keep the four-second floor, because it is process start
and the agent's own prompt, not the model.

This plan adds one way for any feature to ask a model for a typed answer: a `model` crate that
calls OpenRouter with a JSON schema the feature declares, under one key the person stores once,
with the model per kind of task chosen in Settings. Three uses land with it, and the abstraction
is for every later one.

## Principles

- I: one client owns every request. The crate is a third kind of request, to OpenRouter, not to
  an Omnigraph server. This plan amends I's "in practice" line to name `model` beside
  `studio_update` and (spec 018) `studio_telemetry` (a patch).
- II: a call takes its input from the view's snapshot and answers into the view; it never reads
  the graph itself.
- III and IV: a model never writes. An answer is a proposal shown in the view's own controls
  (filter pills, a summary card, cells marked as suggested), and the person's accept is the
  write, on the branch the view has, as any edit of theirs.
- VI: every control explains itself. Each use says what leaves the machine before the first
  call, and the setting names the model and what each task sends.
- VII: loudness matches cost. A call shows as the control's own busy state, never a dialog;
  a failure is one line in place, not a toast.
- VIII: each use is an action with a default binding.
- IX: work is bounded. Each task caps its input, its output and its time; one call runs per
  control at a time, and a newer request cancels the older.
- XII: evidence over assumption. The latencies, costs and the answers' correctness below were
  measured on this machine on 2026-10-29 against the personal graph.
- The logging rule (`crates/studio/src/logging.rs`): a prompt holds schema names and row data,
  so no prompt, answer or key is logged; a failure logs the task name and the HTTP status only.

## What exists today (checked)

- Constitution I: `graph_client` makes every request to an Omnigraph server; `studio_update`
  alone fetches releases. `script/check-layers.py` lists the crates by layer: `graph_client`,
  `oauth_client`, `gq_language` and `studio_ui` at 2, `graph_store` and `studio_update` at 3,
  feature crates at 5, `studio` at 6. `NO_GPUI` names the crates that must not link GPUI.
- `type_table` filters are `gq_language::builder::Filter { property, op: FilterOp, value }`
  (`crates/gq_language/src/builder.rs`). `FilterOp` has 16 operators with stable keys
  (`eq`, `ne`, `lt`, `le`, `gt`, `ge`, `contains`, `in`, `is_empty`, `is_not_empty`, `today`,
  `this_week`, `last_30_days`, `has_any`, `has_none`, `related_to`; `FilterOp::from_key`).
  `build` refuses a filter on a property or edge the type lacks (`BuildError::UnknownProperty`,
  `UnknownEdge`). The table shows filters as pills with a remove button
  (`TypeTable::render_filter_bar`), adds one through a draft (`AddFilter`, `filter_draft`),
  and `ClearFilters` empties them.
- `review` holds a reviewed branch as `ReviewData`: the changes folded by entity
  (`EntityGroups`), `counts: Vec<TypeCounts>`, and a one-sentence `summary` from
  `summary_sentence` (spec 008: "17 entities changed"). A review holds up to 20,000 changes.
- Spec 014's node pages edit cells in place; a table edit goes to the session's branch and shows
  as a change card (spec 004).
- Spec 016 keeps its one secret, the OAuth refresh token, in the OS keychain through GPUI's
  `write_credentials(url, user, secret)` and `read_credentials(url)`; on macOS a
  `kSecClassInternetPassword` under the key string. Studio keeps no other secret of its own:
  Omnigraph tokens are the CLI's (spec 003).
- Settings are one typed document with `x-section` groups (Appearance, Browsing, Connection,
  Agent, Updates) and `x-control` hints (`crates/studio_settings/src/studio_settings.rs`).
- OpenRouter (`https://openrouter.ai/api/v1/chat/completions`, docs read 2026-10-29): the
  OpenAI chat shape; `response_format: {type: "json_schema", json_schema: {name, strict: true,
  schema}}` asks for an answer in the schema; `provider.require_parameters: true` routes only
  to endpoints that support every parameter sent, `provider.data_collection: "deny"` only to
  endpoints that keep no data, and `provider.zdr: true` only to zero-data-retention endpoints.
  The docs say strict enforcement varies by provider: some guarantee the shape, some treat it
  as a strong hint. A `usage` object with the cost comes back on each answer.
  `GET /api/v1/models` (no key) lists every model with `supported_parameters` and prices;
  112 models of the main vendors list `structured_outputs`.
- Measured 2026-10-29, eight filter requests on the personal graph's `Event` type (16
  properties, two enums), each graded against the expected filters; and one branch of 294
  changes condensed to 14 KB in the review's grouping (scratchpad `nlfilter/`):

  | Route | Per filter call | Right | Branch summary |
  |---|---|---|---|
  | OpenRouter, `anthropic/claude-haiku-5.5`, schema | 1.5–2.8 s | 8/8 | — |
  | OpenRouter, `google/gemini-3.1-flash-lite`, schema | 0.9–1.5 s | 8/8 | 2.5 s, thin |
  | OpenRouter, `openai/gpt-5-nano`, schema | 1.1–1.7 s | 5/8 | — |
  | OpenRouter, `anthropic/claude-sonnet-5.5`, schema | — | — | 9.5 s, best |
  | Anthropic API direct, Haiku, schema | 0.9–1.7 s | 8/8 | — |
  | `codex exec --output-schema`, any model | 4–6 s | 8/8 | 15 s |
  | `claude -p --json-schema`, Haiku | 5–10 s, one 20 s | 8/8 | 28 s, 7¢ |
  | Warm ACP turn, schema in the prompt only | 2.6–5.4 s | 0/5 | — |

  OpenRouter's routers (docs read 2026-10-29): `openrouter/auto` classifies the prompt into one
  of ~30 task types and picks the model the community spends most on for it; a `cost_tier`
  plugin setting bounds the price. `typesafe/jev-router` has Jev (below) pick a model and a
  reasoning effort per request. Both take `response_format`. Measured: `openrouter/auto` sent
  the eight filter requests to Azure-hosted OpenAI models, 3–5 s each, 8/8 right, about
  $0.0001 a call: right, but two to three times slower than naming Haiku.

  Jev (`typesafe/jev-1.14`, OpenRouter's "decision model", `POST /api/alpha/decisions`) is not
  a chat model: it takes a `state` object and typed questions, a Choice over named options, a
  Noul (yes or no) or a Score on a scale, and answers each with probabilities. $0.042 per
  million input tokens, output free; about $0.0001 a call. Measured on six sentences with two
  questions each, a Choice over the personal graph's 100 node types and a Noul "does this ask
  for a change": 0.4–3 s (one 6 s). The Noul was right every time with ≥ 0.98 for "delete the
  cancelled trips" and ≤ 0.04 for the reads. The 100-way Choice was right on 4 of 6 and unsure
  where it was right ("dinners in the last month": Event 0.37, Activity 0.32); "flights this
  week" went to Reservation over Event.

  BYOK (OpenRouter's "bring your own key"): a provider's key (Anthropic, OpenAI, Google …) is
  stored in the OpenRouter workspace, on its integrations page or through its management API
  with a management key, and every request through that provider then uses it, billed to that
  provider account plus 5% of OpenRouter's list price in credits. A key is prioritized or a
  fallback, can be kept from falling back to shared capacity, and the workspace's data policies
  still apply to it. Nothing in the request names a provider key: BYOK is not per request.

  Codex answering "ok" takes 4–5 s, the same as a filter, and a larger Codex model is no slower:
  the cost is process start. A custom `--system-prompt` cuts `claude -p` from 17,300 input
  tokens to 1,900 and 2.6¢ to 0.5¢ a call, but not its ~6 s. On a warm ACP session both agents
  answered the schema's `op` as `operator` and its lists as strings every time. OpenRouter adds
  about 0.7 s over the same model called at Anthropic directly.

## Design

### The `model` crate

A layer-2 crate, `crates/model`, in `NO_GPUI`: it depends on `http_client`, `serde`,
`serde_json` and `schemars`, and nothing of Studio's above layer 1. It is the only crate that
makes a request to OpenRouter.

```rust
/// A kind of work a feature asks a model for: its name is the settings key for its model.
pub struct Task {
    /// Stable, kebab-case: "table-filter", "branch-summary", "column-fill".
    pub name: &'static str,
    /// What the person is told leaves the machine for it ("the type's property names and
    /// your request"; "the changed records' titles and property names").
    pub sends: &'static str,
    pub tier: Tier,
    /// Caps: the input in bytes, the answer in tokens, and the call's time.
    pub limits: Limits,
}

/// Which model setting a task uses: fast answers of a line or two, or careful ones.
pub enum Tier { Fast, Careful }

pub struct Request<'a> {
    pub task: &'a Task,
    /// The task's instructions, fixed per task and never holding data.
    pub instructions: &'a str,
    /// The data: the request text, the schema excerpt, the condensed changes.
    pub input: String,
    /// The answer's shape; `strict` is always sent.
    pub schema: schemars::Schema,
}

pub struct Answer<T> {
    pub value: T,
    pub model: String,
    pub cost_usd: Option<f64>,
    pub elapsed: Duration,
}

pub enum Error { NoKey, Refused(u16, String), NotInSchema(String), TooLarge, Timeout, Network(String) }

impl Client {
    /// Dropping the future ends the call.
    pub async fn call<T: DeserializeOwned>(&self, model: &str, key: &str, request: Request<'_>) -> Result<Answer<T>, Error>;
    pub async fn list_models(&self) -> Result<Vec<ModelChoice>, Error>;
}
```

The crate holds no key and reads no setting: the caller gives the model and the key with each
call. `studio_workspace::models::Models`, one per app, is what views call: it reads the key
from the keychain, names the model for a task's tier from the settings, caches the model list, and runs each call off the main thread as a `Task` whose drop
ends the call. A schema is a `serde_json::Value` the feature builds (its enums come from the
graph's schema, not a Rust type).

- `call` builds the body: the instructions as the system message, the input as the user
  message, `response_format` with the task's schema under `strict: true`, `max_tokens` from the
  limits, `provider: {require_parameters: true, data_collection: "deny"}`, and the model the
  settings name for the task's tier. It sends `HTTP-Referer: https://omnigraph.dev/studio` and
  `X-Title: Omnigraph Studio`, which OpenRouter shows in its usage page.
- The answer is parsed as `T`. A body that is not JSON, or JSON that does not deserialize, is
  `NotInSchema` with the first 200 characters: the docs say enforcement varies by provider, so
  the crate trusts no answer it did not parse. A feature checks the parsed value against its
  own rules too (a filter against the type).
- An input over the task's byte cap is `TooLarge` before anything is sent: the feature condenses
  or says it cannot. A call over the time cap is `Timeout`. A `Cancel` dropped before the answer
  ends the request.
- Nothing is logged but the task name, the status code and the elapsed time. A `Refused` error
  keeps the server's message for the view, which shows it once.
- The key is read from the OS keychain under `https://openrouter.ai` (spec 016's mechanism)
  into memory at first use and on change; no file holds it. `NoKey` is the error until one is
  stored.

### Settings

A **Models** section:

| Key | Default | Control |
|---|---|---|
| `models.fast` | `anthropic/claude-haiku-5.5` | model picker |
| `models.careful` | `anthropic/claude-sonnet-5.5` | model picker |
| `models.key_stored` | `false` | **OpenRouter key**: whether the keychain holds one; a masked field, Store, Test, Remove |

The key row writes the key to the keychain and shows "Stored" or "None"; `key_stored` only
mirrors that, so the row reads right at start, and the key itself is never in the file.
**Test** makes the smallest call there is with the stored key and the fast model (the word
"ping", a one-field schema) and says "Connected: {model} answered in {s} s" or why not
(`Models::test_connection`). The row's description names what each task sends, and explains
BYOK in one line ("Your own Anthropic, OpenAI or Google key, set in
your OpenRouter workspace, is used for that provider's models and billed to that account")
and a **Provider keys…** button opens `https://openrouter.ai/settings/integrations`. Studio
holds no provider key: OpenRouter is the one place they live, and a request never carries one.
The model picker lists `GET /api/v1/models` filtered to `structured_outputs`, as "name · $in/$out
per million", read once a day and kept in the data directory; without a key or network it holds
the two defaults. The section's description: "Studio sends a task's input to OpenRouter and the
model it names. Each task says what it sends before its first call. Nothing is sent without a
key."

The defaults are the measured ones: Haiku was right on every filter at 1.5–2.8 s, Sonnet wrote
the best summary. Gemini Flash Lite is a cheaper fast choice the picker offers.

### What is sent is said once, in Settings

No call asks first. The key row's description names what each task sends, each control's
tooltip says the same for its task and the model it goes to, and storing a key is the consent:
nothing is sent without one. (A first draft asked once per task, in the control's place, and
kept the allowed tasks in a setting; it was dropped on 2026-10-30 as a step in the way of a
filter box, since the key is the person's own and the settings row already says what leaves.)

### First uses

**Table filter from a sentence** (`type_table`). The table's toolbar holds a search field,
"Filter in words…", always shown, before the control chips: a pill on the chips' height
(`ds::pill_field`) with the model's mark where the search field has its glass, its spinner while a call is out,
and its red line when one failed, with why in its tooltip (`studio_table::FilterInWords`,
⌘⇧F, puts the cursor in it). Enter calls `table-filter` (Fast; 32 KB in, since a wide type with many relationships describes itself in more than 8 KB; 300 tokens out, 10 s; each related type's properties are said once, and left out, with a line saying so, when they would pass the cap) with the type's property names and
value types, its enums' values, its relationship keys (`relationship_key`), what each operator
takes, today's date, and the sentence. The schema is built per
type: `property` an enum of the type's keys, `op` an enum of the operators, `value` a list of
strings. The answer's filters go through `builder::build`'s checks; any it refuses is dropped
and the bar says "Could not use: {property}". Relationships filter too: `has_any` and
`has_none` as they are; a neighbour's property (`out:Attended.name contains "Aaron"`, spec
004), which the prompt names as the way to filter by a related record's name or kind, so
"events Aaron attended" matches every Aaron as a nested search; and `related_to` for one
exact record, whose name goes through the node search the filter's own value field uses
(`suggest::search_nodes`, the first match taken), so the pill shows the record and the query
gets its id; a name no record matches is dropped and named. What the pills cannot say (`or`,
`not`, a further hop, two tests on one related record) the model puts in `predicate`, a GQ
condition on `$n` the answer's schema has beside `filters`: the input gives each
relationship's GQ traversal and a short grammar; the table makes it a condition filter (spec
004) only when the compiler accepts it, else the note names the diagnostic. The rest become pills as if added by hand, so
they are seen and removable, and the table reads as it does for any filter. A second sentence
replaces the filters the first made and keeps ones added by hand. Measured: 1.5–2.8 s on
Haiku with every answer right; Escape leaves the field and clears what it said.
The Fill button's tooltip and the Summarize button's name what goes out and to which model.

**Branch summary** (`review`). A **Summarize** button beside the review's sentence
(`studio_review::Summarize`, ⌘⇧M) calls `branch-summary` (Careful; 64 KB in,
1,200 tokens out, 60 s) with the branch name, the target, and the changes condensed as the
review groups them: per type, the count by operation, then each change as its operation, id,
title (the `name`, `title` or `subject` property, cut at 80 characters) and, for an update, the
names of the properties that changed, at most 25 per type with "… and N more". No values go:
a title is a value, so this is what the settings row and the tooltip name. The answer is `{headline, points[],
needs_attention[]}`, shown as a card under the sentence with the model's name and "Sent {n}
records' titles", kept with the review until the head moves. Measured: 9.5 s on Sonnet,
which counted every type, named the three edits to existing records and spotted duplicate
emails; 2.5 s on Flash Lite, thin. A review over the byte cap sends its first 25 changes per
type and says so.

**Fill a column** (`type_table`, after spec 014's editing). The bulk bar, which already holds
the selected rows and a property to set, gets **Fill with a model…** (`studio_table::FillColumn`, ⌘⇧L):
a row under it names the target, offers the source columns (every other scalar column, each
one a toggle) and takes one line ("the city from the address"). Each row is one `column-fill` call (Fast; 4 KB in,
200 tokens out, 10 s) with the target property's type and enum values, the source cells, and
the instruction; the answer is `{value: string | null, sure: bool}`. Rows run eight at a time
over the selected rows, at most 500 (IX), each sure answer put on its cell as a draft (spec 014), which the drafts bar saves as one
commit or discards; an unsure answer is counted and left empty rather than marked, since a
draft the person did not mean to keep is worse than a cell to fill by hand. A cell holding a
draft or an edit, one whose value moved while its call was out, and one the answer is the
same for or does not fit are skipped and counted. Closing the row stops the fill; the drafts
made stay.

### What a later use needs

A feature declares a `Task` (name, what it sends, tier, limits), builds its schema with
`schemars`, condenses its input under the cap, checks the parsed answer against its own rules,
and shows the answer in its own controls. It adds its action to `studio_actions` and its task
name to the settings description's list. It never calls OpenRouter itself.

### Limits

| | Fast | Careful |
|---|---|---|
| Input | ≤ 8 KB (`table-filter` 32 KB, `column-fill` 4 KB) | ≤ 64 KB |
| Output | ≤ 300 tokens (`column-fill` 200) | ≤ 1,200 tokens |
| Time | 10 s | 60 s |
| In flight per control | 1 | 1 |
| Rows per fill | 500, 8 at a time | — |

A call is never retried. The model list is read at most once a day.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| OpenRouter is the one provider | Anthropic and OpenAI directly; a provider per key; the agent's login | One key, one request shape and one model picker cover every vendor; routing options give the data policy per call. Direct Anthropic was 0.7 s faster for the same model, not enough for two code paths. The agents' logins cannot make API calls |
| A provider's own key goes through OpenRouter's BYOK, set in its workspace | Provider keys in Studio's settings, sent per request; Studio registering them through the management API | BYOK is not per request, so Studio holding a provider key would gain nothing; registering keys needs a management key and makes Studio a key manager. One link to the integrations page does it, and the provider's own data policies still apply |
| Named models by tier, not a router | `openrouter/auto`; `typesafe/jev-router` | Auto was right on every filter but 3–5 s against Haiku's 1.5–2.8 s, and neither router says what it will cost or which model will see the data before the call; a named model is what the settings row and the tooltips name |
| Jev for yes-or-no gates, later, not for the first uses | Jev as the filter or summary engine; Jev picking the table for a sentence | Jev answers typed questions with probabilities, not generated structures: it cannot write a filter or a summary. It was certain and right on read-versus-write, but unsure across 100 types. A gate such as "does this sentence ask for a change" before a filter, or routing a sentence to filter versus query, is its shape, under $0.0001 a call |
| A schema on every call, parsed in Studio | Prompt for JSON and parse; the agent panel | Without `response_format` both agents misnamed fields every time; with it, every answer parsed. The parse in Studio covers providers that only hint |
| No headless-CLI fallback | `claude -p --json-schema` or `codex exec --output-schema` without a key | Both are 4–6 s a call and `claude -p` costs 2.6¢ each; a feature that is sometimes five times slower is a worse feature than one that asks for a key |
| The key lives in the keychain | A `0600` file as the CLI's tokens; settings.json | Spec 016 already keeps its secret there with GPUI's credentials calls; a file would be the first secret file of Studio's own |
| No confirmation before a call; the settings row and each control's tooltip say what is sent | A confirmation once per task, kept in a setting (the first draft); a dialog per call | A confirmation in a filter box is a step in the way of every first use, and storing the key is already the person's choice; VI is met by saying what each task sends where the key is set and on each control |
| Models per tier, not per task | A model per task | Two choices are explainable; a task names its tier and the picker names the price |
| Answers are proposals in the view's own controls | Apply filters silently; write filled cells at once | III: the person's accept is the write; a filter pill can be removed, a suggested cell discarded |
| Titles but no values in a summary | Whole changes; ids only | Ids alone gave no summary worth reading; titles gave Sonnet enough to name the invoice amounts and the duplicates |

## Deferred

- A Jev gate in the filter field: a Noul "asks for a change" that turns "delete the cancelled
  trips" into a filter plus a note that the table does not delete, before the filter call.
- Streaming a summary as it arrives (OpenRouter supports `stream: true` with a schema).
- A model per graph, for graphs whose data may not leave a region: `provider.zdr` and `only`
  per graph.
- Suggesting a query from a sentence in the query editor (spec 007): the same crate, a schema
  of the catalog's stored queries and their parameters.
- Naming a conversation (spec 011) when the agent gives no title.
- No engine change is needed.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-31 | The sentence filter's input fits the cap on the personal graph's widest types | `type_table` `the_input_says_each_related_type_once_and_fits_the_cap` (a related type reached twice is described once; 400 relationships leave the related types' properties out, say so, and stay within the cap); an uncommitted instrument over the personal graph's schema (100 node types, 549 edge types) | Before: Person 50,383 B and Artifact 48,303 B, over the 32 KiB cap, so the call was refused on both. After: 25,452 B and 31,393 B, every type within the cap and none leaving related properties out. Each relationship repeated the relationship operators and the related type's properties; both are now said once |
| 2026-10-30 | The crate sends the schema under strict with the provider policy, parses the answer as the task's type, and tells its errors apart | `model` `a_call_sends_the_schema_and_parses_the_answer`, `errors_are_told_apart`, `the_model_list_keeps_schema_models_with_prices` (a fake HTTP client) | Pass |
| 2026-10-30 | A sentence's answer becomes filters the type takes, and a branch condenses to titles and changed property names without values | `type_table` `words::tests::an_answer_becomes_filters_the_type_takes`, `fill::tests::a_rows_input_names_the_target_and_its_sources`; `review` `summary::tests::the_input_holds_titles_and_changed_property_names_but_no_values` | Pass. The live calls were not run from the app yet: no key is stored in it |
| 2026-10-29 | Direct schema-bound calls answer filters in 1–3 s and agent routes in 4–6 s; ACP without a schema does not keep the shape | Eight requests on `Event`, graded; `claude -p`, `codex exec`, warm ACP sessions of both agents, OpenRouter with four models, Anthropic direct (scratchpad `nlfilter/`, scripts `run.sh`, `direct.py`, `acp_turns.py`, `grade.py`) | As in the table above. Haiku, Flash Lite and the CLIs 8/8; nano 5/8; ACP 0/5 with the wrong field names |
| 2026-10-29 | The Auto Router is right but slower than a named model; Jev is certain on read-versus-write and unsure on a 100-way type choice | `direct.py` with `openrouter/auto`; `jev.py` (six sentences, a Choice over the schema's node types and a Noul) | Auto 8/8 at 3–5 s via Azure; Jev 0.4–6 s, Noul right 6/6 (0.98 vs ≤ 0.04), Choice 4/6 |
| 2026-10-29 | A condensed 294-change branch summarizes well in ~10 s | `direct_summary.py` on Sonnet and Flash Lite; `claude -p` Sonnet; `codex exec` Astra | Sonnet 9.5 s and best; Astra 15 s; `claude -p` 28 s at 7.3¢; Flash Lite 2.5 s but generic |

## Corrections

None yet.
