# Bench v3 data-collection tooling

Self-contained, in-repo tooling to **run the [bench v3 fixed-workload grid](../../docs/benchv3/scenario-matrix.md),
collect the results without collision, and turn them into the v3 result tables**. It is the concrete
in-repo launcher referenced by the [run procedure](../../docs/benchv3/run-procedure.md); the grid
itself (what/why) is pinned by the [scenario matrix](../../docs/benchv3/scenario-matrix.md) and the
table shapes by the [results template](../../docs/benchv3/results-template.md).

Two pure-Python (stdlib-only) tools:

| Tool | Role |
|---|---|
| `run_matrix.py` | Expand the grid → drive each coordinate through the existing e2e orchestration → save each result under a collision-proof identity name. |
| `report.py` | Aggregate collected result JSON → emit paste-ready Table A / Table B Markdown. |

Both import the shared grid model in `grid.py` (the single in-code source of truth for the grid
contract), so the runner and the report agree on the path↔`(mode,transport)` mapping.

## Prerequisites

- Two RDMA-capable VMs already provisioned and described by the e2e inventory
  (`../e2e/inventory_local.py`; server `vm1`, client `vm2`). Provisioning the VMs is out of scope
  for this tooling.
- The release binaries built and deployed to the VMs — see
  [run-procedure Steps 0–2](../../docs/benchv3/run-procedure.md).
- `python3`, `ansible`, and `just` on the control node.

## Quickstart

**1. Preview the plan (no VMs touched).** Always dry-run first to see the exact coordinates:

```
python3 tests/benchv3/run_matrix.py --vcpu 64 --scenario echo --dry-run
```

`--vcpu` is the target VM's vCPU count — it sets `threads = vcpu` and `connections = mult × vcpu`.
Determine it once with `nproc` on the VM.

**2. Run a sweep.** Drop `--dry-run` to execute. Each coordinate is driven through
`../e2e/playbooks/bench_run.yml`; results land under `results/` (git-ignored):

```
python3 tests/benchv3/run_matrix.py --vcpu 64 --scenario echo
```

**3. Generate tables.** Emit a full board (every Table A + Table B for a scenario/payload) in one
call, or a single table:

```
# whole echo / 64 B board:
python3 tests/benchv3/report.py --results-dir tests/benchv3/results --all --scenario echo --payload 64

# one Table A coordinate:
python3 tests/benchv3/report.py --results-dir tests/benchv3/results \
  --table a --scenario echo --payload 64 --connections-mult 1 --in-flight 64

# one Table B (concurrency grid for a transport path):
python3 tests/benchv3/report.py --results-dir tests/benchv3/results \
  --table b --scenario echo --payload 64 --transport "read-ring (arm-park)"
```

Paste the emitted Markdown into a curated copy under
[`docs/benchv3/results-template.md`](../../docs/benchv3/results-template.md) (the `SKU:` field is
left blank for you to fill). **Only the curated tables are committed — never the raw `results/`
artifacts.**

## `run_matrix.py` reference

| Flag | Meaning |
|---|---|
| `--vcpu N` (required) | Target VM vCPU count → `threads=N`, `connections=mult×N`. |
| `--scenario {echo,grpc,http1}` | Limit to scenario(s) (repeatable). Default: all. |
| `--path LABEL` | Limit to transport-path label(s) (repeatable). Default: all. |
| `--connections-mult {1,4,16}` | Limit to connection multiple(s) (repeatable). |
| `--in-flight N` | Limit to in-flight depth(s) (repeatable). HTTP/1.1 stays at 1 regardless. |
| `--payload {64,8192}` | Limit to payload size(s) (repeatable). |
| `--duration S` / `--warmup S` | Fixed run controls (default 10 s / 3 s). |
| `--results-dir DIR` | Where identity-named results land (default `tests/benchv3/results`). |
| `--inventory PATH` | Ansible inventory (default the in-repo `tests/e2e/inventory_local.py`). |
| `--playbook PATH` | Orchestration playbook driven per coordinate (default `tests/e2e/playbooks/bench_run.yml`). |
| `--reboot-playbook PATH` | Playbook used by `--reboot-between` (default `tests/e2e/playbooks/reboot_vms.yml`). |
| `--reboot-between` | Reboot the VMs at each sweep boundary (a change of `(scenario, transport-path)` group). |
| `--dry-run` | List the planned coordinates and exit — run nothing. |

Defaults for the inventory/playbook/results paths are resolved relative to the repo root, so the
tool works regardless of the caller's working directory.

The grid axes, path→mode/transport mapping, and the invalid-coordinate rules (no gRPC busy/park;
HTTP/1.1 in-flight=1; 8 KiB → ring message size 8192 on ring transports only) are encoded once in
`grid.py` from the [scenario matrix](../../docs/benchv3/scenario-matrix.md) — this tool does not
re-document them.

### Result naming (collision-proof)

Each result is saved as:

```
results/bench-<scenario>-<path>-<mode>-<transport>-<conns>conn-<threads>thr-<inflight>if-<payload>B-<utc>-<commit>-<runid>.json
```

with a `.meta.json` provenance sidecar carrying what the result JSON does not record (duration,
warmup, vCPU, connection multiple, commit, run id). Because the name embeds the **mode/transport**,
the **payload**, a UTC timestamp, the git commit, and a per-invocation run id, runs across
coordinates, payloads, repeats, days, and machines never overwrite each other. A
`run-summary-<runid>.json` records which coordinates succeeded or failed (a single coordinate
failure — including a launcher error such as a missing `ansible-playbook` — never aborts the sweep).
When a `results/` directory accumulates multiple sweeps or SKUs, pass `report.py --run-id <id>` to
scope a report to one sweep. `report.py` ignores the `.meta.json` sidecars and the run summary
when parsing results (it reads the sidecars only for caption provenance).

A single coordinate failure never aborts the sweep — it is recorded and the sweep continues.

## Validating without a cloud

You can smoke-test the **Python tooling and `--dry-run` planning** locally without two cloud VMs by
bringing up a software-RDMA device:

```
just setup-siw    # or: just setup-rxe
```

A full two-VM (`vm1`/`vm2`) sweep still needs the two-host inventory above; the loopback path only
validates the tool wiring and the planned coordinates. The unit tests
(`python3 -m unittest discover -s tests/benchv3/tests`) exercise grid expansion, result naming, and
table rendering with no VMs at all.
