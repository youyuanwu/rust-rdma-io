# V2 RDMA Engine and Message Driver

## Overview

V2 separates shared RDMA runtime mechanics from per-connection message
protocol policy.

An `RdmaEngine` owns one device-scoped resource set and one explicit
`RdmaEngineDriver`. Its internal composition root combines a low-level
`IoCore` with a `SessionManager`; low-level connection and listener frontends
hold narrow, resource-free capabilities rather than the shared engine state.
Each engine-bound message connection additionally returns one non-cloneable
`MessageTransport` frontend and one explicit `MessageTransportDriver`.

```text
application
  ├─ one RdmaEngineDriver task
  │    ├─ engine root: Context / PD / CQ / CM channel
  │    ├─ IoCore: posting / CQE validation / operation ownership
  │    └─ SessionManager: CM routes / connections / teardown / quarantine
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

### Engine composition root

One engine owns:

- an anchored `Context`;
- one `Pd`;
- one shared send/receive `Cq`;
- one CM event channel;
- one CQ completion channel in readiness mode, or none in polling mode;
- one `IoCore`; and
- one `SessionManager`.

The root owns engine-wide lifecycle, terminal state, work signaling, and the
canonical lifetime of device-scoped resources. It does not directly own
connection registries, CM state, session deadlines, or connection quarantine.
Driver-owned I/O and session progress values physically retain the live CQ and
CM readiness resources while they are polled; the root's canonical references
preserve final device-resource drop ordering.

### I/O core

`IoCore` owns operation generations and registrations, CQ admission, provider
posting reconciliation, exact CQE validation, copied and early completions,
cancellation and missing-CQE state, operation-level quarantine, and detached
completion effects. It has no production dependency on the engine composition
root, connection state, CM/listener state, session resource owners, or message
protocol policy.

### Session manager

`SessionManager` owns connection admission and the generational connection
registry, `CmState` and all routes, connect/listen/accept manager records,
listener and established-connection state, QP/CmId-owning bundles, lifecycle
deadlines, close/drain/disconnect/retirement policy, and connection-level
quarantine. It interprets the I/O effects that change session lifecycle before
detached events or wakers are published.

Session code does not receive or recover the concrete composition root.
Immutable engine and provider validation inputs, the I/O owner retain, and
memory-registration authority are copied into the manager at construction.
A bind-once `Weak<dyn SessionEngineRuntime>` capability exposes only admission
and terminal observation, shutdown state, failure escalation, shutdown-deadline
composition, and I/O/session work publication. It exposes no I/O or session
registry, provider resource, QP lifecycle authority, or protocol policy.

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

## ADR: Crate-Private I/O and Session Ownership Boundaries

**Status:** implemented and closure-audited on the v2 feature branch. The
protocol/I/O seam, low-level `IoCore`, narrow `SessionManager` boundary,
owner-local progress components, and thin engine scheduler satisfy the issue
#43 architecture. The issue remains open until its owner explicitly authorizes
closure.

The runtime has five distinct roles:

1. **I/O core:** submission admission, provider posting, operation identity,
   CQ resource/readiness polling, exact CQE validation, accepted-set
   accounting, completion scheduling, I/O deadlines, completion ownership,
   and operation-level quarantine.
2. **Session manager:** connect/listen/accept state, CM routes, connection and
   QP/CmId ownership, CM resource/readiness polling, drain/disconnect/
   retirement, lifecycle deadlines, lifecycle authority, shutdown
   coordination, and connection-level quarantine.
3. **Engine composition root:** global lifecycle and terminal-outcome
   composition across I/O and session readiness.
4. **Engine scheduler:** fair rotation over opaque I/O and session turns,
   post-owner terminal composition, global software-work register/recheck,
   earliest-deadline arming, and cooperative polling-mode yielding.
5. **Protocol:** HELLO, DATA, CREDIT, pools, receive reposting, message
   fairness, and frontend outcomes.

`EngineShared` is the composition root. It directly retains the device-scoped
resources, engine lifecycle, work signal, `Arc<IoCore>`, and
`Arc<SessionManager>`. Session collections and quarantine maps are fields of
`SessionManager`, not parallel fields on the root. The session manager holds
only a weak trait-object runtime capability back toward engine-wide state and
a copied `SessionConfig` containing live-connection capacity, the two
connection-validation capacities, and the connection-drain deadline.
I/O scheduling, completion policy, and engine shutdown policy remain solely in
`EngineConfig` and are not retained by the session owner. Session modules
cannot access the concrete root or use it as an I/O-owner shortcut.
`IoProgress` owns CQ readiness, the CQ buffer, completion-ready rotation,
operation deadlines, bounded I/O terminalization, and the narrow strong
`IoSessionBridge` used only while progress is composed. `IoCore` contains no
session bridge or post-construction binding, so operation futures may outlive
the driver without retaining session progress.
`SessionProgress` owns CM readiness, fair CM source selection,
connection/lifecycle deadlines, bounded shutdown scans, final CM draining, and
bounded session terminalization. The one `RdmaEngineDriver` sees none of those
state machines; it only polls and requeues the two owner classes. No component
creates a task or thread.

Each owner turn returns a private progress report containing only bounded
units consumed, whether immediate work remains, and readiness
registration/recheck status. The driver queries owner deadlines and terminal
eligibility directly; failures remain typed `Result` values. Post-guard
publication is a behavioral invariant covered by reentrant tests rather than
a constant report field. Reports contain no operation, connection, listener,
route, registry, queue, teardown, or deadline-kind identity.

Every driver poll probe-enqueues the I/O and session owners once because an
`AsyncFd` wake does not identify its source. Software pending bits additionally
identify the owning class. A private bounded driver turn snapshots the two
ready-at-entry owners, visits each at most once, appends remaining work for a
later poll, and then evaluates terminal eligibility exactly once. Shutdown,
failure, accepted-operation drain, and session cleanup publish owner work and
wake the driver; terminal composition has no scheduler class or work bit. A
final result is withheld until both bounded owners report cleanup complete. In
readiness mode an idle owner registers and rechecks its fd without requesting
another poll. In polling mode the driver yields cooperatively. The one driver
timer is armed to the minimum deadline reported by the two owners; equal
deadlines are serviced by fair owner rotation rather than cross-layer
insertion order.

Both owners use one payload-generic stable deadline queue. Equal timestamps
retain insertion order through a checked non-wrapping sequence, and only one
due payload is popped per accounted unit. Owner-neutral alternating-source
state preserves inbox/due fairness, odd-budget rotation, and unused-capacity
transfer. Operation tokens and reclamation remain I/O-owned; connection drain
and engine-shutdown deadline meanings remain session-owned.

The source hierarchy mirrors that ownership. `engine/session/mod.rs` defines
the manager and its lifecycle capabilities, while `session/cm.rs`,
`session/listener.rs`, `session/connection.rs`, `session/drain.rs`, and
`session/registry.rs` contain session-owned state and policy. The remaining
`engine/registry.rs` is not a connection owner: it provides opaque connection
and operation identities, exact live-I/O proofs, generic non-wrapping
generational registry storage, and lock helpers shared with `IoCore`. Public
connection and listener types continue to be re-exported by the engine facade,
so this physical relocation does not change public paths.

An established I/O capability carries immutable connection/QP identity, local
posting limits, operation ledgers, and a posting-only authority. That authority
uses a weak reference to the SessionManager-owned QP resource. Production
`RdmaConnection`, `RdmaListener`, and protocol `IoConnection` values retain
direct I/O/immutable state plus weak opaque session capabilities and
resource-free observers; they do not strongly retain or keep alive the shared
engine, `ConnectionState`, `ListenerState`, QP, or CmId. Suspended
connect/listen/accept futures likewise drop strong manager records before
awaiting.

Only `SessionManager` owns `SessionLifecycleAuthority`. QP ERR transition,
result-aware destruction, and final resource extraction require a reference to
that private authority. A successful synchronous QP destruction while the
CmId remains owned can mint one exact connection/`qp_num` proof. The proof is
private, non-copyable, non-cloneable, consumed by value for one reclaim
transaction, and cannot be replayed. Zero-debt retirement records destroyed
state without manufacturing a reclaim proof.

`IoCore` does not import the proof or any session owner. During the consuming
transaction, `SessionManager` passes the already-proven exact connection and
QP identities to the narrow reclaim operation. `IoCore` still verifies the
established I/O identity, exact operation generation and owner, accepted-set
membership, registration, local/CQ credit, and MR before releasing anything.
An anomalous token remains retained rather than making the proof reusable.

For a copied CQE, `IoCore` first resolves the exact operation generation.
`SessionManager` then proves that its registry still contains the operation's
connection generation and exact `qp_num`; the core checks opcode and duplicate
state before consuming ownership. Provider posting retains its existing
outcomes: accepted, exact accepted prefix plus proven-unaccepted suffix,
proven-unaccepted, or complete-batch retention for ambiguity or an observed
early suffix CQE.

One connection-scoped event port carries owned completion and terminal events.
Core mutations return owned effects for event delivery, operation wakes,
accepted-zero transitions, and operation-quarantine transitions.
`SessionManager` applies the session-facing quarantine and drained effects
before detached publication. The port releases its queue mutex before wakeup,
and the message driver preserves check-register-recheck suspension.

Operation quarantine retains one operation's MR, registration, accepted-set
membership, and CQ debt inside `IoCore`. `SessionManager` owns the combined
per-connection index that retains connection admission on the first operation
or connection quarantine key and recovers it only after the last clear.
Connection quarantine retains the QP/CmId-owning state, route, generation,
admission, and unresolved operations when no positive release boundary can be
proven. Protocol code can request close but cannot transition, release, prove,
or quarantine provider-visible ownership.

These boundaries are crate-private and deliberately unstable. V1 APIs are
unchanged; v2 replaces the aggregate reclamation-budget control with separate
I/O and session controls. AST guards reject hidden work, production `IoCore`
dependencies on root/session/connection/CM/listener/protocol types, strong
session-resource retention by frontends and waiters, lifecycle operations
without the private authority, public re-exports of internal capabilities, and
obsolete top-level session-module paths. The guards also parse the current
listed session sources, including test-only items and renamed imports, to
reject direct `EngineShared` dependencies; recursively reject broad
root/session `Deref` adapters and obsolete or newly renamed root-to-owner
forwarding methods; constrain the session runtime method/type surface and
`SessionConfig` fields; and require tests under `engine/io_core/` to use
explicit `IoCore`/`SessionManager` fixture parts with only an opaque runtime
retain.

This enforcement is intentionally narrower than a whole-engine module-graph
proof. New session submodules are not discovered automatically, concrete-root
fixture classification does not scan every engine test module, and aliases
are resolved within each parsed file rather than across files. The current
audited source satisfies the boundary, but these structural-enforcement gaps
remain an accepted review limitation rather than being represented as fixed.

## Completion-to-Message Handoff

The I/O progress owner is the only hardware-CQ poller; `IoCore` is the only
component allowed to validate and consume operation CQEs. A completion must
match the current operation generation, session-proven connection generation,
owning connection, provider-reported `qp_num`, and expected opcode where the
status is successful.

After validation, the core removes operation ownership and creates an owned
completion event containing the opaque request context, completion result, and
releasable MR. Registry, admission, posting, and operation-ledger guards are
released before the event is enqueued on the connection's I/O port and before
the message driver is woken.

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

The SessionManager stops posting, uses its private lifecycle authority to
transition the local QP to ERR, and lets `IoCore` drain exact CQEs. If accepted
WRs remain at the drain deadline, it attempts synchronous destruction of that
exact QP before releasing any associated operation or MR. Successful
destruction creates one internal proof consumed by the exact unresolved
operation-reclamation transaction.

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
| I/O reclamation budget | 16 | 1–4,096 |
| Session reclamation budget | 16 | 1–4,096 |
| Completion-dispatch budget | 32 | 1–4,096 |
| Missing-CQE deadline | 30 s | 1 s–24 h |
| Connection drain deadline | 5 s | 1 ms–5 min |
| Engine shutdown deadline | 30 s | 1 ms–10 min |

Maximum in-flight operations cannot exceed CQ capacity. Device limits such as
`max_qp`, `max_qp_wr`, `max_sge`, `max_cqe`, and RDMA atomic depths are checked
without clamping.

The owner-local reclamation controls replace the former aggregate
`reclamation_budget`. To preserve an old aggregate value, divide it between
`io_reclamation_budget` and `session_reclamation_budget`; either owner may
receive the extra unit for odd values. The old aggregate value `1` has no exact
equivalent because both owners require a nonzero turn, so the minimum
replacement is `(1, 1)`. No compatibility alias is provided.

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

Colocated unit tests name the owning `io_core` or `session` fixture explicitly.
They do not use root-to-session-to-I/O `Deref`, root forwarding methods, or a
strong root field on test connection frontends. A test connection may retain
its `ConnectionState` directly when a lifecycle or accounting assertion needs
that session-owned fixture; this retain exposes neither the root nor another
owner.

## Validation

The complete local gate is:

```sh
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 just validate-v2-engine
```

It runs warning-denied feature builds, all-target workspace builds, formatting,
strict Clippy, rustdoc, doctests, recursive hidden-work and internal-boundary
guards, an isolated production build without `test-hooks`, and serialized
integration suites on both RXE and SIW.

The provider-only matrix is:

```sh
sudo -E env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  CARGO="$(command -v cargo)" \
  ./scripts/validate-v2-engine-providers.sh
```

The script positively identifies each provider, propagates
`CARGO_BUILD_JOBS` through nested user switching, sets
`RDMA_REQUIRE_PROVIDER=1`, runs routing, readiness-race, lifecycle, listener,
message setup/behavior/retry, diagnostics, multi-connection, full-workspace,
and v1 safe-resource suites, then restores RXE. A self-skipped provider suite
is not a pass.

Useful focused modes include `--provider-probe`, `--readiness-race`,
`--driver-flush-gate`, `--operations`, `--connections`, `--listeners`,
`--lifecycle`, `--message-setup`, `--message`, and `--engine-conformance`.

## Issue #43 Closure Evidence

The final audit classifies composition-root uses rather than treating symbol
count as the goal:

| Criterion | As-built evidence |
|---|---|
| Lowest I/O layer excludes message, listener, and CM policy | `IoCore` owns posting, exact CQE validation, operation accounting, readiness, and reclamation behind a structural dependency guard. |
| Message policy excludes engine/session internals | `MessageTransportDriver` uses only `IoConnection`, its event port, and opaque close capability; the structural guard rejects root, registry, and lifecycle internals. |
| CM/listener/session state is outside the I/O core | `SessionManager` owns CM routes, listeners, connections, lifecycle authority, teardown, deadlines, and connection quarantine under the `engine/session/` hierarchy. |
| Composition root does not re-own owner policy | `EngineShared` assembles owners and coordinates global lifecycle, signaling, diagnostics, terminal state, and lifetime ordering. Session-to-engine access is the weak narrow runtime capability described above. |
| Exact routing and fail-closed provider ownership remain intact | Unit and RXE/SIW provider suites cover generation/QP/opcode validation, duplicates, accepted prefixes, proven rejection, acceptance ambiguity, and missing completions. |
| Positive release and teardown boundaries remain intact | Tests cover proven non-acceptance, exact completion, successful QP-destruction proof, QP-before-route/CmId retirement, and complete-bundle quarantine after failed destruction. |
| Publication and progress contracts remain explicit | Tests cover post-guard callbacks/wakers, bounded owner turns, fair rotation, terminal composition, and the recursive no-hidden-task/thread guard. |
| Transitional seams are removed | The aggregate reclamation alias and old source paths remain absent; the final cleanup removes stale migration annotations, root/session test dereference, root forwarding, and full-root I/O fixtures. |
| V1 remains separate | No v1 source is changed by this cleanup, and the complete provider gate retains the v1 safe-resource suite. |
| Documentation matches implementation | This document distinguishes policy ownership, physical readiness/resource retention, narrow runtime composition, and bounded test support. |

This matrix records implementation coverage; actual closure remains a human
issue-management action. The final pull-request report must include the exact
serialized gate result and any environmental limitation before recommending
closure.

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
