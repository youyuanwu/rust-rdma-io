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
import json
import os
import shutil
import subprocess
import sys
import uuid
from datetime import datetime, timezone
from typing import Callable, Dict, List, Optional, Sequence

import grid

# Resolve default paths from this file's location (repo root = two levels up
# from tests/benchv3/), so the tool works regardless of the caller's cwd.
_REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_INVENTORY = os.path.join(_REPO_ROOT, "tests/e2e/inventory_local.py")
DEFAULT_PLAYBOOK = os.path.join(_REPO_ROOT, "tests/e2e/playbooks/bench_run.yml")
DEFAULT_REBOOT_PLAYBOOK = os.path.join(_REPO_ROOT, "tests/e2e/playbooks/reboot_vms.yml")
DEFAULT_RESULTS_DIR = os.path.join(_REPO_ROOT, "tests/benchv3/results")


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
        help="Limit to these connection multiples (repeatable). Default: 1,2,4.",
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
        "--commit",
        default=None,
        help="Provenance commit to record (default: cwd git HEAD). Set this to the "
        "commit the deployed benchmark binary was built from when it differs from "
        "the tooling checkout.",
    )
    p.add_argument(
        "--reboot-between",
        action="store_true",
        help="Reboot the VMs at each sweep boundary (change of transport-path group).",
    )
    # Open-loop offered-load scenarios (echo / http1 only). When either is set the
    # default closed-loop grid is replaced by the corresponding coordinate set.
    p.add_argument(
        "--loaded-latency",
        action="store_true",
        help="Loaded tail-latency sweep: run one scenario across --rate levels at a "
        "fixed connection tier (requires --scenario echo|http1 and one or more --rate).",
    )
    p.add_argument(
        "--matched-throughput",
        action="store_true",
        help="Matched-throughput comparison: run every transport path at one shared "
        "--rate (requires --scenario echo|http1 and exactly one --rate).",
    )
    p.add_argument(
        "--rate",
        action="append",
        type=int,
        help="Open-loop target request rate(s) in req/s (repeatable for --loaded-latency).",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="List the planned coordinates and exit without running anything.",
    )
    return p


def plan_coordinates(args: argparse.Namespace) -> List[grid.Coordinate]:
    """Expand the grid with the CLI's subset filters applied (deduplicated).

    ``--loaded-latency`` / ``--matched-throughput`` select the open-loop
    offered-load coordinate sets instead of the default closed-loop grid.
    """
    if getattr(args, "loaded_latency", False) or getattr(args, "matched_throughput", False):
        if getattr(args, "loaded_latency", False) and getattr(args, "matched_throughput", False):
            raise SystemExit("use only one of --loaded-latency / --matched-throughput")
        scenarios = args.scenario or []
        if len(scenarios) != 1 or scenarios[0] not in grid.LOADED_LATENCY_SCENARIOS:
            raise SystemExit(
                "--loaded-latency / --matched-throughput require exactly one "
                f"--scenario from {grid.LOADED_LATENCY_SCENARIOS}"
            )
        rates = args.rate or []
        if not rates:
            raise SystemExit("--loaded-latency / --matched-throughput require --rate")
        scenario = scenarios[0]
        mult = (args.connections_mult or [1])[0]
        payloads = args.payload or grid.PAYLOADS
        coords: List[grid.Coordinate] = []
        for payload in payloads:
            if args.loaded_latency:
                coords += grid.expand_loaded_latency(
                    args.vcpu, scenario, rates, mult, payload,
                    transports=args.path_labels,
                )
            else:
                if len(rates) != 1:
                    raise SystemExit("--matched-throughput takes exactly one --rate")
                coords += grid.expand_matched_throughput(
                    args.vcpu, scenario, rates[0], mult, payload,
                    transports=args.path_labels,
                )
        return grid.dedupe(coords)

    coords = grid.expand(
        vcpu=args.vcpu,
        scenarios=args.scenario or grid.SCENARIOS,
        conn_mults=args.connections_mult or grid.CONNECTION_MULTIPLES,
        in_flights=args.in_flight,  # None -> per-scenario default
        payloads=args.payload or grid.PAYLOADS,
        path_labels=args.path_labels,
    )
    return grid.dedupe(coords)


def format_coordinate(c: grid.Coordinate) -> str:
    ring = f" ring_max_msg={c.ring_max_msg}" if c.ring_max_msg is not None else ""
    rps = f" target_rps={c.target_rps}" if c.target_rps is not None else ""
    return (
        f"{c.scenario:5s}  {c.path_label:34s}  "
        f"mode={c.mode:9s} transport={c.transport:11s} "
        f"conn={c.connections:<6d} thr={c.threads:<4d} "
        f"if={c.in_flight:<4d} payload={c.payload}B{ring}{rps}"
    )


def print_dry_run(coords: Sequence[grid.Coordinate]) -> None:
    for c in coords:
        print(format_coordinate(c))
    print(f"\n# {len(coords)} coordinate(s)")


# --- Execution engine -------------------------------------------------------

#: A launcher runs one coordinate's playbook. It returns the CompletedProcess-like
#: exit code. Injectable so tests can drive the engine without real ansible.
Launcher = Callable[[List[str]], int]


def _now_utc() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def _git_commit() -> str:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )
        return out.stdout.strip() or "nocommit"
    except OSError:
        return "nocommit"


def ansible_launcher(inventory: str, playbook: str) -> Launcher:
    """Default launcher: invoke ansible-playbook with the given -e vars."""

    def _run(extra_vars: List[str]) -> int:
        cmd = ["ansible-playbook", "-i", inventory, playbook]
        for kv in extra_vars:
            cmd += ["-e", kv]
        return subprocess.run(cmd, check=False).returncode

    return _run


def ansible_reboot(inventory: str, reboot_playbook: str) -> Callable[[], int]:
    def _reboot() -> int:
        cmd = ["ansible-playbook", "-i", inventory, reboot_playbook]
        return subprocess.run(cmd, check=False).returncode

    return _reboot


def _extra_vars(bench_vars: Dict[str, object], out_dir: str) -> List[str]:
    kvs = [f"{k}={v}" for k, v in bench_vars.items()]
    kvs.append(f"bench_out_dir={out_dir}")
    return kvs


def run_sweep(
    coords: Sequence[grid.Coordinate],
    results_dir: str,
    duration: int,
    warmup: int,
    vcpu: int,
    launcher: Launcher,
    reboot: Optional[Callable[[], int]] = None,
    run_id: Optional[str] = None,
    now_fn: Callable[[], str] = _now_utc,
    commit: Optional[str] = None,
) -> Dict[str, object]:
    """Drive each coordinate through the launcher, collecting collision-proof
    results and a run summary. Continues past individual failures.

    For each coordinate: a per-coordinate scratch dir under ``results_dir`` is
    used as ``bench_out_dir``; on success the single emitted ``bench-*.json`` is
    moved to the identity-named result file and a ``.meta.json`` provenance
    sidecar (carrying duration/warmup/vCPU, which the result JSON lacks) is
    written; the scratch dir is then removed. When ``reboot`` is provided the VMs
    are rebooted at each sweep boundary (a change of transport-path label).
    """
    os.makedirs(results_dir, exist_ok=True)
    run_id = run_id or uuid.uuid4().hex[:8]
    commit = commit if commit is not None else _git_commit()

    succeeded: List[str] = []
    failed: List[Dict[str, object]] = []
    reboot_failures: List[str] = []
    prev_group: Optional[tuple] = None

    for coord in coords:
        group = (coord.scenario, coord.path_label)
        if reboot is not None and prev_group is not None and group != prev_group:
            rc = reboot()
            if rc != 0:
                reboot_failures.append(f"before {coord.scenario}/{coord.path_label} (rc={rc})")
        prev_group = group

        utc = now_fn()
        scratch = os.path.join(results_dir, f".tmp-{run_id}", f"{utc}-{uuid.uuid4().hex[:6]}")
        os.makedirs(scratch, exist_ok=True)
        bench_vars = coord.bench_vars(duration=duration, warmup=warmup)
        coord_desc = format_coordinate(coord).strip()
        try:
            try:
                rc = launcher(_extra_vars(bench_vars, scratch))
            except Exception as exc:  # launcher/process failure must not abort the sweep
                failed.append({"coordinate": coord_desc, "exit_code": None,
                               "reason": f"launcher raised: {exc!r}"})
                continue
            produced = _find_result_json(scratch, coord)
            if rc == 0 and produced is not None:
                identity = grid.identity_filename(coord, utc, commit, run_id)
                dest = os.path.join(results_dir, identity)
                shutil.move(produced, dest)
                _write_meta(dest, coord, duration, warmup, vcpu, utc, commit, run_id, rc)
                succeeded.append(identity)
            else:
                failed.append({"coordinate": coord_desc, "exit_code": rc,
                               "reason": "nonzero exit" if rc != 0 else "no result file"})
        finally:
            shutil.rmtree(scratch, ignore_errors=True)

    _cleanup_tmp_root(results_dir, run_id)
    summary = {
        "run_id": run_id,
        "commit": commit,
        "total": len(coords),
        "succeeded": len(succeeded),
        "failed": len(failed),
        "failed_coordinates": failed,
        "reboot_failures": reboot_failures,
    }
    summary_path = os.path.join(results_dir, f"run-summary-{run_id}.json")
    with open(summary_path, "w") as fh:
        json.dump(summary, fh, indent=2)
    summary["summary_path"] = summary_path
    return summary


def _find_result_json(out_dir: str, coord: grid.Coordinate) -> Optional[str]:
    """Locate the playbook's result JSON in bench_out_dir.

    Prefer the exact expected basename the playbook writes
    (bench-<mode>-<transport>-<conns>conn-<threads>thr-<inflight>if.json); fall
    back to any single bench-*.json so a future filename tweak still collects.
    """
    if not os.path.isdir(out_dir):
        return None
    expected = (
        f"bench-{coord.mode}-{coord.transport}-{coord.connections}conn-"
        f"{coord.threads}thr-{coord.in_flight}if.json"
    )
    exact = os.path.join(out_dir, expected)
    if os.path.exists(exact):
        return exact
    hits = [
        os.path.join(out_dir, f)
        for f in os.listdir(out_dir)
        if f.startswith("bench-") and f.endswith(".json")
    ]
    return hits[0] if hits else None


def _write_meta(
    result_path: str,
    coord: grid.Coordinate,
    duration: int,
    warmup: int,
    vcpu: int,
    utc: str,
    commit: str,
    run_id: str,
    exit_code: int,
) -> None:
    """Provenance sidecar carrying what the result JSON does not record."""
    meta = {
        "scenario": coord.scenario,
        "path_label": coord.path_label,
        "mode": coord.mode,
        "transport": coord.transport,
        "connection_mult": coord.connection_mult,
        "connections": coord.connections,
        "threads": coord.threads,
        "in_flight": coord.in_flight,
        "payload": coord.payload,
        "ring_max_msg": coord.ring_max_msg,
        "target_rps": coord.target_rps,
        "duration": duration,
        "warmup": warmup,
        "vcpu": vcpu,
        "utc": utc,
        "commit": commit,
        "run_id": run_id,
        "ansible_exit": exit_code,
    }
    meta_path = result_path[: -len(".json")] + ".meta.json"
    with open(meta_path, "w") as fh:
        json.dump(meta, fh, indent=2)


def _cleanup_tmp_root(results_dir: str, run_id: str) -> None:
    shutil.rmtree(os.path.join(results_dir, f".tmp-{run_id}"), ignore_errors=True)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    coords = plan_coordinates(args)
    if not coords:
        print("no coordinates matched the given filters", file=sys.stderr)
        return 2

    if args.dry_run:
        print_dry_run(coords)
        return 0

    reboot = (
        ansible_reboot(args.inventory, args.reboot_playbook)
        if args.reboot_between
        else None
    )
    summary = run_sweep(
        coords,
        results_dir=args.results_dir,
        duration=args.duration,
        warmup=args.warmup,
        vcpu=args.vcpu,
        launcher=ansible_launcher(args.inventory, args.playbook),
        reboot=reboot,
        commit=args.commit,
    )
    print(
        f"\n# run {summary['run_id']}: {summary['succeeded']}/{summary['total']} "
        f"succeeded, {summary['failed']} failed"
    )
    for f in summary["failed_coordinates"]:
        print(f"#   FAILED: {f['coordinate']} ({f['reason']}, rc={f['exit_code']})",
              file=sys.stderr)
    for rf in summary.get("reboot_failures", []):
        print(f"#   REBOOT FAILED: {rf}", file=sys.stderr)
    print(f"# summary: {summary['summary_path']}")
    return 0 if summary["failed"] == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
