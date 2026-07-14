//! `ConnSlot` — the per-connection handoff between the busy-poll `CoreDriver`
//! and a transport task.
//!
//! This is **Slice A of Phase 1** of the read-ring busy-poll design
//! (`docs/design/rdma-read-ring-busy-poll.md`, §5–§7). It is pure library code
//! with no RDMA dependency: the `CoreDriver` (Slice B) is the *producer* that
//! reaps the shared CQ and pushes each [`WorkCompletion`] into the owning
//! connection's per-direction **inbox**; the transport task is the *consumer*
//! that drains its inbox through a
//! [`crate::completion_source::CompletionSource::Driver`] source.
//!
//! # Why an `Arc<ConnSlot>` and not `Rc<RefCell<…>>`
//!
//! The driver task and the transport/app task run on the **same** core (a
//! pinned `current_thread` runtime), so there is no cross-core contention — but
//! they are *distinct tasks* sharing this state. Expressing that with
//! `Arc<ConnSlot>` + atomics (rather than `Rc<RefCell>`) keeps the transport
//! type `Send + Sync`, which the arm-park path and tonic/hyper require (§7.1).
//! The atomics here are therefore cheap **same-core uncontended** operations,
//! and the [`AtomicWaker`] is required because the slot is `Sync`, not because
//! of a real data race.
//!
//! # Invariants (see the design doc)
//!
//! - **Single producer, single consumer per direction.** The driver is the
//!   only pusher; the transport's read half / write half is the only drainer of
//!   the recv / send inbox respectively (§7.3).
//! - **Inboxes never overflow.** Each is sized to its QP's maximum
//!   simultaneously-completable WRs (§7.2), so [`ConnSlot::deliver`] returning
//!   `Err` is a *fatal, per-connection* fault — the caller retains the rejected
//!   completion and marks the connection fatal.
//! - **WR accounting never underflows.** [`ConnSlot::dec_posted`] panics on
//!   underflow rather than clamping — underflow is an invariant violation (§6.2).
//! - **Owner-worker affinity.** Every access asserts it runs on the worker that
//!   built the slot (§7.5); moving a busy-mode connection off its core is a bug.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::thread::ThreadId;

use futures_util::task::AtomicWaker;

use crate::Result;
use crate::wc::WorkCompletion;

/// Which direction (and therefore which inbox / waker / WR counter) a
/// completion or a wait belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// The send CQ side: Write+Imm, one-sided Read, token sends, MW binds.
    Send,
    /// The recv CQ side: doorbell recvs and their reposts.
    Recv,
}

/// The connection's lifecycle state, as seen by the driver and the teardown
/// barrier (§6.1–§6.2).
///
/// The full transition logic (forced `ERR`, drain-to-zero, retirement) lands in
/// Slice C; Slice A defines the states and provides guarded storage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SlotState {
    /// Setup in progress: QP created, setup WRs may be posted, not yet handed
    /// to the app.
    Connecting = 0,
    /// Steady state: the transport is serving the app.
    Established = 1,
    /// Teardown started: no new WRs are posted; the QP is being forced to `ERR`
    /// and the driver is draining its flush CQEs.
    Closing = 2,
    /// Both WR counters are zero and both inboxes empty; safe to destroy the
    /// QP/MRs and retire the `qp_num`.
    Drained = 3,
}

impl SlotState {
    #[inline]
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Connecting,
            1 => Self::Established,
            2 => Self::Closing,
            3 => Self::Drained,
            other => unreachable!("invalid SlotState byte {other}"),
        }
    }
}

/// An opaque token identifying the worker (core) that owns a [`ConnSlot`].
///
/// Backed by the owning thread's [`ThreadId`]. The `current_thread` runtime
/// only guarantees affinity while the slot stays inside its runtime, so every
/// access asserts the caller is on the owner (§7.5) to catch a stray move to
/// another thread immediately instead of as a heisenbug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WorkerToken(ThreadId);

impl WorkerToken {
    /// The token for the current thread.
    #[inline]
    pub fn current() -> Self {
        Self(std::thread::current().id())
    }
}

/// A bounded single-producer / single-consumer ring of [`WorkCompletion`]s.
///
/// The driver pushes (producer); the transport drains (consumer). Positions are
/// monotonically increasing counters; the slot index is `pos % capacity`. The
/// `head`/`tail` `Acquire`/`Release` pair provides the happens-before edge so a
/// completion's bytes are visible to the consumer before the index is
/// published.
///
/// Overflow is **not** silently handled: [`Inbox::push`] returns the rejected
/// completion so the caller can retain it and mark the connection fatal (§7.2).
struct Inbox {
    cells: Box<[UnsafeCell<WorkCompletion>]>,
    capacity: usize,
    /// Written only by the consumer; read by the producer (`Acquire`) to
    /// compute free space.
    head: AtomicUsize,
    /// Written only by the producer; read by the consumer (`Acquire`) to
    /// compute availability.
    tail: AtomicUsize,
}

// SAFETY: SPSC contract. Exactly one producer calls `push` and one consumer
// calls `drain`; the `head`/`tail` `Acquire`/`Release` pair orders the cell
// writes before the index publish and the index load before the cell reads, so
// no cell is read and written concurrently. `WorkCompletion` is a POD (`ibv_wc`,
// no pointers) and thus `Send`.
unsafe impl Send for Inbox {}
unsafe impl Sync for Inbox {}

impl Inbox {
    fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "inbox capacity must be non-zero");
        let cells = (0..capacity)
            .map(|_| UnsafeCell::new(WorkCompletion::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            cells,
            capacity,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer: enqueue one completion. Returns `Err(wc)` (the rejected
    /// completion, retained for the caller) when the ring is full.
    fn push(&self, wc: WorkCompletion) -> std::result::Result<(), WorkCompletion> {
        let tail = self.tail.load(Ordering::Relaxed); // only the producer writes tail
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.capacity {
            return Err(wc); // full — overflow is fatal (§7.2), caller retains wc
        }
        let idx = tail % self.capacity;
        // SAFETY: `idx` is in the free region [tail, head+capacity); the
        // consumer never reads it until we publish the new tail below. Single
        // producer, so no other writer.
        unsafe {
            *self.cells[idx].get() = wc;
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Consumer: drain up to `out.len()` completions in FIFO order. Returns the
    /// number written.
    fn drain(&self, out: &mut [WorkCompletion]) -> usize {
        if out.is_empty() {
            return 0;
        }
        let head = self.head.load(Ordering::Relaxed); // only the consumer writes head
        let tail = self.tail.load(Ordering::Acquire);
        let available = tail.wrapping_sub(head);
        let n = available.min(out.len());
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            let idx = head.wrapping_add(i) % self.capacity;
            // SAFETY: indices in [head, tail) were published by the producer and
            // are no longer written. Single consumer, so no other reader.
            *slot = unsafe { *self.cells[idx].get() };
        }
        if n > 0 {
            self.head.store(head.wrapping_add(n), Ordering::Release);
        }
        n
    }

    fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One direction's shared state: its inbox, its single waiter, and its
/// outstanding-WR counter.
struct DirState {
    inbox: Inbox,
    /// The single waiter for this direction (the stream's read half or write
    /// half). `AtomicWaker` gives lost-wakeup-safe register/wake (§7.3).
    waker: AtomicWaker,
    /// Outstanding WRs the NIC can still complete on this direction. Started
    /// before the first setup WR, incremented after a successful post,
    /// decremented once per CQE (success or flush). Never clamped (§6.2).
    posted: AtomicU32,
}

impl DirState {
    fn new(capacity: usize) -> Self {
        Self {
            inbox: Inbox::with_capacity(capacity),
            waker: AtomicWaker::new(),
            posted: AtomicU32::new(0),
        }
    }
}

/// The per-connection driver↔transport handoff.
///
/// Shared as `Arc<ConnSlot>` between the [`CoreDriver`](crate) task (producer)
/// and the transport task (consumer). See the module docs for the invariants it
/// enforces.
pub struct ConnSlot {
    send: DirState,
    recv: DirState,
    state: AtomicU8,
    /// Set when this connection hits a per-connection fatal fault (inbox
    /// overflow, unknown/stale routing, WC error the driver escalates). Orthogonal
    /// to [`SlotState`]: a fatal slot still progresses `Closing → Drained` as its
    /// flush CQEs are reaped.
    fatal: AtomicBool,
    /// The QP number this slot is registered under in the driver's routing map.
    /// Stable for the slot's life; reused only after retirement (§4.2).
    qp_num: u32,
    /// Bumped at retirement so stale *software* handles (never raw CQEs) can be
    /// detected. `qp_num` reuse is legal only after this bump.
    generation: AtomicU32,
    owner: WorkerToken,
}

impl ConnSlot {
    /// Build a slot for `qp_num`, owned by `owner`, with per-direction inboxes
    /// sized `send_capacity` / `recv_capacity` (each ≥ that direction's maximum
    /// simultaneously-completable WRs, §7.2). Starts in [`SlotState::Connecting`].
    pub fn new(
        qp_num: u32,
        send_capacity: usize,
        recv_capacity: usize,
        owner: WorkerToken,
    ) -> Self {
        Self {
            send: DirState::new(send_capacity),
            recv: DirState::new(recv_capacity),
            state: AtomicU8::new(SlotState::Connecting as u8),
            fatal: AtomicBool::new(false),
            qp_num,
            generation: AtomicU32::new(0),
            owner,
        }
    }

    #[inline]
    fn dir(&self, dir: Dir) -> &DirState {
        match dir {
            Dir::Send => &self.send,
            Dir::Recv => &self.recv,
        }
    }

    /// Assert the caller runs on the worker that owns this slot (§7.5).
    ///
    /// A cheap `debug_assert` on the hot path; catches a stray move to another
    /// thread/runtime, where touching the QP/CQ would race, immediately.
    #[inline]
    pub fn assert_owner(&self) {
        debug_assert_eq!(
            self.owner,
            WorkerToken::current(),
            "ConnSlot accessed off its owner worker (§7.5)"
        );
    }

    /// The QP number this slot routes for.
    #[inline]
    pub fn qp_num(&self) -> u32 {
        self.qp_num
    }

    // --- Producer (CoreDriver) side ---------------------------------------

    /// Enqueue one reaped completion into `dir`'s inbox.
    ///
    /// Returns `Err(wc)` (the rejected completion, retained) if the inbox is
    /// full — a fatal, per-connection fault (§7.2): the caller keeps the CQE,
    /// marks the slot fatal, and forces the QP to `ERR`. Does **not** wake; the
    /// driver batches a single [`ConnSlot::wake`] per direction per sweep to
    /// coalesce wakeups.
    #[inline]
    pub fn deliver(&self, dir: Dir, wc: WorkCompletion) -> std::result::Result<(), WorkCompletion> {
        self.assert_owner();
        self.dir(dir).inbox.push(wc)
    }

    /// Wake `dir`'s waiter (coalesced: at most once per sweep by the driver).
    #[inline]
    pub fn wake(&self, dir: Dir) {
        self.dir(dir).waker.wake();
    }

    /// Wake both directions — used on disconnect / fatal WC so a task blocked on
    /// the other half unblocks (§7.3).
    #[inline]
    pub fn wake_both(&self) {
        self.send.waker.wake();
        self.recv.waker.wake();
    }

    // --- WR accounting (§6.2) ---------------------------------------------

    /// Record a successfully posted WR on `dir`. Call **after** the post
    /// succeeds.
    #[inline]
    pub fn inc_posted(&self, dir: Dir) {
        self.assert_owner();
        self.dir(dir).posted.fetch_add(1, Ordering::Relaxed);
    }

    /// Account one reaped CQE on `dir` (success or flush). Panics on underflow —
    /// decrementing below zero means a CQE was counted that was never posted,
    /// an invariant violation, not something to clamp (§6.2).
    #[inline]
    pub fn dec_posted(&self, dir: Dir) {
        self.assert_owner();
        let prev = self.dir(dir).posted.fetch_sub(1, Ordering::Relaxed);
        assert!(
            prev != 0,
            "ConnSlot WR-accounting underflow on {dir:?} (qp_num={}): a CQE was \
             counted that was never posted (§6.2)",
            self.qp_num
        );
    }

    /// The current outstanding-WR count on `dir`.
    #[inline]
    pub fn posted(&self, dir: Dir) -> u32 {
        self.dir(dir).posted.load(Ordering::Relaxed)
    }

    // --- Lifecycle --------------------------------------------------------

    /// The current lifecycle state.
    #[inline]
    pub fn state(&self) -> SlotState {
        SlotState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Set the lifecycle state.
    #[inline]
    pub fn set_state(&self, state: SlotState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// Mark this connection as having hit a fatal, per-connection fault (§7.2).
    /// Also wakes both directions so blocked tasks observe the fault.
    #[inline]
    pub fn mark_fatal(&self) {
        self.fatal.store(true, Ordering::Release);
        self.wake_both();
    }

    /// Whether a fatal per-connection fault has been signalled.
    #[inline]
    pub fn is_fatal(&self) -> bool {
        self.fatal.load(Ordering::Acquire)
    }

    /// Whether the teardown drain barrier is satisfied: both WR counters zero
    /// and both inboxes empty (§6.2). The driver uses this to advance
    /// `Closing → Drained`.
    #[inline]
    pub fn drain_complete(&self) -> bool {
        self.posted(Dir::Send) == 0
            && self.posted(Dir::Recv) == 0
            && self.send.inbox.is_empty()
            && self.recv.inbox.is_empty()
    }

    /// The current retirement generation (bumped by [`ConnSlot::retire`]).
    #[inline]
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    /// Retire the slot after a completed drain: bump the generation so any stale
    /// software handle is detectable and the `qp_num` may be reused (§4.2).
    /// Debug-asserts the drain barrier held.
    #[inline]
    pub fn retire(&self) {
        debug_assert!(
            self.drain_complete(),
            "ConnSlot::retire before drain barrier (qp_num={})",
            self.qp_num
        );
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    // --- Consumer (transport) side ----------------------------------------

    /// `Poll`-based drain of `dir`'s inbox for the data path.
    ///
    /// Uses the register–check–recheck ordering (§7.3): drain once, and if empty
    /// register the task waker and drain again to close the
    /// completion-arrived-just-before-registration race. Returns `Ready(Ok(n))`
    /// with `n ≥ 1`, or `Pending` after registering the waker.
    pub fn poll_inbox(
        &self,
        dir: Dir,
        cx: &mut Context<'_>,
        out: &mut [WorkCompletion],
    ) -> Poll<Result<usize>> {
        self.assert_owner();
        let ds = self.dir(dir);

        // 1. Fast path: drain before touching the waker.
        let n = ds.inbox.drain(out);
        if n > 0 {
            return Poll::Ready(Ok(n));
        }

        // 2. Empty: register, then re-drain to catch a push that raced the
        //    registration.
        ds.waker.register(cx.waker());
        let n = ds.inbox.drain(out);
        if n > 0 {
            return Poll::Ready(Ok(n));
        }

        Poll::Pending
    }

    /// Synchronous, non-arming drain of `dir`'s inbox. Used by opportunistic
    /// live-path drains; teardown uses the driver, not this (§6.2).
    #[inline]
    pub fn drain_inbox(&self, dir: Dir, out: &mut [WorkCompletion]) -> usize {
        self.assert_owner();
        self.dir(dir).inbox.drain(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::task::{RawWaker, RawWakerVTable, Waker};

    fn wc_with(wr_id: u64) -> WorkCompletion {
        let mut wc = WorkCompletion::default();
        wc.inner.wr_id = wr_id;
        wc
    }

    // A waker that counts how many times it was awoken.
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

    #[test]
    fn inbox_fifo_and_capacity() {
        let ib = Inbox::with_capacity(4);
        assert!(ib.is_empty());
        for i in 0..4 {
            assert!(ib.push(wc_with(i)).is_ok());
        }
        assert_eq!(ib.len(), 4);
        // Full: next push is rejected and the wc is returned.
        let rejected = ib.push(wc_with(99));
        assert!(matches!(rejected, Err(w) if w.wr_id() == 99));

        let mut out = [WorkCompletion::default(); 8];
        let n = ib.drain(&mut out);
        assert_eq!(n, 4);
        for (i, wc) in out.iter().enumerate().take(4) {
            assert_eq!(wc.wr_id(), i as u64);
        }
        assert!(ib.is_empty());
    }

    #[test]
    fn inbox_wraps_around() {
        let ib = Inbox::with_capacity(2);
        let mut out = [WorkCompletion::default(); 2];
        // Push/drain repeatedly to force index wrap past capacity.
        for round in 0..5u64 {
            assert!(ib.push(wc_with(round)).is_ok());
            let n = ib.drain(&mut out);
            assert_eq!(n, 1);
            assert_eq!(out[0].wr_id(), round);
        }
    }

    fn slot(send_cap: usize, recv_cap: usize) -> ConnSlot {
        ConnSlot::new(0x1234, send_cap, recv_cap, WorkerToken::current())
    }

    #[test]
    fn poll_inbox_drains_before_registering() {
        let s = slot(4, 4);
        s.deliver(Dir::Recv, wc_with(7)).unwrap();
        let cw = CountingWaker::new();
        let waker = cw.waker();
        let mut cx = Context::from_waker(&waker);
        let mut out = [WorkCompletion::default(); 4];
        match s.poll_inbox(Dir::Recv, &mut cx, &mut out) {
            Poll::Ready(Ok(1)) => assert_eq!(out[0].wr_id(), 7),
            other => panic!("expected Ready(Ok(1)), got {other:?}"),
        }
    }

    #[test]
    fn poll_inbox_pending_then_woken_on_deliver() {
        let s = slot(4, 4);
        let cw = CountingWaker::new();
        let waker = cw.waker();
        let mut cx = Context::from_waker(&waker);
        let mut out = [WorkCompletion::default(); 4];

        // Empty inbox: registers the waker and returns Pending.
        assert!(matches!(
            s.poll_inbox(Dir::Send, &mut cx, &mut out),
            Poll::Pending
        ));
        assert_eq!(cw.count(), 0);

        // Producer delivers + wakes: the registered waker fires exactly once.
        s.deliver(Dir::Send, wc_with(42)).unwrap();
        s.wake(Dir::Send);
        assert_eq!(cw.count(), 1);

        // Consumer re-polls and drains the delivered completion.
        match s.poll_inbox(Dir::Send, &mut cx, &mut out) {
            Poll::Ready(Ok(1)) => assert_eq!(out[0].wr_id(), 42),
            other => panic!("expected Ready(Ok(1)), got {other:?}"),
        }
    }

    #[test]
    fn wake_both_wakes_both_directions() {
        let s = slot(2, 2);
        let sw = CountingWaker::new();
        let rw = CountingWaker::new();
        let s_waker = sw.waker();
        let r_waker = rw.waker();
        let mut out = [WorkCompletion::default(); 2];
        assert!(matches!(
            s.poll_inbox(Dir::Send, &mut Context::from_waker(&s_waker), &mut out),
            Poll::Pending
        ));
        assert!(matches!(
            s.poll_inbox(Dir::Recv, &mut Context::from_waker(&r_waker), &mut out),
            Poll::Pending
        ));
        s.wake_both();
        assert_eq!(sw.count(), 1);
        assert_eq!(rw.count(), 1);
    }

    #[test]
    fn wr_accounting_and_drain_barrier() {
        let s = slot(4, 4);
        assert!(s.drain_complete());
        s.inc_posted(Dir::Send);
        s.inc_posted(Dir::Send);
        s.inc_posted(Dir::Recv);
        assert_eq!(s.posted(Dir::Send), 2);
        assert_eq!(s.posted(Dir::Recv), 1);
        assert!(!s.drain_complete());
        s.dec_posted(Dir::Send);
        s.dec_posted(Dir::Send);
        s.dec_posted(Dir::Recv);
        assert!(s.drain_complete());
    }

    #[test]
    #[should_panic(expected = "WR-accounting underflow")]
    fn dec_posted_underflow_panics() {
        let s = slot(2, 2);
        s.dec_posted(Dir::Send);
    }

    #[test]
    fn drain_barrier_requires_empty_inbox() {
        let s = slot(2, 2);
        s.deliver(Dir::Recv, wc_with(1)).unwrap();
        // Counters zero but inbox non-empty → not drained.
        assert!(!s.drain_complete());
        let mut out = [WorkCompletion::default(); 2];
        assert_eq!(s.drain_inbox(Dir::Recv, &mut out), 1);
        assert!(s.drain_complete());
    }

    #[test]
    fn lifecycle_state_and_retire() {
        let s = slot(2, 2);
        assert_eq!(s.state(), SlotState::Connecting);
        s.set_state(SlotState::Established);
        assert_eq!(s.state(), SlotState::Established);
        s.set_state(SlotState::Closing);
        s.set_state(SlotState::Drained);
        assert_eq!(s.generation(), 0);
        s.retire();
        assert_eq!(s.generation(), 1);
    }

    #[test]
    fn fatal_flag_wakes_both() {
        let s = slot(2, 2);
        let sw = CountingWaker::new();
        let rw = CountingWaker::new();
        let s_waker = sw.waker();
        let r_waker = rw.waker();
        let mut out = [WorkCompletion::default(); 2];
        let _ = s.poll_inbox(Dir::Send, &mut Context::from_waker(&s_waker), &mut out);
        let _ = s.poll_inbox(Dir::Recv, &mut Context::from_waker(&r_waker), &mut out);
        assert!(!s.is_fatal());
        s.mark_fatal();
        assert!(s.is_fatal());
        assert_eq!(sw.count(), 1);
        assert_eq!(rw.count(), 1);
    }
}
