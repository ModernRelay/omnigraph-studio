# 003: Connections

Status: in progress · 2026-10-13. Built: servers from the CLI config, the environment and
Studio's own list; token resolution; the contract check; bounded reads; and replacing a refused
token (validation log). OIDC waits (Deferred).

## Problem

Studio has to find OmniGraph servers, authenticate, confirm it speaks the server's contract,
discover which graphs and routes are there, and show the state of all of that at a glance,
without the `omnigraph studio` launcher (deferred).

## Principles

I (HTTP only; contract header), III (unknown outcomes), IV (Cedar decides), VIII (the Open
dialog by keyboard), X (capabilities from the served document).

## What exists today (checked)

- OmniGraph `v0.13.0`, `crates/omnigraph-api-types/src/lib.rs:30-32`: contract header
  `omnigraph-http-api`, value `0.13`. Every protected request must carry it; a mismatch is
  `400 api_contract_mismatch`; responses echo it.
- Public routes: `GET`/`HEAD /healthz`, `GET /readyz`, `GET /openapi.json`. The CLI checks the
  contract with `HEAD /healthz` before a data request.
- `GET /graphs` needs the `graph_list` action, which is denied without a cluster policy, even to
  a valid token. `GET /graphs/discovery` returns ids and names but accepts only identity
  credentials, not static tokens.
- The CLI's operator config is `$OMNIGRAPH_HOME/config.yaml`, else `~/.omnigraph/config.yaml`.
  Since OmniGraph main `2c9eb1e0` (#1090; the first edge build with it is
  `edge-20261020-b692610`) it is
  `target_config_version: 1` with `defaults.target` and `targets` of kind `direct`, `server` or
  `managed`. A `server` target has an `endpoint`, a `default_graph`, and one `credential` owner
  (its token stored by `omnigraph login --target`, bound to the endpoint's scope, or
  `OMNIGRAPH_TOKEN_<OWNER>`) or `auth: anonymous`; a `managed` target's data `endpoint` and
  `default_graph` serve graphs the same way. `omnigraph credential get --target NAME` exports a
  server target's token to a pipe. A saved token is `[<owner>] token = …` with
  `endpoint = …`; the CLI refuses one without an endpoint (`credential_scope_missing`) or for a
  request outside it (`credential_scope_mismatch`: another scheme, host or port, or a path not
  under the endpoint's), in `targets.rs` `resolve_server_token_for` and `graph_http.rs`
  `RequestTarget::is_under`. `OMNIGRAPH_TOKEN_<OWNER>` upper-cases the owner and turns dashes
  into underscores. `credentials` must not be group or world readable. The CLI refuses a file
  with `servers`, `profiles` or no version; `omnigraph target import` converts one, keeping it
  as `config.legacy.yaml`, and makes a target of every server and of every profile, so one
  server's targets can differ only in their default graph. It refuses a profile named like a
  server. An endpoint may be `unix:///…`.
- Routes on OmniGraph main but not in `v0.13.0`: Blob `PUT`/`DELETE`, commit publish and drop.

## Design

### Sources of servers

1. Every server the CLI's targets name. Tokens are resolved exactly as the CLI does; Studio
   never writes the CLI's files. It reads them again when
   `config.yaml` or `credentials` changes (their modification time and size, checked every
   2 s off the main thread): a server added there joins the list, one removed leaves it (a
   window open on it keeps its connection), one at a new address is a new connection, a
   target's graph is added, and a token changed there (`omnigraph login`) replaces the
   connection's, which a connection in use checks at once. A token typed into Studio for a
   CLI server lasts until the files change that server's token. Files that cannot be read
   keep every server as it was and are named in a toast that stays until dismissed, as a
   settings file's are. A token from `OMNIGRAPH_TOKEN_<OWNER>` is the one Studio started with,
   as for the CLI. The untargeted `OMNIGRAPH_BEARER_TOKEN` is sent to no server: a token goes
   only to the server it is keyed to (OmniGraph #1030).
   Studio reads `target_config_version: 1` only: every `server` target, and every `managed`
   cluster target with an `endpoint`, lands on a server, its `default_graph` a known graph of
   that server; a `direct` target opens storage, which Studio does not, and is not listed.
   Targets with one endpoint (as `graph_client::server_root` writes it) and one credential
   owner, or both anonymous, are one server, as an imported file makes them: it is labelled
   with the target named like the owner (an imported server's old name), else the first by
   name, and knows every target's default graph. A server target's token is `OMNIGRAPH_TOKEN_<OWNER>` for
   its credential owner, else `[<owner>] token = …` in `credentials` when that section's
   `endpoint` covers the target's (the same scheme, host and port, the scheme's default filled
   in, and a path that starts with every segment of the endpoint's), as the CLI checks before
   it sends one. A saved token with no endpoint, or for another, is not sent; the toast says so
   and names `omnigraph login --target <name>`. Studio reads the file itself rather than running
   `omnigraph credential get`: it needs no CLI on PATH and the token never passes through a
   process. `auth: anonymous` sends no token, nor does a managed target,
   whose data token the CLI gets from the control plane. A target Studio cannot open is named
   in the toast with the reason and not listed: a `unix:///…` endpoint (`graph_client` speaks
   HTTP(S) over TCP only), a server target without an endpoint, or without one credential owner
   or `auth: anonymous`. A file with another `target_config_version`, or none, is refused like
   a file that does not parse, the toast naming `omnigraph target import` for one with none;
   Studio reads no other form, and never `config.legacy.yaml`. An alias names its `target`; it
   is shown on the server that target lands on, and under the target's `default_graph` when it
   names no graph.
2. Servers added in Studio (URL, optional token). Tokens go to the OS keychain through GPUI's
   credentials API; the server list goes to Studio's settings.
   An address is made a server root before it is used (`graph_client::server_root`): trimmed,
   without a trailing slash, with `http://` added for a loopback host (`localhost`, `127.*`,
   `[::1]`) and `https://` for any other when no scheme is given; anything but an `http(s)`
   URL with a host, or one holding a user name or password, is refused with the reason. The
   same rule applies to addresses from the CLI config, the environment and a saved list, so
   an entry saved before it (`localhost:8080`) loads as `http://localhost:8080`, its token
   copied to the new address. `GraphClient::new` refuses an unusable root rather than
   building URLs on it.
3. Graph ids for a server come from, in order: `GET /graphs` when allowed; else
   `GET /graphs/discovery`, which names the graphs an identity credential (managed or OIDC)
   may know of, with no state; its targets' `default_graph`s; graph ids the person typed
   before. A 403 on both is not an error state; Studio says "This token may not list graphs.
   Enter a graph id." A discovery that fails otherwise says the list could not be read.

### Connecting

1. `HEAD /healthz` with a 5 s bound. Read the contract header.
2. Compare with the contracts `graph_client` speaks (only `0.13` today). Mismatch: state
   `contract mismatch`, with the server's version and "Studio speaks 0.13".
3. Capabilities: every route Studio uses is served by its minimum server (0.14, or the edge
   build before it), so connecting reads no capability set. A surface that comes to use a newer
   route reads `GET /openapi.json` and asks which operation ids exist, never the version string
   (constitution X). Connecting once read the whole document (387 KB at the edge build) before
   listing graphs, for a set nothing asked.
4. `GET /readyz` for serving state; `GET /graphs` when allowed, for per-graph state
   (`ready`, `loading`, `transitioning`, `blocked`, `stopping`) and `read_available` /
   `write_available`.
5. Re-check health every 30 s while a window is open and on wake from sleep; back off to 2 min
   after failures.

### The Open dialog

Servers on the left, the chosen server's graphs and a graph id field on the right, and a form
to add a server (⌥⌘O, `studio::OpenGraph`). The keyboard moves with the select actions Zed's
keymap sends from ↑, ↓, Tab and ⇧Tab, a single-line field passing them on: through the
servers, the token field when a server refused the token, the chosen server's graphs, then the
id field, wrapping around (constitution VIII). Moving to a server chooses it and shows its
graphs, connecting it if it never was; one that could not be reached is tried again with ↵
or Retry, not at every key that passes it. ↵ on a server goes to its first graph (to the id
field when a ready server lists none), ↵ on a graph opens it, and ↵ in a field uses what was
typed. While a server is added, Tab moves through that form's fields. The dialog lists the
registry's servers as they are now: a reload of the CLI's files that adds, drops or replaces
one (a new address is a new connection) is followed, so the replacement's state shows and the
dropped one's no longer reaches the dialog; a choice the registry no longer holds moves to one
it does.

### States (status-bar chip)

| State | Chip | What the person can do |
|---|---|---|
| connecting | grey, "Connecting…" | wait; cancel |
| ready | green, "server · 0.13" | everything the token allows |
| contract mismatch | amber | open in the CLI; nothing else |
| unauthorized (401) | red | update token |
| unreachable | red, last error | retry; requests in flight are marked unknown, never resent |
| graph unavailable (503) | amber, graph state from `/graphs` | wait; reads resume when ready |

### Requests (`graph_client`)

- Built on Zed's `http_client::HttpClient` (Apache-2.0): `reqwest_client` in the app, the fake
  client in tests.
- Every request sets `Authorization: Bearer`, `omnigraph-http-api: 0.13`, and a bounded
  timeout. Every response is checked for the contract header; a write response without it is an
  **unknown** outcome.
- 429 honours `Retry-After` for reads only. Writes are never retried (constitution III).
- Error bodies are parsed into the typed `ErrorOutput` (code, diagnostic, merge conflicts,
  precondition failure, resource limit, …) and surfaced as such.
- Tokens, query text and row data are never logged.
- A connection keeps at most 8 reads on the wire (`READ_CONCURRENCY`, shared by every clone of
  its client); the rest queue. A server admits a bounded number of reads across all clients
  (0.13 refuses past its read ingress cap, 64 by default, with "read ingress count cap 64
  exceeded"), and a view that counts every relationship of a node at once would otherwise be
  refused for some of them. A read waiting out a 429 does so without its slot.
- A read at a commit is answered once per session: the answer is kept (256 reads, 2,048 Blob
  cells, least recently used out first) and an identical read under way is shared, since a
  commit never changes. Failures are not kept, and reads at the branch head always go out.
- A session watches branch heads for the views pinned to them: its own branch always, and any
  branch a view holds a `HeadWatch` for (a review's). Every `HEAD_POLL` (30 s) while a Studio
  window is active, and when a window becomes active after a poll came while none was, it reads
  each watched head with a query that returns no rows (`builder::head_probe`, `limit 0` over
  the schema's first node type: the answer's `graph_commit_id` is the head), at most one under
  way per branch, and reads the branch list again. Studio's own commits set the head without
  asking and drop any head read still under way for that branch, whose answer could name the
  commit before. A head move emits `HeadMoved`; a move from a head already seen that is not
  Studio's own is another writer's commit and emits `Committed`, so commit lists and counts are
  read again as after Studio's own. A changed branch list emits `BranchesChanged`, so a branch
  made or deleted outside Studio appears; an unchanged one says nothing. No poll goes out while
  no window is active or while the graph is unavailable.
- Each branch's commit list is read once and shared by the views that show it (the history,
  the branches panel, every query tab's snapshot picker); a commit Studio makes on a branch
  drops that branch's list, reading the branch list again drops them all (its first read, as
  the window opens, keeps what was asked for meanwhile), and Refresh reads its branch again. A
  write used to set off one full commit list per branch in each of those views, one after
  another. Review and merge checks read their branches' heads themselves, since they decide on
  the live head.
- The footer's "N queries running" and "Writing…" count requests actually in flight. A read
  whose caller stops waiting (a search superseded by the next keystroke, a closed tab) is
  stopped and counted out. A write runs to its outcome whoever still waits for it, and the
  outcome is logged.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Read the CLI's config and credentials | Ask for every token again | The CLI is how people already reach their servers |
| Capabilities from `/openapi.json`, read when a route newer than the minimum server is used | Compare version strings; read the document on every connect | A server on main serves routes a release lacks; no route Studio uses needs the check today |
| Never retry writes | Retry with backoff | A lost response may have committed; only history can tell |

## Deferred

- Engine changes E1–E5, [spec 012](../012-omnigraph-engine-changes/plan.md): the `omnigraph studio` launcher, a supported local embedding
  API, the loopback guard in the launcher, owner discovery, and writer-safety rules for local
  sessions. Until then Studio connects to running servers.
- OIDC and signed data credentials: supported by the server, but Studio starts with static
  bearer tokens.
- A second contract (`0.14`) once OmniGraph ships it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-20 | The Open dialog is reached by keyboard and follows the registry | `studio` tests `the_keyboard_chooses_a_server_and_opens_its_graph` (two servers waiting for a token, so nothing is sent; the select actions dispatched as the keymap sends them) and `a_server_the_registry_replaces_is_followed` (the second server replaced by a new connection); the second against the earlier rule, which subscribed to the servers present when the dialog opened; Zed `v1.23.2` `assets/keymaps/default-macos.json` (up, down, tab and shift-tab to `menu::SelectNext`/`SelectPrevious` with no context) and `crates/editor/src/navigation.rs`, `editor.rs` (`move_up`, `move_down`, `tab` and `backtab` propagate in a single-line editor) | ↓ chose the second server, then its first graph; ↑↑ the first server, ↵ its graph, ↓ the id field with focus in it, ↓ the first server again; ↵ on a graph opened it. After the replacement the choice moved to the first server, the replaced connection's change drew nothing and the replacement's drew once; the earlier rule drew for the replaced one. The registry itself is not built in a test, so its observer is checked by reading only |
| 2026-10-17 | `script/dev-server` runs on any CLI that speaks contract 0.13 | `git show v0.13.0:crates/omnigraph-cli/src/main.rs` (`Command::Version`); `omnigraph version` from a 0.14.0 main build; the script's check against stub CLIs under bash 5 and macOS's bash 3.2; the script itself with 0.14.0 on port 8798 and a scratch `STUDIO_DEV_DATA` | 0.13.0 prints two lines and no contract; 0.14.0 adds `http-contract 0.13`. 0.13.x and 0.14.0 accepted; 0.12.0 and a CLI printing `http-contract 0.14` refused with the version and contract found. On 0.14.0 the cluster applied, both seeds loaded and `GET /graphs` listed `knowledge` and `notes` ready. Before, the script refused 0.14.0 by its version alone |
| 2026-10-12 | A server added as `localhost:8080` no longer crashes Studio | crash report: `GraphClient::url` panicked on `path_segments_mut().expect(..)` from the health probe, since `Url::parse("localhost:8080")` reads `localhost` as the scheme and cannot be a base; `graph_client` tests `a_server_address_gets_a_scheme_and_a_bare_word_is_refused`, `a_client_refuses_a_root_that_cannot_take_a_path` | The address becomes `http://localhost:8080`; a root without an http(s) host, or with credentials, is refused with its reason |
| 2026-10-11 | Contract header and value | `git show v0.13.0:crates/omnigraph-api-types/src/lib.rs` | `omnigraph-http-api`, `0.13` |
| 2026-10-11 | Routes in 0.13.0 vs main | parsed `openapi.json` at `v0.13.0` and main | Blob PUT/DELETE, commit publish/drop only on main |
| 2026-10-11 | CLI token resolution | `git show v0.13.0:crates/omnigraph-cli/src/operator.rs` | env `OMNIGRAPH_TOKEN_<NAME>`, then credentials file section; refuses over-permissive file |
| 2026-10-12 | 429 honours `Retry-After` for reads only | `graph_client` tests `a_rate_limited_read_waits_and_goes_again_a_bounded_number_of_times`, `a_rate_limited_write_is_never_sent_again` (counted requests) | A read waits as told (≤ 30 s, 1 s when unnamed) and goes again at most twice; a write is sent once |
| 2026-10-12 | A 503 graph is unavailable until it answers | `graph_store` test `a_graph_that_answers_503_is_unavailable_until_it_answers_again` (simulated clock) | Marked with the server's reason; asked again after 5 s, doubling to 60 s; any answer clears it. The document footer shows an amber dot by the graph with the reason |
| 2026-10-12 | Abandoned reads leave the running count; abandoned writes still finish | `graph_store` test `dropped_reads_are_counted_out_and_dropped_writes_still_finish` (three reads that never answer, dropped; a write whose task is dropped before it ran); the same test against the earlier session code | Count back to 0 and the write sent once with its refusal logged; the earlier code left the count at 3, as the app showed "100 queries running" after four node searches over crm's 25 types |
| 2026-10-12 | A connection keeps a bounded number of reads on the wire | `graph_client` test `a_connection_keeps_a_bounded_number_of_reads_on_the_wire` (40 reads through two clones against a server that holds each 5 ms) | At most 8 at once, all answered. Seen before the fix in the app on a person with dozens of relationship types: "read ingress count cap 64 exceeded" with Retry on several rows |
| 2026-10-12 | A refused token can be replaced in place | offscreen snapshot with a wrong `OMNIGRAPH_STUDIO_TOKEN`, Open Graph | "The server refused the token" with a masked field and Update token; the connection reconnects with it. A Studio server keeps it in the keychain; a CLI server for this run only. Retry when unreachable, Cancel while connecting |
| 2026-10-13 | The CLI's files are read again when they change | `graph_store` tests `reads_servers_with_their_tokens_and_stamps_the_files` (a rewritten file changes the stamp; open permissions refuse the token once; a config that does not parse keeps the servers) and `cli_servers_follow_the_cli_files` (a changed token reaches the same connection's next request, a typed one stays, a new address is a new connection, servers come and go); offscreen snapshots with a scratch `OMNIGRAPH_HOME`: a server added to `config.yaml` 6 s after launch; a token written to `credentials` while the Open dialog said the server refused the token; a `config.yaml` broken while running | `scratch-beta` listed under CLI config; `scratch-alpha` turned ready and listed `knowledge` and `notes`; a toast "OmniGraph CLI config: could not parse …: servers: invalid type … at line 1 column 10" with `scratch-alpha` still listed. Before, the files were read once at launch and their errors were kept but never shown |
| 2026-10-13 | The session watches heads and the branch list | `graph_store` test `the_head_watch_sees_other_writers_commits_while_a_window_is_active` (simulated clock): the head once the schema loads; no request at a poll with no window active; one at activation, none at the next; a watched branch asked with the session's own; a dropped watch forgotten; a branch added elsewhere listed at the poll; Studio's own commit the head with no request | Each as stated; an unchanged branch list emitted nothing |
| 2026-10-13 | The head probe answers with the head and no rows on 0.13 | `POST /graphs/knowledge/query` with `studio_head` on the dev server, three times | `row_count` 0, `graph_commit_id` the head; 2-17 ms |
| 2026-10-13 | A branch made outside Studio appears | offscreen snapshot on the dev server: the Branches panel, then `POST /branches` `head-watch-check` from curl 25 s in, then 40 s more | The panel listed `main` and `head-watch-check`; before, the list was read only when the window opened and after Studio's own branch writes |
| 2026-10-13 | Opening a window reads each branch's commit list once | `graph_store` test `commit_lists_are_read_again_only_for_the_branch_a_commit_lands_on` (main asked for while the branch list loads, the branches panel asking again on its arrival: one read; it read twice before the fix); offscreen snapshots through a request-counting proxy on the dev graph's four branches: an empty window; three new queries and the history | `GET /commits` 4 times in both, once per branch (before: 5 for the empty window, main twice, and one more per query tab) |
| 2026-10-20 | The CLI's target files are read as the CLI reads them | `git show origin/main` (`b692610c`) of `crates/omnigraph-cli/src/operator.rs`, `targets.rs` and `graph_http.rs`; `graph_store` test `reads_targets_with_their_endpoint_bound_tokens` | A server target and a managed cluster endpoint are listed with their `default_graph`, an anonymous one with no token, a direct target not at all; the owner's token is sent when the saved `endpoint` covers the target's (a trailing slash too), not when it is missing, at another port or scheme, or deeper than the target's, and the reason never holds the token; a Unix socket and a target without an owner are named; `target_config_version: 2` keeps the servers read before; an alias's `target` is read |
| 2026-10-20 | Studio reports a target it cannot open | Offscreen snapshot with `OMNIGRAPH_HOME` holding a version-1 file (a server target on the dev server, a `unix:///` one, a direct one) and a bound `credentials` | The toast reads "OmniGraph CLI config: CLI target 'scratch-sock' is served on a Unix socket (unix:///tmp/scratch.sock); Studio reaches servers over HTTP(S)". The Open dialog did not open in the offscreen run with no server chosen (`studio::OpenGraph`, its button or ⌥⌘O), so the listed target is shown by the test above, not by a render |
| 2026-10-20 | Studio reads target files only, one server per endpoint and credential | `graph_store` tests `targets_on_one_endpoint_are_one_server` (a file as `omnigraph target import` writes it: `default`, the server's own target and a profile's on one endpoint and owner, an anonymous one beside them) and `reads_servers_with_their_tokens_and_stamps_the_files` | The three keyed targets are one server, named like the owner, with both default graphs and the token; the anonymous target is a server of its own with none; an alias on the profile's target lands on that server under the target's graph, one on an unknown target keeps its name. A `servers` file keeps the servers read before, the reason naming `omnigraph target import` |
| 2026-10-20 | A real operator config converts | `omnigraph` 0.14.0-edge+b692610 `target import` on a copy (five servers, one profile named like its server, 32 aliases, dummy tokens), then on the live home | It refused the profile named like a server ("names more than one binding"); without the profile it wrote five targets and every token with its server's `endpoint`. The config was then written by hand with the comments kept and the profile as that target's `default_graph`; `target list`, a read through `--target` and an alias answered with the saved tokens |

## Corrections

- 2026-10-12 · A read or write counted itself out only when it answered, inside the task its
  caller held. The palette's node search drops one read per node type at each keystroke, so
  the footer climbed to "100 queries running" on crm and stayed there; a write dropped the
  same way would have been cut off with its outcome unlogged. Reads now stop and are counted
  out when dropped, and writes run detached to their outcome.

- 2026-10-12 · The client followed redirects: reqwest's default follows up to ten, and nothing
  overrode it. Every request now sets `RedirectPolicy::NoFollow`, so a Blob's external
  reference (302 with its URI) is reported and never fetched.
