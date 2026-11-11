#!/usr/bin/env python3
"""Print the release notes of one version from CHANGELOG.md (F12, criterion REL-02).

Usage: scripts/release-notes.py <version> [--changelog PATH]

Prints the body of the `## [<version>] - <date>` section, without its heading. Exits 1 with a
one-line reason when the changelog does not start with `## [Unreleased]`, when the section or its
date is missing, when it lacks one of `### Features`, `### Accepted limits`, `### Upgrade notes`
(in that order), or when the spec link at the tag is missing. Standard library only.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SUBSECTIONS = ["Features", "Accepted limits", "Upgrade notes"]
SECTION = re.compile(r"^## \[([^\]]+)\](?: - (\S+))?\s*$")
DATE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$")


class NotesError(Exception):
    pass


def extract(text, version):
    """The body of the section of `version`, or NotesError with the reason."""
    if not VERSION.match(version):
        raise NotesError(f"version {version!r} is not a semantic version")
    lines = text.splitlines()
    sections = []  # (line index, name, date)
    fenced = [False] * len(lines)  # inside a ``` fence: no heading there is structure
    inside = False
    for i, line in enumerate(lines):
        if line.lstrip().startswith("```"):
            inside = not inside
            fenced[i] = True
        else:
            fenced[i] = inside
        m = None if fenced[i] else SECTION.match(line)
        if m:
            sections.append((i, m.group(1), m.group(2)))
    if not sections or sections[0][1] != "Unreleased":
        raise NotesError("CHANGELOG.md: the first section is not [Unreleased]")
    found = [s for s in sections if s[1] == version]
    if not found:
        raise NotesError(f"CHANGELOG.md: no section [{version}]")
    if len(found) > 1:
        raise NotesError(f"CHANGELOG.md: section [{version}] appears twice")
    start, _, date = found[0]
    if date is None or not DATE.match(date):
        raise NotesError(f"CHANGELOG.md: section [{version}] has no date (YYYY-MM-DD)")
    later = [s[0] for s in sections if s[0] > start]
    end = later[0] if later else len(lines)
    body = lines[start + 1 : end]
    # Link reference definitions at the end of the file belong to no section.
    while body and (not body[-1].strip() or re.match(r"^\[[^\]]+\]: ", body[-1])):
        body.pop()
    headings = [
        line[4:].strip()
        for k, line in enumerate(body, start + 1)
        if line.startswith("### ") and not fenced[k]
    ]
    if headings != SUBSECTIONS:
        raise NotesError(
            f"CHANGELOG.md: section [{version}] has subsections {headings}, want {SUBSECTIONS}"
        )
    link = f"https://github.com/Thaliph/mock-server/blob/v{version}/docs/SPEC.md#111-accepted-limits"
    if link not in "\n".join(body):
        raise NotesError(f"CHANGELOG.md: section [{version}] does not link {link}")
    return "\n".join(body).strip("\n") + "\n"


def main(argv):
    args = argv[1:]
    path = ROOT / "CHANGELOG.md"
    if len(args) == 3 and args[1] == "--changelog":
        path = Path(args[2])
        args = args[:1]
    if len(args) != 1:
        print("usage: release-notes.py <version> [--changelog PATH]", file=sys.stderr)
        return 1
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as e:
        print(f"{path}: {e.strerror}", file=sys.stderr)
        return 1
    try:
        sys.stdout.write(extract(text, args[0]))
    except NotesError as e:
        print(e, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
