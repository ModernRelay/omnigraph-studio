# 017: Studio updates itself

Status: draft, not implemented; needs Developer ID signed and notarized releases (issue #3), an
update signing key, and answers to the open questions below · 2026-10-22

## Problem

Studio never says that a newer version exists. A person on 0.2.0 learns of 0.2.1 only from the
releases page, downloads a disk image or tarball by hand and replaces the app. `omnigraph studio`
(RFC #1017, "Installing Studio") will install whatever Studio's latest download URL serves and
pins no Studio version, because "Studio updates itself"; today there is neither an updater nor a
latest URL, since every release is a pre-release and every file name carries the version.
Issue #3 asks for both, with signing. This plan makes the releases serve updates in the format
Tauri apps use, and makes Studio check for them, verify them and install them with
`cargo-packager-updater`, Tauri's updater made for any Rust app. It is a spec only; nothing is
implemented.

## Principles

- I: the engine is the authority, and one client owns every request. The update check and
  download are the only requests Studio makes that are not to an Omnigraph server, and the
  download goes through the updater crate's own HTTP client. This plan amends I's "in practice"
  line to name them (a patch: 1.2.0 to 1.2.1), and the architecture and agent rules with it.
- III: a write has three outcomes. Restarting to update goes through the same confirmation as
  Quit, so no draft or unsaved query is lost to an update.
- VI: every control explains itself. The update item says what it is doing and, when it cannot
  install, why and what to do instead.
- VII: loudness matches cost. Checks and downloads are silent; a ready update is one item in
  the status bar, never a dialog or a password prompt; a background failure is logged, not
  shown.
- VIII: keyboard first. Check for Updates, Restart to Update, Download Update and View Release
  Notes are actions with bindings, in the palette and the app menu.
- IX: work is bounded. One check or download at a time. The manifest has a size cap; the
  download is bounded by time, because the crate reads it whole into memory (open question 7).
- XII: evidence over assumption. Every Zed, GitHub, crate and Omnigraph fact below is checked.

## What exists today (checked)

- Studio (`omnigraph-studio` main at `dc2dfe8`):
  - The version is the workspace's, `0.2.1` (`Cargo.toml`). `graph_store::init` builds the one
    HTTP client, Zed's `reqwest_client` (the `zed-reqwest` fork, `0.12.15-zed`) with the user
    agent `Omnigraph-Studio/0.2.1`, and sets it as GPUI's global (`cx.set_http_client`,
    `crates/graph_store/src/graph_store.rs`).
  - Only `graph_client` makes requests: "Every HTTP request" (`docs/architecture.md`, layer 2),
    "Only `graph_client` makes HTTP requests" (`AGENTS.md`), "one client crate owns every
    request" (constitution I).
  - Releases: pushing `v<version>` runs `.github/workflows/release.yml`, which builds macOS
    arm64, Linux x86_64 and arm64 and Windows x86_64, and publishes the files with `SHA256SUMS`;
    a `0.*` version is published with `--prerelease`. macOS builds are signed with a Developer
    ID and notarized only when the workflow's Apple secrets are set; the repository has none
    (`gh secret list` is empty), so all three releases (0.1.0, 0.2.0, 0.2.1) are signed ad hoc.
  - `script/bundle-mac` builds `Omnigraph Studio.app` with the identifier
    `com.modernrelay.omnigraph-studio` and an XML `Info.plist` holding
    `CFBundleShortVersionString`; with `STUDIO_SIGN_IDENTITY` it signs the app (hardened
    runtime, secure timestamp) and the disk image, and with `STUDIO_NOTARY_PROFILE` it notarizes
    and staples the disk image. File names, since `6d1c702`:
    `Omnigraph-Studio-<version>-arm64.dmg`, `omnigraph-studio-<version>-linux-<arch>.tar.gz`,
    `omnigraph-studio-<version>-windows-x86_64.zip`. Linux and Windows archives are unpacked
    anywhere (`script/bundle-linux`, `script/bundle-windows`); there is no installer.
  - A binary under a build directory (`target/…`) counts as a development build and uses
    "Omnigraph Studio Dev" data; a binary inside an `.app` bundle never does
    (`is_build_output`, `crates/studio_settings/src/studio_settings.rs`).
  - Quitting with unsaved work asks first: `Closing::Quit`, "Quit Omnigraph Studio?", naming
    what is lost (`crates/studio_workspace/src/studio_workspace.rs`). The app menu holds
    Settings…, Services and Quit (`crates/studio/src/menus.rs`).
  - `deny.toml` allows MIT and Apache-2.0 and only warns on a crate built in two versions.
  - Nothing checks for updates. The README lists auto-update as not implemented, and spec 001
    defers it "until there is a first signed release".
- GitHub (2026-10-22):
  - `GET /repos/ModernRelay/omnigraph-studio/releases/latest` answers 404, and
    `releases/latest/download/SHA256SUMS` answers 404: `latest` skips pre-releases, and every
    release is one.
  - v0.2.1 carries the macOS disk image (50 MB, named with a capital G, before `6d1c702`), the
    Linux tarballs (179 MB arm64, 211 MB x86_64), the Windows zip (46 MB) and `SHA256SUMS`.
    Sizes moved by under 2 MB from 0.1.0 to 0.2.1.
- `cargo-packager-updater` `0.2.3` (crates.io, last updated 2025-07-21; MIT or Apache-2.0;
  CrabNebula's `cargo-packager` repository, whose updater still had unreleased commits in
  October 2026):
  - The manifest is Tauri's: `version`, optional `notes` and `pub_date`, and `platforms` keyed
    `<os>-<arch>` (`macos-aarch64`, `linux-x86_64`, `windows-x86_64`, …), each with `url`,
    `signature` (the `.sig` file's content) and `format` (`app`, `appimage`, `nsis` or `wix`).
    `RemoteRelease` deserializes it, and every field of `Update` is public (`src/lib.rs`,
    `src/custom_serialization.rs`).
  - `Update::download_extended` reads the whole file into memory, reporting progress per chunk,
    then verifies its minisign signature against the configured public key before returning
    it; it has no size limit, and its request timeout is the caller's.
  - On macOS, `Update::install` unpacks the `.app.tar.gz` into a temporary directory, renames
    the running bundle into a second temporary directory as a backup, and renames the new one
    into place. If the first rename is refused for permission, it runs `osascript` with `do
    shell script "rm -rf '<path>' && mv -f '<new>' '<path>'" with administrator privileges`,
    the paths unescaped inside single quotes. If the last rename fails, the backup is not put
    back and its temporary directory is deleted.
  - `Updater::check` reads the manifest without a size limit and, on Linux, sets
    `SSL_CERT_FILE` and `SSL_CERT_DIR` to Debian's paths for the whole process when they are
    unset.
  - These are unchanged on the repository's `main` (October 2026).
  - It brings upstream `reqwest` 0.12 (blocking, rustls with native roots), which Studio's
    build does not contain yet, and `minisign-verify`.
- `cargo-packager` `0.11.8`: `cargo packager signer generate` makes a minisign key pair and
  `cargo packager signer sign <file>` writes `<file>.sig`, reading the key and its password
  from `CARGO_PACKAGER_SIGN_PRIVATE_KEY` and `CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD`
  (`src/cli/signer/`). It writes no `latest.json`; Tauri's release action does that for Tauri
  apps.
- Zed (`v1.23.2`):
  - `auto_update` asks Zed's cloud for `/releases/<channel>/latest/asset` with the OS, the
    architecture and, when telemetry is on, the metrics and system IDs, through Zed's `Client`;
    it keeps state in Zed's key-value `db` and takes Zed's `Workspace`. On macOS it copies the
    mounted app over the running one with `rsync -av --delete`. No step checks a signature or a
    checksum (`crates/auto_update/src/auto_update.rs`).
  - GPUI restarts into a path: `App::restart`, `set_restart_path`, `app_path` and
    `on_app_restart` (`crates/gpui/src/app.rs`); on macOS the restart waits until the process
    has exited, then opens the app path (`crates/gpui_macos/src/platform.rs`).
- Omnigraph (`origin/main` at `e60eb016`), RFC
  `docs/rfcs/2026-10-16-graph-access-serving-and-cross-process-writes.md`, "Installing Studio":
  the CLI finds Studio by bundle identifier and, before every open and every install, runs
  `codesign --verify --deep --strict` against `identifier "com.modernrelay.omnigraph-studio"
  and anchor apple generic and certificate leaf[subject.OU] = "<team>"`; it downloads "the
  latest disk image for the machine's architecture"; it "pins no Studio version: it installs
  whatever the latest URL serves, and Studio updates itself". The CLI names no Studio file yet.

## Design

### What the releases serve

- **Full releases.** A tagged release is published without `--prerelease`, so GitHub's
  `latest` names it. A `0.x` version already says that things may change.
- **Signed or not published.** A tag push fails before `gh release create` unless the macOS
  app passes `codesign --verify --deep --strict` against the CLI's requirement with Studio's
  team, and the disk image passes `xcrun stapler validate`. A hand-run build may still be ad
  hoc; it is never published.
- **The update file.** `script/bundle-mac` also writes `Omnigraph-Studio-<version>-arm64.app.tar.gz`,
  the signed app with `Omnigraph Studio.app/` as its top directory, and the workflow signs it
  with `cargo packager signer sign` (cargo-packager `0.11.8`), writing its `.sig`.
- **The manifest.** The workflow writes `latest.json` from the release's files and uploads it:

  ```json
  {
    "version": "0.3.0",
    "notes": "<the release's notes>",
    "pub_date": "2026-11-02T10:00:00Z",
    "platforms": {
      "macos-aarch64": {
        "url": "https://github.com/ModernRelay/omnigraph-studio/releases/download/v0.3.0/Omnigraph-Studio-0.3.0-arm64.app.tar.gz",
        "signature": "<the content of Omnigraph-Studio-0.3.0-arm64.app.tar.gz.sig>",
        "format": "app"
      }
    }
  }
  ```

  Linux and Windows get entries when they install in place (Deferred); until then Studio reads
  only `version` for them.
- **Version-free names.** Each release also carries its first-install files under names
  without the version, at `https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/<name>`:
  `Omnigraph-Studio-arm64.dmg`, `omnigraph-studio-linux-x86_64.tar.gz`,
  `omnigraph-studio-linux-aarch64.tar.gz` and `omnigraph-studio-windows-x86_64.zip`. The CLI
  installs from the first; Studio's Download opens the one for its platform. GitHub matches
  names case-sensitively; issue #3's example name, spelled with a capital G, predates the
  rename and is corrected there.
- **The keys.** The update key pair is made once with `cargo packager signer generate`; the
  private key and its password are workflow secrets (`STUDIO_UPDATER_KEY`,
  `STUDIO_UPDATER_KEY_PASSWORD`, open question 2), and the public key is a constant in Studio.
  The Apple team identifier is written in this plan, the README and Studio (`TEAM_ID`, open
  question 1).

### Who checks, and when

- A release build checks: on macOS a Studio running from an `.app` bundle, on Linux and
  Windows a binary that is not a build output (`is_build_output`). A development build never
  checks; Check for Updates there says "Updates are off in a development build."
- The first check runs 30 seconds after launch, then every hour while Studio runs, and when the
  person chooses Check for Updates. One check or download runs at a time; a manual check
  during one joins it.
- `studio.auto_update` (default `true`): when `false`, nothing is checked or downloaded unless
  the person asks.

### The check

- Studio reads the manifest itself, through GPUI's global HTTP client:
  `GET https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/latest.json`,
  following GitHub's redirect to its file host, HTTPS only, at most 64 KiB, 15 seconds, with
  the user agent `Omnigraph-Studio/<version>`. It sends no cookie, token, identifier or
  telemetry. It parses the answer as the crate's `RemoteRelease`.
- The update is newer when `version` is greater than the running version by semver
  precedence; equal or older is "up to date".
- An answer that fails these, or no answer, leaves Studio as it was: a background check logs
  it; a manual check says "Couldn't check for updates: <reason>."

### Downloading and installing on macOS

1. **Where Studio runs.** The bundle is `cx.app_path()`. Studio installs only where it can
   replace the bundle itself: not on a read-only volume or a mounted disk image (`/Volumes/…`),
   not under App Translocation (`…/AppTranslocation/…`), and only when the person can write
   both the bundle and its folder. Anywhere else it offers Download and never calls `install`,
   so the crate's administrator prompt never runs.
2. **Download and verify.** Studio builds an `Update` from the `macos-aarch64` entry (the
   update file's URL and signature, `format: app`, the bundle path, Studio's public key, the
   user agent `Omnigraph-Studio/<version>`, a 30-minute timeout) and calls
   `download_extended` on a background thread. It returns the file only when its minisign
   signature verifies. Progress is reported when the whole percentage changes.
3. **Check the version.** Studio reads `Omnigraph Studio.app/Contents/Info.plist` from the
   downloaded archive in memory; its `CFBundleShortVersionString` must equal the manifest's
   `version`, so an older signed build is never installed under a newer number.
4. **Install.** `Update::install` on a background thread: the crate unpacks the archive,
   renames the running bundle aside and the new one into place. The running process keeps its
   open files.
5. **Clean.** The next launch deletes leftover `packager_current_app*` and
   `packager_updated_app*` directories under the system temp directory older than 24 hours.

Studio is now **ready to restart**. Quitting instead also works: the next launch is the new
version.

### Restart

- Restart to Update asks as Quit does when work would be lost, titled "Restart to update
  Omnigraph Studio?", then calls `cx.restart()`. GPUI waits for the process to exit and opens
  the bundle at the same path. The session is restored on launch, as after any quit.

### Linux and Windows

- In this plan they check and tell, and do not install: Linux tarballs have no install
  location convention (the RFC's Linux path only prints how to install), and Windows ships a
  zip with no installer or package identity (issue #3, item 4). The item offers **Download**,
  which opens the version-free file's URL in the browser, and **Release Notes**.

### What the person sees

The update item sits at the right of the status bar and is absent while there is nothing to
say.

| State | Status bar item | Palette and app menu |
|---|---|---|
| Idle, or checking in the background | nothing | Check for Updates |
| Checking, asked by the person | Checking for updates… | — |
| Up to date, asked by the person | Omnigraph Studio 0.2.1 is up to date (10 s, then gone) | Check for Updates |
| Downloading | Downloading 0.3.0 · 42% | — |
| Installing | Installing 0.3.0… | — |
| Ready | Restart to update to 0.3.0 | Restart to Update, View Release Notes |
| Available, not installable here | 0.3.0 is available · Download | Download Update, View Release Notes |
| Failed, asked by the person | the reason, and what to do | Check for Updates |

Failures, as said:

| Cause | Copy |
|---|---|
| No answer, or a bad manifest | Couldn't check for updates: <reason>. |
| Download failed or timed out | The download of 0.3.0 didn't finish. Studio will try again at the next check. |
| Signature refused | The 0.3.0 download isn't signed for Omnigraph Studio and was not installed. |
| Version mismatch | The 0.3.0 download contains <found> and was not installed. |
| Translocated or on a disk image | Move Omnigraph Studio to Applications to update it. · Download |
| Bundle or folder not writable | Studio can't replace itself in <folder>. · Download |
| Install failed | Couldn't install 0.3.0: <reason>. · Download |

A download refused for its signature or version is not fetched again for that version in this
run; the next newer version is. Failed downloads wait for the next hourly check.

### Actions and settings

- In `studio_actions`: `CheckForUpdates` ("Check for a newer version of Omnigraph Studio."),
  `RestartToUpdate` ("Restart to finish installing the update that is ready."),
  `DownloadUpdate` ("Open the newer version's download in the browser."), and
  `ViewReleaseNotes` ("Open the release notes of the update, or of this version"; the
  release's page, `releases/tag/v<version>`). Each is in the palette only where it applies,
  has a default binding (open question 4) and, on macOS, sits in the app menu after Settings…:
  "Check for Updates…", or "Restart to Update" when one is ready.
- `studio.auto_update`: `true` in `assets/settings/default.json`, titled "Update
  automatically" in Settings.

### Where it lives

- A new crate, `studio_update`, in layer 3 beside `graph_store`: the GPUI entity that
  schedules checks and holds the state, the manifest fetch, and the calls into
  `cargo-packager-updater` `0.2.3` (exact version) on background threads.
- `studio_workspace` draws the status bar item from the entity; `studio` registers the actions
  and the menu item.
- Amendments, made with the crate: constitution I's "in practice" reads "one client crate owns
  every request to a server; `studio_update` alone fetches Studio's own releases" (1.2.1);
  `docs/architecture.md` adds the crate to layer 3 and says `graph_client` owns every request
  to an Omnigraph server; `AGENTS.md` says the same; `script/check-layers.py` gets the row.

### Limits

| What | Limit |
|---|---|
| First check after launch | 30 s |
| Check interval | 1 h |
| Manifest | 64 KiB, 15 s |
| Download | 30 min; no size limit in the crate (open question 7); about 50 MB today |
| Work at a time | one check or download |
| Leftover update directories | deleted after 24 h |

### Release workflow changes

- Publish full releases; keep `--latest`.
- Refuse a tag's release when the macOS app fails the team requirement or the disk image is
  not stapled.
- Build, sign and upload `Omnigraph-Studio-<version>-arm64.app.tar.gz` and its `.sig`
  (installing `cargo-packager` `0.11.8` in the macOS job for `signer sign`).
- Upload the first-install files a second time under their version-free names.
- Write and upload `latest.json`.

### Prerequisites outside Studio

- **ModernRelay**: a Developer ID Application certificate and an App Store Connect API key in
  the workflow's secrets (`release.yml` lists them), the team identifier recorded, and the
  update key pair made and its private half stored (open question 2).
- **Omnigraph**: nothing; the CLI's install path (RFC #1017) reads the version-free disk image.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| `cargo-packager-updater` for the manifest, the signature check and the macOS install | Studio's own updater; Zed's `auto_update`; Velopack; Sparkle | It is Tauri's updater without Tauri: the manifest and minisign keys Tauri apps already use, with the signature checked before anything is installed. Zed's crate asks zed.dev with telemetry identifiers and checks nothing it downloads. Velopack checks only checksums listed in the same feed and needs the .NET SDK in CI. Sparkle is macOS-only Objective-C with its own appcast and windows |
| Full downloads, no deltas | Velopack's delta packages | A full macOS update is about 50 MB and has stayed there across releases |
| Studio reads `latest.json` itself; the crate downloads, verifies and installs | The crate's `Updater::check` | `check` reads the manifest without a size limit, and on Linux sets Debian's certificate paths for the whole process, which other distributions do not have; Studio's own fetch keeps the cap, its user agent and its HTTP client |
| Install only where Studio can write; offer Download elsewhere | The crate's administrator prompt | The prompt runs a root shell command with the paths unescaped in single quotes, and a password prompt from a background update is loud (VII) |
| The bundle's version must equal the manifest's | Trust the manifest | The signature covers the file, not the version written beside it; without the check an older signed build could be installed as newer |
| The workflow checks the Developer ID team; Studio checks the minisign signature | Studio also runs `codesign` against the team before installing | Every published build passes the team check in CI, which the CLI relies on; a second check in Studio would unpack the archive twice for the same assurance |
| Keep the disk image; add the `.app.tar.gz` | One file for both | The CLI and first installs take a disk image; the updater's `app` format is a tarball of the bundle |
| Full releases for `0.x` | Keep pre-releases and find the newest through the API | `latest` and `latest/download` skip pre-releases; the CLI's install needs `latest` |
| Refuse to publish an unsigned macOS release | Publish it and let clients refuse it | Every updating client and the CLI would refuse it anyway |
| macOS installs in place; Linux and Windows tell and link | All three in place now | Linux tarballs have no install location; Windows has no installer (issue #3, item 4) |
| Nothing identifying is sent | Zed's metrics and system IDs | Studio has no telemetry; a static file needs none |
| Development builds never check | Check and refuse to install | A build from `target/` must never be replaced by a release |

## Deferred

- Linux in place, as an AppImage built by `cargo-packager` (`format: appimage`), and Windows,
  as an NSIS installer built by it (`format: nsis`), which also gives Windows the package
  identity issue #3 asks for. Both reuse the same crate and manifest.
- Intel Macs: there is no x86_64 macOS build (issue #3 asks whether to support them).
- Release channels (beta, nightly), delta updates, and a policy that turns updates off for
  every person on a machine.
- Rotating the update key: a new public key ships in a release signed with the old one.
- No Omnigraph engine change is needed; `docs/deferred-engine-changes.md` is unchanged.

## Open questions

1. The Apple team identifier, who holds the Developer ID certificate, and when the secrets are
   set. Nothing in this plan ships before.
2. Who holds the update key and its password, and where: a repository secret, or a protected
   environment whose use needs a reviewer, so that a workflow change alone cannot sign a
   release.
3. Whether a beta channel is wanted, which would bring pre-releases back alongside `latest`.
4. The default bindings for the four actions (every action has one, `AGENTS.md`).
5. Whether a release may ever require an update (a minimum version beside the manifest), for
   example when a contract bump makes older Studios refuse every server.
6. To check when built: that the swapped bundle opens without a Gatekeeper prompt.
7. Upstream: issues for `cargo-packager-updater` to escape the administrator command's paths,
   put the backup back when the last rename fails, limit the download's size, and leave the
   certificate variables alone. If they are not fixed, whether Studio keeps a small patched
   copy of the crate (`lib.rs` is 1,216 lines).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-22 | No release is `latest`, and every file name carries the version | `gh api repos/ModernRelay/omnigraph-studio/releases/latest`; `curl -L …/releases/latest/download/SHA256SUMS`; `gh release view <tag> --json assets,isPrerelease` for 0.1.0, 0.2.0, 0.2.1 | 404 and 404; every release a pre-release. macOS disk image 49,870,872, 50,315,597 and 50,318,066 bytes; Linux arm64 177,883,752 to 179,434,982; Linux x86_64 209,439,435 to 211,172,133; Windows zip (0.2.1 only) 46,235,407 |
| 2026-10-22 | Releases are signed ad hoc because no Apple secrets are set | `gh secret list` on the repository; `.github/workflows/release.yml` (header, the signing step, `--prerelease` for `0.*`) | No secrets listed (organization secrets would not show here); the workflow signs ad hoc without them |
| 2026-10-22 | `cargo-packager-updater` 0.2.3 behaves as stated | The crate's source from crates.io: `src/lib.rs` (`Updater::check`, `Update::download_extended_inner`, `verify_signature`, the macOS `install_inner`, `extract_path_from_executable`), `src/custom_serialization.rs`, `Cargo.toml`; the same file on the repository's `main` | Signature verified in the download before returning; macOS install renames the bundle aside and the new one in, with no restore after a failed last rename; `osascript … with administrator privileges` on a permission error, paths unescaped; Linux `set_var` of `SSL_CERT_FILE` and `SSL_CERT_DIR` in `check`; no size limit; all unchanged on `main` |
| 2026-10-22 | The crates are current enough to depend on, and their alternatives were weighed | crates.io API for `cargo-packager-updater`, `cargo-packager`, `velopack`, `self_update`, `tauri-plugin-updater`; GitHub commits of `crabnebula-dev/cargo-packager`; Velopack `src/lib-rust/src/manager.rs` | crates.io last updated: updater 0.2.3 on 2025-07-21, packager 0.11.8 on 2025-11-27, Velopack 1.2.161 on 2026-10-17. The updater still changes on `main` (#434 and #436, 2026-10-12, unreleased). Velopack verifies SHA-1/SHA-256 from its feed (`verify_package_checksum`), no signature |
| 2026-10-22 | `cargo-packager` signs files but writes no manifest | `cargo-packager` 0.11.8 source: `src/cli/signer/sign.rs`, `generate.rs`; a search for `latest.json` | `signer sign` reads `CARGO_PACKAGER_SIGN_PRIVATE_KEY` and `…_PASSWORD`; no mention of `latest.json` |
| 2026-10-22 | The crate adds a second `reqwest` and nothing `deny.toml` refuses | Studio's `Cargo.lock`; `deny.toml` | Only `zed-reqwest` 0.12.15-zed is present; `multiple-versions = "warn"`; MIT and Apache-2.0 allowed |
| 2026-10-22 | Zed's updater talks to Zed's cloud and checks nothing it downloads | Zed `v1.23.2` `crates/auto_update/src/auto_update.rs` and its `Cargo.toml`; `grep -c -i -E 'codesign\|spctl\|sha256\|checksum\|signature'` on the source | Asks `/releases/<channel>/latest/asset` through `build_zed_cloud_url_with_query`, with metrics and system IDs when telemetry is on; `rsync -av --delete` onto the running app; depends on `client`, `db`, `workspace`; the grep finds 0 matches |
| 2026-10-22 | GPUI restarts into the bundle's path after the process exits | Zed `v1.23.2` `crates/gpui/src/app.rs` (`app_path`, `restart`, `set_restart_path`, `on_app_restart`), `crates/gpui_macos/src/platform.rs` (`restart`) | The macOS restart runs a script that waits while `kill -0 <pid>` succeeds, then opens the app path |
| 2026-10-22 | The CLI's install relies on Studio's signature, a latest disk image and Studio updating itself | Omnigraph `origin/main` (`e60eb016`) `docs/rfcs/2026-10-16-graph-access-serving-and-cross-process-writes.md`, "Installing Studio"; `git grep` for a Studio file name or release URL on `origin/main` | As quoted under What exists today; the CLI names only the bundle identifier |
| 2026-10-22 | Studio has one HTTP client, set as GPUI's global, and an XML `Info.plist` with its version | `crates/graph_store/src/graph_store.rs` (`init`); `script/bundle-mac` | `ReqwestClient::user_agent("Omnigraph-Studio/<version>")`, then `cx.set_http_client`; `CFBundleShortVersionString` in the plist it writes |

## Corrections

None yet.
