# V2 Runtime RDMA Engine

## Overview

The v2 runtime engine is an explicitly driven, device-bound RDMA reactor for
many low-level and message connections. `RdmaEngineBuilder::build()` returns a
cloneable frontend and one `RdmaEngineDriver` future. The library starts no
task or thread: progress occurs only while the application polls that driver.

The ownership model is analogous to an io_uring instance or an IOCP completion
port:

- `RdmaEngine` and its connection/listener handles submit work and hold
  application-visible state.
- `RdmaEngineDriver` is the sole completion and connection-management
  consumer, comparable to the task that drains a completion ring or port.
- connection and operation generations are the engine's completion keys.
- the reported provider `qp_num` is an additional routing identity, not merely
  diagnostic data.

This is an analogy for ownership and progress. The implementation uses
libibverbs and librdmacm, not io_uring or IOCP.

V2 compatibility was not preserved. Endpoint-owned v2 drivers and
separate-CQ construction were removed rather than adapted. V1 modules, feature
behavior, APIs, and tests remain unchanged.

## Runtime and Task Contract

One engine has exactly one driver future. An application normally runs exactly
one task for that future, independent of the number of connections:

```rust,no_run
use rdma_io::v2::{RdmaEngineBuilder, Result};

async fn run() -> Result<()> {
    let (engine, driver) = RdmaEngineBuilder::new("rxe0").build()?;
    let driver_task = tokio::spawn(driver);

    // Create listeners, connections, and message transports through `engine`.

    engine.shutdown().await?;
    driver_task.await.expect("engine driver task panicked")?;
    Ok(())
}
```

The driver can instead be polled directly in an application-owned future:

```rust,no_run
use rdma_io::v2::{RdmaEngineBuilder, Result};

async fn run_without_spawning() -> Result<()> {
    let (engine, driver) = RdmaEngineBuilder::new("rxe0").build()?;
    let application = async {
        // Submit work through cloned engine handles.
        engine.shutdown().await
    };
    let (driver_result, application_result) = tokio::join!(driver, application);
    application_result?;
    driver_result
}
```

`RdmaEngine` is `Clone + Send + Sync + 'static`.
`RdmaEngineDriver` is `Future<Output = rdma_io::v2::Result<()>> + Send +
'static`. Connections, listeners, and message transports add zero library
tasks. There is no receive-pump task: receive completions, reposts, DATA,
CREDIT, HELLO, CM events, and reclamation are bounded work inside the engine
driver.

If the driver is never polled, connect, accept, completion, message readiness,
close, and shutdown work does not progress. Readiness mode does not use a
periodic timer to compensate for a missing wakeup.

Dropping the last `RdmaEngine` clone requests shutdown. Connections, listeners,
and message transports retain the shared state needed for memory safety, but
they are not engine frontend handles and do not prevent that request. Keep an
engine clone alive while submissions remain possible, and call
`shutdown().await` when the terminal result must be observed.

### Tokio requirements

- `CompletionMode::Readiness` is the default. `build()` must run inside an
  active Tokio runtime with I/O enabled because it registers the shared CQ and
  CM descriptors.
- `CompletionMode::Polling` allocates no CQ completion channel and may be built
  outside a runtime.
- Every driver poll must occur inside a Tokio runtime. Tokio time must be
  enabled before an operation can arm a HELLO, reclamation, connection-drain,
  or shutdown deadline.
- With `panic=abort`, callers must enable the needed Tokio I/O/time drivers;
  Tokio does not expose non-panicking capability probes for every case.

## Retained Independent V2 Surface

Production types have one public spelling under `rdma_io::v2::<Item>`.
Implementation modules such as `v2::context`, `v2::engine`,
`v2::message_transport`, and `v2::protocol` are private.

`Context::open_first()` and `Context::open_by_name()` select from
`rdma_get_devices` and retain that complete librdmacm list as their lifetime
anchor. First-device order and named availability follow librdmacm enumeration;
an independently verbs-openable device missing from that list is unavailable.
Repeated same-name opens may share librdmacm's cached raw context. Facade drop
never calls `ibv_close_device`; `rdma_free_devices` runs after the last
dependent PD, CQ, MR, and facade.

The one production CM-wrapper bridge is
`QpBuilder::build_with_cm(&rdma_io::cm::CmId)`. It validates exact raw context
identity before QP creation, and the resulting QP must be dropped before its CM
ID. There is no borrowed `Context::from_cm`, raw resource adoption/accessor, or
public V1 error/remote-MR conversion.

Direct CQ polling, generic notifier readiness, Tokio readiness, and externally
woken polling all use `rdma_io::v2::Completion` buffers. The typed completion
exposes `wr_id`, success, status, opcode, `qp_num`, byte length, vendor error,
and `result()`. SEND, RECV, RDMA WRITE, and RDMA READ are posted only through
the four named `Qp` methods; there is no duplicate `Op`/`OpCode` submission
facade.

The non-default `test-hooks` feature exposes one doc-hidden validation
namespace, `rdma_io::v2::test_support`. It owns V2 lifecycle observations even
though private instrumentation is placed at shared wrapper destructor sites.
It is not a V1 consumer API, raw-resource escape, CQ/CM consumer, or alternate
progress path.

## Public Engine API

The engine API is available with the `tokio` feature and is re-exported from
`rdma_io::v2`.

### Construction and lifecycle

```text
RdmaEngineBuilder::new(device_name: impl Into<String>) -> RdmaEngineBuilder
RdmaEngineBuilder::completion_mode(CompletionMode) -> RdmaEngineBuilder
RdmaEngineBuilder::maximum_live_connections(usize) -> RdmaEngineBuilder
RdmaEngineBuilder::maximum_inflight_operations(usize) -> RdmaEngineBuilder
RdmaEngineBuilder::cq_capacity(usize) -> RdmaEngineBuilder
RdmaEngineBuilder::cq_completion_budget(usize) -> RdmaEngineBuilder
RdmaEngineBuilder::cm_event_budget(usize) -> RdmaEngineBuilder
RdmaEngineBuilder::reclamation_budget(usize) -> RdmaEngineBuilder
RdmaEngineBuilder::ready_connection_quantum(usize) -> RdmaEngineBuilder
RdmaEngineBuilder::missing_cqe_deadline(Duration) -> RdmaEngineBuilder
RdmaEngineBuilder::connection_drain_deadline(Duration) -> RdmaEngineBuilder
RdmaEngineBuilder::shutdown_deadline(Duration) -> RdmaEngineBuilder
RdmaEngineBuilder::message_hello_deadline(Duration) -> RdmaEngineBuilder
RdmaEngineBuilder::build() -> Result<(RdmaEngine, RdmaEngineDriver)>

RdmaEngine::connect(SocketAddr) -> impl Future<Output = Result<RdmaConnection>>
RdmaEngine::connect_with_config(SocketAddr, RdmaConnectionConfig)
    -> impl Future<Output = Result<RdmaConnection>>
RdmaEngine::listen(SocketAddr, RdmaListenerConfig)
    -> impl Future<Output = Result<RdmaListener>>
RdmaEngine::diagnostics() -> RdmaEngineDiagnostics
RdmaEngine::shutdown() -> impl Future<Output = Result<()>>
```

`shutdown()` is idempotent. It closes admission, drains listeners and
connections, and returns the driver's terminal result. Concurrent callers
observe the same engine-wide outcome.

`CompletionMode` has exactly `Readiness` (default) and `Polling`.
`RdmaEngineLifecycle` reports `Created`, `Running`, `ShutdownRequested`,
`Terminated`, or `Failed`. The contextual v2 error family remains
`NoDevices`, `DeviceNotFound`, `Verbs`, `InvalidConfig`, `PostFailed`,
`CompletionError`, `WouldBlock`, `MessageTooLarge`, `TransportClosed`,
`DriverShutdown`, `CapacityExhausted`, `ConnectionQuarantined`,
`ConnectionDestroyQuarantined`, `EngineWedged`, and `ProtocolViolation`.

### Listener API

```text
RdmaListenerConfig::default()                         // userspace backlog 128
RdmaListenerConfig::backlog(usize) -> RdmaListenerConfig
RdmaListenerConfig::backlog_capacity() -> usize

RdmaListener::local_addr() -> Result<SocketAddr>
RdmaListener::accept() -> impl Future<Output = Result<RdmaConnection>>
RdmaListener::accept_with_config(RdmaConnectionConfig)
    -> impl Future<Output = Result<RdmaConnection>>
RdmaListener::close() -> impl Future<Output = Result<()>>
```

The userspace backlog is validated at `RdmaEngine::listen`, not by the setter,
and must be `1..=4,096`. The engine separately requests `i32::MAX` from
`rdma_listen`. A provider or kernel may clamp the kernel backlog, so some
requests may be refused before reaching the engine. A kernel refusal is a
listener-creation/provider error, not a userspace `BacklogFull` event.

Each listener has:

1. accept waiters ordered by registration;
2. admitted children ordered by CM request arrival; and
3. at most one selected/setup pair.

The oldest live waiter is paired with the oldest eligible child. Once selected,
that child cannot overtake to a later waiter. Cancellation before selection
removes only that waiter. Cancellation or setup failure after selection owns
the selected child through one reject/close disposition before another pair is
selected. Different listeners progress independently.

### Low-level connection API

```text
RdmaConnection::register_memory(usize, AccessIntent) -> Result<Mr>
RdmaConnection::send(Mr, Option<(usize, usize)>) -> RdmaOperation
RdmaConnection::recv(Mr, Option<(usize, usize)>) -> RdmaOperation
RdmaConnection::write(Mr, RemoteMr, Option<(usize, usize)>) -> RdmaOperation
RdmaConnection::read(Mr, RemoteMr, Option<(usize, usize)>) -> RdmaOperation
RdmaConnection::local_addr() -> Result<SocketAddr>
RdmaConnection::peer_addr() -> Result<SocketAddr>
RdmaConnection::identity() -> RdmaConnectionIdentity
RdmaConnection::close() -> impl Future<Output = Result<()>>

RdmaOperation: Future<Output = (Result<Completion>, Option<Mr>)>
```

An operation is submitted on first poll. The returned `Option<Mr>` is `Some`
after a positive completion/rejection boundary returns ownership to the
caller. It can be `None` when an accepted or acceptance-ambiguous WR must
remain engine-owned after cancellation or terminal failure.

Low-level `connect`, `connect_with_config`, `accept`, and
`accept_with_config` post exactly zero initial receives. A peer that sends
before the application posts a receive can therefore encounter RNR. The
default RNR retry value is 7 (infinite retry in the verbs encoding), so an
early send can stall until a receive is posted.

### Message transport API

```text
MessageTransportBuilder::new() -> MessageTransportBuilder
MessageTransportBuilder::recv_buffers(usize) -> MessageTransportBuilder
MessageTransportBuilder::send_buffers(usize) -> MessageTransportBuilder
MessageTransportBuilder::buffer_size(usize) -> MessageTransportBuilder
MessageTransportBuilder::connection_config(RdmaConnectionConfig)
    -> MessageTransportBuilder
MessageTransportBuilder::connect_on(&RdmaEngine, SocketAddr)
    -> impl Future<Output = Result<MessageTransport>>
MessageTransportBuilder::accept_on(&RdmaListener)
    -> impl Future<Output = Result<MessageTransport>>

MessageTransport::ready() -> impl Future<Output = Result<()>>
MessageTransport::send(&[u8]) -> impl Future<Output = Result<()>>
MessageTransport::recv() -> impl Future<Output = Result<ReceivedMessage>>
MessageTransport::close() -> impl Future<Output = Result<()>>

ReceivedMessage::len() -> usize
ReceivedMessage::is_empty() -> bool
ReceivedMessage: AsRef<[u8]> + Deref<Target = [u8]>
```

There is no public transport error accessor. Errors are observed from
`ready`, `send`, `recv`, and `close`, with engine-wide summaries available in
`RdmaEngine::diagnostics()`.

The default message builder uses 16 reusable DATA send buffers, 32 DATA receive
buffers, and a 64-KiB maximum payload. Its QP requirements are:

- send: `16 DATA + 2 control + 1 distinct HELLO = 19`;
- receive: `32 DATA + 2 control = 34`;
- message establishment pre-posts exactly all 34 receives before
  `rdma_connect` or `rdma_accept`.

HELLO consumes one control receive and that MR is reposted; there is no 35th
receive. With no explicit `connection_config`, the builder derives the exact
checked maxima `send_buffers + 2 + 1` and `recv_buffers + 2`. An explicit
configuration may exceed but may not undershoot either requirement.

`ready()` waits for HELLO exchange. `send()` also waits for readiness and
returns on local send completion, not remote consumption. `AsRef<[u8]>` and
`Deref<Target = [u8]>` expose exactly the received application payload,
excluding the 12-byte frame header. Dropping `ReceivedMessage` schedules its MR
for engine-driven repost and CREDIT return. Holding all received messages
therefore withholds all negotiated DATA credits and can intentionally stall
the peer until at least one handle is dropped.

The DATA, CREDIT, and HELLO format remains an internal wire contract exercised
through `MessageTransport`. V2 does not expose packet construction or parsing
helpers as public API.

Peer disconnect and normal flush completions are normalized to
`Error::TransportClosed` on HELLO, receive, and steady-state message paths.
Protocol and provider failures keep their contextual error. Pending waiters
are woken; there is no separate disconnect monitor or receive-pump task.

## Configuration and Provider Limits

### Engine settings

| Setting | Default | Inclusive range |
|---|---:|---:|
| Completion mode | Readiness | Readiness or Polling |
| Maximum live connections | 256 | 1–1,048,576 |
| Maximum in-flight operations | 16,384 | 2–16,777,216 |
| Shared CQ capacity | 16,384 | 2–16,777,216 |
| CQ completion budget | 32 | 1–4,096 |
| CM event budget | 32 | 1–4,096 |
| Reclamation budget | 32 | 1–4,096 |
| Ready-connection quantum | 32 | 1–4,096 |
| Missing-CQE deadline | 30 s | 1 s–24 h |
| Connection drain deadline | 5 s | 1 ms–5 min |
| Engine shutdown deadline | 30 s | 1 ms–10 min |
| Message HELLO deadline | 10 s | 1 ms–5 min |

Maximum in-flight operations must not exceed CQ capacity. Maximum live
connections must not exceed provider `max_qp`; CQ capacity must not exceed
`max_cqe`. Registry layout, integer conversion, checked arithmetic, requested
QP capabilities, and provider-returned actual QP capabilities are validated
without clamping.

### Connection settings

`RdmaConnectionConfig::default()` plus the following consuming setters form
the exact public configuration surface:

```text
max_send_wr(usize) -> RdmaConnectionConfig
max_recv_wr(usize) -> RdmaConnectionConfig
max_send_sge(usize) -> RdmaConnectionConfig
max_recv_sge(usize) -> RdmaConnectionConfig
responder_resources(usize) -> RdmaConnectionConfig
initiator_depth(usize) -> RdmaConnectionConfig
retry_count(usize) -> RdmaConnectionConfig
rnr_retry_count(usize) -> RdmaConnectionConfig
```

Configuration is write-only after construction: callers already own the values
they supply, defaults are documented below, and effective/provider state is
reported through connection or engine diagnostics rather than duplicate
builder getters.

| Setting | Default | Inclusive range | Provider limit |
|---|---:|---:|---|
| Maximum send WRs | 19 | 1–1,048,576 | `max_qp_wr` |
| Maximum receive WRs | 34 | 1–1,048,576 | `max_qp_wr` |
| Maximum send SGEs | 1 | 1–32 | `max_sge` |
| Maximum receive SGEs | 1 | 1–32 | `max_sge` |
| Responder resources | 1 | 0–255 | `max_qp_rd_atom` |
| Initiator depth | 1 | 0–255 | `max_qp_init_rd_atom` |
| Retry count | 7 | 0–7 | CM field |
| RNR retry count | 7 | 0–7 | CM field |

RC, signaled sends, zero inline data, and one SGE per default operation are
fixed. One default connection exposes 53 local WR positions. At the default
connection limit, `256 × 53 = 13,568`; the default global capacity leaves
`16,384 − 13,568 = 2,816` application-available positions. Admission charges
actual work and retained debt, not every unused QP position.

## Resource Ownership and Counts

One engine owns:

| Object | Readiness | Polling |
|---|---:|---:|
| Anchored verbs context facade | 1 | 1 |
| Protection domain | 1 | 1 |
| Send/receive completion queue | 1 | 1 |
| CQ completion channel/fd | 1 | 0 |
| CM event channel/fd | 1 | 1 |
| Tokio CQ `AsyncFd` adapter | 1 | 0 |
| Tokio CM `AsyncFd` adapter | 1 | 0 |
| Explicit engine driver future | 1 | 1 |
| Library-created tasks/threads | 0 | 0 |
| Additional message tasks | 0 | 0 |

Resource-object counts are measured from the owned handles. The
`explicit_engine_drivers = 1` and `library_owned_tasks = 0` values are
declarative construction invariants rather than runtime task observation:
`build()` returns exactly one driver, and the source-level
`v2_no_hidden_spawn` test independently rejects engine task creation.

All connections use the exact `ibv_context*` selected from
`rdma_get_devices`. The engine retains the complete device-list owner as a
context anchor. The safe context facade never calls `ibv_close_device`; every
outbound route-resolved ID and inbound child must have raw-pointer-equal
`id->verbs` before QP creation. `rdma_free_devices` runs only after all QPs,
CM IDs, MRs, CQ/channel resources, the PD, and context facades are gone.

Polling mode still owns the CM event channel and its nonblocking fd. It simply
does not register that fd with Tokio; the sole driver checks CM events within
its bounded polling turn.

Connection admission is aggregate across engine clones, listeners, outbound
requests, queued inbound children, selected setup, established connections,
drains, and quarantine. A quarantined bundle retains its connection
reservation, QP registration, operation registrations, MRs, and CQ debt.

## Completion Routing and Batch Ownership

Connection slots and operation slots use non-wrapping generations. Exhausting
a generation retires that slot permanently instead of wrapping. Registries are
lazily paged in groups of 256 slots, and exact lookup does a constant number of
direct probes without scanning idle connections.

Every CQE must agree with all of:

1. current connection slot and generation;
2. current operation slot and generation encoded in `wr_id`;
3. the operation's owning connection; and
4. the provider-reported `qp_num`.

Stale, duplicate, unknown, wrong-connection, wrong-`qp_num`, and
unexpected-opcode completions are rejected and counted. They never decrement
the accepted set or release a CQ credit.

Linked SEND/RECV batches keep stable WR storage across the provider call.
When posting fails, `bad_wr` is interpreted only if it points to a valid member
of that exact batch:

- WRs before `bad_wr` are an accepted prefix and retain operation/MR/CQ debt.
- `bad_wr` and the following suffix are provider-proven unaccepted and roll
  back.
- null, foreign, misaligned, or otherwise invalid `bad_wr` makes the complete
  batch acceptance-ambiguous, so all entries are retained conservatively.

## Wakeups, Polling, and Fairness

Software producers publish work before waking the driver. The driver registers
its waker and rechecks the published epoch/queue before sleeping. This
publish/wake/register/recheck order closes wake-before-register and
wake-during-register races.

CQ readiness uses:

1. drain the CQ;
2. arm notification;
3. immediately poll again;
4. await/read the shared completion fd only if still empty;
5. drain/ack events; and
6. recheck the CQ.

This closes CQE-before-arm and CQE-after-arm/before-wait races without a timer.
Polling mode performs one bounded CQ/CM/software turn and then cooperatively
yields.

Work classes rotate across terminal/control, CM, CQ, reclamation/deadlines, and
ready connections. Ready connections are duplicate-suppressed and tail-rotated
after at most `ready_connection_quantum` work. Idle connections are not
visited. This is bounded fairness, not a hard real-time latency guarantee.

### Synchronous provider calls

Future polling is nonblocking only in the Rust scheduling sense; individual
provider calls are synchronous and have no wall-clock guarantee.
`RdmaOperation` first poll can call `ibv_post_send` or `ibv_post_recv`.
`RdmaEngineDriver::poll` can poll/arm/get/ack CQ events; create, bind, listen,
resolve, connect, accept, reject, disconnect, or destroy CM IDs; create,
modify, post SEND/RECV work to, transition, or destroy QPs; and register or
deregister MRs.

Driver and resource `Drop` paths can additionally transition/destroy QPs,
destroy CM IDs/listeners, deregister MRs, destroy CQs and completion channels,
deallocate the PD and CM event channel, release context facades, and finally
free the anchored device list. Applications should run the driver where an
occasional provider stall cannot block unrelated latency-sensitive futures.

## Cancellation, Close, and Shutdown

Dropping an unposted operation returns local admission immediately. Dropping a
posted operation transfers observation to the engine. The engine retains its
MR, operation token, and CQ credit until an exhaustive positive safety boundary:

1. the provider proves that the WR was not accepted;
2. the engine consumes that WR's exact validated success, error, or flush CQE;
   or
3. synchronous destruction of the owning per-connection QP completes while
   its CM ID remains alive.

QP ERR, timeout, CQ emptiness, driver loss, and a no-match poll are not release
boundaries. QP destruction is a boundary only after the result-returning
`ibv_destroy_qp` call succeeds; no MR or accepted-WR debt is released before
that point. Engine QPs use caller-owned shared CQs, so this direct verbs call
does not invoke librdmacm's internal-CQ cleanup and cannot double-destroy the
shared CQ. The engine clears `cm_id->qp` only after success.

Connection close atomically stops posting, transitions the local QP to ERR
even after peer disconnect, and drains only exact identity-matching CQEs.
At the drain deadline, the sole driver first dispatches every exact CQE already
queued on that connection through the normal admission-serialized completion
path, then re-reads the accepted set. If it is empty, normal retirement runs
without an unnecessary fallback destroy. If accepted WRs remain because the
provider omitted or delayed flush CQEs, close destroys that same
per-connection QP while its CM ID remains alive, then resolves operation
observers/callbacks with the contextual close error, releases MRs and CQ/local
admission, retires operation generations, drains and destroys the CM ID, and
finally retires the connection generation. The post-destroy drain rejects only
CQEs that arrive after the boundary.

This deadline fallback is destructive: a provider success that was not polled
and queued before successful QP destruction is not fabricated or returned.
The operation completes with the contextual close error and any receive payload
is discarded. QP destruction failure retains the live QP, CM ID, MRs,
registrations, and CQ debt and publishes connection quarantine instead.

`Error::ConnectionQuarantined { outstanding_operations, cq_debt }` is reserved
for inability to establish the synchronous QP-destruction boundary, ambiguous
ownership, or another connection-local retirement wedge. It is not the normal
result of provider-omitted flush CQEs.

If result-aware destruction fails after the accepted set is already empty,
retirement enters terminal retained quarantine and `close()` returns
`ConnectionDestroyQuarantined` without failing the shared driver. The
connection registry generation, admission reservation, QP, and CM ID remain
owned and cannot be reused. A pre-registration setup rollback applies the same
retention to its establishing reservation and CM route. A quarantined
establishing reservation permanently pins its admission slot and quarantine
gauge even if a future owner is accidentally dropped. Its retention timestamp
also participates in `oldest_quarantine_age` even though it has no connection
registry token. Before retaining an inbound child, the engine sends a legal
pre-accept RDMA-CM rejection so the
peer fails promptly rather than waiting for its CM timeout. The connect or
accept caller still receives the original setup error; secondary reject and
destroy failures remain diagnostic, and the retained bundle is never retried
by driver-drop cleanup.

Accepted-set/operation-registry mismatches are defensive corruption guards:
normal production posting and completion mutate both on the sole driver. If a
mismatch is nevertheless observed after a successful QP boundary, every
ownership-valid token is still reclaimed; anomalous tokens remain quarantined
with explicit diagnostics rather than stranding the safe tokens.

If engine shutdown reaches its deadline with unsafe ownership,
`Error::EngineWedged { retained_bundles, outstanding_operations, cq_debt }`
is the shared engine-wide terminal result. Unresolved resources move to
fail-closed retention rather than being destroyed unsafely. Abrupt driver drop
or task abort wakes observed waiters and follows the same safety rule.

Fallback quarantine sinks are intentionally process-lifetime and unbounded.
They cannot be drained after the sole progress driver is gone because the
positive CQE/CM acknowledgement boundaries can no longer be proved. Repeated
unrecoverable engine failures in a long-lived process can therefore retain
kernel objects, memory, connection admission, and CQ credit until process
restart.

Graceful shutdown should be awaited before dropping the driver. Driver drop
performs bounded synchronous preparation, but individual ibverbs/librdmacm
destructors can block and have no wall-clock latency guarantee.

## Diagnostics

`RdmaEngine::diagnostics()` returns an O(1), nonblocking aggregate snapshot
that remains readable after terminal state while an engine handle exists.
Connection reservation/QP state uses maintained atomic state counts with a
versioned consistent read, rather than subtracting independently sampled
registry, drain, and quarantine sources during retirement. It contains:

- lifecycle and terminal error class/message;
- configured capacities, budgets, device, and completion mode;
- actual shared resource/object counts and task counts;
- aggregate establishing, established, draining, live, and quarantine gauges;
- free/retired registry slots, accepted operations, CQ credits, reclamation,
  retained MR/byte/bundle counts, and oldest quarantine age;
- listener queue totals and ready-queue depth; and
- monotonic admission, posting, batch, CQE/CM routing/rejection, lifecycle,
  cancellation, deadline, QP, quarantine, shutdown, wake, and yield counters.

The operation ledger separates `operations_completed` (exact CQEs) from
`operations_released_after_qp_destroy` (CQE-less forced releases).
`cq_credits_released` is their combined release count, so accepted/completed/
forced accounting remains explainable to terminal observers.

`RdmaEngineDiagnostics::connections()` and `listeners()` are explicit detailed
queries. They are O(number of current objects), return sorted snapshots, and
are separate from aggregate snapshot creation. Connection details include
identity, exact accepted outstanding count, drain state, and quarantine state.
Listener details include token, address, queued children, pending waiters, and
selected-pair count. A snapshot holds only a `Weak` detail source. If every
engine owner is gone before a detail query, the query returns an empty vector;
that result is intentionally indistinguishable from an engine with no current
objects. `PartialEq`/`Eq` compare all public snapshot data and ignore only this
internal weak handle.

The canonical public quarantine gauge is `quarantined_bundles`; each counted
bundle retains its QP registration, admission reservation, and unsafe debt.
The canonical retired-slot gauges are `retired_connection_slots` and
`retired_operation_slots`. Slot retirement is permanent, so each gauge also
serves as its monotonic retirement counter.

## Provider Validation

RXE and SIW both run exact success/error routing and close stress in readiness
and polling modes with no provider-specific skip. Real flush CQEs are consumed
when delivered. If a provider omits one, deterministic suppression tests prove
that synchronous owning-QP destruction precedes MR release, close remains
clean, accounting/generations are reusable, and late CQEs are rejected.
Injected result-aware destruction failures prove that MRs and CQ debt remain
quarantined and that externally supplied shared CQs are not destroyed twice.

Run the complete sequential provider validation with:

```sh
sudo -E ./scripts/validate-v2-engine-providers.sh
```

The script sets `RDMA_REQUIRE_PROVIDER=1`, so a missing rxe/siw device is a hard
test failure rather than a silently green early return. It also builds an
isolated release `rdma-io` artifact with `tokio` and without `test-hooks`
before running the full hook-enabled provider suites.

The `rdma-io-tests` helper library necessarily enables `test-hooks`: its shared
engine fixtures call doc-hidden injection, resource-lease, and destruction
recording APIs, and all integration targets link that helper crate. Cargo
therefore unifies `test-hooks` for workspace-wide test commands. Moving only
the dependency stanza cannot isolate the feature without splitting and
rewriting the shared test crate. Production configuration is instead validated
by the explicit isolated no-`test-hooks` release build; full RXE/SIW behavior,
including injected failure paths, remains validated with hooks enabled.

The script validates RXE first, removes it, validates SIW second, then always
removes SIW and restores/verifies RXE. It preserves the original test failure
while also reporting restoration failure.

Useful focused modes include `--provider-probe`, `--driver-flush-gate`,
`--readiness-race`, `--phase6-lifecycle`, and `--engine-conformance`.

## Limitations

- one RDMA device and one exact verbs context per engine;
- one shared CQ, not separate send/receive or per-connection CQs;
- RC QPs only; no UD, inline-data configuration, multi-SGE operation builder,
  atomics, or message ring transport in the engine surface;
- Tokio is the current driver runtime integration;
- no byte-stream, tonic, Quinn, or V1 transport adapter for the v2 engine;
- no dynamic message buffer resizing;
- message send completion means local completion, not remote consumption;
- low-level early sends can wait indefinitely under RNR retry 7 until a receive
  is posted;
- whole-bundle quarantine intentionally retains kernel/user resources and
  capacity when safety cannot be proven;
- bounded fairness is not a hard latency or real-time guarantee;
- provider limits may reject otherwise syntactically valid maximum settings;
- existing v2 endpoint callers must migrate; no compatibility layer is
  provided.
