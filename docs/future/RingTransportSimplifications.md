# Ring Transport Simplifications

**Status:** Backlog (Batches A–C landed; Batch D remaining)
**Date:** 2026-07-17 (originally reviewed 2026-07-16)
**Scope:** `rdma-io/src/{read_ring,credit_ring,send_recv}_transport.rs` +
`transport_common.rs`

A review of the three ring transports surfaced 14 concrete de-duplication
opportunities. The safe, high-value bulk (items #1–#8, #10, #11 — the shared
helpers, the setup-drain/token-exchange unification, and porting credit-ring onto
read-ring's transaction-guard setup design) has landed. What remains is the
higher-touch / host-sensitive work in **Batch D** below.

## Landed

| Batch | Items | Commit | What |
|---|---|---|---|
| A | #1–#8 | `2349b0b` | Extract shared CM-disconnect / teardown-drain / doorbell / immediate-data helpers into `transport_common`. |
| B | #10 | `da95f2e` | Unify the setup-send drain (`drain_ring_setup_sends` takes an `expected: &[u64]`) + token exchange (`exchange_setup_token`). |
| C | #11 | `02ff676` | Move `CmSetupEndpoint` to `transport_common`; add `PendingCreditRing` + `prepare_pending_credit`; reduce credit `connect`/`accept` to thin handshake wrappers. |

Details for the landed work are in those commit messages and the code.

## Gates (every batch)

`cargo fmt --all -- --check` +
`cargo clippy --workspace --all-targets --features tokio -- -D warnings` +
`cargo test -p rdma-io --lib --features tokio`. Batches that touch a runtime path
(recv/setup/send) additionally get a MANA reboot-clean run of the relevant suite
(`async_stream_tests` covers all three transports' connect/echo/shutdown).

> ⚠️ MANA host state: when the VMs are churn-degraded, busy/churn tests flake with
> ~74 s CM-wedge hangs even reboot-clean. Use the stash-and-run-baseline check to
> tell an environmental wedge from a regression (see the `rdma-io-dev` skill). Do
> **not** land the send-path change (#13) until the host is healthy enough to
> re-run the deadlock / echo / gRPC matrices.

---

## Batch D — remaining

Function names are stable; line numbers shifted with Batches A–C and are omitted.

### #9 — "find a free virtual `buf_idx` slot" linear scan (3 copies)

`credit_ring` (`drain_recv_credits`, `poll_recv`) and `read_ring` (`poll_recv`)
each re-implement the same scan of `virtual_idx_map` for a free slot, assign
`recv_arrival_seq`, and advance `next_virt_idx`.

- **Fix:** a shared `VirtualIndexMap { map, next }` with
  `alloc(offset, len) -> Option<(virt_idx, seq)>`.
- **Triage first (possible latent bug):** `credit::drain_recv_credits` omits the
  recv-ring offset/length bounds check that both `poll_recv` paths perform.
  Confirm whether that is safe by construction; if it is a real gap, fix it as a
  bug (own commit) **before/independent of** the cleanup.
- **Risk:** low-medium (recv path). ~40 lines.

### #12 — shared `from_parts` recv-state + `ReadRingTransport` delegation

Both rings' `from_parts` initialize the same recv-side state (`virtual_idx_map`,
`next_virt_idx`, `recv_arrival_seq`, `recv_stash`, `recv_tracker`,
`doorbell_repost_idx`, `send_ring`/`recv_ring`, `remote_write_tail`). Separately,
`ReadRingTransport` forwards all 12 `Transport` methods to `inner()`/`inner_mut()`
purely because busy-poll teardown needs to `take()` the inner.

- **Fix:** (a) group the shared recv-side fields into one `RingRecvState`
  constructed once; (b) macro-generate the delegation
  (`delegate_transport!(ReadRingTransport => inner)`).
- **Risk:** low-medium; weigh the greppability cost of the macro. ~40–60 lines.

### #13 — `send_gather` reserve/padding/rollback/WR-build

`read_ring send_gather` and `credit send_gather` share the reserve → (on
`padding > 0`) roll back `send_ring.tail` (identical 4-line rollback) → post
padding `RdmaWriteWithImm` → `gather_into` → build data `RdmaWriteWithImm`
sequence. The only divergence is the flow-control gate (credits vs
`remote_free_space`) and read-ring's proactive RDMA-Read hook.

- **Fix:** at minimum add a `RingBuffer::rollback_reserve()` helper (the tail
  restore is subtle and copied verbatim); optionally a shared
  `reserve_and_frame(...) -> Option<PostPlan>`. Keep each caller's flow-control
  checks and post-send hooks.
- **Risk:** medium — **deadlock-sensitive send path; refactor structure only,
  never inline-reap the CQ here (prior attempts caused lost wakeups).** Requires a
  healthy host to re-run the echo/gRPC deadlock matrices. ~35 lines.

### #14 — unify `RingToken` (20 B) and `ReadRingToken` (32 B)

`ReadRingToken` is a strict superset of `RingToken` (adds `offset_va` /
`offset_rkey`). Unifying into one 32-byte token (offset fields zero for credit)
would delete `RingToken`, its `to_bytes`/`from_bytes`, and one token-exchange
path.

- **Risk:** on-wire format + version-byte change (v1 vs v2); lowest priority, VMs
  required to re-validate. ~60 lines / medium-high.

**Order:** reassess once the host recovers; #9's bug triage is worth doing
independently of the rest.
