# V2 Single-Owner Reactor Migration Contract

Status: implementation contract for
[issue #59](https://github.com/youyuanwu/rust-rdma-io/issues/59).

This document freezes the state inventory, target contracts, migration order,
and deletion gates before an adapter path is introduced. It describes the
baseline at commit `30b4a0bc5dfa07fac689a60e2b7fa4114cbb554b`.
The implementation status remains the architecture described in
[v2-rdma-engine.md](v2-rdma-engine.md) until later phases update that document.

## Non-Negotiable Boundaries

- `RdmaEngineDriver` remains the sole caller-polled owner of shared v2 engine
  progress. The library creates no hidden task, thread, or alternate executor
  ([engine/mod.rs:219-246](../../rdma-io/src/v2/engine/mod.rs#L219-L246),
  [driver/mod.rs:220-290](../../rdma-io/src/v2/engine/driver/mod.rs#L220-L290)).
- V1 is unchanged.
- Existing provider-safety implementations remain authoritative. Adapters do
  not copy WR acceptance reconciliation, CQE validation, ownership release,
  QP reclamation, or teardown logic.
- Scheduling and provider equivalence must pass before mutable lifecycle state
  is consolidated.
- Phase 2 migrates only connect, listen, connection-close, and shutdown command
  admission. Accept, listener-close/drop, listener identity, and the existing
  waiter/child/selected lifecycle remain on the one authoritative current path
  through the scheduling-equivalence gate. Phase 8 moves listener identity and
  bounded accept/child admission together; no parallel listener identity or
  accept path is permitted.
- `LiveIoConnectionProof` and `QpDestructionProof` remain explicit runtime
  evidence in the final architecture.
- Message transport is not modified during core command/scheduler Phases 1-5.
  Its engine-facing ownership adapter is a separate Phase 5A milestone.
  `MessageTransportDriver` protocol and scheduling remain separate.

## Current End-to-End Flows

### Construction and explicit progress

`RdmaEngineBuilder::build` validates configuration, creates provider resources
and `EngineShared`, then returns one frontend and one unspawned driver
([engine/mod.rs:219-246](../../rdma-io/src/v2/engine/mod.rs#L219-L246),
[engine/mod.rs:572-634](../../rdma-io/src/v2/engine/mod.rs#L572-L634)).
The driver owns `IoProgress`, `SessionProgress`, `OwnerScheduler`, and a shared
deadline sleep
([driver/mod.rs:94-128](../../rdma-io/src/v2/engine/driver/mod.rs#L94-L128)).
Each poll snapshots work, gives each owner at most one ready-at-entry turn,
composes terminal state, and registers/rechecks before suspension
([driver/mod.rs:199-290](../../rdma-io/src/v2/engine/driver/mod.rs#L199-L290)).

### Connect

First polling a connect future validates configuration, takes the admission
read guard, reserves connection capacity, queues an `OutboundRequest`, releases
admission, and wakes session progress
([cm/outbound.rs:23-54](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L23-L54)).
Driver-owned CM work creates and generationally routes the `CmId`, resolves
address/route, builds the QP/resource bundle, runs optional setup, and calls
`rdma_connect`
([cm/outbound.rs:56-148](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L56-L148),
[cm/outbound.rs:246-419](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L246-L419)).
Establishment publishes a take-once result; cancellation keeps provider-owned
cleanup in session progress
([cm/outbound.rs:447-477](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L447-L477),
[cm/outbound.rs:762-838](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L762-L838)).

### Listen and accept

First polling listen queues `ListenRequest`; CM work creates a listener ID,
installs exact routing, and currently requests provider backlog `i32::MAX`
([listener.rs:177-200](../../rdma-io/src/v2/engine/session/listener.rs#L177-L200),
[cm/inbound.rs:20-106](../../rdma-io/src/v2/engine/session/cm/inbound.rs#L20-L106)).
Accept queues an ordered `AcceptRequest`
([listener.rs:202-229](../../rdma-io/src/v2/engine/session/listener.rs#L202-L229)).
Inbound children consume connection admission, pair with the oldest waiter or
enter the userspace backlog, and retain exact child ownership through
establishment, cancellation, close, or quarantine
([listener.rs:694-831](../../rdma-io/src/v2/engine/session/listener.rs#L694-L831),
[cm/inbound.rs:228-370](../../rdma-io/src/v2/engine/session/cm/inbound.rs#L228-L370)).

### Scalar and protocol operations

Creating a scalar operation does no provider work. Its first future poll
currently validates, acquires admission/posting guards and credits, allocates a
generational operation, calls the provider, and reconciles the result
([operation/future.rs:91-173](../../rdma-io/src/v2/engine/io_core/operation/future.rs#L91-L173),
[operation/future.rs:197-353](../../rdma-io/src/v2/engine/io_core/operation/future.rs#L197-L353)).
Protocol batches call the same I/O owner synchronously and consume one stable
ownership ledger into accepted, exact-prefix, or ambiguous outcomes
([io.rs:82-103](../../rdma-io/src/v2/engine/io.rs#L82-L103),
[operation/batch.rs:140-404](../../rdma-io/src/v2/engine/io_core/operation/batch.rs#L140-L404)).

### Completion

`IoProgress` polls the shared CQ, resolves `wr_id`, validates the current
connection generation and QP, validates successful opcode, and records
completion ownership once
([io_core/progress.rs:154-230](../../rdma-io/src/v2/engine/io_core/progress.rs#L154-L230),
[operation/completion.rs:118-191](../../rdma-io/src/v2/engine/io_core/operation/completion.rs#L118-L191)).
Bounded connection dispatch removes the operation slot and releases accepted
membership, local capacity, CQ credit, and retained debt only through the
single completion path
([operation/completion.rs:215-305](../../rdma-io/src/v2/engine/io_core/operation/completion.rs#L215-L305)).
Session effects commit before events and operation wakes are published.

### Close and teardown

Close shuts posting, moves the QP to error, detaches observers, schedules a
drain deadline, and waits for accepted work
([session/drain.rs:12-65](../../rdma-io/src/v2/engine/session/drain.rs#L12-L65)).
If CQEs do not prove release, successful destruction of the exact owning QP
mints `QpDestructionProof`; reclamation revalidates connection, QP, and
operation identity
([session/drain.rs:103-165](../../rdma-io/src/v2/engine/session/drain.rs#L103-L165),
[operation/reclamation.rs:141-217](../../rdma-io/src/v2/engine/io_core/operation/reclamation.rs#L141-L217)).
Only then may the route retire and its `CmId` be destroyed. Failed or uncertain
destruction retains the complete resource bundle
([cm/retirement.rs:300-393](../../rdma-io/src/v2/engine/session/cm/retirement.rs#L300-L393)).

### Shutdown and driver loss

Normal shutdown closes admission, schedules bounded connection/CM cleanup, and
publishes a terminal result only after I/O and session work are quiescent
([engine/mod.rs:637-733](../../rdma-io/src/v2/engine/mod.rs#L637-L733),
[session/progress.rs:63-227](../../rdma-io/src/v2/engine/session/progress.rs#L63-L227)).
Driver drop is synchronous because no later poll exists. It closes admission,
attempts safe destruction, terminalizes observers, and process-lifetime
quarantines the root when provider ownership remains uncertain
([engine/mod.rs:770-791](../../rdma-io/src/v2/engine/mod.rs#L770-L791),
[engine/mod.rs:879-927](../../rdma-io/src/v2/engine/mod.rs#L879-L927)).

### Message transport

Message setup posts receive batches before connect/accept; normal protocol work
is driven by a separate explicit `MessageTransportDriver`
([message_transport.rs:249-418](../../rdma-io/src/v2/message_transport.rs#L249-L418),
[message_transport.rs:1798-1899](../../rdma-io/src/v2/message_transport.rs#L1798-L1899)).
Core Phases 1-5 leave this path unchanged.

## State, Invariant, Test, and Disposition Inventory

| State or mechanism | Required invariant and existing evidence | Final disposition |
|---|---|---|
| `RdmaEngineBuilder`, `RdmaEngine`, `RdmaEngineDriver` | Build returns one unspawned driver; withholding it prevents CM progress ([api_tests.rs:84-95](../../rdma-io/src/v2/engine/api_tests.rs#L84-L95), [v2_engine_connection_tests.rs:660-697](../../rdma-io-tests/tests/v2_engine_connection_tests.rs#L660-L697)) | Retain public API; Phase 8 removes `EngineShared` from the frontend; Phase 4 makes the driver own `EngineReactor` |
| `RdmaConnection`, `RdmaListener`, `RdmaOperation`, `IoConnection` | Current handles preserve close/cancel/result behavior but connection/operation/protocol handles reach shared owners ([connection/mod.rs:70-200](../../rdma-io/src/v2/engine/session/connection/mod.rs#L70-L200), [operation/future.rs:54-184](../../rdma-io/src/v2/engine/io_core/operation/future.rs#L54-L184), [io.rs:49-124](../../rdma-io/src/v2/engine/io.rs#L49-L124)) | Phase 2 migrates connect/listen/connection-close/shutdown admission only; Phase 3 public scalar operations; Phase 5A protocol I/O; Phase 8 accept/listener identity, bounded admission, and listener close together. Retain stable IDs and resource-free result plumbing |
| `MemoryRegistrar` | Immutable shared-PD registration capability; no mutable engine registry ([io.rs:24-47](../../rdma-io/src/v2/engine/io.rs#L24-L47)) | Retain |
| `ConnectionSetup` | Setup completes and accepted posts are verified before connect/accept ([cm/tests.rs:483-577](../../rdma-io/src/v2/engine/session/cm/tests.rs#L483-L577)) | Phase 5A changes the argument to borrow-scoped `BorrowedSetupIo<'_>`; retain the one-shot contract |
| Request/waiter observers and close states | Take-once results, cancellation, and register/recheck publication ([cm/outbound.rs:670-838](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L670-L838), [listener.rs:323-638](../../rdma-io/src/v2/engine/session/listener.rs#L323-L638)) | Phase 8 replaces named implementations with typed resource-free completion cells |
| `EngineShared` and admission barrier | Serializes admission/shutdown, owns terminal state and final resources ([engine/mod.rs:439-492](../../rdma-io/src/v2/engine/mod.rs#L439-L492), [engine/mod.rs:637-768](../../rdma-io/src/v2/engine/mod.rs#L637-L768)) | Phase 8 deletes shared mutable root/barrier; retain command admission, immutable diagnostics/terminal observation, and complete-reactor quarantine |
| `WorkSignal` | Epoch plus register/recheck closes producer races ([driver/mod.rs:49-86](../../rdma-io/src/v2/engine/driver/mod.rs#L49-L86), [driver/tests.rs:184-241](../../rdma-io/src/v2/engine/driver/tests.rs#L184-L241)) | Phase 4 replaces owner bits with one reactor source epoch and retains register/recheck |
| `OwnerScheduler`, `AlternatingSources` | Ready-at-entry owner/source rotation bounds and alternates work ([scheduler.rs:13-68](../../rdma-io/src/v2/engine/scheduler.rs#L13-L68), [scheduler.rs:153-200](../../rdma-io/src/v2/engine/scheduler.rs#L153-L200)) | Delete in Phase 4 after unified source rotation tests pass |
| `DeadlineQueue` and deadline inboxes | Checked sequence preserves equal-deadline order; bounded transfer and expiry ([scheduler.rs:70-151](../../rdma-io/src/v2/engine/scheduler.rs#L70-L151)) | Retain heaps in reactor from Phase 4; move I/O inbox in Phase 6 and session inbox in Phase 7; delete inbox locks |
| Engine resource split | Anchors context/PD/CQ/CM lifetime and readiness ([resources.rs:16-157](../../rdma-io/src/v2/engine/resources.rs#L16-L157)) | Phase 8 replaces split bundles with one reactor-owned canonical bundle; retain readiness and destruction order |
| `PagedRegistry` plus operation/connection/route/listener indexes | Non-wrapping generations and exact stale/duplicate routing ([registry.rs:115-277](../../rdma-io/src/v2/engine/registry.rs#L115-L277), [registry.rs:521-548](../../rdma-io/src/v2/engine/registry.rs#L521-L548)) | Phase 6 moves operation registry, Phase 7 connection/routes, Phase 8 listeners; Phase 7 deletes synchronized generic storage after all users migrate |
| `IoCore`, `EstablishedIoConnection`, operation registry, credits, accepted set, completion/reclamation queues | One authoritative provider ledger and exact release accounting ([io_core/mod.rs:90-304](../../rdma-io/src/v2/engine/io_core/mod.rs#L90-L304), [io_core/mod.rs:348-520](../../rdma-io/src/v2/engine/io_core/mod.rs#L348-L520)) | Phase 6 replaces with private reactor-owned `IoState` and local connection-I/O records; retain accounting and queues without owner locks |
| `OperationState`/`OperationInner` and completion marker | MR, completion, cancellation, and quarantine ownership moves exactly once ([operation/state.rs:26-65](../../rdma-io/src/v2/engine/io_core/operation/state.rs#L26-L65), [operation/state.rs:133-414](../../rdma-io/src/v2/engine/io_core/operation/state.rs#L133-L414)) | Phase 6 moves backend state into reactor records; retain only resource-free frontend synchronization |
| `IoCoreEffects` family | Session mutation commits before events/wakes; reentry sees released guards ([operation/effects.rs:11-185](../../rdma-io/src/v2/engine/io_core/operation/effects.rs#L11-L185), [operation/tests.rs:1341-1685](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1341-L1685)) | Delete in Phase 4 after all publication uses bounded `ReactorActions` |
| `IoEventPort` family | Queue-before-wake and register/recheck preserve events ([io.rs:385-571](../../rdma-io/src/v2/engine/io.rs#L385-L571), [io.rs:642-687](../../rdma-io/src/v2/engine/io.rs#L642-L687)) | Retain as resource-free engine/message result plumbing |
| `SessionManager`, session deadline and quarantine maps | Owns CM, connection, listener, deadline, and quarantine policy ([session/mod.rs:299-448](../../rdma-io/src/v2/engine/session/mod.rs#L299-L448)) | Phase 7 moves connection/deadline state, Phase 8 listener/shutdown state and deletes the owner/maps/locks |
| Connection admission, reservation, and gauges | Exact establishing/established/draining/quarantine accounting ([connection/mod.rs:794-1122](../../rdma-io/src/v2/engine/session/connection/mod.rs#L794-L1122)) | Phase 7 moves facts into generational connection entries and retains exact gauges |
| `ConnectionState`, outbound/inbound routes, QP indexes, shared CM context-route index | Exact generation/raw/context/QP route and lifecycle ownership ([connection/mod.rs:283-365](../../rdma-io/src/v2/engine/session/connection/mod.rs#L283-L365), [cm/mod.rs:975-1273](../../rdma-io/src/v2/engine/session/cm/mod.rs#L975-L1273)) | Phase 7 replaces independently mutable connection owners with one connection enum and moves connection-only indexes. The single context-route index remains authoritative because it also contains listener routes; Phase 8 moves it with listener identity, without a parallel map or dispatcher |
| `ListenerState`, listener identity/indexes, waiter/child/selected queues, backlog layers | FIFO selection, cancellation ownership, bounded child backlog ([listener.rs:640-831](../../rdma-io/src/v2/engine/session/listener.rs#L640-L831), [listener.rs:1176-1428](../../rdma-io/src/v2/engine/session/listener.rs#L1176-L1428)) | Retain together on the authoritative current path through Phase 7. Phase 8 moves identity, state, accept admission, and close together into the reactor; public backlog bounds accept requests and children and is passed to the provider; remove `i32::MAX` policy |
| Retirement/CM-destruction queues and QP/CM bundles | QP destruction precedes route/CM destruction and `WouldBlock` gates ID destruction ([cm/retirement.rs:17-167](../../rdma-io/src/v2/engine/session/cm/retirement.rs#L17-L167), [cm/retirement.rs:300-393](../../rdma-io/src/v2/engine/session/cm/retirement.rs#L300-L393)) | Phase 7 moves connection retirement and bundles into reactor-owned connection state but retains the one common CM-destruction service because it also owns listener destruction. Phase 8 moves that service with listener lifecycle; retain one FIFO destruction barrier and complete bundles |
| `SessionProgress`, `IoProgress`, `CmShutdownCursor`, bounded scan cursors | Bounded CQ/CM/deadline/reclamation/terminal service ([io_core/progress.rs:21-129](../../rdma-io/src/v2/engine/io_core/progress.rs#L21-L129), [session/progress.rs:17-122](../../rdma-io/src/v2/engine/session/progress.rs#L17-L122)) | Delete progress owner structs in Phase 4; retain readiness, deadline heaps, CQ buffer, and cursors as reactor source fields |
| Message state, MR pools, queues, send requests, received messages, and driver | Separate explicit protocol runtime with bounded resources and budget 32 ([message_transport.rs:571-863](../../rdma-io/src/v2/message_transport.rs#L571-L863), [message_transport.rs:1708-1899](../../rdma-io/src/v2/message_transport.rs#L1708-L1899)) | Retain unchanged; Phase 5A changes only the engine I/O adapter |

### Detailed queue, registry, and migration-type dispositions

This table expands the grouped inventory above. Target-only adapter rows are
identified as design decisions; their listed tests are mandatory exit evidence
for the phase that introduces them.

| Mechanism | Current invariant and evidence | Existing or required test evidence | Exact disposition |
|---|---|---|---|
| `CmState.pending` | FIFO owns admitted outbound requests until one bounded CM software unit consumes them ([cm/mod.rs:98-140](../../rdma-io/src/v2/engine/session/cm/mod.rs#L98-L140), [cm/mod.rs:252-335](../../rdma-io/src/v2/engine/session/cm/mod.rs#L252-L335)) | Strong request ownership without retaining the engine root is covered by [cm/tests.rs:27-68](../../rdma-io/src/v2/engine/session/cm/tests.rs#L27-L68); Phase 2 adds explicit FIFO/cancel/service coverage | Phase 2 replaces it with the bounded connect command lane and deletes the queue |
| `CmState.pending_listens` | FIFO owns listen requests until CM software work creates the listener ([cm/mod.rs:98-140](../../rdma-io/src/v2/engine/session/cm/mod.rs#L98-L140), [cm/mod.rs:313-332](../../rdma-io/src/v2/engine/session/cm/mod.rs#L313-L332)) | Suspended listen ownership is covered by [listener.rs:1127-1164](../../rdma-io/src/v2/engine/session/listener.rs#L1127-L1164); Phase 2 adds explicit FIFO/capacity coverage because the baseline has no queue-order test | Phase 2 replaces it with the bounded listen command lane and deletes the queue |
| `CmState.cancellations` | Defers outbound cancellation to the CM owner rather than releasing provider state from a dropped waiter ([cm/mod.rs:98-121](../../rdma-io/src/v2/engine/session/cm/mod.rs#L98-L121), [cm/outbound.rs:151-204](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L151-L204)) | Cancellation after provider connect is covered by [v2_engine_connection_tests.rs:727-780](../../rdma-io-tests/tests/v2_engine_connection_tests.rs#L727-L780) | Phase 7 replaces it with the per-token coalesced reactor control set and deletes the queue |
| `CmState.listener_work` | Deduplicates listener service requests through `work_enqueued` and bounded software scheduling ([cm/mod.rs:121-143](../../rdma-io/src/v2/engine/session/cm/mod.rs#L121-L143), [listener.rs:746-831](../../rdma-io/src/v2/engine/session/listener.rs#L746-L831)) | FIFO/cancellation/close action behavior is covered by [listener.rs:1200-1428](../../rdma-io/src/v2/engine/session/listener.rs#L1200-L1428) | Retain through scheduling and connection consolidation; Phase 8 replaces it with the reactor listener-ready set when listener identity/lifecycle moves |
| `CmState.retirements` | Owns connection tokens awaiting bounded retirement ([cm/mod.rs:104-106](../../rdma-io/src/v2/engine/session/cm/mod.rs#L104-L106), [cm/retirement.rs:300-393](../../rdma-io/src/v2/engine/session/cm/retirement.rs#L300-L393)) | QP-before-CM retirement is covered by [v2_engine_lifecycle_tests.rs:287-442](../../rdma-io-tests/tests/v2_engine_lifecycle_tests.rs#L287-L442) | Phase 7 moves it unchanged to a reactor-owned FIFO and removes its mutex |
| `CmState.cm_destructions` / `PendingCmDestruction` | Retains exact connection or listener CM owners until the event channel reaches `WouldBlock` ([cm/mod.rs:104-106](../../rdma-io/src/v2/engine/session/cm/mod.rs#L104-L106), [cm/mod.rs:909-925](../../rdma-io/src/v2/engine/session/cm/mod.rs#L909-L925), [cm/retirement.rs:17-122](../../rdma-io/src/v2/engine/session/cm/retirement.rs#L17-L122)) | Bounded destruction barrier is covered by [cm/tests.rs:922-1003](../../rdma-io/src/v2/engine/session/cm/tests.rs#L922-L1003); Phase 8 adds mixed connection/listener ownership coverage | Retain as the one authoritative queue through Phase 7; Phase 8 moves it unchanged into reactor ownership with listener lifecycle and removes its mutex |
| `IoCore.published_completion_connections` | `completion_published` atomically deduplicates a connection before queue insertion and is cleared on dequeue ([io_core/mod.rs:293-298](../../rdma-io/src/v2/engine/io_core/mod.rs#L293-L298), [io_core/mod.rs:505-520](../../rdma-io/src/v2/engine/io_core/mod.rs#L505-L520)); dispatch then rotates the ready queue ([io_core/progress.rs:280-343](../../rdma-io/src/v2/engine/io_core/progress.rs#L280-L343)) | Dispatch bounds and absence of idle scans are covered by [operation/tests.rs:607-632](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L607-L632) | Phase 6 retains a reactor-owned deduplicated ready queue and deletes the mutex |
| Per-connection copied-CQE queue and completion marker | Records completion ownership once before bounded dispatch ([io_core/mod.rs:90-142](../../rdma-io/src/v2/engine/io_core/mod.rs#L90-L142), [operation/state.rs:38-54](../../rdma-io/src/v2/engine/io_core/operation/state.rs#L38-L54)) | Duplicate/stale routing is covered by [operation/tests.rs:388-582](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L388-L582) | Phase 6 retains both as reactor-owned fields and deletes their owner locks |
| `IoCore.reclamation_requests` and I/O deadline heap | Transfers dropped/ambiguous operation deadlines into bounded due work ([io_core/mod.rs:455-470](../../rdma-io/src/v2/engine/io_core/mod.rs#L455-L470), [io_core/progress.rs:233-278](../../rdma-io/src/v2/engine/io_core/progress.rs#L233-L278)) | Reclamation fairness/bounds are covered by [io_core/progress.rs:529-644](../../rdma-io/src/v2/engine/io_core/progress.rs#L529-L644) | Phase 4 moves the heap to the reactor; Phase 6 moves the inbox and deletes its mutex |
| Session deadline inbox and heap | Transfers close/shutdown deadlines into bounded due work ([session/mod.rs:583-598](../../rdma-io/src/v2/engine/session/mod.rs#L583-L598), [session/progress.rs:348-414](../../rdma-io/src/v2/engine/session/progress.rs#L348-L414)) | Deadline rotation is covered by [session/progress.rs:529-647](../../rdma-io/src/v2/engine/session/progress.rs#L529-L647) | Phase 4 moves the heap to the reactor; Phase 7 moves the inbox and deletes its mutex |
| `CmState.software_next_class` | Rotates a ready-at-entry snapshot across five software classes and prevents requeued work from running twice in a pass ([cm/mod.rs:252-357](../../rdma-io/src/v2/engine/session/cm/mod.rs#L252-L357)) | One-pass requeue bounds are covered by [cm/tests.rs:700-780](../../rdma-io/src/v2/engine/session/cm/tests.rs#L700-L780); Phase 4 adds explicit all-class rotation coverage | Phase 4 replaces it with reactor source rotation |
| `CmState.outbound_setup_active` | Allows only one outbound setup and clears ownership on start failure or route progress ([cm/mod.rs:307-321](../../rdma-io/src/v2/engine/session/cm/mod.rs#L307-L321), [cm/outbound.rs:232-243](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L232-L243)) | Phase 2 adds a direct concurrent-connect setup-serialization test; baseline route/service stress is covered by [cm/tests.rs:140-181](../../rdma-io/src/v2/engine/session/cm/tests.rs#L140-L181) | Phase 7 moves the flag into reactor-owned outbound setup state |
| `CmState.shutting_down` | Rejects new inbound/outbound setup after CM shutdown begins ([cm/inbound.rs:20-31](../../rdma-io/src/v2/engine/session/cm/inbound.rs#L20-L31), [cm/outbound.rs:56-67](../../rdma-io/src/v2/engine/session/cm/outbound.rs#L56-L67), [cm/shutdown.rs:34-72](../../rdma-io/src/v2/engine/session/cm/shutdown.rs#L34-L72)) | Shutdown cancellation/cleanup is covered by [cm/tests.rs:641-699](../../rdma-io/src/v2/engine/session/cm/tests.rs#L641-L699); Phase 2 adds direct post-close admission rejection | Phase 8 replaces it with closed command/listener admission |
| Operation registry and `CqCreditPool` | Generational identity, global capacity, and retained debt cannot be reused early ([operation/accounting.rs:15-126](../../rdma-io/src/v2/engine/io_core/operation/accounting.rs#L15-L126)) | Registry/credit behavior is covered by [operation/tests.rs:46-98](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L46-L98) | Phase 6 moves both into `IoState`, retains semantics, and deletes synchronization |
| Connection registry and QP index | Exact generation/QP identity gates live connection proof ([session/registry.rs:12-113](../../rdma-io/src/v2/engine/session/registry.rs#L12-L113)) | Exact live/QP identity is covered by [connection/tests.rs:62-134](../../rdma-io/src/v2/engine/session/connection/tests.rs#L62-L134) | Phase 7 moves both to reactor-local storage and deletes their mutexes |
| Connection CM route registry | Raw ID, route class, slot, and generation must all agree for outbound/inbound connection routes ([cm/mod.rs:54-132](../../rdma-io/src/v2/engine/session/cm/mod.rs#L54-L132)) | Outbound/inbound route rejection is covered by [cm/tests.rs:204-331](../../rdma-io/src/v2/engine/session/cm/tests.rs#L204-L331) | Phase 7 moves connection-only route ownership into reactor connection entries and retains a non-owning connection lookup |
| Shared CM context-route index | Exact provider context routes both connection and listener events and therefore cannot be split without a parallel listener identity/dispatcher ([cm/mod.rs:88-106](../../rdma-io/src/v2/engine/session/cm/mod.rs#L88-L106)) | Connection rejection remains covered by [cm/tests.rs:204-331](../../rdma-io/src/v2/engine/session/cm/tests.rs#L204-L331); listener identity by [cm/tests.rs:334-399](../../rdma-io/src/v2/engine/session/cm/tests.rs#L334-L399) | Retain as the one authoritative index through Phase 7; Phase 8 moves it atomically with listener identity and adds mixed connection/listener dispatch tests |
| Listener token/raw-ID/context indexes | Duplicate token or raw ID cannot replace the incumbent listener ([cm/mod.rs:218-235](../../rdma-io/src/v2/engine/session/cm/mod.rs#L218-L235)) | Listener route ownership and duplicate identity are covered by [cm/tests.rs:334-399](../../rdma-io/src/v2/engine/session/cm/tests.rs#L334-L399) and [cm/tests.rs:461-475](../../rdma-io/src/v2/engine/session/cm/tests.rs#L461-L475) | Retain as the sole listener identity through Phase 7. Phase 8 atomically moves them into one bounded reactor-local registry and deletes their mutexes; no adapter identity exists |
| Operation/session quarantine maps and fallback root retention | Pins MR, debt, route, admission, and complete bundles after uncertain failure ([session/mod.rs:299-314](../../rdma-io/src/v2/engine/session/mod.rs#L299-L314), [engine/mod.rs:879-927](../../rdma-io/src/v2/engine/mod.rs#L879-L927)) | Complete-bundle retention is covered by [v2_engine_lifecycle_tests.rs:516-661](../../rdma-io-tests/tests/v2_engine_lifecycle_tests.rs#L516-L661) and [v2_engine_lifecycle_tests.rs:784-1102](../../rdma-io-tests/tests/v2_engine_lifecycle_tests.rs#L784-L1102) | Phase 6 moves operation quarantine, Phase 7 connection/setup quarantine, and Phase 8 root retention into one complete-reactor quarantine |
| `MemoizedTerminalResult` | Stores one reusable terminal result and excludes connection-local quarantine from root terminal state ([lifecycle.rs:13-68](../../rdma-io/src/v2/engine/lifecycle.rs#L13-L68), [engine/mod.rs:666-670](../../rdma-io/src/v2/engine/mod.rs#L666-L670)) | Idempotent engine terminal result is covered by [api_tests.rs:84-95](../../rdma-io/src/v2/engine/api_tests.rs#L84-L95) | Retain as the shared resource-free terminal completion |
| Planned `CommandIngress` and connect/listen/operation permits | Target mechanism, absent at baseline | Phase 2 requires connect/listen capacity, FIFO anti-barging, cancellation, close-wake, and shutdown/drop accounting tests; Phase 3 adds operation-capacity evidence | Introduce connect/listen/connection-close/shutdown in Phase 2 and operations in Phase 3; retain as final frontend/reactor plumbing |
| Planned core session command adapter | Target adapter, absent at baseline | Existing connect/listen/connection-close tests pass unchanged through it; existing accept/listener-close tests pass unchanged on the current path | Introduce for connect/listen/connection-close/shutdown in Phase 2; fold connection work in Phase 7 and listener/shutdown work in Phase 8 |
| Planned listener command/identity adapter | Prohibited before scheduling equivalence because it would create a second listener identity/lifecycle seam | Existing listener FIFO/backlog/cancellation tests remain unchanged through Phase 7; Phase 8 adds bounded admission/identity migration evidence | Do not introduce. Phase 8 moves the existing identity and lifecycle directly into reactor-owned state |
| Planned `OperationCommandAdapter` | Target adapter, absent at baseline | First-poll/no-provider, cancellation, CQE, credit, and retention tests gate Phase 3 | Introduce Phase 3; delete Phase 6 |
| Planned `ProtocolIoCommandAdapter` and `BorrowedSetupIo<'_>` | Deferred target adapters, absent at baseline | Message setup/DATA/CREDIT/retry and batch-ownership tests gate Phase 5A | Introduce Phase 5A; delete `ProtocolIoCommandAdapter` Phase 6 and retain `BorrowedSetupIo<'_>` |
| Planned `DriverTermination` | Target synchronous adapter over current `EngineShared`, absent at baseline | Phase 2 requires queued/waiting/accepted driver-drop tests | Introduce Phase 2; move into `EngineReactor::terminate_on_driver_drop` and delete helper Phase 4 |

## Authority, Evidence, and Handoff Disposition

| Type or handoff | Classification, source, and test evidence | Disposition |
|---|---|---|
| `SessionEngineRuntime` | Weak root forwarding, not evidence ([engine/mod.rs:471-526](../../rdma-io/src/v2/engine/mod.rs#L471-L526)); terminal ordering is tested at [driver/tests.rs:560-645](../../rdma-io/src/v2/engine/driver/tests.rs#L560-L645) | Delete Phase 8 |
| `IoDriverSignal` / `EngineIoDriverSignal` | The trait forwards CQ recheck, completion dispatch, and reclamation publication ([io_core/mod.rs:50-57](../../rdma-io/src/v2/engine/io_core/mod.rs#L50-L57)); the concrete adapter maps all three to the I/O `WorkSignal` bit ([engine/mod.rs:546-567](../../rdma-io/src/v2/engine/mod.rs#L546-L567)); wake coalescing is tested at [driver/tests.rs:105-241](../../rdma-io/src/v2/engine/driver/tests.rs#L105-L241) | Delete Phase 4 |
| `IoSessionBridge` | I/O-to-session behavior bridge, not evidence ([io_core/mod.rs:59-75](../../rdma-io/src/v2/engine/io_core/mod.rs#L59-L75)); exact dispatch is tested at [operation/tests.rs:388-632](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L388-L632) | Delete Phase 6 |
| `IoPostAuthority`, `SessionIoPostAuthority`, `WorkRequestPoster` | Provider forwarding authority ([connection/mod.rs:1131-1199](../../rdma-io/src/v2/engine/session/connection/mod.rs#L1131-L1199)); weak posting-only ownership is tested at [connection/tests.rs:47-61](../../rdma-io/src/v2/engine/session/connection/tests.rs#L47-L61), and destruction authority is tested at [connection/tests.rs:384-458](../../rdma-io/src/v2/engine/session/connection/tests.rs#L384-L458) | Delete Phase 9 |
| `SessionConnection` | Connection-close forwarding capability ([session/mod.rs:142-193](../../rdma-io/src/v2/engine/session/mod.rs#L142-L193)); resource-free behavior is tested at [session/mod.rs:1015-1052](../../rdma-io/src/v2/engine/session/mod.rs#L1015-L1052) | Delete Phase 7 when connection close/result ownership moves into the coherent connection lifecycle |
| `SessionListener` | Accept/listener-close forwarding capability ([session/mod.rs:194-250](../../rdma-io/src/v2/engine/session/mod.rs#L194-L250)); resource-free behavior is tested at [session/mod.rs:1015-1052](../../rdma-io/src/v2/engine/session/mod.rs#L1015-L1052) | Retain unchanged through Phase 7; delete Phase 8 when listener identity, admission, and lifecycle move together |
| `SessionLifecycleAuthority` | Transition authority, not destruction proof ([session/mod.rs:59-67](../../rdma-io/src/v2/engine/session/mod.rs#L59-L67)); proof minting is tested at [session/mod.rs:1054-1088](../../rdma-io/src/v2/engine/session/mod.rs#L1054-L1088) | Delete Phase 9 |
| `IoEffectsCommitAuthority` | Publication authority, not provider evidence ([operation/effects.rs:171-184](../../rdma-io/src/v2/engine/io_core/operation/effects.rs#L171-L184)); ordering is tested at [operation/tests.rs:1341-1685](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1341-L1685) | Delete Phase 4 |
| `QpReclaimCapability` | Reclamation forwarding bridge ([operation/reclamation.rs:15-49](../../rdma-io/src/v2/engine/io_core/operation/reclamation.rs#L15-L49)); exact proof gating is tested at [operation/tests.rs:355-386](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L355-L386) | Delete Phase 6 |
| `LiveIoProofAuthority` | Private proof constructor ([session/registry.rs:12-29](../../rdma-io/src/v2/engine/session/registry.rs#L12-L29)); exact identity minting is tested at [connection/tests.rs:62-134](../../rdma-io/src/v2/engine/session/connection/tests.rs#L62-L134) | Delete Phase 7 after exclusive registry lookup becomes the only constructor |
| `LiveIoConnectionProof` | Evidence of current generation and exact QP ([registry.rs:20-47](../../rdma-io/src/v2/engine/registry.rs#L20-L47)); tested at [connection/tests.rs:62-134](../../rdma-io/src/v2/engine/session/connection/tests.rs#L62-L134) | Retain |
| `QpDestructionProof` / `QpDestroyStatus` | Non-replayable successful destruction evidence ([session/mod.rs:76-81](../../rdma-io/src/v2/engine/session/mod.rs#L76-L81), [session/mod.rs:504-529](../../rdma-io/src/v2/engine/session/mod.rs#L504-L529)); tested at [session/mod.rs:1054-1088](../../rdma-io/src/v2/engine/session/mod.rs#L1054-L1088) | Retain |
| Acceptance ownership evidence | `BatchPostOutcome`, `PreparedBatchOwnership`, and `BatchOwnershipTransfer` prevent invented release ([operation/batch.rs:538-597](../../rdma-io/src/v2/engine/io_core/operation/batch.rs#L538-L597)); `IoSubmissionDisposition` preserves the protocol-facing result ([io.rs:261-330](../../rdma-io/src/v2/engine/io.rs#L261-L330)); prefix/ambiguity tests are at [operation/tests.rs:634-666](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L634-L666) and [operation/tests.rs:943-1305](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L943-L1305) | Retain |
| Completion evidence | `PendingCompletion` and `CqeReject` represent lookup/validation state ([operation/completion.rs:63-191](../../rdma-io/src/v2/engine/io_core/operation/completion.rs#L63-L191)); `CompletionOwnership::{Queued, Early}` records single-shot ownership under the operation record ([operation/state.rs:38-54](../../rdma-io/src/v2/engine/io_core/operation/state.rs#L38-L54)); tested at [operation/tests.rs:388-582](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L388-L582) | Retain |
| CM event evidence | `CmEventSnapshot`, `ContextRoute`, `CmEventReject`, and `EventDisposition` preserve acknowledged raw/context identity ([cm/event.rs:13-98](../../rdma-io/src/v2/engine/session/cm/event.rs#L13-L98), [cm/mod.rs:87-92](../../rdma-io/src/v2/engine/session/cm/mod.rs#L87-L92)); tested at [cm/tests.rs:204-331](../../rdma-io/src/v2/engine/session/cm/tests.rs#L204-L331) | Retain |
| Driver to owner turns | Current calls and reports are at [driver/mod.rs:158-217](../../rdma-io/src/v2/engine/driver/mod.rs#L158-L217); bounded rotation is tested at [scheduler.rs:222-247](../../rdma-io/src/v2/engine/scheduler.rs#L222-L247) | Phase 4 replaces with one `EngineReactor::turn` |
| Producer to owner work bits | Current producer publication is at [io_core/mod.rs:431-460](../../rdma-io/src/v2/engine/io_core/mod.rs#L431-L460); race tests are at [driver/tests.rs:184-241](../../rdma-io/src/v2/engine/driver/tests.rs#L184-L241) | Phase 4 replaces with one reactor source epoch |
| Session to root weak calls | Current forwarding is at [session/mod.rs:400-448](../../rdma-io/src/v2/engine/session/mod.rs#L400-L448); terminal ordering is tested at [driver/tests.rs:560-645](../../rdma-io/src/v2/engine/driver/tests.rs#L560-L645) | Phase 8 deletes with `SessionEngineRuntime` |
| `IoProgress -> IoSessionBridge -> SessionManager` completion/dispatch/reclamation | Current bridge calls are at [io_core/progress.rs:154-297](../../rdma-io/src/v2/engine/io_core/progress.rs#L154-L297); exact routing is tested at [operation/tests.rs:388-632](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L388-L632) | Phase 6 replaces with exclusive reactor field borrows and deletes `IoSessionBridge` |
| `IoCoreEffects -> SessionManager -> frontend/message observers` | Current effect commit and publication are at [session/mod.rs:683-734](../../rdma-io/src/v2/engine/session/mod.rs#L683-L734) and [operation/effects.rs:102-184](../../rdma-io/src/v2/engine/io_core/operation/effects.rs#L102-L184); ordering is tested at [operation/tests.rs:1341-1685](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1341-L1685) | Phase 4 replaces the complete chain with one consumed `ReactorActions` batch |
| Session to I/O reclamation | Current calls are at [session/mod.rs:759-853](../../rdma-io/src/v2/engine/session/mod.rs#L759-L853); proof gating is tested at [operation/tests.rs:355-386](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L355-L386) | Phase 6 replaces with exclusive reactor field borrows |
| Session to provider bundle | Current authority calls are at [session/mod.rs:504-568](../../rdma-io/src/v2/engine/session/mod.rs#L504-L568); failed-destroy behavior is tested at [connection/tests.rs:384-458](../../rdma-io/src/v2/engine/session/connection/tests.rs#L384-L458) | Phase 9 replaces with private reactor-owned bundle methods |
| Public scalar to I/O owner | Current direct entry is at [connection/mod.rs:119-168](../../rdma-io/src/v2/engine/session/connection/mod.rs#L119-L168) and [operation/future.rs:125-173](../../rdma-io/src/v2/engine/io_core/operation/future.rs#L125-L173); no-post cancellation is tested at [operation/tests.rs:703-737](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L703-L737) | Phase 3 replaces with typed commands |
| `MessageTransportDriver -> IoConnection` normal protocol post | Current direct batch calls are at [io.rs:82-103](../../rdma-io/src/v2/engine/io.rs#L82-L103) and [message_transport.rs:1347-1441](../../rdma-io/src/v2/message_transport.rs#L1347-L1441); provider behavior is covered by [v2_engine_message_tests.rs:1-260](../../rdma-io-tests/tests/v2_engine_message_tests.rs#L1-L260) | Phase 5A replaces normal posts with stable-ID engine commands; message driver remains separate |
| `ConnectionSetup -> IoConnection` pre-establish post | Setup receives the capability before connect/accept at [message_transport.rs:304-418](../../rdma-io/src/v2/message_transport.rs#L304-L418); ordering is tested at [v2_engine_message_setup_tests.rs:115-175](../../rdma-io-tests/tests/v2_engine_message_setup_tests.rs#L115-L175) | Phase 5A replaces only this handoff with `BorrowedSetupIo<'_>` so setup invokes the same backend under the reactor borrow |
| Message close `SessionConnection` handoff | Current opaque close delegation is at [io.rs:105-124](../../rdma-io/src/v2/engine/io.rs#L105-L124) and [message_transport.rs:118-128](../../rdma-io/src/v2/message_transport.rs#L118-L128); message close behavior is covered by [v2_engine_message_tests.rs:1-260](../../rdma-io-tests/tests/v2_engine_message_tests.rs#L1-L260) | Phase 5A replaces it with a stable-ID close command; message driver remains separate |

## Typed Admission and Completion Contract

### Exact capacities and staged introduction

- Connect command lane: `max_live_connections`.
- Listen command lane: `max_live_connections`.
- Scalar/protocol operation lane, introduced in Phase 3 for scalar operations
  and extended in Phase 5A for protocol batches: `max_inflight_operations`; scalar commands
  consume one permit and an `N`-WR batch atomically consumes `N`.
- Listener registry, introduced only during Phase 8 lifecycle consolidation:
  `max_live_connections`.
- Per listener, introduced together with that Phase-8 identity migration:
  `backlog` accept-request permits, `backlog` pending-child slots, and one
  selected-pair slot.
- Coalesced controls:
  `2 * max_live_connections + max_inflight_operations + 1`, checked during
  configuration. The terms are connection targets, listener targets,
  operation targets, and one engine terminal/shutdown bit.

Connect/listen lanes use fair cancellation-safe permit acquisition from Phase
2; the operation lane does so from Phase 3.
Unadmitted futures remain frontend-owned. Closing admission wakes every waiter;
each removes its registration and observes terminal state. Admission acquires
capacity and rechecks open state under the existing barrier before enqueue.

Connect commands also own `ConnectionReservation`. In Phases 2-7, listen
commands dequeue into the current authoritative `ListenRequest` and listener
identity; there is no new listener registry. Accept remains entirely on the
current listener path. In Phase 8, listen commands blocked on a full bounded
listener registry remain at the listen-lane head, accept requests own an
accept permit through selection, queued children own separate child slots, and
selection atomically consumes one of each into the single selected pair.
Shutdown and driver drop close every pool that exists at that phase, wake all
unadmitted waiters, drain queued commands, reject/retire children and selected
pairs through their one authoritative owner, release logical permits exactly
once, and quarantine uncertain provider ownership.

First polling a request may validate and admit it but performs no provider
call. Only a later `RdmaEngineDriver` poll invokes the provider. Backend local
credits, registry slots, and CQ credits are separate from ingress permits and
are acquired exactly once by the authoritative backend.

### Driver drop

Phase 2 introduces synchronous `DriverTermination` over the existing root;
Phase 4 moves it to `EngineReactor::terminate_on_driver_drop`. It is never a
queued command. It closes admission, drains unstarted requests, publishes one
shared memoized terminal result, attempts current fail-closed teardown, and
retains the complete reactor if release cannot be proven.

## Bounded Reactor and Publication Contract

One ready-at-entry turn rotates across command classes, CQ, CM,
operation deadline/reclamation, session deadlines, listener/CM software,
shutdown, and terminal cleanup. Each source receives at most one finite
quantum and newly produced work waits for a later external poll.

One leaf action is one event delivery, one operation/command result or wake,
one connection/listener close result, or the one engine-terminal broadcast.
`REACTOR_ACTION_BUDGET` is exactly 32. A transition reserves its exact leaf
count before mutation; bulk work is split into one-record units. Each bounded
record may hold at most one pending action of each applicable kind, and a
deduplicated ready set selects deferred publication. At most 32 leaf actions
are published after the mutable reactor borrow ends, ordered as connection/
protocol events, operation results/wakes, close/listener results, then engine
terminal.

CQ arm/re-poll, CM re-registration, source epoch register/recheck, deadline
rearm, and polling-mode cooperative yield remain mandatory
([completion.rs:348-451](../../rdma-io/src/v2/completion.rs#L348-L451),
[session/progress.rs:464-527](../../rdma-io/src/v2/engine/session/progress.rs#L464-L527)).

## Old-Path Deletion Gates

1. No runtime feature switch is introduced.
2. Phase 2 replaces direct public connect/listen/connection-close/shutdown
   calls. It does not change accept, listener close/drop, listener identity, or
   listener-local admission.
3. Phase 3 replaces direct public scalar SEND/RECV/WRITE/READ and ends mixed
   scalar-provider entry. Accept/listener lifecycle remains deliberately on
   its sole current path until Phase 8 rather than becoming a dual path.
4. The unchanged crate-private `IoConnection` compatibility path is deleted in
   the separate Phase 5A milestone, before exclusive ownership.
5. Phase 4 deletes `OwnerScheduler`, `AlternatingSources`, the progress-owner
   structs, owner work bits, and the old effect typestate pipeline.
6. Phase 5 must prove bounded fairness, readiness, reentrancy, terminal,
   polling/readiness-mode, and RXE/SIW equivalence before Phase 5A or lifecycle
   ownership work, including the Phase-8 listener identity/admission move.
7. Phase 6 deletes independent `IoCore`/`EstablishedIoConnection` ownership,
   operation adapters, `IoSessionBridge`, and `QpReclaimCapability`.
8. Phase 7 deletes independent connection/route owners and
   `LiveIoProofAuthority`.
9. Phase 8 moves listener identity, accept/child/selected bounded admission,
   listener close/drop, and listener lifecycle together; then deletes
   `EngineShared`, `SessionManager`, listener request capabilities,
   request-specific observers, admission barrier, and `SessionEngineRuntime`.
10. Phase 9 deletes remaining provider/lifecycle forwarding authorities.
11. Every old and adapter entry calls the same authoritative provider-safety
    functions until its assigned deletion.

## Invariant and Test Gate

| Invariant | Existing evidence retained through migration |
|---|---|
| Explicit driver/no hidden runtime | `engine::api_tests::driver_is_directly_spawnable_and_shutdown_is_idempotent`; `withholding_the_driver_prevents_cm_progress_and_cancellation_releases_admission` |
| Outer fairness/register-recheck | Scheduler and driver tests at [scheduler.rs:222-247](../../rdma-io/src/v2/engine/scheduler.rs#L222-L247) and [driver/tests.rs:105-241](../../rdma-io/src/v2/engine/driver/tests.rs#L105-L241) |
| CQ/CM readiness races | Completion tests at [completion.rs:348-451](../../rdma-io/src/v2/completion.rs#L348-L451), CM test at [session/progress.rs:464-527](../../rdma-io/src/v2/engine/session/progress.rs#L464-L527), provider race at [v2_engine_readiness_race.rs:51-165](../../rdma-io-tests/tests/v2_engine_readiness_race.rs#L51-L165) |
| Bounded deadlines/reclamation/shutdown | [io_core/progress.rs:529-644](../../rdma-io/src/v2/engine/io_core/progress.rs#L529-L644), [session/progress.rs:529-839](../../rdma-io/src/v2/engine/session/progress.rs#L529-L839) |
| Publication after mutation | [operation/tests.rs:1341-1685](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1341-L1685), [operation/tests.rs:1865-1990](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1865-L1990) |
| Exact generation/QP/opcode/duplicate routing | [operation/tests.rs:388-582](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L388-L582), [v2_engine_operation_tests.rs:38-160](../../rdma-io-tests/tests/v2_engine_operation_tests.rs#L38-L160) |
| Exact-prefix/ambiguous acceptance | [wr.rs:649-697](../../rdma-io/src/wr.rs#L649-L697), [qp.rs:487-530](../../rdma-io/src/v2/qp.rs#L487-L530), [operation/tests.rs:943-1305](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L943-L1305) |
| MR/CQ retention and cancellation races | [operation/tests.rs:668-875](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L668-L875), [operation/tests.rs:1687-1748](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1687-L1748), [operation/tests.rs:1992-2065](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1992-L2065) |
| QP proof, teardown order, quarantine | [operation/tests.rs:355-386](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L355-L386), [v2_engine_lifecycle_tests.rs:287-442](../../rdma-io-tests/tests/v2_engine_lifecycle_tests.rs#L287-L442), [v2_engine_lifecycle_tests.rs:516-1102](../../rdma-io-tests/tests/v2_engine_lifecycle_tests.rs#L516-L1102) |
| Listener FIFO/backlog/cancellation | [listener.rs:1176-1428](../../rdma-io/src/v2/engine/session/listener.rs#L1176-L1428) |
| Driver-loss fail closed | [operation/tests.rs:1750-1863](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L1750-L1863), [v2_engine_lifecycle_tests.rs:685-768](../../rdma-io-tests/tests/v2_engine_lifecycle_tests.rs#L685-L768) |

A removed test is acceptable only when its replacement states the same
invariant and records why the old test no longer applies.

## Provider Gates

All commands run serially with one build job and incremental compilation
disabled. A missing provider fails because the script sets
`RDMA_REQUIRE_PROVIDER=1`.

```sh
sudo -E env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  CARGO="$(command -v cargo)" \
  ./scripts/validate-v2-engine-providers.sh --engine-conformance
```

Provider-facing phases additionally run their exact focused modes:
`--provider-probe`, `--readiness-race`, `--driver-flush-gate`,
`--operations`, `--connections`, `--listeners`, `--lifecycle`,
`--message-setup`, or `--message` as assigned by the implementation plan.
The Phase 5 equivalence gate runs the complete no-argument matrix on both RXE
and SIW. No ownership-consolidation phase begins until that gate passes.

## Baseline Record

Phase 1 must run, in order:

```sh
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  cargo test -p rdma-io --lib v2::engine::driver::tests -- --nocapture
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  cargo test -p rdma-io --lib v2::engine::io_core::operation::tests -- --nocapture
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  cargo test -p rdma-io --lib v2::engine::session -- --nocapture
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  cargo test -p rdma-io --lib v2::engine::api_tests -- --nocapture
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  cargo test -p rdma-io --lib driver_drop -- --nocapture
git diff --check
```

Results are recorded in the Phase 1 implementation review. Phase 1 changes no
runtime or provider behavior and requires no provider gate.

Baseline results on 2026-09-08 at
`30b4a0bc5dfa07fac689a60e2b7fa4114cbb554b`:

- driver tests: 24 passed;
- operation safety tests: 33 passed;
- session/CM/listener tests: 69 passed;
- public API/shutdown tests: 8 passed;
- driver-drop filter: 6 passed; and
- `git diff --check`: passed.

## Phase 2 Evidence

Phase 2 introduces bounded connect/listen command lanes, coalesced
generational connection-close control, shutdown control admission, and typed
take-once connect/listen completion storage. The driver transfers at most one
ordinary command and one connection control into the existing authoritative
session backend per external poll. Accept, listener identity, listener close,
and waiter/child/selected ownership remain unchanged.

One provider test expectation changed without weakening its invariant:
`shutdown_waits_for_connect_admission_publication_in_both_modes` now expects
the live-connection gauge to be zero immediately after shutdown wins the
admission race. The old path retained the pre-provider reservation in
`CmState.pending` until driver cleanup; the new bounded command path drains the
unstarted command and releases that same reservation synchronously when
admission closes. The preserved invariant is stronger: the connect still
returns `DriverShutdown`, performs no provider work after shutdown wins, leaks
no reservation, and shutdown reaches the same clean terminal result.

Serialized validation on 2026-09-08:

- reactor command/completion tests: 8 passed;
- session/CM/listener tests: 71 passed;
- public API/shutdown tests: 11 passed;
- driver-drop filter: 8 passed;
- connection integration compile: passed;
- RXE/SIW provider probe: 4 tests per provider passed;
- RXE/SIW connections: 11 tests per provider passed;
- RXE/SIW listeners: 3 tests per provider passed; and
- RXE/SIW lifecycle: 11 tests per provider passed.
