# CQ Completion Channel Lost Notification (EPOLLET)

**Status**: ✅ Fixed  
**Affects**: All async RDMA operations using `AsyncCq`  
**Severity**: High (causes indefinite hangs under concurrent I/O)

## Problem

`AsyncCq::poll_completions` could miss CQ completion notifications when
multiple events accumulated on the comp_channel fd between poll cycles.

### Root Cause

Tokio's `AsyncFd` uses edge-triggered epoll (EPOLLET). The previous code
consumed only ONE event from the comp_channel after readiness was signaled:

```rust
// BEFORE (buggy):
match self.notifier.poll_readable(cx) {      // clears tokio readiness
    Ready(Ok(())) => {
        self.channel.get_cq_event()?;         // consume ONE event
        self.ack_event();
    }
}
```

With EPOLLET, the edge trigger fires once when the fd transitions from
not-readable to readable. If two events accumulate before the trigger:

1. Both events arrive → one EPOLLET fire
2. `poll_readable` → Ready → `clear_ready()` (tokio forgets the fd is readable)
3. `get_cq_event` → consumes first event (second still on fd)
4. Later: arm CQ, poll CQ → empty → `poll_readable` → **Pending forever**
   (EPOLLET won't re-fire because the fd never transitioned from not-readable)

### When It Happens

This occurs under concurrent bidirectional I/O — exactly what HTTP/2 does.
The h2 protocol interleaves reads and writes on the same connection, and
both SEND and RECV completions arrive on the same CQ. Multiple completions
can accumulate between arm/drain cycles, producing multiple comp_channel events.

## Fix

Drain ALL events from the comp_channel after readiness is signaled:

```rust
// AFTER (fixed):
match self.notifier.poll_readable(cx) {
    Ready(Ok(())) => {
        self.drain_channel_events()?;     // drain until WouldBlock
    }
}

fn drain_channel_events(&self) -> Result<()> {
    loop {
        match self.channel.get_cq_event() {
            Ok(_) => self.ack_event(),
            Err(WouldBlock) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}
```

After draining, the fd is truly empty. The next completion notification
triggers a clean edge transition, and EPOLLET fires correctly.

## Verification

- All 40 tests pass from clean siw state
- Tonic gRPC test (concurrent bidirectional h2 I/O) no longer hangs
- Server graceful shutdown completes in <300ms (was hanging indefinitely)

## Related Bug: Work Completion Batch Stashing

A second bug was found in `AsyncRdmaStream::poll_read`/`poll_write`:
when a matching completion was found in the middle of an `ibv_poll_cq`
batch, the remaining completions in the same batch were silently
dropped. Under HTTP/2, send and recv completions interleave on the
same CQ and can appear in a single batch. Dropping them caused
permanent stalls.

Fix: before returning from the completion loop, stash remaining WCs
in `stashed_recv`/`stashed_send` for the next poll call.

## Related

- `rdma-io/src/async_cq.rs` — `poll_completions()` and `drain_channel_events()`
- `rdma-io/src/tokio_notifier.rs` — `TokioCqNotifier::poll_readable` (calls `clear_ready`)
- tokio `AsyncFd` EPOLLET documentation
