#!/usr/bin/env python3
"""Which CI jobs a change needs (spec FND-10, the `changes` job of .github/workflows/ci.yml).

    python3 -I scripts/ci_plan.py --event pull_request --changed FILE   # FILE: NUL-separated paths
    python3 -I scripts/ci_plan.py --event push                          # every event but a pull request

Prints one `key=true|false` line per output, for $GITHUB_OUTPUT. A file sets one or more categories
(rust, image, chart, e2e, workflows, docs, spec); a file of no category sets `other`. A job's gate
is set when one of its categories is. Everything is set (`full`) for `other`, a change under
.github/, any event but a pull request, and a pull request with no changed file: an unknown change
runs every job. The rules only add categories, so a mistake in them can only run more jobs.
Unit tests: scripts/test_ci_plan.py.
"""

import argparse
import sys
from pathlib import Path

CATEGORIES = ("rust", "image", "chart", "e2e", "workflows", "docs", "spec")

# A path starts with one of the prefixes (a directory of the repository) or is one of the files.
RUST_PREFIXES = ("crates/", "fuzz/")
RUST_FILES = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "deny.toml",
        "scripts/cooldown-allowlist.toml",
        "scripts/check-cooldown.py",
        "scripts/test_check_cooldown.py",
        "scripts/vendor-swagger-ui.py",
        "scripts/test_vendor_swagger_ui.py",
        "scripts/check-test-messages.sh",
        "scripts/test_check_test_messages.sh",
    }
)

# The image holds the Rust code (crates/, the Cargo files) and its own build and scan inputs.
IMAGE_PREFIXES = ("crates/", "images/")
IMAGE_FILES = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "Dockerfile",
        ".dockerignore",
        ".trivyignore.yaml",
        "scripts/check-image.sh",
        "scripts/check-init-image.sh",
        "scripts/pinned-image.sh",
    }
)

# The chart and its checks: scripts/check-pins.sh reads images/, check-release.sh reads Cargo.toml.
CHART_PREFIXES = ("deploy/", "images/")
CHART_FILES = frozenset(
    {
        "Cargo.toml",
        "scripts/check-chart.sh",
        "scripts/check-release-chart.sh",
        "scripts/check-pins.sh",
        "scripts/check-release.sh",
        "scripts/check-dashboards.py",
        "scripts/test_check_dashboards.py",
        "scripts/pin-chart.sh",
        "scripts/pinned-image.sh",
    }
)

E2E_PREFIXES = ("scripts/e2e/",)
E2E_FILES = frozenset({"scripts/e2e.sh", "scripts/e2e-load.sh"})

# Everything under .github is a workflow concern: it sets `full` (see below). So does the classifier
# itself: a pull request that changes it runs every job.
WORKFLOW_PREFIXES = (".github/",)
WORKFLOW_FILES = frozenset({"scripts/ci_plan.py", "scripts/test_ci_plan.py"})

# Markdown anywhere, the docs tree, and the scripts that check the release documentation.
DOCS_PREFIXES = ("docs/",)
DOCS_SUFFIXES = (".md",)
DOCS_FILES = frozenset({"scripts/release-notes.py", "scripts/test_release_notes.py", "scripts/publish-wiki.sh"})

SPEC_FILES = frozenset({"docs/SPEC.md"})

# The gate of each job: it runs when one of its categories is set.
GATES = {
    "lint": ("rust",),
    "test": ("rust",),
    "codeql_rust": ("rust",),
    "codeql_actions": ("workflows",),
    "security": ("rust", "workflows"),
    "cooldown": ("rust",),
    "vendored_ui": ("rust", "chart", "docs", "spec"),
    # The chart stage runs before e2e, so it runs whenever the images are built for e2e.
    "chart": ("chart", "docs", "spec", "image", "e2e"),
    "trivy_config": ("chart", "image"),
    "image": ("image", "chart", "e2e"),
    "e2e": ("image", "chart", "e2e"),
    "e2e_load": ("image", "chart", "e2e"),
}

# The fuzz jobs (PRX-28, DSC-23, PUL-20, MCK-20, LDT-18): a directory entry matches itself and what is under it.
CODEC_DIRS = ("crates/mimic-core/src/proxy/http1", "fuzz")
CODEC_FILES = frozenset({"crates/mimic-core/src/proxy.rs", "Cargo.toml", "Cargo.lock"})
DISCOVERY_DIRS = ("crates/mimic-core/src/discovery", "crates/mimic-core/src/mock", "crates/mimic-core/src/load", "fuzz")
DISCOVERY_FILES = frozenset({"crates/mimic-core/src/mock.rs"})


def categories_of(path):
    """The categories that one changed path sets (empty for an unknown path)."""
    cats = set()
    if path.startswith(RUST_PREFIXES) or path in RUST_FILES:
        cats.add("rust")
    if path.startswith(IMAGE_PREFIXES) or path in IMAGE_FILES:
        cats.add("image")
    if path.startswith(CHART_PREFIXES) or path in CHART_FILES:
        cats.add("chart")
    if path.startswith(E2E_PREFIXES) or path in E2E_FILES:
        cats.add("e2e")
    if path.startswith(WORKFLOW_PREFIXES) or path in WORKFLOW_FILES:
        cats.add("workflows")
    if path.startswith(DOCS_PREFIXES) or path.endswith(DOCS_SUFFIXES) or path in DOCS_FILES:
        cats.add("docs")
    if path in SPEC_FILES:
        cats.add("spec")
    return cats


def under(path, dirs, files):
    """The path is one of the files, or is a directory of `dirs` or under one (not `proxy_x` for `proxy`)."""
    return path in files or any(path == d or path.startswith(d + "/") for d in dirs)


def classify(event, paths):
    """The categories, the gates and the fuzz gates of a change: a dict of booleans, in a fixed order."""
    paths = list(paths)
    cats = {c: False for c in CATEGORIES}
    other = False
    for path in paths:
        found = categories_of(path)
        if not found:
            other = True
        for c in found:
            cats[c] = True

    full = event != "pull_request" or not paths or other or cats["workflows"]
    if full:
        cats = {c: True for c in CATEGORIES}

    result = {c: cats[c] for c in CATEGORIES}
    result["other"] = other
    result["full"] = full
    for job, needed in GATES.items():
        result[f"gate_{job}"] = any(cats[c] for c in needed)
    result["gate_fuzz_http1"] = full or any(under(p, CODEC_DIRS, CODEC_FILES) for p in paths)
    result["gate_fuzz_discovery"] = full or any(under(p, DISCOVERY_DIRS, DISCOVERY_FILES) for p in paths)
    return result


def read_paths(path):
    """The NUL-separated list that `git diff -z --name-only --no-renames` writes."""
    data = Path(path).read_bytes()
    return [p.decode("utf-8", "surrogateescape") for p in data.split(b"\0") if p]


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--event", required=True, help="the GitHub event name")
    parser.add_argument("--changed", help="NUL-separated paths changed by the pull request")
    args = parser.parse_args(argv)

    if args.event == "pull_request" and args.changed is None:
        print("ci-plan: a pull request needs --changed (the changed paths)", file=sys.stderr)
        return 1
    paths = read_paths(args.changed) if args.changed is not None else []

    result = classify(args.event, paths)
    for key, value in result.items():
        print(f"{key}={'true' if value else 'false'}")
    on = [k for k, v in result.items() if v and k != "full"]
    print(f"ci-plan: {len(paths)} changed path(s), full={result['full']}, on: {' '.join(on)}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
