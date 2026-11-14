# Omnigraph Studio privacy notice

Last updated: 14 November 2026

This notice covers the Omnigraph Studio desktop app ("Studio"). It says what Studio sends to us,
which from version 0.4.7 is nothing; which requests Studio makes for itself; and what three
earlier builds sent, who holds it, how long it is kept, and what you can do about it.
[What Omnigraph Studio sends](telemetry.md) lists every report and field of those builds.

## Who we are

EQTR, Inc., which does business as Modern Relay ("we"), is the controller of the data described
here. To ask about it or to use your rights, write to
[privacy@modernrelay.com](mailto:privacy@modernrelay.com).

## Your graphs stay with you

Studio talks to the Omnigraph servers you connect it to. Your graphs, queries, values, names,
server addresses, accounts and tokens go only to those servers. They never come to us, and no
report from Studio ever contained them.

## What Studio sends to us

Nothing. From version 0.4.7, Studio sends no usage data, no crash reports and no ID of you, your
computer or your installation. The code that made and sent those reports is removed, and Studio
has no setting that turns any of them on.

## Requests Studio makes for itself

Studio checks GitHub for new versions of itself. GitHub receives those requests and your IP
address under [GitHub's privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).
We receive nothing from these checks. With **Update automatically** off in Settings, Studio
checks only when you ask it to.

## The builds that sent reports

Three builds of Studio sent reports to us: version 0.4.5, released on 13 November 2026, and the
pre-release ("edge") builds of 13 and 14 November 2026. No version before or after them sends
any. None of them sent anything while the `DO_NOT_TRACK` or `OMNIGRAPH_STUDIO_TELEMETRY=0`
environment variable was set.

**Usage data.** It included the features you used (for example, "a query ran" or "a write
finished") and their outcomes, durations and sizes. Numbers were rounded into ranges, so a
report said "11 to 100 rows", never the exact count. A daily summary added counts of query
times, the peak memory range, and how many times Studio crashed. Every report also carried
Studio's version, your operating system and its major version, and your processor architecture.

Version 0.4.5 and the edge build of 13 November sent usage data from their first start unless
you turned it off. They said so in the status bar, and did not wait for your answer. The edge
build of 14 November asked first and sent none until you agreed. The edition of this notice
dated 13 November 2026 said usage data was sent only if you agreed. That described the edge
build of 14 November, and not version 0.4.5.

Each run of Studio used a new random ID that was never saved. As a result, we cannot connect one
run to the next, and we cannot tell which reports came from you.

**An install ID, only if you chose it.** If you chose to be counted across runs, Studio saved a
random ID on your computer and sent it with your usage data. The ID is not linked to your name,
email, account, computer or servers.

**Crash reports, only when you said yes.** After a crash, Studio showed you the report and sent
it only if you chose **Send** or **Always send**. A report contained the error's type, the names
of the functions that were running, how long Studio had run, and the version and system details
above. Any path inside your home folder was replaced with `~`, and quoted text in error messages
was removed.

**What reached us without being kept.** A request that carried a report also carried your IP
address and Studio's user agent (`Omnigraph-Studio/<version>`). Our analytics provider received
the IP address because the internet works that way, and discarded it before storing the report.
It did not look up your location. The stored reports contain no IP address and no location.

## Why we used it, and who holds it

We used these reports only to find and fix crashes, to learn which features are used, and to
make Studio faster. We do not sell them, use them for advertising, or combine them with other
data about you. We believe usage data without an install ID holds no personal data once stored.

The reports went to [PostHog](https://posthog.com), our analytics provider, which stores them
for us in the United States (PostHog US Cloud). PostHog processes them only on our instructions,
under a data processing agreement. Transfers from the EU, EEA, UK and Switzerland rely on
PostHog's certification under the EU-U.S. Data Privacy Framework and its UK and Swiss
extensions, and on the Standard Contractual Clauses in that agreement.

## How long it is kept

- Reports stored by PostHog: one year, after which PostHog deletes them, unless we delete them
  sooner.
- On your computer: starting version 0.4.7 or later deletes the reports that were waiting to be
  sent, the list of reports sent, and any crash report. It leaves one file, your install ID, if
  you chose one, so that you can still quote it to us (see below). Studio no longer reads or
  sends it.

## Your choices and rights

- **Stop the reports:** update to version 0.4.7 or later. To stop them in one of the three
  builds without updating, open the status bar's notice and turn usage data off, or set usage
  data to off and crash reports to never in Settings, Privacy.
- **Access or erase your data:** reports sent without an install ID cannot be linked to you, so
  we cannot find them. If you chose an install ID, send it to privacy@modernrelay.com, and we
  will give you a copy of its reports or delete them. The ID is the text of the file
  `telemetry/install-id` in Studio's data folder: `~/Library/Application Support/Omnigraph Studio`
  on macOS, `%APPDATA%\Omnigraph Studio` on Windows, and `~/.config/omnigraph-studio` on Linux
  (under `$XDG_CONFIG_HOME` when that is set). You can delete that file, or the whole
  `telemetry` folder, at any time.
- **Complain:** if you are in the EU, EEA or UK, you can complain to your data protection
  authority. We would like the chance to fix the problem first, so please write to us.

Depending on where you live, you may have other rights over your personal data, such as
correction or restriction. Write to us to use them.

## Changes

If this notice changes, we update it here and change the date at the top. If Studio were to
send any kind of report again, we would list it in [What Omnigraph Studio sends](telemetry.md)
before any release sent it.
