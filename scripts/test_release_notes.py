#!/usr/bin/env python3
"""Tests of scripts/release-notes.py (F12, criterion REL-02).

The shipped CHANGELOG.md must give the 0.2.0-alpha.1 notes, and a copy with each listed defect
must fail with one line that names the cause. Offline; run from the repository root:
python3 -I scripts/test_release_notes.py
"""

import importlib.util
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "release-notes.py"
CHANGELOG = ROOT / "CHANGELOG.md"
VERSION = "0.2.0-alpha.1"

SPEC = importlib.util.spec_from_file_location("release_notes", SCRIPT)
notes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(notes)

LINK = f"https://github.com/Thaliph/mock-server/blob/v{VERSION}/docs/SPEC.md#111-accepted-limits"
GOOD = f"""# Changelog

## [Unreleased]

## [{VERSION}] - 2026-11-11

### Features

- One (#1).

### Accepted limits

See {LINK}.

### Upgrade notes

- Fresh install.

## [0.1.0] - 2026-01-01

### Features

- Old.
"""


def fails(text, version=VERSION):
    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / "CHANGELOG.md"
        path.write_text(text, encoding="utf-8")
        return subprocess.run(
            [sys.executable, "-I", str(SCRIPT), version, "--changelog", str(path)],
            capture_output=True,
            text=True,
        )


class ReleaseNotes(unittest.TestCase):
    def test_rel_02_section_body_without_heading(self):
        out = notes.extract(GOOD, VERSION)
        self.assertTrue(out.startswith("### Features"))
        self.assertNotIn("0.1.0", out)
        self.assertNotIn("## [", out)

    def test_rel_02_cli_prints_body_and_exits_0(self):
        r = fails(GOOD)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(r.stdout.startswith("### Features"))

    def test_rel_02_missing_section(self):
        r = fails(GOOD, "0.3.0")
        self.assertEqual(r.returncode, 1)
        self.assertIn("no section [0.3.0]", r.stderr)
        self.assertEqual(len(r.stderr.strip().splitlines()), 1)

    def test_rel_02_missing_date(self):
        r = fails(GOOD.replace(f"[{VERSION}] - 2026-11-11", f"[{VERSION}]"))
        self.assertEqual(r.returncode, 1)
        self.assertIn("no date", r.stderr)

    def test_rel_02_bad_date(self):
        r = fails(GOOD.replace("2026-11-11", "11/11/2026"))
        self.assertEqual(r.returncode, 1)

    def test_rel_02_unreleased_not_first(self):
        r = fails(GOOD.replace("## [Unreleased]\n\n", ""))
        self.assertEqual(r.returncode, 1)
        self.assertIn("[Unreleased]", r.stderr)

    def test_rel_02_missing_subsection(self):
        r = fails(GOOD.replace("### Upgrade notes\n\n- Fresh install.\n", ""))
        self.assertEqual(r.returncode, 1)
        self.assertIn("subsections", r.stderr)

    def test_rel_02_subsections_out_of_order(self):
        r = fails(GOOD.replace("### Features", "### Zzz").replace("### Upgrade notes", "### Features").replace("### Zzz", "### Upgrade notes"))
        self.assertEqual(r.returncode, 1)

    def test_rel_02_missing_spec_link(self):
        r = fails(GOOD.replace(LINK, "the spec"))
        self.assertEqual(r.returncode, 1)
        self.assertIn("does not link", r.stderr)

    def test_rel_02_invalid_version(self):
        r = fails(GOOD, "latest")
        self.assertEqual(r.returncode, 1)

    def test_rel_02_duplicate_version_section(self):
        dup = GOOD + GOOD[GOOD.index(f"## [{VERSION}]") : GOOD.index("## [0.1.0]")]
        r = fails(dup)
        self.assertEqual(r.returncode, 1)
        self.assertIn("twice", r.stderr)
        self.assertEqual(len(r.stderr.strip().splitlines()), 1)

    def test_rel_02_heading_in_code_fence_is_ignored(self):
        fenced = GOOD.replace("- Fresh install.\n", "- Fresh install.\n\n```sh\n### Features\n## [9.9.9] - 2026-01-01\n```\n")
        r = fails(fenced)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("### Features\n## [9.9.9]", r.stdout)
        r = fails(GOOD.replace("### Upgrade notes", "```\n### Upgrade notes\n```"))
        self.assertEqual(r.returncode, 1)
        self.assertIn("subsections", r.stderr)

    def test_rel_02_each_feature_entry_names_its_issue(self):
        body = notes.extract(CHANGELOG.read_text(encoding="utf-8"), VERSION)
        features = body.split("### Accepted limits")[0].split("### Features")[1]
        entries = re.split(r"^- ", features, flags=re.M)[1:]
        self.assertGreaterEqual(len(entries), 13)
        for entry in entries:
            self.assertRegex(entry, r"#[0-9]+", f"entry without an issue: {entry[:40]!r}")

    def test_rel_02_shipped_changelog(self):
        text = CHANGELOG.read_text(encoding="utf-8")
        body = notes.extract(text, VERSION)
        features = body.split("### Accepted limits")[0]
        for n in ["F%d" % i for i in range(12) if i != 12]:
            self.assertRegex(features, r"\b%s\b" % n, f"Features has no entry for {n}")
        self.assertIn("Monitoring", features)
        for issue in ("#378", "#385"):
            self.assertIn(issue, features)
        limits = body.split("### Accepted limits")[1].split("### Upgrade notes")[0]
        self.assertIn("wiki/Security", limits)
        upgrade = body.split("### Upgrade notes")[1]
        for needle in ("v0.1.0", "--version", "v1alpha1", "roll back"):
            self.assertIn(needle, upgrade.replace("Roll back", "roll back"))
        self.assertEqual(
            len([s for s in re.findall(r"^## \[", text, re.M)]),
            len(set(re.findall(r"^## \[([^\]]+)\]", text, re.M))),
        )


if __name__ == "__main__":
    unittest.main()
