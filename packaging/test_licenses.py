#!/usr/bin/env python3
"""Tests of licenses.py with made-up `cargo metadata`: a crate's licence files
are files of its own package (20th assessment, R23-23: a `license_file` of
`../../outside.txt` copied a file from outside the crate into the notices).

    python3 packaging/test_licenses.py
"""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# The scripts it loads leave no __pycache__ in the repository.
sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("phv_licenses", HERE / "licenses.py")
licenses = importlib.util.module_from_spec(spec)
spec.loader.exec_module(licenses)


class Metadata:
    def __init__(self, packages):
        root = {"id": "root", "name": "phv_cli", "version": "1.0.0", "source": None, "license": "MIT",
                "manifest_path": "Cargo.toml"}
        meta = {
            "packages": [root, *packages],
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": p["id"], "dep_kinds": [{"kind": None}]} for p in packages]},
                *({"id": p["id"], "deps": []} for p in packages),
            ]},
        }
        self.stdout = json.dumps(meta).encode()


def crate(base: Path, name: str, license_file=None, licence="MIT") -> dict:
    folder = base / "registry" / f"{name}-0.1.0"
    folder.mkdir(parents=True)
    (folder / "Cargo.toml").write_text(f"[package]\nname = '{name}'\n", encoding="utf-8")
    p = {"id": name, "name": name, "version": "0.1.0", "license": licence,
         "source": "registry+https://github.com/rust-lang/crates.io-index",
         "manifest_path": str(folder / "Cargo.toml")}
    if license_file is not None:
        p["license_file"] = license_file
    return p


def run(packages, out: Path):
    with mock.patch.object(subprocess, "run", return_value=Metadata(packages)), \
            mock.patch.object(sys, "argv", ["licenses.py", "x86_64-unknown-linux-musl", str(out)]), \
            mock.patch("sys.stderr"), mock.patch("sys.stdout"):
        return licenses.main()


class LicenceFiles(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.base = Path(self.tmp.name)
        (self.base / "outside-secret.txt").write_text("mk1 file outside the crate\n", encoding="utf-8")
        self.out = self.base / "notices.txt"

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_license_file_inside_the_crate_is_read(self):
        p = crate(self.base, "good", "docs/TERMS.txt")
        terms = Path(p["manifest_path"]).parent / "docs" / "TERMS.txt"
        terms.parent.mkdir()
        terms.write_text("the good terms\n", encoding="utf-8")
        self.assertEqual(run([p], self.out), 0)
        text = self.out.read_text(encoding="utf-8")
        self.assertIn("good 0.1.0: docs/TERMS.txt", text)
        self.assertIn("the good terms", text)

    def test_a_license_file_outside_the_crate_is_not_read(self):
        for path in ["../../outside-secret.txt", "docs/../../../outside-secret.txt",
                     str(self.base / "outside-secret.txt")]:
            with self.subTest(path=path):
                p = crate(self.base, f"evil{abs(hash(path))}", path)
                # The crate has no licence file of its own: it fails, and nothing outside is read.
                self.assertEqual(run([p], self.out), 1)
                self.assertFalse(self.out.exists() and "mk1" in self.out.read_text(encoding="utf-8"))
                good = crate(self.base, f"withown{abs(hash(path))}", path)
                (Path(good["manifest_path"]).parent / "LICENSE").write_text("own licence\n", encoding="utf-8")
                self.assertEqual(run([good], self.out), 0)
                text = self.out.read_text(encoding="utf-8")
                self.assertIn("own licence", text)
                self.assertNotIn("mk1", text)

    def test_links_in_the_crate_are_not_read(self):
        p = crate(self.base, "linked", "TERMS.txt")
        folder = Path(p["manifest_path"]).parent
        try:
            os.symlink(self.base / "outside-secret.txt", folder / "LICENSE")
            os.symlink(self.base / "outside-secret.txt", folder / "TERMS.txt")
        except (OSError, NotImplementedError) as e:
            self.skipTest(f"no symbolic links here: {e}")
        (folder / "COPYING").write_text("own licence\n", encoding="utf-8")
        self.assertEqual(run([p], self.out), 0)
        text = self.out.read_text(encoding="utf-8")
        self.assertIn("own licence", text)
        self.assertNotIn("mk1", text)


if __name__ == "__main__":
    unittest.main()
