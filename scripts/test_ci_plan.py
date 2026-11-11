#!/usr/bin/env python3
"""Tests of scripts/ci_plan.py (spec FND-10: the path-filtered CI).

Offline; run from the repository root: python3 -I scripts/test_ci_plan.py
The `changes` job of ci.yml runs them before it classifies a pull request, so a broken classifier
fails the run instead of skipping jobs.
"""

import importlib.util
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "ci_plan.py"
CI_YML = ROOT / ".github" / "workflows" / "ci.yml"

SPEC = importlib.util.spec_from_file_location("ci_plan", SCRIPT)
plan = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plan)

GATES = [
    "gate_lint",
    "gate_test",
    "gate_codeql_rust",
    "gate_codeql_actions",
    "gate_security",
    "gate_cooldown",
    "gate_vendored_ui",
    "gate_chart",
    "gate_trivy_config",
    "gate_image",
    "gate_e2e",
    "gate_e2e_load",
    "gate_fuzz_http1",
    "gate_fuzz_discovery",
]


def run(paths, event="pull_request"):
    """The gates and categories for a list of changed paths of an event."""
    return plan.classify(event, paths)


def on(result, *names):
    """The result has every name set (true) and nothing else among the gates."""
    gates = {k: v for k, v in result.items() if k.startswith("gate_")}
    wanted = {f"gate_{n}" for n in names}
    return {k for k, v in gates.items() if v} == wanted


def all_on():
    return set(GATES)


class Fnd10Classification(unittest.TestCase):
    def test_fnd_10_docs_only_runs_no_build_scan_or_kind(self):
        r = run(["docs/wiki/Home.md", "README.md", "CHANGELOG.md", "SECURITY.md"])
        self.assertFalse(r["full"])
        self.assertTrue(on(r, "chart", "vendored_ui"))
        for gate in ("gate_image", "gate_e2e", "gate_e2e_load", "gate_lint", "gate_test"):
            self.assertFalse(r[gate], gate)

    def test_fnd_10_spec_only_runs_the_dashboard_and_chart_checks(self):
        # check-dashboards.py and its test read section 9 of docs/SPEC.md.
        r = run(["docs/SPEC.md"])
        self.assertFalse(r["full"])
        self.assertTrue(r["spec"] and r["docs"])
        self.assertTrue(on(r, "chart", "vendored_ui"))
        self.assertFalse(r["gate_image"])

    def test_fnd_10_chart_only_builds_and_runs_e2e(self):
        # e2e deploys the chart with the images, so a chart change needs both.
        r = run(["deploy/helm/mimic/values.yaml"])
        self.assertFalse(r["full"])
        self.assertTrue(on(r, "chart", "vendored_ui", "trivy_config", "image", "e2e", "e2e_load"))
        self.assertFalse(r["gate_lint"])
        self.assertFalse(r["gate_test"])
        self.assertFalse(r["gate_security"])

    def test_fnd_10_rust_only_runs_rust_checks_and_e2e(self):
        r = run(["crates/mimic-core/src/lib.rs"])
        self.assertFalse(r["full"])
        # The binaries are in the image, so the image and e2e follow the Rust code.
        self.assertTrue(
            on(
                r,
                "lint",
                "test",
                "codeql_rust",
                "security",
                "cooldown",
                "vendored_ui",
                "chart",
                "trivy_config",
                "image",
                "e2e",
                "e2e_load",
            )
        )
        self.assertFalse(r["gate_codeql_actions"])
        self.assertFalse(r["gate_fuzz_http1"])
        self.assertFalse(r["gate_fuzz_discovery"])

    def test_fnd_10_cargo_lock_runs_cooldown_and_security(self):
        r = run(["Cargo.lock"])
        self.assertTrue(r["gate_cooldown"] and r["gate_security"] and r["gate_lint"])

    def test_fnd_10_cargo_toml_also_runs_the_release_version_check(self):
        # check-release.sh compares the workspace version with the chart.
        r = run(["Cargo.toml"])
        self.assertTrue(r["gate_chart"])

    def test_fnd_10_workflow_change_runs_everything(self):
        r = run([".github/workflows/ci.yml"])
        self.assertTrue(r["full"])
        self.assertEqual(all_on(), {g for g in GATES if r[g]})

    def test_fnd_10_unknown_path_runs_everything(self):
        r = run(["docs/wiki/Home.md", "some/unknown/file.xyz"])
        self.assertTrue(r["other"])
        self.assertTrue(r["full"])
        self.assertEqual(all_on(), {g for g in GATES if r[g]})

    def test_fnd_10_prefix_matches_respect_path_boundaries(self):
        # "crates-old/x" is not under "crates/", so it is unknown and runs everything.
        self.assertTrue(run(["crates-old/x.rs"])["full"])
        self.assertTrue(run(["deployment/x.yaml"])["full"])
        # "docs-extra/x.txt" is not under "docs/" and has no .md suffix.
        self.assertTrue(run(["docs-extra/x.txt"])["full"])
        # "docs-extra/x.md" is a Markdown file, hence docs.
        self.assertFalse(run(["docs-extra/x.md"])["full"])

    def test_fnd_10_e2e_scripts_run_the_image_and_e2e_jobs(self):
        r = run(["scripts/e2e/apps/provider.py"])
        self.assertFalse(r["full"])
        self.assertTrue(r["gate_e2e"] and r["gate_image"] and r["gate_e2e_load"])
        self.assertFalse(r["gate_lint"])

    def test_fnd_10_codec_path_runs_only_the_codec_fuzz_job(self):
        r = run(["crates/mimic-core/src/proxy/http1/codec.rs"])
        self.assertTrue(r["gate_fuzz_http1"])
        self.assertFalse(r["gate_fuzz_discovery"])

    def test_fnd_10_proxy_rs_is_the_codec_fuzz_path_but_not_a_directory_prefix(self):
        self.assertTrue(run(["crates/mimic-core/src/proxy.rs"])["gate_fuzz_http1"])
        self.assertFalse(run(["crates/mimic-core/src/proxy_extra.rs"])["gate_fuzz_http1"])

    def test_fnd_10_discovery_path_runs_only_the_discovery_fuzz_jobs(self):
        r = run(["crates/mimic-core/src/mock/mod.rs"])
        self.assertTrue(r["gate_fuzz_discovery"])
        self.assertFalse(r["gate_fuzz_http1"])
        self.assertTrue(run(["crates/mimic-core/src/load/plan.rs"])["gate_fuzz_discovery"])

    def test_fnd_10_fuzz_crate_runs_both_fuzz_jobs(self):
        r = run(["fuzz/fuzz_targets/http1.rs"])
        self.assertTrue(r["gate_fuzz_http1"] and r["gate_fuzz_discovery"])

    def test_fnd_10_mixed_docs_and_rust_run_the_rust_gates(self):
        r = run(["docs/wiki/Home.md", "crates/mimic-controller/src/lib.rs"])
        self.assertTrue(r["gate_lint"] and r["gate_image"] and r["gate_chart"])
        self.assertFalse(r["gate_codeql_actions"])

    def test_fnd_10_push_schedule_and_dispatch_run_everything(self):
        for event in ("push", "schedule", "workflow_dispatch", "something_new"):
            with self.subTest(event=event):
                r = run(["docs/wiki/Home.md"], event=event)
                self.assertTrue(r["full"])
                self.assertEqual(all_on(), {g for g in GATES if r[g]})

    def test_fnd_10_empty_pull_request_runs_everything(self):
        r = run([])
        self.assertTrue(r["full"])
        self.assertEqual(all_on(), {g for g in GATES if r[g]})

    def test_fnd_10_every_category_and_gate_is_reported(self):
        r = run(["docs/wiki/Home.md"])
        for key in ("rust", "image", "chart", "e2e", "workflows", "docs", "spec", "other", "full"):
            self.assertIn(key, r)
        for gate in GATES:
            self.assertIn(gate, r)
            self.assertIsInstance(r[gate], bool)


class Fnd10Cli(unittest.TestCase):
    def run_cli(self, *args, paths=None):
        with tempfile.TemporaryDirectory() as d:
            cmd = [sys.executable, "-I", str(SCRIPT), *args]
            if paths is not None:
                changed = Path(d) / "changed.z"
                changed.write_bytes(b"\0".join(p.encode() for p in paths) + b"\0")
                cmd += ["--changed", str(changed)]
            return subprocess.run(cmd, capture_output=True, text=True, timeout=60)

    def test_fnd_10_cli_prints_key_value_lines_for_github_output(self):
        proc = self.run_cli("--event", "pull_request", paths=["docs/SPEC.md"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = dict(line.split("=", 1) for line in proc.stdout.splitlines() if line)
        self.assertEqual(lines["gate_lint"], "false")
        self.assertEqual(lines["gate_chart"], "true")
        self.assertEqual(lines["full"], "false")

    def test_fnd_10_cli_empty_list_runs_everything(self):
        proc = self.run_cli("--event", "pull_request", paths=[])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("full=true", proc.stdout.splitlines())

    def test_fnd_10_cli_pull_request_without_list_fails(self):
        proc = self.run_cli("--event", "pull_request")
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout, "")

    def test_fnd_10_cli_push_needs_no_list(self):
        proc = self.run_cli("--event", "push")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("gate_lint=true", proc.stdout.splitlines())


class Fnd10Workflow(unittest.TestCase):
    def test_fnd_10_workflow_reads_only_known_gates(self):
        # Every gate that ci.yml reads exists in the classifier, and every gate is read.
        text = CI_YML.read_text(encoding="utf-8")
        read = set(re.findall(r"needs\.changes\.outputs\.(\w+)", text))
        self.assertEqual(read, set(GATES), "gates read by ci.yml differ from ci_plan.GATES")

    def test_fnd_10_ci_ok_reads_every_gated_job(self):
        text = CI_YML.read_text(encoding="utf-8")
        ci_ok = text[text.index("\n  ci-ok:") :]
        for gate in GATES:
            self.assertIn(f"needs.changes.outputs.{gate}", ci_ok, gate)


if __name__ == "__main__":
    unittest.main()
