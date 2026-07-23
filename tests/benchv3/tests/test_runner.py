"""Unit tests for the run_matrix execution engine using a fake launcher.

No real ansible or VMs: the launcher is injected and simulates the playbook by
writing a dummy ``bench-*.json`` into the given ``bench_out_dir``.
"""

import json
import os
import sys
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(_HERE))

import grid  # noqa: E402
import run_matrix  # noqa: E402


def _out_dir_from_extra_vars(extra_vars):
    for kv in extra_vars:
        if kv.startswith("bench_out_dir="):
            return kv.split("=", 1)[1]
    raise AssertionError("bench_out_dir not passed to launcher")


def fake_launcher_ok(extra_vars):
    """Simulate a successful playbook: drop a dummy bench-*.json."""
    out = _out_dir_from_extra_vars(extra_vars)
    os.makedirs(out, exist_ok=True)
    payload = {k: v for k, v in (kv.split("=", 1) for kv in extra_vars)}
    with open(os.path.join(out, "bench-dummy.json"), "w") as fh:
        json.dump({"throughput_rps": 1.0, "vars": payload}, fh)
    return 0


def fail_on_credit_ring(extra_vars):
    """Succeed except for credit-ring coordinates (simulated failure)."""
    kv = {k: v for k, v in (x.split("=", 1) for x in extra_vars)}
    if kv.get("bench_transport") == "credit-ring":
        return 1  # non-zero, and writes no result file
    return fake_launcher_ok(extra_vars)


class TestRunSweep(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmp, ignore_errors=True))

    def _coords(self, **kw):
        return grid.expand(64, scenarios=["echo"], **kw)

    def test_rename_to_identity_and_meta(self):
        coords = self._coords(conn_mults=[1], in_flights=[1], payloads=[64],
                              path_labels=["send-recv"])
        summary = run_matrix.run_sweep(
            coords, results_dir=self.tmp, duration=10, warmup=3, vcpu=64,
            launcher=fake_launcher_ok, run_id="testrun", now_fn=lambda: "20260723T000000Z",
            commit="abc123",
        )
        self.assertEqual(summary["succeeded"], 1)
        self.assertEqual(summary["failed"], 0)
        files = sorted(os.listdir(self.tmp))
        results = [f for f in files if f.endswith(".json") and not f.endswith(".meta.json")
                   and not f.startswith("run-summary")]
        metas = [f for f in files if f.endswith(".meta.json")]
        self.assertEqual(len(results), 1)
        self.assertEqual(len(metas), 1)
        self.assertTrue(results[0].startswith("bench-echo-send-recv-"))
        self.assertIn("64B", results[0])
        # No dummy filename leaks; scratch dir removed.
        self.assertNotIn("bench-dummy.json", files)
        self.assertFalse(any(f.startswith(".tmp-") for f in files))
        # Meta carries duration/warmup/vcpu that the result JSON lacks.
        with open(os.path.join(self.tmp, metas[0])) as fh:
            meta = json.load(fh)
        self.assertEqual(meta["duration"], 10)
        self.assertEqual(meta["warmup"], 3)
        self.assertEqual(meta["vcpu"], 64)

    def test_payload_variants_do_not_collide(self):
        coords = self._coords(conn_mults=[1], in_flights=[1], payloads=[64, 8192],
                              path_labels=["read-ring (arm-park)"])
        run_matrix.run_sweep(
            coords, results_dir=self.tmp, duration=10, warmup=3, vcpu=64,
            launcher=fake_launcher_ok, run_id="r", now_fn=lambda: "20260723T000000Z",
            commit="abc",
        )
        results = [f for f in os.listdir(self.tmp)
                   if f.endswith(".json") and not f.endswith(".meta.json")
                   and not f.startswith("run-summary")]
        self.assertEqual(len(results), 2)  # 64B and 8192B, distinct
        self.assertTrue(any("64B" in f for f in results))
        self.assertTrue(any("8192B" in f for f in results))

    def test_failure_does_not_abort_and_is_summarized(self):
        coords = self._coords(conn_mults=[1], in_flights=[1], payloads=[64])
        summary = run_matrix.run_sweep(
            coords, results_dir=self.tmp, duration=10, warmup=3, vcpu=64,
            launcher=fail_on_credit_ring, run_id="r", now_fn=lambda: "t",
            commit="abc",
        )
        # 6 echo paths at this coordinate; credit-ring fails, other 5 succeed.
        self.assertEqual(summary["total"], 6)
        self.assertEqual(summary["succeeded"], 5)
        self.assertEqual(summary["failed"], 1)
        self.assertIn("credit-ring", summary["failed_coordinates"][0]["coordinate"])
        # Summary persisted to disk.
        self.assertTrue(os.path.exists(summary["summary_path"]))

    def test_scratch_dirs_cleaned_up(self):
        coords = self._coords(conn_mults=[1], in_flights=[1], payloads=[64],
                              path_labels=["send-recv"])
        run_matrix.run_sweep(
            coords, results_dir=self.tmp, duration=10, warmup=3, vcpu=64,
            launcher=fake_launcher_ok, run_id="cleanme", now_fn=lambda: "t", commit="c",
        )
        self.assertFalse(os.path.exists(os.path.join(self.tmp, ".tmp-cleanme")))


if __name__ == "__main__":
    unittest.main()
