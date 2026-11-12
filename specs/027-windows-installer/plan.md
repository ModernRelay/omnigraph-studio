# 027: A Windows installer the launcher can find

Status: step 1 built · 2026-11-09 (branch `claude/sharp-varahamihira-ee7f4e`)

## Problem

Windows ships as a zip. A person unzips it somewhere, and nothing on the machine says that
Studio is installed or where: no Start menu entry, no entry in Installed apps, no uninstaller.
`omnigraph studio` (ModernRelay/omnigraph#1017) finds Studio on macOS by its bundle identifier
and installs it when it is missing; on Windows it has nothing to look up, so it can only print
how to install Studio. Issue #3, item 4, asks for "a package identity the CLI can detect (an
installer or a store package rather than a zip)". This plan picks the installer, builds it in
the release, and says what the CLI looks up.

## Principles

- I: one client owns every request. Nothing here adds a request: the installer is a release
  file, and Studio's Download opens it in the browser as it opened the zip.
- VI: every control explains itself. The installer's pages are Inno Setup's own; the release
  notes say what SmartScreen asks while the installer is unsigned and how to answer.
- VII: loudness matches cost. The installer asks for no administrator rights, so installing and
  updating never raise a UAC prompt.
- XII: evidence over assumption. The release workflow installs the built installer on its
  runner, checks the registry entries the CLI reads, and uninstalls it (Validation log).

## What exists today (checked)

- Studio (`0211c683`, v0.4.1):
  - `script/bundle-windows`, run in Git Bash, builds `omnigraph-studio-<version>-windows-<arch>.zip`
    holding `omnigraph-studio.exe`, `LICENSE` and `THIRD-PARTY-NOTICES.md`. Its header says "An
    installer is not built yet, and the executable is not signed."
  - `.github/workflows/release.yml` builds it on `windows-2025` (job `build`, target
    `windows-x86_64`), uploads `target/bundle/*.zip`, and `script/release-files.py` copies it to
    the version-free `omnigraph-studio-windows-x86_64.zip`. The release notes say SmartScreen may
    ask first ("More info, then Run anyway").
  - Studio keeps its settings in `%APPDATA%\Omnigraph Studio`, apart from the executable
    (`config_dir`, `crates/studio_settings/src/studio_settings.rs`). A binary is a release build
    unless it sits in a Cargo build directory (`is_release_build`), so an installed copy checks
    for updates as the zip's copy does.
  - The updater installs in place on macOS only. On Windows it checks `latest.json` and offers
    **Download**, which opens `download_url("windows-x86_64")`, the version-free zip
    (`crates/studio_update/src/manifest.rs`). It decodes only its own platform's entry of
    `latest.json`, and only a macOS `app` entry is ever turned into an update, so a Windows entry
    a later release adds is ignored by every Studio up to 0.4.1 (spec 017, "The manifest").
  - The update key `crates/studio_update/update-key.pub` is empty (0 bytes), so the `sign-update`
    job signs nothing yet.
  - The executable has no icon resource (spec 001, Deferred: "A Windows installer, a signed
    executable and its icon").
- GitHub's runner images (`actions/runner-images`, `images/windows/Windows2025-Readme.md` and
  `Windows2022-Readme.md`, read 2026-11-09): `windows-2025` has Inno Setup 6.7.1 and WiX Toolset
  3.14, and no NSIS; `windows-2022` has Inno Setup 6.7.1, NSIS 3.10 and WiX 3.14.
- There is no Windows code-signing certificate.

## The options

What the CLI needs on Windows is the macOS check's counterpart: a fixed name it can look up to
learn whether Studio is installed, where, and at which version; and a file it can download and
install without the person's help.

| | MSIX | MSI (WiX) | Inno Setup (or NSIS) |
|---|---|---|---|
| **Identity the CLI looks up** | A package family name, `<Name>_<publisher hash>`, read with `Get-AppxPackage` or the `PackageManager` API. The hash is taken from the `Publisher` in the manifest, which must equal the signing certificate's subject: a new certificate is a new family, so the first signed release would not be the same app as an unsigned one. | The `UpgradeCode` GUID, fixed for good; the CLI asks `MsiEnumRelatedProducts`, or scans the uninstall keys, since the key is named by the `ProductCode`, which changes with every major upgrade. | An uninstall key whose name we choose and never change: Inno's `AppId` gives `…\CurrentVersion\Uninstall\<AppId>_is1`, with `InstallLocation`, `DisplayVersion` and `Publisher`. An `App Paths` key lets anything start Studio by its executable's name. |
| **Install without admin** | Yes: packages install per user. | Yes, with a per-user package (`Scope="perUser"`), but a per-machine install is MSI's habit, and dual-purpose packages are a known trap (`ALLUSERS`, `MSIINSTALLPERUSER`). | Yes: `PrivilegesRequired=lowest` installs into `%LOCALAPPDATA%\Programs` and writes under `HKCU`. |
| **Updates through Studio's updater** | Studio cannot replace its own files: the package is read-only under `WindowsApps`. Updating goes through the deployment API (`AddPackageAsync`) or an `.appinstaller` feed that Windows polls, which are neither Studio's manifest nor its minisign check. Writes to `%APPDATA%` from a packaged app are redirected unless the manifest opts out, which moves Studio's settings. | Studio downloads the next `.msi`, checks its minisign signature, and runs `msiexec /i … /qn` after it quits; a major upgrade replaces the old version. | Studio downloads the next installer, checks its minisign signature, and runs it with `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART` after it quits; Inno closes running copies through the Restart Manager and installs over the old version under the same `AppId`. No UAC prompt, since nothing needs admin. |
| **Code signing** | Required. An MSIX must be signed by a certificate the machine trusts to install at all; an unsigned one installs only with developer mode or `Add-AppxPackage -AllowUnsigned` (Windows 11) and a special publisher OID. Store distribution signs for us but adds a submission process and Store policies. | Optional. Unsigned, SmartScreen asks on a browser download ("Windows protected your PC": More info, Run anyway). Signing later needs an Authenticode certificate (an OV certificate, or Azure Trusted Signing) and `signtool`; the identity does not change. | Optional, as for MSI. ISCC's `SignTool` directive signs the installer and its uninstaller when a certificate exists; the identity does not change. |
| **Tooling** | `makeappx` and `signtool` from the Windows SDK. | WiX 3.14 is on the runner; WiX 4 and later is a .NET tool, and WiX 6 releases come under the Open Source Maintenance Fee for organisations that earn revenue. | Inno Setup 6.7.1 is on `windows-2025`, free for commercial use. NSIS is not on `windows-2025`; `cargo-packager`'s `nsis` format downloads it while it builds. |

Unsigned, each option behaves the same way on a browser download: Edge and Chrome mark the
file with the Mark of the Web, and SmartScreen asks once. A file the CLI or Studio downloads
with its own HTTP client carries no mark, so SmartScreen does not ask. Smart App Control, where
a person has it turned on (Windows 11, new installs only), blocks unsigned executables
whatever their origin; only signing fixes that.

## Design

### The installer

Inno Setup, built by `script/bundle-windows` from `resources/windows/omnigraph-studio.iss`, beside
the zip it already builds:

- `AppId=com.modernrelay.omnigraph-studio`, the macOS bundle identifier. It never changes: it
  names the uninstall key and is how an installer recognises the version it upgrades.
- Per user, no administrator rights (`PrivilegesRequired=lowest`): Studio goes to
  `%LOCALAPPDATA%\Programs\Omnigraph Studio`, the Start menu entry to the user's Programs, and
  every registry value under `HKCU`. `/ALLUSERS` on the command line installs for every user
  instead (`PrivilegesRequiredOverridesAllowed=commandline`), into `Program Files` and `HKLM`,
  and asks for admin.
- 64-bit Windows 10 1809 or later (`MinVersion=10.0.17763`, `x64compatible`), as the zip.
- Installs `omnigraph-studio.exe`, `LICENSE` and `THIRD-PARTY-NOTICES.md`; a Start menu entry,
  and a desktop shortcut when the person ticks it (off by default). No licence page: the GPL
  asks for no acceptance, and the file is installed beside the executable.
- Running copies are closed through the Restart Manager before files are replaced
  (`CloseApplications=yes`), so an upgrade over a running Studio does not fail on a locked file.
- The last page offers to open Studio; a silent install never opens it.
- Uninstalling removes the installed files, the shortcuts and the registry keys it wrote, and
  leaves `%APPDATA%\Omnigraph Studio` (settings, connections, agent conversations): Studio
  writes that folder, the installer never did.
- File names: `omnigraph-studio-<version>-windows-x86_64-setup.exe`, and the version-free
  `omnigraph-studio-windows-x86_64-setup.exe` at `releases/latest/download/` (and at
  `releases/download/edge/` for an edge build). The zip stays, under both of its names, for
  people who want a copy without installing.
- Studio's **Download** on Windows opens the installer instead of the zip.

### What the CLI looks up

In this order, the first that exists:

1. `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\com.modernrelay.omnigraph-studio_is1`
2. `HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\com.modernrelay.omnigraph-studio_is1`
   (the 64-bit view; written only by an `/ALLUSERS` install)

Values: `InstallLocation` (the folder, with a trailing `\`), `DisplayVersion` (the version, as
`Cargo.toml` writes it), `Publisher` (`ModernRelay`), `DisplayName` (`Omnigraph Studio`),
`UninstallString`, `QuietUninstallString`. Studio is `InstallLocation` + `omnigraph-studio.exe`.

The installer also writes `…\CurrentVersion\App Paths\omnigraph-studio.exe` in the same hive,
whose default value is the executable's full path, so `start omnigraph-studio` and
`ShellExecute("omnigraph-studio.exe")` open it.

To install when neither key exists, the CLI downloads the version-free installer and runs it
with `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`, then reads the key again. Until Windows builds
are signed, the integrity check is the update key's minisign signature (below), or `SHA256SUMS`
before the key is published. ModernRelay/omnigraph#1017 owns that side; this plan fixes the
names it reads.

### Signatures

- While there is no certificate, the installer is unsigned and the release notes say what
  SmartScreen asks.
- Once `update-key.pub` holds the update key, the `sign-update` job signs the installer with it
  too (`omnigraph-studio-<version>-windows-x86_64-setup.exe.sig`), beside the macOS update, and
  the job checks both signatures against the committed key. That gives the CLI, and later
  Studio's updater, a check of the downloaded file that does not depend on Authenticode.
- When a certificate exists, `script/bundle-windows` signs the executable before packing and
  the installer after, through `signtool` (Inno's `SignTool` directive signs the uninstaller it
  writes). Nothing else changes: the `AppId`, the key and the file names stay.

### Steps

1. **Built here.** The installer in `script/bundle-windows`, uploaded by the release workflow,
   which installs it on the runner, checks the keys above and uninstalls it; the version-free
   copy; Download opens it; the release notes and README describe it; the installer is signed
   with the update key when the key is published.
2. **Studio updates itself on Windows.** A `windows-x86_64` entry in `latest.json` with
   `format: "inno"`, the release's installer and its `.sig`; Studio downloads it with its own
   client, checks the signature, and on Restart to Update quits and starts the installer with
   `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`, which reopens Studio when it is done (a
   `[Run]` entry for silent installs, or Studio's own relaunch). Only a Studio installed by the
   installer does this (its executable is under the key's `InstallLocation`); a copy from the
   zip keeps offering Download. Studios up to 0.4.1 ignore the entry (What exists today).
3. **Signing**, when there is a certificate: Authenticode on the executable, installer and
   uninstaller, and the release notes' SmartScreen paragraph removed.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Inno Setup | MSIX; MSI with WiX; NSIS, by hand or through `cargo-packager` | MSIX cannot install unsigned, changes identity with the certificate, cannot be replaced by Studio's own updater, and redirects `%APPDATA%`. MSI's identity is a GUID found through MSI's API, per-user MSIs are fragile, and current WiX carries a maintenance fee for a company. Inno is on the runner, free, installs per user by one directive, upgrades in place under a fixed `AppId` with the Restart Manager, and signs later without changing anything the CLI reads. NSIS would do the same with more script and is not on `windows-2025`; `cargo-packager` (spec 017's guess) downloads it at build time and names the key itself |
| `AppId` is the bundle identifier | A GUID | One name for Studio on every platform, readable in the registry; Inno takes any string |
| Per user by default, `/ALLUSERS` allowed | Per machine; per user only | No UAC prompt to install or update, which the CLI and the updater need; an administrator can still install for everyone |
| Keep the zip | Installer only | A copy that runs from anywhere without installing is still asked for; it costs one file |
| Uninstall leaves `%APPDATA%\Omnigraph Studio` | Delete it | It holds connections and conversations a reinstall should find; removing it is the person's call |
| The update key signs the installer | Wait for Authenticode | The key exists for this purpose already, and the CLI can check a minisign signature on every platform |
| No Studio change beyond Download | Studio installs Windows updates now | Step 2 needs the restart path tested on Windows; it reuses this step's installer and names unchanged |

## Deferred

- Step 2 (Studio updates itself on Windows) and step 3 (Authenticode), above.
- The executable's and installer's icon: the executable has no icon resource yet (spec 001).
- Windows on arm64: there is no arm64 Windows build.
- No Omnigraph engine change is needed; `docs/deferred-engine-changes.md` is unchanged.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-09 | Inno Setup is on `windows-2025`, NSIS is not | `actions/runner-images` `images/windows/Windows2025-Readme.md` and `Windows2022-Readme.md` | 2025: InnoSetup 6.7.1, WiX 3.14.1.8722, no NSIS. 2022: InnoSetup 6.7.1, NSIS 3.10, WiX 3.14.1.8722 |
| 2026-11-09 | Release files carry the installer under both names | `python3 script/test_release_files.py`; `cargo test -p studio_update --lib manifest` | 8 passed (a missing installer stops the release); 11 passed (Download opens the installer) |
| 2026-11-10 | The installer builds, installs per user, writes the keys the CLI reads, and uninstalls | Release workflow run by hand on this branch (run 42761573400, nothing published), job Build (windows-x86_64), step "The installer installs and registers" | Passed: `omnigraph-studio-0.4.1-windows-x86_64-setup.exe` built by Inno Setup 6.7.1 on `windows-2025`; silent install without admin; HKCU uninstall key with DisplayName `Omnigraph Studio`, DisplayVersion `0.4.1`, Publisher `ModernRelay`, InstallLocation under `%LOCALAPPDATA%\Programs\Omnigraph Studio`; App Paths key; no HKLM key; silent uninstall removed the files and both keys. The `windows-x86_64` artifact (installer and zip) is 91 MB |

## Corrections
