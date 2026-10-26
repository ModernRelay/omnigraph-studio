#!/usr/bin/env python3
"""Add a release's version-free copies and its update manifest to its files (spec 017).

    script/release-files.py dist 0.3.0 crates/studio_update/update-key.pub

`releases/latest/download/<name>` then always serves the newest first-install file, and
`latest.json` tells Studio's updater the newest version and, for macOS, the signed update
(`Omnigraph-Studio-<version>-arm64.app.tar.gz` with its `.sig`, which `cargo packager signer
sign` writes as base64). While the committed update key is empty no platform is offered an
update to install, and Studio offers the download instead; once it holds the key, a release
without the `.sig` stops here, since every Studio would otherwise stop updating without a
word. GITHUB_REPOSITORY names the repository.
"""
import datetime
import json
import os
import pathlib
import shutil
import sys

# Version-free name, then the release's own; `{v}` is the version.
FIRST_INSTALL = [
    ("Omnigraph-Studio-arm64.dmg", "Omnigraph-Studio-{v}-arm64.dmg"),
    ("omnigraph-studio-linux-x86_64.tar.gz", "omnigraph-studio-{v}-linux-x86_64.tar.gz"),
    ("omnigraph-studio-linux-aarch64.tar.gz", "omnigraph-studio-{v}-linux-aarch64.tar.gz"),
    ("omnigraph-studio-windows-x86_64.zip", "omnigraph-studio-{v}-windows-x86_64.zip"),
]

# The updater's platform key, its update file, and the crate's format for it.
UPDATES = [
    ("macos-aarch64", "Omnigraph-Studio-{v}-arm64.app.tar.gz", "app"),
]


def stable_copies(dist, version):
    for stable, versioned in FIRST_INSTALL:
        source = dist / versioned.format(v=version)
        if not source.is_file():
            sys.exit(f"release-files: {source.name} is missing; every release carries it")
        shutil.copyfile(source, dist / stable)


def manifest(dist, version, repository, date, keyed=False):
    """The manifest for the files in `dist`; `keyed` when the update key is committed, which
    makes every update's signature required."""
    releases = f"https://github.com/{repository}/releases"
    platforms = {}
    for key, name, kind in UPDATES:
        update = dist / name.format(v=version)
        signature = update.with_name(update.name + ".sig")
        if update.is_file() and signature.is_file():
            platforms[key] = {
                "url": f"{releases}/download/v{version}/{update.name}",
                "signature": signature.read_text().strip(),
                "format": kind,
            }
        elif keyed:
            sys.exit(f"release-files: the update key is committed, but {signature.name} or its "
                     "update is missing; every Studio would refuse or miss this release")
    return {
        "version": version,
        "pub_date": date,
        "platforms": platforms,
    }


def main():
    if len(sys.argv) != 4:
        sys.exit("usage: script/release-files.py <dist> <version> <update-key.pub>")
    dist, version = pathlib.Path(sys.argv[1]), sys.argv[2]
    keyed = bool(pathlib.Path(sys.argv[3]).read_text().strip())
    repository = os.environ.get("GITHUB_REPOSITORY", "ModernRelay/omnigraph-studio")
    date = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    stable_copies(dist, version)
    written = manifest(dist, version, repository, date, keyed)
    (dist / "latest.json").write_text(json.dumps(written, indent=2) + "\n")
    offered = ", ".join(written["platforms"]) or "no platform"
    print(f"latest.json: {version}, an update to install for {offered}")


if __name__ == "__main__":
    main()
