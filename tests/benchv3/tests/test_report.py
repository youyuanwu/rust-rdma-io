"""Unit tests for the bench v3 report generator (report.py).

Feeds hand-written sample result JSON (+ meta sidecars) through the renderers
and asserts table headers are verbatim, rows land on the right template rows via
(mode,transport) reconstruction, derived metrics compute, and missing data
degrades to blank/n/a without error.
"""

import json
import os
import sys
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(_HERE))

import grid  # noqa: E402
import report  # noqa: E402


def _result(mode, transport, connections, threads, in_flight, payload,
            rps=1000.0, p50=10.0, p95=20.0, p99=30.0,
            cpu_seconds=1.0, cpu_us_per_op=5.0, peak_rss_kb=2048, errors=0,
            duration_secs=10.0):
    d = {
        "mode": mode, "transport": transport, "connections": connections,
        "threads": threads, "in_flight": in_flight, "payload_bytes": payload,
        "duration_secs": duration_secs, "total_requests": int(rps * duration_secs),
        "throughput_rps": rps, "errors": errors,
        "latency_us": {"p50": p50, "p95": p95, "p99": p99, "p999": p99 * 2,
                       "min": 1.0, "max": p99 * 3, "avg": p50},
    }
    if cpu_seconds is not None:
        d["cpu_seconds"] = cpu_seconds
    if cpu_us_per_op is not None:
        d["cpu_us_per_op"] = cpu_us_per_op
    if peak_rss_kb is not None:
        d["peak_rss_kb"] = peak_rss_kb
    return d


def _meta(scenario, path_label, mode, transport, mult, connections, threads,
          in_flight, payload, duration=10, warmup=3, vcpu=64):
    return {
        "scenario": scenario, "path_label": path_label, "mode": mode,
        "transport": transport, "connection_mult": mult, "connections": connections,
        "threads": threads, "in_flight": in_flight, "payload": payload,
        "duration": duration, "warmup": warmup, "vcpu": vcpu,
        "commit": "abc1234", "run_id": "r1", "utc": "20260723T000000Z",
    }


class TestReport(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmp, ignore_errors=True))

    def _write(self, name, data, meta=None):
        with open(os.path.join(self.tmp, name + ".json"), "w") as fh:
            json.dump(data, fh)
        if meta is not None:
            with open(os.path.join(self.tmp, name + ".meta.json"), "w") as fh:
                json.dump(meta, fh)

    def _seed_echo_coordinate(self, payload=64, in_flight=64, mult=1, vcpu=64):
        """Write results for all echo read-ring topologies + send-recv at a coord."""
        conns = mult * vcpu
        specs = [
            ("send-recv", "echo", "send-recv"),
            ("read-ring (arm-park)", "echo", "read-ring"),
            ("read-ring (busy-poll)", "echo-busy", "read-ring"),
            ("read-ring (thread-per-core park)", "echo-park", "read-ring"),
            ("credit-ring", "echo", "credit-ring"),
            ("kernel baseline", "echo", "tcp"),
        ]
        for i, (label, mode, transport) in enumerate(specs):
            self._write(
                f"r-{i}",
                _result(mode, transport, conns, vcpu, in_flight, payload, rps=1000.0 + i),
                _meta("echo", label, mode, transport, mult, conns, vcpu, in_flight, payload),
            )

    def test_table_a_64_headers_verbatim(self):
        self._seed_echo_coordinate(payload=64)
        recs = report.load_records(self.tmp)
        out = report.render_table_a(recs, "echo", 64, 1, 64)
        self.assertIn(report.TABLE_A_HEADER_64, out)
        self.assertIn(report.TABLE_A_SEP_64, out)
        self.assertNotIn("Gbps", out)

    def test_table_a_8k_has_gbps_header_verbatim(self):
        self._seed_echo_coordinate(payload=8192)
        recs = report.load_records(self.tmp)
        out = report.render_table_a(recs, "echo", 8192, 1, 64)
        self.assertIn(report.TABLE_A_HEADER_8K, out)
        self.assertIn(report.TABLE_A_SEP_8K, out)
        # Gbps derived = rps * 8192 * 8 / 1e9 for the send-recv row (rps=1000).
        self.assertIn("0.07", out)  # 1000*8192*8/1e9 = 0.0655 -> 0.07

    def test_readring_topologies_land_on_distinct_rows(self):
        self._seed_echo_coordinate(payload=64)
        recs = report.load_records(self.tmp)
        out = report.render_table_a(recs, "echo", 64, 1, 64)
        # All six template rows present, busy/park distinguished though both
        # report transport=read-ring.
        self.assertIn("`read-ring` (arm-park)", out)
        self.assertIn("`read-ring` (busy-poll)", out)
        self.assertIn("`read-ring` (thread-per-core park)", out)
        self.assertIn("`send-recv`", out)
        self.assertIn("`credit-ring`", out)
        self.assertIn("kernel baseline", out)

    def test_derived_cores_and_caption_from_meta(self):
        self._seed_echo_coordinate(payload=64)
        recs = report.load_records(self.tmp)
        out = report.render_table_a(recs, "echo", 64, 1, 64)
        # cores = cpu_seconds(1.0)/duration(10) = 0.10
        self.assertIn("0.10", out)
        # caption pulls duration/warmup/vCPU from meta
        self.assertIn("**vCPU:** 64", out)
        self.assertIn("**duration/warmup:** 10 s / 3 s", out)

    def test_missing_optional_fields_blank_not_crash(self):
        # One result with no cpu/rss; must render blank cells, no error.
        self._write(
            "only",
            _result("echo", "send-recv", 64, 64, 64, 64,
                    cpu_seconds=None, cpu_us_per_op=None, peak_rss_kb=None),
            _meta("echo", "send-recv", "echo", "send-recv", 1, 64, 64, 64, 64),
        )
        recs = report.load_records(self.tmp)
        out = report.render_table_a(recs, "echo", 64, 1, 64)
        self.assertIn(report.TABLE_A_HEADER_64, out)  # rendered ok
        # send-recv row has throughput but blank cpu/cores/rss.
        sr = [ln for ln in out.splitlines() if ln.startswith("| `send-recv`")][0]
        self.assertIn("1000", sr)

    def test_absent_coordinate_renders_na_row(self):
        # No results at all: every row is n/a, no crash.
        recs = report.load_records(self.tmp)
        out = report.render_table_a(recs, "grpc", 64, 4, 512)
        self.assertIn(report.TABLE_A_HEADER_64, out)
        self.assertIn("`n/a`", out)
        # gRPC Table A has 4 path rows (no busy/park).
        rows = [ln for ln in out.splitlines() if ln.startswith("| ")]
        # header + separator + 4 path rows = 6 pipe-led lines
        self.assertEqual(len([r for r in rows if "n/a" in r]), 4)

    def test_table_b_headers_and_rows(self):
        # Seed echo read-ring across the whole concurrency grid for payload 64.
        for mult in (1, 4, 16):
            for nf in (1, 64, 512):
                conns = mult * 64
                self._write(
                    f"b-{mult}-{nf}",
                    _result("echo", "read-ring", conns, 64, nf, 64, rps=100.0 * mult),
                    _meta("echo", "read-ring (arm-park)", "echo", "read-ring",
                          mult, conns, 64, nf, 64),
                )
        recs = report.load_records(self.tmp)
        out = report.render_table_b(recs, "echo", 64, "read-ring (arm-park)")
        self.assertIn(report.TABLE_B_HEADER_64, out)
        self.assertIn(report.TABLE_B_SEP_64, out)
        # 9 data rows for echo.
        data_rows = [ln for ln in out.splitlines() if ln.startswith("| 1× vCPU")
                     or ln.startswith("| 4× vCPU") or ln.startswith("| 16× vCPU")]
        self.assertEqual(len(data_rows), 9)

    def test_table_b_http1_three_rows(self):
        recs = report.load_records(self.tmp)  # empty is fine, structure only
        out = report.render_table_b(recs, "http1", 64, "send-recv")
        data_rows = [ln for ln in out.splitlines() if "× vCPU" in ln]
        self.assertEqual(len(data_rows), 3)  # in-flight always 1

    def test_table_b_8k_gbps_header(self):
        recs = report.load_records(self.tmp)
        out = report.render_table_b(recs, "echo", 8192, "send-recv")
        self.assertIn(report.TABLE_B_HEADER_8K, out)
        self.assertIn(report.TABLE_B_SEP_8K, out)

    def test_all_batch_emits_tables_and_excludes_meta_and_summary(self):
        self._seed_echo_coordinate(payload=64, in_flight=1, mult=1)
        # Sidecar + run-summary must be ignored by the loader.
        with open(os.path.join(self.tmp, "run-summary-r1.json"), "w") as fh:
            json.dump({"total": 6}, fh)
        recs = report.load_records(self.tmp)
        # 6 results, not counting meta/summary.
        self.assertEqual(len(recs), 6)
        out = report.render_all(recs, "echo", 64)
        self.assertIn("### Table A", out)
        self.assertIn("### Table B", out)
        # Table A appears once per (mult,in_flight) coordinate for this payload = 9.
        self.assertEqual(out.count(report.TABLE_A_HEADER_64), 9)
        # Table B appears once per echo path = 6.
        self.assertEqual(out.count(report.TABLE_B_HEADER_64), 6)


if __name__ == "__main__":
    unittest.main()
