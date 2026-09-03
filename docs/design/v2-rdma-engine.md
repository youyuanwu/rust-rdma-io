# V2 RDMA Engine and Message Driver

## Overview

V2 separates shared RDMA runtime mechanics from per-connection message
protocol policy.

An `RdmaEngine` owns one device-scoped resource set and one explicit
`RdmaEngineDriver`. Low-level connections use that shared engine directly.
Each engine-bound message connection additionally returns one non-cloneable
`MessageTransport` frontend and one explicit `MessageTransportDriver`.

```text
application
  ├─ one RdmaEngineDriver task
  │    ├─ shared Context / PD / CQ / completion channel
  │    ├─ CM channel and generational route registries
  │    ├─ exact CQE validation and owned event dispatch
  │    └─ safe QP / CmId teardown and quarantine
  │
  └─ one MessageTransportDriver task per message connection
       ├─ HELLO negotiation and timeout
       ├─ DATA / CREDIT parsing and production
       ├─ receive reposting and registered-buffer pools
       ├─ connection-local fairness
       └─ message lifecycle and terminal outcomes
```

The library creates no task or thread. Applications may spawn the returned
drivers or poll them directly. V1 remains a separate API and is unchanged.

## Progress and Task Contract

`RdmaEngineBuilder::build()` returns:

```text
Result<(RdmaEngine, RdmaEngineDriver)>
```

`MessageTransportBuilder::connect_on()` and `accept_on()` return:

```text
Result<(MessageTransport, MessageTransportDriver)>
```

A typical client explicitly starts both progress owners:

```rust,no_run
use rdma_io::v2::*;

async fn run() -> Result<()> {
    let (engine, engine_driver) = RdmaEngineBuilder::new("rxe0").build()?;
    let engine_task = tokio::spawn(engine_driver);

    let (transport, message_driver) = MessageTransportBuilder::new()
        .connect_on(&engine, "192.0.2.1:7471".parse().unwrap())
        .await?;
    let message_task = tokio::spawn(message_driver);

    transport.ready().await?;
    transport.send(b"hello").await?;
    transport.close().await?;
    message_task.await.expect("message driver panicked")?;

    engine.shutdown().await?;
    engine_task.await.expect("engine driver panicked")?;
    Ok(())
}
```

An unpolled engine driver provides no CM, CQ, reclamation, or shutdown
progress. An unpolled message driver provides no HELLO, DATA, CREDIT, repost,
or message-lifecycle progress. Its HELLO timer is armed on first poll, so a
never-polled driver also provides no timeout guarantee. Dropping an unfinished
message driver publishes `Error::DriverShutdown` and asks the engine to close
the connection safely.

Readiness is the default engine completion mode. Building it requires an
active Tokio I/O runtime. Polling mode allocates no CQ completion channel and
may be built outside a runtime, but polling either driver still requires an
active Tokio runtime; Tokio time must be enabled before a deadline is armed.

## Layer Responsibilities

### Shared engine layer

One engine owns:

- an anchored `Context`;
- one `Pd`;
- one shared send/receive `Cq`;
- one CM event channel;
- one CQ completion channel in readiness mode, or none in polling mode;
- connection, operation, and CM-route registries;
- aggregate connection, operation, and CQ-credit admission;
- exact CQE validation and owned completion-event dispatch;
- connection drain, QP destruction, CmId destruction, and fail-closed
  quarantine.

The engine has a per-connection completion-dispatch queue so one connection
cannot monopolize event delivery. That queue contains validated low-level
completions only. It is not a message scheduler and does not parse frames,
manage message credits or pools, repost message receives, or own HELLO
deadlines.

Low-level `connect`, `connect_with_config`, `accept`, and
`accept_with_config` post no initial receives. Their callers own all operation
submission and buffers.

### Message layer

`MessageTransport` is the sole, non-cloneable application frontend for one
message connection. Its `send`, `recv`, `ready`, and `close` futures run in
the caller's task and communicate with the connection's driver.

`MessageTransportDriver` is the single logical writer for protocol state. It
owns:

- the HELLO deadline and capability negotiation;
- DATA and CREDIT frame processing;
- registered send/control pools and remote-credit accounting;
- completed receive delivery and receive reposting;
- connection-local scheduling and fairness; and
- translation of protocol, engine, peer-disconnect, and close events into one
  terminal message outcome.

Message setup allocates and posts every configured receive before
`rdma_connect` or `rdma_accept`. With defaults, the QP requirements are:

- 19 send WRs: 16 DATA, 2 control, and 1 HELLO;
- 34 receive WRs: 32 DATA and 2 control.

HELLO reuses a control receive; there is no additional receive.

## ADR: Crate-Private I/O Boundary

**Status:** accepted for the first issue #43 extraction milestone.

The destination architecture has three ownership layers:

1. **I/O core:** submission admission, provider posting, operation identity,
   exact CQE validation, accepted-set accounting, completion ownership, and
   post-QP-destruction reclamation.
2. **CM/session:** connect, accept, disconnect, route ownership, drain,
   retirement, QP-before-CmId ordering, and connection quarantine.
3. **Protocol:** HELLO, DATA, CREDIT, pools, receive reposting, message
   fairness, and frontend outcomes.

This milestone establishes the dependency boundary between protocol policy and
low-level I/O. It does not yet move the I/O core or CM/session state machines
into final standalone module hierarchies. The existing engine driver still
composes CQ, CM, reclamation, retirement, and shutdown progress.

The protocol receives one opaque connection capability before provider
establishment. It uses that capability to register message memory, prepost
receives, submit later SEND/RECV requests, request close, and await connection
close. It cannot access engine shared state, connection state, registries,
generational tokens, scheduler work bits, CM routes, or reconstruction
helpers.

Each submitted request transfers its MR and an opaque, protocol-owned context
to the I/O core. The post-reconciliation disposition reports one of these
ownership results:

- every request accepted;
- an exact accepted prefix plus a provider-proven unaccepted suffix;
- every request provider-proven unaccepted;
- the complete batch retained because provider acceptance is ambiguous; or
- the complete batch retained because an exact-prefix result raced with an
  already-observed CQE in the nominally unaccepted suffix.

The final case is intentionally fail-closed: an early suffix CQE proves that
the raw prefix classification no longer describes the ownership visible to
the operation ledger, so no suffix MR is returned as unaccepted.

One connection-scoped event port carries owned completion and terminal events.
Operation state stores only the opaque context and event destination—never a
protocol closure. Producers finish admission, posting, registry, accepted-set,
and lifecycle mutations before enqueueing an event or waking the protocol
driver. The port releases its queue mutex before wakeup. The message driver
pops both local protocol work and I/O events before processing them, alternates
the two sources, and uses check-register-recheck suspension for both wakers.

Unresolved accepted operations may be reclaimed only with an engine-private
QP-destruction proof. The proof has no public or protocol-visible constructor;
the connection owner creates it only after result-aware synchronous QP
destruction succeeds while the owning CmId is still alive. Drain consumes the
proof for operation reclamation. Zero-debt CM retirement and bounded driver
drop establish and discard the same proof without moving CM ownership.

Operation quarantine retains one operation's MR, registration, accepted-set
membership, and CQ debt. Connection quarantine retains the complete QP, CmId,
route, generation, admission, and unresolved operation bundle when no positive
release boundary can be proven. Protocol failure can request close, but it
cannot release or quarantine provider-visible ownership itself.

The crate-private boundary is deliberately concrete and unstable. It is not
re-exported from `rdma_io::v2`. Structural tests reject protocol references to
engine shared/connection state, registries, callback-era types, reconstruction
helpers, and detached-post methods; they also reject dependencies from the
boundary module to CM, listener, or message-policy modules.

## Completion-to-Message Handoff

The engine remains the only component allowed to validate and consume CQEs.
A completion must match the current operation generation, connection
generation, owning connection, provider-reported `qp_num`, and expected opcode
where the status is successful.

After validation, the engine removes operation ownership and creates an owned
completion event containing the opaque request context, completion result, and
releasable MR. Registry and admission guards are released before the event is
enqueued on the connection's I/O port and before the message driver is woken.

The driver then parses the frame or advances the corresponding send/repost
state. Neither the frontend nor the engine directly mutates driver-owned
protocol state.

Suspension uses check-register-recheck behavior: the driver checks for work,
registers both its local-work and I/O-port wakers, and checks again before
returning `Pending`. This prevents an event, terminal notification, frontend
close, or timeout from being lost between an empty-queue observation and
suspension. Events are removed from their queue before protocol processing.

## Wire Protocol, Credits, and Fairness

The internal message protocol has a 12-byte magic/version/type/length header
and three frame types:

- `HELLO` exchanges receive capacity and maximum message size;
- `DATA` carries one application message;
- `CREDIT` reports reposted receive capacity to the peer.

The codec is not public API.

Each DATA send consumes one negotiated remote receive credit. Dropping a
`ReceivedMessage` returns its MR to driver-owned repost work; after the repost
is accepted, the driver returns CREDIT to the peer. Holding all received
messages intentionally withholds all DATA receive capacity.

Within one driver turn, ready application events, control credit work, and
reposts are bounded and rotated. Pending CREDIT/repost work is explicitly
given opportunities between message events, so sustained DATA demand cannot
indefinitely starve control progress. The engine separately rotates validated
completion dispatch across connections. Neither layer promises real-time
latency.

## Hardware Ownership and CQE Routing

An MR offered to a provider remains owned by the engine until one of these
positive boundaries:

1. the provider proves the WR was not accepted;
2. the engine consumes the WR's exact validated CQE; or
3. synchronous destruction of the owning QP succeeds while its owning CmId is
   still alive.

QP ERR, cancellation, a deadline, CQ emptiness, driver loss, or an attempted
QP destruction is not a release boundary.

Connection and operation slots use non-wrapping generations. Exhausting a
generation retires the slot permanently. Stale, retired, duplicate, unknown,
wrong-connection, wrong-`qp_num`, and unexpected-success-opcode CQEs cannot
change live ownership. An exact error CQE, including a provider fatal or
unknown status, is consumed for that operation and delivered as
`Error::CompletionError`.

For linked posting batches, only a valid `bad_wr` pointer into the exact batch
proves a suffix unaccepted. A null, foreign, misaligned, or otherwise invalid
pointer leaves the complete batch acceptance-ambiguous, so all entries are
retained.

An exact prefix is also promoted to complete retained ownership if any CQE was
already observed for its nominally unaccepted suffix before the post call
returned. The provider classification remains the starting point; the
operation ledger's observed completion is the stronger ownership fact.

Providers differ in whether and when they emit flush CQEs. Teardown consumes
the exact flush CQEs that arrive, but never assumes that every accepted WR
will produce one.

## Close, Shutdown, and Quarantine

All shutdown orderings converge on engine-owned hardware teardown:

- **Frontend first:** dropping or closing `MessageTransport` wakes its driver;
  the driver stops message work and requests connection close.
- **Message driver first:** dropping the driver terminalizes pending frontend
  operations with `DriverShutdown` and requests close.
- **Engine first:** engine shutdown stops admission, publishes engine
  unavailability to each message driver, and safely drains or quarantines
  every connection.

The engine stops posting, transitions the local QP to ERR, and drains exact
CQEs. If accepted WRs remain at the drain deadline, it attempts synchronous
destruction of that exact QP before releasing any associated operation or MR.
Successful destruction creates the internal proof required by every unresolved
operation reclamation.

For a clean zero-debt retirement, successful QP destruction is also
established before the connection's CM route is retired. The owning CmId is
destroyed only after the QP and any required CM acknowledgement. Connection
and operation generations are retired only after their ownership is no longer
live.

If QP destruction fails or its result is uncertain, the engine fails closed.
It retains the exact QP, owning CmId, CM route and generation, admission
reservation, accepted operation records, CQ debt, and MRs as one bundle.
Neither another connection nor a later generation can reuse those resources.

`ConnectionQuarantined` describes outstanding hardware-visible work whose
release boundary could not be established.
`ConnectionDestroyQuarantined` describes failed zero-debt connection
finalization. If engine-wide shutdown cannot resolve unsafe ownership before
its deadline, it returns `EngineWedged`. After the sole engine driver is gone,
unresolved bundles are intentionally retained until process exit.

## Configuration

### Engine defaults

| Setting | Default | Range |
|---|---:|---:|
| Completion mode | Readiness | Readiness or Polling |
| Maximum live connections | 256 | 1–1,048,576 |
| Maximum in-flight operations | 16,384 | 2–16,777,216 |
| Shared CQ capacity | 16,384 | 2–16,777,216 |
| CQ completion budget | 32 | 1–4,096 |
| CM event budget | 32 | 1–4,096 |
| Reclamation budget | 32 | 1–4,096 |
| Completion-dispatch budget | 32 | 1–4,096 |
| Missing-CQE deadline | 30 s | 1 s–24 h |
| Connection drain deadline | 5 s | 1 ms–5 min |
| Engine shutdown deadline | 30 s | 1 ms–10 min |

Maximum in-flight operations cannot exceed CQ capacity. Device limits such as
`max_qp`, `max_qp_wr`, `max_sge`, `max_cqe`, and RDMA atomic depths are checked
without clamping.

### Message defaults

| Setting | Default | Validation |
|---|---:|---|
| DATA receive buffers | 32 | greater than zero |
| DATA send buffers | 16 | greater than zero |
| Maximum payload | 64 KiB | greater than zero and wire-representable |
| HELLO deadline | 10 s | 1 ms–5 min |

An explicit `RdmaConnectionConfig` may exceed, but cannot undershoot, the WR
requirements derived from the message configuration.

## Compact Diagnostics and Test Support

`RdmaEngine::diagnostics()` is an O(1) lifecycle and hardware-debt snapshot.
It reports only:

- lifecycle and an optional engine-wide terminal error;
- live connections;
- registered and accepted operations;
- pending reclamations;
- available and retained CQ credits;
- quarantined operations, MRs, bytes, and connections.

It intentionally has no per-object listings, configuration echoes, scheduler
visits, task-count declarations, event ledger, or message-protocol counters.
Operation and message futures carry their contextual errors.

The non-default, doc-hidden `rdma_io::v2::test_support` namespace is limited to
otherwise unobservable safety boundaries: exact-CQE suppression and routing,
posting acceptance, readiness-arm races, forced QP-destroy failure,
destruction order, exact route retention, and opaque shared-resource identity.
Malformed protocol tests use an independently encoded test peer rather than a
production frame-mutation hook.

## Validation

The complete local gate is:

```sh
just validate-v2-engine
```

It runs warning-denied feature builds, all-target workspace builds, formatting,
strict Clippy, rustdoc, doctests, recursive hidden-work and internal-boundary
guards, an isolated production build without `test-hooks`, and serialized
integration suites on both RXE and SIW.

The provider-only matrix is:

```sh
sudo -E env CARGO="$(command -v cargo)" \
  ./scripts/validate-v2-engine-providers.sh
```

The script positively identifies each provider, sets
`RDMA_REQUIRE_PROVIDER=1`, runs routing, readiness-race, lifecycle, listener,
message setup/behavior/retry, diagnostics, multi-connection, full-workspace,
and v1 safe-resource suites, then restores RXE. A self-skipped provider suite
is not a pass.

Useful focused modes include `--provider-probe`, `--readiness-race`,
`--driver-flush-gate`, `--operations`, `--connections`, `--listeners`,
`--lifecycle`, `--message-setup`, `--message`, and `--engine-conformance`.

## Limitations

- One RDMA device, anchored context, PD, and shared CQ per engine.
- RC QPs only; no UD, inline-data configuration, multi-SGE message API,
  atomics, or message ring transport in this layer.
- Tokio is the current engine/message-driver runtime integration.
- No byte-stream, tonic, Quinn, or V1 adapter is built into the v2 engine.
- Message send completion is local completion, not remote consumption.
- Message buffer pools are fixed for the connection lifetime.
- Low-level early SENDs can wait under RNR retry until a receive is posted.
- Quarantine intentionally retains memory, kernel objects, and admission when
  safe release cannot be proven.
- Bounded fairness is not a real-time guarantee.
