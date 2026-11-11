#!/usr/bin/env python3
"""Tests of how compare_php.py finds `--php` and `--phv` (20th assessment,
R23-24: on Windows a `php.bat` in the current folder ran in place of PHP).

    python3 bench/test_compare_php.py
"""

import importlib.util
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# The scripts it loads leave no __pycache__ in the repository.
sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location("compare_php", HERE / "compare_php.py")
compare_php = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare_php)

NAME = "php.bat" if os.name == "nt" else "php"


def executable(path: Path):
    path.write_text("@echo off\n" if os.name == "nt" else "#!/bin/sh\n", encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class FindingPrograms(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.cwd, self.bin, self.empty = base / "cwd", base / "bin", base / "empty"
        for d in (self.cwd, self.bin, self.empty):
            d.mkdir()
        executable(self.cwd / NAME)
        self.old = os.getcwd()
        os.chdir(self.cwd)

    def tearDown(self):
        os.chdir(self.old)
        self.tmp.cleanup()

    def env(self, path: str):
        return mock.patch.dict(os.environ, {"PATH": path, "NoDefaultCurrentDirectoryInExePath": ""})

    def test_a_bare_name_is_not_taken_from_the_current_folder(self):
        with self.env(str(self.empty)):
            self.assertIsNone(compare_php.program("php"))
        # A relative folder in PATH (".", "") is not searched either.
        with self.env(os.pathsep.join([".", "", str(self.empty)])):
            self.assertIsNone(compare_php.program("php"))

    def test_a_bare_name_is_found_in_path(self):
        executable(self.bin / NAME)
        with self.env(os.pathsep.join([str(self.empty), str(self.bin)])):
            found = compare_php.program("php")
        self.assertIsNotNone(found)
        self.assertEqual(Path(found), (self.bin / NAME).resolve())

    def test_a_path_is_taken_as_given(self):
        with self.env(str(self.empty)):
            for given in [os.path.join(".", NAME), str(self.cwd / NAME)]:
                found = compare_php.program(given)
                self.assertIsNotNone(found, given)
                self.assertEqual(Path(found), (self.cwd / NAME).resolve())
            self.assertIsNone(compare_php.program(os.path.join(".", "missing")))


if __name__ == "__main__":
    unittest.main()
