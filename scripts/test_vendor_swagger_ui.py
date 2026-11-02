#!/usr/bin/env python3
"""Offline tests of scripts/vendor-swagger-ui.py (#239, criterion UI-13).

Each test builds a fixture repository and a fixture tarball in a temporary directory, so no
network is needed. Run: python3 -I scripts/test_vendor_swagger_ui.py
"""

import base64
import contextlib
import hashlib
import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("vendor-swagger-ui.py")
SPEC = importlib.util.spec_from_file_location("vendor_swagger_ui", SCRIPT)
vendor = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(vendor)

VERSION = "9.9.9"
UI_DIR = "crates/mimic-controller/ui"


def tarball_bytes(extra=None, drop=()):
    """A gzip tar with package/<file> for each vendored file, minus <drop>, plus one file the UI does not need."""
    members = {name: f"fixture {name}\n".encode() for name in vendor.FILES if name not in drop}
    members["index.html"] = b"<!doctype html>\n"
    members.update(extra or {})
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz") as tar:
        for name, data in sorted(members.items()):
            info = tarfile.TarInfo(f"package/{name}")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return out.getvalue()


def integrity_of(data):
    return "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode("ascii")


class VendorFixture(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.ui = self.root / UI_DIR
        self.ui.mkdir(parents=True)
        self.tarball = self.root / "fixture.tgz"
        self.data = tarball_bytes()
        self.tarball.write_bytes(self.data)
        self.write_lock(self.data)

    def tearDown(self):
        self._tmp.cleanup()

    def write_lock(self, data, version=VERSION, declared=VERSION):
        url = f"https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-{version}.tgz"
        (self.ui / "package.json").write_text(json.dumps(
            {"name": "fixture", "private": True, "dependencies": {"swagger-ui-dist": declared}}))
        (self.ui / "package-lock.json").write_text(json.dumps({
            "lockfileVersion": 3,
            "packages": {
                "": {"dependencies": {"swagger-ui-dist": declared}},
                "node_modules/swagger-ui-dist": {
                    "version": version, "resolved": url, "integrity": integrity_of(data)},
            },
        }))

    def run_vendor(self, *args):
        """Exit status of the script on the fixture repository; its output is discarded."""
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return vendor.main(["--root", str(self.root), "--tarball", str(self.tarball), *args])

    @property
    def vendor_dir(self):
        return self.root / vendor.VENDOR_DIR


class Ui13Vendoring(VendorFixture):
    def test_ui_13_vendoring_copies_only_the_ui_files_and_writes_sha256sums(self):
        self.assertEqual(self.run_vendor(), 0)
        self.assertEqual(
            sorted(p.name for p in self.vendor_dir.iterdir()),
            sorted([*vendor.FILES, vendor.SUMS]),
        )
        for name in vendor.FILES:
            self.assertEqual((self.vendor_dir / name).read_bytes(), f"fixture {name}\n".encode())
        sums = (self.vendor_dir / vendor.SUMS).read_text()
        for name in vendor.FILES:
            digest = hashlib.sha256(f"fixture {name}\n".encode()).hexdigest()
            self.assertIn(f"{digest}  {name}\n", sums)

    def test_ui_13_check_passes_on_a_fresh_vendoring(self):
        self.assertEqual(self.run_vendor(), 0)
        self.assertEqual(self.run_vendor("--check"), 0)

    def test_ui_13_check_fails_on_one_changed_byte(self):
        self.assertEqual(self.run_vendor(), 0)
        path = self.vendor_dir / "swagger-ui.css"
        path.write_bytes(path.read_bytes() + b" ")
        self.assertEqual(self.run_vendor("--check"), 1)

    def test_ui_13_check_fails_on_an_extra_or_a_missing_file(self):
        self.assertEqual(self.run_vendor(), 0)
        (self.vendor_dir / "index.html").write_bytes(b"extra")
        self.assertEqual(self.run_vendor("--check"), 1)
        (self.vendor_dir / "index.html").unlink()
        (self.vendor_dir / "NOTICE").unlink()
        self.assertEqual(self.run_vendor("--check"), 1)

    def test_ui_13_tarball_not_matching_the_lock_sha512_is_refused(self):
        self.assertEqual(self.run_vendor(), 0)
        tampered = tarball_bytes({"swagger-ui.css": b"tampered"})
        self.tarball.write_bytes(tampered)
        self.assertEqual(self.run_vendor(), 1)
        self.assertEqual(self.run_vendor("--check"), 1)
        self.assertEqual((self.vendor_dir / "swagger-ui.css").read_bytes(), b"fixture swagger-ui.css\n")

    def test_ui_13_tarball_without_a_needed_file_is_refused(self):
        data = tarball_bytes(drop=("NOTICE",))
        self.tarball.write_bytes(data)
        self.write_lock(data)
        self.assertEqual(self.run_vendor(), 1)
        self.assertFalse(self.vendor_dir.exists())

    def test_ui_13_lock_must_pin_one_exact_version(self):
        self.write_lock(self.data, version=VERSION, declared="^9.9.9")
        self.assertEqual(self.run_vendor("--check"), 2)
        self.write_lock(self.data, version="9.9.9-beta.1", declared="9.9.9-beta.1")
        self.assertEqual(self.run_vendor("--check"), 2)


if __name__ == "__main__":
    unittest.main()
