//! `CoreDriver` — the per-core sole reaper of a shared, poll-only CQ pair.
//!
//! This is **Slice B of Phase 1** of the read-ring busy-poll design
//! (`docs/design/rdma-read-ring-busy-poll.md`, §4–§5). In busy-poll a single
//! send CQ + single recv CQ are **shared by every QP on the core**, and this
//! driver is the *only* code that calls `ibv_poll_cq` on them (the sole-reaper
//! rule, §5.1). It runs as one `current_thread` task per core: sweep both CQs
//! with bounded work, route each [`WorkCompletion`] by `wc.qp_num` into the
//! owning connection's [`ConnSlot`] inbox, wake that direction, then yield.
//!
//! # What this slice does and does *not* do
//!
//! Slice B proves the **data path** end-to-end for a single connection: shared
//! CQ, `qp_num` demux, inbox delivery + waker. It deliberately does **not** do
//! WR accounting or the teardown barrier — those (`dec_posted` on each reaped
//! CQE, the forced-`ERR` drain-to-zero, `ResourceBundle` reclaim) land together
//! in Slice C (§6.2), which is where they are actually used. Here the driver
//! only *delivers and wakes*.
//!
//! # Routing map ownership
//!
//! The `qp_num → Arc<ConnSlot>` map is shared between the driver task and the
//! connection-setup code via `Arc<Mutex<…>>`. On a pinned `current_thread`
//! runtime the lock is **same-core uncontended** (µs-free), and the sweep holds
//! it only for the short route step. Slice D replaces this with the
//! driver-owned reclaim-queue model once teardown needs it.

use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use crate::conn_slot::{ConnSlot, Dir, SlotState, TerminalFault};
use crate::cq::CompletionQueue;
use crate::device::Context;
use crate::wc::WorkCompletion;

/// Max completions drained from **one** CQ in a single sweep before moving to
/// the other CQ / yielding — bounds the poll-gap when one connection floods
/// completions (§5.2, §9).
const MAX_CQE_PER_CQ: usize = 64;

/// Max reclaim turns a bundle may take before the driver force-frees it (the
/// wedge escape hatch, §10). Generous: `to_error()` normally flushes every
/// outstanding WR within a handful of turns, so this only trips on a wedged
/// NIC that never delivers a flush CQE.
const RECLAIM_MAX_TURNS: usize = 4096;

type SlotMap = Arc<Mutex<HashMap<u32, Arc<ConnSlot>>>>;
type ReclaimQueue = Arc<Mutex<Vec<ReclaimEntry>>>;

/// A connection's resources awaiting teardown reclaim (§6.2 / §10).
///
/// `Drop` hands one of these to the driver instead of destroying the QP/MRs
/// synchronously (which is illegal while flush CQEs for them may still be in the
/// shared CQ). The driver drains the flush CQEs to zero, then frees `resources`
/// (dropping the concrete transport-inner in verbs order) and retires `slot`.
struct ReclaimEntry {
    slot: Arc<ConnSlot>,
    /// Type-erased owner of the RDMA/CM resources (e.g. a read-ring
    /// `ReadRingInner`). `None` once freed on `Drained`. Dropped on the driver's
    /// core, which is the resources' owner core (§7.5), so the verbs destroys
    /// run where the objects live.
    resources: Option<Box<dyn Send>>,
    /// An opaque **admission lease** (e.g. the pool's `AdmissionGuard`) held for
    /// the connection's whole lifetime. Dropped when this entry is freed — i.e.
    /// only after the QP is drained + retired/quarantined — so a pool releases its
    /// admission slot (and thus the shared-CQ budget) at **retirement**, not when
    /// the app task ends (review #2). `None` when the caller holds no lease.
    lease: Option<Box<dyn Send + Sync>>,
    /// Reclaim turns spent; bounds the wait so a wedged NIC cannot hang
    /// shutdown (§10 wedge escape hatch).
    turns: usize,
}

/// The per-core reaper. Built with [`CoreDriver::new`], which also returns a
/// [`CoreDriverHandle`] for registering connections and building QPs against the
/// shared CQs. Consumed by [`CoreDriver::run`] (spawn it on the core's runtime).
pub struct CoreDriver {
    send_cq: Arc<CompletionQueue>,
    recv_cq: Arc<CompletionQueue>,
    slots: SlotMap,
    /// Connections handed off for background teardown (§6.2 / §10).
    reclaim: ReclaimQueue,
    shutdown: Arc<AtomicBool>,
    /// Count of completions whose `qp_num` was not in the routing map. Post-
    /// retirement-barrier (Slice C) this is impossible (§4.2); a nonzero value
    /// is a fatal invariant-breach indicator, surfaced for observability (§12).
    unknown_qp: Arc<AtomicU64>,
}

/// A cloneable handle to a [`CoreDriver`]: register/deregister connections,
/// borrow the shared CQs to create QPs against, and request shutdown.
#[derive(Clone)]
pub struct CoreDriverHandle {
    send_cq: Arc<CompletionQueue>,
    recv_cq: Arc<CompletionQueue>,
    slots: SlotMap,
    reclaim: ReclaimQueue,
    shutdown: Arc<AtomicBool>,
    unknown_qp: Arc<AtomicU64>,
}

impl CoreDriver {
    /// Create a driver owning a fresh poll-only shared CQ pair (`send_depth` /
    /// `recv_depth` entries) on `ctx`, plus its handle.
    ///
    /// The CQs are created with a **null completion channel** (no fd, no
    /// interrupt on the data path) — the busy-poll sole-reaper model
    /// ([`CompletionQueue::new`]).
    pub fn new(
        ctx: Arc<Context>,
        send_depth: i32,
        recv_depth: i32,
    ) -> crate::Result<(Self, CoreDriverHandle)> {
        let send_cq = CompletionQueue::new(ctx.clone(), send_depth)?;
        let recv_cq = CompletionQueue::new(ctx, recv_depth)?;
        let slots: SlotMap = Arc::new(Mutex::new(HashMap::new()));
        let reclaim: ReclaimQueue = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let unknown_qp = Arc::new(AtomicU64::new(0));
        let handle = CoreDriverHandle {
            send_cq: send_cq.clone(),
            recv_cq: recv_cq.clone(),
            slots: slots.clone(),
            reclaim: reclaim.clone(),
            shutdown: shutdown.clone(),
            unknown_qp: unknown_qp.clone(),
        };
        let driver = CoreDriver {
            send_cq,
            recv_cq,
            slots,
            reclaim,
            shutdown,
            unknown_qp,
        };
        Ok((driver, handle))
    }

    #[inline]
    fn cq(&self, dir: Dir) -> &Arc<CompletionQueue> {
        match dir {
            Dir::Send => &self.send_cq,
            Dir::Recv => &self.recv_cq,
        }
    }

    /// Sweep one CQ once (bounded to `MAX_CQE_PER_CQ`) and route what it yields.
    /// Returns the number of completions reaped.
    fn sweep(&self, dir: Dir, batch: &mut [WorkCompletion]) -> usize {
        match self.cq(dir).poll(batch) {
            Ok(0) => 0,
            Ok(n) => {
                self.route(dir, &batch[..n]);
                n
            }
            Err(e) => {
                tracing::error!(
                    ?dir,
                    error = %e,
                    "CoreDriver: ibv_poll_cq failed; failing every slot on this core (review #4)"
                );
                self.fail_all_slots(TerminalFault::SharedCqPollFailed);
                0
            }
        }
    }

    /// Route a batch of reaped completions: demux each by `qp_num` into the
    /// owning slot's `dir` inbox, then wake each touched slot **once** for this
    /// sweep (coalesced, §7.3). An unknown `qp_num` is counted and logged as a
    /// fatal invariant breach (§4.2) rather than mis-routed.
    fn route(&self, dir: Dir, wcs: &[WorkCompletion]) {
        let map = self.slots.lock().expect("CoreDriver slot map poisoned");
        route_into(&map, &self.unknown_qp, dir, wcs);
    }

    /// Fail every registered connection with `fault` — used when a whole-core
    /// error (a shared-CQ poll failure) leaves no connection recoverable. Each
    /// faulted slot's `poll_inbox` then returns the terminal error instead of
    /// hanging on completions that will never arrive (review #4).
    fn fail_all_slots(&self, fault: TerminalFault) {
        let map = self.slots.lock().expect("CoreDriver slot map poisoned");
        for slot in map.values() {
            slot.mark_fatal(fault);
        }
    }

    /// Run the cooperative reaper loop until [`CoreDriverHandle::shutdown`].
    ///
    /// Each turn sweeps the recv CQ then the send CQ (bounded), then yields the
    /// core to the app tasks (§5.2). This is **pure spin** — the idle-CPU
    /// bounded-spin hybrid (§5.4) is deferred — so run it on a dedicated pinned
    /// core.
    pub async fn run(self) {
        let mut recv_batch = [WorkCompletion::default(); MAX_CQE_PER_CQ];
        let mut send_batch = [WorkCompletion::default(); MAX_CQE_PER_CQ];
        loop {
            self.sweep(Dir::Recv, &mut recv_batch);
            self.sweep(Dir::Send, &mut send_batch);
            // Advance teardown reclaim: drain handed-off connections' flush CQEs,
            // then free + retire the ones that reached the drain barrier (§6.2 /
            // §10).
            self.process_reclaim(RECLAIM_MAX_TURNS);
            // Shutdown join (§10): once asked to stop, keep sweeping + reclaiming
            // until every handed-off bundle is freed, so no QP/MR outlives — and
            // no flush CQE is stranded in — the shared CQs about to be dropped.
            //
            // Contract (review #3): the owner must **close every connection
            // before shutdown** — await each `JoinHandle` (aborting first if the
            // app loop must be interrupted), so each transport `Drop` has already
            // enqueued its reclaim. The reclaim push and this drain both run on
            // this one core, so a closed connection's bundle is always visible
            // here; we only need the queue to drain. The `debug_assert` catches a
            // caller that shut down while a connection was still live.
            if self.shutdown.load(Ordering::Acquire)
                && self
                    .reclaim
                    .lock()
                    .expect("CoreDriver reclaim queue poisoned")
                    .is_empty()
            {
                debug_assert!(
                    self.slots
                        .lock()
                        .expect("CoreDriver slot map poisoned")
                        .values()
                        .all(|s| s.state() == SlotState::Drained),
                    "CoreDriver shut down with a non-Drained slot still registered: close every \
                     connection (await its JoinHandle) before shutdown (§10, review #3)"
                );
                break;
            }
            // Cooperative, runtime-agnostic yield: hand the core to app tasks
            // for one turn, then resume. Avoids depending on tokio's "rt"
            // feature (this crate enables only "net").
            yield_now().await;
        }
    }

    /// Advance the teardown reclaim queue by one bounded pass (§6.2 / §10).
    ///
    /// For each handed-off (`Closing`) bundle: discard any inbox backlog (CQEs
    /// delivered before the app dropped), then test the drain barrier
    /// ([`ConnSlot::drain_complete`]). When both WR counters are zero and both
    /// inboxes empty — mark it `Drained`, free its resources (dropping the
    /// transport-inner in verbs order MW→QP→MR→PD), retire the slot, and remove
    /// its `qp_num` from the routing map so the number may be reused (§4.2, the
    /// `TIME_WAIT` analogue). If instead the per-bundle `budget` of turns is
    /// exhausted (the wedge escape hatch, a NIC that never delivered the flush
    /// CQEs), force-free the resources but **quarantine** the `qp_num` — leave the
    /// `Drained` slot registered so the number is never reused and any straggler
    /// CQE is counted down + discarded rather than mis-routed.
    fn process_reclaim(&self, budget: usize) {
        let mut scratch = [WorkCompletion::default(); MAX_CQE_PER_CQ];
        let mut queue = self
            .reclaim
            .lock()
            .expect("CoreDriver reclaim queue poisoned");
        queue.retain_mut(|entry| {
            // Discard any inbox backlog delivered before the app dropped, so the
            // barrier (which requires both inboxes empty) can complete. Fully
            // drain this turn; `route_into` discards further CQEs for a `Closing`
            // slot, so the inboxes stay empty afterwards.
            while entry.slot.drain_inbox(Dir::Send, &mut scratch) > 0 {}
            while entry.slot.drain_inbox(Dir::Recv, &mut scratch) > 0 {}
            entry.turns += 1;

            let done = entry.slot.drain_complete();
            let wedged = entry.turns >= budget;
            if !done && !wedged {
                return true; // still draining — keep in the queue
            }
            entry.slot.set_state(SlotState::Drained);
            let qp_num = entry.slot.qp_num();
            // Free MW→QP→MR→PD via the inner's field drop order.
            entry.resources = None;
            if done {
                // Normal drain: the barrier held, so the `qp_num` is safe to
                // reuse. Retire (asserting the barrier) and remove the routing
                // key so a future QP may take the number.
                entry.slot.retire();
                self.slots
                    .lock()
                    .expect("CoreDriver slot map poisoned")
                    .remove(&qp_num);
            } else {
                // Wedge escape hatch (§10): the NIC never delivered the
                // outstanding flush CQEs. Force-free without asserting the drain
                // barrier (which would panic), and **quarantine** the `qp_num` —
                // keep the `Drained` slot registered so the number can never be
                // reused and a straggler CQE for the old QP is counted down +
                // discarded here (`route_into`, §4.2) instead of mis-routed to a
                // new connection that reused the number.
                tracing::error!(
                    qp_num,
                    posted_send = entry.slot.posted(Dir::Send),
                    posted_recv = entry.slot.posted(Dir::Recv),
                    "CoreDriver: reclaim budget exhausted; force-freeing and \
                     quarantining qp_num (NIC wedge, §10/§12)"
                );
                entry.slot.abandon();
            }
            // Release the admission lease now that the connection is fully
            // reclaimed (retired or quarantined + resources freed) — this ties a
            // pool's admission release to retirement, not app completion, so the
            // shared CQ can never be over-subscribed (review #2).
            drop(entry.lease.take());
            false // done — drop the reclaim entry
        });
    }
}

impl CoreDriverHandle {
    /// The shared send CQ, to create a connection's QP against.
    #[inline]
    pub fn send_cq(&self) -> &Arc<CompletionQueue> {
        &self.send_cq
    }

    /// The shared recv CQ, to create a connection's QP against.
    #[inline]
    pub fn recv_cq(&self) -> &Arc<CompletionQueue> {
        &self.recv_cq
    }

    /// Register a connection so the driver routes its `qp_num` to `slot`.
    /// Must be called **before** the connection posts any WR (§6.1).
    ///
    /// **Rejects a `qp_num` already in the routing map** rather than replacing it:
    /// a live entry for the same number means a previous connection has not yet
    /// retired (still draining, or quarantined after a wedge, §10), and clobbering
    /// it would strand that connection's accounting and mis-route its flush CQEs
    /// to the new one. The verified retirement barrier is the only route to reuse
    /// (§4.2).
    pub fn register(&self, slot: Arc<ConnSlot>) -> crate::Result<()> {
        use std::collections::hash_map::Entry;
        let mut map = self.slots.lock().expect("CoreDriver slot map poisoned");
        match map.entry(slot.qp_num()) {
            Entry::Occupied(_) => Err(crate::Error::InvalidArg(format!(
                "qp_num {} is already registered (previous connection not yet retired / \
                 quarantined); refusing reuse before the retirement barrier (§4.2)",
                slot.qp_num()
            ))),
            Entry::Vacant(e) => {
                e.insert(slot);
                Ok(())
            }
        }
    }

    /// Ask the driver loop to exit after its current turn.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Hand a connection's resources to the driver for background teardown
    /// (§6.2 / §10). **Non-blocking**: pushes the bundle onto the reclaim queue;
    /// the driver drains the QP's flush CQEs, frees the resources (dropping
    /// `resources` in verbs order), and retires the `qp_num`.
    ///
    /// The caller (transport `Drop`) must already have marked `slot` `Closing`
    /// and forced its QP to `ERR` (`to_error()`), so the flush terminates. Only
    /// owner-agnostic ops may run before this call (§10). `lease` is an opaque
    /// admission lease dropped only when this bundle is fully reclaimed — tying a
    /// pool's admission release to retirement, not app completion (review #2).
    pub fn reclaim(
        &self,
        slot: Arc<ConnSlot>,
        resources: Box<dyn Send>,
        lease: Option<Box<dyn Send + Sync>>,
    ) {
        self.reclaim
            .lock()
            .expect("CoreDriver reclaim queue poisoned")
            .push(ReclaimEntry {
                slot,
                resources: Some(resources),
                lease,
                turns: 0,
            });
    }

    /// Synchronously drop a slot from the routing map **without** the reclaim
    /// drain barrier. Used only by setup rollback before a connection has
    /// exchanged any data, when the resources are torn down synchronously by the
    /// caller rather than handed to the reclaim queue (e.g. a `connect().await`
    /// cancelled before the CM id is owned by the transferable bundle, so the QP
    /// cannot be deferred to the driver — see the read-ring setup transaction).
    ///
    /// The caller owns the QP/MRs and destroys them itself; any late flush CQE
    /// for this now-unregistered `qp_num` is routed to the unknown-qp counter
    /// (benign). Unlike [`reclaim`](Self::reclaim), the `qp_num` becomes reusable
    /// immediately — there is no quarantine, because setup rollback happens
    /// before the connection could have wedged the QP.
    pub fn unregister(&self, qp_num: u32) {
        self.slots
            .lock()
            .expect("CoreDriver slot map poisoned")
            .remove(&qp_num);
    }

    /// Count of completions routed to an unknown `qp_num` (§4.2). Zero in a
    /// correct run; nonzero signals a retirement-barrier breach.
    pub fn unknown_qp_count(&self) -> u64 {
        self.unknown_qp.load(Ordering::Relaxed)
    }
}

/// Demux one batch of completions into the routing `map` for direction `dir`,
/// waking each touched slot once (coalesced). Free function so the reaper and
/// the unit tests exercise the *same* routing logic.
fn route_into(
    map: &HashMap<u32, Arc<ConnSlot>>,
    unknown_qp: &AtomicU64,
    dir: Dir,
    wcs: &[WorkCompletion],
) {
    let mut woken: Vec<u32> = Vec::new();
    for wc in wcs {
        let qp_num = wc.qp_num();
        match map.get(&qp_num) {
            Some(slot) => {
                // Every reaped CQE decrements the outstanding-WR count (§6.2);
                // this drives the teardown barrier to zero. Underflow is a fatal
                // accounting bug (dec without a matching post).
                slot.dec_posted(dir);
                // Teardown: once the app has dropped and the slot is `Closing`,
                // discard the CQE (the ring framing no longer matters) but still
                // count it down so the drain barrier can reach zero. Delivering
                // would refill the inbox that reclaim just drained (§10).
                if matches!(slot.state(), SlotState::Closing | SlotState::Drained) {
                    continue;
                }
                match slot.deliver(dir, *wc) {
                    Ok(()) => {
                        if !woken.contains(&qp_num) {
                            woken.push(qp_num);
                        }
                    }
                    Err(_rejected) => {
                        // Inbox overflow: a per-connection fatal fault (§7.2).
                        // Inboxes are sized overflow-free, so this should be
                        // unreachable; if seen, isolate this connection.
                        tracing::error!(qp_num, ?dir, "CoreDriver: inbox overflow (fatal)");
                        slot.mark_fatal(TerminalFault::InboxOverflow);
                    }
                }
            }
            None => {
                unknown_qp.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    qp_num,
                    ?dir,
                    "CoreDriver: completion for unknown qp_num \
                     (retirement-barrier breach, §4.2)"
                );
            }
        }
    }
    for qp_num in woken {
        if let Some(slot) = map.get(&qp_num) {
            slot.wake(dir);
        }
    }
}

/// A cooperative, runtime-agnostic yield: returns `Pending` once (re-waking
/// immediately) so the scheduler can run other ready tasks, then completes.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn_slot::WorkerToken;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context as TaskContext, RawWaker, RawWakerVTable, Waker};

    fn wc_for(qp_num: u32, wr_id: u64) -> WorkCompletion {
        let mut wc = WorkCompletion::default();
        wc.inner.qp_num = qp_num;
        wc.inner.wr_id = wr_id;
        wc
    }

    // A no-op waker for driving poll_inbox in assertions.
    struct CountingWaker(AtomicUsize);
    impl CountingWaker {
        fn new() -> Arc<Self> {
            Arc::new(Self(AtomicUsize::new(0)))
        }
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
        fn waker(self: &Arc<Self>) -> Waker {
            fn clone(p: *const ()) -> RawWaker {
                let arc = unsafe { Arc::from_raw(p as *const CountingWaker) };
                let cloned = arc.clone();
                std::mem::forget(arc);
                RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
            }
            fn wake(p: *const ()) {
                let arc = unsafe { Arc::from_raw(p as *const CountingWaker) };
                arc.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(p: *const ()) {
                let arc = unsafe { Arc::from_raw(p as *const CountingWaker) };
                arc.0.fetch_add(1, Ordering::SeqCst);
                std::mem::forget(arc);
            }
            fn drop_fn(p: *const ()) {
                unsafe { drop(Arc::from_raw(p as *const CountingWaker)) };
            }
            static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_fn);
            let raw = RawWaker::new(Arc::into_raw(self.clone()) as *const (), &VTABLE);
            unsafe { Waker::from_raw(raw) }
        }
    }

    // Build a driver without RDMA: we only exercise `route`, which never touches
    // a CQ. Construct the struct directly with empty CQ handles is impossible
    // (CQs need a device), so `route` is tested via a standalone helper mirror.
    fn slot(qp_num: u32) -> Arc<ConnSlot> {
        Arc::new(ConnSlot::new(qp_num, 8, 8, WorkerToken::current()))
    }

    #[test]
    fn demux_routes_by_qp_num() {
        let a = slot(10);
        let b = slot(20);
        let mut map = HashMap::new();
        map.insert(10u32, a.clone());
        map.insert(20u32, b.clone());
        let unknown = AtomicU64::new(0);

        // Interleaved recv completions for two connections.
        let batch = [wc_for(10, 1), wc_for(20, 2), wc_for(10, 3)];
        // Balance the driver's dec_posted: pre-count the posts these CQEs reap.
        a.inc_posted(Dir::Recv);
        a.inc_posted(Dir::Recv);
        b.inc_posted(Dir::Recv);
        route_into(&map, &unknown, Dir::Recv, &batch);
        assert_eq!(unknown.load(Ordering::Relaxed), 0);

        // Each slot got exactly its own completions, in order.
        let cw = CountingWaker::new();
        let waker = cw.waker();
        let mut cx = TaskContext::from_waker(&waker);
        let mut out = [WorkCompletion::default(); 8];
        match a.poll_inbox(Dir::Recv, &mut cx, &mut out) {
            Poll::Ready(Ok(2)) => {
                assert_eq!(out[0].wr_id(), 1);
                assert_eq!(out[1].wr_id(), 3);
            }
            other => panic!("slot a: expected Ready(Ok(2)), got {other:?}"),
        }
        match b.poll_inbox(Dir::Recv, &mut cx, &mut out) {
            Poll::Ready(Ok(1)) => assert_eq!(out[0].wr_id(), 2),
            other => panic!("slot b: expected Ready(Ok(1)), got {other:?}"),
        }
    }

    #[test]
    fn coalesced_wake_once_per_sweep() {
        let a = slot(10);
        let mut map = HashMap::new();
        map.insert(10u32, a.clone());
        let unknown = AtomicU64::new(0);

        // Park a waiter on the recv direction.
        let cw = CountingWaker::new();
        let waker = cw.waker();
        let mut cx = TaskContext::from_waker(&waker);
        let mut out = [WorkCompletion::default(); 8];
        assert!(matches!(
            a.poll_inbox(Dir::Recv, &mut cx, &mut out),
            Poll::Pending
        ));

        // Three completions for the same slot in one sweep → exactly one wake.
        let batch = [wc_for(10, 1), wc_for(10, 2), wc_for(10, 3)];
        a.inc_posted(Dir::Recv);
        a.inc_posted(Dir::Recv);
        a.inc_posted(Dir::Recv);
        route_into(&map, &unknown, Dir::Recv, &batch);
        assert_eq!(cw.count(), 1);
    }

    #[test]
    fn unknown_qp_is_counted_not_routed() {
        let a = slot(10);
        let mut map = HashMap::new();
        map.insert(10u32, a.clone());
        let unknown = AtomicU64::new(0);

        let batch = [wc_for(999, 1), wc_for(10, 2)];
        // Only the known slot's CQE is accounted (the unknown one is dropped).
        a.inc_posted(Dir::Send);
        route_into(&map, &unknown, Dir::Send, &batch);
        assert_eq!(unknown.load(Ordering::Relaxed), 1);

        // The known slot still received its own completion.
        let cw = CountingWaker::new();
        let waker = cw.waker();
        let mut cx = TaskContext::from_waker(&waker);
        let mut out = [WorkCompletion::default(); 8];
        match a.poll_inbox(Dir::Send, &mut cx, &mut out) {
            Poll::Ready(Ok(1)) => assert_eq!(out[0].wr_id(), 2),
            other => panic!("expected Ready(Ok(1)), got {other:?}"),
        }
    }

    #[test]
    fn send_and_recv_directions_are_independent() {
        let a = slot(10);
        let mut map = HashMap::new();
        map.insert(10u32, a.clone());
        let unknown = AtomicU64::new(0);

        a.inc_posted(Dir::Send);
        a.inc_posted(Dir::Recv);
        route_into(&map, &unknown, Dir::Send, &[wc_for(10, 100)]);
        route_into(&map, &unknown, Dir::Recv, &[wc_for(10, 200)]);

        let cw = CountingWaker::new();
        let waker = cw.waker();
        let mut cx = TaskContext::from_waker(&waker);
        let mut out = [WorkCompletion::default(); 8];
        match a.poll_inbox(Dir::Send, &mut cx, &mut out) {
            Poll::Ready(Ok(1)) => assert_eq!(out[0].wr_id(), 100),
            other => panic!("send: expected Ready(Ok(1)), got {other:?}"),
        }
        match a.poll_inbox(Dir::Recv, &mut cx, &mut out) {
            Poll::Ready(Ok(1)) => assert_eq!(out[0].wr_id(), 200),
            other => panic!("recv: expected Ready(Ok(1)), got {other:?}"),
        }
    }

    // A `Closing` slot's flush CQEs are counted down (so the drain barrier can
    // reach zero) but NOT delivered to the inbox — otherwise reclaim's inbox
    // drain would be refilled and the barrier would never complete (§10).
    #[test]
    fn closing_slot_discards_but_decrements() {
        let a = slot(10);
        let mut map = HashMap::new();
        map.insert(10u32, a.clone());
        let unknown = AtomicU64::new(0);

        // Two posts outstanding on each direction, then teardown begins.
        a.inc_posted(Dir::Send);
        a.inc_posted(Dir::Send);
        a.inc_posted(Dir::Recv);
        a.inc_posted(Dir::Recv);
        a.set_state(SlotState::Closing);
        assert!(!a.drain_complete());

        // The forced-ERR flush CQEs arrive on the shared CQ.
        route_into(&map, &unknown, Dir::Send, &[wc_for(10, 1), wc_for(10, 2)]);
        route_into(&map, &unknown, Dir::Recv, &[wc_for(10, 3), wc_for(10, 4)]);

        // Every CQE was accounted (nothing mis-routed) ...
        assert_eq!(unknown.load(Ordering::Relaxed), 0);
        assert_eq!(a.posted(Dir::Send), 0);
        assert_eq!(a.posted(Dir::Recv), 0);
        // ... but none were delivered: both inboxes stay empty, so the barrier
        // is now satisfied.
        let cw = CountingWaker::new();
        let waker = cw.waker();
        let mut cx = TaskContext::from_waker(&waker);
        let mut out = [WorkCompletion::default(); 8];
        assert!(matches!(
            a.poll_inbox(Dir::Send, &mut cx, &mut out),
            Poll::Pending
        ));
        assert!(matches!(
            a.poll_inbox(Dir::Recv, &mut cx, &mut out),
            Poll::Pending
        ));
        assert!(a.drain_complete());
    }
}
