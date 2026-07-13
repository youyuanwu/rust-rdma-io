//! Completion source — the single seam through which a transport acquires
//! work completions, independent of *how* they are produced.
//!
//! This is Phase 0 of the read-ring busy-poll design
//! (`docs/design/rdma-read-ring-busy-poll.md`, Appendix A). It exists so the
//! same transport can, in a later phase, be driven either by its own
//! per-connection [`AsyncCq`] (arm-and-park, today) or by a per-core busy-poll
//! driver that fills a per-direction inbox — differing only in which
//! `CompletionSource` variant it holds.
//!
//! # Phase-0 scope
//!
//! Only the [`CompletionSource::ArmPark`] variant is implemented. The
//! driver-fed variant depends on the Phase-1 `ConnSlot`/inbox types and is
//! added then; keeping the enum single-variant now makes that purely additive.
//!
//! # Why a source instead of the raw CQ
//!
//! Routing *all* completion access — data path, setup token/MW-bind drain, and
//! teardown drain — through one type means nothing calls `ibv_poll_cq`
//! directly, and CQ ownership is decoupled from the QP: the source owns the
//! [`AsyncCq`] (arm-park), so a future driver-owned shared CQ can outlive the
//! QP without breaking the verbs "destroy QP before its CQ" rule. The owning
//! transport preserves that rule by declaring the QP field *before* its
//! sources.

use std::task::{Context, Poll};

use crate::Result;
use crate::async_cq::{AsyncCq, CqPollState};
use crate::wc::WorkCompletion;

/// Number of arm-then-poll iterations `drain_setup` spins to reap the
/// completions of already-posted setup work requests (token Send, MW binds).
/// The completions are in flight when the drain starts, so this is a short
/// bounded spin, not an unbounded wait. Mirrors the legacy `drain_send_cq`.
const SETUP_DRAIN_POLLS: usize = 100;

/// A single direction's completion stream. The transport still interprets the
/// [`WorkCompletion`]s it returns (Read-sentinel, immediate decode, tracker);
/// the source only *delivers* them.
pub enum CompletionSource {
    /// Owns a per-connection [`AsyncCq`] and its drain-after-arm state, and
    /// uses the arm-and-park pattern to sleep when the CQ is empty.
    ArmPark(ArmParkSource),
    // Phase 1: `Driver(DriverSource)` — reads a per-direction inbox filled by
    // the per-core busy-poll driver. Added when `ConnSlot` lands.
}

/// The arm-and-park completion source: a per-connection [`AsyncCq`] plus the
/// [`CqPollState`] that tracks its position in the arm → drain → wait loop.
pub struct ArmParkSource {
    cq: AsyncCq,
    state: CqPollState,
}

impl CompletionSource {
    /// Build an arm-and-park source that owns `cq`.
    pub fn arm_park(cq: AsyncCq) -> Self {
        Self::ArmPark(ArmParkSource {
            cq,
            state: CqPollState::default(),
        })
    }

    /// `Poll`-based acquire for the data path: returns `Ready(Ok(n))` with at
    /// least one completion, or registers the task waker and returns `Pending`.
    ///
    /// This is the arm-then-drain path verbatim — no behavior change from the
    /// previous `AsyncQp::poll_send_cq`/`poll_recv_cq(state)` calls.
    #[inline]
    pub fn poll_completions(
        &mut self,
        cx: &mut Context<'_>,
        wc_buf: &mut [WorkCompletion],
    ) -> Poll<Result<usize>> {
        match self {
            Self::ArmPark(s) => s.cq.poll_completions(cx, &mut s.state, wc_buf),
        }
    }

    /// Async acquire of at least one completion, used by connection setup
    /// (awaiting the peer's token Send). Self-contained arm-and-park loop;
    /// does not touch the data-path [`CqPollState`].
    pub async fn acquire(&mut self, wc_buf: &mut [WorkCompletion]) -> Result<usize> {
        match self {
            Self::ArmPark(s) => s.cq.poll(wc_buf).await,
        }
    }

    /// Synchronous, non-arming drain: a single `ibv_poll_cq`. Used by the
    /// teardown barrier, where the QP has already been forced to `ERROR` so the
    /// completions are present and no notification arming is needed.
    #[inline]
    pub fn try_drain(&mut self, wc_buf: &mut [WorkCompletion]) -> Result<usize> {
        match self {
            Self::ArmPark(s) => s.cq.cq().poll(wc_buf),
        }
    }

    /// Arm the CQ notification, then poll once (non-blocking drain-after-arm).
    ///
    /// Used by opportunistic synchronous drains on the live data path (e.g.
    /// credit harvesting) that must keep the async notification armed so a
    /// later completion still wakes the parked reactor.
    #[inline]
    pub fn drain_once(&mut self, wc_buf: &mut [WorkCompletion]) -> Result<usize> {
        match self {
            Self::ArmPark(s) => {
                s.cq.cq().req_notify(false)?;
                s.cq.cq().poll(wc_buf)
            }
        }
    }

    /// Drain the completions of already-posted setup work requests (token Send,
    /// MW binds) with a short bounded arm-then-poll spin. Replaces the legacy
    /// `transport_common::drain_send_cq` for sources. Returns the count drained.
    pub fn drain_setup(&mut self) -> Result<usize> {
        match self {
            Self::ArmPark(s) => {
                let mut wc = [WorkCompletion::default(); 16];
                let mut total = 0;
                for _ in 0..SETUP_DRAIN_POLLS {
                    s.cq.cq().req_notify(false)?;
                    match s.cq.cq().poll(&mut wc) {
                        Ok(0) => {
                            if total > 0 {
                                break;
                            }
                            std::hint::spin_loop();
                        }
                        Ok(n) => total += n,
                        Err(e) => return Err(e),
                    }
                }
                Ok(total)
            }
        }
    }
}
