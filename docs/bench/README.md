# Benchmarks

Measured benchmark write-ups for the `rdma-io-bench` tool, comparing the three
RDMA transports (`send-recv` / `read-ring` / `credit-ring`) against a kernel
baseline. Each environment's **scoreboard** is the comparison-first entry point;
the directory is organized so results scale across **hardware environments** and
**repeated runs over time**:

- **Per-environment scoreboards** present the RDMA-vs-TCP comparison (all three
  transports vs the baseline) in one canonical view, backed by a coverage matrix.
- **Shared docs** define what/how/why once, independent of any environment.
- **Environment trees** (`<env>/`) hold the measured results for one NIC/driver
  era; each has a `scoreboard.md` (start here) and regime detail pages.

## Shared documentation

| Doc | Purpose |
|---|---|
| [strategy.md](strategy.md) | What we measure and why — transports, scenarios, the regime matrix, non-goals |
| [methodology.md](methodology.md) | How we run — durations, reboot cadence, fd-limit fix, `TCP_NODELAY`, NIC caveats |
| [metrics.md](metrics.md) | Metric definitions — req/s, p50/p99, `cores busy`, `cpu_us_per_op`, peak RSS, Gbps |
| [collection.md](collection.md) | How results are recorded & compared — canonical schema, regime kinds, provenance blocks, coverage matrix, the "adding data / new environment" workflow |
| [scenarios/echo.md](scenarios/echo.md) | Echo scenario — raw `Transport` echo, completion topologies |
| [scenarios/grpc.md](scenarios/grpc.md) | gRPC scenario — tonic `rh2` over the full TLS + HTTP/2 stack |
| [scenarios/h1.md](scenarios/h1.md) | HTTP/1.1 scenario — hyper `http1`, one request in flight per connection |

## Environments

| Environment | Hardware / NIC era | Scoreboard | Environment README |
|---|---|---|---|
| **azure-mana-rocev2** | Azure MANA RoCEv2, 64-vCPU E-series VMs | [▶ scoreboard](azure-mana-rocev2/scoreboard.md) | [README](azure-mana-rocev2/README.md) |

## Adding results / a new environment

See the [collection protocol](collection.md) for the full workflow. In brief:

- **New run of an existing regime** — **prepend** a dated result block (newest on
  top) to the matching `<env>/<scenario>/<regime>.md` file, with its Date /
  Environment / Commit / Command provenance header and canonical comparison table,
  then flip the affected cell in that environment's coverage matrix.
- **A new NIC/driver era** — create a new `<env>/` tree (durable slug
  `<cloud>-<nic>[-<variant>]`, e.g. `azure-mana-rocev2`) with its own `scoreboard.md`
  (comparison-first view + coverage matrix) and `README.md` (environment definition +
  results index), and add a row to the Environments table above. Shared docs are
  reused unchanged.

Raw result JSON is archived by the bench tool under the gitignored
`build/…/archive/`; these docs are the curated write-ups, not the raw log.
