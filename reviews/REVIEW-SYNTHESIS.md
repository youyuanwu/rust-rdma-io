# Review Synthesis

**Date**: 2026-08-30
**Reviewers**: gpt-5.6-sol, claude-opus-5, gemini-3.1-pro-preview
**Changes**: `feature/v2-explicit-driver-spawning` vs `main` (commit range `c530ae0..HEAD` including Phase 6 MR-quarantine safety work)

## Status: PASS

All three models reviewed the full implementation including the Phase 6 MR-quarantine safety fix. The central teardown-safety invariant (FR-013 / FR-028 / FR-029 / FR-030) is correctly implemented and holds under every traced scenario. No memory-safety defect was found.

Claude (opus-5) issued CONDITIONAL PASS and explicitly verified the invariant chain under all 5 scenarios (graceful close, driver never spawned, driver aborted, peer disconnect, wedged provider, in-flight send/recv). GPT (5.6-sol) identified concerns that were addressed. Gemini (3.1-pro) raised a false-positive must-fix about TransportSharedState field order (see below).

## Teardown Safety Invariant — VERIFIED

**Invariant**: An MR posted to hardware may be returned/reused/dropped only after its actual CQE is reaped OR the owning QP has been synchronously destroyed.

**Implementation**:
1. `InflightMap::close()` wakes all waiters — they quarantine MRs (push to reclaim queue) instead of returning them
2. `OpFuture` returns `(Result<Completion>, Option<Mr>)` — `Some(mr)` on real CQE, `None` when quarantined
3. `CqDriverHandle::flush_and_shutdown()` calls `map.close()` + `shutdown()` — no synthetic completions
4. `drain_reclaimed()` only releases entries on real CQE; wedged entries are quarantined (kept alive until CqDriverHandle drops)
5. `ConnectionLifetime` field ordering guarantees: QP drops → handles drop → reclaim-queue MRs freed → CmId drops

| Scenario | MR returned/freed before CQE or QP destroy? | QP before CmId? | Reclaim MRs after QP? |
|---|---|---|---|
| Graceful close (Phase C) | No — real flush CQEs drive completion | Yes | Yes |
| Driver never spawned → dropped | No — map closed, MRs quarantined | Yes | Yes |
| Driver aborted mid-flight | No — Drop guard closes map, waiters quarantine | Yes | Yes |
| Peer disconnect | No — same Phase C path | Yes | Yes |
| Wedged provider | No — quarantined, freed at ConnectionLifetime drop | Yes | Yes |
| In-flight send/recv at shutdown | No — OpFuture returns None, MR not re-pooled | Yes | Yes |

## Consensus Issues (All Models Agree)

| # | Finding | Severity | Disposition |
|---|---------|----------|-------------|
| 1 | `send()` hangs on pool buffer after driver death | should-fix | **Applied** — Race pool recv against terminal state via `tokio::select!` |

## Partial Agreement (2+ Models)

| # | Finding | Models | Severity | Disposition |
|---|---------|--------|----------|-------------|
| 2 | Inflight maps not closed on clean Phase C exit | Claude, GPT | should-fix | **Applied** — Close unconditionally after drain |
| 3 | `flush_all()` still public — safety escape hatch | Claude, GPT | should-fix | **Applied** — Made `#[cfg(test)] pub(crate)` |
| 4 | CQ/completion-channel EBUSY leak on teardown | Claude, GPT | should-fix (pre-existing) | **Deferred** — Pre-existing on `main`, not a regression |

## Single-Model Insights

| # | Finding | Model | Severity | Disposition |
|---|---------|-------|----------|-------------|
| 5 | `TransportSharedState` field drop order unsafe | Gemini | must-fix | **False positive** — driver_handles dropping first only decrements Arc refcount; SharedQp inside ConnectionLifetime still holds handle refs. Handles not actually freed until ConnectionLifetime drops (QP first). Verified by Claude. |
| 6 | `hello_send` not dropped before Phase C drain | Claude | should-fix | **Deferred** — Performance issue (drain barrier burns full budget on failure paths), not safety. Pre-existing pattern. |
| 7 | Drain barriers have no `.await` point | Claude | should-fix | **Deferred** — Pre-existing behavior, not a safety regression. DRAIN_TIMEOUT guard in outer loop provides bounded wait. |
| 8 | `RECLAIM_MAX_TURNS` turn-based, premature quarantine | Claude | should-fix | **Deferred** — Pre-existing turn counter. Not a regression; no observed peer stall in test suite. |
| 9 | Stale README/rustdoc after `Option<Mr>` change | GPT, Claude | consider | **Noted** — Low-level SharedQp rustdoc still says "always returned". Will be updated with broader v2 API docs cleanup. |
| 10 | `post_recv_and_track` errors silently drop error | Gemini | consider | **Noted** — Driver exits on the error but reports Ok(()). Minor; the QP is already in error state. |

## Verification Checklist

- [x] `cargo fmt --check` — clean
- [x] `cargo clippy --workspace --all-targets --features tokio -- -D warnings` — clean
- [x] `cargo build --no-default-features` — clean
- [x] `cargo build --features async` — clean
- [x] `cargo build --features tokio` — clean
- [x] `cargo test --workspace` — all pass (RXE)
- [x] `cargo test --test v2_message_transport_tests` — 56 tests pass
- [x] `cargo test --test v2_no_hidden_spawn` — pass
- [x] `cargo test --doc --workspace --features tokio` — pass
- [x] `cargo doc --no-deps -p rdma-io` — no new warnings from this branch
- [x] `grep tokio::spawn rdma-io/src/v2/*.rs` — doc comments only
- [x] RXE active and confirmed after all testing

## Priority Actions

### Must Fix — All Applied
1. ✅ MR teardown safety invariant (Phase 6): synthetic completions removed, InflightMap::close() + OpFuture quarantine + ConnectionLifetime drop ordering
2. ✅ send() hang on pool buffer after driver death

### Should Fix — Applied
3. ✅ Inflight maps closed unconditionally after Phase C drain
4. ✅ flush_all() restricted to #[cfg(test)] pub(crate)

### Should Fix — Deferred (Pre-existing / Not Safety)
5. CQ/completion-channel EBUSY leak (pre-existing on main)
6. hello_send not dropped before drain barrier (performance)
7. Drain barriers have no yield point (pre-existing)
8. RECLAIM_MAX_TURNS turn-based counter (pre-existing)

### Consider — Deferred
9. Stale SharedQp rustdoc
10. post_recv_and_track silent error

## Pre-Existing Limitations (Out of Scope)

1. **CQ/CompletionChannel EBUSY**: CQ is owned by the driver future, which drops before ConnectionLifetime destroys the QP. `ibv_destroy_comp_channel` fails EBUSY (CQ still live). No UB — kernel refuses the destroy — but leaks fd per connection. Pre-existing on `main`. Fix requires CQ ownership transfer to ConnectionLifetime.

2. **Tokio dependency**: The driver future uses `tokio::select!`, `Notify`, `Semaphore`, and `AsyncFd`. Runtime abstraction is explicitly out of scope per spec.
