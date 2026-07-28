"""Bench v3 fixed-workload grid model and coordinate expansion.

This module is the single in-code source of truth for the bench v3 grid contract
documented in ``docs/benchv3/scenario-matrix.md``. It is pure, dependency-free
Python (stdlib only) so it can be imported by both the runner (``run_matrix.py``)
and the report generator (``report.py``) and unit-tested without any VMs.

The grid is a *fixed* workload: threads always equal the target vCPU count,
connections are fixed multiples of vCPU, in-flight is a fixed set, and payloads
are a fixed pair. Nothing is tuned per run — see ``docs/benchv3/scenario-matrix.md``.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

# --- Fixed grid axes (docs/benchv3/scenario-matrix.md) ----------------------

#: Connection counts are these multiples of the target VM vCPU count.
CONNECTION_MULTIPLES: Tuple[int, ...] = (1, 2, 4)

#: In-flight depths for echo and gRPC. HTTP/1.1 is pinned to 1 (see PATHS).
IN_FLIGHTS_ECHO_GRPC: Tuple[int, ...] = (1, 64, 512)
IN_FLIGHTS_HTTP1: Tuple[int, ...] = (1,)

#: Payload sizes in bytes.
PAYLOADS: Tuple[int, ...] = (64, 8192)

#: Payload that triggers the ring message-size bump on ring transports.
LARGE_PAYLOAD: int = 8192
#: Ring message size for a large *raw* payload (echo): the ring message is exactly
#: the payload bytes, so the payload itself is the required size.
RING_MAX_MSG_LARGE: int = 8192
#: Ring message size for a large *framed* payload (gRPC / HTTP-1.1): the on-wire
#: message is ``payload + framing`` — gRPC adds protobuf + gRPC-prefix + HTTP/2
#: DATA header (~payload+23), and HTTP/1.1 adds request/status line + headers +
#: TLS record — so the ring message must hold more than the payload. 9216 clears
#: the ~8215 B framed 8 KiB message while still leaving 7 ring slots
#: (65536 / 9216). Matches the benchv1 sizing.
RING_MAX_MSG_LARGE_FRAMED: int = 9216
#: Scenarios whose on-wire ring message carries protocol framing on top of the
#: payload (so they need the framed ring size); ``echo`` is raw and does not.
FRAMED_SCENARIOS: Tuple[str, ...] = ("grpc", "http1")
#: Transports whose ring buffers must be sized up for the large payload.
RING_TRANSPORTS: Tuple[str, ...] = ("read-ring", "credit-ring")

SCENARIOS: Tuple[str, ...] = ("echo", "grpc", "http1")


@dataclass(frozen=True)
class Path:
    """A measured transport path within a scenario.

    ``mode``/``transport`` are the exact ``--mode``/``--transport`` values (and
    the matching ``bench_mode``/``bench_transport`` playbook vars) that select
    this path in the benchmark tool.
    """

    scenario: str
    label: str
    mode: str
    transport: str


# The full path table. Order within a scenario matches the row order of Table A
# in docs/benchv3/results-template.md so the report renders rows in place.
PATHS: Tuple[Path, ...] = (
    # echo — 6 paths
    Path("echo", "send-recv", "echo", "send-recv"),
    Path("echo", "read-ring (arm-park)", "echo", "read-ring"),
    Path("echo", "read-ring (busy-poll)", "echo-busy", "read-ring"),
    Path("echo", "read-ring (thread-per-core park)", "echo-park", "read-ring"),
    Path("echo", "credit-ring", "echo", "credit-ring"),
    Path("echo", "kernel baseline", "echo", "tcp"),
    # gRPC (rh2) — 4 paths, NO busy/park variant
    Path("grpc", "send-recv", "rh2", "send-recv"),
    Path("grpc", "read-ring (arm-park)", "rh2", "read-ring"),
    Path("grpc", "credit-ring", "rh2", "credit-ring"),
    Path("grpc", "kernel baseline", "tcp", "tcp"),
    # HTTP/1.1 (rh1) — 6 paths
    Path("http1", "send-recv", "rh1", "send-recv"),
    Path("http1", "read-ring (arm-park)", "rh1", "read-ring"),
    Path("http1", "read-ring (busy-poll)", "rh1-busy", "read-ring"),
    Path("http1", "read-ring (thread-per-core park)", "rh1-park", "read-ring"),
    Path("http1", "credit-ring", "rh1", "credit-ring"),
    Path("http1", "kernel baseline", "tcp1", "tcp"),
)

# (mode, transport) -> Path, the inverse mapping shared with report.py so a
# result's mode/transport reconstructs exactly the same path label. This must be
# unambiguous: arm/busy/park all report transport=read-ring but differ by mode;
# baselines differ by mode (echo+tcp / tcp / tcp1).
_PATH_BY_MODE_TRANSPORT: Dict[Tuple[str, str], Path] = {
    (p.mode, p.transport): p for p in PATHS
}
assert len(_PATH_BY_MODE_TRANSPORT) == len(PATHS), "ambiguous (mode,transport) mapping"


def paths_for(scenario: str) -> List[Path]:
    """Return the transport paths for a scenario, in template row order."""
    if scenario not in SCENARIOS:
        raise ValueError(f"unknown scenario {scenario!r}; expected one of {SCENARIOS}")
    return [p for p in PATHS if p.scenario == scenario]


def lookup_path(mode: str, transport: str) -> Optional[Path]:
    """Reconstruct the Path from a result's (mode, transport). None if unknown."""
    return _PATH_BY_MODE_TRANSPORT.get((mode, transport))


def path_label(mode: str, transport: str) -> Optional[str]:
    """Reconstruct the transport-path label from a result's (mode, transport)."""
    p = lookup_path(mode, transport)
    return p.label if p else None


def scenario_of(mode: str, transport: str) -> Optional[str]:
    """Reconstruct the scenario from a result's (mode, transport)."""
    p = lookup_path(mode, transport)
    return p.scenario if p else None


def in_flights_for(scenario: str) -> Tuple[int, ...]:
    """In-flight depths a scenario runs (HTTP/1.1 is pinned to 1)."""
    return IN_FLIGHTS_HTTP1 if scenario == "http1" else IN_FLIGHTS_ECHO_GRPC


@dataclass(frozen=True)
class Coordinate:
    """One fully-specified benchmark run in the v3 grid."""

    scenario: str
    path_label: str
    mode: str
    transport: str
    connection_mult: int
    connections: int
    threads: int
    in_flight: int
    payload: int
    ring_max_msg: Optional[int] = None
    #: Open-loop target request rate (req/s) for the loaded-latency and
    #: matched-throughput scenarios; ``None`` = the default closed-loop
    #: saturation run. Split across ``connections`` by the client.
    target_rps: Optional[int] = None

    def bench_vars(self, duration: int, warmup: int) -> Dict[str, object]:
        """The ``bench_*`` playbook variables for this coordinate."""
        v: Dict[str, object] = {
            "bench_mode": self.mode,
            "bench_transport": self.transport,
            "bench_connections": self.connections,
            "bench_threads": self.threads,
            "bench_in_flight": self.in_flight,
            "bench_payload": self.payload,
            "bench_duration": duration,
            "bench_warmup": warmup,
        }
        if self.ring_max_msg is not None:
            v["bench_ring_max_msg"] = self.ring_max_msg
        if self.target_rps is not None:
            v["bench_target_rps"] = self.target_rps
        return v


def _ring_max_msg_for(scenario: str, transport: str, payload: int) -> Optional[int]:
    """8 KiB payloads on ring transports need the ring message size bumped.

    Echo carries the raw payload (message == payload), so it uses the payload
    size directly. gRPC and HTTP/1.1 wrap the payload in protocol framing +
    (for HTTP/1.1) TLS, so the on-wire ring message exceeds the payload and needs
    the larger framed size — otherwise the ring transport truncates (echo) /
    fragments (byte-stream) the message. See the benchv1 sizing rationale.
    """
    if payload == LARGE_PAYLOAD and transport in RING_TRANSPORTS:
        if scenario in FRAMED_SCENARIOS:
            return RING_MAX_MSG_LARGE_FRAMED
        return RING_MAX_MSG_LARGE
    return None


def expand(
    vcpu: int,
    scenarios: Sequence[str] = SCENARIOS,
    conn_mults: Sequence[int] = CONNECTION_MULTIPLES,
    in_flights: Optional[Sequence[int]] = None,
    payloads: Sequence[int] = PAYLOADS,
    path_labels: Optional[Iterable[str]] = None,
) -> List[Coordinate]:
    """Expand the (optionally subset-filtered) grid into individual runs.

    ``vcpu`` sets threads (= vcpu) and connections (= mult * vcpu). ``in_flights``
    overrides the default per-scenario set (still intersected with what the
    scenario supports, so HTTP/1.1 stays at in-flight 1). ``path_labels`` filters
    to specific transport-path labels.
    """
    if vcpu <= 0:
        raise ValueError("vcpu must be a positive integer")

    label_filter = set(path_labels) if path_labels is not None else None
    coords: List[Coordinate] = []
    for scenario in scenarios:
        supported_if = in_flights_for(scenario)
        # If the caller narrowed in-flights, keep only the supported ones.
        chosen_if = (
            supported_if
            if in_flights is None
            else tuple(f for f in in_flights if f in supported_if)
        )
        for path in paths_for(scenario):
            if label_filter is not None and path.label not in label_filter:
                continue
            for mult in conn_mults:
                for in_flight in chosen_if:
                    for payload in payloads:
                        coords.append(
                            Coordinate(
                                scenario=scenario,
                                path_label=path.label,
                                mode=path.mode,
                                transport=path.transport,
                                connection_mult=mult,
                                connections=mult * vcpu,
                                threads=vcpu,
                                in_flight=in_flight,
                                payload=payload,
                                ring_max_msg=_ring_max_msg_for(
                                    scenario, path.transport, payload
                                ),
                            )
                        )
    return coords


def _slug(text: str) -> str:
    """Filesystem-safe slug: lowercase, non-alphanumerics -> single hyphen."""
    s = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return s or "x"


def identity_filename(
    coord: Coordinate,
    utc: str,
    commit: str,
    run_id: str,
    ext: str = "json",
) -> str:
    """Collision-proof result filename embedding the full scenario identity.

    Includes payload, a UTC timestamp, the git commit, and a per-invocation run
    id so runs across coordinates, payloads, repeats, days, and machines never
    collide. ``utc``/``commit``/``run_id`` are treated opaquely (already safe).
    """
    parts = [
        "bench",
        _slug(coord.scenario),
        _slug(coord.path_label),
        _slug(coord.mode),
        _slug(coord.transport),
        f"{coord.connections}conn",
        f"{coord.threads}thr",
        f"{coord.in_flight}if",
        f"{coord.payload}B",
    ]
    # Offered-load runs are distinguished by their target rate so a loaded-latency
    # sweep's rate levels never collide; closed-loop names are unchanged.
    if coord.target_rps is not None:
        parts.append(f"{coord.target_rps}rps")
    parts += [
        utc,
        _slug(commit),
        _slug(run_id),
    ]
    return "-".join(parts) + "." + ext


#: Scenarios that support the open-loop offered-load scenarios (gRPC is excluded
#: because its tonic/HTTP-2 client paces itself internally).
LOADED_LATENCY_SCENARIOS: Tuple[str, ...] = ("echo", "http1")

#: Default per-connection queue capacity (``--in-flight``) to provision for the
#: open-loop echo scenarios, so the transport queue is not the limiter below
#: saturation (HTTP/1.1 is always pinned to 1). Size by Little's law:
#: ``in_flight >= target_rps * expected_latency`` per connection.
DEFAULT_OPEN_LOOP_IN_FLIGHT: int = 64


def _open_loop_coords(
    vcpu: int,
    scenario: str,
    rates: Sequence[int],
    connection_mult: int,
    payload: int,
    in_flight: int,
    transports: Optional[Iterable[str]],
) -> List[Coordinate]:
    """Shared expansion for the two offered-load scenarios: one coordinate per
    (transport path x rate) at a fixed connection tier, carrying ``target_rps``.

    ``transports`` filters by transport-path *label* or *transport* name; ``None``
    keeps every path for the scenario. HTTP/1.1 pins ``in_flight`` to 1.
    """
    if vcpu <= 0:
        raise ValueError("vcpu must be a positive integer")
    if scenario not in LOADED_LATENCY_SCENARIOS:
        raise ValueError(
            f"scenario {scenario!r} does not support offered-load runs; "
            f"expected one of {LOADED_LATENCY_SCENARIOS}"
        )
    if any(r <= 0 for r in rates):
        raise ValueError("target rates must be positive integers")

    cap = 1 if scenario == "http1" else in_flight
    keep = set(transports) if transports is not None else None
    coords: List[Coordinate] = []
    for path in paths_for(scenario):
        if keep is not None and path.label not in keep and path.transport not in keep:
            continue
        for rate in rates:
            coords.append(
                Coordinate(
                    scenario=scenario,
                    path_label=path.label,
                    mode=path.mode,
                    transport=path.transport,
                    connection_mult=connection_mult,
                    connections=connection_mult * vcpu,
                    threads=vcpu,
                    in_flight=cap,
                    payload=payload,
                    ring_max_msg=_ring_max_msg_for(scenario, path.transport, payload),
                    target_rps=rate,
                )
            )
    return coords


def expand_loaded_latency(
    vcpu: int,
    scenario: str,
    rates: Sequence[int],
    connection_mult: int = 1,
    payload: int = 64,
    in_flight: int = DEFAULT_OPEN_LOOP_IN_FLIGHT,
    transports: Optional[Iterable[str]] = None,
) -> List[Coordinate]:
    """Loaded tail-latency sweep: each transport path run across several
    sub-saturation ``rates`` at a fixed connection tier, for ``echo``/``http1``.
    """
    return _open_loop_coords(
        vcpu, scenario, rates, connection_mult, payload, in_flight, transports
    )


def expand_matched_throughput(
    vcpu: int,
    scenario: str,
    rate: int,
    connection_mult: int = 1,
    payload: int = 64,
    in_flight: int = DEFAULT_OPEN_LOOP_IN_FLIGHT,
    transports: Optional[Iterable[str]] = None,
) -> List[Coordinate]:
    """Matched-throughput comparison: every transport path run at one shared
    ``rate`` so CPU cost is compared at identical delivered load.
    """
    return _open_loop_coords(
        vcpu, scenario, [rate], connection_mult, payload, in_flight, transports
    )


def dedupe(coords: Sequence["Coordinate"]) -> List["Coordinate"]:
    """Drop duplicate coordinates (e.g. from repeated CLI filters), keep order."""
    seen = set()
    out: List[Coordinate] = []
    for c in coords:
        if c in seen:
            continue
        seen.add(c)
        out.append(c)
    return out
