# 016: Studio as an OAuth client

Status: implemented for a CLI server target with `auth: anonymous` whose server offers sign-in
(`oauth_client`, `graph_store::sign_in`, 2026-10-26), and tested end to end against Omnigraph
v0.14.0 (`script/dev-sign-in`). Studio's client ID is its metadata document,
`https://www.omnigraph.dev/oauth/studio-client-metadata.json` (ModernRelay/brand#26), and the
managed control plane's AuthKit takes it on any loopback port (2026-10-29). A managed target
still reaches its cluster on the CLI's login (spec 003, since 2026-10-25) until the managed
control plane's other prerequisites below are met ·
2026-10-29

## Problem

A server that admits people through OIDC asks for a short-lived access token issued for it,
and Studio has no way to get one. Studio connects with no credential or with a static token the
Omnigraph CLI exports for a server target. Against an OIDC
server it gets a 401 and can only say so. A managed target is reached for now with the data
token the CLI exchanges from the person's `omnigraph login` (spec 003), an interim the person
chose on 2026-10-25 against RFC #1017's Appendix A2. That token is the CLI's: the CLI asks for
3600 s and caches it, a cached `cluster token` can last up to 86400 s, removing the person's
issuer membership does not retract it, and Studio acts as the CLI's principal rather than as a
client of its own, which the server cannot tell apart or give less authority. This plan
makes Studio a standard OAuth public client of any OIDC-enabled Omnigraph server, self-hosted or
managed, signing a person in through their browser.

## Principles

- I: the server stays the authority. Studio learns how to sign in from what the server
  publishes (its 401 challenge and protected-resource metadata), never from a guess about its
  host, and the server alone verifies the token.
- III: a write has three outcomes. A 401 is a refusal that ran nothing; Studio never sends a
  write again on its own after a sign-in, and never re-sends one whose outcome it does not know.
- VI: every control explains itself. The connection says who is signed in, to which sign-in
  service, and why a sign-in is needed.
- VII: loudness matches cost. A browser opens only when the person asks; refreshes are silent.
- IX: work is bounded. Every fetch, the sign-in wait and refresh retries have limits; one
  refresh runs at a time per sign-in.
- XII: evidence over assumption. Every server fact below is checked against Omnigraph main.

## What exists today (checked)

- Studio:
  - Studio keeps no server list or tokens of its own (spec 003): every server is a CLI target
    (`omnigraph target list --json`), its token comes from `omnigraph credentials export`, and
    Add server runs `omnigraph target add` and `omnigraph credentials set --stdin --replace`.
    The OAuth refresh token below is the only secret Studio would keep.
  - GPUI's `write_credentials(url, user, secret)` and `read_credentials(url)` reach the OS
    keychain, one item per key string: on macOS a `kSecClassInternetPassword` whose
    `kSecAttrServer` is the key and whose `kSecAttrAccount` is the user; a write replaces the
    item for that key (`gpui_macos/src/platform.rs`, Zed `v1.23.2`).
  - A 401 is `ConnectionState::Unauthorized`, and the connection knows whether a token was sent
    (`refused_with_token`); the copy is "Asks for a token" or "Token refused" (spec 003,
    2026-10-21).
  - Direct targets are refused, and managed targets are refused until this plan is built
    (spec 003); the CLI runs one command at a time, since it holds its operator home's lock
    (`d9b361d`).
  - Studio already binds a loopback listener for the agent gateway (spec 011, `a56532b`).
- Omnigraph (main at `7fff9dfd`):
  - Every 401 carries `WWW-Authenticate: Bearer`, adding `error="invalid_token"` when a
    presented bearer was refused and, with OIDC configured,
    `resource_metadata="<url>"` (`crates/omnigraph-server/src/challenge.rs`;
    `docs/user/operations/mcp.md`, "Sign-in and discovery"). One server has one resource
    identifier, so every route's challenge names the same metadata.
  - The metadata is served at `/.well-known/oauth-protected-resource` and at that prefix
    followed by the resource identifier's path, and is
    `{"resource": <audience>, "authorization_servers": [<issuer>], "bearer_methods_supported":
    ["header"]}` (`crates/omnigraph-server/src/handlers.rs`, `src/oidc_identity.rs`
    `resource_metadata`).
  - An admitted human access token is RS256, names exactly one audience (the configured
    resource), the configured organization and an admitted subject, and expires within 300
    seconds of issuance. OAuth-client ID tokens and delegated or impersonated credentials are
    refused. Every refresh must request the exact resource again and check the returned
    audience (`docs/user/operations/server.md`, "OIDC resource identities and MCP").
  - The server's admission snapshot is checked locally every 5 seconds and lives at most 300
    seconds, so a publisher outage closes OIDC access within 300 seconds (330 with clock skew)
    while the process runs on (same section; identity RFC, "Provider-native access and standard
    clients").
  - A managed target names a profile (with `organization_id`) and an optional data `endpoint`;
    without one the registration is control-only. `target inspect` describes it without network
    access (`docs/user/cli/targets.md`).
  - A managed data token is a bearer credential valid for up to 24 hours that removing
    admission does not retract; clients other than the CLI should be OAuth clients instead
    (RFC #1017, Appendix A2; tracking issue #1025).

## Design

### When Studio signs in

A server is signed in to through OAuth when its 401 challenge names `resource_metadata` and
Studio holds no static credential for it:

| Server | Credential |
|---|---|
| CLI server target with a `token` or `exec` provider | The CLI's exported token (spec 003). A refused one is said as refused; Studio does not swap in OAuth. |
| CLI server target with `auth: anonymous` | None; if the server's challenge offers OAuth, "Sign in" is offered (built 2026-10-26). |
| CLI managed target with a data `endpoint` | OAuth once the managed control plane offers it and registers Studio; until then the CLI's login (spec 003). |
| CLI managed target without an `endpoint` | Refused: "This managed target has no data endpoint." |
| Direct target | Refused (spec 003). |

Studio keeps no server list of its own (RFC #1017, "Changes by repository"): every server is a
CLI target, and a target names one identity, so two targets on one endpoint are two accounts.

A challenge without `resource_metadata` offers no sign-in, and Studio says the server asks for
a token, as today.

### Discovery

1. **Protected-resource metadata.** Take the URL from the challenge's `resource_metadata`, which
   the server derives from its resource identifier (RFC 9728 §3.1). With no challenge yet,
   Studio sends one request without a credential to get one; it never assumes the metadata
   sits at the host's root. The URL must be on the endpoint's origin. `GET` it over HTTPS (HTTP
   only for a loopback endpoint), 10 seconds, at most 64 KiB, JSON, no redirect followed.
2. **Check it.** `resource` must be an absolute HTTPS URL (HTTP only on loopback) on the
   endpoint's origin, for every target, managed ones included: a managed cluster's resource
   names its own data host, `https://<data host>/mcp`. A resource on any other origin is
   refused, so a data host cannot have Studio fetch a token for someone else's resource. The
   token is requested for exactly that resource, as written, and sent only to the endpoint, on
   REST and `/mcp` alike (RFC #1017, A2): a server has one audience and accepts it on every
   route. `authorization_servers` must name exactly one
   issuer, an HTTPS URL; several are refused for now (open question 6).
   `bearer_methods_supported`, when present, must include `header`.
3. **Authorization-server metadata.** Fetch RFC 8414's
   `/.well-known/oauth-authorization-server` for the issuer (path inserted after the host),
   else OpenID Connect discovery's `/.well-known/openid-configuration`; same bounds. Its
   `issuer` must equal the named issuer exactly; an issuer the resource metadata does not name
   is never used. `authorization_endpoint`, `token_endpoint` and `revocation_endpoint` must be
   HTTPS, or HTTP on loopback; the issuer must be HTTPS. `code_challenge_methods_supported` must
   include `S256`. Studio does not use a `registration_endpoint`.
4. **Transport.** Requests to the endpoint's origin take the CLI target's own transport (its
   proxy and certificate authority, spec 003); requests to any other host take Studio's own
   client, as the CLI's identity-provider traffic does not inherit a target's transport
   (Omnigraph `docs/user/cli/targets.md`, Transport and capability boundaries).

Discovery answers are kept in memory per endpoint and fetched again after a sign-in fails or
Studio restarts; they hold no secrets.

### Client identity

- Studio's `client_id` is the URL of its Client ID Metadata Document,
  `https://www.omnigraph.dev/oauth/studio-client-metadata.json`, served by the omnigraph.dev
  site (ModernRelay/brand, `apps/omnigraph/public/oauth/`) and used with an authorization
  server whose metadata says `client_id_metadata_document_supported: true`:
  `{"client_id": <its URL>, "client_name": "Omnigraph Studio", "client_uri":
  "https://www.omnigraph.dev", "logo_uri": <its logo beside it>, "application_type": "native",
  "redirect_uris": ["http://127.0.0.1/callback"], "grant_types": ["authorization_code",
  "refresh_token"], "response_types": ["code"], "token_endpoint_auth_method": "none"}`. The
  loopback redirect is listed without a port; the authorization server must accept any port on
  it (RFC 8252 §7.3), as it must for Claude Code's document, which does the same. The URL is on
  `www.` because the apex redirects, and a service must not follow a redirect to the document.
  It is Studio's identity: another URL is another client, and everyone signed in signs in again.
- An authorization server that does not accept a metadata-document client ID may have Studio
  pre-registered as a public client with loopback redirects. Its ID is kept in Studio per
  issuer, as data shipped with Studio, not typed by the person. A registered ID is used before
  the metadata document. For development, `OMNIGRAPH_STUDIO_OAUTH_CLIENT` names one client ID
  used at any sign-in service.
- With neither, sign-in is refused: "The sign-in service at <issuer host> does not accept
  Omnigraph Studio." Studio never registers itself dynamically.

### Signing in

- The person chooses **Sign in**, on the connection chip or the server's menu. Studio never
  opens a browser on its own: not on a 401, not at launch.
- Studio binds `127.0.0.1` on a free port (an IPv4 literal, never `localhost`, RFC 8252 §8.3),
  plain HTTP, and makes a `state` and a PKCE verifier, each 32 random bytes in base64url, the
  challenge being the verifier's SHA-256 in base64url (`S256`).
- It opens the system browser (`open_url`) at the authorization endpoint with
  `response_type=code`, `client_id`, `redirect_uri=http://127.0.0.1:<port>/callback`,
  `code_challenge`, `code_challenge_method=S256`, `state` and `resource=<resource>` (RFC 8707),
  and `scope=offline_access` when the authorization server lists that scope (open question 3).
- The listener takes `GET /callback` whose `state` matches; a callback with another `state` is
  answered 400 and the listener waits on, and anything else is 404. An `iss` that is not the
  issuer ends the sign-in (RFC 9207). It takes one callback, answers it with a short page
  ("Signed in to <host>. You can close this tab." or the error), and closes. It closes after 5
  minutes, on Cancel, or when the server is removed. An `error` in the callback is shown with its
  `error_description`, as plain text, cut at 300 characters. Each request has 10 seconds and 8 KiB
  to arrive.
- The code is exchanged at the token endpoint (`grant_type=authorization_code`, `code`,
  `redirect_uri`, `client_id`, `code_verifier`, `resource`), 15 seconds, at most 64 KiB. The
  answer must have `token_type` `Bearer`, an `access_token` and an `expires_in` of 300 or less.
  The token lives its `expires_in`. Its `exp` is the service's clock and is not compared with
  this machine's, which would shorten every token, or refuse the sign-in, when this clock runs
  ahead. A token is renewed no sooner than a second after it was given.
- **Checking the token.** Studio reads the access token's JWT claims without verifying its
  signature (the server does that, with the same claims) and refuses to use it unless `aud` is
  one string equal to the resource, `iss` the issuer and `client_id` Studio's own. `sub` is the
  account and `org_id` its organization. When the target names an organization (a managed
  profile's `organization_id`), `org_id` must equal it, or the sign-in is refused with both
  organizations named; a self-hosted server's organization is not published, so Studio shows
  `org_id` and leaves the check to the server. Studio never asks for or sends an ID token.

### Custody

- The refresh token goes to the OS keychain, under the target, the issuer and the resource,
  with the account as the item's user: key
  `omnigraph-studio-oauth:<target>#<issuer>#<resource>`. One account is signed in per target;
  signing in as another replaces it: the sign-in replaced ends and its refresh token is revoked,
  and the keychain keeps the new account's refresh token, or none when its grant gave none,
  never the old account's. A second account is a second CLI target. As the item is the
  target's, a target the CLI renames is, to its sign-in, the old target gone and a new one at
  the same address: a sign-in held or under way is dropped as for a target the CLI no longer
  lists, what it leaves kept under the old name, and the server offers a sign-in under the new
  name, which reads that name's item. An ending says what it left only for the item of the
  target's name now.
- The access token stays in memory, per target, for at most its lifetime. Neither
  token is written to disk, logs, saved state, links, crash reports or any agent's environment;
  the agent gateway uses Studio's connection, and the agent never sees the token.
- A keychain that refuses the write leaves the sign-in good for this run and says so ("Signed
  in for this session; the keychain did not keep it"); there is no file fallback. A sign-in the
  keychain keeps nothing of deletes the item at its target, so the next launch does not read an
  earlier account's; one the keychain does not delete is said with it. A delete is done when the
  key holds no item afterwards: macOS and Windows refuse to delete an item that is not there.
- **Sign out** drops the access token, deletes the keychain item and, when the authorization
  server publishes a `revocation_endpoint`, revokes the refresh token there (RFC 7009),
  bounded to 10 seconds and not retried. A keychain that refuses the delete leaves the person
  signed out for this run and says so ("The keychain kept a sign-in it would not delete"),
  since Studio may read the item back later. The delete does not wait for a refresh in flight,
  only for a keychain write one has under way, so quitting meanwhile leaves the next launch
  nothing to sign in with.
- A refresh token the service gave that nothing holds is revoked: one given with tokens Studio
  refuses (an access token living past the server's limit, say), the token from before, which
  that rotation spent, forgotten from the keychain; one given to a sign-in cancelled while
  its code exchange was answered, the exchange running to its end on its own; and an earlier
  item's, which a sign-in writes over or forgets (one a sign-in that ended left for the next
  launch), unless the sign-in it replaces answers for that item and revokes it itself; one the
  keychain would not let go of at the sign-in, the sign-in keeps, and revokes once the item no
  longer holds it.
- A sign-in ends one way, for a sign-out, a replacement by another sign-in, or a drop (the
  CLI's credential for the target, a retired connection, the service refusing it for good),
  and ends once. No refresh begins after, and one under way touches the keychain no more, its
  rotated token held for the ending: each keychain operation of a refresh begins only while the
  sign-in stands, under a lock its ending takes, so an ending waits for one in progress, never
  for the service. Only a drop waits for a refresh in flight (its retry included, which runs
  whether or not the sign-in ended), to keep the token it rotates or forget one it learns no
  longer refreshes. A
  sign-in under way waits for the ending of the one it replaces before it writes, that one held
  or ended by the service meanwhile. The item is then deleted (sign-out), left
  to the sign-in replacing it, or kept (drop), a drop giving the keychain a token rotated
  meanwhile; and the refresh token is revoked in the background, unless a dropped sign-in's
  token is what the keychain keeps for the next launch. Nobody waits on the revocation. The
  keychain work at an item (a sign-in's write or delete, an ending, a launch's read) takes its
  turns in order, whichever connection does it (one retired and the one that replaces it for
  the same target among them), so none overtakes another.
- Once the service gives a sign-in its tokens it is finishing, and is no longer to be
  cancelled: the sign-in it replaces ends in its keychain turn. Cancel, before then, says the
  sign-in still held again, or why it ended meanwhile, and Sign out is offered whenever a
  sign-in is held and none is under way, beside Cancel while connecting. A sign-out or a
  drop cancels a sign-in under way until it is finishing; one finishing is kept, then ends so, as
  one held would, the one it replaces ending as replaced, and no other sign-in starts until it is
  kept, so nothing is offered before. A sign-in as someone else that fails says why, and why the
  one held ended if it did meanwhile; one held that still stands reads as not admitted, or as not
  answering when its service did not answer. While a sign-in is under way, the one it replaces answering, a renewal or that it ended,
  does not change the state shown, and one the service ends meanwhile ends as replaced, its token
  revoked, since the sign-in under way answers for the item; should that one not be taken, the
  item is spent, and the next launch, refused once, forgets it. A connection released while a
  sign-in finishes (its target gone from the CLI) lets its keychain work finish, which runs on
  its own, and the sign-in then ends as the connection told it: signed out of after a sign-out,
  and otherwise dropped as for a retired connection, kept for the target's return when the
  keychain holds its token, revoked otherwise. A sign-in finishing holds its item's keychain
  turn until it is taken or has ended so, so no other work reads the item between its write and
  that ending. While a
  sign-in finishes, the one it replaces answers that it ended, and the token it gave stands
  until the new one is taken.
- What the keychain holds for the target is the connection's to say, whatever its state: the
  sign-in held answers for it, and with none held, what a sign-in that ended left there (its
  item, kept for the target made anonymous again, or one the keychain would not delete).
  It is not a message a later attempt replaces. A keychain that cannot be read is taken as
  holding nothing, as a launch takes it: one unreadable only for the moment (macOS, locked)
  can then keep an item unsaid.

### Refresh

- An access token is refreshed when 60 seconds or less of its life remain (halfway through a
  token that lives two minutes or less) and the server is in use, and before a request that
  would otherwise send an expired one. Nothing refreshes for a server no view reads; the next
  view to show it asks. A renewal replaces the token without counting as another credential, so
  nothing read is read again. A token refreshed after a 401 is a new credential and is tried at
  once. A refused token is not renewed: a renewal runs only while the server is ready, and a 401
  cancels the one scheduled. A CLI target whose transport changes keeps its sign-in: its
  requests to the server's address go through the new client from then on, and a refresh under
  way finishes and is kept. Reading a second sign-in from the keychain then would race that
  refresh for the refresh token it rotates.
- One refresh runs at a time per target (its client, issuer, resource and account), for
  every view and window; an ask while one is under way joins it and takes its answer, its
  token included, so a refresh that fails is not begun again by each ask that waited for it,
  nor does an ask take a later refresh's answer for its own. A busy token endpoint
  (429, `slow_down`, `temporarily_unavailable`) is a failure that may pass, not a refusal. A refresh runs on a task of its own to its
  end: an ask dropped mid-exchange (a connect replaced, a renewal overtaken) does not take it
  along, so a refresh token the service has rotated is never lost. These tokens are never mixed
  with a static token or the CLI's credentials.
- Every refresh sends `grant_type=refresh_token`, `refresh_token`, `client_id` and `resource`;
  without the resource an authorization server may issue a token for a default audience. The
  new token is checked as a new sign-in's is.
- Refresh tokens rotate: the one returned replaces the keychain item before the new access
  token is used.
- `invalid_grant` signs the person out of that server ("Signed out: sign in again"). A refresh
  that fails or times out may have rotated the refresh token anyway; Studio tries once more
  after 5 seconds. No refresh is retried a third time. A second failure does not end the
  sign-in: the service may answer later, and the keychain's refresh token is still good. While
  the access token held still lives it stays in use, unsaid, and the renewal is asked again
  halfway to the token's end (no sooner than 5 seconds before it); with none held the server
  reads **Sign-in not answering** with why, and the next health check asks again. Only
  `invalid_grant`, a refusal, a sign-out or a sign-in with no refresh token ends it, and after a
  401 no failure keeps the refused token.

### 401 and 403

- A 401 to a request sent with an OAuth token refreshes once. A read is then sent again once;
  a write is not sent again: it ran nothing, and Studio says so ("Signed out before the change
  was sent; nothing was written") and keeps what the person typed, as for any refused write.
- When the refresh fails, or the request is refused again, the connection becomes **Sign in
  needed**. After a fresh sign-in, a 401 again says that the server refused the signed-in
  account (its `error_description` when given) and stops; the next sign-in is the person's
  choice. There is no loop.
- A 403 is the server's policy refusing the account, shown as today. It never triggers a
  sign-in.

### What the person sees

| State | Chip | Menu |
|---|---|---|
| The server offers sign-in, none held | Sign in needed | Sign in with <issuer host> |
| Browser open | Signing in… | Cancel |
| Signed in | as today, Ready | Signed in as <account> · <issuer host> · Sign out |
| Refreshing | unchanged | unchanged |
| The service does not answer, no token held | Sign-in not answering | Signed in as <account> · <issuer host> · Sign out |
| Refused after sign-in | Not admitted | Sign in as someone else · Sign out |

### Limits

| What | Limit |
|---|---|
| Metadata and token answers | 64 KiB each |
| Metadata fetch | 10 s |
| Token exchange and refresh | 15 s |
| Sign-in wait | 5 min, one callback |
| Access token life accepted | 300 s or less |
| Refresh lead | 60 s |
| Refresh attempts | 2, 5 s apart |
| Revocation | 10 s, once |

### Prerequisites outside Studio

- **The managed control plane**:
  - enable and qualify a standard OAuth resource profile on managed servers, so they run with
    OIDC identity trust (which also admits the resource's host for MCP);
  - register Studio as a public client with loopback redirects, or accept its Client ID
    Metadata Document. Done (2026-10-29): its AuthKit takes metadata documents;
  - name each managed cluster's resource on its own data host, `https://<data host>/mcp`, and
    forward that path and its metadata through the data host's public route, which OAuth alone
    does not create. Done (2026-10-23): a client following RFC 9728 §3.3 (the MCP TypeScript
    SDK's `checkResourceAllowed` included) now finds the resource where it calls.

  Its authorization server is AuthKit. Studio must not depend on `/openapi.json` being
  reachable through every deployment's proxy.
- **ModernRelay**: publish Studio's Client ID Metadata Document at a stable HTTPS address.
  Done (2026-10-29, ModernRelay/brand#26).
- **Omnigraph**: nothing beyond what 0.14.0 serves (A1 is implemented); tracked in #1025.
- **Studio's CLI finder** (spec 003): it chooses a CLI by `version --json`, and 0.14.0's CLI
  reports access protocol 2 (target and credential 2 as well); Studio must accept it before a
  managed target's endpoint can be read from `target inspect`.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Studio is its own OAuth public client | Export the CLI's managed data token | That token lives up to 24 hours and removing admission does not retract it; an OIDC server can scope a token per client (RFC #1017, A2) |
| Client ID Metadata Document, or pre-registration | Dynamic client registration | A2 rules it out; a registration per install would leave clients the control plane cannot account for |
| Loopback HTTP redirect on `127.0.0.1` | `omnigraph-studio://` as the redirect | A private-use scheme can be claimed by another app on the machine; RFC 8252 §7.3 loopback is what A2 names |
| A browser opens only when the person asks | Open it on the first 401 | A window appearing unasked is loud and looks like phishing; a 401 can come from a background read |
| A refused write is not re-sent after sign-in | Re-send it once the token is fresh | The person may not want it after a detour through the browser; constitution III keeps writes explicit |
| One account per CLI target | Several accounts per target, kept side by side | A CLI target names one identity, and GPUI keeps one keychain item per key; a second account is a second target on the same endpoint |
| Claims read unverified, for audience and display only | Verify the JWT in Studio | The server verifies; Studio's check only stops it sending a token issued for another resource |
| The target's transport for the endpoint's origin, Studio's own client elsewhere | The target's transport for every sign-in request | The CLI's rule: identity-provider traffic does not inherit a target's transport. A sign-in service behind the server's own address (as `script/dev-sign-in` serves it) is reached as the server is |
| HTTP accepted for the authorization, token and revocation endpoints on loopback | HTTPS only | A sign-in service on this machine (a development one) answers over HTTP; nothing leaves the machine. The issuer and the resource stay HTTPS, as the server requires |
| A renewal is quiet; a refresh after a 401 is a new credential | Every new token counts as another credential | Counting renewals would read every view again every few minutes. After a 401 the views' reads failed, and reading them again with the new token is what is wanted |

## Deferred

- A managed target's sign-in, once the managed control plane offers it and registers Studio: its
  organization check (`org_id` against the profile's `organization_id`) and the end of the CLI
  login interim.
- A refused write's message after sign-in ("Signed out before the change was sent; nothing was
  written"): a 401 to a write is said as any refusal today.
- Whether managed agents use the CLI's relay or are OAuth clients themselves (RFC #1017, A3;
  #1025's last item).
- Several authorization servers for one resource (open question 6).
- Signing in from a link (`omnigraph-studio://open?target=…`): a link never starts a sign-in on
  its own; it opens the server, which then shows Sign in needed.

## Open questions

1. Settled (2026-10-29): the managed control plane's AuthKit takes Studio's metadata
   document; no environment needs a registered ID.
2. Settled (2026-10-29): AuthKit takes the document's loopback redirect on any port.
3. How long a refresh token lasts. AuthKit lists `offline_access`, so Studio requests it; that
   refresh tokens rotate and must repeat the resource is settled.
4. How a person in several organizations gets a token for the target's organization: the
   hosted flow's organization choice, or a hint Studio sends.
5. What to show for the account: `sub` is an opaque identifier, and Studio requests no ID
   token.
6. What to do with metadata naming several authorization servers (none does today).
7. Settled (2026-10-29): AuthKit publishes no `revocation_endpoint`, so Sign out deletes the
   refresh token from the keychain without revoking it, and it stays usable at AuthKit until
   it ends.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-21 | The server's challenge, metadata and token rules are as stated | Omnigraph main `7fff9dfd`: `crates/omnigraph-server/src/challenge.rs` (tests render `Bearer resource_metadata="…/.well-known/oauth-protected-resource/mcp"` and the `error="invalid_token"` form), `src/oidc_identity.rs` `resource_metadata`, `src/handlers.rs` (the two well-known routes), `docs/user/operations/server.md` and `mcp.md` | As written under What exists today; the metadata names one issuer and only `header` bearer use |
| 2026-10-21 | GPUI keeps one keychain item per key string | Zed `v1.23.2` `crates/gpui/src/platform.rs` (`write_credentials(url, username, password)`, `read_credentials(url)`), `crates/gpui_macos/src/platform.rs` (`kSecAttrServer` = the key, `kSecAttrAccount` = the user) | A write replaces the item for its key; a read returns that one item's user and secret |
| 2026-10-21 | The challenge's metadata URL follows the resource, and the server checks the claims Studio reads | Omnigraph main `7fff9dfd`: `crates/omnigraph-server/src/mcp.rs` (`advertised_metadata_url`: the well-known prefix before the resource's path, on the resource's origin), `src/oidc_identity.rs` (required `iss`, `sub`, `aud`, `iat`, `exp`; `aud` one string equal to the audience, `org_id` equal to the binding's organization, a bounded `client_id`) | A resource on another origin moves the metadata there; an `aud` array is refused, so Studio requires a string |
| 2026-10-21 | The managed control plane's own design agrees with this plan | Its identity and client specifications, read at their current head (a private repository, not cited here) | It publishes each managed cluster's resource and metadata itself, on its own origin, not on the data host; it prefers metadata-document clients; refresh tokens rotate and must repeat the resource; a client never silently opens a browser or replays a write; OAuth access needs a public route to the data host. Its OAuth profile is built, off by default and enabled per deployment |
| 2026-10-21 | A managed resource on the data host suits every client | The MCP TypeScript SDK sends `new URL(resource).href` as `resource`, which turns a bare origin into `origin/`; a resource ending in `/mcp` is sent unchanged, and `checkResourceAllowed` requires the server's origin and a path prefix | The managed resource is `https://<data host>/mcp`; Studio requires the endpoint's origin for every target, and the profile-`api` exception is gone |
| 2026-10-21 | A managed target can name its data endpoint without network access | Omnigraph main `docs/user/cli/targets.md` ("Configure destinations", `target inspect`) | `endpoint` is optional on a managed target and required for data operations |

| 2026-10-26 | The client signs in to a real Omnigraph server | `script/dev-sign-in` (Omnigraph v0.14.0 `omnigraph-server --oidc-identity-trust`, a TLS front from its own CA, a stand-in authorization server); `graph_store` `a_real_server_is_signed_in_to_through_the_bed` | The server's own 401 names its metadata; the token is accepted and the graphs list; sign-out revokes; a subject the admission file does not name gets 401 `invalid_token` again after one refresh, shown as Not admitted (Omnigraph #1172) |
| 2026-10-26 | The protocol rules hold | `oauth_client` tests (RFC 7636 vector, discovery refusals, callback state and `iss`, token claims), `graph_store` `sign_in` tests (browser sign-in, keychain custody, rotation, quiet renewal, `invalid_grant`, one refresh after a refusal, no sign-in over a CLI credential) | Pass; a refreshed token applied as a quiet renewal after a 401 was never sent, found by the bed and fixed with a test that fails without the fix |

| 2026-10-26 | Three review findings on ModernRelay/omnigraph-studio#12 (Cursor Bugbot) hold | `graph_store` `sign_in` tests, each extended to fail without its fix: `a_sign_in_the_service_ended_or_the_server_does_not_admit_is_said` (a watched server with two-second tokens refreshed again after Not admitted), `a_server_that_offers_sign_in_is_signed_in_through_the_browser` (a second account whose grant gives no refresh token left the first account's in the keychain, unrevoked), `a_new_transport_keeps_the_sign_in_through_it` (after `replace_http`, nothing read the sign-in again until a probe; then a second finding, that reading it again from the keychain raced a refresh under way) | Fixed: a 401 cancels the renewal and a renewal runs only while ready; the replaced sign-in is revoked and the keychain item replaced or deleted; `replace_http` keeps the sign-in and points its requests to the server at the new client (`OAuthSource::reach_through`, `Clients::through`), reading nothing from the keychain |

| 2026-10-26 | A second review of #12 (the session sharing the checkout, read-only) | `graph_store` `a_dropped_ask_leaves_its_refresh_to_finish` (the service holds its answer after rotating the refresh token; the ask is dropped there; the next ask is answered without asking again), `a_new_transport_keeps_the_sign_in_through_it` (extended: the server is checked again through the new client unasked); `oauth_client` token test (a clock ten minutes ahead keeps `expires_in`) | Fixed: refreshes run on their own task and asks join them; a renewal is quiet by its flag and holder, not only when the server is ready; the lifetime is `expires_in` alone, renewed no sooner than a second; a transport change checks a server in use again. Not taken here, as older than this PR: a token source's temporary failure dropping a working token, one failed health check showing Unreachable for two minutes, the CLI's stdin write without a timeout |
| 2026-10-28 | A sign-in whose service does not answer stands | `graph_store` `sign_in::tests::a_sign_in_whose_service_does_not_answer_stands` (a kept sign-in whose service answers 503 at launch, then again at a renewal), `connection::tests::a_renewal_that_cannot_be_answered_keeps_the_token_held` (asked again halfway to the token's end, then renewed quietly); `studio` `a_server_that_offers_a_sign_in_says_where_and_as_whom` (extended: the status line and chip) | All pass. At launch the server read Sign-in not answering, signed in as the keychain's account, with no browser opened and the keychain item kept; the next health check signed in. At the renewal the token stayed in use with the same credential. Before, both ended the sign-in and offered a browser sign-in. This takes the first of the three items the 2026-10-26 review left; the other two (a single failed health check, the CLI's stdin write) were fixed on main in #13 |
| 2026-10-29 | A keychain that refuses the keep or the delete is said, and a missing item is no refusal | `graph_store` `sign_in::tests::a_server_that_offers_sign_in_is_signed_in_through_the_browser` (extended: a refused write; the sign-out of that unkept sign-in; a refused delete at sign-out; a sign-in kept nothing of over an item the keychain will not delete; a refused write over an earlier item) and `a_rotation_the_keychain_refuses_leaves_what_it_holds_said` (a refused rotated token: a kept sign-in reads not kept, an earlier one stays said), with the test keychain refusing to delete a missing item; Zed `v1.23.2` and `v1.26.2` `delete_credentials` (`gpui_macos`, then `gpui_apple/src/keychain.rs`: `SecItemDelete` succeeds only with `errSecSuccess`; `gpui_windows`: `CredDeleteW` fails on `ERROR_NOT_FOUND`; `gpui_linux`: a missing item returns `Ok`) and `read_credentials` (macOS and Windows return `None` for a missing item) | Pass. The refused write reads not kept, with the keychain empty, and its sign-out says nothing; the refused delete is said at the offered sign-in, the item still in the keychain; a sign-in kept nothing of reads that the next launch reads the earlier one; a refused write over an earlier item deletes it. Before, every refused delete was dropped without a word, so the next launch read the item back and, where the service had not revoked it, signed in with it, an earlier account's included. Checking the first fix against the platforms found that a sign-out with nothing to delete would have been said as a refusal on macOS and Windows: a delete is now done when the key holds no item afterwards |
| 2026-10-29 | An independent review of the custody change holds | `graph_store` `sign_in::tests`: `a_delete_is_done_when_the_next_launch_reads_nothing`, `a_rotation_the_keychain_refuses_leaves_what_it_holds_said` (now refreshing, checked by the service taking the token), `a_grant_the_service_ended_says_what_the_keychain_kept`, `a_refresh_that_answers_after_its_sign_out_keeps_nothing`; `studio` `a_server_that_offers_a_sign_in_says_where_and_as_whom` (Not admitted over an earlier item) | Pass. Taken: the rotation test passed without refreshing, as an ended access token counts as fresh for a second; a read that failed counted as an item left, where a launch reads nothing, so a Linux keychain without a Secret Service would have said a refusal at every sign-in and sign-out; an ended grant dropped a refused delete; Not admitted did not say an earlier item; a sign-out's answer came after its revocation and was matched by the service alone, so it could land on a later sign-out, and a refresh running across a sign-out wrote its rotated token after it began. Not taken: a refused rotation reads not kept although the key holds this account's token from before, which a service that keeps old refresh tokens valid would honour at the next launch, for the same account |
| 2026-10-29 | A second independent review of the custody change holds | `graph_store` `sign_in::tests`: `a_refresh_that_answers_after_its_sign_out_keeps_nothing` (extended: the sign-out revokes the rotated token), `a_sign_in_made_during_a_sign_out_reaches_the_keychain_after_it` (a delete held open; fails without the turn, checked by removing it), `a_server_that_offers_sign_in_is_signed_in_through_the_browser` (extended: an item left survives a sign-in begun and cancelled), `a_grant_the_service_ended_says_what_the_keychain_kept`; `studio` `a_server_that_offers_a_sign_in_says_where_and_as_whom` (an item left, said while a sign-in is offered) | Pass. Taken: a refresh answered after its sign-out kept nothing, but so held no rotated token, and the sign-out revoked the retired one, leaving the live token valid; an item left was said only in a message, which a sign-in begun and given up, or failed, replaced; the turn had no test; the sign-in-not-answering status did not say an item left. Stated rather than changed: an unreadable keychain counts as holding nothing |
| 2026-10-29 | A third independent review of the custody change holds | `graph_store` `sign_in::tests::a_sign_in_made_during_a_sign_out_reaches_the_keychain_after_it` (rewritten: a sign-in cancelled while its turn waits revokes its token; one let through holds its own token, compared exactly), `a_server_that_offers_sign_in_is_signed_in_through_the_browser` (an unkept sign-out leaves nothing said, now checked where it is kept); `studio` `a_server_that_offers_a_sign_in_says_where_and_as_whom` (what was kept, said while the server is unreachable) | Pass. Taken: what a held sign-in kept was read only while it showed signed in, so after Not admitted a sign-in begun and given up lost it; the status said it only in sign-in states; a sign-in cancelled while its keychain work waited dropped a refresh token the service honours; a sign-in took its turn when it began, and turns did not chain; a launch's read took none; two tests checked less than they claimed; this log held the 2026-10-29 second-review row twice |
| 2026-10-29 | A fourth independent review of the custody change finds the queue and the revocation guard sound | `graph_store` `sign_in::tests::a_server_that_offers_sign_in_is_signed_in_through_the_browser` (extended: the CLI's credential replaces a sign-in whose token the keychain does not hold, over an earlier item) | Pass. It found no defect in the queue, `Unclaimed`, `kept_behind` or the status, and checked that turns resolve, the queue keeps about two links, and the ordering tests fail without their fixes. Taken: a sign-in dropped for the CLI's credential lost an earlier item's warning and left its refresh token, which the keychain did not hold, unrevoked; the guard was made a tick after the exchange, so a cancel in between dropped the tokens; a launch read that found nothing left an item said. Not taken, as before this change and needing an unlikely race: a late answer from a sign-in already replaced is not told from the current one's |
| 2026-10-29 | The two items left open are closed | `graph_store` `sign_in::tests::a_rotation_the_keychain_refuses_leaves_what_it_holds_said` (extended: a refused rotation forgets the token from before, and says one the keychain will not delete), `connection::tests::a_token_answered_after_its_source_was_replaced_is_not_taken`, `a_connect_whose_source_was_replaced_asks_the_new_one` | Pass. A refused rotation left this account's token from before in the keychain while saying the sign-in lasts until Studio quits; a service that keeps old refresh tokens honoured would have signed the next launch in with it. It is forgotten now, as a sign-in that keeps nothing forgets. A token source's answer was taken whatever source the connection had come to take its token from: a Refresh or renewal under way at a sign-out could put the signed-out token back. An answer is now taken only from the source still in use, and a connect that waited on a source replaced meanwhile asks the one now in use, which may give the token already held and so start no connect of its own |
| 2026-10-29 | A fifth independent review, of the last three commits | `graph_store` `connection::tests::a_connect_whose_source_was_replaced_asks_the_new_one` (extended: a window waiting after a Cancel), `sign_in::tests::a_sign_in_dropped_during_a_refresh_is_decided_after_it` | Pass. It found no answer of the source in use dropped, no double revocation and no guard held across an await. Taken: a connect's answer dropped for a replaced source, the connection idle after a Cancel, left a waiting window without its token; a sign-in dropped for the CLI's credential was judged before a refresh under way settled, so a token it rotated could be left with nobody; a sign-in given up while it waited for the one it replaced could lose that one's revocation. Endings now run on their own once begun |
| 2026-10-29 | A sixth independent review, of the endings commit | `graph_store` `sign_in::tests::a_refresh_that_answers_after_its_sign_out_keeps_nothing` (extended: the sign-in reads not kept), `connection::tests::a_connect_whose_source_was_replaced_asks_the_new_one` (extended: the target made anonymous while a window waits) | Pass. It found no defect in that commit. Taken, from its edges: a refresh answered after its sign-in ended still said the keychain held the sign-in, so an ending deciding by that (the CLI's credential) could leave the rotated token unrevoked; a window waiting for a source's answer did not hear the token was ready when the source went |
| 2026-10-29 | Studio's metadata document follows the draft and the clients that use it | draft-ietf-oauth-client-id-metadata-document (July 2026) §3 (URL rules), §4 (`client_id` equal to the fetched URL, served 200 as JSON), §4.2 (exact redirect match), §5 (no redirect followed); the documents Claude Code and VS Code publish; `graph_store` `sign_in::tests::studio_s_client_id_is_a_metadata_document_url` | The URL is `https` with a path and no query or fragment, on `www.` since the apex redirects. §4.2 asks for an exact redirect match while Studio's port changes at each sign-in; Claude Code lists `http://127.0.0.1/callback` without a port too, so a sign-in through AuthKit on a free port is the check that remains (open question 2) |
| 2026-10-29 | The managed control plane's AuthKit takes Studio's metadata document on any loopback port | `creative-fruit-62.authkit.app/.well-known/oauth-authorization-server`; its `/oauth2/authorize` with Studio's client ID, PKCE S256, `scope=offline_access` and `redirect_uri` `http://127.0.0.1:54321/callback`, then port 61987, then the path `/elsewhere`, then `http://localhost:54321/callback`, then with a `resource`; the document served by omnigraph.dev | The metadata says `client_id_metadata_document_supported: true`, S256, scopes `offline_access` among four, and no `revocation_endpoint`. Both ports reach the sign-in page; the other path and `localhost` get `invalid_redirect_uri`, so the document is read. With no resource indicators configured a `resource` is accepted and, by WorkOS's documentation, ignored, so the token's audience is the environment's and Studio refuses it until the control plane registers the cluster's resource. The document is served 200 as `application/json`, its `client_id` its own URL |
| 2026-10-29 | A final whole-change review, and the custody redesign it led to | `graph_store` `sign_in::tests`: `a_sign_in_refused_for_good_revokes_a_token_nobody_holds`, `a_sign_in_under_way_keeps_saying_so_as_the_one_it_replaces_renews` (both fail without their fix, checked by removing it), `a_sign_in_made_during_a_sign_out_reaches_the_keychain_after_it` (rewritten: Cancel refused while finishing; a sign-in dropped while finishing revokes its token), `a_sign_in_dropped_during_a_refresh_is_decided_after_it` (the refresh writes nothing once the drop began); `studio` `a_server_that_offers_a_sign_in_says_where_and_as_whom` (the signed-in line by custody) | Pass. The review found a design grown by accretion: custody held in three places (the sign-in state, the source, what was left), four shapes of ending, the delete-to-custody mapping written three times, a generation counter beside the queue. And five defects it let through: a Cancel after the browser answered revoked the sign-in still in use; a sign-in refused for good was dropped unrevoked; a sign-out could delete the item of the sign-in replacing it; a dropped sign-in's refresh wrote outside the queue; and a replacement held the queue through a network revocation. Now one ending (`OAuthSource::end`), one mapping (`forgotten`), custody read from the connection only (`Connection::keychain`), every ending in a keychain turn, no counter, a Finishing state, and revocations in the background |
| 2026-10-29 | An independent review of the custody redesign | `graph_store` `sign_in::tests::a_sign_in_dropped_during_a_refresh_is_decided_after_it` (both keychains: one that takes the rotated token keeps it for the next launch, unrevoked; one that refuses it forgets the token from before and revokes the rotated one), `a_sign_in_as_someone_else_ends_with_a_cancel_or_a_sign_out` | Pass. Taken: the redesign stopped a refresh at once on a drop, so its rotated token was revoked and the keychain kept the spent one, the next launch signed out where main signed in (a regression); Cancel of a sign-in as someone else said the person signed out with the sign-in still held (as on main); the sign-in replaced answering that it ended overwrote Finishing; an ending finishing with a sign-in held said what was left. Also: a sign-in ends once, a second ending answering with the first's; `Connection::keychain` is a `Kept`, not an `Option` with two spellings of nothing |
| 2026-10-29 | A second independent review of the custody redesign | `graph_store` `sign_in::tests::a_sign_in_refused_while_another_is_under_way_is_replaced` (fails without its fix, checked by removing it) | Pass. Taken: a sign-in held that the service refused for good while another was under way ended as dropped and kept its token, which the new sign-in's item then replaced, leaving a token the service honours with nobody (main revoked it); it ends as replaced now, and, touching no keychain item, takes no keychain turn, which also removes a deadlock no path reached; a failed sign-in as someone else hid Sign out while a sign-in was still held (as on main), now offered whenever one is held. Not taken: a drop for the CLI's credential during Finishing answers with the replacement's ending, so the status says the keychain holds nothing while it holds a spent item; nothing usable is left, the next launch is refused once and forgets it, as on main |
| 2026-10-29 | A narrow review of that change | `graph_store` `sign_in::tests::an_item_left_stays_said_when_the_sign_in_replacing_it_is_cancelled` | Pass. It found the replacement and the shared ending sound, and the Sign out button offered only where signing out is right. Taken: a sign-in held over an item the keychain would not delete, ended as replaced while another was under way, dropped what it said of that item, so a sign-in under way then cancelled left it unsaid; the item is now kept as what is left until a sign-in is taken |
| 2026-10-29 | The item left open by the second review is closed | `graph_store` `sign_in::tests::a_sign_in_dropped_while_it_finishes_is_kept_then_dropped` (fails without its fix, checked by removing it), `a_sign_in_made_during_a_sign_out_reaches_the_keychain_after_it` (changed: one dropped while finishing is kept, unrevoked) | Pass. A drop (the CLI's credential, a retired connection) or a sign-out still cancelled a sign-in finishing, which a Cancel could not: the one it replaced was already revoked, so the keychain kept that account's item, spent where the service revokes and signed in with at the next launch where it does not, breaking one account per target. A sign-in finishing is now kept whatever ends it, then ends as one held would, and no other sign-in replaces it until then |
| 2026-10-29 | A third independent review, of the whole change | `graph_store` `sign_in::tests`: `a_sign_in_cancelled_during_its_exchange_revokes_its_token`, `tokens_studio_may_not_use_have_their_refresh_token_revoked`, `a_sign_out_deletes_at_once_over_a_refresh_in_flight`, `a_connection_reads_the_keychain_after_another_signs_out`, `an_item_left_stays_said_when_the_sign_in_replacing_it_is_cancelled` (extended: the reason), each failing without its fix, checked by removing it; `studio` `a_server_that_offers_a_sign_in_says_where_and_as_whom` (the Finishing line) | Pass. It found no defect in the custody paths. Taken, the first three also on main: a sign-in cancelled, signed out or dropped while the service answered its code exchange dropped the request after the code was redeemed, leaving its refresh token with nobody; the exchange runs on its own now, and an answer nobody takes is revoked. Tokens Studio refuses were dropped with their refresh token, at a sign-in and at a refresh that rotated the kept one; the refusal carries it now, and it is revoked. A sign-out's delete waited on a refresh in flight, up to 20 seconds, so quitting then let the next launch sign in; it deletes at once, and again once the refresh settles. Keychain turns were each connection's, so one made for a target while another's ending ran could overtake it; they are the item's, shared by every connection. A held sign-in refused while another was in the browser lost its reason at Cancel; Sign out was missing for an offered sign-in with one held after the connection became ready. Also: what a sign-in that ended left is a `Kept` (one mapping, and a drop that keeps its token says so), the sign-in under way carries its finishing, one revocation helper, stale comments. Not taken: a connection released while a sign-in finishes stops it, its token revoked; nothing leaks |
| 2026-10-29 | A fourth independent review, of the third's fixes | `graph_store` `sign_in::tests`: `a_replaced_sign_in_waits_for_no_refresh_and_leaves_the_item_after` (fails without either of its two fixes), `a_sign_out_during_a_refresh_write_leaves_nothing`, `tokens_studio_may_not_use_have_their_refresh_token_revoked` (extended: the spent token forgotten), `a_sign_in_dropped_while_it_finishes_is_kept_then_dropped` (extended: the target made anonymous again), each failing without its fix, checked by removing it | Pass. It found no defect that leaks a refresh token or loses a sign-out in the paths most likely to run. Taken: a refresh answered `invalid_grant` after its sign-in ended deleted the item outside any turn, which by then could be the sign-in replacing it, and a replacement waited for the replaced sign-in's refresh in flight with the keychain's turn held, up to 20 seconds. Both came from a refresh holding its lock across the request: each keychain operation of a refresh now begins only while the sign-in stands, under a lock its ending takes, so an ending waits for one in progress, never for the service, and the sign-out's second delete is gone; only a drop waits for the refresh, to keep its rotated token, and a sign-in under way waits for the ending of the one it replaces, held or ended meanwhile. An unusable answer revoked the token held when the service did not rotate it; it revokes only a rotated one now, and forgets the token that rotation spent. `TokenError`'s `Debug` would have printed that refresh token; it says why only. A finishing sign-in dropped as the target was made anonymous again left no sign-in offered until a restart; once kept it is offered, and signed in with. Under an offered sign-in with one held, the label says Not admitted and the status that the person stays signed in, and Retry is no longer displaced. Stale doc comments corrected |
| 2026-10-29 | A fifth independent review, of the refresh and ending locks | `graph_store` `sign_in::tests::a_drop_decides_by_what_the_refresh_in_flight_learns` (fails without either of its two fixes, checked by removing it) | Pass. It found no deadlock, no keychain operation of a refresh after its ending decided the item or after the replacing sign-in's write, and no wait on the service for a sign-out or a replacement. Taken: a drop during a refresh's 5-second retry pause waited it out and then skipped the retry, keeping the token from before where main kept the retry's rotation; the retry runs whether or not the sign-in ended. A drop during a refresh that answered `invalid_grant`, or rotated the token with tokens Studio refuses, kept the spent item as the next launch's sign-in; the refresh now tells the ending the item is spent, and the drop forgets it. An ask answered after the ending began could be handed the access token; it is told the sign-in ended. Not taken, both also on main: while a sign-in finishes, the one it replaces still answers asks, with "Signed out.", so reads in that moment go without a token until the new one is taken; and the keychain key is the target's name when the sign-in is made, so a CLI target renamed while signed in leaves its item under the old name, a separate change |
| 2026-10-29 | A narrow review of the fifth's fixes | `graph_store` `sign_in::tests::a_drop_decides_by_what_the_refresh_in_flight_learns` (a third case: a rotation with tokens Studio refuses; the service's first answer held until the drop has begun, so the retry case no longer depends on timing: 40 runs, none failing; still fails without either fix) | Pass. It found no defect in the retry, the `Stale` hand-off or the late ask, and traced what the keychain holds and which token is revoked for every ending against every refresh outcome. Taken: the retry case of the test could fail by timing alone (the fake answered the first request at once, and a stalled test thread let the retry fail too); no test drove an unusable rotation after the ending; two doc comments did not say a drop forgets an item the refresh found spent |
| 2026-10-29 | The two items the fifth review left open are closed | `graph_store` `sign_in::tests::a_renamed_target_is_signed_in_to_under_its_new_name`, `the_token_of_a_sign_in_being_replaced_stands_while_the_new_one_finishes` (each fails without its fix, checked by removing it) | Pass. The keychain key is the target's name when a sign-in is made, and the registry renamed a target in place, keeping its connection and sign-in: refreshes wrote under the old name while the next launch read the new one. A first fix moved the item on a rename; an independent review of it found the move raced the other actors on the item (a drop's kept item stayed under the old key, unrevoked and then lost to the next move; two movers could delete each other's work; a second rename read a key a pending move had not changed), so the design changed instead: a rename is the old target gone and a new one at the same address, the sign-in dropped as for a target the CLI no longer lists and a sign-in offered under the new name, and an ending says what it left only for the item of the target's name now. That review also asked the finishing arm to keep a token only as the may-pass arm does (a token held, from the source, unexpired, not just refused); it does. While a sign-in finished, the one it replaced answered that it ended, and the connection dropped its token for that moment; the token now stands until the new one is taken |
| 2026-10-30 | A review of the simplified rename | `graph_store` `sign_in::tests`: `a_renamed_target_is_signed_in_to_under_its_new_name` (a refresh held across the rename, so the old sign-in's ending reports after the new name's offer; fails without `Connection::names`), `a_rename_while_a_sign_in_finishes_offers_one_under_the_new_name`, `a_sign_in_made_during_a_sign_out_reaches_the_keychain_after_it` (extended: a window waits for the CLI's answer), each failing without its fix, checked by removing it; 20 runs of each, none failing | Pass. It found no defect that leaks a token, records an item from another name, or leaves the connection without an offer under the new name. Taken: the rename test's assertion that nothing is said of the old name's item could pass by timing alone; while a sign-in a rename or drop ended was being kept, the connection read as asking for a token and showed the token field; a new token source left windows reading with what the old one's answers left (the rename cleared it to none); doc comments for the drop, `End::Dropped` and the ending, and the CLI targets' doc, which sat above another function, are corrected. By the rule, a rename of a target signed in leaves its item under the old name, as a target the CLI no longer lists does |
| 2026-10-30 | A final independent review of the whole change, committed | `graph_store` `sign_in::tests`: `a_sign_in_over_an_item_left_revokes_its_token`, `a_sign_out_while_a_sign_in_finishes_offers_one_once_it_is_kept`, `a_failed_sign_in_says_what_became_of_the_one_held` (fails without either of its two fixes), `a_busy_token_endpoint_is_asked_again`, `an_ask_joins_the_refresh_under_way`, each failing without its fix, checked by removing it; 20 runs of each, and of the two timing fixes below, none failing | Pass. It found the custody core sound: no deadlock, no keychain operation overtaking another, no refresh writing after its sign-in ended. Taken, the first two also on main: a sign-in refused for good was dropped with its item kept for the next launch, and a sign-in made after wrote over that item, leaving its token with nobody, unrevoked (an earlier item a sign-in writes over or forgets now has its token revoked); a busy token endpoint (429) was a refusal for good and ended the sign-in; asks could join a refresh under way only in the moment it began, so after a failure each began another, with its own retry (the refresh under way is now joined without waiting for the tokens it holds); a failed sign-in as someone else read Not admitted when the one held was not answering, and lost why the one held ended meanwhile; a sign-out while one finished offered a Sign in that did nothing; Sign out was missing for an offered sign-in with one held while unreachable; a spent token was revoked later for nothing. Also: the item-left sentence, doc comments for `Finishing` and `ended`, a test setter in the registry that nothing called under another setter's doc, the keychain key and Studio's client identity written once in the tests, and four tests whose checks depended on timing |
| 2026-10-30 | A review of the final review's fixes (43d4237) | `graph_store` `sign_in::tests`: `an_earlier_token_is_revoked_once_the_item_no_longer_holds_it` (new), `a_sign_in_under_way_keeps_saying_so_as_the_one_it_replaces_renews` (waits for the renewal taken, by the lease it replaces), `a_busy_token_endpoint_is_asked_again` (each of 429, `slow_down`, `temporarily_unavailable`), `tokens_studio_may_not_use_have_their_refresh_token_revoked` (the spent token is not revoked), each failing without its fix, checked by removing it; 20 runs of each, none failing | Pass. It found no defect that loses a token, deadlocks, hands out an ended sign-in's token or begins two refreshes at once. Taken: an ask that read no refresh under way just before one began could wait through it and, after its failure, begin another (a sign-in's refreshes now count the ones ended and keep the last failure, which such an ask takes); an ask that joined a refresh read its token from the tokens held after, so a later refresh's failure could end the sign-in (the answer now carries the token); an earlier item's token the keychain would not let go of at the sign-in was dropped unrevoked once a rotation, a sign-out or an ended grant changed the item (the sign-in keeps it, and revokes it then); the earlier token was revoked through the client the sign-in began with, not the one in use; Sign out had taken Cancel's place while a kept sign-in first connected; three tests could pass by timing alone, and two answer kinds and the spent token's revocation were not tested |
| 2026-10-30 | A review of 791093d | `graph_store` `sign_in::tests`: `a_connection_released_while_a_sign_in_finishes_lets_it_finish` (new; the connection's release asserted before the write lands), `an_ask_joins_the_refresh_under_way` (extended: a refusal while a refresh is under way, which only a joining ask takes the refresh's token for), each failing without its fix, checked by removing it; `an_earlier_token_is_revoked_once_the_item_no_longer_holds_it` (waits for the earlier sign-in refused), the renewal test (waits for a lease, not its absence); 20 runs of each, none failing | Pass. It found the refresh accounting and the kept earlier token sound: no ask hangs, takes an ended token or an old failure over a fresh token, and no two refreshes run at once. Taken: a released connection dropped its sign-in task, cutting a finishing sign-in's keychain work off mid-write, so an earlier item it wrote over had its token left with nobody (the keychain work now runs on its own to its end, revoking through the client in use when it began finishing); the join test no longer told joining from waiting; while connecting, which of Cancel and Sign out showed depended on the sign-in state (both show while a sign-in is held); two waits in tests could be met too early |
| 2026-10-30 | A review of ddddd54 | `graph_store` `sign_in::tests::a_connection_released_while_a_sign_in_finishes_lets_it_finish` (changed: the sign-in kept for the target's return, not revoked, and a later connection takes its turn at the item and signs in with it; fails without its fix, checked by removing it); 20 runs, none failing | Pass. It found no deadlock, no token left with nobody and no broken turn order in the detached keychain work. Taken: a sign-in finishing when its connection was released was revoked while the keychain kept it, though a window holding the same retired connection dropped it as told, keeping it; the release is now a drop as for a retired connection on whichever side finds the connection gone (`Finished::dropped`); the earlier token was revoked through the client captured when finishing began, which a transport change during the keychain wait made stale; the connection now revokes it through the client in use when it takes the sign-in, and only a released connection's work falls back to that one |
| 2026-10-30 | A review of e27d0a3 | `graph_store` `sign_in::tests`: `a_sign_out_while_a_sign_in_finishes_stands_when_the_connection_is_released` (new: a sign-out, then the retirement and release; the item deleted, both tokens revoked, and a later connection, its read held while the deletion is, offered a sign-in; fails without either fix, checked by removing each), `a_sign_out_while_a_sign_in_finishes_offers_one_once_it_is_kept` (extended: another connection to the target, waiting while the sign-in finishes, reads the item only after the sign-out's deletion; fails when the sign-out ends in a turn of its own, checked), `a_connection_released_while_a_sign_in_finishes_lets_it_finish` (changed: retired before the release, as the registry does; fails when the retirement's drop is not heard, checked); each 20 runs, none failing | Pass. Taken: a connection released after a sign-out while a sign-in finished dropped the sign-in, so the keychain kept the token the person signed out of; a sign-in taken after a sign-out, or dropped by a release, passed its item's keychain turn on before it ended, so other work could read the item between its write and the deletion; and a released connection revoked the new token through the client captured when the sign-in began, not the one it was finishing with. The sign-in finished is now a guard (`Finished`) holding the sign-in and its turn: taken, it ends in that turn as told; dropped untaken, by any path, it ends as the connection last told it through a shared cell, and passes the turn on only once that ending has dealt with the item. Both fallbacks of e27d0a3 are removed |

## Corrections

None yet.
