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
    prev_label: Optional[str] = None

    for coord in coords:
        if reboot is not None and prev_label is not None and coord.path_label != prev_label:
            reboot()
        prev_label = coord.path_label

        utc = now_fn()
        scratch = os.path.join(results_dir, f".tmp-{run_id}", f"{utc}-{uuid.uuid4().hex[:6]}")
        os.makedirs(scratch, exist_ok=True)
        bench_vars = coord.bench_vars(duration=duration, warmup=warmup)
        coord_desc = format_coordinate(coord).strip()
        try:
            rc = launcher(_extra_vars(bench_vars, scratch))
            produced = _find_result_json(scratch)
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
    }
    summary_path = os.path.join(results_dir, f"run-summary-{run_id}.json")
    with open(summary_path, "w") as fh:
        json.dump(summary, fh, indent=2)
    summary["summary_path"] = summary_path
    return summary


def _find_result_json(out_dir: str) -> Optional[str]:
    """The playbook writes exactly one bench-*.json into bench_out_dir."""
    if not os.path.isdir(out_dir):
        return None
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
    )
    print(
        f"\n# run {summary['run_id']}: {summary['succeeded']}/{summary['total']} "
        f"succeeded, {summary['failed']} failed"
    )
    for f in summary["failed_coordinates"]:
        print(f"#   FAILED: {f['coordinate']} ({f['reason']}, rc={f['exit_code']})",
              file=sys.stderr)
    print(f"# summary: {summary['summary_path']}")
    return 0 if summary["failed"] == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
