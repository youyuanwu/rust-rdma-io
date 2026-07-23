"""Unit tests for the bench v3 grid model (grid.py). Stdlib-only, no VMs."""

import os
import sys
import unittest

# Import grid.py from the parent tests/benchv3 directory.
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(_HERE))

import grid  # noqa: E402


VCPU = 64


class TestPathTable(unittest.TestCase):
    def test_path_counts_per_scenario(self):
        self.assertEqual(len(grid.paths_for("echo")), 6)
        self.assertEqual(len(grid.paths_for("grpc")), 4)
        self.assertEqual(len(grid.paths_for("http1")), 6)

    def test_no_grpc_busy_or_park(self):
        # gRPC has arm-park (the default read-ring) but NO busy-poll or
        # thread-per-core park variant (those are echo/HTTP-1.1 only).
        for p in grid.paths_for("grpc"):
            self.assertNotIn("busy", p.mode)
            self.assertNotIn("park", p.mode)  # e.g. echo-park/rh1-park modes
            self.assertNotIn("busy-poll", p.label)
            self.assertNotIn("thread-per-core", p.label)

    def test_path_label_roundtrip(self):
        # Every path's (mode, transport) reconstructs its own label + scenario.
        for p in grid.PATHS:
            self.assertEqual(grid.path_label(p.mode, p.transport), p.label)
            self.assertEqual(grid.scenario_of(p.mode, p.transport), p.scenario)

    def test_read_ring_topologies_disambiguated(self):
        # arm/busy/park all report transport=read-ring but differ by mode.
        self.assertEqual(
            grid.path_label("echo", "read-ring"), "read-ring (arm-park)"
        )
        self.assertEqual(
            grid.path_label("echo-busy", "read-ring"), "read-ring (busy-poll)"
        )
        self.assertEqual(
            grid.path_label("echo-park", "read-ring"),
            "read-ring (thread-per-core park)",
        )

    def test_baselines_disambiguated_by_mode(self):
        self.assertEqual(grid.path_label("echo", "tcp"), "kernel baseline")
        self.assertEqual(grid.scenario_of("echo", "tcp"), "echo")
        self.assertEqual(grid.scenario_of("tcp", "tcp"), "grpc")
        self.assertEqual(grid.scenario_of("tcp1", "tcp"), "http1")

    def test_unknown_mode_transport_returns_none(self):
        self.assertIsNone(grid.path_label("rh2", "tcp"))  # invalid combo
        self.assertIsNone(grid.lookup_path("bogus", "send-recv"))


class TestExpand(unittest.TestCase):
    def test_full_cardinality(self):
        # echo: 6 paths x 3 conn x 3 in-flight x 2 payload = 108
        echo = grid.expand(VCPU, scenarios=["echo"])
        self.assertEqual(len(echo), 108)
        self.assertEqual(len({c.path_label for c in echo}), 6)
        self.assertEqual(
            len({(c.connection_mult, c.in_flight, c.payload) for c in echo}), 18
        )
        # gRPC: 4 paths x 18 = 72
        self.assertEqual(len(grid.expand(VCPU, scenarios=["grpc"])), 72)
        # HTTP/1.1: in-flight pinned to 1 -> 6 paths x 3 conn x 1 x 2 payload = 36
        http1 = grid.expand(VCPU, scenarios=["http1"])
        self.assertEqual(len(http1), 36)
        self.assertEqual({c.in_flight for c in http1}, {1})

    def test_http1_never_runs_deep_inflight(self):
        # Even if the caller asks for 512, HTTP/1.1 stays at in-flight 1.
        http1 = grid.expand(VCPU, scenarios=["http1"], in_flights=[1, 64, 512])
        self.assertEqual({c.in_flight for c in http1}, {1})

    def test_no_grpc_busy_park_coordinates(self):
        grpc = grid.expand(VCPU, scenarios=["grpc"])
        for c in grpc:
            self.assertNotIn("busy", c.mode)
            self.assertNotIn("park", c.mode)

    def test_connections_and_threads(self):
        for c in grid.expand(VCPU, scenarios=["echo"]):
            self.assertEqual(c.threads, VCPU)
            self.assertEqual(c.connections, c.connection_mult * VCPU)

    def test_ring_max_msg_only_for_large_ring(self):
        for c in grid.expand(VCPU):
            if c.payload == grid.LARGE_PAYLOAD and c.transport in grid.RING_TRANSPORTS:
                self.assertEqual(c.ring_max_msg, grid.RING_MAX_MSG_LARGE)
            else:
                self.assertIsNone(c.ring_max_msg)

    def test_ring_max_msg_absent_for_sendrecv_and_baseline_and_small(self):
        for c in grid.expand(VCPU):
            if c.transport in ("send-recv", "tcp"):
                self.assertIsNone(c.ring_max_msg)  # never on send-recv / baseline
            if c.payload == 64:
                self.assertIsNone(c.ring_max_msg)  # never for 64 B

    def test_bench_vars_include_ring_only_when_set(self):
        big = [
            c
            for c in grid.expand(VCPU, scenarios=["echo"])
            if c.transport == "read-ring" and c.payload == 8192
        ][0]
        v = big.bench_vars(duration=10, warmup=3)
        self.assertEqual(v["bench_ring_max_msg"], 8192)
        self.assertEqual(v["bench_mode"], "echo")
        self.assertEqual(v["bench_duration"], 10)
        self.assertEqual(v["bench_warmup"], 3)
        small = [c for c in grid.expand(VCPU, scenarios=["echo"]) if c.payload == 64][0]
        self.assertNotIn("bench_ring_max_msg", small.bench_vars(10, 3))

    def test_subset_filters(self):
        coords = grid.expand(
            VCPU,
            scenarios=["echo"],
            conn_mults=[1],
            in_flights=[64],
            payloads=[64],
            path_labels=["send-recv", "credit-ring"],
        )
        self.assertEqual({c.path_label for c in coords}, {"send-recv", "credit-ring"})
        self.assertEqual({c.connection_mult for c in coords}, {1})
        self.assertEqual({c.in_flight for c in coords}, {64})
        self.assertEqual({c.payload for c in coords}, {64})
        self.assertEqual(len(coords), 2)

    def test_invalid_vcpu(self):
        with self.assertRaises(ValueError):
            grid.expand(0, scenarios=["echo"])


class TestIdentityFilename(unittest.TestCase):
    def _coord(self, payload=64, in_flight=1, mult=1):
        return grid.expand(
            VCPU,
            scenarios=["echo"],
            conn_mults=[mult],
            in_flights=[in_flight],
            payloads=[payload],
            path_labels=["read-ring (arm-park)"],
        )[0]

    def test_payload_makes_names_distinct(self):
        a = grid.identity_filename(self._coord(payload=64), "20260723T000000Z", "abc", "r1")
        b = grid.identity_filename(self._coord(payload=8192), "20260723T000000Z", "abc", "r1")
        self.assertNotEqual(a, b)
        self.assertIn("64B", a)
        self.assertIn("8192B", b)

    def test_run_id_makes_names_distinct(self):
        c = self._coord()
        a = grid.identity_filename(c, "20260723T000000Z", "abc", "r1")
        b = grid.identity_filename(c, "20260723T000000Z", "abc", "r2")
        self.assertNotEqual(a, b)

    def test_coordinate_makes_names_distinct(self):
        a = grid.identity_filename(self._coord(mult=1), "t", "abc", "r1")
        b = grid.identity_filename(self._coord(mult=4), "t", "abc", "r1")
        self.assertNotEqual(a, b)

    def test_slug_is_filesystem_safe(self):
        name = grid.identity_filename(
            self._coord(), "20260723T000000Z", "abc123", "run-1"
        )
        # No spaces or parens leak from the path label into the filename.
        self.assertNotIn(" ", name)
        self.assertNotIn("(", name)
        self.assertTrue(name.endswith(".json"))


if __name__ == "__main__":
    unittest.main()
