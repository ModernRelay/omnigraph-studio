# Omnigraph Studio privacy notice

Last updated: 13 November 2026

This notice covers the Omnigraph Studio desktop app ("Studio"). It says what Studio sends to us,
why, who else receives it, how long it is kept, and what you can do about it.
[What Omnigraph Studio sends](telemetry.md) lists every report and field.

## Who we are

EQTR, Inc., which does business as Modern Relay ("we"), is the controller of the data described
here. To ask about it or to use your rights, write to
[privacy@modernrelay.com](mailto:privacy@modernrelay.com).

## Your graphs stay with you

Studio talks to the Omnigraph servers you connect it to. Your graphs, queries, values, names,
server addresses, accounts and tokens go only to those servers. They never come to us, and no
report from Studio contains them.

## What Studio sends to us

Studio sends nothing from a development build, or while the `DO_NOT_TRACK` or
`OMNIGRAPH_STUDIO_TELEMETRY=0` environment variable is set.

**Usage data, only if you agree.** Studio asks you in the status bar and sends no usage data
until you choose **Share** or **Share and count my runs**. It includes the features you use (for example,
"a query ran" or "a write finished") and their outcomes, durations and sizes. Numbers are rounded
into ranges, so a report says "11 to 100 rows", never the exact count. A daily summary adds
counts of query times, the peak memory range, and how many times Studio crashed. Every report
also carries Studio's version, your operating system and its major version, and your processor
architecture.

Each run of Studio gets a new random ID that is never saved. As a result, we cannot connect one
run to the next, and we cannot tell which reports came from you.

**An install ID, only if you choose it.** If you choose **Share and count my runs**, Studio saves a random
ID on your computer and sends it with your usage data. This lets us count how many people come
back to Studio. The ID is not linked to your name, email, account, computer or servers. You can
reset it, and turning usage data off deletes it.

**Crash reports, only when you say yes.** After a crash, Studio shows you the report and sends it
only if you choose **Send** or **Always send**. A report contains the error's type, the names of
the functions that were running, how long Studio had run, and the version and system details
above. Any path inside your home folder is replaced with `~`, and quoted text in error messages
is removed.

## What reaches us without being kept

When Studio sends a report, the request carries your IP address and Studio's user agent
(`Omnigraph-Studio/<version>`). Our analytics provider receives the IP address because the
internet works that way, and it discards the address before storing the report. It does not look
up your location. Our stored reports contain no IP address and no location.

Studio also checks GitHub for new versions of itself. GitHub receives those requests and your IP
address under [GitHub's privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).
We receive nothing from these checks. With **Update automatically** off in Settings, Studio
checks only when you ask it to.

## Why we use it, and on what legal basis

We use these reports only to find and fix crashes, to learn which features are used, and to make
Studio faster. We do not sell them, use them for advertising, or combine them with other data
about you.

Usage data, the install ID and crash reports are each sent only with your consent (Article 6(1)(a)
of the GDPR, and the rules on sending information from your device). You give it by choosing
them, and you can withdraw it at any time in Settings. We believe usage data without an install
ID holds no personal data once stored.

## Who receives it

Reports go to [PostHog](https://posthog.com), our analytics provider, which stores them for us
in the United States (PostHog US Cloud). PostHog processes them only on our instructions, under
a data processing agreement. Transfers from the EU, EEA, UK and Switzerland rely on PostHog's
certification under the EU-U.S. Data Privacy Framework and its UK and Swiss extensions, and on
the Standard Contractual Clauses in that agreement.

## How long it is kept

- Reports waiting on your computer to be sent: at most 7 days. Turning a report type off deletes
  its waiting reports at once.
- Reports stored by PostHog: one year, after which PostHog deletes them.

## Your choices and rights

- **Turn usage data off:** choose **Don't share** in the status bar notice, or change it in
  Settings, Privacy.
  Crash reports have their own choice: Ask each time, Always send or Never send.
- **See what is sent:** **View Telemetry Log** in the command palette shows every report Studio
  is holding and every report it sent in the last 7 days.
- **Access or erase your data:** reports sent without an install ID cannot be linked to you, so
  we cannot find them. If you chose an install ID, send it (shown in Settings, Privacy) to
  privacy@modernrelay.com, and we will give you a copy of its reports or delete them.
- **Complain:** if you are in the EU, EEA or UK, you can complain to your data protection
  authority. We would like the chance to fix the problem first, so please write to us.

Depending on where you live, you may have other rights over your personal data, such as
correction or restriction. Write to us to use them.

## Changes

If this notice changes, we update it here and change the date at the top. If Studio starts
sending a new kind of report, we will list it in [What Omnigraph Studio sends](telemetry.md)
before any release sends it.
