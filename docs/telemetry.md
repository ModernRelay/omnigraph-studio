<!-- Generated from crates/studio_telemetry/src/catalog.rs by its test `docs_telemetry_md_matches_the_catalog`. Do not edit by hand. -->

# What Omnigraph Studio sends

Studio sends anonymous usage data unless you turn it off (spec 018): which features are used and how fast they are, tied to one run of Studio and nothing else. You can choose to be counted across runs with a random install ID. Crash reports are shown to you and sent only when you say so. Nothing is sent from a development build, or while `DO_NOT_TRACK` or `OMNIGRAPH_STUDIO_TELEMETRY=0` is set. Change either choice under Settings, Privacy, or from the status bar's notice.

No report holds a graph, type, property, branch, query or file name, a server address, an account or token, query text, a value, a row, an error message or your IP address. Reports go to PostHog US Cloud, which is told to discard the IP address a request comes from and to add no location.

Who receives this data, how long it is kept and your rights over it are in the [privacy notice](privacy.md).

## Sent with every report

| Property | Value |
|---|---|
| `version` | Studio's version |
| `os` | `macos`, `linux` or `windows` |
| `os_version` | the system's major version, or `unknown` |
| `arch` | `aarch64` or `x86_64` |

Usage events go under a random ID made at each start and never written down, or, if you chose to be counted across runs, under a random install ID, deleted when you turn that off and replaced by Reset ID. They also carry a random session ID. Crash reports carry neither: each has a random ID of its own.

## Usage events

### `app_started`

Studio started.

| Property | Values |
|---|---|
| `first_start_of_version` | `true` or `false` |
| `windows_restored` | `0`, `1`, `2_to_10`, `11_to_100`, `101_to_1000`, `1001_to_10000`, `more_than_10000` |

### `graph_opened`

A graph was opened in a window.

| Property | Values |
|---|---|
| `server` | `local`, `remote`, `managed` |
| `contract_matches` | `true` or `false` |

### `query_ran`

A query from the query editor finished.

| Property | Values |
|---|---|
| `kind` | `read`, `mutation` |
| `outcome` | `ok`, `refused`, `failed`, `cancelled`, `unknown` |
| `duration` | `under_10ms`, `under_30ms`, `under_100ms`, `under_300ms`, `under_1s`, `under_3s`, `under_10s`, `under_30s`, `30s_or_more` |
| `rows` | `0`, `1`, `2_to_10`, `11_to_100`, `101_to_1000`, `1001_to_10000`, `more_than_10000` |

### `write_finished`

A write finished.

| Property | Values |
|---|---|
| `kind` | `mutation`, `stored_query`, `branch` |
| `outcome` | `held` |

### `connection_changed`

A server's connection changed state.

| Property | Values |
|---|---|
| `state` | `ready`, `unauthorized`, `unreachable`, `contract_mismatch` |
| `round_trip` | `under_10ms`, `under_30ms`, `under_100ms`, `under_300ms`, `under_1s`, `under_3s`, `under_10s`, `under_30s`, `30s_or_more`, or none |

### `agent_turn_finished`

An agent's turn ended.

| Property | Values |
|---|---|
| `outcome` | `done`, `stopped`, `failed` |
| `duration` | `under_10ms`, `under_30ms`, `under_100ms`, `under_300ms`, `under_1s`, `under_3s`, `under_10s`, `under_30s`, `30s_or_more` |
| `tool_calls` | `0`, `1`, `2_to_10`, `11_to_100`, `101_to_1000`, `1001_to_10000`, `more_than_10000` |

### `update`

The updater moved a step.

| Property | Values |
|---|---|
| `step` | `found`, `downloaded`, `installed`, `failed` |

## `daily_summary`

Once a day while usage is on: how often things took how long, and what went wrong how often. Each histogram is a list of counts, one per duration bucket in this order: under 10 ms, 30 ms, 100 ms, 300 ms, 1 s, 3 s, 10 s, 30 s, and 30 s or more.

| Property | Values |
|---|---|
| `queries_ok` | Query durations that succeeded, one count per duration bucket in order. (a list of counts) |
| `queries_failed` | Query durations that failed or were refused, per duration bucket. (a list of counts) |
| `round_trips` | Health-check round trips, per duration bucket. (a list of counts) |
| `peak_memory` | The most memory Studio used while its window was in front. (`under_256mb`, `under_512mb`, `under_1gb`, `under_2gb`, `under_4gb`, `4gb_or_more`, or none) |
| `panics` | Panics Studio caught. (a count) |
| `native_crashes` | Crashes macOS reported for the previous run. (a count) |
| `dropped` | Reports dropped because the queue was full. (a count) |
| `ran` | How long Studio has run since its start. (`under_1m`, `under_10m`, `under_1h`, `under_4h`, `under_12h`, `12h_or_more`, or none) |

## Crash reports

A `$exception` event, the shape PostHog's error tracking reads: the kind (`panic` or the signal's name), the panic's one line with every quoted part replaced by `…` and paths under your home folder shortened to `~`, the names of the functions on the crashed thread (at most 64), the thread's name and how long Studio had run, as a range. A crash report is shown to you before it is sent, unless you chose to always send them.
