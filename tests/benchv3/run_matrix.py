#!/usr/bin/env python3
"""Bench v3 grid runner — expand the fixed-workload grid and (Phase 2) drive it.

Phase 1 provides grid expansion, subset selection, and ``--dry-run`` listing of
the planned coordinates. The execution engine that drives the coordinates
through the e2e orchestration is added in Phase 2.

Examples:
    # Preview the echo coordinates for a 64-vCPU SKU (nothing is run):
    python3 tests/benchv3/run_matrix.py --vcpu 64 --scenario echo --dry-run
"""

from __future__ import annotations

import argparse
import sys
from typing import List, Optional, Sequence

import grid

DEFAULT_INVENTORY = "tests/e2e/inventory_local.py"
DEFAULT_PLAYBOOK = "tests/e2e/playbooks/bench_run.yml"
DEFAULT_REBOOT_PLAYBOOK = "tests/e2e/playbooks/reboot_vms.yml"
DEFAULT_RESULTS_DIR = "tests/benchv3/results"


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="run_matrix.py",
        description="Expand and run the bench v3 fixed-workload grid.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    p.add_argument(
        "--vcpu",
        type=int,
        required=True,
        help="Target VM vCPU count. Sets threads (=vcpu) and connections (=mult*vcpu).",
    )
    # Subset selection (default: the full grid).
    p.add_argument(
        "--scenario",
        action="append",
        choices=grid.SCENARIOS,
        help="Limit to these scenarios (repeatable). Default: all.",
    )
    p.add_argument(
        "--path",
        action="append",
        dest="path_labels",
        help="Limit to these transport-path labels (repeatable). Default: all.",
    )
    p.add_argument(
        "--connections-mult",
        action="append",
        type=int,
        choices=grid.CONNECTION_MULTIPLES,
        help="Limit to these connection multiples (repeatable). Default: 1,4,16.",
    )
    p.add_argument(
        "--in-flight",
        action="append",
        type=int,
        help="Limit to these in-flight depths (repeatable). Default: per-scenario set.",
    )
    p.add_argument(
        "--payload",
        action="append",
        type=int,
        choices=grid.PAYLOADS,
        help="Limit to these payload sizes in bytes (repeatable). Default: 64,8192.",
    )
    # Fixed run controls.
    p.add_argument("--duration", type=int, default=10, help="Measurement seconds.")
    p.add_argument("--warmup", type=int, default=3, help="Warmup seconds.")
    # Execution wiring (used by the Phase 2 engine).
    p.add_argument("--results-dir", default=DEFAULT_RESULTS_DIR)
    p.add_argument("--inventory", default=DEFAULT_INVENTORY)
    p.add_argument("--playbook", default=DEFAULT_PLAYBOOK)
    p.add_argument("--reboot-playbook", default=DEFAULT_REBOOT_PLAYBOOK)
    p.add_argument(
        "--reboot-between",
        action="store_true",
        help="Reboot the VMs at each sweep boundary (change of transport-path group).",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="List the planned coordinates and exit without running anything.",
    )
    return p


def plan_coordinates(args: argparse.Namespace) -> List[grid.Coordinate]:
    """Expand the grid with the CLI's subset filters applied."""
    return grid.expand(
        vcpu=args.vcpu,
        scenarios=args.scenario or grid.SCENARIOS,
        conn_mults=args.connections_mult or grid.CONNECTION_MULTIPLES,
        in_flights=args.in_flight,  # None -> per-scenario default
        payloads=args.payload or grid.PAYLOADS,
        path_labels=args.path_labels,
    )


def format_coordinate(c: grid.Coordinate) -> str:
    ring = f" ring_max_msg={c.ring_max_msg}" if c.ring_max_msg is not None else ""
    return (
        f"{c.scenario:5s}  {c.path_label:34s}  "
        f"mode={c.mode:9s} transport={c.transport:11s} "
        f"conn={c.connections:<6d} thr={c.threads:<4d} "
        f"if={c.in_flight:<4d} payload={c.payload}B{ring}"
    )


def print_dry_run(coords: Sequence[grid.Coordinate]) -> None:
    for c in coords:
        print(format_coordinate(c))
    print(f"\n# {len(coords)} coordinate(s)")


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    coords = plan_coordinates(args)
    if not coords:
        print("no coordinates matched the given filters", file=sys.stderr)
        return 2

    if args.dry_run:
        print_dry_run(coords)
        return 0

    # The execution engine (drive bench_run.yml per coordinate, rename results,
    # continue-on-failure, optional reboot-between) is implemented in Phase 2.
    print(
        "execution engine not yet available; re-run with --dry-run to preview "
        "the planned coordinates",
        file=sys.stderr,
    )
    return 3


if __name__ == "__main__":
    raise SystemExit(main())
