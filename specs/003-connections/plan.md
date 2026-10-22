# 003: Connections

Status: in progress · 2026-10-13. Built: servers from the CLI config, the environment and
Studio's own list; token resolution; the contract check; bounded reads; and replacing a refused
token (validation log). OIDC waits (Deferred).

## Problem

Studio has to find Omnigraph servers, authenticate, confirm it speaks the server's contract,
discover which graphs and routes are there, and show the state of all of that at a glance,
without the `omnigraph studio` launcher (deferred).

## Principles

I (HTTP only; contract header), III (unknown outcomes), IV (Cedar decides), VIII (the Open
dialog by keyboard), X (capabilities from the served document).

## What exists today (checked)

- Omnigraph `v0.13.0`, `crates/omnigraph-api-types/src/lib.rs:30-32`: contract header
  `omnigraph-http-api`, value `0.13`. Every protected request must carry it; a mismatch is
  `400 api_contract_mismatch`; responses echo it.
- Public routes: `GET`/`HEAD /healthz`, `GET /readyz`, `GET /openapi.json`. The CLI checks the
  contract with `HEAD /healthz` before a data request.
- `GET /graphs` needs the `graph_list` action, which is denied without a cluster policy, even to
  a valid token. `GET /graphs/discovery` returns ids and names but accepts only identity
  credentials, not static tokens.
- The CLI's operator config is `$OMNIGRAPH_HOME/config.yaml`, else `~/.omnigraph/config.yaml`.
  Since Omnigraph main `2c9eb1e0` (#1090; the first edge build with it is
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
- Routes on Omnigraph main but not in `v0.13.0`: Blob `PUT`/`DELETE`, commit publish and drop.

## Design

### Sources of servers

1. Every server the CLI's targets name. Tokens are resolved exactly as the CLI does; Studio
   never writes the CLI's files. It reads them again when
   `config.yaml` or `credentials` changes (their modification time, size and permissions,
   checked every 2 s off the main thread, so a `chmod 600` on `credentials` is seen at the next
   check): a server added there joins the list, one removed leaves it (a window open on it keeps
   its connection), one at a new address is a new connection, a target renamed on the same
   endpoint is the same connection, renamed in place, which the windows open on it keep and a
   later `omnigraph login` reaches (servers are matched by address, since 2026-10-21), a
   target's graph is added, and a token changed there (`omnigraph login`) replaces the
   connection's, which a connection in use checks at once. Every session on the connection
   then forgets every answer its read memo kept with the old one, which may have seen more, and
   the old token's type counts, stored-query catalog and branch list; a shown one reads what
   the new token may do and those three again at once, a parked one when it is shown again,
   since a parked graph asks the server nothing (since 2026-10-21), and its views hear of its
   branch list only then, since History reads on it; neither shows any of the old token's
   meanwhile (spec 002, "What the credential may do"; `docs/architecture.md` says what is kept
   and why). A token typed into
   Studio for a CLI server lasts until the files change that server's token. Files that cannot be read
   keep every server as it was and are named in a toast that stays until dismissed, as a
   settings file's are. A token from `OMNIGRAPH_TOKEN_<OWNER>` is the one Studio started with,
   as for the CLI. The untargeted `OMNIGRAPH_BEARER_TOKEN` is sent to no server: a token goes
   only to the server it is keyed to (Omnigraph #1030).
   Studio reads `target_config_version: 1` only: every `server` target, and every `managed`
   cluster target with an `endpoint`, lands on a server, its `default_graph` a known graph of
   that server; a `direct` target opens storage, which Studio does not, and is not listed.
   An address is one server (decided 2026-10-21): every graph's key is its server's address
   and its id, so a second connection to one address would open its graphs twice, and window
   restore, links and toasts would each find either. Of the targets on one endpoint (as
   `graph_client::server_root` writes it), Studio connects as the CLI's `defaults.target` when
   it is one of them, else as the first in the file. The targets with its credential (one
   owner, or both anonymous) are that server too, as an imported file makes them: it is
   labelled with the target named like the owner (an imported server's old name), else with
   the one it connects as, and knows each of their default graphs. A target on the endpoint
   with another credential is named in the toast with why (`CLI target 'open' is not listed:
   Studio keeps one connection per address, and reaches … as 'default' (the CLI's default
   target), whose credential differs`), and its aliases land on no server. A CLI target at the
   address of a server added in Studio is named the same way and not connected; Studio's own
   servers load before the CLI's, so the one added first keeps the address. A server target's token is `OMNIGRAPH_TOKEN_<OWNER>` for
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
   credentials API; the server list goes to `servers.json` in the data directory. One that
   cannot be read or parsed is set aside as `servers.json.unreadable-<seconds>` and named in a
   toast ("Servers at start: Studio's saved servers could not be read (…)"), and one that
   cannot be set aside is never saved over (since 2026-10-21). A token typed while the
   keychain is still being read at start stands when the keychain answers after it.
   An address is made a server root before it is used (`graph_client::server_root`): trimmed,
   without a trailing slash, with `http://` added for a loopback host (`localhost`, `127.*`,
   `[::1]`) and `https://` for any other when no scheme is given; anything but an `http(s)`
   URL with a host, or one holding a user name or password, is refused with the reason. The
   same rule applies to addresses from the CLI config and the environment. Studio's saved list
   holds each address as `add_server` checked it, scheme included, and is read as written:
   nothing converts an entry saved without a scheme (`localhost:8080`), which the toast of the
   servers' problems at start names with its reason (Corrections, 2026-10-18). Those toasts
   read "Servers at start: …", a prefix true of a server not added and of a saved list not read
   alike (since 2026-10-21; before, "A server could not be added: …" headed both).
   `GraphClient::new` refuses an unusable root rather than building URLs on it.
3. Graph ids for a server come from, in order: `GET /graphs` when allowed; else
   `GET /graphs/discovery`, which names the graphs an identity credential (managed or OIDC)
   may know of, with no state; its targets' `default_graph`s; graph ids the person typed
   before. A 403 on both is not an error state; Studio says "This token may not list graphs.
   Enter a graph id." A discovery that fails otherwise says the list could not be read.

### Connecting

1. `HEAD /healthz` with a 5 s bound. Read the contract header.
2. Compare with the contracts `graph_client` speaks (only `0.14` today, `CONTRACT`). Mismatch:
   state `contract mismatch`, which reads "Server speaks 0.13; Studio speaks 0.14" for a 0.13
   server.
3. Capabilities: every route Studio uses is served by its minimum server (0.14, or the edge
   build before it), so connecting reads no capability set. A surface that comes to use a newer
   route reads `GET /openapi.json` and asks which operation ids exist, never the version string
   (constitution X). Connecting once read the whole document (387 KB at the edge build) before
   listing graphs, for a set nothing asked.
4. `GET /readyz` for serving state; `GET /graphs` when allowed, for per-graph state
   (`ready`, `loading`, `transitioning`, `blocked`, `stopping`) and `read_available` /
   `write_available`.
5. Re-check health every 30 s and on wake from sleep while something shows the server: a
   window showing one of its graphs (a parked graph does not count), or the Open dialog listing
   it (`Connection::watch`); back off to 2 min after failures. A check that comes while nothing
   shows it is skipped and the checks stop; the next watch checks at once. A graph list that
   failed is read again when something next watches the server and on a graph's Refresh
   (`Connection::list_graphs_again`, since 2026-10-21).

### The Open dialog

Servers on the left, the chosen server's graphs and a graph id field on the right, and a form
to add a server (⌥⌘O, `studio::OpenGraph`). The keyboard moves with the select actions Zed's
keymap sends from ↑, ↓, Tab and ⇧Tab, a single-line field passing them on: through the
servers, the token field when a server refused the token, the chosen server's graphs, then the
id field, wrapping around (constitution VIII). Moving to a server chooses it and shows its
graphs, connecting it if it never was; one that could not be reached is tried again with ↵
or Retry, not at every key that passes it. ↵ on a server goes to its first graph (to the id
field when a ready server lists none), ↵ on a graph opens it, and ↵ in a field uses what was
typed. A new token typed for a server that refused its token is that server's alone: choosing
another, by a click, by the keyboard or by the registry dropping the one chosen, clears the field
and its error, so Update token never sends it to another address nor saves it under another
server's keychain entry (since 2026-10-21; before, one field served every server and kept what
was typed). While a server is added, Tab moves through that form's fields. The dialog lists the
registry's servers as they are now: a reload of the CLI's files that adds, drops or replaces
one (a new address is a new connection) is followed, so the replacement's state shows and the
dropped one's no longer reaches the dialog; a choice the registry no longer holds moves to one
it does.

### States (status-bar chip)

| State | Chip | What the person can do |
|---|---|---|
| connecting | grey, "Connecting…" | wait; cancel |
| ready | green, "contract 0.14" | everything the token allows |
| contract mismatch | amber | open in the CLI; nothing else |
| unauthorized (401) | red, "Token refused"; "Asks for a token" when the request carried none | update token |
| unreachable | red, last error | retry; requests in flight are marked unknown, never resent |
| graph unavailable (503) | amber, graph state from `/graphs` | wait; reads resume when ready |

A 401 says what it answered. A request that carried no token, to a server Studio has none
configured for (a managed CLI target, whose data token the CLI gets from the control plane, or
a CLI target whose saved token is bound to another endpoint), reads "This server asks for a
token; none is configured for it."; one that carried a token, "The server refused the token."
Update token keeps a Studio server's token in the keychain; a CLI server's lasts the run, and
its tooltip names `omnigraph credentials set --target <name> --stdin`, which keeps one for the
CLI and Studio (since 2026-10-21; before, a server sent no token was said to have refused it,
and the tooltip named `omnigraph login`, which is managed sign-in only now).

### Requests (`graph_client`)

- Built on Zed's `http_client::HttpClient` (Apache-2.0): `reqwest_client` in the app, the fake
  client in tests.
- Every request sets `Authorization: Bearer`, `omnigraph-http-api: 0.14`, and a bounded
  timeout. Every response is checked for the contract header; a write response without it is an
  **unknown** outcome. A read's error without it came from something between Studio and the
  server (a proxy, a gateway) and is no refusal: the read fails as one that did not reach the
  server (`ReadError::Transport`), so a gateway's 404 or 502 neither says the server lacks what
  was asked nor marks or clears a graph's availability. An answer naming another contract is a
  contract mismatch (since 2026-10-21; before, a read took any error status for the server's
  refusal).
- 429 honours `Retry-After` for reads only. Writes are never retried (constitution III).
- Error bodies are parsed into the typed `ErrorOutput` (code, diagnostic, merge conflicts,
  precondition failure, resource limit, …) and surfaced as such.
- Tokens, query text and row data are never logged.
- A connection keeps at most 8 reads on the wire (`READ_CONCURRENCY`, shared by every clone of
  its client); the rest queue. A server admits a bounded number of reads across all clients
  (0.13 refuses past its read ingress cap, 64 by default, with "read ingress count cap 64
  exceeded"), and a view that counts every relationship of a node at once would otherwise be
  refused for some of them. A read waiting out a 429 does so without its slot.
- A read at a commit is answered once per session: the answer is kept (up to 4,096 answered
  reads and 2,048 Blob cells, least recently used out first) and an identical read under way is
  shared, since a commit never changes. A read under way is let go only for the count, once
  no answer is left to, never for the answers' bytes (spec 015). Failures are not kept, and
  reads at the branch head always go out. A new token forgets every answer kept, and a read
  still out when it changed keeps nothing: the new token may see less, and its refusals are shown (constitution IV). The type counts, the
  stored-query catalog and the branch list are read again under it; the schema, the heads and
  the change logs stay (the schema is answered alike to every credential that may read the
  graph; heads and logs describe commits, and carry forward only answers read since).
- A session watches branch heads for the views pinned to them: its own branch always, and any
  branch a view holds a `HeadWatch` for (a review's). Every `HEAD_POLL` (30 s) while a Studio
  window is active, and when a window becomes active after a poll came while none was, it reads
  each watched head with a query that returns no rows (`builder::head_probe`, `limit 0` over
  the schema's first node type: the answer's `graph_commit_id` is the head), at most one under
  way per branch, and reads the branch list again. Studio's own commits set the head without
  asking and drop any head read still under way for that branch, whose answer could name the
  commit before. A head move emits `HeadMoved`; a move from a head already seen that is not
  Studio's own is another writer's commit and emits `Committed`, so head commits and counts are
  read again as after Studio's own. A changed branch list emits `BranchesChanged`, so a branch
  made or deleted outside Studio appears; an unchanged one says nothing. Branch lists are
  taken in the order they were asked for: one answering after a newer one was taken is ignored
  (since 2026-10-21). No poll goes out while no window is active, while the graph is
  unavailable, or while its window parked it to show another graph; shown again after a poll
  came meanwhile, it asks at once.
- Each branch's newest commit is read once (a page of one) and shared by the views that show
  it (the history's tags, the branches panel); a commit Studio makes on a branch drops that
  branch's, reading the branch list again drops those of the branches that came or went (its
  first read, as the window opens, keeps what was asked for meanwhile), and Refresh reads its
  branch again. A write used to set off one full commit list per branch in each of those views,
  one after another. Since 2026-10-20 nothing reads a branch's whole commit list: views read
  pages (specs 008, 009). Review and merge checks read their branches' heads themselves, since
  they decide on the live head.
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
- A second contract (`0.14`) once Omnigraph ships it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-21 | A 401 to a request with no token says the server asks for one | `graph_store` `connection::a_401_says_whether_a_token_was_sent` (a server answering 401 to every read; connected with no token, then a token, then none); `studio` `open_graph::a_server_sent_no_token_is_said_to_ask_for_one`; `omnigraph` main `7fff9dfd` `crates/omnigraph-cli/src/cli.rs` (`credentials` subcommand) and `helpers.rs` (`omnigraph credentials set --target {name} --stdin` in the CLI's own hint) | No token: "Asks for a token" on the chip, "This server asks for a token; none is configured for it." in the dialog; a token: "Token refused", "The server refused the token."; each new token's connect says so anew. A CLI server's Update token tooltip names `omnigraph credentials set --target cloud --stdin` and no `login`. Before, every 401 read as a refused token, a managed target's included |
| 2026-10-21 | A parked graph's views hear of a new token's branch list only when shown | `graph_store` `a_new_token_reads_access_and_every_answer_again`, extended: `BranchesChanged` and `CredentialChanged` recorded from the token change while parked to after it is shown | Parked: neither event. Shown: `BranchesChanged` first, then `CredentialChanged`. Before, `read_again_for_credential` emitted `BranchesChanged` to a parked session, and a History tab of it whose last read failed read its first page (`history` `branches_changed`) |
| 2026-10-21 | A read takes an error for the server's only with the contract header | `graph_client` at `abe09ad`: `an_error_is_the_servers_refusal_only_with_its_contract_header` (all 20 reads: a 404 and a 502 without the header, a 403 with it, a 404 naming 0.13), `a_blob_cell_is_null_only_by_the_servers_answer`, `a_refusal_on_a_graph_is_counted_for_that_graph_alone` (a 403 naming 0.13); `check_contract` and `refusal` in `client.rs` | Without the header every read fails as `Transport` naming the status; with it, `Refused`; naming 0.13, `ContractMismatch` |
| 2026-10-21 | The contract the plan names is the one Studio speaks | `graph_client::CONTRACT` (`0.14`) and `ConnectionState::label` (`contract 0.14`, "Server speaks …; Studio speaks 0.14") | The request header, the comparison and the ready chip said 0.13; corrected. The facts about Omnigraph `v0.13.0` above and the older rows stay as checked then |
| 2026-10-21 | The saved list, a renamed target, a `chmod`, a failed graph list and the branch lists' order | `graph_store` at `43ad4f3`: `an_unreadable_server_list_is_set_aside_and_said`, `a_renamed_cli_target_keeps_its_connection`, `reads_servers_with_their_tokens_and_stamps_the_files` (a permissions change alone changes the stamp), `a_graph_list_that_failed_is_read_again_when_next_shown`, `an_older_branch_list_landing_last_does_not_replace_a_newer_one`; `studio_workspace` `a_start_notice_says_only_what_happened` | Written here from that commit and its tests, read against `registry.rs`, `cli_config.rs` and `connection.rs`. The saved list's notice reached the toast as "A server could not be added: Studio's saved servers could not be read …"; it reads "Servers at start: …" now, as every start problem does |
| 2026-10-21 | A token typed for one refused server never reaches another | `studio` `a_token_typed_for_one_server_is_cleared_when_another_is_chosen` (two servers that refused their tokens: a token typed for the first, then the second clicked; one typed for the second, then ↑↑; one typed for the first, then the registry's list without it; one typed and its own server clicked again) | Each change of server left the field empty, its error gone and the keyboard off the field; Update token then said "Enter the new token." and sent nothing; the same server clicked again kept what was typed. Before, `select`, `move_to` and `follow` changed `selected` and left the one shared field as it was, so Update token sent the first server's token to the second's address and saved it under the second's keychain entry |
| 2026-10-21 | The read memo keeps up to 4,096 answered reads | `graph_store/src/session.rs` `PINNED_READS` (4,096, with `PINNED_READ_BYTES`) and `PINNED_BLOBS` (2,048), read at `a073bf1` | The plan said 256 reads; corrected to the constant |
| 2026-10-21 | No conversion of saved addresses remains | `graph_store/src/registry.rs` `ConnectionRegistry::load` (a saved server is added at its saved address; `add_connection` refuses an unusable root into the start errors); `6f69def` removed the conversion | The plan still said an entry saved without a scheme loaded with `http://` and its token copied; that text is removed (Corrections) |
| 2026-10-21 | A parked graph reads nothing for a new token until it is shown | `graph_store` `a_new_token_reads_access_and_every_answer_again`, extended: the session parked (`set_shown(false)`), the token replaced, then shown again | Parked: no request at all (no `/access`, `/snapshot`, `/queries` or `/branches`), Person's count gone and the branch list and catalog loading. Shown: each of the three read once and `/access` once, the new token's answer allowing change, Person 7 again. Before, `sync_access` → `read_again_for_credential` read all four for every session on the connection, parked or shown |
| 2026-10-21 | A new token reads the counts, the stored-query catalog and the branch list again | `graph_store` `a_new_token_reads_access_and_every_answer_again`, extended: the fake answers `/snapshot` (Person: 7), `/queries` and `/branches`, and lists what is asked; `GraphSession::read_again_for_credential` from `sync_access`; at `7fd9cc9` `server_branch_list`, `server_schema_get` (a read with no branch) and `server_list_queries` (a read on main) in `omnigraph-server/src/handlers.rs` | Before, `forget_reads` cleared the read memo only: the old token's counts, catalog and branch list stayed shown. Now, right after the token changes, `count_of(Person)` is none and the branch list and catalog are loading; then each of `/snapshot`, `/queries` and `/branches` is asked once under the new token, and Person shows 7 again. The schema, the heads and the change logs stay, as `docs/architecture.md` says |
| 2026-10-21 | A new token reads what it may do at once and forgets what the old one read | `graph_store` test `a_new_token_reads_access_and_every_answer_again` (a server answering `/access` by the bearer token; the session opened with a read-only token whose access read is held); `graph_client` `a_replaced_token_reaches_every_clone` (only a token that differs counts as a credential change); `access` `a_forgotten_answer_is_read_again`. The same session test against the earlier code, by reading: `Connection::set_token` moved the client's count and nothing subscribed to it, and `Access::landed` compared only its own generation | The write-capable token's access read went at once, with no write, and allowed changing main; the read-only answer released after it landed nowhere; a read at commit c1 kept under the old token was asked again under the new; the narrower token's own answer refused change. A 403 on a write read access again (one read) and kept the answers. Before: the table kept "Your credential may not change main" until a branch move or a write, the held answer landed, and the memo answered without asking |
| 2026-10-21 | Only a 403 on a graph says its policy may have changed | `graph_client` test `a_refusal_on_a_graph_is_counted_for_that_graph_alone` (a 403 from `GET /graphs`, `/graphs/discovery`, and one without the contract header; then a read, a write and a forwarded request on a graph, one named `discovery` among them) | The server's own routes counted nothing, the graph's three; neither moved the credential count. Before, the connect probe's `/graphs` refusal, which a static token always gets, forgot every access answer at each connect |
| 2026-10-21 | An address is one connection | `graph_store` tests `targets_on_one_endpoint_are_one_server` (a file as `omnigraph target import` writes it, with `defaults.target: default`, an anonymous target on the same endpoint, and two targets on another endpoint, the anonymous one first in the file), `cli_servers_follow_the_cli_files` (a server added in Studio at an address two CLI targets then name, one of them moved there) | Three servers: the imported one (named like its owner, both default graphs, its token), the other endpoint's as its first target with its default graph, and the plain one; the anonymous target beside `default` and the keyed one after the first were named in the toast with the target Studio connects as and why, their aliases on no server. The Studio server stayed the address's one connection, both CLI targets named, the moved one's old connection gone. Before, the anonymous target was a second connection to the same address |
| 2026-10-21 | A token typed while the keychain is read stands | `graph_store` test `a_connect_waits_for_its_token_and_a_cancel_stays` (the keychain's answer given after a token was set) | Nothing sent with the keychain's token; before, `token_read` replaced the typed token unconditionally |
| 2026-10-21 | Health is checked only while something shows the server, and a parked graph polls nothing | `graph_store` tests `health_is_checked_only_while_the_server_is_watched` (simulated clock), `the_head_watch_sees_other_writers_commits_while_a_window_is_active` (parked, two polls and an activation, then shown); `studio_workspace` `a_graph_reopened_behind_opens_its_tabs_when_first_shown` (a graph added behind and one shown, then switched) | No check while nothing watched, through three intervals; a watch checked at once and then at each interval; dropped, none. Parked, no head probe or branch list at the polls or on activation; shown, one probe at once that saw the newer head. Behind and switched-from graphs were not shown, the shown one was. Before, every server connected once was checked every 30 s for the rest of the run, and every parked graph polled its heads and branches |
| 2026-10-21 | Every route the client calls is in the contract | `graph_client` test `every_route_the_client_calls_is_in_the_contract` (29 calls, every public read and write, conditional writes both ways, against `openapi/omnigraph-0.14.0-edge.json`'s paths, a `{…}` matching one segment); `api` `object_types_match_the_contract`, `enum_types_match_the_contract` with `BranchMergeRequest.dry_run`, `InvokeStoredQueryRequest`, `QueriesCatalogOutput`, `QueryCatalogEntry`, `ParamDescriptor`, `ParamKind`, `DiffOutput` and the client's `CommitChanges` (`CommitChangesOutput`) added | All pass. `docs/architecture.md` had claimed a route test that did not exist |
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
| 2026-10-13 | The CLI's files are read again when they change | `graph_store` tests `reads_servers_with_their_tokens_and_stamps_the_files` (a rewritten file changes the stamp; open permissions refuse the token once; a config that does not parse keeps the servers) and `cli_servers_follow_the_cli_files` (a changed token reaches the same connection's next request, a typed one stays, a new address is a new connection, servers come and go); offscreen snapshots with a scratch `OMNIGRAPH_HOME`: a server added to `config.yaml` 6 s after launch; a token written to `credentials` while the Open dialog said the server refused the token; a `config.yaml` broken while running | `scratch-beta` listed under CLI config; `scratch-alpha` turned ready and listed `knowledge` and `notes`; a toast "Omnigraph CLI config: could not parse …: servers: invalid type … at line 1 column 10" with `scratch-alpha` still listed. Before, the files were read once at launch and their errors were kept but never shown |
| 2026-10-13 | The session watches heads and the branch list | `graph_store` test `the_head_watch_sees_other_writers_commits_while_a_window_is_active` (simulated clock): the head once the schema loads; no request at a poll with no window active; one at activation, none at the next; a watched branch asked with the session's own; a dropped watch forgotten; a branch added elsewhere listed at the poll; Studio's own commit the head with no request | Each as stated; an unchanged branch list emitted nothing |
| 2026-10-13 | The head probe answers with the head and no rows on 0.13 | `POST /graphs/knowledge/query` with `studio_head` on the dev server, three times | `row_count` 0, `graph_commit_id` the head; 2-17 ms |
| 2026-10-13 | A branch made outside Studio appears | offscreen snapshot on the dev server: the Branches panel, then `POST /branches` `head-watch-check` from curl 25 s in, then 40 s more | The panel listed `main` and `head-watch-check`; before, the list was read only when the window opened and after Studio's own branch writes |
| 2026-10-13 | Opening a window reads each branch's commit list once | `graph_store` test `commit_lists_are_read_again_only_for_the_branch_a_commit_lands_on` (main asked for while the branch list loads, the branches panel asking again on its arrival: one read; it read twice before the fix); offscreen snapshots through a request-counting proxy on the dev graph's four branches: an empty window; three new queries and the history | `GET /commits` 4 times in both, once per branch (before: 5 for the empty window, main twice, and one more per query tab) |
| 2026-10-20 | The CLI's target files are read as the CLI reads them | `git show origin/main` (`b692610c`) of `crates/omnigraph-cli/src/operator.rs`, `targets.rs` and `graph_http.rs`; `graph_store` test `reads_targets_with_their_endpoint_bound_tokens` | A server target and a managed cluster endpoint are listed with their `default_graph`, an anonymous one with no token, a direct target not at all; the owner's token is sent when the saved `endpoint` covers the target's (a trailing slash too), not when it is missing, at another port or scheme, or deeper than the target's, and the reason never holds the token; a Unix socket and a target without an owner are named; `target_config_version: 2` keeps the servers read before; an alias's `target` is read |
| 2026-10-20 | Studio reports a target it cannot open | Offscreen snapshot with `OMNIGRAPH_HOME` holding a version-1 file (a server target on the dev server, a `unix:///` one, a direct one) and a bound `credentials` | The toast reads "Omnigraph CLI config: CLI target 'scratch-sock' is served on a Unix socket (unix:///tmp/scratch.sock); Studio reaches servers over HTTP(S)". The Open dialog did not open in the offscreen run with no server chosen (`studio::OpenGraph`, its button or ⌥⌘O), so the listed target is shown by the test above, not by a render |
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

- 2026-10-18 · The conversion of server addresses saved without a scheme (`localhost:8080`
  loaded as `http://localhost:8080`, its keychain token copied to the new address) was removed
  (`6f69def`): every release saves an address as `add_server` checked it. This plan described
  it until 2026-10-21. Graph data filed under a server's name is not converted either (spec
  002, 2026-10-21).
