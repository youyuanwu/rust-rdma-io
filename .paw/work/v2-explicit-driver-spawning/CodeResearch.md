# Code Research: V2 Explicit Driver Spawning

**Branch**: feature/v2-explicit-driver-spawning | **Date**: 2026-08-30

## tokio::spawn Sites to Remove (Production)

| File | Line | What | Context |
|------|------|------|---------|
| `rdma-io/src/v2/message_transport.rs` | 568 | `tokio::spawn(recv_pump(...))` | Inside `from_connection()` |
| `rdma-io/src/v2/message_transport.rs` | 589 | `tokio::spawn(disconnect_monitor(...))` | Inside `from_connection()` |
| `rdma-io/src/v2/connection.rs` | 411 | `tokio::spawn(driver.run_tokio())` | `spawn_driver()` readiness path |
| `rdma-io/src/v2/connection.rs` | 416 | `tokio::spawn(driver.run())` | `spawn_driver()` polling path |

## MessageTransportBuilder API

- `connect()`: `message_transport.rs:235-279` → returns `Result<MessageTransport>`
- `accept()`: `message_transport.rs:282-316` → returns `Result<MessageTransport>`
- Both call `from_connection()`: `message_transport.rs:460-611`

### from_connection() Step Sequence
1. Resource sizing: `:472-473`
2. HELLO MR allocation + serialize: `:476-481`
3. Send HELLO: `:482-485`
4. Wait peer HELLO with timeout + poll_any_ready: `:488-522`
5. Repost HELLO recv buffer: `:519-521`
6. Credit init (`Semaphore::new(peer_capacity)`): `:529`
7. Send pool allocation: `:535-542`
8. Control MR pool: `:545-552`
9. Recv channels: `:555-556`
10. **SPAWN recv_pump**: `:558-581`
11. **SPAWN disconnect_monitor**: `:583-595`
12. Return Self: `:597-610`

## recv_pump() Implementation: `message_transport.rs:867-1036`

Takes: qp, send/recv handles, pd, initial recv futures, channels, credits, closed flag
Loop structure:
- Drain pending credits (`:896-909`)
- If no recv buffers: wait repost_rx/control MR return (`:911-943`)
- Otherwise: `poll_any_ready()` for recv completions (`:947-1012`)
  - FRAME_DATA → send to app channel (`:958-963`)
  - FRAME_CREDIT → add_permits + repost control MR (`:965-974`)
  - FRAME_HELLO/unknown → warn + repost (`:975-991`)
- Concurrent repost channel monitoring (`:1014-1026`)

Helpers:
- `poll_any_ready()`: `message_transport.rs:1040-1062`
- `post_recv_and_track()`: `message_transport.rs:1065-1084`
- `post_send_and_detach()`: `message_transport.rs:1088-1111`

## disconnect_monitor() Implementation: `message_transport.rs:783-828`

- Waits `cm_async_fd.readable()` + `event_channel.try_get_event()` (`:790-819`)
- Breaks on Disconnected/DeviceRemoval (`:798-800`)
- Idempotent shutdown via `closed.compare_exchange()` (`:820-827`)
- On close: `credits.close()` + `flush_and_shutdown()` all handles (`:824-826`)

## CQ Driver Architecture

### FdCqDriver: `driver.rs:239-373`
- Fields: `cq, handle, inflight` (`:239-242`)
- `new()`: `:244-258` → returns `(Self, Arc<CqDriverHandle>)`
- `run_tokio()`: `:265-273` → creates TokioCqNotifier, calls `self.run(notifier)`
- `run()`: `:275-373`
  - Arm CQ notify (`:281`)
  - Poll CQ (`:284-289`)
  - Reclaim drain (`:291-292`)
  - `tokio::select!` on shutdown/reclaim/work/readable/fallback (`:295-323`)
  - Drain completions after wakeup (`:326-335`)
  - Final drain barrier (`:339-372`)

### PollingCqDriver: `driver.rs:402-481`
- Fields: `cq, handle, inflight` (`:402-406`)
- `new()`: `:409-420` → returns `(Self, Arc<CqDriverHandle>)`
- `run()`: `:437-481`
  - Bounded polling loop (`:441-461`)
  - yield_now after budget/idle (`:448-458`)
  - Final drain barrier (`:463-479`)

### CqDriverHandle: `driver.rs:58-194`
- Fields: shutdown_tx, reclaim_tx, work_notify, inflight, generation (`:58-71`)
- `notify_work()` (`:90-94`), `shutdown()` (`:96-101`)
- `push_detached()` (`:113-143`), `drain_reclaimed()` (`:145-184`)
- `flush_and_shutdown()` (`:186-194`)
- `READINESS_POLL_FALLBACK` constant (`:34-35`)

## ConnectionBuilder + Connection: `connection.rs`

### Connection struct: `:94-103`
Fields: shared_qp, driver_handles, driver_tasks (`Vec<JoinHandle>`), pd, cm_id, event_channel, cm_async_fd, shutdown_initiated

### spawn_driver(): `:403-418`
- Readiness: `FdCqDriver::new()` + `tokio::spawn(driver.run_tokio())` (`:409-413`)
- Polling: `PollingCqDriver::new()` + `tokio::spawn(driver.run())` (`:414-418`)
- Returns `(Arc<CqDriverHandle>, JoinHandle<Result<()>>)`

### SharedQp construction in connect/accept:
- Separate CQs: `:253-263` / `:354-364`
- Shared CQ: `:275-277` / `:374-376`

## SharedQp: `shared_qp.rs`
- Struct: `:74-79` (qp, send_handle, recv_handle, pd)
- `new(qp, send_handle, recv_handle, pd)`: `:93-106`

## Protocol: `protocol.rs`
- Frame types: DATA/CREDIT/HELLO (`:1-35`)
- `CREDIT_FRAME_SIZE` (`:66`), `HELLO_FRAME_SIZE` (`:69`)
- `write_credit_frame()` (`:145-151`)
- `write_hello_frame()` (`:159-167`)
- `parse_hello()` (`:240-262`)
- `parse_credit()` (`:269-279`)

## MessageTransport Struct + API: `message_transport.rs`
- Fields: `:438-459` — connection, buffer_size, send_pool_tx/rx, recv_msg_rx, repost_tx, remote_credits, closed, recv_pump_task, disconnect_task
- `send()` (`:634-713`), `recv()` (`:737-752`), `close()` (`:764-789`)
- JoinHandles as `Option<JoinHandle<()>>` (`:455-457`)
- `close()` aborts tasks (`:771-785`)
- `Drop` aborts tasks (`:792-803`)

## OpFuture + Cancellation: `shared_qp.rs`
- Struct: `:269-275`
- `Drop`: `:405-419` — pushes detached via `handle.push_detached()` (no spawn)

## Error Types: `error.rs:21-67`
Existing: NoDevices, DeviceNotFound, Verbs, InvalidConfig, PostFailed, CompletionError, WouldBlock, MessageTooLarge, TransportClosed, DriverShutdown, CapacityExhausted, ProtocolViolation

## Module Re-exports: `mod.rs`
- Always: Context, Cq, CqBuilder, Error, Result, etc. (`:63-69`)
- Async: Completions, CqPoller, CqNotifier (`:72-78`)
- Tokio: CqDriverHandle, FdCqDriver, PollingCqDriver, OpFuture, SharedQp, CompletionMode, Connection, MessageTransport, MessageTransportBuilder, ReceivedMessage, TokioCompletions (`:81-93`)

## Test Files
- `rdma-io-tests/tests/v2_message_transport_tests.rs` — 29 test functions, all via `make_transport_pair()` (`:22-43`)
- `rdma-io-tests/tests/v2_tests.rs` — lower-level v2 tests
- `rdma-io-tests/tests/v2_shared_qp_tests.rs` — SharedQp tests

## README
- V2 message transport: `README.md:157-205`

## Tokio Support: `tokio_support.rs:1-22`
- `TokioCompletions` type alias (`:10`)
- `Cq::completions_tokio()` (`:12-22`)

## Cargo.toml Features
- `default = ["tokio"]` (`:12`)
- `tokio` feature gates all v2 driver/transport types (`:14`)
