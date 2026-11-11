# 029: The first run

Status: implemented · 2026-11-11 (approved the same day). Built: every part below, with the changes in Corrections.

## Problem

A review of Studio's first run, for three kinds of new user, found that each one hits a wall
Studio could have explained:

- **Someone with a graph** (local, served with `omnigraph-server`, or in Omnigraph Cloud) is put
  straight into "Add a server", whose Address placeholder `http://127.0.0.1:8787` invites a setup
  the Omnigraph CLI refuses as soon as a token is typed. A graph id that does not exist closes the
  dialog and leaves a window that says only "Studio could not read the schema." A server started
  with `--unauthenticated` answers `GET /graphs` with 403, which the dialog calls "could not be
  read".
- **Someone with no graph** finds no way to make one. The commands exist in the CLI, but Studio
  never shows them, and the one trap in them (the lock `cluster apply` keeps) stops the server
  from starting.
- **Someone connecting a Claude or Codex subscription** for the agent panel gets "Authentication
  required" with nothing to do about it, an error about a Settings field that does not exist
  when Node.js is missing, and an Agent action that does nothing when no graph is open.

Around all three: a missing or old CLI is said in a 10 s toast behind the modal, a refused Cloud
login keeps saying "Signed in", a failed first sign-in disables sign-in until Studio restarts,
and a ready server is labelled "contract 0.14".

## Principles

- **I** (HTTP only): the start screen, the waits and the error pages read only through
  `graph_client`; Studio runs the CLI for targets as today, and never runs `cluster` or
  `omnigraph-server` itself.
- **III**: nothing here writes graph data. Adding a target is a CLI write the person asked for,
  never retried by itself.
- **VI** (every control explains itself; show the CLI command): the local recipe, the install
  command and the login commands are the CLI's own, shown with Copy.
- **VII** (loudness matches cost; an error says what to do next): every state below names its
  next step, beside the button that takes it. The CLI notice leaves the toast for a lasting
  state in the dialog.
- **VIII** (keyboard first): the three start choices are rows ↑/↓/↵ reach; Check again, Retry
  and Restart are buttons with bindings where they are actions.
- **X**: every engine gap found is filed in spec 012 (E45–E47) and designed around.

## What exists today (checked)

Studio at `b494fc68` (main, 2026-11-11). Omnigraph CLI and server `0.15.0`
(`omnigraph --version`), contract 0.14.

### Open a graph

- With no server listed the dialog opens on the add form: `adding: connections.is_empty()`
  (`crates/studio/src/open_graph.rs:200`). The Address placeholder is `http://127.0.0.1:8787`
  (`:154`). The pane with no server says "Add a server to start." (`:893`).
- Add and connect runs `target add NAME` with `auth: {provider: token, source: {store: {}}}` when a
  token is typed, then `credentials set` (`crates/graph_store/src/cli_tokens.rs:651-705`). The
  CLI refuses the first step for an `http://` endpoint: `endpoint requires HTTPS; HTTP is only
  allowed for anonymous loopback servers` (validation log). So a loopback server that wants a
  token cannot be added with one.
- Update token keeps a token in memory only (`ConnectionRegistry::update_token`,
  `crates/graph_store/src/registry.rs:1548`). Its tooltip for a CLI target says to pipe it to
  `omnigraph credentials set --target NAME --stdin` (`open_graph.rs:1410-1416`), which the CLI
  refuses for an anonymous `http://` target for the reason above.
- An empty graph list says "Its graphs show once Studio connects to it." whenever the server is
  not ready (`open_graph.rs:902`), including while it refuses the token, when connecting will not
  show them. `GraphListing::Failed` says "The graph list could not be read. Enter a graph id."
  (`:886`).
- `open` closes the dialog for any id typed (`open_graph.rs:685-737`), even when the server
  listed its graphs and the id is not one of them.
- Errors render once, under the whole pane (`open_graph.rs:1543`), far from the field or button
  they are about.

### Servers started by hand

- `omnigraph-server --cluster DIR --unauthenticated` answers `GET /graphs` with `403 forbidden`:
  "server-scoped actions require an applied cluster policy permit … The management surface is
  closed by default, including with --unauthenticated." `GET /graphs/{id}/schema` answers 200,
  and a missing id answers `404 not_found` "graph not found", naming no other graph (validation
  log).
- `cluster apply --target DIR` keeps its admission lock after it finishes, and
  `omnigraph-server --cluster DIR` refuses to start until `cluster force-unlock LOCK_ID` releases
  it; the lock id is in `cluster status --target DIR --json` as `lock_id` (validation log).

### The CLI missing or too old

- `found_cli` fails with "Studio found no Omnigraph CLI it can drive (Omnigraph … or later …)"
  (`cli_tokens.rs:444-457`). The registry keeps it as a notice, and the workspace shows each
  notice as an Info toast (`crates/studio_workspace/src/studio_workspace.rs:2693-2709`), which
  sits behind an open modal and goes by itself.
- The agent panel already shows the install command for a missing agent CLI
  (`crates/agent_panel/src/cli_home.rs:338-369`).

### The window after a failed open

- A failed schema read becomes `Loadable::Failed(error.to_string())`
  (`crates/graph_store/src/session.rs:1503`); the Data panel shows "Studio could not read the
  schema." with the error's text and Try again (`crates/type_table/src/types_list.rs:337-365`).
  The status kept is gone by then: a 404, a 401 and an unreachable server look alike. The centre
  shows nothing of it.
- A server that asks for a sign-in already gets a centre page (`studio_workspace::sign_in_prompt`,
  spec 016); this spec adds its siblings.

### Copy

- A ready server's state label is `contract 0.14` (`crates/graph_store/src/connection.rs:79`).
- A server with no environment set reads "Environment not set" in the top bar
  (`studio_workspace.rs:3141`), a loopback server too.

### The agent panel

- Studio speaks ACP through `agent-client-protocol = "=2.2.0"` (`Cargo.toml:109`; schema crate
  1.9.1). `initialize` is sent at `crates/agent_panel/src/host.rs:517-535` with no `auth`
  capability, and its answer's `auth_methods` are never read (`host.rs:536-551`); nothing calls
  `authenticate`. In the schema, `InitializeResponse.auth_methods: Vec<AuthMethod>`; an
  `AuthMethod` is `Agent { id, name, description }` (the default) or `Terminal { …, args, env }`,
  which a client may not pass to `authenticate` and which an agent offers only to a client that
  sets `AuthCapabilities.terminal`. The refusal is `ErrorCode::AuthRequired` (-32000), shown as
  "Authentication required".
- That refusal reaches the person two ways: a failed `session/new` stops the agent with "The
  agent could not open a conversation: Authentication required" and a Restart button
  (`host.rs:719-734`, `panel.rs:3454-3462`, `:3059-3075`); a failed prompt adds "The turn failed:
  Authentication required" (`panel.rs:1644-1647`). Neither says how to sign in.
- The presets run `npx -y @agentclientprotocol/claude-agent-acp@latest` and
  `npx -y @agentclientprotocol/codex-acp@latest` (`panel.rs:43-52`). `resolve` looks up only
  `npx` (`program.rs:61-94`); a missing one says "Studio could not find `npx`. Install it, or
  set its path in Settings → Agent." (`program.rs:97-99`). Settings → Agent has no path field
  (`studio/src/settings_view.rs:766-805`: None, the presets, Custom). Node.js itself is never
  checked.
- With no graph open, the Agent action returns `None` from `panel::open` and does nothing
  (`panel.rs:185-186`).
- The agent never uses the OpenRouter key in Settings → Models (spec 019 rejected the agents'
  logins as a model provider, and the agent panel has no OpenRouter call); the row does not say
  so (`settings_view.rs:471-475`).
- The install command for the Omnigraph CLI is a private `INSTALL` in
  `crates/agent_panel/src/cli_home.rs:241-247`, written only into the agent's CLI wrapper; the
  person never sees it. `studio_ui::ds::code_block` (`ds.rs:2836`) has no Copy.

### Omnigraph Cloud

- A kept sign-in is taken up without a check (`crates/graph_store/src/control_plane.rs:283`).
  When its refresh is refused, the source ends (`crates/graph_store/src/sign_in.rs:1199-1211`,
  "Signed out: … Sign in again."), but `with_access` maps that to
  `ControlError::Unauthenticated(None)`, dropping the words (`control_plane.rs:727-736`), and
  `read_clusters` sets only `organizations = Failed` (`:511`): the plane stays `SignedIn`. The
  dialog then shows "Sign in needed" with a Try again that fails the same way
  (`crates/studio/src/organizations.rs:186`, `:379`), and Settings → Cloud still says "Signed in"
  (`crates/studio/src/cloud_settings.rs:426`).
- A failed read of the sign-in details sets `SignIn::Unavailable` (`control_plane.rs:335`);
  `enter` does nothing in that state (`organizations.rs:67`) and the pane has no button
  (`:399`), so only a restart or a new profile tries again.
- The pane's `SignedIn` arm says "Reading your organizations…" for everything but `Failed`,
  `Listed(vec![])` included (`organizations.rs:386`): zero organizations reads as a read that
  never ends, with no row in the sidebar and no Sign out.
- A cluster's data token that the control plane did not give is said as "The Omnigraph CLI gave
  no token for this server: …" (`open_graph.rs:1336`) for every source, though for a
  control-plane cluster the CLI was not asked.

## Design

### 1. A start screen for an empty dialog

With no server listed and no control plane signed in, the dialog's pane shows **Start** in place
of the add form, and the sidebar's first row is **Start** (above Add server…, selected). Three
rows, each a `MenuRow` with an icon, a title and one line; ↑/↓ move, ↵ opens:

| Row | Line | Opens |
|---|---|---|
| Omnigraph Cloud | Sign in and open a cluster of your organization. | The control plane's sign-in row (spec 021) when the CLI names one; else the line says "Sign in with `omnigraph login` first, then Check again" with Copy and Check again (the CLI's profiles are how Studio finds a control plane today). |
| Connect to a server | Add a server by its address. | The add form (§2, §6). |
| Make a local graph | Run a graph on this computer with the Omnigraph CLI. | The local recipe below. |

Once any server is listed the dialog opens on it as today; Start stays in the sidebar under the
servers as a muted row, so the recipe is reachable later.

**The local recipe** is one pane of numbered steps, each a code block with Copy
(`studio_ui::ds` code block; a copy is silent, VII):

1. Make the folders and files (a heredoc writing `cluster.yaml` and `schema.pg` into
   `~/omnigraph/source`, with `mkdir -p ~/omnigraph/source ~/omnigraph/data`):

   ```yaml
   version: 1
   state: { backend: cluster, lock: true }
   graphs:
     knowledge:
       schema: schema.pg
   ```

   and a two-line `schema.pg` (`node Person { slug: String @key  name: String }`).
2. `omnigraph cluster apply --target ~/omnigraph/data --config ~/omnigraph/source`
3. Release the lock apply keeps (E45):
   `omnigraph cluster force-unlock "$(omnigraph cluster status --target ~/omnigraph/data --json | sed -n 's/.*"lock_id": "\(.*\)".*/\1/p')" --target ~/omnigraph/data`
   (on Windows, PowerShell:
   `omnigraph cluster force-unlock (omnigraph cluster status --target $HOME\omnigraph\data --json | ConvertFrom-Json).lock_id --target $HOME\omnigraph\data`).
4. `omnigraph-server --cluster ~/omnigraph/data --bind 127.0.0.1:8787 --unauthenticated`
   — "Leave it running."

Under the steps, **Connect** (filled, ↵). It adds the anonymous CLI target `local` at
`http://127.0.0.1:8787` through `target add` (as Add and connect does, no token), selects it, and
the pane says "Waiting for the server at http://127.0.0.1:8787…" with Cancel. While the pane
waits, the connection is checked every 2 s (in place of the 120 s `FAILED_CHECK_INTERVAL`), for
at most 10 minutes, then "The server did not start. Check the terminal where you ran step 4."
with Retry. When it answers, the dialog opens graph `knowledge` (the id the recipe declared;
the server cannot list it, §6) and closes. An address listed already (`local` exists) skips the
add and waits on that server.

The paths are shown with `~` on macOS and Linux and `$HOME` on Windows. The recipe's text lives
in one module (`studio::first_run`) with a test that its YAML parses and its schema compiles with
`gq_language`.

### 2. A loopback `http://` server with a token

When the address is `http://` to a loopback host (`127.0.0.1`, `::1`, `localhost`) and a token is
typed, Add and connect adds the target **anonymous** (the CLI's only form for it) and gives the
token to the new connection in memory, as Update token does
(`ConnectionRegistry::update_token`). The token field's line says, before the add:
"The Omnigraph CLI keeps tokens only for `https://` servers. Studio keeps this one until it
quits." After it, the server's status line ends "Token kept until Studio quits." The registry's
`add_target` gets a `KeptInMemory` outcome so the dialog can say which happened. A non-loopback
`http://` address with a token is refused before the CLI runs, in the CLI's words.

The Update token tooltip's `credentials set` sentence is shown only for a target whose CLI auth
takes a token; for an anonymous target it says "Used until Studio quits. The Omnigraph CLI keeps
no token for a server it reaches without one." (the CLI target's auth kind is already known from
`target list`).

The Address placeholder becomes `https://omnigraph.example.com or http://127.0.0.1:8787`.

### 3. No CLI, or one too old

The registry keeps the CLI's state as a value (`CliState::Missing { why }`,
`CliState::Unfit { why }`, `CliState::Found`) beside the notices. The dialog shows it as a
lasting band at the top of the pane while it is not `Found`:

- Title: "Studio needs the Omnigraph CLI" (missing) or "Studio needs a newer Omnigraph CLI"
  (unfit), with the CLI's reason in muted text under it.
- The install command, which today only the agent's CLI wrapper carries (`cli_home.rs:241-247`):
  the constant moves to `graph_store::cli_tokens::INSTALL` beside `FIRST_CLI_RELEASE`, and a new
  `studio_ui::ds::command_block(id, text)` draws it with Copy. The agent panel and the dialog
  both use them (both sit above those crates).
- **Check again**, which asks the registry to look for the CLI and list its targets again
  (`ConnectionRegistry::refresh`), with "Looking for the CLI…" while it runs.

The toast for that notice is dropped; other notices keep their toasts. The start screen's Cloud
and local rows, which need the CLI, are disabled with "Needs the Omnigraph CLI" while the band
shows (VI).

### 4. The agent panel

Four changes in `agent_panel`, none in another feature crate:

- **Offer the agent's sign-ins.** The host keeps `InitializeResponse.auth_methods` and sends
  `HostEvent::AuthMethods`. When `session/new` or a prompt fails with `AuthRequired`, the panel
  shows a **Sign in to AGENT** state in place of the error callout: one button per `Agent`
  method, its `name` the label and its `description` the tooltip. A click sends
  `AuthenticateRequest::new(id)`; on success the panel opens the conversation again (and resends
  a message that was waiting, as Restart does); a refusal shows the agent's words under the
  buttons. Studio sets no terminal capability: it has no terminal to run a `Terminal` method in.
- **Otherwise, say the command.** With no `Agent` method offered, the state reads, for the
  presets, "Claude needs you to sign in. Run `claude /login` in a terminal, then Restart." and
  "Codex needs you to sign in. Run `codex login` in a terminal, then Restart.", each command with
  Copy; for a custom agent, "The agent asks you to sign in. Sign in with its own command, then
  Restart." Restart is the primary button there.
- **Node.js first.** Before a preset starts, `program::resolve` looks up `node` as well as `npx`
  on the agent's PATH. Missing either, the agent does not start and the panel says "The Claude
  agent runs on Node.js, which Studio could not find. Install Node.js 18 or later from
  nodejs.org, then Restart." (Codex alike). The missing-program message for a custom command
  becomes "Studio could not find `PROGRAM`. Install it, or choose another agent in Settings →
  Agent." (the field that exists).
- **No graph open.** The Agent action shows an Info toast "Open a graph first: the agent works on
  the graph in front." with **Open a graph…** (`OpenGraph`), in place of doing nothing.

### 5. A centre page per failed open

`GraphSession` keeps why its schema read failed as a kind, not only text:
`SchemaFailure::{NotFound, Unauthorized, Forbidden, Unreachable, Other}` from `ReadError`
(`Refused` with status 404/401/403, `Transport`). The workspace shows a page in the centre while
the schema failed and no tab is open, as `sign_in_prompt` does:

| Kind | Title | Detail | Actions |
|---|---|---|---|
| NotFound | "No graph named `ID` on SERVER" | the server's graph ids when it lists them ("It has: a, b, c"), else "This server does not list its graphs." | a row per listed graph (opens it in this window), **Open a graph…** |
| Unauthorized | "SERVER refused the token" | the refusal's reason | a masked token field and **Update token** (the dialog's flow), **Open a graph…** |
| Forbidden | "This token may not read `ID`" | the server's reason | **Open a graph…** |
| Unreachable | "Could not reach SERVER" | the transport error, and for a loopback address "Is `omnigraph-server` running?" | **Retry** (reloads the schema), **Open a graph…** |
| Other | "Studio could not read the schema of `ID`" | the error | **Try again** |

The Data panel keeps a one-line note pointing at the page ("See the page in the centre.").

The dialog stops a mistyped id before it closes: when the selected server lists its graphs
(`Listed` or `Discovered`) and the id is not among them, Open says, beside the id field,
"SERVER has no graph `ID`. It has: …" and stays open. An unlisted server opens it as today, and
the centre page catches a 404.

### 6. A Graph field on Add server

The add form gets an optional **Graph** field after Name ("Graph id, if the server does not list
its graphs"). When given, it is remembered with the server (`remember_graph`) and opened once the
server connects. For `GraphListing::NotAllowed` and a 403 the note becomes "This server does not
list its graphs. Enter a graph id." in place of "could not be read"; `Failed`
keeps its text only for other failures.

### 7. Cloud sign-in states

- **A refused kept sign-in signs out.** When the plane's token source ends (its refresh refused),
  `ControlPlane` moves to `SignIn::Offered { why: Some(words) }` and drops the source, as
  `Connection::no_token` does for a server. `with_access` keeps the source's words in
  `Unauthenticated(Some(why))`. The dialog then offers Sign in with "Your sign-in at SERVICE
  ended. Sign in again.", and Settings → Cloud follows (it reads the same state).
- **A failed first sign-in can be tried again.** In `SignIn::Unavailable`, the pane shows the
  reason and **Try again**, and `enter` (↵) runs it: it reads the sign-in details again
  (`ControlPlane::sign_in`, which already does so while it holds no service).
- **Reading only while reading.** The pane says "Reading your organizations…" only for
  `Organizations::Reading`; a `SignedIn` plane holding no list and no read starts one.
  `Listed(vec![])` shows **No organizations**: "You are signed in as ACCOUNT, but no organization
  has you as a member. Ask an owner of one to invite you, then Refresh." with **Refresh** and
  **Sign out**. The sidebar keeps a row for such a plane ("No organizations"), so the keyboard
  reaches it.
- **Data-token words name their source.** For a `ServerSource::ControlPlane` connection the line
  reads "The control plane gave no token for this cluster: REASON"; the CLI wording stays for CLI
  targets.

### 8. Copy

| Where | Today | Becomes |
|---|---|---|
| Connection state, ready (`connection.rs:79`) | contract 0.14 | Connected (the contract stays in the chip's tooltip) |
| Top bar, environment unset, loopback server (`studio_workspace.rs:3141`) | Environment not set | Development (shown, not saved: the menu still sets one) |
| Empty graph list, server refusing (`open_graph.rs:902`) | Its graphs show once Studio connects to it. | while connecting: "Its graphs show once Studio connects."; while refused: "Its graphs show once the server accepts a token."; while unreachable: "Its graphs show once Studio reaches it." |
| Dialog errors (`open_graph.rs:1543`) | one line under the pane | each next to its field or button: the id field's under it, Add's under the form's buttons, Update token's under its field |
| Settings → Models, the OpenRouter key (`settings_view.rs:471-475`) | "Your own provider keys live in your OpenRouter workspace…" | "For Studio's own model features, such as filters from a sentence. The agent panel uses your Claude or Codex sign-in instead." |

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Show the local recipe as commands to copy, and wait for the server | Run `cluster apply` and `omnigraph-server` from Studio | Studio runs no server and takes no cluster admission (constitution I, spec 012 E5); a server Studio started would stop when Studio quits. E1 (`omnigraph studio`) is the engine's answer. |
| Keep a loopback server's token in memory | Write it to Studio's own keychain item; refuse the add | The CLI holds every target's credential (spec 003); a second store would drift from it. Refusing leaves no way in. E46 removes the need. |
| Add the target anonymous and hold the token | Add no target, connect to the URL directly | A target keeps the server listed across starts and gives the agent's CLI the same server (spec 013). |
| Release the lock with a one-line pipe through `sed` | Two steps with a typed id | The person copies once; the PowerShell form covers Windows. E45 removes the step. |
| Poll every 2 s only while the recipe's pane waits, for at most 10 min | A background watch; the 120 s failed interval | Bounded work (IX); the person is looking at the wait. |
| Failure kind on the session, page in the workspace | Format the status into the error string | The page's actions differ by kind; parsing text back is fragile. |

## Deferred

Engine changes, filed in [spec 012](../012-omnigraph-engine-changes/plan.md) and
[docs/deferred-engine-changes.md](../../docs/deferred-engine-changes.md):

- **E45**: a direct `cluster apply` that releases the admission lock it took once it ends (or a
  `--release-lock`), so a local server starts without `force-unlock`.
- **E46**: the CLI keeps a token for a loopback `http://` server.
- **E47**: an unauthenticated server lists its graphs (`GET /graphs` without a cluster policy on
  `--unauthenticated`), so the Graph field and the 404 page's "does not list its graphs" go.

E1 (`omnigraph studio`) would replace the local recipe.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-11 | The CLI refuses a token target at a loopback `http://` endpoint | `target add loc --json` with `{"kind":"server","endpoint":"http://127.0.0.1:8787/","auth":{"provider":"token","source":{"store":{}}}}` on a fresh operator file, CLI 0.15.0 | `targets.loc.endpoint: endpoint requires HTTPS; HTTP is only allowed for anonymous loopback servers` |
| 2026-11-11 | The recipe makes a graph a server can serve | `cluster apply --target DATA --config SRC --json` with the cluster.yaml and schema above | `knowledge` `outcome: created`, `converged: true` |
| 2026-11-11 | The server will not start while apply's lock is held | `omnigraph-server --cluster DATA --bind 127.0.0.1:8799 --unauthenticated` before `force-unlock` | exits with a panic at `crates/omnigraph-server/src/settings.rs:79`; after the `sed` one-liner's `force-unlock`, `serving omnigraph … graph_count=1` |
| 2026-11-11 | An unauthenticated server refuses `GET /graphs`, serves the schema, and 404s a missing graph without a list | curl with `Omnigraph-Http-Api: 0.14` against that server | 403 `forbidden` (cluster policy permit), 200 for `/graphs/knowledge/schema`, 404 `not_found` "graph not found" for `/graphs/nope/schema` |
| 2026-11-11 | Studio never reads the agent's `auth_methods` nor calls `authenticate` | `grep -rn "auth_methods\|authenticate\|AuthenticateRequest" crates/` | no match outside the ACP crate |
| 2026-11-11 | An `Agent` auth method may be passed to `authenticate`; a `Terminal` one may not | `agent-client-protocol-schema-1.9.1/src/v1/agent.rs:581-709` | as stated; `Terminal` needs `AuthCapabilities.terminal` (`client.rs:2508`) |
| 2026-11-11 | Settings → Agent has no path field | `studio/src/settings_view.rs:766-805`, `studio_settings.rs:441-453` | segments None / presets / Custom; `agent.command` and a hidden `agent.env` |
| 2026-11-11 | A refused kept Cloud sign-in leaves the plane `SignedIn` | `control_plane.rs:283`, `:511`, `:727-736`; `sign_in.rs:1199-1211` | the source ends, the plane's `sign_in` is never changed |
| 2026-11-11 | The recipe's commands, as Studio shows them, make a graph a server serves | `steps(false)`'s first three commands run in a shell with `HOME` set to a scratch folder, CLI 0.15.0 | `cluster.yaml` and `schema.pg` written, `knowledge` created, the lock released |
| 2026-11-11 | An anonymous server's discovery read is a 401, not a 403 | `curl -H 'Omnigraph-Http-Api: 0.14' http://127.0.0.1:8792/graphs/discovery` on a server run `--unauthenticated` | 401 `unauthorized` "missing bearer token"; Studio said "could not be read" until it took 401 as `NotAllowed` too |
| 2026-11-11 | Each screen draws as designed | `crates/studio/examples/snapshot.rs` at 1280×800: no CLI (`PATH` without it), no targets (a scratch `HOME`), the recipe (↓ ↓ ↵), a seeded and an empty `--unauthenticated` server (`OMNIGRAPH_STUDIO_SERVER`), a missing graph id, a closed port, and Open a graph over the anonymous server | the band with the install command over the choices, Connect and Local disabled; the recipe's steps with Copy; Connected and Development in the top bar; the 404 and unreachable pages in the centre with the Data panel pointing at them; "This server does not list its graphs." |

## Corrections

- **Omnigraph Cloud needs no CLI.** Studio holds its own sign-in to the control plane the
  `omnigraph_cloud` setting names (spec 021), so the Cloud choice goes to that plane's sign-in
  and is offered with or without a CLI. Without that setting it says to set it in Settings.
- **Get started is one sidebar row**, after Add server…, whether or not servers are listed: one
  place, so it never moves (constitution V).
- **The 401 page sends the person to the dialog** for a new token rather than holding a token
  field of its own: the workspace crate holds no editor, and Open a graph now opens on the
  window's own server, where Update token is.
- **A cluster's missing data token names the control plane**, not Omnigraph Cloud: a CLI profile
  may name a control plane that is not Omnigraph Cloud. "No token from the control plane" and
  "The control plane gave no token for this cluster: …".
- **The page fills an empty centre only.** A tab open over a graph that would not open stays as it
  is (a restored tab, a query being written); the page shows where the centre would be empty, and
  the Data panel says "The graph did not open." with the server's words and Try again.
- **Dialog rows are one line**, title and meta side by side as the graph rows are, rather than a
  new two-line row size.
- **The agent's sign-in restarts the agent's conversation** once the agent accepts it, on the same
  agent process, which the panel holds while the sign-in runs.
