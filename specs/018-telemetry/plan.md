# 018: Telemetry, with consent

Status: draft · 2026-10-29

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
UUID each start, kept in memory.

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
  hour while anything is pending. A batch is deleted when the endpoint answers 2xx; anything else
  is retried after 1 h, doubling to at most 24 h. A report older than 7 days is deleted unsent.
- Uploads go through Studio's one HTTP client (the global `graph_store::init` sets), with the
  user agent it already sends, to one endpoint (Open questions 1 and 2) as a JSON array of
  reports. The request carries no cookies and no credential of the person's.
- Turning a switch off deletes everything pending for that report at once, and turning usage
  off deletes the install ID.
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
| Studio's own uploader through its one HTTP client | Sentry's or PostHog's Rust SDK | SDKs gather device and environment data by default, bring their own HTTP stack, and would be a second client making requests; the catalog and queue are small |
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

1. Where reports go, and who runs it: one endpoint under `omnigraph.dev` forwarding usage to
   PostHog (EU cloud or self-hosted) and crash reports to a crash tracker, or one store for both.
2. What the endpoint keeps: no IP address stored, and how long raw reports are kept (a proposal:
   90 days raw, aggregates kept).
3. The privacy notice the first popover and `docs/telemetry.md` link to, and its legal review: a
   resettable install ID is still pseudonymous personal data under the GDPR.
4. Whether an organization signed in to a managed cluster may turn usage off for its people.
5. What GPUI exposes about a frame's time at Zed `v1.23.2`, to time frames without adding work to
   one. `script/measure-frames` reads the main thread's CPU time per frame, but by driving the
   frames itself from the snapshot tool (`docs/drawing.md`); an installed Studio's frames are
   driven by the display.
6. Whether release builds should also archive their debug files (`dSYM`) for line numbers in
   crash reports, beyond the function names the binary keeps.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-29 | Studio sends no telemetry, and only two crates make requests | Spec 017's decision table; constitution 1.2.2, I, "In practice" | "Studio has no telemetry"; `graph_client` and `studio_update` |
| 2026-10-29 | Zed's telemetry crate is linked and drops every event | `Cargo.lock` (packages depending on `telemetry`); Zed `v1.23.2` `crates/telemetry/src/telemetry.rs`; a search of Studio's `crates/` for `telemetry` | Depended on by `client`, `dap`, `editor`, `fs`, `remote`, `workspace`; `send_event` sends only once `init` set the queue; no match in Studio |
| 2026-10-29 | Panics are written as one line with quoted text removed | `crates/studio/src/logging.rs` (`init`, `report_panic`, `panic_line`, `without_quotes`) and its two tests | As described; Rust's report not chained |
| 2026-10-29 | Release builds unwind and keep symbol names | `Cargo.toml` `[profile.release]` and `[profile.release-fast]`; `nm -U` on the bundle `script/bundle-mac --fast` built from `09e459b` | No `panic` setting (unwind); 328,592 defined symbols, 3,918 from `studio_workspace` |
| 2026-10-29 | An installed build is told from a development build | `studio_settings::is_release_build` and its test `only_an_installed_build_is_a_release_build` | A build directory, a test binary or a bundle under `target/` is not a release build |

## Corrections

None yet.
