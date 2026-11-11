#!/usr/bin/env python3
"""Tests of scripts/check-dashboards.py (#380, criterion MON-22, part U).

The shipped dashboards must pass, and a copy of them with each of the listed changes must fail with a
message that names the file and the cause. Offline; run from the repository root:
python3 -I scripts/test_check_dashboards.py
"""

import importlib.util
import json
import shutil
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "check-dashboards.py"
SPEC_FILE = ROOT / "docs" / "SPEC.md"
SHIPPED = ROOT / "deploy" / "helm" / "mimic" / "dashboards"

MODULE_SPEC = importlib.util.spec_from_file_location("check_dashboards", SCRIPT)
checker = importlib.util.module_from_spec(MODULE_SPEC)
MODULE_SPEC.loader.exec_module(checker)

NAME = "mimic-controller.json"


def dump(doc):
    """The canonical text of a dashboard: 2-space indentation, sorted keys."""
    return json.dumps(doc, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def panels(doc):
    return [p for p in doc["panels"] if p["type"] not in ("row", "text")]


class CheckDashboards(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="dash-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.dir = self.tmp / "dashboards"
        self.dir.mkdir()
        shutil.copy(SHIPPED / NAME, self.dir / NAME)

    def errors(self):
        return checker.check(self.dir, SPEC_FILE)

    def mutate(self, change, name=NAME):
        """Apply change(doc) to the copy and write it back in canonical form."""
        path = self.dir / name
        doc = json.loads(path.read_text())
        change(doc)
        path.write_text(dump(doc))

    def assert_fails(self, cause, name=NAME):
        errs = self.errors()
        self.assertTrue(
            any(name in e and cause in e for e in errs),
            f"no error naming {name} and {cause!r}: {errs}",
        )

    def set_first_expr(self, expr):
        def change(doc):
            panels(doc)[0]["targets"][0]["expr"] = expr

        self.mutate(change)

    def test_mon_22_shipped_files_pass(self):
        self.assertTrue(list(SHIPPED.glob("*.json")), "no dashboard shipped")
        self.assertEqual(checker.check(SHIPPED, SPEC_FILE), [])

    def test_mon_22_unknown_metric(self):
        self.set_first_expr("sum(mimic_nope_total)")
        self.assert_fails("unknown metric")

    def test_mon_22_bucket_after_counter(self):
        self.set_first_expr("sum(rate(mimic_log_lines_dropped_total_bucket[$__rate_interval]))")
        self.assert_fails("not a histogram")

    def test_mon_22_unknown_label(self):
        self.set_first_expr('sum(rate(mimic_http_requests_total{pod_ip="x"}[$__rate_interval]))')
        self.assert_fails("pod_ip")

    def test_mon_22_by_authority_over_store_writes(self):
        self.set_first_expr("sum by (authority) (rate(mimic_store_writes_total[$__rate_interval]))")
        self.assert_fails("authority")

    def test_mon_22_datasource_uid_written(self):
        def change(doc):
            panels(doc)[0]["datasource"] = {"type": "prometheus", "uid": "mimic-prometheus"}

        self.mutate(change)
        self.assert_fails("datasource")
        self.assert_fails("mimic-prometheus")

    def test_mon_22_html_text_panel(self):
        def change(doc):
            text = next(p for p in doc["panels"] if p["type"] == "text")
            text["options"]["mode"] = "html"

        self.mutate(change)
        self.assert_fails("markdown")

    def test_mon_22_duplicated_uid(self):
        shutil.copy(self.dir / NAME, self.dir / "mimic-valkey.json")
        self.assert_fails("duplicate uid", "mimic-valkey.json")

    def test_mon_22_keys_out_of_order(self):
        path = self.dir / NAME
        doc = json.loads(path.read_text())
        path.write_text(json.dumps(doc, indent=2, sort_keys=False) + "\n")
        self.assert_fails("sorted keys")

    def test_mon_22_file_of_151_kib(self):
        def change(doc):
            doc["description"] = "x" * (151 * 1024)

        self.mutate(change)
        self.assertGreater((self.dir / NAME).stat().st_size, 150 * 1024)
        self.assert_fails("KiB")

    def test_mon_22_panel_missing_from_the_table(self):
        def change(doc):
            extra = json.loads(json.dumps(panels(doc)[0]))
            extra["title"] = "Not in the spec"
            extra["id"] = 9999
            doc["panels"].append(extra)

        self.mutate(change)
        self.assert_fails("not in the §9.1 table")

    def test_mon_22_panel_of_the_table_absent(self):
        def change(doc):
            doc["panels"].remove(panels(doc)[0])

        self.mutate(change)
        self.assert_fails("missing")

    def test_mon_22_table_query_absent_from_its_panel(self):
        self.set_first_expr("sum(up)")
        self.assert_fails("query of the §9.1 table")

    def test_mon_22_regex_matcher_on_authority(self):
        self.set_first_expr('sum(rate(mimic_http_requests_total{authority=~"a.*"}[$__rate_interval]))')
        self.assert_fails("regex matcher on authority")

    def test_mon_22_without_and_label_replace(self):
        self.set_first_expr("sum without (pod) (up)")
        self.assert_fails("without")
        self.set_first_expr('label_replace(up, "a", "b", "c", "d")')
        self.assert_fails("label_replace")

    def test_mon_22_url_alert_and_unit(self):
        def change(doc):
            p = panels(doc)[0]
            p["url"] = "https://example.com"
            p["alert"] = {}
            p["fieldConfig"]["defaults"]["unit"] = "furlongs"

        self.mutate(change)
        self.assert_fails("url")
        self.assert_fails("alert")
        self.assert_fails("furlongs")

    def test_mon_22_conventions(self):
        def change(doc):
            doc["editable"] = True
            doc["refresh"] = "5s"
            doc["tags"] = ["other"]

        self.mutate(change)
        self.assert_fails("editable")
        self.assert_fails("refresh")
        self.assert_fails("tags")


if __name__ == "__main__":
    unittest.main()
