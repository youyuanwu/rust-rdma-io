# Code Simplification Opportunities — `rdma-io`

A review of the `rdma-io` crate (`rust-rdma-io/rdma-io/src/`) surfaced a set of
concrete simplifications. The bulk of the opportunity is in the two ring
transports (`read_ring_transport.rs`, 2139 lines; `credit_ring_transport.rs`,
1293 lines): `read_ring` has been refactored into a transactional-setup /
`Inner`-split / completion-backend design, while `credit_ring` still uses the
older copy-paste style, and several small helpers are duplicated verbatim across
all three transports.

Items are ordered lowest-risk first. Line references are approximate and were
current at review time.

> ⚠️ The send/recv completion path in both rings is deadlock-sensitive (see
> `docs/bugs/ring-send-queue-exhaustion.md` and
> `docs/bugs/read-ring-concurrent-stream-deadlock.md`). Structural refactors of
> the send path (#9, #10) must preserve completion timing exactly and be
> re-validated against the echo / gRPC deadlock matrices on the RDMA VMs.

---

## Tier 1 — trivial, zero behavior change

### 1. Shadowed `WR_ID_TOKEN_SEND` constant
`transport_common.rs:26` already defines `pub(crate) const WR_ID_TOKEN_SEND =
u64::MAX - 2`, and `read_ring_transport.rs:79` re-defines the identical value
locally, shadowing the glob import (`credit_ring` correctly uses the shared one).

- **Fix:** delete the local const in `read_ring_transport.rs`; rely on the
  `transport_common` one.
- **Saved / risk:** ~3 lines / very low.

### 2. Dead `_pad_len` binding
`credit_ring_transport.rs:645` computes `let _pad_len = self.recv_ring.capacity -
offset;` and never uses it (read-ring uses the analogous value for
`slot_lengths`; credit does not).

- **Fix:** delete the binding.
- **Saved / risk:** 1 line / very low.

### 3. Duplicated comment
`credit_ring_transport.rs:159-160` has two back-to-back
`// -- Virtual buffer index mapping --` comment lines.

- **Fix:** remove the redundant line.
- **Saved / risk:** 1 line / very low.

---

## Tier 2 — mechanical extract-to-`transport_common`

These are byte-for-byte (or near) duplicates across `read_ring`, `credit_ring`,
and in some cases `send_recv`. Each can move to a shared free function/helper in
`transport_common.rs`.

### 4. `check_cm_event` (identical in all three transports)
`read_ring_transport.rs:1428`, `credit_ring_transport.rs:588`,
`send_recv_transport.rs:289` — identical `try_get_event` → ack → set
`peer_disconnected` on `Disconnected`/error.

- **Fix:** `transport_common::check_cm_event(ch, &mut peer_disconnected) -> bool`.
- **Saved / risk:** ~40 lines / low.

### 5. `poll_disconnect` (identical in all three transports)
`read_ring_transport.rs:1904`, `credit_ring_transport.rs:1201`,
`send_recv_transport.rs:436` — identical `cm_async_fd.poll_read_ready` loop that
calls `check_cm_event`.

- **Fix:** pair with #4 — `transport_common::poll_cm_disconnect(fd, ch, &mut
  peer_disconnected, cx)`.
- **Saved / risk:** ~45 lines / low.

### 6. Bounded teardown drain loop + its three identical constants
`READ_RING_TEARDOWN_DRAIN_POLLS`, `CREDIT_RING_TEARDOWN_DRAIN_POLLS`,
`SEND_RECV_TEARDOWN_DRAIN_POLLS` are **all `4096`**, and the drain loop
(`to_error()` then `for _ in 0..N { s = try_drain; r = try_drain; if s==0 && r==0
break }`) is copied in the credit `Drop`, send_recv `Drop`, read_ring
`drain_teardown`, and the read_ring arm-park pending drop.

- **Fix:** one `const TEARDOWN_DRAIN_POLLS` + `transport_common::drain_flushed(&mut
  send_src, &mut recv_src)`.
- **Saved / risk:** ~40 lines + 2 consts / low.

### 7. `repost_doorbell` (identical between the two rings)
`read_ring_transport.rs:1447`, `credit_ring_transport.rs:607` — identical
round-robin post of a 4-byte doorbell recv WR over `doorbell_bufs` /
`doorbell_repost_idx`.

- **Fix:** extract a `DoorbellPool { bufs, repost_idx }` struct in
  `transport_common` with `repost(&qp)` and an init helper (also unifies the
  copied `doorbell_bufs` posting loop in both setups).
- **Saved / risk:** ~25 lines / low.

### 8. Immediate-data encode/decode (`(offset << 16) | len`)
The 16-bit offset|len packing is open-coded ~5×: decode at
`credit_ring_transport.rs:646`, `:1031`, `read_ring_transport.rs:1769`; encode in
both `send_gather` bodies. This is the same encoding that hard-caps the ring at
65536 (per repo notes).

- **Fix:** `transport_common::{encode_ring_imm(offset, len) -> u32,
  decode_ring_imm(u32) -> (usize, usize)}`, documenting the wire format once.
- **Saved / risk:** ~15 lines / very low.

---

## Tier 3 — recv/setup helper unification (safety-sensitive)

### 9. "Find a free virtual `buf_idx` slot" linear scan (3 copies)
`credit_ring_transport.rs` (`drain_recv_credits` ~660, `poll_recv` ~1069) and
`read_ring_transport.rs` (`poll_recv` ~1801) each re-implement the same scan of
`virtual_idx_map` for a free slot, assign `recv_arrival_seq`, and advance
`next_virt_idx`.

- **Fix:** a shared `VirtualIndexMap { map, next }` with `alloc(offset, len) ->
  Option<(virt_idx, seq)>`.
- **Note:** while consolidating, confirm intent — `credit::drain_recv_credits`
  omits the recv-ring offset/length bounds check that both `poll_recv` paths
  perform. Either it is safe by construction or it is a latent bug worth fixing.
- **Saved / risk:** ~40 lines / low-medium (recv path).

### 10. Setup-send drain + token-exchange helpers (common vs read_ring)
`read_ring::finish_setup_sends` (~653) duplicates
`transport_common::drain_ring_setup_sends` (the same expected-`wr_id` state
machine; read-ring just adds `WR_ID_OFFSET_MW_BIND`), and
`read_ring::complete_read_ring_token_exchange` (~468) duplicates
`transport_common::complete_token_exchange` (post signaled/inline token Send,
timeout-bounded recv, validate version + capacity ≤ 65536).

- **Fix:** make `drain_ring_setup_sends(send_src, expected: &[u64], deadline)`
  take the wr-id slice so read-ring's copy is deleted; factor a
  `recv_setup_token(qp, recv_src, deadline, buf)` that does the
  send/timeout/status-check, leaving only per-token parse/validate in each
  caller. Keep the exact-count checks.
- **Saved / risk:** ~70 lines / low-medium (setup correctness).

---

## Tier 4 — structural (biggest win, higher touch)

### 11. `credit_ring::connect` and `accept` are ~90% copy-paste
`credit_ring_transport.rs` `connect` (~229–370) and `accept` (~372–520) share
essentially their entire bodies: iWARP check, `ctx`/`pd`, `resolve_mr_rkey`,
`effective_max_outstanding`, CQ depths, `qp_attr`, arm-park sources,
`create_qp_with_cq`, MW bind, send/recv MRs, `doorbell_bufs`, `post_token_recv`,
`bind_recv_mw`, `complete_token_exchange`, `drain_ring_setup_sends`, and
`from_parts`. Only the CM handshake differs
(`resolve_addr/route/connect` vs `get_request/accept/await_established/migrate`).

`read_ring` already solved this with `prepare_pending` + a `CmSetupEndpoint`
trait (unifying client/server) + a `Pending*` transaction guard for
cancellation-safe teardown.

- **Fix:** port `credit_ring` onto the same pattern — move `CmSetupEndpoint` to
  `transport_common`, add `prepare_pending_credit`, and reduce `connect`/`accept`
  to ~15-line handshake wrappers. (The transaction guard exists specifically
  because setup drop-order is subtle, so reuse that design rather than a looser
  one.)
- **Saved / risk:** ~120–150 lines / medium (setup ordering + teardown safety).

### 12. `from_parts` shared recv-state + `ReadRingTransport` delegation boilerplate
Both rings' `from_parts` initialize the same recv-side state
(`virtual_idx_map`, `next_virt_idx`, `recv_arrival_seq`, `recv_stash`,
`recv_tracker`, `doorbell_repost_idx`, `send_ring`/`recv_ring`,
`remote_write_tail`). Separately, `ReadRingTransport` forwards all 12 `Transport`
methods to `inner()`/`inner_mut()` (~1944–1996) purely because busy-poll teardown
needs to `take()` the inner.

- **Fix:** (a) group the shared recv-side fields into one `RingRecvState`
  constructed once; (b) macro-generate the delegation
  (`delegate_transport!(ReadRingTransport => inner)`).
- **Saved / risk:** ~40–60 lines / low-medium.

### 13. `send_gather` reserve/padding/rollback/WR-build is near-identical
`read_ring send_gather` (~1584–1660) and `credit send_gather` (~878–940) share
the reserve → (on `padding > 0`) roll back `send_ring.tail` (identical 4-line
rollback) → post padding `RdmaWriteWithImm` → `gather_into` → build data
`RdmaWriteWithImm` sequence. The only divergence is the flow-control gate
(credits vs `remote_free_space`) and read-ring's proactive RDMA-Read hook.

- **Fix:** at minimum add a `RingBuffer::rollback_reserve()` helper (the tail
  restore is subtle and copied verbatim); optionally a shared
  `reserve_and_frame(...) -> Option<PostPlan>`. Keep each caller's flow-control
  checks and post-send hooks.
- **Saved / risk:** ~35 lines / medium — **deadlock-sensitive send path; refactor
  structure only, never inline-reap the CQ here (prior attempts caused lost
  wakeups).**

---

## Optional / higher-risk

### 14. Unify `RingToken` (20 B) and `ReadRingToken` (32 B)
`ReadRingToken` is a strict superset of `RingToken` (adds `offset_va` /
`offset_rkey`). Unifying into one 32-byte token (offset fields zero for credit)
would delete `RingToken`, its `to_bytes`/`from_bytes`, and one token-exchange
path.

- **Risk:** on-wire format + version-byte change (v1 vs v2); only worth doing with
  the RDMA VMs available to re-validate. ~60 lines / medium-high.

---

## Summary

| Tier | Items | Approx. lines saved | Risk |
|---|---|---|---|
| 1 — trivial | #1–#3 | ~5 | very low |
| 2 — extract helpers | #4–#8 | ~165 | low |
| 3 — recv/setup unify | #9–#10 | ~110 | low-medium |
| 4 — structural | #11–#13 | ~200–245 | medium |
| optional | #14 | ~60 | medium-high |

**Total addressable:** roughly 450–550 lines, the majority from porting
`credit_ring` onto `read_ring`'s setup design (#11) plus the shared-helper
extractions (#4–#8).

**Suggested sequencing:** land Tier 1 + Tier 2 first (mechanical, low risk, no
VM needed), then Tier 3, and only tackle Tier 4 / #14 with the RDMA VMs available
to re-run the deadlock and echo/gRPC benchmark matrices.

---

## Execution plan (reviewed 2026-07-16)

Each item above was re-verified against the current source. All are still
applicable; line numbers were current at review time. Three items (#1, #6, #10)
are duplication introduced or left behind by the recent read-ring setup-review
work (`PendingReadRing` transaction, `token_timeout`, exact setup drain, the
credit-ring A2/A3 port) — landing them "finishes" that shared-helper story.

**Gates for every batch:** `cargo fmt --all -- --check` +
`cargo clippy --workspace --all-targets --features tokio -- -D warnings` +
`cargo test -p rdma-io --lib --features tokio`. Batches that touch a runtime path
(recv/setup/send) additionally get a MANA reboot-clean run of the relevant suite
(`async_stream_tests` covers all three transports' connect/echo/shutdown).

> ⚠️ MANA host state: the VMs are churn-degraded this session — busy/churn tests
> flake ~74 s reboot-clean. Use the stash-and-run-baseline check to tell an
> environmental wedge from a regression (see the `rdma-io-dev` skill). Do **not**
> land any send-path change (#13) until the host is healthy enough to re-run the
> deadlock/echo/gRPC matrices.

### Batch A — Tier 1 + Tier 2 (mechanical, no VM needed) — ✅ DONE
Items **#1, #2, #3, #4, #5, #6, #7, #8**. Zero/behaviour-preserving; local gates
suffice. #1/#6 also remove duplication from the recent work.
- #1 delete the local `WR_ID_TOKEN_SEND` in `read_ring_transport.rs` (keep
  `WR_ID_OFFSET_MW_BIND` — genuinely read-ring-only).
- #4/#5 → `transport_common::{check_cm_event, poll_cm_disconnect}`.
- #6 → one `TEARDOWN_DRAIN_POLLS` const + `transport_common::drain_flushed(...)`.
- #7 `DoorbellPool`, #8 `{encode,decode}_ring_imm`.
- Validation: local gates only. One MANA `async_stream_tests` smoke at the end
  (it exercises the extracted CM-disconnect + teardown paths).

### Batch B — #10 (setup drain + token exchange unification) — ✅ DONE
Give `drain_ring_setup_sends(send_src, expected: &[u64], deadline)` a wr-id slice
so `read_ring::finish_setup_sends` is deleted; factor
`recv_setup_token(qp, recv_src, deadline, buf)` so read-ring's
`complete_read_ring_token_exchange` keeps only its per-token parse/validate. Keep
the exact-count/status checks. Setup-correctness sensitive → MANA reboot-clean
`read_ring_transport_tests` (5/5) + `async_stream_tests` (credit `*_ring`).

### Batch C — #11 (port credit-ring onto read-ring's setup design) — ✅ DONE
Move `CmSetupEndpoint` to `transport_common`; add `prepare_pending_credit` (reuse
the transaction-guard design — setup drop-order is subtle); reduce credit
`connect`/`accept` to thin handshake wrappers. This is effectively "the A5 dedup
for credit-ring". Medium risk → MANA reboot-clean `async_stream_tests`
(credit `*_ring` connect/echo/shutdown).

### Batch D — deferred / needs a healthy host or separate triage
- **#9** — do the map-scan unification, but first **triage the flagged
  latent bug**: confirm whether `credit::drain_recv_credits` legitimately omits
  the recv-ring offset/length bounds check the two `poll_recv` paths perform. If
  it's a real gap, fix it as a bug (own commit) before/independent of the cleanup.
- **#12** — `RingRecvState` + `delegate_transport!` macro; low-medium, but weigh
  the greppability cost of the macro. Optional.
- **#13** — `RingBuffer::rollback_reserve()` only; **deadlock-sensitive send
  path**, structure-only, never inline-reap the CQ. Requires a healthy host to
  re-run the echo/gRPC deadlock matrices.
- **#14** — token unification: on-wire/version change; lowest priority, VMs
  required to re-validate.

**Order:** A → B → C, then reassess D once the host recovers. A + B + C is the
bulk of the safe, high-value win (~330–420 lines) and needs only local gates plus
`async_stream_tests` smokes.
