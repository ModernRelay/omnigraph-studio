# What Omnigraph Studio sends

Nothing. From version 0.4.7, Studio sends no usage data, no crash reports and no ID of you, your computer or your installation. The code that made and sent those reports is removed, and so are its settings. Starting 0.4.7 or later also deletes what an earlier version kept on your computer: the reports not yet sent, the list of those sent and its crash reports. An install ID, if you chose one, stays in its file so that you can still quote it to us; the [privacy notice](privacy.md) says where it is.

Studio still makes the requests its work needs: to the Omnigraph servers you connect it to, and to GitHub to check for a newer version of itself. The privacy notice covers those too.

## The builds that sent reports

Three builds of Studio had telemetry, all made between 12 and 14 November 2026. They sent reports to PostHog US Cloud, which was told to discard the IP address a request came from and to add no location.

| Build | Usage data | Crash reports |
|---|---|---|
| 0.4.5, released 13 November 2026 | Sent from the first start, unless turned off. The status bar said "Anonymous usage data is on" and did not wait for an answer | Shown to you, and sent only when you chose Send or Always send |
| The edge build of 13 November 2026 | As 0.4.5 | As 0.4.5 |
| The edge build of 14 November 2026 | Asked first, and sent none until you answered | As 0.4.5 |

No other version, before or after, sends any of this. Nothing was sent from a development build, or while `DO_NOT_TRACK` or `OMNIGRAPH_STUDIO_TELEMETRY=0` was set.

If you run one of these builds, update to 0.4.7 or later. To stop its reports without updating, open the status bar's notice and turn usage data off, or set usage data to off and crash reports to never under Settings, Privacy.

The rest of this page is what those builds could send, kept for anyone who ran them. No report held a graph, type, property, branch, query or file name, a server address, an account or token, query text, a value, a row, an error message or your IP address.

## Sent with every report

| Property | Value |
|---|---|
| `version` | Studio's version |
| `os` | `macos`, `linux` or `windows` |
| `os_version` | the system's major version, or `unknown` |
| `arch` | `aarch64` or `x86_64` |

Usage events went under a random ID made at each start and never written down, or, if you chose to be counted across runs, under a random install ID. They also carried a random session ID. Crash reports carried neither: each had a random ID of its own.

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
| `kind` | `mutation`, `stored_query`, `blob`, `branch`, `merge` |
| `outcome` | `published`, `held`, `refused`, `unknown` |

Until 14 November 2026 this page listed only `mutation`, `stored_query` and `branch` as kinds and only `held` as an outcome. The builds could send every value above.

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

Once a day while usage data was on: how often things took how long, and what went wrong how often. Each histogram was a list of counts, one per duration bucket in this order: under 10 ms, 30 ms, 100 ms, 300 ms, 1 s, 3 s, 10 s, 30 s, and 30 s or more.

| Property | Values |
|---|---|
| `queries_ok` | Query durations that succeeded, one count per duration bucket in order. (a list of counts) |
| `queries_failed` | Query durations that failed or were refused, per duration bucket. (a list of counts) |
| `round_trips` | Health-check round trips, per duration bucket. (a list of counts) |
| `peak_memory` | The most memory Studio used while its window was in front. (`under_256mb`, `under_512mb`, `under_1gb`, `under_2gb`, `under_4gb`, `4gb_or_more`, or none) |
| `panics` | Panics Studio caught. (a count) |
| `native_crashes` | Crashes macOS reported for the previous run. (a count) |
| `dropped` | Reports dropped because the queue was full. (a count) |
| `ran` | How long Studio had run since its start. (`under_1m`, `under_10m`, `under_1h`, `under_4h`, `under_12h`, `12h_or_more`, or none) |

## Crash reports

A `$exception` event, the shape PostHog's error tracking reads: the kind (`panic` or the signal's name), the panic's one line with every quoted part replaced by `…` and paths under your home folder shortened to `~`, the names of the functions on the crashed thread (at most 64), the thread's name and how long Studio had run, as a range. A crash report was shown to you before it was sent, unless you chose to always send them.
