"""Deterministic `pattern` rules: the baseline for the model `question` rules.

The text is normalised first: percent-decoding (two passes) and lower case.
Log4Shell lookups are also flattened: `${lower:j}`, `${::-j}` and `${env:X:-j}` become `j`.
"""
import re
import urllib.parse

ATTACK = [
    r"\bunion\b[\s/*]+(all[\s/*]+)?select\b", r"'\s*or\s+'?\d*'?\s*=\s*'?\d*", r"\bor\s+1\s*=\s*1\b",
    r";\s*drop\s+table\b", r"\b(sleep|pg_sleep|benchmark)\s*\(", r"\bwaitfor\s+delay\b", r"extractvalue\s*\(",
    r"information_schema\.", r"'\s*--", r"\w'\s*and\s",
    r"(\.\.[/\\]){2,}", r"\.\.\.\.//", r"/etc/(passwd|shadow|hosts)\b", r"/proc/self/", r"\bwin\.ini\b",
    r"file:///", r"serviceaccount/token", r"\.ssh/id_", r"\.\./.*\.env\b",
    r"\$\{jndi:",
]
SECRET = [
    r"\bAKIA[0-9A-Z]{16}\b", r"\bghp_[A-Za-z0-9]{36}\b", r"\bsk-(proj-)?[A-Za-z0-9_-]{32,}", r"\bxox[abpr]-[0-9A-Za-z-]{20,}",
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----", r"://[^:/\s]+:[^@\s]{6,}@", r"(?i)\bpassword\s*[=:]\s*\S{6,}",
    r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{20,}", r"\bglpat-[A-Za-z0-9_-]{20}\b", r"\bAIza[0-9A-Za-z_-]{35}\b",
]
PII = [
    r"[A-Za-z0-9._%+-]+@(?!.*shop\.svc)[A-Za-z0-9.-]+\.[A-Za-z]{2,}", r"\b\d{3}-\d{2}-\d{4}\b", r"\b(?:\d[ -]?){13,16}\b",
    r"\+\d{1,3}[ \d-]{8,}", r"\b555-01\d\d\b", r"\b[A-Z]{2}\d{2}(?: ?[0-9A-Z]{4}){3,7}\b",
]
INJECTION = [
    r"(?i)\b(ignore|disregard|forget)\b.{0,30}\b(previous|prior|all|above|the user)\b.{0,20}(instructions|task|rules)?",
    r"(?i)\bsystem prompt\b", r"(?i)\b(DAN|developer mode|unfiltered|no rules)\b", r"(?i)\bsystem override\b",
    r"(?i)\bignore(z)? les instructions\b", r"(?i)assistant[,:] (call|disregard|send)",
]

_FLAT = [(re.compile(r"\$\{(?:lower|upper):([^}]*)\}", re.I), r"\1"),
         (re.compile(r"\$\{[^{}$]*?:-([^}]*)\}"), r"\1"),
         (re.compile(r"\$\{date:'[^']*'\}"), "")]

RULES = {k: [re.compile(p, re.I if k == "attack" else 0) for p in v]
         for k, v in {"attack": ATTACK, "secret": SECRET, "pii": PII, "injection": INJECTION}.items()}


def normalise(text):
    t = urllib.parse.unquote(urllib.parse.unquote(text))
    for _ in range(4):
        before = t
        for rx, rep in _FLAT:
            t = rx.sub(rep, t)
        if t == before:
            break
    return t


def judge(text):
    t = normalise(text)
    return {k: 1.0 if any(rx.search(t) for rx in rxs) else 0.0 for k, rxs in RULES.items()}
