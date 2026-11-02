# 003: Connections

Status: in progress · 2026-10-25. Built: servers from the targets the Omnigraph CLI lists and
from the environment; a server added as a CLI target, through the CLI; tokens from the CLI's
`credentials export` (on Unix; on Windows only anonymous targets); a target's own certificate
authority; the contract check;
bounded reads; and replacing a refused token (validation log). A managed cluster is reached on
the CLI's login, an interim since 2026-10-25; OIDC servers, and managed clusters without the
CLI, wait for Studio's own sign-in (Deferred; spec 016).

## Problem

Studio has to find Omnigraph servers, authenticate, confirm it speaks the server's contract,
discover which graphs and routes are there, and show the state of all of that at a glance.
`omnigraph studio` (Omnigraph 0.14.1) hands a target to Studio by link: see "Links from the
CLI" below.

## Links from the CLI

`omnigraph studio --target NAME [--graph ID]` opens
`omnigraph-studio://open?target=NAME&binding=sha256:…&home=sha256:…[&graph=ID]`
(`protocols.studio` 1): the target by name, a digest of what the name resolves to (`binding`)
and one of the operator file it was read from (`home`). The link carries no address, token or
credential. Studio (`studio::deep_link`, `open_cli_link`):

- refuses a link whose target it does not list (a link never adds a server);
- runs `omnigraph target inspect NAME --json` (`cli_tokens::target_digests`, one turn at the
  CLI, off the main thread) and refuses the link, naming the digest that differs, when either is
  not what this Mac's CLI reports: a link made on another Mac, or before the target was changed,
  opens nothing. A target of another kind, or a CLI older than 0.14.1, is refused in words;
- on a match opens the target's server on the graph the link names, else the target's default
  graph, else the first graph known on the server, as a `graph` link does (one that names no
  branch opens main). A start's window waits for the check as for any link.

`omnigraph-studio://connect?code=…`, reserved for the launcher before it existed, stays refused.

## Principles

I (HTTP only; contract header), III (unknown outcomes), IV (Cedar decides), VIII (the Open
dialog by keyboard), X (capabilities from the served document).

## What exists today (checked)

- Omnigraph 0.14, from the release `v0.14.0` on (Studio's minimum server; its document is
  `crates/graph_client/openapi/omnigraph-0.14.0.json`): contract header `omnigraph-http-api`,
  value `0.14` (`graph_client::CONTRACT`). Every protected request must carry it; a mismatch is
  `400 api_contract_mismatch`; responses echo it. Every error states a `disposition` (`action`,
  `effects`), which alone decides a write's outcome (constitution III): effects `none` is a
  refusal, anything else or none unknown. The edge builds from `edge-20261020-7fd9cc9` to the
  release send `0.14` too but are not supported: their errors state no disposition, so a write
  they refuse reads as unknown, and their access reports name the actor `actor`, which Studio
  reads as absent. Earlier builds and `v0.13.0` speak `0.13`, which Studio refuses.
- Public routes: `GET`/`HEAD /healthz`, `GET /readyz`, `GET /openapi.json`. The CLI checks the
  contract with `HEAD /healthz` before a data request.
- `GET /graphs` needs the `graph_list` action, which is denied without a cluster policy, even to
  a valid token. `GET /graphs/discovery` returns ids and names but accepts only identity
  credentials, not static tokens.
- The CLI's operator home is `$OMNIGRAPH_HOME`, else `~/.omnigraph`, its file `config.yaml`
  there. Since Omnigraph main `f7db38da` (#1115) and `ff542202` (#1126) the file is the version-1
  operator configuration (`version: 1` with `profiles`, `targets`, `aliases` and `defaults`); a
  file with `target_config_version` is refused, and the keyed `credentials` file and
  `OMNIGRAPH_TOKEN_<OWNER>` are gone. A server target owns one `auth` provider: `token` from the
  CLI's `store`, an `env` variable or a `file`, an `exec` helper, or `anonymous`. Checked on
  2026-10-21 against `0.14.0-edge+7fff9df` (main `7fff9dfd`), on a scratch operator home holding
  each kind of target and no secret, and on the person's own home:
  - `version --json` reports `protocols: {target: 1, credential: 1, access: 1}`. `50283ce` (before
    #1115) reports target and credential 1 only, and `888223b` (#1115 without #1126) all three, so
    the numbers alone do not tell the formats apart (#1127 asks for a bump). The `v0.14.0`
    release reports `{target: 2, credential: 2, access: 2}` (#1133, #1135), the only numbers
    Studio drives since 2026-10-24.
  - `target list --json` is an object from target name to its entry, sorted by name. A `server`
    entry has `kind`, `endpoint` (written with a trailing slash), `auth` (`{provider: anonymous}`;
    `{provider: token, source: {store: {}}}`, `{env: NAME}` or `{file: PATH}` with the path made
    absolute; or `{provider: exec, command, timeout}`), and, when set, `transport` (`tls.ca:
    {mode, file}` and `proxy: {mode: direct}`, `{mode: environment}` or `{mode: url, url}`) and
    `default_graph`. A `managed` entry has `profile`, `cluster_id`, `cluster_incarnation` and,
    when set, `endpoint` and `default_graph`; a `direct` entry has `scope` (`graph` or `root`),
    `uri` and, for a root, `default_graph`. It names neither `defaults.target` nor any alias,
    and no command lists the aliases. A home without the file answers `{}`. A file the CLI
    refuses answers `{"error": "target_config_invalid: …", "code": "target_config_invalid"}` on
    standard output with exit 2; for the old format the message names
    `docs/user/cli/targets.md#upgrade-from-the-old-format` and says there is no importer. An
    endpoint that is neither HTTPS nor HTTP to an anonymous loopback server is refused the same
    way (a `unix:///…` one included).
  - `target inspect NAME --json` is `{name, target}`, `target` the same entry; with `--resolved` a
    managed target's `profile` (`api`, `organization_id`, `auth`) is added, and `identity:
    {status: unknown, source: offline inspection}`.
  - `credentials status --target NAME --json` reads no secret: `{owner: {kind, owner_id,
    profile_id, route, organization_id}, profile, availability, identity, server_validity,
    observed_at, verified_at}`, its `availability` `present` or `absent` for an `env` source,
    `present` or `unavailable` for a `file`, `unknown` for the `store` and `not executed` for an
    `exec` helper. An anonymous target answers `{owner: NAME, profile, availability: anonymous,
    identity}`; a direct or managed one is refused (`target_incompatible`, exit 2). It notes the
    target it chose on standard error.
  - `credentials export --target NAME` writes the token and a newline (65 bytes for the person's
    `personal` target, counted with `wc -c`), refuses a terminal or a regular file before it reads
    a credential (exit 1), and says a failure on standard error as `Error:` with numbered causes
    in colour, a managed target not signed in as `{"type":"login_required",…}`; an unknown target
    is refused with exit 2. It exports a managed target's cluster data token without starting a
    login: the one it has cached while that still validates, else a new one it asks the issuer
    for with a 3600 s lifetime, which the issuer may shorten (one made with `omnigraph cluster
    token --ttl` can last up to 86400 s; `managed/data.rs`). Asked again after a 401 that refused
    it, while the cache still validates, it gives the same token; a target whose endpoint pin no longer matches
    fails the export. `credentials status --target` refuses a managed target ("managed
    credentials set/status/remove require explicit --profile"). On a system other than Unix it refuses every export: at main `242ab02f` its
    check of standard output (`commands::targets::stdout_is_pipe` in `commands/targets/mod.rs`)
    asks whether `/dev/stdout` is a FIFO on Unix and is `false` elsewhere, with no named pipe or
    other route; its `exec` helpers are refused there too (`credential_sources::exec`,
    `HelperUnsupported`). `version --json`, `target list`, `target inspect` and `credentials
    status` are not gated by platform.
- Routes served from 0.14 on but not in `v0.13.0`: Blob `PUT`/`DELETE`, commit publish and drop.

## Design

### Sources of servers

1. Every server target the Omnigraph CLI lists (since 2026-10-21). Studio parses no operator
   configuration and no credential source, and writes neither (Omnigraph RFC 1017, "Static
   tokens for clients"): it asks the CLI, whose own code is the one reader of its formats.
   - **The CLI.** Studio looks first at the CLI its settings name (`omnigraph_cli`, Settings →
     Connection → Omnigraph CLI, a leading `~` read as the home folder), then at each `omnigraph`
     on `PATH`, then in `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`
     (`graph_store::cli::candidates`). It runs the first that fits (`cli::probe`): its `version
     --json` reports Studio's HTTP contract (`0.14`) and target, credential and access protocol
     2, which Omnigraph `v0.14.0` is the first release to report; an edge build before it
     reports 1 and is passed over. That one command is the whole probe: every CLI that reports
     2 has the commands Studio runs. Each program passed over is named with why. What a program
     was found to be is kept, in memory and in `cli_probes.json` in the data directory, with the
     requirements it was judged against, until its file changes (its modification time, size or
     permissions) or those requirements do, so a start runs no probe an earlier launch ran;
     one that could not be run or did not answer in time is asked again at the next look (since
     2026-10-22; before, a slow probe passed a CLI over for the run, and no probe was kept past
     the run). A changed setting moves where Studio looks, which the next
     check of the CLI's files sees: it lists the targets with that CLI and asks every token
     again. The agent's CLI is chosen from the same places with the same probe (spec 013).
     Studio needs a CLI of `v0.14.0` or later and a version-1 operator configuration. On Windows
     Studio finds the CLI and lists its targets as on Unix, and uses the anonymous ones: the CLI
     gives a token only through a Unix pipe, so each server target that takes one is refused as
     it is listed (`cli_config::Platform`; since 2026-10-22, before which a Windows Studio listed
     no targets at all and had no server but the environment's).
   - **Running it.** The CLI runs off the main thread, one sequence of Studio's commands at a time
     (`cli::take_turn`): a listing, an Add server, one target's `credentials status`,
     `credentials export` and `target inspect` together, or the agent's choice of its CLI. The
     CLI takes its operator home's lock without waiting, so two commands at once refuse each
     other, and a command refused for a lock another program holds is run again, four times at
     most, 250 ms apart. Sequences take the turn in the order they asked, but one a window waits
     on goes first: the token of a graph a window shows (`Connection::wait_for_token`), and the
     agent's choice. A sequence waits for its turn 10 s at most for a listing or an Add server,
     as long as its export may take for a token, and 120 s for the agent's choice; one held back
     past that by another of Studio's (a helper that takes long) is not run, and says so; the
     next listing or connect asks again (decided 2026-10-22). Each command is bounded too: 10 s
     for a read, 30 s for an export, an `exec` helper's timeout and 15 s more for its export. It
     runs with its standard input empty (but for Add server's entry and token), `NO_COLOR`, and
     none of the Omnigraph variables in Studio's environment (every name that starts with
     `OMNIGRAPH_`, in any ASCII case, on every system: `cli::omnigraph_variables`) but
     `OMNIGRAPH_HOME` and the one its target's `env` source names, so it reads the operator home
     Studio's environment names, as it would in a terminal.
   - **Targets.** `target list --json` names them, one command once the CLI is known. A start
     lists them off the main thread and waits for nothing: the windows it reopens open at once,
     and they and a link that comes meanwhile take their graphs once the listing lands
     (`ConnectionRegistry::when_listed`; since 2026-10-22, before which a start waited on the
     main thread for the listing, up to 5 s). A start connects only the servers it shows, a
     link's or a reopened window's; the Open dialog connects the others when it lists them. A
     listing that waited past its bound for its turn lists nothing and leaves the operator file
     watched unstamped, so the next check lists the targets. They are listed again when the
     operator file, a token file a target names, or a program where Studio looks for the CLI
     changes (their modification time, size and permissions, checked every 2 s off the main
     thread; the file itself is never read). A `server` target is a server, its `default_graph`
     a known graph of it. Each `direct` target is refused with why: Studio never opens storage or
     starts a server. A `managed` target with a data endpoint lands on that endpoint, its
     credential the cluster data token the CLI holds for its profile (`CliCredential::Managed`;
     for a profile that signs in, from the person's `omnigraph login`). This is an interim until
     Studio signs in itself (spec 016), and it departs from RFC 1017's A2, which has Studio not
     borrow that token; the person chose it on 2026-10-25, and A2's amendment is proposed
     (Omnigraph #1152). Its token is asked for with `credentials export` alone, kept in memory
     only, and asked for again after a 401 that refused it: a lapsed one is replaced, and one
     refused for another reason comes back the same and stays refused. `target inspect` must still name that
     managed target at that endpoint. The Open dialog names the CLI profile the token comes
     from, and choosing the server or its Retry asks the CLI again after it gave none (since
     2026-10-25: before, nothing asked again until Refresh, so a person who signed in after the
     CLI said "login required" saw it stay). A managed target without an endpoint is refused with why.
     The listing keeps every managed target as `target list` describes it (its profile, its
     cluster id and incarnation, and its endpoint and default graph; `CliTargets::managed`)
     for spec 016. Where the CLI exports no token (Windows), each server target that
     takes one, from any source or an `exec` helper, is refused the same way ("CLI target 'x'
     takes a token, and the Omnigraph CLI gives a token only through a Unix pipe, so Studio on
     Windows cannot use this target yet"), after any refusal that holds on every system; its
     token is never asked for and its token file is not watched, and an anonymous target on its
     address is then that address's server. A target with `transport.tls.ca` trusts its own
     certificate authority, as the CLI does (`operator_transport.rs`): the listing reads the
     bundle, a regular file of at most 1 MiB holding PEM certificates, at least one, and
     watches it like a token file (`cli_config::CliRoots`, since 2026-10-25). The target gets an
     HTTP client of its own, made from the bundle the listing read, which trusts it with the
     system's roots (`append`) or alone (`replace`); a bundle that cannot be used refuses the
     target with why, and with an HTTPS proxy it is refused, as the CLI refuses that pair. One
     with `proxy: {mode: url}` gets an HTTP client of its own through that proxy (Zed's client,
     which also leaves out the hosts in `NO_PROXY`); `direct` and `environment` use Studio's
     client, which follows the system's proxy settings. A changed proxy or bundle makes a new
     connection. A `unix:///…` endpoint is
     refused, since Studio's client speaks TCP (the CLI refuses one in the file anyway). A
     configuration the CLI refuses lists nothing and keeps every server as it was; the toast,
     which stays until dismissed, gives the CLI's own words, which for a file in the old format
     say how to convert it (Omnigraph's `docs/user/cli/targets.md`, "Upgrade from the old
     format"; there is no importer). The CLI lists no aliases, so Studio offers none.
   - **One connection per address, one identity per target** (decided 2026-10-21). An address is
     one server: an open graph is found by its server's address and its id, so a second
     connection to one address would open its graphs twice, and links and toasts would each find
     either. Each server target owns its credential, so two targets on one endpoint (as
     `graph_client::server_root` writes it) are two identities. Studio connects as the first of
     them by name, since `target list --json` does not say which is `defaults.target`, and names
     the others in the toast as alternatives it does not connect as (`CLI target 'b' is at … too,
     which Studio reaches as 'a', the first by name. Each target is an identity of its own …`);
     the server's name, token and default graph are that target's alone. A CLI target at the
     address of the environment's server is named the same way and not connected. A server is its
     address: a target renamed on the same endpoint is the same connection, renamed in place,
     which the windows open on it keep and which asks the CLI for its token by the new name (a
     server's revision holds its target's name, since 2026-10-22; before, a rename kept asking
     by the old one); one at a new address is a new connection (with the graphs remembered for
     its name); and one no longer listed leaves the list (a window open on it keeps its
     connection). The graph ids opened on a target are remembered by its name in
     `known_graphs.json` in the data directory, so the Open dialog offers them when a token may
     not list the server's graphs; the file names no server.
   - **Tokens.** A server target's token comes from `credentials export --target NAME`, read
     from a pipe: an `env` variable, a `file`, the CLI's secure `store` and an `exec` helper are
     each resolved by Omnigraph's code. It is asked for when the server connects, before
     anything is sent, and a graph's first reads wait until it is in (`Connection::token_ready`,
     since 2026-10-22); once more after a 401 that refused a token from it (one that refused none,
     the token never arriving, asks nothing); on the person's Refresh, not when a graph is first
     shown, whose first reads use the connect's token (since 2026-10-22; before, opening a graph
     asked twice and dropped a token typed in); when the target's entry or token file changes;
     and when a CLI is installed, upgraded or removed. After an ask after a refusal, a 401 to
     any request of the graph (a read, a write, the agent's through its gateway) asks nothing
     until the server takes a request of a graph with the token held or the credential changes otherwise
     (a renewal's or a Refresh's token, a new transport); after a probe's ask, a probe's 401 asks
     nothing until a connect is ready, the token is asked for anew (Refresh, a change to the CLI)
     or a new source is taken (since 2026-10-30; before, a read's 401 asked again when the
     probe's ask gave the refused token back). The secure store
     cannot be watched, so nothing polls it. Each ask runs `credentials status` first: a
     credential it finds missing (`absent`, `unavailable`) is said (`No credential is configured
     for 'personal': its token file … is not there`) and nothing is exported or sent. Then the
     export, then `target inspect`: the token is kept only while the CLI still names the
     endpoint it was asked for, so it reaches no other server. A token is held in memory only,
     in the connection's client, and sent only under its target's endpoint (redirects are not
     followed); it is never logged, shown, written, or passed to an agent, whose CLI holds the
     gateway's own token (spec 013). A refused export is said with the CLI's reason, without its
     colours or its note of the target, and never with what it printed. An ask the CLI could not
     answer now (Studio's other commands held it past the export's bound, a command ran past
     its timeout, the program could not be started) leaves the token the CLI gave before in use,
     unsaid, and a connect goes on with it; the ask after a 401 keeps none, since the server
     refused that token. With none held, the server reads "No token from the CLI" with why, and
     the next health check asks again, which a refused or missing credential is not (since
     2026-10-28; before, any failure dropped a working token, and the server stayed refused
     until the person acted). An anonymous target asks
     the CLI nothing and sends no token. A token that changed reaches the same connection, which a
     connection in use checks at once. Every session on the connection then forgets every answer
     its read memo kept with the old one, which may have seen more, and the old token's type
     counts, stored-query catalog and branch list; a shown one reads what the new token may do
     and those three again at once (the catalog once a view has wanted it, and the schema too
     when none is held: a 401 may have refused it), a parked one when it is shown again, since a
     parked graph asks the server nothing (since 2026-10-21), and its views hear of its branch
     list only then, since History reads on it; neither shows any of the old token's meanwhile
     (spec 002, "What the credential may do"; `docs/architecture.md` says what is kept and
     why). A token typed into Studio for a CLI server stands until Refresh, a change to its
     target, or a CLI installed, upgraded or removed, asks the CLI again.
2. A server is added as a CLI target, through the CLI (since 2026-10-22): Studio keeps no
   server list and no token of its own (RFC 1017; its "Studio keeps its own server list and
   token copies" alternative is rejected). Add server runs `omnigraph target add NAME` with the
   target's entry on its standard input (`{"kind": "server", "endpoint": …, "auth": …}`), its
   `auth` `{provider: token, source: {store: {}}}` when a token is typed and `{provider:
   anonymous}` otherwise, then `omnigraph credentials set --target NAME --stdin --replace` with
   the token on its standard input, never on a command line. The CLI keeps the token in its own
   secure store and writes its operator file; Studio writes neither, and lists the targets again
   at once. A refusal shows in the CLI's own words ("The server was not added: target already
   exists (…)"); a token the CLI would not keep removes the target again (`target remove`). A
   name may be left out: the host is the target's name, made into one a target can have
   (`[A-Za-z][A-Za-z0-9_.-]*`, `server-` before one that starts otherwise). An address Studio
   connects to already is that server, and nothing is added. The CLI rewrites the operator file
   when it adds a target, so the file's comments do not survive an Add server. Where the CLI
   exports no token (Windows), Add server adds only servers that need none: its token field is
   disabled, with its reason in its tooltip ("Studio on Windows adds only servers that need no
   token: the Omnigraph CLI gives a token only through a Unix pipe."), and Tab passes it by
   (since 2026-10-22). A token that reaches the registry anyway is refused before the CLI is
   asked ("… so Studio on Windows cannot use a server that needs a token yet; leave the token
   empty to add one that needs none"): Studio could not use the target it would add.
   For development and tests, `OMNIGRAPH_STUDIO_SERVER` (with `OMNIGRAPH_STUDIO_TOKEN`, which
   lasts the run, and `OMNIGRAPH_STUDIO_GRAPH`) still adds a server named `environment`: the
   development server (`script/dev-server`) is plain HTTP with a bearer token, which a CLI
   target cannot be (the CLI takes HTTP only for an anonymous loopback server); a TLS front with
   a certificate authority of its own can be one now. A problem with it is said
   in a toast ("The server OMNIGRAPH_STUDIO_SERVER names was not added: …").
   An address is made a server root before it is used (`graph_client::server_root`): trimmed,
   without a trailing slash, with `http://` added for a loopback host (`localhost`, `127.*`,
   `[::1]`) and `https://` for any other when no scheme is given; anything but an `http(s)`
   URL with a host, or one holding a user name or password, is refused with the reason. The
   same rule applies to addresses from the CLI's targets and the environment.
   `GraphClient::new` refuses an unusable root rather than building URLs on it.
3. Graph ids for a server come from, in order: `GET /graphs` when allowed; else
   `GET /graphs/discovery`, which names the graphs an identity credential (managed or OIDC)
   may know of, with no state; its target's `default_graph`; graph ids the person typed
   before. A 403 on both is not an error state; Studio says "This token may not list graphs.
   Enter a graph id." A discovery that fails otherwise says the list could not be read.

### Connecting

1. `HEAD /healthz` with a 5 s bound. Read the contract header.
2. Compare with the contracts `graph_client` speaks (only `0.14` today, `CONTRACT`). Mismatch:
   state `contract mismatch`, which reads "Server speaks 0.13; Studio speaks 0.14" for a 0.13
   server.
3. Capabilities: every route Studio uses is served by its minimum server (`v0.14.0`), so
   connecting reads no capability set. A surface that comes to use a newer
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
   (`Connection::list_graphs_again`, since 2026-10-21). Refresh also asks the CLI for a CLI
   server's token again (`GraphSession::refresh`, which calls `Connection::refresh_token`).

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
and its error, so Update token never sends it to another address (since 2026-10-21; before, one
field served every server and kept what was typed). While a server is added, Tab moves through
that form's fields, past its token field where that is disabled (Windows). The dialog lists the
registry's servers as they are now: a new listing of the CLI's targets that adds, drops or
replaces one (a new address is a new connection) is followed, so the replacement's state shows
and the dropped one's no longer reaches the dialog; a choice the registry no longer holds moves
to one it does.

### States (status-bar chip)

| State | Chip | What the person can do |
|---|---|---|
| connecting | grey, "Connecting…" | wait; cancel |
| ready | green, "contract 0.14" | everything the token allows |
| contract mismatch | amber | open in the CLI; nothing else |
| unauthorized (401) | red, "Token refused"; "Asks for a token" when the server found none; "No token from the CLI" when the CLI gave none, and nothing was sent | update token |
| unreachable | red, last error | retry; requests in flight are marked unknown, never resent |
| graph unavailable (503) | amber, graph state from `/graphs` | wait; reads resume when ready |

A 401 says what it answered, by the server's challenge (`WWW-Authenticate`, sent with every 401
since Omnigraph #1122; `graph_client::Challenge`, which keeps `resource_metadata` for spec 016):
`error="invalid_token"` reads "The server refused the token."; a bare `Bearer`, "This server asks
for a token; none is configured for it." From a server before #1122, which sends no challenge, a
request that carried a token is taken as refused. A CLI server the CLI gave no token is sent
nothing, and reads "The Omnigraph CLI gave no token for this server:" with the CLI's reason
(since 2026-10-21; before, the token of a target in the old format whose saved token was bound to
another endpoint was not sent, and its server answered 401).
Update token's token lasts until Studio quits or, for a CLI server whose token the CLI gives,
asks the CLI again (at Refresh, or a change to the target or to the CLI). On Unix a CLI server
target's tooltip names `omnigraph credentials set --target <name> --stdin`, which keeps one for
the CLI and Studio, and a managed profile's names `omnigraph login --profile` (since 2026-10-21; before, a server sent no token was said to have refused it,
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
  still out when it changed keeps nothing: the new token may see less, and its refusals are
  shown (constitution IV). The type counts, the stored-query catalog (once a view has wanted
  it) and the branch list are read again under it, and the schema when none is held; a schema
  held, the heads and the change logs stay (the schema is answered alike to every credential
  that may read the graph; heads and logs describe commits, and carry forward only answers read
  since).
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
| Ask the Omnigraph CLI for its targets and tokens (`target list`, `target inspect`, `credentials status`, `credentials export`) | Parse the operator file and the credential sources; ask for every token again | The CLI is how people already reach their servers, and its own code is the one reader of its formats: a copy of it in Studio stopped matching the CLI once (Corrections, 2026-10-21), and RFC 1017 says Studio keeps none |
| Add a server as a CLI target through the CLI (`target add`, `credentials set`) | Keep Studio's own server list and keychain tokens | RFC 1017 rejects a client's own list and token copies: the CLI's targets are the one list, which the agent's CLI and the person's terminal see too |
| One connection per address, as the first target on it by name; the others named as alternatives | A connection per target; the CLI's default target first | A graph's key is its server's address and id; `target list --json` does not say which target is the default |
| Capabilities from `/openapi.json`, read when a route newer than the minimum server is used | Compare version strings; read the document on every connect | A server on main serves routes a release lacks; no route Studio uses needs the check today |
| Never retry writes | Retry with backoff | A lost response may have committed; only history can tell |

## Deferred

- Engine changes E1–E5, [spec 012](../012-omnigraph-engine-changes/plan.md): the `omnigraph studio` launcher, a supported local embedding
  API, the loopback guard in the launcher, owner discovery, and writer-safety rules for local
  sessions. Until then Studio connects to running servers.
- OIDC and signed data credentials: supported by the server, but Studio uses static bearer
  tokens and, for a managed cluster, the CLI's login; its own OAuth client is spec 016, which
  replaces the CLI's login for managed clusters.
- A CLI target's token on Windows. It needs a `credentials export` route the CLI offers off Unix
  (a named pipe, say), an Omnigraph change; until then such a target is refused and Add server
  takes no token there.
- Honouring `proxy: {mode: direct}` exactly: Zed's client follows the system's proxy settings and
  has no constructor that turns them off.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-01 | A link from `omnigraph studio` opens its target once the digests match, and is refused naming the one that differs | `studio` `deep_link::tests::an_open_link_names_a_target_and_its_digests` (target, both digests, the graph when named; a part missing or a digest that is not sha256 refused), `an_open_link_is_verified_against_the_cli_digests`, `start_tests::a_link_from_the_cli_opens_its_target_once_the_digests_match` (the default graph, then the named one, only that server asked), `a_link_from_the_cli_is_refused_when_its_digests_differ` (binding, home, a target not listed); live, offscreen (`examples/snapshot.rs` dispatching `studio::OpenLink`) with the link `omnigraph studio --target law --graph law --json` printed (CLI 0.14.1, the law server at `https://localhost:8453`) and the same link with its home digest changed | The real link opened law/law on main, with no toast; the changed one opened nothing, the window asking for a graph with the toast "The link was made from another operator file than this Mac's CLI reads: its home digest differs…". Before, every `open` link was refused as "Studio cannot open “open” links" |
| 2026-10-28 | A token source that cannot answer now leaves a working token in use | `graph_store` `cli_tokens::tests::a_cli_that_cannot_answer_leaves_its_token_in_use` (Refresh and a connect under a busy CLI), `a_cli_that_cannot_answer_after_a_refusal_is_asked_again` (no token kept after a 401; the next health check asks), `no_credential_sends_nothing` (extended: a missing credential is not asked again by health checks), `connection::tests::a_renewal_that_cannot_be_answered_keeps_the_token_held`, the stand-in CLI tests extended to say which failures may pass (a busy turn and a stopped export do; a refusal, a missing credential, a moved target and a held lock do not) | All pass. Before, every failure dropped the token: Refresh or a reconnect while the CLI was busy with Studio's other commands, or after an upgrade whose first run XProtect stalls, showed the server refused until the person acted |
| 2026-10-26 | A read refused for its token asks the source again, once per token, and a target's changed transport moves every holder of its client | `graph_store` `connection::tests::a_read_refused_for_its_token_asks_the_source_once` (told first, renewed once, the same token not asked twice, a typed token not asked), `registry::tests::a_renamed_cli_target_keeps_its_connection` (a moved address retires the old connection, which sends nothing; a proxy or certificate authority keeps the connection and makes one client each; an unusable transport retires it); `graph_client` `a_replaced_http_client_reaches_every_clone`; CI on PR #11 (run 40281608899), Check on macOS, Linux and Windows: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` | All pass: 1014 passed on macOS, 1009 on Linux, 979 on Windows; 0 failed |
| 2026-10-25 | Choosing a server the CLI gave no token asks the CLI again | `studio` `a_server_sent_no_token_is_said_to_ask_for_one` (a server in "No token from the CLI" chosen in the Open dialog goes to connecting; one whose token the server refused does not), run again with the fix undone | Ragnor signed in after the CLI had told Studio "login required" for `modernrelay-og-cluster`, and the dialog kept that reason: nothing asked again before a Refresh, and choosing the server did not connect it. Now choosing it, or its Retry, asks again; with the fix undone the server stayed `Unauthorized`. The reason now wraps within the pane instead of running past its edge |
| 2026-10-25 | A managed cluster is reached on the CLI's login | `graph_store` `cli_tokens::tests::a_managed_targets_token_is_exchanged_from_the_clis_login` (a stand-in CLI: the commands run, the token kept from files, a target turned into a server target while its token was read), `the_targets_are_listed_as_the_cli_describes_them` (`cloud` lands, `control` without an endpoint refused, both kept; on Windows `cloud` refused for its token); `studio` `a_server_sent_no_token_is_said_to_ask_for_one` (the dialog's words); once, by hand, Studio's listing and token source against Ragnor's operator home and `~/.local/bin/omnigraph` `0.14.1-edge+7711e4a`, in a temporary test since removed | The stand-in ran `credentials export --target cloud` and `target inspect cloud --json` only, and the token reached no file; a target turned into a server target dropped it. Live: `modernrelay-og-cluster` landed with `Managed { profile: "managed-og" }` and no notice was left for any target; the export gave a 760-byte token; `GET /graphs` was refused by the cluster's policy (`graph_list`, 403) and `GET /graphs/discovery` named 2 graphs, which the connection falls back to |
| 2026-10-25 | A CLI target that trusts its own certificate authority is reached with it, as the CLI reaches it | `graph_store` `registry::tests::a_target_is_reached_with_its_own_certificate_authority` (a test authority and an HTTPS server on `127.0.0.1` with a certificate it signed: the target's client with `append`, then `replace`; the same with another authority's bundle; Studio's own client), run again with the bundle's certificates not added; `cli_tokens::tests::a_targets_certificate_authority_is_read_as_the_cli_reads_it` (both modes, an HTTP proxy, a missing file, a folder, a file over 1 MiB, broken PEM, a key with no certificate, mode `merge`, an HTTPS proxy); `the_targets_are_listed_as_the_cli_describes_them` with a real bundle; `a_renamed_cli_target_keeps_its_connection`; the CLI's `operator_transport.rs` at `v0.14.0` read for the rules | Both modes answered 200; another authority's bundle and Studio's own client failed the handshake, as did both modes with the certificates not added. The bundle rules refuse each bad case with why, the CLI's limits; every bundle named is watched, a refused one too, and the HTTPS proxy is refused before its bundle is read. The target lands on Unix and is refused for its token on Windows. A changed bundle makes a new connection, the same one keeps it. Once, by hand, against a server whose certificate the system trusts (`omnigraph.ragnor.co/healthz`), with a test authority's bundle: `append` answered 200, `replace` failed with `UnknownIssuer`, so `replace` leaves out the system's roots. Not run against a real server with its own authority: the local `strategy` server was not running |
| 2026-10-24 | Cancel on a slow connect leaves a waiting window its token, and Refresh before the token sends nothing | `graph_store` `a_graph_waiting_for_its_token_gets_it_through_cancel_and_refresh` (a CLI server whose token arrives when the test sends it, its graph shown; Cancel, then the token; a second graph's Refresh before its token, then Cancel and Refresh), failing at the token's send with `Connection::cancel` dropping the ask; `the_first_reads_wait_for_the_token_and_the_schema_loads`, its server and token source made helpers the new test shares | Cancel left the server Idle and the ask running; the token came, and the schema, snapshot, branch list and access were read with it, the server staying as Cancel left it. Refresh before the token sent nothing; after a Cancel it connected the server again, which asked for the token, and the graph read with it, the server Ready. With the ask dropped, the token's sender found no receiver: the window stayed empty until hidden and shown. Before, Refresh sent the first reads with no token (refused, 401) and did not connect a cancelled server |
| 2026-10-24 | The Open dialog's Update token says what is true where the CLI exports no token | `studio` `a_server_sent_no_token_is_said_to_ask_for_one` (extended: the tooltip given `Platform::Other`); `ConnectionRegistry::update_token` and `Connection::set_token` by reading | On Windows the tooltip says the token lasts until Studio quits, and that Studio there cannot use a target that needs a token yet, naming no command. Typing a token sets it on the connection for this run alone: an anonymous target has no token source, so neither Refresh nor a change to its entry asks over it. On Unix it still names `omnigraph credentials set --target NAME --stdin`. Before, Windows was told to pipe the token to that command, which there either fails or makes the target one that takes a token, which Studio on Windows refuses |
| 2026-10-24 | The CLI gives a server target no identity that outlives its name, so saved state is keyed by name and address | Installed CLI `0.14.1-edge+7711e4a` in a scratch `OMNIGRAPH_HOME`: `target add` (an anonymous and a token target), `target inspect --json` and `--resolved --json`, `credentials status --target NAME --json`; the token target renamed by editing the operator file (the CLI has no `target rename`), then removed and its name added again at another endpoint; `graph_store` `a_graphs_saved_state_is_filed_by_its_targets_name_and_address` | `credentials status` reports `owner.owner_id` and `profile_id` for a token target, kept in `auth-state/<hash>.json` under `owners."server:<name>"`. The renamed target got new ids; the name removed and added at another endpoint kept the old ids (`credentials_preserved: true`), its `route` the new endpoint. An anonymous target has no owner at all, and `inspect --resolved` reports identity unknown offline. So no binding survives a rename or tells a reused name apart: Studio keys saved state by `<name>@<address>/<graph>` (`ServerEntry::saved_key`), taken when the session is made |
| 2026-10-24 | Studio drives a CLI of Omnigraph `v0.14.0` or later, and an error's disposition alone decides a write's outcome | `graph_store` `cli_tokens::tests::a_cli_is_chosen_by_its_contract_and_protocols` (stand-ins reporting `v0.14.0`'s `version --json`, an edge build's protocol 1, one with no access protocol, and 0.15.0's target protocol 3), `a_probe_is_remembered_across_launches` (a probe kept under other requirements); `graph_client` `a_disposition_decides_a_write_outcome_over_its_status`, `object_types_match_the_contract` and `enum_types_match_the_contract` against `openapi/omnigraph-0.14.0.json`, with every refusal fixture given the disposition `v0.14.0` sends (`omnigraph-server` `lib.rs` `disposition`); `~/.local/bin/omnigraph` `version --json`; `/healthz` of the hosted servers | The release stand-in is chosen, running only `version --json`; the others are passed over ("is 0.14.0-edge+7fff9df, whose target protocol 1 Studio does not know"); a probe kept under the earlier rule runs again. Effects `none` is refused on a 500, `unknown` and `partial` are unknown on a 409 and a 503, and none at all is unknown. The installed `0.14.1-edge+7711e4a` reports protocols 2, 2 and 2, which Studio before this refused ("whose access protocol 2 Studio does not know"). A server on `0.14.0-edge+50283ce` sends no disposition (its document has none), so a write it refuses reads as unknown until it runs `v0.14.0` |
| 2026-10-22 | Studio on Windows lists the CLI's targets, connects the anonymous ones, refuses each that takes a token, and adds only servers that need none | Omnigraph main `242ab02f`: `crates/omnigraph-cli/src/commands/targets/mod.rs` (`stdout_is_pipe`, `run_credential`), `commands/targets/args.rs` ("Write an access token to a Unix pipe"), `credential_sources/exec.rs` (`HelperUnsupported` off Unix); `graph_store` `registry::tests::without_token_export_only_anonymous_targets_connect` (the registry's own listing given `Platform::Other`), `cli_tokens::tests::the_targets_are_listed_as_the_cli_describes_them` (extended: the stand-in CLI listed with `Platform::Other`), `cli_tokens::every_system_tests::the_cli_is_looked_for_on_every_system` (no platform gate); `studio` `open_graph::tests::without_token_export_the_new_servers_token_field_is_disabled` and `the_new_servers_token_field_takes_a_token_where_the_cli_exports_one`; an offscreen snapshot of the Open dialog with `Platform::CURRENT` set to `Other` for the run and an empty `OMNIGRAPH_HOME`, the pointer on the token field | The CLI's export refuses every standard output off Unix, with no named-pipe route; the other four commands are not gated. With `Platform::Other`: the CLI was found and `target list --json` alone ran; only the anonymous targets were servers, one connected Ready with no Authorization; the store, env, file and exec targets were refused with the reason and asked nothing; the direct, managed and custom-CA targets kept their reasons. Add server's token field was drawn disabled, Tab went address, name, address; its tooltip read the reason; a token put in the field by the test was refused by the registry and said in the dialog, with nothing added or sent. With `Platform::Unix` the same targets landed as before and Tab reached the token field. Before, `find_cli` refused on every non-Unix system, so a Windows Studio listed no CLI target |
| 2026-10-22 | The stand-in CLI's tests hold no bound a busy machine can reach | `graph_store` `cli_tokens::tests`: `READ_TIMEOUT` an hour in tests, and the stand-ins' sources given it; `a_refused_export_says_why_and_never_the_token` (an export whose stand-in never answers, run in a turn the test holds, stopped at its own 300 ms); `tokens_asked_together_take_turns_at_the_cli` (a lock the stand-in lets go once it has refused a command, then holds); `a_command_held_back_past_its_timeout_is_not_run` (a turn the test holds) | The timeout path is reached only by a stand-in that never answers, and the busy path only by a turn the test holds; the lock is retried once, then refused through every retry. Before, on a loaded machine, 7 of the tests failed after waiting 60 s for their turn behind the others' commands ("did not answer `version --json` in time"), and the lock test released its lock from a thread 400 ms in |
| 2026-10-22 | A graph's first reads wait for its connection's token, and a new token reads again every read a 401 could have refused, the schema included | `graph_store` `the_first_reads_wait_for_the_token_and_the_schema_loads` (a CLI server whose token arrives after its graph is shown, as a start reopens a window; then a session whose schema read a stale token was refused, given a good one); `GraphSession::open_when_ready`, `Connection::token_ready` and `ConnectionEvent::TokenReady` | Nothing was sent before the token; then the schema, snapshot, branch list and access, each with it, and the schema loaded with no Refresh. The refused schema was read again under the next token. Before, measured on a 0.14 server: the CLI gave the token 30–320 ms after the session sent its first reads, the schema read was refused (401), and `read_again_for_credential` read the counts, the catalog and the branch list again but never the schema, so a reopened window stayed empty until Refresh |
| 2026-10-22 | A start connects only the servers it shows; the others connect when the Open dialog lists them | `studio` `a_link_at_a_start_connects_its_server_alone` (a window that would ask for a graph and a link, both before the targets are listed), `a_reopened_window_connects_its_own_servers` (a saved window of two targets of three, then "Open a graph") | The link: nothing sent until the targets were listed, then requests to its server alone, its graph shown and no dialog. The reopened window: its two servers connected and the third not, until the dialog listed it. Before, measured on a 0.14 server: `ask_for_graph` connected every CLI target at a start that reopened no window, even when a link opened one graph at once (10 requests to 5 servers, 15 CLI runs), and a down server's health check waited 15 s |
| 2026-10-22 | One target's token is one turn at the CLI, and the token a window waits on is served first | `graph_store` `tokens_asked_together_take_turns_at_the_cli` (extended: four targets asked at once, each target's `credentials status`, `credentials export` and `target inspect` in a row), `the_token_a_window_waits_on_goes_first` (three asks queued behind a turn the test holds, the third hurried once queued), `a_command_held_back_past_its_timeout_is_not_run` (a token's ask past its bound waiting for the turn) | Each target's three commands ran together; the hurried export ran first, then the others in the order they asked; the ask held back past its bound ran nothing and said Studio's other commands held the CLI. Before, `TURN` was taken per command, so targets asked together interleaved and every token waited for nearly every other's commands |
| 2026-10-22 | The CLI's targets are listed off the main thread, and a probe of the CLI is remembered across launches | `graph_store` `a_start_lists_the_targets_off_the_main_thread` (a listing held until the test answers it), `a_probe_is_remembered_across_launches` (the probes in memory forgotten as a launch starts; the program rewritten); `studio::open_windows` and `fill_reopened` by reading | The registry was made at once with no CLI server; `when_listed` resolved when the listing landed, with the server there. The second launch read `cli_probes.json` in the data directory and ran no command; the rewritten program was probed again (`version --json` and four `--help`). Before, measured: `ConnectionRegistry::load` blocked the main thread in `smol::block_on` for 65–85 ms at every start, the listing and its seven commands, four of them `--help` probes kept in memory only |
| 2026-10-22 | A start lists the CLI's targets once | `graph_store` `a_listing_keeps_the_stamps_it_took` (a listing that names a token file for the first time, then three checks, then the file rewritten) | No listing at the three checks; one when the file changed. Before, `keep_stamp` dropped the stamps when a listing named a file not watched before, so every start listed the targets again at the first check, 2 s later |
| 2026-10-22 | A managed target is kept as `target list` describes it, with no other command | `graph_store` `the_targets_are_listed_as_the_cli_describes_them` (the commands a listing runs: `target list --json` alone) | Both managed targets kept with their profile, cluster id and incarnation, endpoint and default graph. `ConnectionRegistry::managed_targets`, which nothing called, is removed, with the `target inspect --resolved` a listing ran for every managed target to fill it |
| 2026-10-22 | The notices about the servers are one list, each in its own words | `graph_store` `the_notices_are_one_list` (the environment's server not added, then a listing refused, then a listing's notice); `cli_servers_follow_the_cli_files` ("CLI target 'bad' cannot be reached: …") | The environment's notice first, then what the last listing could not give, which the next listing replaces; no "Omnigraph CLI config:" or "Servers at start:" prefix, since Studio reads no CLI config and only the environment's server is added at start. `studio_workspace` `a_start_notice_says_only_what_happened` asserted a notice about Studio's saved servers that nothing makes since Studio keeps none; removed with `server_notices` |
| 2026-10-22 | What only tests read is gone, and a Bearer challenge after a token68 one is still read | `graph_client` `a_401_carries_its_bearer_challenge` (`Negotiate abc==, Bearer …` and `Basic dXNlcg=, Bearer`); `graph_store` `an_anonymous_cli_target_asks_the_cli_nothing` (its token ready, Refresh asks the CLI nothing) | Both Bearer challenges read, the token68 payloads no parameter. `Challenge.token68` and `Connection::token_from_cli`, read by tests alone, are removed |
| 2026-10-22 | Studio keeps no server list; a server is added through the CLI | `graph_store` `cli_tokens::tests::a_server_is_added_through_the_cli` (a stand-in CLI with `target add`, `credentials set` and `target remove`), `registry::tests::cli_servers_follow_the_cli_files` (an address listed already, a name a target cannot have, the environment's server holding an address), `graphs_are_remembered_by_target`, `a_server_added_with_no_name_is_named_by_its_host`; `omnigraph` `0.14.0-edge+7fff9df` `target add` on a scratch home (an entry from standard input, the same name again, HTTP with a token, a bad name) | The entry reached `target add` on its standard input, the token `credentials set` on its own, and neither a command line; anonymous with no token; a refused set removed the target and said so; the CLI's refusals ("target already exists", "endpoint requires HTTPS; …", the name rule) are said as it said them. The CLI rewrote the scratch file whole |
| 2026-10-22 | The review's findings on the CLI's targets and tokens | `graph_store` `opening_a_graph_asks_the_cli_once`, `a_renamed_cli_target_keeps_its_connection` (now listed through `servers_from`, the same entry under two names), `a_start_past_its_bound_lists_at_the_first_check`, `a_program_that_did_not_answer_is_asked_again`, `a_command_held_back_past_its_timeout_is_not_run`, `the_export_runs_without_studios_omnigraph_variables` (a process of its own with `OMNIGRAPH_STUDIO_TEST_SECRET` and a kept variable set); `graph_client` `a_401_carries_its_bearer_challenge` (`Negotiate abc==, Bearer …`) | Opening and reloading a graph asked once, a typed token stood, Refresh asked again; a renamed target asked by its new name; the first check after an unfinished start listed; a program that could not start was asked again; a command held back past its bound was not run; the export's environment held `OMNIGRAPH_HOME` and the kept variable alone; the Bearer challenge after a token68 one was read |
| 2026-10-21 | Studio takes the CLI's targets and tokens from the CLI | `graph_store` `cli_tokens::tests` with a stand-in CLI (a shell script answering the four commands and `version --json` from files, its export refusing a standard output that is not a pipe): `a_cli_is_chosen_by_its_protocols_and_commands`, `the_targets_are_listed_as_the_cli_describes_them`, `a_token_is_exported_through_a_pipe`, `a_refused_export_says_why_and_never_the_token`, `only_one_token_and_a_newline_is_a_token`, `what_the_cli_said_is_one_line`, `the_cli_keeps_only_the_operator_home_and_its_own_variable`, `a_refused_token_is_exported_again_once`, `no_credential_sends_nothing`, `a_bare_challenge_asks_for_a_token`; `registry` `cli_servers_follow_the_cli_files`, `a_renamed_cli_target_keeps_its_connection`, `an_anonymous_cli_target_asks_the_cli_nothing`; `graph_client` `a_401_carries_its_bearer_challenge` | A CLI without `version --json`, without the access protocol, with target protocol 3, or without `credentials status` is passed over, each named; protocols 1 and 2 are taken. The listing in the shape `7fff9df` prints gives five servers (an anonymous one, a file source watched), refuses two direct and two managed targets and a custom CA, keeps both managed targets with their profiles, and names the second target on one address. The old format's refusal is said in the CLI's words. A token is read from a pipe after `credentials status` and kept only while `target inspect` names its endpoint; the CLI ran with no Omnigraph variable but `OMNIGRAPH_HOME`; no error, debug output or file other than the stand-in's store held the token. A missing credential sends nothing; a refused token is exported again once, the same one asked no more through three health checks, and Refresh asks again |
| 2026-10-21 | The CLI's JSON shapes | `~/.local/bin/omnigraph` `0.14.0-edge+7fff9df` (main `7fff9dfd`) on a scratch `OMNIGRAPH_HOME` with each kind of target (`target list --json`, `target list`, `target inspect … --json [--resolved]`, `credentials status … --json`, `config validate --json`), on homes with an old-format file, a `unix://` endpoint and no file, and `credentials export --target personal` counted with `wc -c` | As "What exists today" says. The human `target list` marks the default target; the JSON does not |
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
| 2026-10-31 | A CLI transport's client, made once | `graph_store` `registry::tests::a_renamed_cli_target_keeps_its_connection` (extended: another endpoint behind the same certificate authority makes no client). Fails without its fix (checked) | Taken: the client prepared off the main thread for a listing's transport was taken by the first target naming it, so a second target with the same transport (two endpoints behind one company certificate authority), or a new target with a transport a connection held, built its own on the main thread, which reads the system's roots and stalls the window. The registry now keeps one client per transport, shared by every target naming it and let go when none does, and each listing prepares every transport it names, since a listing applied meanwhile may let a client go (`a_transport_that_could_not_be_made_is_not_made_again_here`). A listing the CLI was too busy to make kept the operator file unstamped, which a missing file matches, so with no operator file the busy notice stood until something else changed; it keeps a stamp no file has, as does a listing that failed in a way that may pass (a CLI that could not be run, or did not answer in time), so the next check lists again (`the_targets_are_listed_as_the_cli_describes_them`) |

## Corrections

- 2026-10-22 · This plan still described what `1b5a110` and the settings' one CLI changed. It
  said Studio ran the CLI one command at a time, each bounded with its wait for a turn; it runs
  one sequence at a time (a target's three token commands together), the one a window waits on
  first. It said a start listed the targets within 5 s before its windows looked for their
  servers; the listing runs off the main thread with no bound at start. It said managed targets
  were kept as `target inspect --resolved` describes them (`ConnectionRegistry::managed_targets`,
  removed) and named `ConnectionRegistry::aliases_for` (removed), `Connection::refresh` (Refresh
  is `GraphSession::refresh`, which calls `Connection::refresh_token`), a server's keychain entry
  and the "Servers at start:" prefix, none of which exists. It did not say that Studio looks
  first at the `omnigraph_cli` setting, that the probe checks the HTTP contract and is kept in
  `cli_probes.json`, that the CLI's environment loses every `OMNIGRAPH_` name in any case, that
  the agent's CLI is found the same way, that a graph's first reads wait for the token, or that
  a new token reads the stored-query catalog only once a view has wanted it and the schema when
  none is held. "What exists today" gave Omnigraph `v0.13.0`'s contract as the current one, and
  Deferred still listed contract 0.14, which Studio speaks; that item is removed.

- 2026-10-22 · Studio kept a server list of its own: servers added in Studio, in
  `servers.json` in the data directory, each token in the OS keychain under `omnigraph-studio`
  and read back at start, and the graph ids remembered for each server. RFC 1017 rejects a
  client's own list and token copies, so it is removed, with no conversion: Add server adds a
  CLI target through the CLI, `servers.json` is ignored, the keychain items Studio made stay
  where they are, unread, and the graph ids are remembered by target name in
  `known_graphs.json`. The tests of that list (`an_unreadable_server_list_is_set_aside_and_said`,
  the keychain read of `a_connect_waits_for_its_token_and_a_cancel_stays`) are removed or
  rewritten; the rows of the validation log that name them stand as checked then.

- 2026-10-21 · Studio read the CLI's operator file itself, in the target format the CLI read
  from main `2c9eb1e0` (`target_config_version: 1`, a `credential` owner per server target,
  `OMNIGRAPH_TOKEN_<OWNER>`, and `[<owner>] token = …` in `credentials` bound to an endpoint),
  and resolved tokens as that CLI did. The CLI stopped reading that format at `f7db38da`
  (#1115), so a migrated home gave Studio no servers. That reader, its token resolution, the
  grouping of targets that shared an owner, the hints that named `omnigraph target import` and
  `omnigraph login --target`, and their tests (`reads_servers_with_their_tokens_and_stamps_the_files`,
  `targets_on_one_endpoint_are_one_server`, `reads_targets_with_their_endpoint_bound_tokens`,
  `reads_a_token_section_and_refuses_open_permissions`) are removed, with no fallback. The
  decision that Studio reads the file itself rather than running the CLI is reversed. The rows
  of the validation log that name those tests stand as checked then.

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
