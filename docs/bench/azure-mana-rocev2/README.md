# Environment: azure-mana-rocev2

Results measured on Azure VMs with the **MANA** NIC in **RoCEv2** mode. This file
is the environment definition **and** the authoritative index of results measured
here. Shared strategy / methodology / metrics live one level up
([strategy](../strategy.md) · [methodology](../methodology.md) ·
[metrics](../metrics.md)).

## Environment definition

| Field | Value |
|---|---|
| Cloud / NIC | Azure MANA, RoCEv2 |
| VMs | Two E-series VMs (client + server) |
| CPU topology | **64 logical CPUs = 2 sockets × 16 physical × 2 SMT** |
| VM SKU | `TODO` — record exact `Standard_E…` size |
| Kernel | `TODO` — record `uname -r` |
| MANA driver | `TODO` — record driver/firmware version |
| Distro | `TODO` — record distro + release |
| NIC handling | reboot between RDMA sweeps (`just reboot && just prepare-rdma`); fd soft-limit raised (see [methodology](../methodology.md#the-fd-wall-fixed)) |

> The `TODO` fields are not yet recorded; fill them in from the running VMs. The
> CPU topology is derived from documented measurements
> (`2 sockets × 16 physical × 2 SMT`, the NUMA/SMT knee that shapes the
> thread-per-core results).

## Results index

Results for each scenario (regime files grouped by scenario). The regime files are
added as each scenario is migrated.

- **[Echo](../scenarios/echo.md)** —
  [message-rate (64 B)](echo/message-rate-64b.md) ·
  [large-payload (8 KiB)](echo/large-payload-8kib.md) ·
  [busy-poll](echo/busy-poll.md) ·
  [thread-per-core (echo-park)](echo/thread-per-core-park.md)
- **[gRPC](../scenarios/grpc.md)** —
  [throughput & pipelining (64 B)](grpc/throughput-64b.md) ·
  [payload size](grpc/payload.md) ·
  [client CPU & memory](grpc/cpu-memory.md)
- **[HTTP/1.1](../scenarios/h1.md)** — _populated during migration_

## Recording convention (read before adding results)

Results are **append-only, newest on top**. Each regime file
(`<scenario>/<regime>.md`) is:

```
# <Regime title>
<one-line intro linking the shared scenario doc>

## Results
### <YYYY-MM-DD> — <label>          (newest first)
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `192daa7`             (or `unknown` — never guessed)
- **Command:** `just bench-echo --transport read-ring …`   (or `TODO — not recorded`)
- *(optional)* duration/warmup/threads · reboot-clean NIC · raw JSON `build/…/archive/`

<this run's tables AND its analysis stay together, in this block>

### Undated — historical baseline    (last block, only if the baseline date is unrecoverable)
- **Date:** unknown
- **Environment:** … **Commit:** … **Command:** …
```

Rules:

- **Mandatory provenance (per block):** `**Date:**`, `**Environment:**`,
  `**Commit:**`, `**Command:**`. Unrecoverable values are marked `unknown` /
  `TODO — not recorded`, **never guessed**.
- **Exact command:** the real recorded `**Command:**` carries the full command line
  (no illustrative `…`).
- **Newest first;** an `### Undated — historical baseline` block sorts **last**.
- **Per-block attribution:** each run's tables and the prose analysing them stay
  **together** in that run's block. Only environment-invariant explanation belongs
  in the shared docs.
- **Adding a new environment:** create `docs/bench/<env>/README.md` (this template)
  and add a row to the [bench Environments table](../README.md#environments). Shared
  docs are reused unchanged.
