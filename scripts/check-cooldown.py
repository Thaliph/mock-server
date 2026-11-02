#!/usr/bin/env python3
"""Cooldown for new registry releases (#269, spec FND-10; the npm lockfile of #239, UI-13).

A crate version that a change adds or bumps in a Cargo.lock must have been published at least
COOLDOWN_DAYS days before the check, unless scripts/cooldown-allowlist.toml lists it with a reason.
The publication date is the `created_at` of the version in the crates.io API. The same holds for
an npm package version that a change adds or bumps in a package-lock.json (the vendored Swagger UI,
crates/mimic-controller/ui): its date is the `time` of that version in the npm registry. Only
registry packages count: workspace members have no source, and git sources are refused by
cargo-deny. An npm lockfile is held to a stricter rule: every entry must be an installed package
that resolves to the npm registry with a version, and the lockfile must have a `packages` map;
anything else is an error (fail closed), never a silent skip.

usage: check-cooldown.py <base-commit> <lockfile>...

  A file named package-lock.json is read as npm, any other as Cargo.lock. Each lockfile is
  compared with its content at <base-commit> (a lockfile absent there counts as empty).
  Environment: GITHUB_STEP_SUMMARY, when set, receives a Markdown table.
  --now YYYY-MM-DDTHH:MM:SSZ fixes the clock (tests).

Exit 0 when every new version is old enough or allowed; 1 when one is not, or when the registry
cannot be asked (fails closed); 2 on a usage error or an invalid allowlist.
"""

import argparse
import json
import os
import subprocess
import sys
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

COOLDOWN = timedelta(days=14)
REGISTRY_PREFIX = "registry+"
NPM_REGISTRY_PREFIX = "https://registry.npmjs.org/"
ALLOWLIST = Path(__file__).resolve().with_name("cooldown-allowlist.toml")
API = "https://crates.io/api/v1/crates/{name}/{version}"
NPM_API = "https://registry.npmjs.org/{name}"
USER_AGENT = "mimic-cooldown-check (https://github.com/Thaliph/mock-server)"
CRATES, NPM = "crates.io", "npm"


def registry_packages(text):
    """The (crates.io, name, version) triples of the registry packages of a Cargo.lock text."""
    doc = tomllib.loads(text)
    return {
        (CRATES, pkg["name"], pkg["version"])
        for pkg in doc.get("package", [])
        if str(pkg.get("source", "")).startswith(REGISTRY_PREFIX)
    }


def npm_packages(text):
    """The (npm, name, version) triples of the package-lock.json entries that come from the npm registry."""
    doc = json.loads(text)
    entries = doc.get("packages") if isinstance(doc, dict) else None
    if not isinstance(entries, dict):
        raise ValueError("package-lock.json has no packages map (lockfileVersion 2 or 3 is needed)")
    found = set()
    for key, entry in entries.items():
        if key == "":
            continue  # the root project, not a dependency
        if not isinstance(entry, dict) or "node_modules/" not in key:
            raise ValueError(f"package-lock.json entry {key!r} is not an installed npm package")
        resolved = entry.get("resolved")
        if not isinstance(resolved, str) or not resolved.startswith(NPM_REGISTRY_PREFIX):
            raise ValueError(f"package-lock.json entry {key!r} does not resolve to the npm registry "
                             f"(resolved: {resolved!r})")
        version = entry.get("version")
        if not isinstance(version, str) or not version:
            raise ValueError(f"package-lock.json entry {key!r} has no version")
        found.add((NPM, key.rsplit("node_modules/", 1)[1], version))
    return found


def packages(lockfile, text):
    """The registry packages of a lockfile text: npm for a package-lock.json, crates.io otherwise."""
    return npm_packages(text) if Path(lockfile).name == "package-lock.json" else registry_packages(text)


def base_packages(base, lockfile):
    """Registry packages of <lockfile> at <base>; empty when the file does not exist there."""
    spec = f"{base}:{lockfile}"
    if subprocess.run(["git", "cat-file", "-e", spec], capture_output=True).returncode != 0:
        return set()
    out = subprocess.run(["git", "show", spec], capture_output=True, text=True, check=True)
    return packages(lockfile, out.stdout)


def load_allowlist(path):
    """{(name, version): reason}; every entry needs a name, a version and a non-empty reason."""
    if not path.exists():
        return {}
    allowed = {}
    for entry in tomllib.loads(path.read_text()).get("allow", []):
        name = str(entry.get("name", "")).strip()
        version = str(entry.get("version", "")).strip()
        reason = str(entry.get("reason", "")).strip()
        if not (name and version and reason):
            raise ValueError(f"allowlist entry needs name, version and reason: {entry}")
        allowed[(name, version)] = reason
    return allowed


def fetch_json(url):
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.load(response)


def published_crate(name, version):
    """The crates.io publication time of one version (timezone-aware)."""
    url = API.format(name=urllib.parse.quote(name, safe=""), version=urllib.parse.quote(version, safe=""))
    data = fetch_json(url)
    return datetime.fromisoformat(data["version"]["created_at"].replace("Z", "+00:00"))


def published_npm(name, version):
    """The npm registry publication time of one version: the `time` map of the package document."""
    data = fetch_json(NPM_API.format(name=urllib.parse.quote(name, safe="")))
    return datetime.fromisoformat(data["time"][version].replace("Z", "+00:00"))


def published_at(kind, name, version):
    return published_npm(name, version) if kind == NPM else published_crate(name, version)


def main(argv):
    parser = argparse.ArgumentParser(description="Refuse package versions published less than 14 days ago.")
    parser.add_argument("base", help="commit whose lockfiles the change is compared with")
    parser.add_argument("lockfiles", nargs="+")
    parser.add_argument("--now", help="fixed clock, YYYY-MM-DDTHH:MM:SSZ (tests)")
    args = parser.parse_args(argv)

    if subprocess.run(["git", "cat-file", "-e", f"{args.base}^{{commit}}"], capture_output=True).returncode != 0:
        print(f"unknown base commit: {args.base}", file=sys.stderr)
        return 2
    now = datetime.strptime(args.now, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc) if args.now \
        else datetime.now(timezone.utc)
    try:
        allowed = load_allowlist(ALLOWLIST)
    except (ValueError, tomllib.TOMLDecodeError) as error:
        print(f"invalid {ALLOWLIST.name}: {error}", file=sys.stderr)
        return 2

    new = set()
    try:
        for lockfile in args.lockfiles:
            head = packages(lockfile, Path(lockfile).read_text())
            new |= head - base_packages(args.base, lockfile)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"cannot read the lockfiles, failing closed: {error}", file=sys.stderr)
        return 1

    rows, failures = [], []
    for kind, name, version in sorted(new):
        if (name, version) in allowed:
            rows.append((kind, name, version, "allowed", allowed[(name, version)]))
            continue
        try:
            published = published_at(kind, name, version)
        except (urllib.error.URLError, OSError, KeyError, ValueError) as error:
            print(f"cannot read the publication date of {kind} {name} {version}: {error}", file=sys.stderr)
            return 1
        age = now - published
        status = "too new" if age < COOLDOWN else "ok"
        rows.append((kind, name, version, status, f"published {published:%Y-%m-%d}, {age.days} days ago"))
        if status == "too new":
            failures.append(f"{name} {version}")

    for kind, name, version, status, detail in rows:
        print(f"{status:8} {kind} {name} {version}: {detail}")
    print(f"{len(new)} new or bumped registry versions, {len(failures)} inside the {COOLDOWN.days}-day cooldown")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
            summary.write("### Dependency cooldown (#269, #239)\n\n"
                          "| status | ecosystem | package | version | detail |\n|---|---|---|---|---|\n")
            for kind, name, version, status, detail in rows:
                summary.write(f"| {status} | {kind} | {name} | {version} | {detail} |\n")
    if failures:
        print("inside the cooldown, add an entry with a reason to scripts/cooldown-allowlist.toml "
              "or wait: " + ", ".join(failures), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
