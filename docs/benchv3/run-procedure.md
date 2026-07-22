# End-to-end run procedure (bench v3)

How to execute the [fixed-workload grid](scenario-matrix.md) and record the numbers into the
[results-template](results-template.md). This is the "how to run" companion to the scenario
matrix (what/why) — see the [bench v3 README](README.md) for the framework overview.

The benchmark runs across **two RDMA-capable VMs** (a server on `vm1`, a client on `vm2`)
orchestrated from a control node with Ansible.

> **Orchestration is pluggable — bench v3 is launcher-agnostic.** What v3 pins is the **grid**
> and the **coordinate → flags/vars contract** below; *how* those flags reach the two VMs is an
> orchestration detail that can evolve independently. The same Ansible playbooks
> (`tests/e2e/playbooks/*.yml`) can be driven several ways — the in-repo `tests/e2e/run_bench.sh`
> convenience script, a raw `ansible-playbook` invocation, or a higher-level matrix/reporting
> wrapper layered on top. This document shows the in-repo launchers because they are
> self-contained here; any matrix + reporting wiring is maintained separately (see
> [Launchers](#launchers-pluggable)). Pick whichever launcher you have — the coordinate contract
> and caveats are identical.

## The coordinate → flags/vars contract

Every grid coordinate maps to this fixed set of tool inputs, whatever launcher applies them
(client/server flags, or the `-e bench_*` playbook vars of the same name):

| Grid axis | Client/server flag | Playbook var |
|---|---|---|
| scenario/path | `--mode` (`echo`/`rh2`/`rh1`/`tcp`/`tcp1`, or `echo-busy`/`echo-park`/`rh1-busy`/`rh1-park`) | `bench_mode` |
| transport | `--transport` (`send-recv`/`read-ring`/`credit-ring`; `tcp` for echo baseline) | `bench_transport` |
| connections = mult × vCPU | `--connections` | `bench_connections` |
| threads = vCPU | `--threads` | `bench_threads` |
| in-flight (echo/gRPC) | `--in-flight` | `bench_in_flight` |
| payload | `--payload` | `bench_payload` |
| 8 KiB ring sizing | `--ring-max-msg 8192` | `bench_ring_max_msg` |
| duration / warmup | `--duration` / `--warmup` | `bench_duration` / `bench_warmup` |

The rest of this doc shows one launcher applying that contract, then the caveats (which are
launcher-independent facts about the tool/NIC).

## Prerequisites

- A control node with `cargo`, `just`, and `ansible` installed.
- Two RDMA-capable VMs reachable over a private link, described by the Ansible inventory
  `tests/e2e/inventory_local.py` (server = `vm1`, client = `vm2`).
- RDMA hardware set up on both VMs via `tests/e2e/playbooks/setup_rdma_hw.yml`, which also
  installs the **fd-limit fix** (`/etc/security/limits.d/99-rdma-bench.conf`, `nofile` raised to
  the hard cap). See [`methodology.md`](../bench/methodology.md#the-fd-wall-fixed).

## Step 0 — build the release binaries

```
cargo build -p rdma-io-bench --release
```

Produces `target/release/rdma-bench-server` and `target/release/rdma-bench-client`.

## Step 1 — deploy binaries + certs

```
ansible-playbook -i tests/e2e/inventory_local.py tests/e2e/playbooks/deploy_bench.yml
```

`deploy_bench.yml` auto-generates a self-signed TLS cert/key under `build/certs/` (if missing),
copies the release binaries to `~/bin/` and the certs to `~/certs/` on both VMs.

## Step 2 — set `--threads = vCPU count`

Every coordinate is run with **`--threads` equal to the target VM's vCPU count** (the executor
budget, not a load axis — see the [scenario matrix](scenario-matrix.md#threads-fixed)). Determine
it once with `nproc` on the VM and use it for `bench_threads` below. The **connection** count is
the chosen `{1×, 4×, 16×}` multiple of that vCPU count.

## Step 3 — run a coordinate

`run_bench.sh` exposes `--mode/--transport/--connections/--threads/--duration/--payload` but
**not** `--in-flight`, `--warmup`, or `--ring-max-msg`. So:

**in-flight = 1 rows** (all HTTP/1.1 rows; the round-trip rows of echo/gRPC) — use the
convenience script:

```
tests/e2e/run_bench.sh \
  --mode echo --transport read-ring \
  --connections 64 --threads 64 \
  --duration 10 --payload 64
```

The built-in matrix form iterates its own lists (defaults run at in-flight 1); override with the
`--matrix-*` flags — note single-run `--mode/--transport/...` flags are ignored in matrix mode.
**`--matrix` cross-products modes × transports**, so keep each mode with only transports it
accepts: `echo` (which alone accepts `--transport tcp` for its baseline) is safe to sweep across
all four paths, but `rh2`/`rh1` accept only the RDMA transports — their kernel baselines are
**separate modes** (`tcp` / `tcp1`), not `--transport tcp`. So sweep echo across all paths:

```
tests/e2e/run_bench.sh --matrix \
  --matrix-modes 'echo' \
  --matrix-transports 'send-recv read-ring credit-ring tcp' \
  --matrix-connections '64 256 1024' \
  --matrix-threads '64'
```

…and run the gRPC / HTTP-1.1 rows as their own single-run invocations — the RDMA paths with
`--mode rh2|rh1 --transport <rdma>`, and the baselines as `--mode tcp` (gRPC) / `--mode tcp1`
(HTTP/1.1). (Do **not** put `rh2`/`rh1` and `tcp` in one matrix — the cross-product would emit the
invalid `--mode rh2 --transport tcp`, which the client rejects.)

**in-flight ∈ {64, 512} rows** (echo/gRPC) — invoke the orchestration playbook directly, which
does accept the in-flight / warmup / ring-message variables:

```
ansible-playbook -i tests/e2e/inventory_local.py tests/e2e/playbooks/bench_run.yml \
  -e bench_mode=echo -e bench_transport=read-ring \
  -e bench_connections=64 -e bench_threads=64 \
  -e bench_in_flight=512 \
  -e bench_duration=10 -e bench_warmup=3 \
  -e bench_payload=64
```

The client is launched internally with `--report json`; its stdout is captured to a result file
(see [Where results land](#where-results-land)).

### Warmup

`run_bench.sh` has no warmup flag and uses the playbook default (`bench_warmup=5`). To control it,
use the direct playbook form with `-e bench_warmup=<seconds>`. Record the actual duration/warmup
in each results-table caption.

> **Hold duration and warmup constant across a grid.** Because bench v3 compares cells at
> identical coordinates, every cell of a given grid must use the **same** `--duration` and
> `--warmup`. The convenience-script path defaults to `warmup=5`, so if you mix it with
> direct-playbook runs, pass the same value there too (`-e bench_warmup=5`) — or run every row
> via the playbook — rather than leaving the two paths on different warmups.

### 8 KiB payload → ring message sizing (required)

The `echo` path truncates ring-transport payloads larger than `--ring-max-msg` (default
**1500 B**). For the **8 KiB** rows on the ring transports (`read-ring` / `credit-ring`), set the
ring message size to 8192 on **both** peers via the direct playbook (`run_bench.sh` cannot supply
it). `send-recv` sizes its echo buffers from `--payload`; the TCP baseline is not affected by this
ring knob.

```
ansible-playbook -i tests/e2e/inventory_local.py tests/e2e/playbooks/bench_run.yml \
  -e bench_mode=echo -e bench_transport=read-ring \
  -e bench_connections=64 -e bench_threads=64 \
  -e bench_in_flight=64 -e bench_ring_max_msg=8192 \
  -e bench_duration=10 -e bench_warmup=3 \
  -e bench_payload=8192
```

### Read-ring completion topologies (busy-poll / park)

`read-ring` has two pinned-core completion topologies that bench v3 records as **separate
transport paths** (echo & HTTP/1.1 only — there is no gRPC busy/park variant). They are selected
by **mode**, always with `--transport read-ring`, and `--threads` is the number of **pinned
cores** (set to the vCPU count, same on both peers):

| Path | echo mode | HTTP/1.1 mode |
|---|---|---|
| `read-ring` (arm-park, default) | `echo` | `rh1` |
| `read-ring` (busy-poll) | `echo-busy` | `rh1-busy` |
| `read-ring` (thread-per-core park) | `echo-park` | `rh1-park` |

Example — echo busy-poll at 1× connections, in-flight 64 (via the direct playbook so warmup /
in-flight are explicit):

```
ansible-playbook -i tests/e2e/inventory_local.py tests/e2e/playbooks/bench_run.yml \
  -e bench_mode=echo-busy -e bench_transport=read-ring \
  -e bench_connections=64 -e bench_threads=64 \
  -e bench_in_flight=64 \
  -e bench_duration=10 -e bench_warmup=3 \
  -e bench_payload=64
```

Swap `bench_mode` to `echo-park` (echo) or `rh1-busy` / `rh1-park` (HTTP/1.1) for the other
topology/scenario. gRPC (`rh2`) has no busy/park mode, so a gRPC board carries only the single
arm-park `read-ring` path.

### Deep in-flight (512) — ring queue sizing

The ring transports auto-size their in-flight budget from `--in-flight` when
`--ring-queue-depth` is `0` (the default), so a 512-deep run is sized automatically — no manual
tuning. Override `--ring-queue-depth` only to deliberately reproduce an over-subscription case.

### High connections (16× vCPU)

The fd wall is already fixed (`setup_rdma_hw.yml` + launch-time `ulimit -n` in `bench_run.yml`),
so high connection counts are **not** fd-bound. Beyond a few hundred connections MANA RoCEv2 can
show RDMA-CM **setup** flakiness — that is a setup property, not a data-path wall; record the cell
as `fail (CM setup)` and re-run isolated on a fresh NIC before concluding. See
[`methodology.md`](../bench/methodology.md#the-fd-wall-fixed).

### Reboot cadence

Back-to-back RDMA ring runs progressively wedge the MANA NIC's RDMA-CM. Reboot between sweeps:

```
ansible-playbook -i tests/e2e/inventory_local.py tests/e2e/playbooks/reboot_vms.yml
```

A `fail`/timeout on a churned NIC is a setup wedge, not a code result — re-run isolated on a fresh
NIC. See [`methodology.md`](../bench/methodology.md#reboot-cadence-and-nic-wedges).

### Where results land

`bench_run.yml` writes the client JSON to `<bench_out_dir>/bench-<mode>-<transport>-
<connections>conn-<threads>thr-<in_flight>if.json`. The directory depends on the launcher's
`bench_out_dir` (the in-repo `run_bench.sh` / raw playbook default is `/tmp`; a matrix wrapper may
point it elsewhere):

```
<bench_out_dir>/bench-<mode>-<transport>-<connections>conn-<threads>thr-<in_flight>if.json
```

> **The filename encodes no payload and no date**, so a 64 B then 8 KiB run (or a repeat) at the
> same coordinate **overwrites** the previous file. Curate each cell into the
> [results-template](results-template.md) immediately, or set a per-payload/per-run `bench_out_dir`
> (or rename to include payload + date + commit) before the next run.

## Launchers (pluggable)

bench v3 pins the grid and the [coordinate contract](#the-coordinate--flagsvars-contract), not a
specific launcher. Each launcher below applies the same contract to the same playbooks — use
whichever your environment provides. A higher-level **matrix** driver (looping the grid across
paths/coordinates) and a **report** step (JSON → Markdown/charts) can be layered on top; that
wiring is maintained **separately** from these docs and may evolve, so v3 stays agnostic to it.

| Launcher | Where | Notes |
|---|---|---|
| `tests/e2e/run_bench.sh` | in this repo | Single run (mode/transport/connections/threads/duration/payload) or `--matrix` (in-flight 1); no `--in-flight`/`--warmup`/`--ring-max-msg`. |
| raw `ansible-playbook … bench_run.yml -e bench_*` | in this repo | Full control of every `bench_*` var (the examples above). |
| higher-level matrix/report wrapper | external tooling | Loops the grid and aggregates results; drives the same `bench_*` vars. Whatever wrapper you use, keep every cell on the fixed coordinates and hold duration/warmup constant. |

Note that a "sweep transports × in-flights at fixed cpus" matrix maps cleanly onto the v3 grid
(run it per connection multiple and per payload), but such wrappers typically cover only the
default arm-park echo/gRPC paths — the HTTP/1.1 (`rh1`) and read-ring busy-poll/park paths are
driven with single `bench_mode=…` runs (the raw-playbook examples above).

## Teardown

The VMs are yours to manage — stop or deallocate them when a sweep is done to avoid cost.

## Validating without a cloud

You can smoke-test the tooling locally without two cloud VMs by bringing up a software RDMA
device (soft-RoCE / soft-iWARP):

```
just setup-siw    # or: just setup-rxe
```

then run the client/server against loopback. This validates the binaries and flags; the published
grid numbers still come from the two-VM RDMA setup above.
