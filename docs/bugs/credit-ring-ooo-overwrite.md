# CreditRingTransport: Out-of-Order Repost Can Overwrite Unreleased Data

## Summary

When `repost_recv` is called out of order on `CreditRingTransport`, the sender
can wrap around the ring buffer and overwrite data that the receiver application
still holds a reference to via `recv_buf(buf_idx)`. The credit-based flow control
counts **messages**, not **ring byte positions**, so it cannot prevent positional
overwrites when releases are non-contiguous.

## Status

**Fixed** — `CompletionTracker`-based contiguous credit advancement prevents the overwrite.

## Root Cause

`repost_recv(buf_idx)` does two things:
1. `recv_ring.release(length)` — advances head by `length` (position-blind)
2. Sends a credit update (absolute count) to the sender

Credits control how many messages the sender can have in-flight, but NOT where
those messages land in the remote ring. The sender writes at `remote_write_tail`,
which advances sequentially and wraps around independently of the receiver's head.

When the receiver frees messages out of order, credits are returned for non-contiguous
slots. The sender can accumulate enough credits to wrap `remote_write_tail` into
a region where an unreleased message still resides.

## Reproduction Scenario

```
Ring: 64KB (65536 bytes), max_outstanding = 43, msg_size = 1500 bytes

1. Sender sends 43 messages, filling the ring
   Offsets: [0, 1500, 3000, ..., 63000]
   remote_write_tail = 64500, credits = 0

2. Receiver reposts messages 1–42 (NOT message 0 at offset 0)
   → 42 credits returned to sender
   → Message 0 at offset 0..1500 still held by application

3. Sender sends with wrap-around:
   a. Padding WR at offset 64500 → costs 1 credit → remote_write_tail = 0
   b. Data WR at offset 0        → costs 1 credit → overwrites message 0!

4. Application calls recv_buf(0) → returns CORRUPTED data
   (original message replaced by new message's bytes)
```

## Trigger Conditions (ALL required)

1. Ring is nearly full (most credits exhausted)
2. Receiver calls `repost_recv` out of order, leaving a gap near the ring head
3. Sender wraps `remote_write_tail` around to the gap position
4. Application still holds a `recv_buf` reference to the unreleased slot

## Why Credits Don't Prevent This

Credits are a **count** (scalar), not a **position set**. They answer "how many
messages can the sender send?" but not "where can the sender write?" The ring
buffer is a spatial structure, but credits abstract away spatial information.

```
Credits say:  "42 slots free"       → sender can send 42 messages
Ring says:    "slots 1–42 free,     → only positions 1500..63000 are safe
               slot 0 is occupied"
                                    → position 0 is NOT safe
Credits don't encode this ──────────────────────────┘
```

## Impact

- **Data corruption**: `recv_buf()` returns overwritten bytes
- **Silent**: No error returned — the overwrite is a valid RDMA Write
- **Hard to reproduce**: Requires near-full ring + out-of-order release + wrap-around
- **Practical risk**: Low — QUIC datagrams are typically processed and released
  in-order. But the `Transport` trait does not require in-order `repost_recv`.

## Proposed Fix

Use `CompletionTracker` (already exists at `credit_ring_transport.rs:238`,
currently `#[allow(dead_code)]`) to make credits position-aware:

```rust
fn repost_recv(&mut self, buf_idx: usize) -> Result<()> {
    let (offset, length) = self.virtual_idx_map[buf_idx].take().unwrap();

    // Mark this slot as released in the tracker.
    let slot = offset / self.config.max_message_size;  // or slot-granularity tracking
    let contiguous_advance = self.completion_tracker.release(slot);

    // Only send credits for contiguous head advances.
    if contiguous_advance > 0 {
        self.recv_ring.head = /* new contiguous head */;
        self.local_freed_credits += contiguous_advance;
        let imm = self.local_freed_credits as u32;
        post_send(SendWithImm(imm));  // credits reflect contiguous space only
    }
    // Non-contiguous release: slot is tracked but no credit sent yet.
    // Sender can't write into the gap.

    self.repost_doorbell()?;
    Ok(())
}
```

**Key insight**: Credits are sent only when the contiguous head advances. Freeing
B before A returns **0 credits**. When A is finally freed, the head chases forward
past A+B, sending 2 credits at once. The sender's `remote_write_tail` can never
reach unreleased positions because the credit count always corresponds to
contiguous free space from the wrap-around perspective.

## Affected Components

- `rdma-io/src/credit_ring_transport.rs` — `repost_recv()`, `recv_ring.release()`
- `rdma-io/src/credit_ring_transport.rs:238` — `CompletionTracker` (exists, unused)
- `ReadRingTransport` — implemented with chase-forward release

## Workaround

Call `repost_recv` in the same order as `poll_recv` returned completions. This is
the natural pattern for QUIC datagram processing and avoids the bug entirely.

## Related

- [rdma-read-ring-transport.md](../design/rdma-read-ring-transport.md#out-of-order-buffer-release) —
  ReadRingTransport design already requires CompletionTracker for the offset buffer
- msquic uses `RecvCompletionTable` hash tables for the same reason
