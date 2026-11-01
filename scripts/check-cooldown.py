#!/usr/bin/env python3
"""Cooldown for new crate releases (#269, spec FND-10).

A crate version that a change adds or bumps in a Cargo.lock must have been published at least
COOLDOWN_DAYS days before the check, unless scripts/cooldown-allowlist.toml lists it with a reason.
The publication date is the `created_at` of the version in the crates.io API. Only registry
packages count: workspace members have no source, and git sources are refused by cargo-deny.

usage: check-cooldown.py <base-commit> <lockfile>...

  Each lockfile is compared with its content at <base-commit> (a lockfile absent there counts as
  empty). Environment: GITHUB_STEP_SUMMARY, when set, receives a Markdown table.
  --now YYYY-MM-DDTHH:MM:SSZ fixes the clock (tests).

Exit 0 when every new version is old enough or allowed; 1 when one is not, or when crates.io cannot
be asked (fails closed); 2 on a usage error or an invalid allowlist.
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
ALLOWLIST = Path(__file__).resolve().with_name("cooldown-allowlist.toml")
API = "https://crates.io/api/v1/crates/{name}/{version}"
USER_AGENT = "mimic-cooldown-check (https://github.com/Thaliph/mock-server)"


def registry_packages(text):
    """The (name, version) pairs of the registry packages of a Cargo.lock text."""
    doc = tomllib.loads(text)
    return {
        (pkg["name"], pkg["version"])
        for pkg in doc.get("package", [])
        if str(pkg.get("source", "")).startswith(REGISTRY_PREFIX)
    }


def base_packages(base, lockfile):
    """Registry packages of <lockfile> at <base>; empty when the file does not exist there."""
    spec = f"{base}:{lockfile}"
    if subprocess.run(["git", "cat-file", "-e", spec], capture_output=True).returncode != 0:
        return set()
    out = subprocess.run(["git", "show", spec], capture_output=True, text=True, check=True)
    return registry_packages(out.stdout)


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


def published_at(name, version):
    """The crates.io publication time of one version (timezone-aware)."""
    url = API.format(name=urllib.parse.quote(name, safe=""), version=urllib.parse.quote(version, safe=""))
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=20) as response:
        data = json.load(response)
    return datetime.fromisoformat(data["version"]["created_at"].replace("Z", "+00:00"))


def main(argv):
    parser = argparse.ArgumentParser(description="Refuse crate versions published less than 14 days ago.")
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
    for lockfile in args.lockfiles:
        head = registry_packages(Path(lockfile).read_text())
        new |= head - base_packages(args.base, lockfile)

    rows, failures = [], []
    for name, version in sorted(new):
        if (name, version) in allowed:
            rows.append((name, version, "allowed", allowed[(name, version)]))
            continue
        try:
            published = published_at(name, version)
        except (urllib.error.URLError, OSError, KeyError, ValueError) as error:
            print(f"cannot read the publication date of {name} {version}: {error}", file=sys.stderr)
            return 1
        age = now - published
        status = "too new" if age < COOLDOWN else "ok"
        rows.append((name, version, status, f"published {published:%Y-%m-%d}, {age.days} days ago"))
        if status == "too new":
            failures.append(f"{name} {version}")

    for name, version, status, detail in rows:
        print(f"{status:8} {name} {version}: {detail}")
    print(f"{len(new)} new or bumped registry versions, {len(failures)} inside the {COOLDOWN.days}-day cooldown")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
            summary.write("### Crate cooldown (#269)\n\n| status | crate | version | detail |\n|---|---|---|---|\n")
            for name, version, status, detail in rows:
                summary.write(f"| {status} | {name} | {version} | {detail} |\n")
    if failures:
        print("inside the cooldown, add an entry with a reason to scripts/cooldown-allowlist.toml "
              "or wait: " + ", ".join(failures), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
