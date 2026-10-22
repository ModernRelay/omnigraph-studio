#!/usr/bin/env python3
"""Tests for script/release-files.py: the version-free copies and latest.json a release
publishes (spec 017). Run: python3 script/test_release_files.py"""
import importlib.util
import json
import pathlib
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("release_files", HERE / "release-files.py")
release_files = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_files)

FILES = [
    "Omnigraph-Studio-0.3.0-arm64.dmg",
    "Omnigraph-Studio-0.3.0-arm64.app.tar.gz",
    "omnigraph-studio-0.3.0-linux-x86_64.tar.gz",
    "omnigraph-studio-0.3.0-linux-aarch64.tar.gz",
    "omnigraph-studio-0.3.0-windows-x86_64.zip",
]


class ReleaseFiles(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.dist = pathlib.Path(self.dir.name)
        for name in FILES:
            (self.dist / name).write_bytes(name.encode())

    def tearDown(self):
        self.dir.cleanup()

    def test_each_first_install_file_gets_a_version_free_copy(self):
        release_files.stable_copies(self.dist, "0.3.0")
        for stable, versioned in [
            ("Omnigraph-Studio-arm64.dmg", "Omnigraph-Studio-0.3.0-arm64.dmg"),
            ("omnigraph-studio-linux-x86_64.tar.gz", "omnigraph-studio-0.3.0-linux-x86_64.tar.gz"),
            ("omnigraph-studio-linux-aarch64.tar.gz", "omnigraph-studio-0.3.0-linux-aarch64.tar.gz"),
            ("omnigraph-studio-windows-x86_64.zip", "omnigraph-studio-0.3.0-windows-x86_64.zip"),
        ]:
            self.assertEqual((self.dist / stable).read_bytes(), versioned.encode(), stable)
        self.assertFalse((self.dist / "Omnigraph-Studio-arm64.app.tar.gz").exists(),
                         "the update file is found through latest.json, not by name")

    def test_a_missing_first_install_file_stops_the_release(self):
        (self.dist / "omnigraph-studio-0.3.0-windows-x86_64.zip").unlink()
        with self.assertRaises(SystemExit):
            release_files.stable_copies(self.dist, "0.3.0")

    def test_a_signed_update_is_offered_to_macos(self):
        (self.dist / "Omnigraph-Studio-0.3.0-arm64.app.tar.gz.sig").write_text("c2lnbmF0dXJl")
        manifest = release_files.manifest(self.dist, "0.3.0", "ModernRelay/omnigraph-studio",
                                          "2026-11-02T10:00:00Z")
        self.assertEqual(manifest["version"], "0.3.0")
        self.assertEqual(manifest["pub_date"], "2026-11-02T10:00:00Z")
        self.assertEqual(manifest["notes"],
                         "https://github.com/ModernRelay/omnigraph-studio/releases/tag/v0.3.0")
        self.assertEqual(manifest["platforms"], {
            "macos-aarch64": {
                "url": "https://github.com/ModernRelay/omnigraph-studio/releases/download/"
                       "v0.3.0/Omnigraph-Studio-0.3.0-arm64.app.tar.gz",
                "signature": "c2lnbmF0dXJl",
                "format": "app",
            }
        })

    def test_without_its_signature_the_update_is_not_offered(self):
        manifest = release_files.manifest(self.dist, "0.3.0", "ModernRelay/omnigraph-studio",
                                          "2026-11-02T10:00:00Z")
        self.assertEqual(manifest["platforms"], {})
        self.assertEqual(manifest["version"], "0.3.0", "Linux and Windows still learn of it")

    def test_the_manifest_studio_reads_is_this_scripts(self):
        (self.dist / "Omnigraph-Studio-0.3.0-arm64.app.tar.gz.sig").write_text("c2lnbmF0dXJl")
        manifest = release_files.manifest(self.dist, "0.3.0", "ModernRelay/omnigraph-studio",
                                          "2026-11-02T10:00:00Z")
        fixture = HERE.parent / "crates/studio_update/tests/fixtures/latest.json"
        self.assertEqual(manifest, json.loads(fixture.read_text()),
                         "Studio's parse test reads this fixture")


if __name__ == "__main__":
    unittest.main()
