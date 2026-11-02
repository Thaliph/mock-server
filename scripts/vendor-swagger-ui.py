#!/usr/bin/env python3
"""Vendor the Swagger UI assets of the controller (#239, spec §7.5, criterion UI-13).

usage: vendor-swagger-ui.py [--check] [--tarball PATH] [--root PATH]

The package and its exact version come from crates/mimic-controller/ui/package.json and
package-lock.json. The locked tarball is downloaded from the npm registry (or read from --tarball,
a fixture for tests) and its bytes must match the lockfile's sha512 integrity. Only the FILES below
are read from it, straight from the archive: nothing else is extracted. They are written to
crates/mimic-controller/ui/vendor/swagger-ui/ with SHA256SUMS (sha256sum format). Any other file in
that directory is removed.

--check writes nothing: it builds the same files in memory and fails unless the committed directory
holds exactly those names with byte-identical contents. A Dependabot bump therefore fails until the
files are vendored again here and the diff is reviewed.

Exit 0 when done or when --check matches; 1 on a mismatch, a failed download, or a tarball that
does not match the lockfile or lacks a file; 2 on a lockfile that does not pin the package.
"""

import argparse
import base64
import hashlib
import io
import json
import re
import shutil
import sys
import tarfile
import urllib.error
import urllib.request
import zlib
from pathlib import Path

PACKAGE = "swagger-ui-dist"
FILES = (
    "LICENSE",
    "NOTICE",
    "swagger-ui-bundle.js",
    "swagger-ui-bundle.js.LICENSE.txt",
    "swagger-ui.css",
)
SUMS = "SHA256SUMS"
REGISTRY = "https://registry.npmjs.org"
ROOT = Path(__file__).resolve().parent.parent
UI_DIR = Path("crates/mimic-controller/ui")
VENDOR_DIR = UI_DIR / "vendor" / "swagger-ui"
MAX_TARBALL = 32 * 1024 * 1024
USER_AGENT = "mimic-vendor-swagger-ui (https://github.com/Thaliph/mock-server)"
EXACT_VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
SHA512_INTEGRITY = re.compile(r"^sha512-[A-Za-z0-9+/]{86}==$")


class LockError(Exception):
    """package.json or package-lock.json does not pin the package the way this script expects."""


class VerifyError(Exception):
    """The tarball does not match the lockfile, or lacks a file the UI needs."""


def locked(ui):
    """(version, tarball URL, sha512 integrity) of the package, from the two JSON files of <ui>."""
    manifest = json.loads((ui / "package.json").read_text(encoding="utf-8"))
    lock = json.loads((ui / "package-lock.json").read_text(encoding="utf-8"))
    declared = manifest.get("dependencies", {}).get(PACKAGE)
    entry = lock.get("packages", {}).get(f"node_modules/{PACKAGE}")
    if declared is None or not isinstance(entry, dict):
        raise LockError(f"{PACKAGE} is not pinned in package.json and package-lock.json")
    version = entry.get("version", "")
    if not EXACT_VERSION.match(version) or declared != version:
        raise LockError(
            f"{PACKAGE} must have one exact version in both files "
            f"(package.json {declared!r}, lockfile {version!r})"
        )
    url = f"{REGISTRY}/{PACKAGE}/-/{PACKAGE}-{version}.tgz"
    if entry.get("resolved") != url:
        raise LockError(f"{PACKAGE} must resolve to {url} in package-lock.json")
    integrity = entry.get("integrity", "")
    if not SHA512_INTEGRITY.match(integrity):
        raise LockError(f"{PACKAGE} needs a sha512 integrity in package-lock.json")
    return version, url, integrity


def download(url):
    """The bytes at <url>, at most MAX_TARBALL of them."""
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(MAX_TARBALL + 1)
    if len(data) > MAX_TARBALL:
        raise VerifyError(f"{url} is larger than {MAX_TARBALL} bytes")
    return data


def check_integrity(data, integrity):
    """Refuses bytes whose sha512 is not the lockfile's integrity value."""
    digest = base64.b64encode(hashlib.sha512(data).digest()).decode("ascii")
    if f"sha512-{digest}" != integrity:
        raise VerifyError("the tarball does not match the sha512 integrity of package-lock.json")


def extract(data):
    """{name: bytes} of FILES, read from package/<name> of the tarball."""
    files = {}
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            for name in FILES:
                try:
                    member = tar.getmember(f"package/{name}")
                except KeyError:
                    raise VerifyError(f"the tarball has no package/{name}") from None
                if not member.isfile():
                    raise VerifyError(f"package/{name} in the tarball is not a regular file")
                files[name] = tar.extractfile(member).read()
    except (tarfile.TarError, EOFError, OSError, zlib.error) as error:
        raise VerifyError(f"the tarball is not a readable gzip tar: {error}") from None
    return files


def render(files):
    """{name: bytes} of the vendored files, SHA256SUMS included."""
    sums = "".join(f"{hashlib.sha256(files[name]).hexdigest()}  {name}\n" for name in sorted(FILES))
    return {**files, SUMS: sums.encode("ascii")}


def write(target, outputs):
    target.mkdir(parents=True, exist_ok=True)
    for name, data in outputs.items():
        (target / name).write_bytes(data)


def prune(target, keep):
    """Removes every entry of <target> whose name is not in <keep>."""
    if not target.is_dir():
        return
    for path in target.iterdir():
        if path.name in keep:
            continue
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink()


def compare(target, outputs):
    """Prints every difference between <target> and <outputs>; True when there is none."""
    committed = {path.name for path in target.iterdir()} if target.is_dir() else set()
    problems = []
    for name in sorted(committed - set(outputs)):
        problems.append(f"unexpected {name}")
    for name in sorted(outputs):
        path = target / name
        if name not in committed:
            problems.append(f"missing {name}")
        elif path.is_symlink() or not path.is_file():
            problems.append(f"{name} is not a regular file")
        elif path.read_bytes() != outputs[name]:
            problems.append(f"{name} differs from the locked tarball")
    for problem in problems:
        print(f"vendor-swagger-ui: {problem}", file=sys.stderr)
    return not problems


def main(argv):
    parser = argparse.ArgumentParser(description="Vendor Swagger UI from the locked npm tarball.")
    parser.add_argument("--check", action="store_true",
                        help="fail unless the committed vendored files are what the lockfile yields")
    parser.add_argument("--tarball", type=Path,
                        help="read this tarball instead of downloading the locked one (tests); "
                             "its sha512 is still checked against the lockfile")
    parser.add_argument("--root", type=Path, default=ROOT, help="repository root (tests)")
    args = parser.parse_args(argv)

    ui = args.root / UI_DIR
    vendor = args.root / VENDOR_DIR
    try:
        version, url, integrity = locked(ui)
    except (LockError, OSError, ValueError) as error:
        print(f"vendor-swagger-ui: {error}", file=sys.stderr)
        return 2

    try:
        data = args.tarball.read_bytes() if args.tarball else download(url)
        check_integrity(data, integrity)
        outputs = render(extract(data))
    except (VerifyError, urllib.error.URLError, OSError) as error:
        print(f"vendor-swagger-ui: {error}", file=sys.stderr)
        return 1

    if args.check:
        if compare(vendor, outputs):
            print(f"vendor-swagger-ui: {vendor} matches {PACKAGE} {version} ({len(outputs)} files)")
            return 0
        print("vendor-swagger-ui: run scripts/vendor-swagger-ui.py and review the diff", file=sys.stderr)
        return 1

    prune(vendor, set(outputs))
    write(vendor, outputs)
    print(f"vendor-swagger-ui: wrote {len(outputs)} files of {PACKAGE} {version} to {vendor}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
