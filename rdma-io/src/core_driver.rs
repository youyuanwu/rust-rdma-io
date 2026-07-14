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

use crate::conn_slot::{ConnSlot, Dir};
use crate::cq::CompletionQueue;
use crate::device::Context;
use crate::wc::WorkCompletion;

/// Max completions drained from **one** CQ in a single sweep before moving to
/// the other CQ / yielding — bounds the poll-gap when one connection floods
/// completions (§5.2, §9).
const MAX_CQE_PER_CQ: usize = 64;

type SlotMap = Arc<Mutex<HashMap<u32, Arc<ConnSlot>>>>;

/// The per-core reaper. Built with [`CoreDriver::new`], which also returns a
/// [`CoreDriverHandle`] for registering connections and building QPs against the
/// shared CQs. Consumed by [`CoreDriver::run`] (spawn it on the core's runtime).
pub struct CoreDriver {
    send_cq: Arc<CompletionQueue>,
    recv_cq: Arc<CompletionQueue>,
    slots: SlotMap,
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
        let shutdown = Arc::new(AtomicBool::new(false));
        let unknown_qp = Arc::new(AtomicU64::new(0));
        let handle = CoreDriverHandle {
            send_cq: send_cq.clone(),
            recv_cq: recv_cq.clone(),
            slots: slots.clone(),
            shutdown: shutdown.clone(),
            unknown_qp: unknown_qp.clone(),
        };
        let driver = CoreDriver {
            send_cq,
            recv_cq,
            slots,
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
                tracing::error!(?dir, error = %e, "CoreDriver: ibv_poll_cq failed");
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

    /// Run the cooperative reaper loop until [`CoreDriverHandle::shutdown`].
    ///
    /// Each turn sweeps the recv CQ then the send CQ (bounded), then yields the
    /// core to the app tasks (§5.2). This is **pure spin** — the idle-CPU
    /// bounded-spin hybrid (§5.4) is deferred — so run it on a dedicated pinned
    /// core.
    pub async fn run(self) {
        let mut recv_batch = [WorkCompletion::default(); MAX_CQE_PER_CQ];
        let mut send_batch = [WorkCompletion::default(); MAX_CQE_PER_CQ];
        while !self.shutdown.load(Ordering::Acquire) {
            self.sweep(Dir::Recv, &mut recv_batch);
            self.sweep(Dir::Send, &mut send_batch);
            // Cooperative, runtime-agnostic yield: hand the core to app tasks
            // for one turn, then resume. Avoids depending on tokio's "rt"
            // feature (this crate enables only "net").
            yield_now().await;
        }
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
    pub fn register(&self, slot: Arc<ConnSlot>) {
        let mut map = self.slots.lock().expect("CoreDriver slot map poisoned");
        map.insert(slot.qp_num(), slot);
    }

    /// Deregister a connection's `qp_num` from the routing map.
    ///
    /// Slice B calls this at connection drop; the verified-retirement ordering
    /// (drain to zero *before* removal) is Slice C (§6.2).
    pub fn deregister(&self, qp_num: u32) {
        let mut map = self.slots.lock().expect("CoreDriver slot map poisoned");
        map.remove(&qp_num);
    }

    /// Ask the driver loop to exit after its current turn.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
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
            Some(slot) => match slot.deliver(dir, *wc) {
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
                    slot.mark_fatal();
                }
            },
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
}
