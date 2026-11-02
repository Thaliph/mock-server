#!/usr/bin/env python3
"""Offline tests of the npm side of scripts/check-cooldown.py (#239, criterion UI-13).

The npm registry is stubbed (check-cooldown's fetch_json is replaced), so no network is needed.
The base commit is HEAD, a local git object. Run from the repository root:
python3 -I scripts/test_check_cooldown.py
"""

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).resolve().with_name("check-cooldown.py")
SPEC = importlib.util.spec_from_file_location("check_cooldown", SCRIPT)
cooldown = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cooldown)

VERSION = "5.34.0"
PUBLISHED = "2026-10-19T12:18:04.849Z"
REGISTRY_URL = "https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-5.34.0.tgz"


def lock_text(packages):
    """A package-lock.json text with the given packages map (the root entry is always present)."""
    return json.dumps({"lockfileVersion": 3, "packages": {"": {"name": "fixture"}, **packages}})


def swagger_entry(**overrides):
    entry = {"version": VERSION, "resolved": REGISTRY_URL, "integrity": "sha512-x"}
    entry.update(overrides)
    return entry


class Ui13NpmCooldown(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def lockfile(self, text):
        path = self.dir / "package-lock.json"
        path.write_text(text)
        return path

    def run_check(self, text, now="2026-10-30T00:00:00Z", allowed=None):
        """Exit status of check-cooldown on a fixture lockfile, with the registry stubbed."""
        path = self.lockfile(text)
        registry = mock.Mock(return_value={"time": {VERSION: PUBLISHED}})
        with mock.patch.object(cooldown, "fetch_json", registry), \
                mock.patch.object(cooldown, "load_allowlist", return_value=allowed or {}), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return cooldown.main(["HEAD", str(path), "--now", now])

    def test_ui_13_npm_version_younger_than_14_days_is_refused(self):
        text = lock_text({"node_modules/swagger-ui-dist": swagger_entry()})
        self.assertEqual(self.run_check(text, now="2026-10-30T00:00:00Z"), 1)

    def test_ui_13_npm_version_older_than_14_days_passes(self):
        text = lock_text({"node_modules/swagger-ui-dist": swagger_entry()})
        self.assertEqual(self.run_check(text, now="2026-11-03T00:00:00Z"), 0)

    def test_ui_13_npm_version_named_by_the_allowlist_passes(self):
        text = lock_text({"node_modules/swagger-ui-dist": swagger_entry()})
        allowed = {("swagger-ui-dist", VERSION): "fixture reason"}
        self.assertEqual(self.run_check(text, now="2026-10-30T00:00:00Z", allowed=allowed), 0)

    def test_ui_13_unreadable_npm_registry_fails_closed(self):
        text = lock_text({"node_modules/swagger-ui-dist": swagger_entry()})
        path = self.lockfile(text)
        registry = mock.Mock(side_effect=urllib.error.URLError("fixture: registry down"))
        with mock.patch.object(cooldown, "fetch_json", registry), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(cooldown.main(["HEAD", str(path), "--now", "2026-11-03T00:00:00Z"]), 1)

    def test_ui_13_npm_entry_without_resolved_is_refused(self):
        entry = swagger_entry()
        del entry["resolved"]
        self.assertEqual(self.run_check(lock_text({"node_modules/swagger-ui-dist": entry})), 1)

    def test_ui_13_npm_entry_resolved_outside_the_npm_registry_is_refused(self):
        entry = swagger_entry(resolved="https://example.invalid/swagger-ui-dist-5.34.0.tgz")
        self.assertEqual(self.run_check(lock_text({"node_modules/swagger-ui-dist": entry})), 1)

    def test_ui_13_lockfile_without_a_packages_map_is_refused(self):
        self.assertEqual(self.run_check(json.dumps({"lockfileVersion": 1, "dependencies": {}})), 1)


if __name__ == "__main__":
    unittest.main()
