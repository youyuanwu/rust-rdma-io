# Benchmarks

Measured benchmark write-ups for the `rdma-io-bench` tool, comparing the three
RDMA transports (`send-recv` / `read-ring` / `credit-ring`) against a kernel
baseline. This directory is organized so results scale across **hardware
environments** and **repeated runs over time**:

- **Shared docs** define what/how/why once, independent of any environment.
- **Environment trees** (`<env>/`) hold the measured results for one NIC/driver
  era; each environment's `README.md` is the authoritative index of its results.

## Shared documentation

| Doc | Purpose |
|---|---|
| [strategy.md](strategy.md) | What we measure and why — transports, scenarios, the regime matrix, non-goals |
| [methodology.md](methodology.md) | How we run — durations, reboot cadence, fd-limit fix, `TCP_NODELAY`, NIC caveats |
| [metrics.md](metrics.md) | Metric definitions — req/s, p50/p99, `cores busy`, `cpu_us_per_op`, peak RSS, Gbps |
| [recording.md](recording.md) | How results are recorded — dated provenance blocks, the "adding data / new environment" workflow |
| [scenarios/echo.md](scenarios/echo.md) | Echo scenario — raw `Transport` echo, completion topologies |
| [scenarios/grpc.md](scenarios/grpc.md) | gRPC scenario — tonic `rh2` over the full TLS + HTTP/2 stack |
| [scenarios/h1.md](scenarios/h1.md) | HTTP/1.1 scenario — hyper `http1`, one request in flight per connection |

## Environments

| Environment | Hardware / NIC era | Results index |
|---|---|---|
| **azure-mana-rocev2** | Azure MANA RoCEv2, 64-vCPU E-series VMs | [azure-mana-rocev2/README.md](azure-mana-rocev2/README.md) |

## Adding results / a new environment

See the [recording convention](recording.md) for the full workflow. In brief:

- **New run of an existing regime** — **prepend** a dated result block (newest on
  top) to the matching `<env>/<scenario>/<regime>.md` file, with its Date /
  Environment / Commit / Command provenance header.
- **A new NIC/driver era** — create a new `<env>/` tree (durable slug
  `<cloud>-<nic>[-<variant>]`, e.g. `azure-mana-rocev2`) with its own `README.md`
  (environment definition + results index) and add a row to the Environments table
  above. Shared docs are reused unchanged.

Raw result JSON is archived by the bench tool under the gitignored
`build/…/archive/`; these docs are the curated write-ups, not the raw log.
