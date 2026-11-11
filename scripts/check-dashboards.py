#!/usr/bin/env python3
"""Checks the Grafana dashboards of the chart (#380, spec section 12.13, criterion MON-22).

For every deploy/helm/mimic/dashboards/*.json: the conventions of spec section 9.1, the datasource and panel
rules, every PromQL expression (tokenised; metric names and labels read from section 9 of docs/SPEC.md), and the
panels against the table of section 9.1. Python standard library only; run as `python3 -I`:

    python3 -I scripts/check-dashboards.py [DASHBOARDS_DIR [SPEC_FILE]]

Exit 0 when every file passes, 1 with one line per problem otherwise. The tests are test_check_dashboards.py.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAX_BYTES = 150 * 1024
DS = {"type": "prometheus", "uid": "${datasource}"}
UNITS = {"s", "bytes", "Bps", "reqps", "ops", "percentunit", "short"}
PANEL_TYPES = {"timeseries", "stat", "table", "bargauge", "text"}
TARGET_LABELS = {"job", "namespace", "pod", "deployment", "load_test"}
# The selectors of section 9.1, expanded in the queries of its tables.
SELECTORS = {
    "C": 'job="$controller"',
    "S": 'job="mimic-sidecar", namespace="$namespace", deployment="$deployment"',
    "V": 'job="mimic-valkey", namespace="$namespace"',
    "L": 'job="mimic-load", namespace="$namespace", load_test=~"$load_test"',
}

Q_CONTROLLER = 'label_values(mimic_build_info{component="controller"}, job)'
Q_NS_CONTROLLER = 'label_values(mimic_build_info{job="$controller"}, namespace)'
Q_NS_SIDECAR = 'label_values(mimic_build_info{job="mimic-sidecar"}, namespace)'
Q_DEPLOYMENT = 'label_values(mimic_build_info{job="mimic-sidecar", namespace="$namespace"}, deployment)'
Q_LOAD_TEST = 'label_values(mimic_load_in_flight{job="mimic-load", namespace="$namespace"}, load_test)'
# uid -> the variables after `datasource`, with their queries (section 9.1).
EXPECTED_VARIABLES = {
    "mimic-controller": [("controller", Q_CONTROLLER)],
    "mimic-valkey": [("controller", Q_CONTROLLER), ("namespace", Q_NS_CONTROLLER)],
    "mimic-sidecar": [("namespace", Q_NS_SIDECAR), ("deployment", Q_DEPLOYMENT)],
    "mimic-discovery": [("controller", Q_CONTROLLER), ("namespace", Q_NS_SIDECAR), ("deployment", Q_DEPLOYMENT)],
    "mimic-mock-load": [
        ("controller", Q_CONTROLLER),
        ("namespace", Q_NS_SIDECAR),
        ("deployment", Q_DEPLOYMENT),
        ("load_test", Q_LOAD_TEST),
    ],
}
# uid -> {annotation name: expression} that section 9.1 fixes.
EXPECTED_ANNOTATIONS = {
    "mimic-controller": {"Process started": "count(time() - process_start_time_seconds{C} < 60) > 0"},
}

AGGREGATIONS = {
    "sum", "min", "max", "avg", "count", "group", "stddev", "stdvar", "topk", "bottomk", "quantile", "count_values",
}
FUNCTIONS = {
    "rate", "increase", "irate", "delta", "idelta", "deriv", "histogram_quantile", "time", "vector", "scalar", "abs",
    "ceil", "floor", "round", "clamp", "clamp_min", "clamp_max", "sort", "sort_desc", "changes", "resets", "absent",
    "absent_over_time", "avg_over_time", "max_over_time", "min_over_time", "sum_over_time", "count_over_time",
    "quantile_over_time", "last_over_time", "timestamp", "label_values",
}
FORBIDDEN_FUNCTIONS = {"label_replace", "label_join"}
KEYWORDS = {"by", "without", "on", "ignoring", "group_left", "group_right", "and", "or", "unless", "bool", "offset", "atan2"}
CLAUSES = {"by", "without", "on", "ignoring", "group_left", "group_right"}

TOKEN_RE = re.compile(
    r"""
    (?P<ws>\s+)
    |(?P<str>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`[^`]*`)
    |(?P<num>\d[\w.]*)
    |(?P<id>[A-Za-z_:][A-Za-z0-9_:]*)
    |(?P<op>=~|!~|!=|==|>=|<=|[-+*/%^<>=@])
    |(?P<punct>[(){}\[\],])
    """,
    re.X,
)
NAME_RE = re.compile(r"`((?:mimic|process|redis)_[a-z0-9_]+)(?:\{([^}`]*)\})?(?=[`\s])")


def split_cells(line):
    """The cells of a Markdown table row, split at unescaped pipes, or None when the line is not a row."""
    line = line.strip()
    if not line.startswith("|"):
        return None
    parts = re.split(r"(?<!\\)\|", line[1:])
    if parts and not parts[-1].strip():
        parts.pop()
    return [p.strip() for p in parts]


def label_cell(cell):
    """The label names of the Labels column of a section 9 table: the leading backticked name of each
    top-level comma-separated part, up to the first part that explains itself after a colon."""
    if cell in ("", "-", "—"):
        return set()
    parts, depth, start = [], 0, 0
    for i, ch in enumerate(cell):
        depth += (ch == "(") - (ch == ")")
        if ch == "," and depth == 0:
            parts.append(cell[start:i])
            start = i + 1
    parts.append(cell[start:])
    labels = set()
    for part in parts:
        m = re.match(r"\s*`([a-z_]+)`(\s*\(.*\))?\s*(:.*)?$", part, re.S)
        if not m:
            break
        labels.add(m.group(1))
        if m.group(3):
            break
    return labels


class Spec:
    """The metrics of section 9 (name -> labels, histogram or not) and the panel tables of section 9.1."""

    def __init__(self, text):
        start = text.index("## 9. Observability")
        mid = text.index("### 9.1 Dashboards", start)
        end = text.index("\n## ", mid)
        self.metrics = {}
        self._read_metrics(text[start:mid])
        self.tables = self._read_tables(text[mid:end])

    def _add(self, name, labels, histogram):
        old_labels, old_histogram = self.metrics.get(name, (set(), False))
        self.metrics[name] = (old_labels | labels, old_histogram or histogram)

    def _read_metrics(self, section):
        for m in NAME_RE.finditer(section):
            labels = {x.strip() for x in (m.group(2) or "").split(",") if x.strip()}
            self._add(m.group(1), labels, section[m.end() : m.end() + 12] == " (histogram")
        for line in section.splitlines():
            cells = split_cells(line)
            if not cells or len(cells) not in (3, 4):
                continue
            m = re.fullmatch(r"`((?:mimic|process|redis)_[a-z0-9_]+)`", cells[0])
            if m:
                kind = cells[1] if len(cells) == 3 else cells[2]
                self._add(m.group(1), label_cell(cells[-1]), kind.startswith("histogram"))

    @staticmethod
    def _read_tables(section):
        tables, uid = {}, None
        for line in section.splitlines():
            m = re.match(r"\*\*`(mimic-[a-z-]+)`\*\*", line)
            if m:
                uid = m.group(1)
                tables[uid] = {}
                continue
            cells = split_cells(line)
            if uid is None or not cells or len(cells) != 4 or cells[0] == "Row" or set(cells[0]) <= {"-"}:
                continue
            queries = [expand(q) for q in re.findall(r"`([^`]+)`", cells[3].replace("\\|", "|")) if "(" in q]
            tables[uid][(cells[0], cells[1])] = queries
        return tables

    def resolve(self, name):
        """(labels, error) of a metric name as a query writes it."""
        if name == "up":
            return set(), None
        if name in self.metrics:
            return set(self.metrics[name][0]), None
        for suffix in ("_bucket", "_count", "_sum"):
            base = name[: -len(suffix)]
            if name.endswith(suffix) and base in self.metrics:
                labels, histogram = self.metrics[base]
                if not histogram:
                    return None, f"`{name}`: `{suffix}` after `{base}`, which is not a histogram"
                return set(labels) | ({"le"} if suffix == "_bucket" else set()), None
        return None, f"unknown metric `{name}` (not a name of section 9)"


def expand(expr):
    return re.sub(r"\{([CSVL])(?=[,}])", lambda m: "{" + SELECTORS[m.group(1)], expr)


def norm(expr):
    return " ".join(expr.split())


def lex(expr):
    """Tokens (kind, text) of an expression, with the Grafana variables read as the spec says (MON-22)."""
    expr = re.sub(
        r"\$\{?(__rate_interval|__range|\w+)\}?",
        lambda m: {"__rate_interval": "1m", "__range": "1h"}.get(m.group(1), "x"),
        expr,
    )
    tokens, pos = [], 0
    while pos < len(expr):
        m = TOKEN_RE.match(expr, pos)
        if not m:
            raise ValueError(f"unexpected character {expr[pos]!r}")
        pos = m.end()
        if m.lastgroup != "ws":
            tokens.append((m.lastgroup, m.group()))
    return tokens


def pair_brackets(tokens):
    pairs, stack = {}, []
    for i, (kind, text) in enumerate(tokens):
        if kind != "punct":
            continue
        if text in "([{":
            stack.append(i)
        elif text in ")]}":
            if not stack or "([{".index(tokens[stack[-1]][1]) != ")]}".index(text):
                raise ValueError("unbalanced brackets")
            pairs[stack.pop()] = i
    if stack:
        raise ValueError("unbalanced brackets")
    return pairs


def is_tok(tokens, i, text):
    return i < len(tokens) and tokens[i][1] == text and tokens[i][0] in ("punct", "id")


def label_list(tokens, pairs, open_):
    """The names inside a `(a, b)` clause that starts at the token `open_`."""
    return [t for k, t in tokens[open_ + 1 : pairs[open_]] if k == "id"]


def analyse(expr, spec):
    """(errors, selectors) of one expression; each selector is (metric name, allowed labels or None)."""
    try:
        tokens = lex(expr)
        pairs = pair_brackets(tokens)
    except ValueError as e:
        return [f"`{expr}`: {e}"], []
    errors, selectors, aggregations = [], [], []
    i = 0
    while i < len(tokens):
        kind, text = tokens[i]
        if kind != "id":
            i += 1
            continue
        if text in CLAUSES:
            if text == "without":
                errors.append(f"`{expr}`: `without` is not allowed")
            i = pairs[i + 1] + 1 if is_tok(tokens, i + 1, "(") else i + 1
            continue
        if text in KEYWORDS or text.lower() in ("inf", "nan"):
            i += 1
            continue
        if is_tok(tokens, i + 1, "(") or (text in AGGREGATIONS and tokens[i + 1:][:1] and tokens[i + 1][1] in ("by", "without")):
            if text in FORBIDDEN_FUNCTIONS:
                errors.append(f"`{expr}`: `{text}` is not allowed")
            elif text in AGGREGATIONS:
                aggregations.append(i)
            elif text not in FUNCTIONS:
                errors.append(f"`{expr}`: unknown function `{text}`")
            i += 1
            continue
        labels, error = spec.resolve(text)
        if error:
            errors.append(f"`{expr}`: {error}")
        start = i
        i += 1
        if is_tok(tokens, i, "{"):
            close = pairs[i]
            errors += matchers(expr, text, labels, tokens[i + 1 : close])
            i = close + 1
        allowed = None if labels is None else labels | TARGET_LABELS
        selectors.append((start, text, allowed))
    for a in aggregations:
        errors += check_by(expr, tokens, pairs, a, selectors)
    return errors, [(name, allowed) for _, name, allowed in selectors]


def matchers(expr, metric, labels, tokens):
    errors, group = [], []
    for tok in tokens + [("punct", ",")]:
        if tok != ("punct", ","):
            group.append(tok)
            continue
        if not group:
            continue
        if [k for k, _ in group] != ["id", "op", "str"] or group[1][1] not in ("=", "!=", "=~", "!~"):
            errors.append(f"`{expr}`: malformed matcher in `{metric}`")
        else:
            label, op = group[0][1], group[1][1]
            if label == "authority" and op in ("=~", "!~"):
                errors.append(f"`{expr}`: regex matcher on authority (its values come from traffic)")
            if labels is not None and label not in labels and label not in TARGET_LABELS:
                errors.append(f"`{expr}`: unknown label `{label}` on `{metric}`")
        group = []
    return errors


def check_by(expr, tokens, pairs, i, selectors):
    """The `by (...)` labels of the aggregation at token i against the metrics inside its arguments."""
    j, by = i + 1, None
    if tokens[j][0] == "id" and tokens[j][1] in ("by", "without") and is_tok(tokens, j + 1, "("):
        by, j = label_list(tokens, pairs, j + 1), pairs[j + 1] + 1
    if not is_tok(tokens, j, "("):
        return [f"`{expr}`: `{tokens[i][1]}` without arguments"]
    open_, close = j, pairs[j]
    k = close + 1
    if by is None and k < len(tokens) and tokens[k][1] == "by" and is_tok(tokens, k + 1, "("):
        by = label_list(tokens, pairs, k + 1)
    inside = [(name, allowed) for pos, name, allowed in selectors if open_ < pos < close]
    if any(allowed is None for _, allowed in inside):
        return []
    allowed = set(TARGET_LABELS).union(*[a for _, a in inside]) if inside else set(TARGET_LABELS)
    names = ", ".join(sorted({n for n, _ in inside})) or "none"
    return [
        f"`{expr}`: label `{label}` of by (...) is not a label of a metric inside the aggregation ({names})"
        for label in by or []
        if label not in allowed
    ]


def walk(node, path=""):
    """Every (path, dict) of a JSON document."""
    if isinstance(node, dict):
        yield path, node
        for k, v in node.items():
            yield from walk(v, f"{path}/{k}")
    elif isinstance(node, list):
        for i, v in enumerate(node):
            yield from walk(v, f"{path}/{i}")


def flat_panels(doc):
    """(row title, panel) in order, nested rows flattened; the row panels themselves are included."""
    out, row = [], None
    for p in doc.get("panels", []):
        if p.get("type") == "row":
            row = p.get("title")
            out.append((row, p))
            out += [(row, c) for c in p.get("panels", [])]
        else:
            out.append((row, p))
    return out


def query_text(variable):
    q = variable.get("query")
    return q.get("query") if isinstance(q, dict) else q


def check_label_values(query, spec):
    """Errors of a `label_values(selector, label)` variable query (any other query is analysed as it is)."""
    m = re.fullmatch(r"label_values\((.*)\)", query.strip(), re.S)
    if not m:
        return analyse(query, spec)[0]
    inner = m.group(1)
    cut = inner.rindex(",")
    errors, selectors = analyse(inner[:cut], spec)
    label = inner[cut + 1 :].strip()
    for name, allowed in selectors:
        if allowed is not None and label not in allowed:
            errors.append(f"`{query}`: unknown label `{label}` on `{name}`")
    return errors


def check_file(path, text, spec, seen_uids):
    """The problems of one dashboard file, as messages without the file name."""
    errors = []
    size = len(text.encode())
    if size > MAX_BYTES:
        errors.append(f"is {size / 1024:.0f} KiB, over the {MAX_BYTES // 1024} KiB limit")
    try:
        doc = json.loads(text)
    except ValueError as e:
        return [f"is not JSON: {e}"]
    if not isinstance(doc, dict):
        return ["is not a JSON object"]
    if json.dumps(doc, indent=2, sort_keys=True, ensure_ascii=False) + "\n" != text:
        errors.append("is not in the form of `json.dumps(indent=2, sort_keys=True)` (2-space indentation, sorted keys)")
    if "mimic-prometheus" in text:
        errors.append("contains `mimic-prometheus`: no datasource uid is written in a file")

    uid = doc.get("uid")
    if uid != path.stem:
        errors.append(f"uid `{uid}` is not the file name `{path.stem}`")
    if uid in seen_uids:
        errors.append(f"duplicate uid `{uid}` (also in {seen_uids[uid]})")
    seen_uids.setdefault(uid, path.name)
    if not str(doc.get("title", "")).startswith("Mimic / "):
        errors.append("title does not start with `Mimic / `")
    expected = {"tags": ["mimic"], "id": None, "editable": False, "refresh": "30s", "graphTooltip": 1,
                "time": {"from": "now-1h", "to": "now"}}
    for key, value in expected.items():
        if key not in doc or doc[key] != value:
            errors.append(f"{key} is {doc.get(key)!r}, not {value!r}")
    links = doc.get("links")
    link = links[0] if isinstance(links, list) and len(links) == 1 and isinstance(links[0], dict) else {}
    if not (link.get("type") == "dashboards" and link.get("tags") == ["mimic"] and link.get("asDropdown") is True
            and link.get("keepTime") is True and link.get("includeVars") is True):
        errors.append("links is not the single `dashboards` link (tag mimic, asDropdown, keepTime, includeVars)")

    errors += check_variables(doc, uid, spec)
    errors += check_datasources(doc)
    panels = flat_panels(doc)
    errors += check_panels(panels)
    errors += check_expressions(doc, panels, spec)
    errors += check_table(doc, uid, panels, spec)
    errors += check_annotations(doc, uid)
    return errors


def check_variables(doc, uid, spec):
    errors = []
    variables = doc.get("templating", {}).get("list", [])
    first = variables[0] if variables else {}
    if not (first.get("name") == "datasource" and first.get("type") == "datasource" and first.get("query") == "prometheus"):
        errors.append("the first variable is not `datasource` (type datasource, query prometheus)")
    if uid not in EXPECTED_VARIABLES:
        errors.append(f"uid `{uid}` is not one of the five dashboards")
        return errors
    wanted = EXPECTED_VARIABLES[uid]
    got = [(v.get("name"), query_text(v)) for v in variables[1:]]
    if got != wanted:
        errors.append(f"variables are {[n for n, _ in got]} with queries {[q for _, q in got]}, "
                      f"not {[n for n, _ in wanted]} with the queries of section 9.1")
    for v in variables[1:]:
        if v.get("type") != "query" or v.get("refresh") != 2 or not v.get("sort"):
            errors.append(f"variable `{v.get('name')}` is not a sorted query variable refreshed on time-range change")
        if bool(v.get("multi")) != (v.get("name") == "load_test") or bool(v.get("includeAll")) != (v.get("name") == "load_test"):
            errors.append(f"variable `{v.get('name')}` has the wrong multi-value and All setting")
        errors += [f"variable `{v.get('name')}`: {e}" for e in check_label_values(query_text(v) or "", spec)]
    return errors


def check_datasources(doc):
    errors = []
    for path, node in walk(doc):
        if "datasource" in node and node["datasource"] != DS:
            errors.append(f"{path or '/'}: datasource is {json.dumps(node['datasource'])}, not the `${{datasource}}` variable")
    for p in doc.get("templating", {}).get("list", []):
        if p.get("type") == "query" and p.get("datasource") != DS:
            errors.append(f"variable `{p.get('name')}` has no datasource {DS['uid']}")
    for a in doc.get("annotations", {}).get("list", []):
        if a.get("datasource") != DS:
            errors.append(f"annotation `{a.get('name')}` has no datasource {DS['uid']}")
    return errors


def check_panels(panels):
    errors = []
    for row, p in panels:
        name = f"panel `{row} / {p.get('title')}`"
        kind = p.get("type")
        if kind != "row" and kind not in PANEL_TYPES:
            errors.append(f"{name} has the type `{kind}`, not one of {sorted(PANEL_TYPES)}")
        if kind == "text" and p.get("options", {}).get("mode") != "markdown":
            errors.append(f"{name}: a text panel is not in markdown mode")
        if "alert" in p:
            errors.append(f"{name} has an `alert`")
        if kind in ("timeseries", "stat", "table", "bargauge"):
            if p.get("datasource") != DS:
                errors.append(f"{name}: datasource is not the `${{datasource}}` variable")
            if p.get("fieldConfig", {}).get("defaults", {}).get("unit") is None:
                errors.append(f"{name} has no unit")
    return errors


def check_expressions(doc, panels, spec):
    errors = []
    for path, node in walk(doc):
        if "url" in node:
            errors.append(f"{path or '/'}: has a `url`")
        if "unit" in node and node["unit"] not in UNITS:
            errors.append(f"{path or '/'}: unit `{node['unit']}` is not one of {sorted(UNITS)}")
        if isinstance(node.get("expr"), str):
            errors += [f"{path}: {e}" for e in analyse(node["expr"], spec)[0]]
    return errors


def check_table(doc, uid, panels, spec):
    if uid not in spec.tables:
        return [f"has no table in section 9.1 for `{uid}`"]
    table, errors, seen = spec.tables[uid], [], {}
    for row, p in panels:
        if p.get("type") in ("row", "text"):
            continue
        key = (row, p.get("title"))
        if key in seen:
            errors.append(f"panel `{key[0]} / {key[1]}` is duplicated")
        seen[key] = {norm(t.get("expr", "")) for t in p.get("targets", [])}
    for key in seen:
        if key not in table:
            errors.append(f"panel `{key[0]} / {key[1]}` is not in the §9.1 table")
    for key, queries in table.items():
        if key not in seen:
            errors.append(f"panel `{key[0]} / {key[1]}` of the §9.1 table is missing")
            continue
        errors += [
            f"panel `{key[0]} / {key[1]}`: query of the §9.1 table not found: {q}"
            for q in queries
            if norm(q) not in seen[key]
        ]
    return errors


def check_annotations(doc, uid):
    got = {a.get("name"): norm(a.get("expr", "")) for a in doc.get("annotations", {}).get("list", [])}
    return [
        f"annotation `{name}` is not `{expand(expr)}`"
        for name, expr in EXPECTED_ANNOTATIONS.get(uid, {}).items()
        if got.get(name) != norm(expand(expr))
    ]


def check(dashboards_dir, spec_path):
    """Every problem of every dashboard of the directory, as `<file>: <cause>` lines."""
    spec = Spec(Path(spec_path).read_text())
    files = sorted(Path(dashboards_dir).glob("*.json"))
    if not files:
        return [f"{dashboards_dir}: no dashboard found"]
    errors, seen = [], {}
    for path in files:
        errors += [f"{path.name}: {e}" for e in check_file(path, path.read_text(), spec, seen)]
    return errors


def main(argv):
    dashboards = Path(argv[1]) if len(argv) > 1 else ROOT / "deploy" / "helm" / "mimic" / "dashboards"
    spec = Path(argv[2]) if len(argv) > 2 else ROOT / "docs" / "SPEC.md"
    errors = check(dashboards, spec)
    for e in errors:
        print(f"FAIL {e}", file=sys.stderr)
    if errors:
        return 1
    print(f"dashboards ok: {len(list(dashboards.glob('*.json')))} files")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
