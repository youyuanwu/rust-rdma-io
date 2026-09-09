# V2 Single-Owner Reactor Migration

Status: completed implementation record for
[issue #59](https://github.com/youyuanwu/rust-rdma-io/issues/59).

The migration replaced the former shared I/O/session owner graph with one
caller-polled, `RdmaEngineDriver`-owned `EngineReactor`. The work was performed
as local, evidence-gated phases on `feature/v2-single-owner-reactor`. V1
public behavior was preserved; shared low-level files received only
ownership-safe support used by v2 and test instrumentation.

The final architecture is documented in
[V2 RDMA Engine and Message Driver](v2-rdma-engine.md). This document records
what moved or was removed, why retained mechanisms still exist, and which
tests established behavioral and provider equivalence.

## Final Contract

- `RdmaEngineDriver` is the only engine progress future. Its poll path invokes
  the sole production `EngineReactor::turn`
  ([driver/mod.rs:143-236](../../rdma-io/src/v2/engine/driver/mod.rs#L143-L236),
  [reactor/mod.rs:288-674](../../rdma-io/src/v2/engine/reactor/mod.rs#L288-L674)).
- All mutable operation, connection, listener, CM, shutdown, deadline,
  quarantine, and provider-resource state is reachable through the
  driver-owned reactor
  ([reactor/mod.rs:35-82](../../rdma-io/src/v2/engine/reactor/mod.rs#L35-L82)).
- Frontends use stable generational identities, bounded typed command
  admission, and resource-free completion observers. They do not expose a
  separately pollable or independently mutable subsystem owner.
- Provider submission and completion paths retain exact acceptance
  reconciliation, early-CQE ownership, CQ-credit and MR retention, and exact
  CQE checks
  ([operation/future.rs:497-655](../../rdma-io/src/v2/engine/io_core/operation/future.rs#L497-L655),
  [operation/batch.rs:135-478](../../rdma-io/src/v2/engine/io_core/operation/batch.rs#L135-L478),
  [operation/completion.rs:120-217](../../rdma-io/src/v2/engine/io_core/operation/completion.rs#L120-L217)).
- Teardown retains QP-before-CmId ordering, event-drained CM destruction,
  complete-bundle quarantine, and positive destruction evidence
  ([session/drain.rs:270-367](../../rdma-io/src/v2/engine/session/drain.rs#L270-L367),
  [session/cm/retirement.rs:440-559](../../rdma-io/src/v2/engine/session/cm/retirement.rs#L440-L559)).
- Message protocol/scheduler integration remains a separate future milestone.
  The message driver uses the reactor command boundary but remains explicitly
  caller-polled.

## Completed State and Type Dispositions

| Baseline state or mechanism | Final disposition | Runtime necessity or replacement evidence |
|---|---|---|
| `EngineShared` mutable composition root | Removed. `EngineFrontendRoot` contains frontend coordination only; mutable runtime state is in `EngineReactor`. | The reactor fields directly own I/O, session, lifecycle, scheduler, and provider resources ([reactor/mod.rs:35-82](../../rdma-io/src/v2/engine/reactor/mod.rs#L35-L82)). |
| `Arc<IoCore>` / `Arc<SessionManager>` mutable owners | Removed. `IoState` is value-owned under `IoReactorSources`; `SessionManager` is a reactor-owned policy/frontend-binding value without runtime registries. | `IoState` and `SessionReactorSources` are reached through exclusive reactor access ([io_core/mod.rs:148-180](../../rdma-io/src/v2/engine/io_core/mod.rs#L148-L180), [session/progress.rs:22-41](../../rdma-io/src/v2/engine/session/progress.rs#L22-L41), [session/mod.rs:355-449](../../rdma-io/src/v2/engine/session/mod.rs#L355-L449)). |
| `IoProgress`, `SessionProgress`, owner scheduler, owner-only progress reports/turns | Removed. Source state remains as `IoReactorSources` and `SessionReactorSources`, scheduled only by `EngineReactor::turn`. | Ready-at-entry fair rotation is implemented once by `ReactorScheduler` ([reactor/scheduler.rs:1-92](../../rdma-io/src/v2/engine/reactor/scheduler.rs#L1-L92)). |
| Direct connect/listen/scalar/protocol provider entry paths | Removed. Frontends admit typed commands; the driver executes provider work on a later poll. | Command admission and service are bounded and centralized ([reactor/command.rs:261-524](../../rdma-io/src/v2/engine/reactor/command.rs#L261-L524), [reactor/command.rs:746-1024](../../rdma-io/src/v2/engine/reactor/command.rs#L746-L1024)). |
| Parallel listener identity and accept ownership | Removed. One generational `ListenerRegistry` owns identity, backlog permits, pending children, selected pair, close, and CM ownership. | Registry and listener state are co-located ([session/listener.rs:1030-1445](../../rdma-io/src/v2/engine/session/listener.rs#L1030-L1445)). |
| Separate connection, route, QP, and quarantine owners | Removed. One generational `ConnectionRegistry` entry owns the route direction, connection resource bundle, I/O ledger, lifecycle, retirement, and quarantine facts. | Registration and transitions occur through the single registry ([session/registry.rs:432-715](../../rdma-io/src/v2/engine/session/registry.rs#L432-L715)). |
| Split root/session CM context and destruction ownership | Removed. One `CmState` has the shared context route and common CM-destruction service for both listeners and connections. | The common service retains the `WouldBlock` barrier ([session/cm/mod.rs:662-691](../../rdma-io/src/v2/engine/session/cm/mod.rs#L662-L691), [session/cm/retirement.rs:16-93](../../rdma-io/src/v2/engine/session/cm/retirement.rs#L16-L93)). |
| `IoSessionBridge`, `SessionEngineRuntime`, and effect/lifecycle/post forwarding authorities | Removed. Direct borrow-scoped methods operate on reactor-owned state and resource bundles. | Source code contains no production bridge or authority trait; provider access stays private on `ConnectionPoster`/`VerbsConnectionResources` ([session/connection/mod.rs:1140-1305](../../rdma-io/src/v2/engine/session/connection/mod.rs#L1140-L1305)). |
| `WorkRequestPoster` production/test compatibility abstraction | Removed. Production uses the concrete connection resource enum; a cfg-gated test provider seam injects deterministic provider outcomes only. | The test seam cannot compile into a normal build and owns no production path ([session/connection/mod.rs:1100-1165](../../rdma-io/src/v2/engine/session/connection/mod.rs#L1100-L1165)). |
| `IoCore` compatibility alias and owner-only `ProgressReport` | Removed. Final code names `IoState` and the one reactor turn directly. | No alias or alternate turn/report path remains. |
| Split resource bundles and independently retained provider roots | Removed. One `EngineReactorResources` bundle owns readiness adapters, CQ, PD, CM channel, and context in canonical drop order. | Resource fields and drop tests make the retained order explicit ([resources.rs:18-37](../../rdma-io/src/v2/engine/resources.rs#L18-L37), [resources/drop_tests.rs:79-82](../../rdma-io/src/v2/engine/resources/drop_tests.rs#L79-L82)). |
| Aggregate or migration-only queue/scheduler compatibility | Removed. Only final bounded ingress/control, ready-source, completion, deadline, CM-destruction, and quarantine storage remains. | Each retained collection has a capacity, per-turn budget, deduplication role, provider ownership role, or stable deadline ordering role. |

## Retained Runtime Mechanisms

The following mechanisms are intentionally not migration scaffolding:

| Mechanism | Why it remains |
|---|---|
| `CommandIngress` ordinary lanes | Bound frontend-owned commands and retained MR/WR payloads before reactor execution. Connect and listen are separate so listener-slot pressure cannot block connect admission. |
| Coalesced control sets | Close, cancellation, and shutdown must bypass ordinary lane saturation while remaining bounded by generational live-target capacities. |
| `BorrowedSetupIo<'_>` | Connect/accept setup must post initial protocol receives before the provider handshake without exposing a shareable I/O owner. The lifetime prevents escape from exclusive setup access ([io.rs:425-477](../../rdma-io/src/v2/engine/io.rs#L425-L477)). |
| `ReactorScheduler` ready snapshot | One quantum per ready-at-entry source plus rotating start prevents hot CQ, CM, listener, command, or shutdown work from starving another source. |
| `ReactorActions` | A fixed 32-leaf batch guarantees state-before-publication ordering and bounds wake/event work per ordinary turn; it has no overflow queue ([reactor/action.rs:11-151](../../rdma-io/src/v2/engine/reactor/action.rs#L11-L151)). |
| CQ/CM readiness fields and `WorkSignal` | Provider fds and software producers require arm/register/recheck state to prevent lost wakeups. |
| I/O and session deadline queues | Operation missing-CQE deadlines and connection drain deadlines have different owners and budgets but both require stable equal-time ordering. |
| Separate I/O and session reclamation budgets | Operation reclamation cannot consume all connection-lifecycle work, and connection draining cannot consume all operation-reclamation work ([engine/mod.rs:173-192](../../rdma-io/src/v2/engine/mod.rs#L173-L192)). |
| Admission `RwLock` | A waiting frontend must acquire capacity and recheck engine/listener open state atomically against shutdown or close. It protects that cross-thread transaction, not reactor backend state. |
| Resource-free observer mutexes/wakers | Take-once results and register/recheck notification cross the driver/frontend task boundary; they contain no provider or backend owner. |
| `LiveIoConnectionProof` | Records that the exact connection generation and `qp_num` were live when the copied CQE entered routing ([registry.rs:20-39](../../rdma-io/src/v2/engine/registry.rs#L20-L39)). |
| `QpDestructionProof` | Records a successful synchronous destruction of the exact owning QP and is required before missing-CQE ownership can be reclaimed ([session/mod.rs:50-58](../../rdma-io/src/v2/engine/session/mod.rs#L50-L58)). |
| Common CM-destruction FIFO | Preserves event-drained `CmId` destruction for both connection and listener owners without duplicating provider-safety logic. |
| Complete-reactor quarantine | Driver drop has no later poll; unresolved provider ownership must outlive frontend teardown rather than be guessed safe. |

## Behavioral Equivalence Evidence

### Scheduling and publication

- Every ready source receives at most one quantum and the starting source
  rotates
  ([reactor/scheduler.rs:97-132](../../rdma-io/src/v2/engine/reactor/scheduler.rs#L97-L132)).
- Work made ready during a turn is deferred to the next ready snapshot
  ([reactor/scheduler.rs:133-157](../../rdma-io/src/v2/engine/reactor/scheduler.rs#L133-L157)).
- `ReactorActions` rejects a thirty-third ordinary leaf and publishes consumed
  batches in the required order
  ([reactor/action.rs:184-242](../../rdma-io/src/v2/engine/reactor/action.rs#L184-L242)).
- Driver wake/register races, deadline rearming, terminal composition, and
  source interleaving remain covered by engine driver tests
  ([driver/tests.rs:350-735](../../rdma-io/src/v2/engine/driver/tests.rs#L350-L735)).

### Provider acceptance and completion

- Exact-prefix and ambiguous batch outcomes preserve whole ownership until
  positive evidence permits release
  ([operation/tests.rs:315-392](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L315-L392)).
- Wrong QP, wrong opcode, unknown/stale generation, and duplicate CQEs cannot
  release ownership
  ([operation/tests.rs:393-494](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L393-L494)).
- QP-destruction reclamation requires the exact connection and QP
  ([operation/tests.rs:495-535](../../rdma-io/src/v2/engine/io_core/operation/tests.rs#L495-L535)).
- The eight-connection provider test injects invalid and duplicate CQEs through
  the explicitly driven path and checks exact rejection classes
  ([v2_engine_tests.rs:350-419](../../rdma-io-tests/tests/v2_engine_tests.rs#L350-L419)).

### Listener, close, and teardown

- Listener capacity, non-wrapping generation, stale identity rejection, FIFO
  accept admission, cancellation, ownership transfer, and exact provider
  backlog are covered directly
  ([listener.rs:2256-2529](../../rdma-io/src/v2/engine/session/listener.rs#L2256-L2529)).
- Connection drain tests retain ambiguous operations, preserve bounded close,
  and keep registered ownership when QP destruction fails
  ([drain.rs:560-1065](../../rdma-io/src/v2/engine/session/drain.rs#L560-L1065)).
- Actual wrapper destruction order is checked in readiness and polling modes
  ([resources/drop_tests.rs:79-82](../../rdma-io/src/v2/engine/resources/drop_tests.rs#L79-L82)).

## Phase Gate Record

All commands used `CARGO_BUILD_JOBS=1` and `CARGO_INCREMENTAL=0` and ran
serially.

| Phase | Gate evidence |
|---|---|
| Baseline and command boundary | Existing v2 behavior plus bounded connect/listen/control admission unit tests. |
| Scalar and unified reactor turn | Scalar admission/cancellation tests, scheduler/action tests, readiness and publication race tests. |
| Scheduling equivalence | Focused RXE and SIW provider probe, readiness race, driver flush, operation, connection, and lifecycle gates. |
| Message engine boundary | Focused RXE and SIW message setup/behavior/retry gates while leaving the message protocol scheduler separate. |
| I/O ownership | Full v2 engine unit suite and focused provider operation/conformance evidence. |
| Connection ownership | Focused RXE/SIW connection and lifecycle validation. |
| Listener/shutdown ownership | Focused RXE/SIW listener and lifecycle validation. |
| Authority cleanup | No-default and Tokio checks; formatting; strict all-target/all-feature Clippy; 185 focused engine unit tests; focused RXE/SIW engine conformance. RXE was restored and SIW removed after the gate. |
| Final validation | Constrained repository validation, final focused provider evidence, and patch hygiene are recorded in the local PAW `Docs.md`. |

## Old-Path Removal Criteria

- [x] One production engine progress path remains:
  `RdmaEngineDriver::poll` → `EngineReactor::turn`.
- [x] No frontend performs a provider call on its admission poll.
- [x] No `Arc<IoCore>`, `Arc<SessionManager>`, parallel connection registry,
  parallel listener registry, or alternate CM dispatcher owns mutable backend
  state.
- [x] No owner scheduler, owner-only turn, compatibility progress report,
  authority token, bridge trait, or forwarding provider trait remains in the
  production path.
- [x] Scalar and protocol submission share validation and one concrete
  connection provider resource path; no old/new provider-safety
  implementation is duplicated.
- [x] Action production is bounded and publication occurs only after reactor
  mutation for that turn.
- [x] Admission, cancellation, result delivery, and receiver-loss ownership
  have one defined disposition before and after command admission.
- [x] Exact CQE checks, provider acceptance reconciliation, MR retention, local
  QP credits, CQ credits, and early-CQE retention remain enforced.
- [x] `LiveIoConnectionProof` and `QpDestructionProof` remain because they
  encode runtime facts.
- [x] QP-before-CmId teardown, `WouldBlock` event draining, complete-bundle
  quarantine, listener behavior, and driver-drop fail-closed handling remain.
- [x] Every retained lock, queue, budget, wrapper, and proof has a documented
  cross-thread, boundedness, scheduling, ordering, or provider-evidence role.
- [x] V1 public API and behavior are preserved; shared low-level support
  changes remain covered by the v1 safe-resource provider suite.
- [x] Message protocol/scheduler integration remains explicitly deferred.
