# 016: Studio as an OAuth client

Status: implemented for a CLI server target with `auth: anonymous` whose server offers sign-in
(`oauth_client`, `graph_store::sign_in`, 2026-10-26), and tested end to end against Omnigraph
v0.14.0 (`script/dev-sign-in`). Studio has no client identity yet (open question 1): until its
metadata document is published or it is registered, a real sign-in service refuses it, and only
`OMNIGRAPH_STUDIO_OAUTH_CLIENT` (development) signs in. A managed target still reaches its
cluster on the CLI's login (spec 003, since 2026-10-25) until the managed control plane's
prerequisites below are met · 2026-10-26

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

- Studio's `client_id` is the URL of its Client ID Metadata Document, an HTTPS JSON document
  ModernRelay publishes at a stable address (open question 1), used with an authorization
  server whose metadata says `client_id_metadata_document_supported: true`:
  `{"client_id": <its URL>, "client_name": "Omnigraph Studio", "redirect_uris":
  ["http://127.0.0.1/callback"], "grant_types": ["authorization_code", "refresh_token"],
  "response_types": ["code"], "token_endpoint_auth_method": "none"}`. The loopback redirect is
  listed without a port; the authorization server must accept any port on it (RFC 8252 §7.3).
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
  never the old account's. A second account is a second CLI target.
- The access token stays in memory, per target, for at most its lifetime. Neither
  token is written to disk, logs, saved state, links, crash reports or any agent's environment;
  the agent gateway uses Studio's connection, and the agent never sees the token.
- A keychain that refuses the write leaves the sign-in good for this run and says so ("Signed
  in for this session; the keychain did not keep it"); there is no file fallback.
- **Sign out** drops the access token, deletes the keychain item and, when the authorization
  server publishes a `revocation_endpoint`, revokes the refresh token there (RFC 7009),
  bounded to 10 seconds and not retried.

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
  every view and window; others wait for its answer. A refresh runs on a task of its own to its
  end: an ask dropped mid-exchange (a connect replaced, a renewal overtaken) does not take it
  along, so a refresh token the service has rotated is never lost. These tokens are never mixed with a static token or the
  CLI's credentials.
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
    Metadata Document;
  - name each managed cluster's resource on its own data host, `https://<data host>/mcp`, and
    forward that path and its metadata through the data host's public route, which OAuth alone
    does not create. Done (2026-10-23): a client following RFC 9728 §3.3 (the MCP TypeScript
    SDK's `checkResourceAllowed` included) now finds the resource where it calls.

  Its authorization server is AuthKit. Studio must not depend on `/openapi.json` being
  reachable through every deployment's proxy.
- **ModernRelay**: publish Studio's Client ID Metadata Document at a stable HTTPS address.
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

1. Where Studio's Client ID Metadata Document is published, and whether the managed control
   plane's authorization server accepts it for Studio, or Studio is pre-registered there (and
   with which ID per environment). The managed control plane prefers metadata-document
   clients and keeps pre-registration for clients that need it.
2. Whether that authorization server accepts a loopback redirect on any port (RFC 8252 §7.3)
   for a public client.
3. Which scopes Studio requests so that a refresh token is issued (for example
   `offline_access`), and how long a refresh token lasts. That they rotate and must repeat the
   resource is settled.
4. How a person in several organizations gets a token for the target's organization: the
   hosted flow's organization choice, or a hint Studio sends.
5. What to show for the account: `sub` is an opaque identifier, and Studio requests no ID
   token.
6. What to do with metadata naming several authorization servers (none does today).
7. Whether the authorization server publishes a `revocation_endpoint`.

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

## Corrections

None yet.
