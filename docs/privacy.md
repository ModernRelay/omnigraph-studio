# Omnigraph Studio privacy notice

Last updated: 14 November 2026

This notice covers the Omnigraph Studio desktop app ("Studio"). It says where your graphs go;
what Studio sends to us, which from version 0.4.7 is no report of any kind; which requests
Studio makes for itself; which features send data to another company once you set them up; and
what three earlier builds sent, who holds it, how long it is kept, and what you can do about it.
[What Omnigraph Studio sends](telemetry.md) lists every report and field of those builds.

## Who we are

EQTR, Inc., which does business as Modern Relay ("we"), is the controller of the data described
here. To ask about it or to use your rights, write to
[privacy@modernrelay.com](mailto:privacy@modernrelay.com).

## Your graphs stay with you

Studio talks to the Omnigraph servers you connect it to, and to the sign-in service a server
names when you sign in to it. Your graphs, queries, values, names, server addresses, accounts
and tokens go nowhere else, unless you set up one of the two
[features that send data to another company](#features-you-set-up-yourself).

They come to us only if a server you connect is ours: Omnigraph Cloud, when you sign in to it.
No report from Studio ever contained them.

## What Studio sends to us

No reports. From version 0.4.7, Studio sends no usage data, no crash reports and no ID of you,
your computer or your installation. The code that made and sent those reports is removed, and
Studio has no setting that turns any of them on.

Studio contacts a server of ours in one case: Omnigraph Cloud, the service we run. It sends
nothing there until you have signed in to it.

## Requests Studio makes for itself

**Updates.** Studio checks GitHub for new versions of itself. GitHub receives those requests
and your IP address under [GitHub's privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).
We receive nothing from these checks. With **Update automatically** off in Settings, Studio
checks only when you ask it to.

**The list of AI models.** When Studio starts and its copy of the list is more than a day old,
it reads the list of AI models and their prices from [OpenRouter](https://openrouter.ai),
whether or not you use an AI feature. The request holds no key and nothing about you or your
graphs. OpenRouter receives your IP address and Studio's user agent
(`Omnigraph-Studio/<version>`) under [OpenRouter's privacy policy](https://openrouter.ai/privacy).
We receive nothing from it.

## Features you set up yourself

Two features send what they work on to a company other than us. Neither sends anything until
you set it up, and we receive nothing from either.

**AI features, with your own OpenRouter key.** Once you store an OpenRouter key in Settings, an
AI feature you use sends its input to OpenRouter and the model named in Settings, under your
OpenRouter account:

- A table filter written as a sentence sends that sentence, today's date and the names in your
  schema it can filter by: the type, its properties and their enum values, and its
  relationships and the types they reach. It sends no row.
- A branch summary sends each changed record's type, ID and title, and the names of the
  properties that changed. It sends no other value.
- A column fill sends, for each row it fills, the values in the columns you pick with those
  columns' names, the name and type of the property being filled, and your instruction.
- Testing the key in Settings sends the word "ping".

Each request asks OpenRouter to use only providers that do not collect the data they are sent.
Without a key, no AI feature sends anything, and removing the key in Settings stops them.

**The agent panel, with an agent you choose.** The agent panel runs the agent program you name
in Settings, such as Claude Code or Codex, on your computer. What you write to it and what it
reads from your graph go where that agent sends them: for those two, to the agent's provider,
under your account with them. Studio runs no agent until you choose one.

## The builds that sent reports

Three builds of Studio sent reports to us: version 0.4.5, released on 13 November 2026, and two
pre-release ("edge") builds, the one of 13 November 2026 and the earlier of the two made on
14 November 2026. No version before or after them sends any. Every edge build downloaded from
21:45 UTC on 14 November 2026 is version 0.4.7 or later and sends none. You can tell one of the
three builds by the Privacy section in its Settings, which no other build has. None of the
three sent anything while the `DO_NOT_TRACK` or `OMNIGRAPH_STUDIO_TELEMETRY=0` environment
variable was set.

**Usage data.** It included the features you used (for example, "a query ran" or "a write
finished") and their outcomes, durations and sizes. Numbers were rounded into ranges, so a
report said "11 to 100 rows", never the exact count. A daily summary added counts of query
times, the peak memory range, and how many times Studio crashed. Every report also carried
Studio's version, your operating system and its major version, and your processor architecture.

Version 0.4.5 and the edge build of 13 November sent usage data from their first start unless
you turned it off. They said so in the status bar, and did not wait for your answer. The earlier
edge build of 14 November asked first and sent none until you agreed. The edition of this notice
dated 13 November 2026 said usage data was sent only if you agreed. That described the earlier
edge build of 14 November, and not version 0.4.5.

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
