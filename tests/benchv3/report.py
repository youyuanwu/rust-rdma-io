#!/usr/bin/env python3
"""Bench v3 report generator — collected result JSON -> paste-ready v3 tables.

Reads the identity-named ``bench-*.json`` result files produced by
``run_matrix.py`` (and their ``.meta.json`` provenance sidecars) and emits the
Table A / Table B Markdown defined in ``docs/benchv3/results-template.md``,
ready to paste into a curated results doc. Derived metrics (`cores`, `Gbps`) are
computed from the result fields; missing optional fields and absent coordinates
degrade to blank / ``n/a`` rather than failing.

Examples:
    # One Table A for a coordinate:
    python3 tests/benchv3/report.py --results-dir tests/benchv3/results \
        --table a --scenario echo --payload 64 --connections-mult 1 --in-flight 64

    # A full board (every Table A + Table B) for a scenario+payload:
    python3 tests/benchv3/report.py --results-dir tests/benchv3/results \
        --all --scenario echo --payload 64
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import sys
from typing import Dict, List, Optional, Sequence, Tuple

import grid

# --- Verbatim table headers (docs/benchv3/results-template.md) ---------------

TABLE_A_HEADER_64 = (
    "| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) "
    "| CPU/op (µs) | cores | peak RSS (MB) | errors |"
)
TABLE_A_SEP_64 = (
    "| -------------- | ------------------ | -------- | -------- | -------- "
    "| ----------- | ----- | ------------- | ------ |"
)
TABLE_A_HEADER_8K = (
    "| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) "
    "| CPU/op (µs) | cores | peak RSS (MB) | errors |"
)
TABLE_A_SEP_8K = (
    "| -------------- | ------------------ | ---- | -------- | -------- | -------- "
    "| ----------- | ----- | ------------- | ------ |"
)
TABLE_B_HEADER_64 = (
    "| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) "
    "| CPU/op (µs) | cores | peak RSS (MB) | errors |"
)
TABLE_B_SEP_64 = (
    "| ----------- | --------- | ------------------ | -------- | -------- | -------- "
    "| ----------- | ----- | ------------- | ------ |"
)
TABLE_B_HEADER_8K = (
    "| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) "
    "| CPU/op (µs) | cores | peak RSS (MB) | errors |"
)
TABLE_B_SEP_8K = (
    "| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- "
    "| ----------- | ----- | ------------- | ------ |"
)

_SCENARIO_TITLE = {"echo": "echo", "grpc": "gRPC", "http1": "HTTP/1.1"}


# --- Loading ----------------------------------------------------------------


class Record:
    """One parsed result JSON + its optional meta sidecar."""

    def __init__(self, data: Dict, meta: Optional[Dict]):
        self.data = data
        self.meta = meta or {}
        self.mode = data.get("mode", "")
        self.transport = data.get("transport", "")
        self.scenario = self.meta.get("scenario") or grid.scenario_of(self.mode, self.transport)
        self.path_label = self.meta.get("path_label") or grid.path_label(self.mode, self.transport)
        self.connections = int(data.get("connections", 0))
        self.threads = int(data.get("threads", 0))
        self.in_flight = int(data.get("in_flight", 0))
        self.payload = int(data.get("payload_bytes", self.meta.get("payload", 0)))
        cm = self.meta.get("connection_mult")
        if cm is None and self.threads:
            cm = round(self.connections / self.threads)
        self.connection_mult = cm

    @property
    def key(self) -> Tuple:
        return (self.scenario, self.path_label, self.connection_mult, self.in_flight, self.payload)


def load_records(results_dir: str) -> List[Record]:
    """Load all result JSON files, excluding meta sidecars and run summaries."""
    records: List[Record] = []
    for path in sorted(glob.glob(os.path.join(results_dir, "*.json"))):
        base = os.path.basename(path)
        if base.endswith(".meta.json") or base.startswith("run-summary"):
            continue
        try:
            with open(path) as fh:
                data = json.load(fh)
        except (OSError, json.JSONDecodeError):
            continue
        meta = None
        meta_path = path[: -len(".json")] + ".meta.json"
        if os.path.exists(meta_path):
            try:
                with open(meta_path) as fh:
                    meta = json.load(fh)
            except (OSError, json.JSONDecodeError):
                meta = None
        records.append(Record(data, meta))
    return records


def index(records: Sequence[Record]) -> Dict[Tuple, Record]:
    return {r.key: r for r in records}


# --- Cell formatting --------------------------------------------------------

NA = "`n/a`"


def _f(value: Optional[float], fmt: str) -> str:
    if value is None:
        return ""
    try:
        return format(float(value), fmt)
    except (TypeError, ValueError):
        return ""


def _latency(rec: Record, pct: str) -> str:
    lat = rec.data.get("latency_us") or {}
    return _f(lat.get(pct), ".1f")


def _cores(rec: Record) -> str:
    cpu = rec.data.get("cpu_seconds")
    dur = rec.data.get("duration_secs")
    if cpu is None or not dur:
        return ""
    return format(cpu / dur, ".2f")


def _rss_mb(rec: Record) -> str:
    kb = rec.data.get("peak_rss_kb")
    return "" if kb is None else format(kb / 1024.0, ".0f")


def _gbps(rec: Record) -> str:
    rps = rec.data.get("throughput_rps")
    if rps is None or not rec.payload:
        return ""
    return format(rps * rec.payload * 8 / 1e9, ".2f")


def _metric_cells(rec: Optional[Record], with_gbps: bool) -> List[str]:
    """The per-cell metric values (throughput..errors). ``n/a`` row if absent."""
    if rec is None:
        cells = [NA]  # throughput
        if with_gbps:
            cells.append(NA)
        cells += [NA, NA, NA, NA, NA, NA, NA]  # p50 p95 p99 cpu cores rss errors
        return cells
    cells = [_f(rec.data.get("throughput_rps"), ".0f")]
    if with_gbps:
        cells.append(_gbps(rec))
    cells += [
        _latency(rec, "p50"),
        _latency(rec, "p95"),
        _latency(rec, "p99"),
        _f(rec.data.get("cpu_us_per_op"), ".2f"),
        _cores(rec),
        _rss_mb(rec),
        str(int(rec.data.get("errors", 0))),
    ]
    return cells


def _display_label(label: str) -> str:
    """Row label matching the template's backtick style for the transport token."""
    if label == "kernel baseline":
        return "kernel baseline"
    if " (" in label:
        token, rest = label.split(" (", 1)
        return f"`{token}` ({rest}"
    return f"`{label}`"


# --- Caption ----------------------------------------------------------------


def _payload_str(payload: int) -> str:
    return "8 KiB" if payload == grid.LARGE_PAYLOAD else f"{payload} B"


def _caption_provenance(recs: Sequence[Record]) -> Dict[str, str]:
    """Pull vCPU / duration / warmup / commit / date from any record's meta."""
    vcpu = duration = warmup = commit = date = None
    for r in recs:
        m = r.meta
        vcpu = vcpu or m.get("vcpu")
        duration = duration or m.get("duration") or r.data.get("duration_secs")
        warmup = warmup if warmup is not None else m.get("warmup")
        commit = commit or m.get("commit")
        date = date or (m.get("utc") or "")
    return {
        "vcpu": str(vcpu) if vcpu else "`__`",
        "duration": str(duration) if duration is not None else "`__`",
        "warmup": str(warmup) if warmup is not None else "`__`",
        "commit": f"`{commit}`" if commit else "`________`",
        "date": f"`{date}`" if date else "`________`",
    }


# --- Table A ----------------------------------------------------------------


def render_table_a(
    records: Sequence[Record],
    scenario: str,
    payload: int,
    connection_mult: int,
    in_flight: int,
) -> str:
    with_gbps = payload == grid.LARGE_PAYLOAD
    idx = index(records)
    matching = [
        r for r in records
        if r.scenario == scenario and r.payload == payload
        and r.connection_mult == connection_mult and r.in_flight == in_flight
    ]
    prov = _caption_provenance(matching)
    caption = (
        f"> **SKU:** `________` · **vCPU:** {prov['vcpu']} · "
        f"**scenario:** {_SCENARIO_TITLE.get(scenario, scenario)} · "
        f"**payload:** {_payload_str(payload)} · "
        f"**connections:** {connection_mult}× vCPU · **in-flight:** {in_flight} · "
        f"**duration/warmup:** {prov['duration']} s / {prov['warmup']} s · "
        f"**git commit:** {prov['commit']} · **date:** {prov['date']}"
    )
    header = TABLE_A_HEADER_8K if with_gbps else TABLE_A_HEADER_64
    sep = TABLE_A_SEP_8K if with_gbps else TABLE_A_SEP_64
    lines = [caption, "", header, sep]
    for path in grid.paths_for(scenario):
        rec = idx.get((scenario, path.label, connection_mult, in_flight, payload))
        cells = _metric_cells(rec, with_gbps)
        lines.append("| " + " | ".join([_display_label(path.label)] + cells) + " |")
    return "\n".join(lines)


# --- Table B ----------------------------------------------------------------


def render_table_b(
    records: Sequence[Record],
    scenario: str,
    payload: int,
    path_label: str,
) -> str:
    with_gbps = payload == grid.LARGE_PAYLOAD
    idx = index(records)
    matching = [
        r for r in records
        if r.scenario == scenario and r.payload == payload and r.path_label == path_label
    ]
    prov = _caption_provenance(matching)
    transport_token = path_label.split(" (")[0]
    caption = (
        f"> **SKU:** `________` · **vCPU:** {prov['vcpu']} · "
        f"**scenario:** {_SCENARIO_TITLE.get(scenario, scenario)} · "
        f"**payload:** {_payload_str(payload)} · "
        f"**transport:** `{transport_token}` · "
        f"**duration/warmup:** {prov['duration']} s / {prov['warmup']} s · "
        f"**git commit:** {prov['commit']} · **date:** {prov['date']}"
    )
    header = TABLE_B_HEADER_8K if with_gbps else TABLE_B_HEADER_64
    sep = TABLE_B_SEP_8K if with_gbps else TABLE_B_SEP_64
    lines = [caption, "", header, sep]
    for mult in grid.CONNECTION_MULTIPLES:
        for in_flight in grid.in_flights_for(scenario):
            rec = idx.get((scenario, path_label, mult, in_flight, payload))
            cells = _metric_cells(rec, with_gbps)
            row = [f"{mult}× vCPU", str(in_flight)] + cells
            lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


# --- Batch ------------------------------------------------------------------


def render_all(records: Sequence[Record], scenario: str, payload: int) -> str:
    """Every Table A (per coordinate) + Table B (per path) for scenario+payload."""
    blocks: List[str] = [f"## {_SCENARIO_TITLE.get(scenario, scenario)} — {_payload_str(payload)}"]
    blocks.append("\n### Table A — per-coordinate comparison\n")
    for mult in grid.CONNECTION_MULTIPLES:
        for in_flight in grid.in_flights_for(scenario):
            blocks.append(f"#### {mult}× vCPU · in-flight {in_flight}\n")
            blocks.append(render_table_a(records, scenario, payload, mult, in_flight))
            blocks.append("")
    blocks.append("\n### Table B — concurrency grid per transport\n")
    for path in grid.paths_for(scenario):
        blocks.append(f"#### {path.label}\n")
        blocks.append(render_table_b(records, scenario, payload, path.label))
        blocks.append("")
    return "\n".join(blocks)


# --- CLI --------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="report.py",
        description="Render bench v3 Table A/B Markdown from collected results.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    p.add_argument("--results-dir", default="tests/benchv3/results")
    p.add_argument("--table", choices=["a", "b"], help="Emit a single Table A or B.")
    p.add_argument("--all", action="store_true", help="Emit all Table A + B for a scenario/payload.")
    p.add_argument("--scenario", choices=grid.SCENARIOS)
    p.add_argument("--payload", type=int, choices=grid.PAYLOADS)
    p.add_argument("--connections-mult", type=int, choices=grid.CONNECTION_MULTIPLES)
    p.add_argument("--in-flight", type=int)
    p.add_argument("--transport", dest="path_label", help="Transport-path label (Table B).")
    return p


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    records = load_records(args.results_dir)

    if args.all:
        if args.payload is None:
            print("--all requires --payload", file=sys.stderr)
            return 2
        if args.scenario:
            scenarios = [args.scenario]
        else:
            # No scenario given: emit for every scenario present in the results.
            present = [s for s in grid.SCENARIOS if any(r.scenario == s for r in records)]
            if not present:
                print("no results found in results dir", file=sys.stderr)
                return 2
            scenarios = present
        print("\n\n".join(render_all(records, s, args.payload) for s in scenarios))
        return 0

    if args.table == "a":
        missing = [n for n in ("scenario", "payload", "connections_mult", "in_flight")
                   if getattr(args, n) is None]
        if missing:
            print(f"--table a requires --scenario --payload --connections-mult --in-flight",
                  file=sys.stderr)
            return 2
        print(render_table_a(records, args.scenario, args.payload,
                             args.connections_mult, args.in_flight))
        return 0

    if args.table == "b":
        if not args.scenario or args.payload is None or not args.path_label:
            print("--table b requires --scenario --payload --transport", file=sys.stderr)
            return 2
        print(render_table_b(records, args.scenario, args.payload, args.path_label))
        return 0

    print("nothing to do: pass --table a|b or --all", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
