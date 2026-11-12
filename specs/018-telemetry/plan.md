# 018: Telemetry, with consent

Status: draft · 2026-10-29 · reports go to PostHog (decided 2026-10-29), US Cloud (decided 2026-11-11)

## Problem

Nobody who works on Studio learns that it crashed, which of its features people use, or how it
performs on their graphs. What reaches us is what a few people write in an issue. Studio sends
nothing today: spec 017 decided "Nothing identifying is sent" because "Studio has no telemetry",
and constitution I lets only `graph_client` and `studio_update` make requests.

This plan adds three reports, each off until the person says otherwise, and keeps Studio's
promise that nothing about a person's graph leaves their machine: crash reports, usage events
and performance summaries. Each is a fixed, typed set of fields that can hold no graph content,
the person can read every report before or after it is sent, and one setting or one environment
variable turns all of it off.

## Principles

- I: the engine is the authority, and one client owns every request. Telemetry uploads are a
  third kind of request that is not to an Omnigraph server. This plan amends I's "in practice"
  line to name `studio_telemetry` beside `studio_update` (a patch: 1.2.2 to 1.2.3).
- VI: every control explains itself. Each switch says what it sends, and Studio can show every
  report it holds or sent.
- VII: loudness matches cost. Consent is one status bar item until the person chooses, never a
  dialog at start; a crash is offered the same way at the next start. Uploads are silent, and a
  failed upload is logged, not shown.
- VIII: keyboard first. View Telemetry Log, Send Crash Report and the settings are actions.
- IX: work is bounded. The queue, the file on disk, a batch and the retries each have a cap.
  Recording an event is a push onto a bounded queue: no I/O in a frame.
- XII: evidence over assumption. The catalog is a typed list with a test that refuses free
  text; every claim below about Zed and Studio is checked.
- The logging rule (`crates/studio/src/logging.rs`): nothing Studio logs carries a token, query
  text or row data. Telemetry keeps a stricter rule: it carries nothing a person typed, named or
  stored, and nothing that addresses their servers.
- Spec 016: no token is written to logs, saved state or crash reports.

## What exists today (checked)

- Studio sends no telemetry. Spec 017's decision table: "Nothing identifying is sent | Zed's
  metrics and system IDs | Studio has no telemetry; a static file needs none". Constitution I,
  "In practice": one client crate owns every request to a server, and `studio_update` alone
  fetches Studio's own releases.
- Zed's `telemetry` crate is linked but sends nothing. Zed's `client`, `dap`, `editor`, `fs`,
  `remote` and `workspace` crates depend on it (`Cargo.lock`), and they record events with
  `telemetry::event!`. At Zed `v1.23.2`, `crates/telemetry/src/telemetry.rs`: `send_event` sends
  only once `init` has set `TELEMETRY_QUEUE`, and nothing in Studio calls `init` (no match for
  `telemetry` in Studio's sources). Zed's events are dropped where they are made.
- Panics are reported in one line. `crates/studio/src/logging.rs` installs Studio's panic hook
  before the log starts: the thread, the file and line, and the message with every quoted part
  replaced by `…`, cut to 300 characters, written to `logs/Studio.log` in the data directory
  (about 1 MB, with `Studio.old.log` beside it). Rust's own report, which prints the message
  whole, is not chained. Tests: `a_panic_message_loses_the_text_it_quotes`,
  `a_panic_reports_one_line_without_the_text_it_quotes`.
- Release builds unwind on panic (`[profile.release]` sets no `panic`) and keep their symbols:
  the bundle `script/bundle-mac --fast` wrote holds 328,592 defined symbols (`nm -U`), 3,918 of
  them from `studio_workspace`. A backtrace taken in the panic hook names functions without
  debug files.
- An installed build is told from a development build by `studio_settings::is_release_build`
  (spec 017): a binary under a Cargo build directory, a test binary or a bundle in
  `target/bundle/` is not one.
- Studio-wide switches live under `studio` in the settings file: `studio.auto_update`
  (default `true`) and `studio.health_check_seconds` (`crates/studio_settings/src/studio_settings.rs`).
- The data directory is `studio_settings::data_dir()`, or `OMNIGRAPH_STUDIO_DATA` when set.
- The status bar already shows Studio's own items on its right (memory and CPU, the update item
  of spec 017); memory and CPU are sampled by `studio_workspace` only while the window is in
  front.
- Every frame builds the whole window's scene again (spec 002, validation 2026-10-29): about
  2.6 ms a frame over a 126-row table on a release build. Recording must not add work to a frame.

## Design

### Three reports, two switches

| Report | Setting | Default | What it is for |
|---|---|---|---|
| Crash reports | `studio.telemetry.crash_reports`: `"ask"`, `"always"` or `"never"` | `"ask"` | Finding and fixing crashes |
| Usage events and performance summaries | `studio.telemetry.usage`: `true` or `false` | `false` | Which features are used, and how fast Studio is on real graphs |

Nothing is sent, whatever the settings say, when:

- the build is not a release build (`is_release_build`);
- `OMNIGRAPH_STUDIO_TELEMETRY` is `0`, `off` or `false`, or `DO_NOT_TRACK` is set to anything but
  `0` or an empty value;
- the person has not chosen yet (usage), or has not said yes to this report (crash reports
  under `"ask"`).

When the environment turns telemetry off, Settings shows both switches disabled with "Turned off
by `DO_NOT_TRACK`" or "Turned off by `OMNIGRAPH_STUDIO_TELEMETRY`".

### What a report may hold

Every report is a value of one Rust enum, `studio_telemetry::Report`, and every field is one
of: an enum Studio defines, a `bool`, a bucket (below), Studio's version, or the name of one of
Studio's actions (`gpui::Action::name()`, a `&'static str` from Studio's code, never an
action's fields). There is no `String` field. A test walks the catalog's JSON schema
(`schemars`, already in the build) and fails on any string that is not one of those.

Never sent, in any report: a graph, type, property, branch, query or file name; a server
address or target name; an actor, account or token; query text, a value, a row or a count of
one graph's own rows beyond its bucket; an error message (only its class); a window title; the
log.

Buckets, so a number says how much and not which:

- Durations: under 10 ms, 30 ms, 100 ms, 300 ms, 1 s, 3 s, 10 s, 30 s, and longer.
- Counts (rows, tool calls, tabs): 0, 1, 2 to 10, 11 to 100, 101 to 1,000, 1,001 to 10,000, more.
- Memory: under 256 MB, 512 MB, 1 GB, 2 GB, 4 GB, and more.

Each report carries Studio's version, the OS (`macos`, `linux`, `windows`) and its major
version, and the architecture. Usage reports also carry the install ID and a session ID (below);
crash reports carry neither.

### Crash reports

- A panic: the hook writes the line it writes today, and in a release build also keeps a report
  in `telemetry/crashes/` in the data directory: that line, the backtrace's function names and
  offsets (`std::backtrace::Backtrace::force_capture`, frames outside Studio's and Zed's crates
  kept by their library name only), the thread's name, how long Studio had run (bucketed) and
  the version, OS and architecture. Paths in frames are made relative to the crate.
- A crash the hook cannot see (a signal, an abort in native code): at start, macOS's report for
  the last run (`~/Library/Logs/DiagnosticReports/omnigraph-studio-*.ips`, newer than the
  previous start Studio recorded) is read and cut down to its exception type, the crashed
  thread's frames (image and symbol) and the OS version. Every path under the home folder
  becomes `~`. Linux and Windows wait (Deferred).
- Under `"ask"`, the next start shows one status bar item, "Studio quit unexpectedly · Send
  report…". It opens a popover with the report as it would be sent, and **Send**, **Don't
  send** and **Always send** (which sets `"always"`). A report not answered in 7 days is deleted
  unsent. Under `"always"` reports are queued at start; under `"never"` they are deleted.

### Usage events

The first catalog. Each name is the event's tag in the enum; adding one is a code change with
its row in `docs/telemetry.md`.

| Event | Fields |
|---|---|
| `app_started` | first start of this version (`bool`), windows restored (count bucket) |
| `action` | action name, how it was invoked (`key`, `palette`, `menu`, `pointer`) |
| `graph_opened` | server class (`local` for a loopback address, `remote`, `managed`), contract match (`bool`) |
| `query_ran` | kind (`read`, `mutation`), outcome (`ok`, `refused`, `failed`, `cancelled`, `unknown`), duration bucket, rows bucket |
| `write_finished` | kind (`cell`, `bulk_set`, `drafts`, `insert`, `delete`, `blob`, `merge`, `schema`), outcome (`published`, `refused`, `unknown`), rows bucket |
| `connection_changed` | state (`ready`, `unauthorized`, `unreachable`, `contract_mismatch`), round trip bucket |
| `agent_turn_finished` | outcome (`done`, `stopped`, `failed`), duration bucket, tool calls bucket |
| `update` | step (`found`, `downloaded`, `installed`, `failed`) |

The install ID is a random UUID (version 4) written to `telemetry/install-id` when usage is
first turned on, deleted when it is turned off, and replaced by **Reset ID**. Nothing ties it to
an account, a machine or a server, including a managed sign-in. The session ID is a new random
UUID version 7 each start, kept in memory, and a new one after 24 hours of running: PostHog
counts a custom session ID only when it is a UUIDv7 whose time plus 24 hours is after the
session's last event.

### Performance summaries

While usage is on, Studio keeps histograms in memory, in the buckets above, of query durations
by outcome, health-check round trips, and memory, and sends them as one `daily_summary` event a
day (or at quit, when the day's has not gone): counts per bucket, the peak memory bucket, the
number of panics and of native crashes seen, and how long Studio ran. Frame times join the
summary once it is checked what GPUI exposes about a frame (Open questions).

### Recording, storing and sending

- `studio_telemetry::record(report)` checks the switches (read once and on every settings
  change, an atomic in memory) and pushes onto a bounded channel: 1,000 reports, the oldest
  dropped first and the drops counted into the next summary. It does no I/O and takes no lock
  a frame waits on.
- A background task writes the queue to `telemetry/pending.jsonl` every 60 s and at quit (at
  most 1 MB; older lines go first), and sends batches of at most 200 reports or 256 KB once an
  hour while anything is pending. A batch is deleted when the endpoint answers 2xx (which says it was received, not that every
  event was valid: a test checks each report against the fields PostHog requires); on a timeout,
  a 429 or a 5xx it is retried after 1 h, doubling to at most 24 h; any other 4xx (a body PostHog
  refuses, a wrong token, a billing quota) deletes it, since sending it again cannot succeed. A
  report older than 7 days is deleted unsent. PostHog's capture endpoints have no request rate
  limit, and a batch may be up to 20 MB and an event 1 MB, far above Studio's caps.
- Uploads go through Studio's one HTTP client (the global `graph_store::init` sets), with the
  user agent it already sends, to PostHog US Cloud's batch endpoint,
  `https://us.i.posthog.com/batch/`, gzipped (`Content-Encoding: gzip`). The body is PostHog's:
  the project token as `api_key`, the reports as `batch`, and `sent_at`, so PostHog corrects
  each `timestamp` for a client clock that is wrong. The host is compiled in with the token: an
  installed build keeps sending where it was built to, so the region is fixed for good (moving a
  project between regions is a migration PostHog does only on its top plans). The token is compiled into release builds; a
  development build has none. The request carries no cookies and no credential of the
  person's.
- A usage report is one PostHog event: its tag as `event`, the install ID as `distinct_id`, its
  fields as `properties` with `$process_person_profile: false` (PostHog's anonymous events: no
  person profile is made or updated), `$geoip_disable: true` and `$session_id`, and the time
  it was recorded as `timestamp`. A daily summary is the event `daily_summary`.
- A crash report is a `$exception` event, the shape PostHog's error tracking reads: the panic's
  line (or the signal) as the exception's `value`, its kind (`panic`, or the signal's name) as
  `type`, which PostHog groups issues by, `$exception_level: "fatal"`, `mechanism.handled:
  false`, `$geoip_disable: true`, and the frames as
  a raw stack trace (`platform: "custom"`, `lang: "rust"`, `function`, `in_app` for Studio's
  crates). Its `distinct_id` is a new random UUID for that report alone, with
  `$process_person_profile: false`, so a crash report is tied to no install.
- PostHog's project for Studio, on US Cloud, is set up by hand, since a US organization's
  projects start the other way round: **Discard client IP data** on (US projects capture it until
  it is set, and changing the organization's default later leaves existing projects as they
  are), and the default GeoIP enrichment transformation off. GeoIP runs before the IP is
  dropped, so discarding the IP alone still stores a location; each report's
  `$geoip_disable: true` keeps it off if the project setting is ever changed. PostHog still
  receives the request's IP and removes it before storing the event; the privacy notice says so
  (Open question 3). Transformations do not run on error tracking's exceptions, so whether the IP
  setting covers `$exception` events is checked on a test project before crash reports ship
  (Open question 7).
- Turning a switch off deletes everything pending for that report at once, and turning usage
  off deletes the install ID. Events already sent carry no person profile, so PostHog's
  person deletion does not reach them; a person who asks for their data to be erased gives their
  install ID (shown in Settings), and its events are removed with PostHog's event deletion
  (a HogQL deletion request, run in PostHog's weekly batch).
- Zed's telemetry queue is never started: Studio does not call `telemetry::init`, and a source
  guard test refuses one.

### Seeing what is sent

- **View Telemetry Log** (an action, in the palette and in Settings) opens a tab listing every
  report held and every report sent in the last 7 days, newest first, as the JSON that goes, with
  when it went or why it waits.
- `docs/telemetry.md` lists every report, field, value and bucket. A test generates it from the
  catalog and fails when the checked-in file differs.

### What the person sees

- First start of a release build with no `studio.telemetry.usage` in the settings: one status bar
  item, "Usage data: not shared · Choose…". It opens a popover: "Send anonymous usage data to
  help improve Studio?", a line on what that is and is not ("Which features you use and how fast
  they are; never names, queries, values or server addresses"), **Share**, **Don't share** and
  **What Studio sends** (`docs/telemetry.md`). The item goes once either is chosen; until then
  nothing is recorded.
- Settings, "Privacy": the crash reports choice (Ask each time, Always send, Never send), Share
  usage data, Reset ID, View Telemetry Log, What Studio sends.

### Where it lives

A new crate, `studio_telemetry`: the catalog, the buckets, the queue, the file, the uploader and
the crash report builders. It depends on `studio_settings` and on `http_client` for the global
client; feature crates depend on it only to call `record`. The panic hook in
`crates/studio/src/logging.rs` calls into it to keep a crash report.

### Limits

| What | Limit |
|---|---|
| Reports in memory | 1,000; oldest dropped, drops counted |
| Pending file | 1 MB; oldest lines dropped |
| A batch | 200 reports or 256 KB |
| Sending | once an hour; retries from 1 h doubling to 24 h |
| A report kept unsent | 7 days |
| A crash report's backtrace | 64 frames |
| A request | 15 s |

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| A closed, typed catalog with no free text | Zed's `FlexibleEvent` (a name and any JSON); an analytics SDK's `capture(name, properties)` | Free text is how a branch name or an error message carrying a value leaks; a type that cannot hold one cannot leak one, and a test can prove it |
| Usage off until chosen; crash reports asked for each time | Usage on unless turned off | Studio shows private graphs to people who never agreed to share anything; asking once costs one status bar item |
| One status bar item, not a dialog at first start | A first-run dialog | Constitution VII: a start that asks a question before showing anything is loud for a choice that can wait |
| PostHog for usage, summaries and crash reports | A crash tracker beside it (Sentry); a store of Studio's own | Chosen 2026-10-29. One API for all three: events and `$exception` events through the same capture endpoint; anonymous events need no person profile; client IP data discarded by a project setting |
| PostHog US Cloud (`us.i.posthog.com`) | EU Cloud; self-hosted | Chosen 2026-11-11. ModernRelay and most of Studio's people are in the US; the reports hold no graph content, no IP and no person profile, so the gap EU hosting would close is small. PostHog takes part in the EU-U.S. Data Privacy Framework and its DPA adds Standard Contractual Clauses, so EU users are covered; the region cannot be changed later without a migration |
| Studio's own uploader through its one HTTP client, speaking PostHog's capture API | PostHog's or Sentry's Rust SDK | An SDK brings its own HTTP stack (`posthog-rs` is built on `reqwest`) and would be a second client making requests, and its `capture` takes any properties, where the catalog takes none; the catalog and queue are small, and the capture API is a JSON body |
| A random install ID the person can reset | A hardware or system ID; none | Without an ID retention cannot be counted; a hardware ID follows a person across resets and installs |
| Bucketed numbers | Exact durations and counts | A bucket says enough to rank and compare; an exact row count can identify a graph |
| Performance as a daily summary of histograms | One event per query or frame | A summary sends a few hundred bytes a day and nothing about when someone worked |
| Panics from the hook now; native crashes from macOS's own report | Crashpad or Breakpad minidumps | macOS already writes the report; minidumps need a C++ toolchain and a symbol server, and hold memory |
| Never start Zed's telemetry queue | Forward Zed's events | Zed's events are Zed's, with free-form properties |
| Nothing sent from development builds | Send, marked as development | Development builds are run by people working on Studio, against test data |

## Deferred

- Native crash reports on Linux and Windows.
- A machine-wide policy that turns telemetry off for every person on a machine, with spec 017's
  for updates.
- Frame-time histograms (Open question 5).
- Usage measured by servers (a managed control plane's own metrics) is not Studio's and not this
  plan's.
- No Omnigraph engine change is needed; `docs/deferred-engine-changes.md` is unchanged.

## Open questions

1. ~~Where reports go.~~ PostHog, for all three (2026-10-29), on US Cloud (2026-11-11). Still
   open: which organization holds the project, and who can read it.
2. How long PostHog keeps raw events. The Free plan keeps them a year and cannot shorten it;
   paid plans keep two years by default and can be set, in months, to less (a proposal: 3
   months). Exceptions are kept as long. Free covers 1 million events and 100,000 exceptions a
   month, and one project.
3. The privacy notice the first popover and `docs/telemetry.md` link to, and its legal review: a
   resettable install ID is still pseudonymous personal data under the GDPR.
4. Whether an organization signed in to a managed cluster may turn usage off for its people.
5. What GPUI exposes about a frame's time at Zed `v1.23.2`, to time frames without adding work to
   one. `script/measure-frames` reads the main thread's CPU time per frame, but by driving the
   frames itself from the snapshot tool (`docs/drawing.md`); an installed Studio's frames are
   driven by the display.
6. Whether release builds should also archive their debug files (`dSYM`) for line numbers in
   crash reports, beyond the function names the binary keeps. PostHog symbolicates Rust only
   from the frame format `posthog-rs` 0.16+ sends (instruction addresses and a build ID, not
   `platform: "custom"`), with `debug = "line-tables-only"`, `split-debuginfo = "packed"` and
   the `.dSYM` uploaded by `posthog-cli` 0.8.1+; the frame fields are documented only in the
   `posthog-rs` repository.
7. Whether the project's **Discard client IP data** also covers `$exception` events, which skip
   transformations. Checked on a test project before crash reports are turned on.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-29 | Studio sends no telemetry, and only two crates make requests | Spec 017's decision table; constitution 1.2.2, I, "In practice" | "Studio has no telemetry"; `graph_client` and `studio_update` |
| 2026-10-29 | Zed's telemetry crate is linked and drops every event | `Cargo.lock` (packages depending on `telemetry`); Zed `v1.23.2` `crates/telemetry/src/telemetry.rs`; a search of Studio's `crates/` for `telemetry` | Depended on by `client`, `dap`, `editor`, `fs`, `remote`, `workspace`; `send_event` sends only once `init` set the queue; no match in Studio |
| 2026-10-29 | Panics are written as one line with quoted text removed | `crates/studio/src/logging.rs` (`init`, `report_panic`, `panic_line`, `without_quotes`) and its two tests | As described; Rust's report not chained |
| 2026-10-29 | Release builds unwind and keep symbol names | `Cargo.toml` `[profile.release]` and `[profile.release-fast]`; `nm -U` on the bundle `script/bundle-mac --fast` built from `09e459b` | No `panic` setting (unwind); 328,592 defined symbols, 3,918 from `studio_workspace` |
| 2026-10-29 | PostHog takes the reports as designed (two claims corrected 2026-11-11) | PostHog's documentation: "Capture and batch API endpoints" (`/docs/api/capture`), "Data collection" (`/docs/privacy/data-collection`), "GeoIP" (`/docs/cdp/geoip-enrichment`), error tracking's manual capture (`/docs/error-tracking/installation/manual`) | `/i/v0/e` and `/batch/` on `us.i.posthog.com`, `eu.i.posthog.com` or a self-hosted domain; every event needs the project token, `distinct_id` and `event`; `$process_person_profile: false` with a random ID makes an anonymous event; IP data capture is set per organization and project, and EU organizations start with it off; GeoIP is a transformation a project adds; `$exception` with `$exception_list` takes raw frames with `platform: "custom"`, `lang` and `function` |
| 2026-10-29 | An installed build is told from a development build | `studio_settings::is_release_build` and its test `only_an_installed_build_is_a_release_build` | A build directory, a test binary or a bundle under `target/` is not a release build |
| 2026-11-11 | PostHog US Cloud takes the reports as designed, and the 2026-10-29 row holds for it | PostHog's documentation read whole for US Cloud (the `.md` of each page): capture (`/docs/api/capture`), API (`/docs/api`), anonymous events, sessions (`/docs/data/sessions`), timestamps, ingestion warnings, data collection, data storage, GDPR compliance (`/docs/privacy/gdpr-compliance`), transformations, events retention, projects settings, error tracking (manual capture, fingerprints, Rust), the Rust SDK reference, pricing, the DPA | `/batch/` on `us.i.posthog.com`, body under 20 MB, events under 1 MB, gzip; no request rate limit, 200 does not mean ingested; a US organization's projects capture IP until set by hand; GeoIP enrichment is on by default and survives discarding the IP; transformations skip exceptions; a custom session ID must be a UUIDv7; Free keeps events 1 year, unchangeable; moving regions is a migration on top plans only. Three claims corrected (Corrections) |

## Corrections

- 2026-11-11: "IP data capture … which an EU organization's projects start with" applied to EU
  Cloud; on US Cloud the project is set to discard client IP data by hand.
- 2026-11-11: "GeoIP is a transformation a project adds" was wrong: PostHog enriches events with
  GeoIP by default, before the IP is dropped. The project turns it off, and each report sets
  `$geoip_disable: true`.
- 2026-11-11: The session ID was a UUID version 4; PostHog counts a custom session ID only when it
  is a UUIDv7, so it is one, renewed after 24 hours.
- 2026-11-11: "Anything else is retried" would have retried a refused batch for 7 days; only a
  timeout, a 429 or a 5xx is retried now.
- 2026-11-11: The SDK was rejected partly because "SDKs gather device and environment data by
  default"; PostHog's Rust SDK documents no such default, so the reason now rests on its own HTTP
  stack and its free-form properties.
