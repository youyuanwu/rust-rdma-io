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

## Results

> **▶ [RDMA-vs-TCP scoreboard](scoreboard.md)** — the comparison-first view of every
> regime (all three RDMA transports vs the kernel baseline) plus the coverage matrix.
> Start here.

### Results index

Regime detail pages (full data, sweeps, and per-run analysis), grouped by scenario:

- **[Echo](../scenarios/echo.md)** —
  [message-rate (64 B)](echo/message-rate-64b.md) ·
  [large-payload (8 KiB)](echo/large-payload-8kib.md) ·
  [busy-poll](echo/busy-poll.md) ·
  [thread-per-core (echo-park)](echo/thread-per-core-park.md)
- **[gRPC](../scenarios/grpc.md)** —
  [throughput & pipelining (64 B)](grpc/throughput-64b.md) ·
  [payload size](grpc/payload.md) ·
  [client CPU & memory](grpc/cpu-memory.md)
- **[HTTP/1.1](../scenarios/h1.md)** —
  [throughput (64 B)](h1/throughput-64b.md) ·
  [thread-per-core](h1/thread-per-core.md) ·
  [large-payload (8 KiB)](h1/large-payload-8kib.md)

## Collection protocol

Results in this tree follow the shared **[collection protocol](../collection.md)** —
the canonical comparison-table schema, regime kinds, append-only dated blocks
(newest first) with a mandatory Date / Environment / Commit / Command provenance
header, and the coverage-matrix rules. Read it before adding results or a new
environment.
