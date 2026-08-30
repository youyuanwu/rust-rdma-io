//! Completion registry for per-operation future routing.
//!
//! Provides [`InflightMap`], a shared registry that maps `wr_id` tokens to
//! per-operation waker slots. The CQ completion driver uses this to dispatch
//! each CQE to the correct waiting future.
//!
//! # Design
//!
//! Follows the compio/tokio-uring pattern: each in-flight operation registers
//! a slot keyed by a unique token (encoded `wr_id`). When the CQE arrives,
//! the driver stores the completion in the slot and wakes the future.
//!
//! ## Free-List Allocation
//!
//! Slot allocation uses an O(1) amortized free-list (a `Mutex<Vec<u32>>`)
//! instead of O(n) linear scan. `register()` pops from the free-list;
//! `release()` pushes back. The registry accepts any capacity without
//! a hard ceiling.
//!
//! ## Generation Protection
//!
//! Each slot carries a generation counter, and the token encodes both the
//! slot index and the generation at registration time. A completion for a
//! stale generation is logged and discarded rather than delivered to a new
//! occupant. Duplicate completions (a second completion for the same live
//! token) are also detected and logged.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::Waker;

use crate::wc::WorkCompletion;

/// Encodes a slot index and generation into a single `wr_id` token.
///
/// Layout: bits [63..32] = generation (u32), bits [31..0] = slot index (u32).
#[inline]
fn encode_token(index: u32, generation: u32) -> u64 {
    ((generation as u64) << 32) | (index as u64)
}

/// Decodes a `wr_id` token into (slot_index, generation).
#[inline]
fn decode_token(token: u64) -> (u32, u32) {
    let index = token as u32;
    let generation = (token >> 32) as u32;
    (index, generation)
}

/// State of a single in-flight operation slot.
struct Slot {
    /// Current generation. Incremented each time the slot is reused.
    generation: AtomicU32,
    /// Inner state protected by mutex (waker + completion result).
    inner: Mutex<SlotInner>,
}

/// Mutable state inside a slot.
struct SlotInner {
    /// Whether this slot is currently occupied by an in-flight operation.
    occupied: bool,
    /// The waker to notify when the completion arrives.
    waker: Option<Waker>,
    /// The completion result, set by the driver when the CQE is reaped.
    completion: Option<WorkCompletion>,
}

impl Slot {
    fn new() -> Self {
        Self {
            generation: AtomicU32::new(0),
            inner: Mutex::new(SlotInner {
                occupied: false,
                waker: None,
                completion: None,
            }),
        }
    }
}

/// Shared completion registry for routing CQEs to per-operation futures.
///
/// Thread-safe and `Send + Sync`. Typically wrapped in `Arc` and shared
/// between the operation-posting side and the completion driver.
///
/// # Capacity
///
/// The registry accepts any capacity at construction. Slot allocation is
/// O(1) amortized via an internal free-list. Generation-protected tokens
/// reject stale, duplicate, and unknown completions.
pub struct InflightMap {
    slots: Box<[Slot]>,
    /// Free-list of unoccupied slot indices. Pop to allocate, push to release.
    free_list: Mutex<Vec<u32>>,
}

// Safety: All interior state uses Mutex/AtomicU32 for synchronization.
unsafe impl Send for InflightMap {}
unsafe impl Sync for InflightMap {}

/// Result of attempting to register an in-flight operation.
pub struct Registration {
    /// The `wr_id` token to use when posting the work request.
    pub token: u64,
}

impl InflightMap {
    /// Create a new registry with the given capacity.
    ///
    /// The capacity determines the maximum number of concurrent in-flight
    /// operations. No hard ceiling is imposed.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        let mut slots = Vec::with_capacity(cap);
        for _ in 0..cap {
            slots.push(Slot::new());
        }
        // Initialize free-list with all indices in reverse order so that
        // pop() returns 0, 1, 2, ... (natural order, nice for debugging).
        let free_list: Vec<u32> = (0..cap as u32).rev().collect();
        Self {
            slots: slots.into_boxed_slice(),
            free_list: Mutex::new(free_list),
        }
    }

    /// Register a new in-flight operation, returning a generation-protected token.
    ///
    /// Returns `None` if all slots are occupied (registry full).
    /// Allocation is O(1) amortized (free-list pop).
    pub fn register(&self) -> Option<Registration> {
        let mut free = self.free_list.lock().unwrap();
        let index = free.pop()?;
        let slot = &self.slots[index as usize];
        let mut inner = slot.inner.lock().unwrap();
        inner.occupied = true;
        inner.waker = None;
        inner.completion = None;
        let gen_val = slot.generation.load(Ordering::Relaxed);
        let token = encode_token(index, gen_val);
        Some(Registration { token })
    }

    /// Store a waker for a registered slot. Called by the operation future
    /// when it is polled.
    ///
    /// Returns `true` if the waker was stored (slot still occupied and no
    /// completion yet). Returns `false` if the completion already arrived
    /// (the future should check `take_completion`).
    pub fn register_waker(&self, token: u64, waker: &Waker) -> bool {
        let (index, gen_val) = decode_token(token);
        if let Some(slot) = self.slots.get(index as usize) {
            let current_gen = slot.generation.load(Ordering::Relaxed);
            if current_gen != gen_val {
                return false; // stale token
            }
            let mut inner = slot.inner.lock().unwrap();
            if inner.completion.is_some() {
                return false; // completion already arrived
            }
            if !inner.occupied {
                return false; // slot was released
            }
            match &inner.waker {
                Some(existing) if existing.will_wake(waker) => {}
                _ => inner.waker = Some(waker.clone()),
            }
            true
        } else {
            false
        }
    }

    /// Deliver a completion to the registered slot. Called by the CQ driver.
    ///
    /// Returns `true` if delivered successfully. Returns `false` if the
    /// token is stale, unknown, the slot is not occupied, or a duplicate
    /// completion (completion is logged/discarded by the caller or internally).
    pub fn complete(&self, token: u64, wc: WorkCompletion) -> bool {
        let (index, gen_val) = decode_token(token);
        if let Some(slot) = self.slots.get(index as usize) {
            let current_gen = slot.generation.load(Ordering::Relaxed);
            if current_gen != gen_val {
                tracing::warn!(
                    token,
                    expected_gen = current_gen,
                    got_gen = gen_val,
                    "stale completion token — discarding"
                );
                return false;
            }
            let mut inner = slot.inner.lock().unwrap();
            if !inner.occupied {
                tracing::warn!(token, "completion for unoccupied slot — discarding");
                return false;
            }
            if inner.completion.is_some() {
                tracing::warn!(token, "duplicate completion for occupied slot — discarding");
                return false;
            }
            inner.completion = Some(wc);
            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }
            true
        } else {
            tracing::warn!(token, "completion for unknown slot index — discarding");
            false
        }
    }

    /// Take the completion result from a slot. Called by the operation future
    /// when woken.
    ///
    /// Returns `None` if no completion has been delivered yet.
    pub fn take_completion(&self, token: u64) -> Option<WorkCompletion> {
        let (index, gen_val) = decode_token(token);
        if let Some(slot) = self.slots.get(index as usize) {
            let current_gen = slot.generation.load(Ordering::Relaxed);
            if current_gen != gen_val {
                return None;
            }
            let mut inner = slot.inner.lock().unwrap();
            inner.completion.take()
        } else {
            None
        }
    }

    /// Release a slot, incrementing its generation to invalidate any
    /// outstanding tokens. Called when:
    /// - The operation future completes (success or error)
    /// - The operation future is dropped after completion
    /// - Post failure cleanup
    ///
    /// Release is O(1) (free-list push).
    pub fn release(&self, token: u64) {
        let (index, gen_val) = decode_token(token);
        if let Some(slot) = self.slots.get(index as usize) {
            let current_gen = slot.generation.load(Ordering::Relaxed);
            if current_gen != gen_val {
                tracing::debug!(
                    token,
                    current_gen,
                    token_gen = gen_val,
                    "stale release — slot already reused"
                );
                return; // stale — already released by someone else
            }
            let mut inner = slot.inner.lock().unwrap();
            inner.occupied = false;
            inner.waker = None;
            inner.completion = None;
            // Increment generation so any racing completion is rejected
            slot.generation.fetch_add(1, Ordering::Relaxed);
            drop(inner);
            // Return index to the free-list
            self.free_list.lock().unwrap().push(index);
        }
    }

    /// Complete all occupied slots with the given work completion (for teardown).
    /// Used when the QP is moved to error state and all WRs are flushed.
    pub fn flush_all(&self, wc: WorkCompletion) {
        for slot in self.slots.iter() {
            let mut inner = slot.inner.lock().unwrap();
            if inner.occupied && inner.completion.is_none() {
                inner.completion = Some(wc);
                if let Some(waker) = inner.waker.take() {
                    waker.wake();
                }
            }
        }
    }

    /// Total capacity of the registry.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Number of currently occupied slots.
    ///
    /// Derived from `capacity - free_list.len()` for O(1) instead of O(n).
    pub fn inflight_count(&self) -> usize {
        self.slots.len() - self.free_list.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_complete() {
        let map = InflightMap::new(4);
        let reg = map.register().unwrap();

        let wc = WorkCompletion::default();
        assert!(map.complete(reg.token, wc));

        let result = map.take_completion(reg.token);
        assert!(result.is_some());

        map.release(reg.token);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn test_generation_protection() {
        let map = InflightMap::new(4);

        // Register and release → generation bumps
        let reg1 = map.register().unwrap();
        let token1 = reg1.token;
        map.release(token1);

        // Re-register same slot → new generation
        let reg2 = map.register().unwrap();
        assert_ne!(reg2.token, token1); // different token (generation bumped)

        // Old token should not deliver
        let wc = WorkCompletion::default();
        assert!(!map.complete(token1, wc));

        // New token should deliver
        assert!(map.complete(reg2.token, WorkCompletion::default()));
        map.release(reg2.token);
    }

    #[test]
    fn test_capacity_limit() {
        let map = InflightMap::new(2);
        let _r1 = map.register().unwrap();
        let _r2 = map.register().unwrap();
        assert!(map.register().is_none()); // full
    }

    #[test]
    fn test_flush_all() {
        let map = InflightMap::new(4);
        let r1 = map.register().unwrap();
        let r2 = map.register().unwrap();

        let flush_wc = WorkCompletion::default();
        map.flush_all(flush_wc);

        assert!(map.take_completion(r1.token).is_some());
        assert!(map.take_completion(r2.token).is_some());

        map.release(r1.token);
        map.release(r2.token);
    }

    #[test]
    fn test_waker_registration() {
        let map = InflightMap::new(4);
        let reg = map.register().unwrap();
        let waker = std::task::Waker::noop();

        // Register waker — should succeed (no completion yet)
        assert!(map.register_waker(reg.token, waker));

        // Deliver completion — should wake
        assert!(map.complete(reg.token, WorkCompletion::default()));

        // Register waker again — should return false (completion ready)
        assert!(!map.register_waker(reg.token, waker));

        map.release(reg.token);
    }

    #[test]
    fn test_release_after_stale() {
        let map = InflightMap::new(4);
        let reg = map.register().unwrap();
        let token = reg.token;
        map.release(token);

        // Double release should be a no-op (stale generation)
        map.release(token); // no panic
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn test_large_capacity() {
        // Capacity > 256 — no longer capped by MAX_INFLIGHT
        let map = InflightMap::new(1024);
        assert_eq!(map.capacity(), 1024);

        // Register all 1024 slots
        let mut tokens = Vec::new();
        for _ in 0..1024 {
            let reg = map.register().unwrap();
            tokens.push(reg.token);
        }
        assert!(map.register().is_none()); // full
        assert_eq!(map.inflight_count(), 1024);

        // Complete and release all
        for &token in &tokens {
            assert!(map.complete(token, WorkCompletion::default()));
            map.release(token);
        }
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn test_free_list_reuse() {
        let map = InflightMap::new(4);

        // Register slot 0, release it
        let reg1 = map.register().unwrap();
        let (idx1, gen1) = decode_token(reg1.token);
        map.release(reg1.token);

        // Re-register — should reuse the same index with a new generation
        let reg2 = map.register().unwrap();
        let (idx2, gen2) = decode_token(reg2.token);
        assert_eq!(idx1, idx2); // same slot reused
        assert_eq!(gen2, gen1 + 1); // generation incremented

        map.release(reg2.token);
    }

    #[test]
    fn test_inflight_count_accuracy() {
        let map = InflightMap::new(8);
        assert_eq!(map.inflight_count(), 0);

        let r1 = map.register().unwrap();
        assert_eq!(map.inflight_count(), 1);

        let r2 = map.register().unwrap();
        assert_eq!(map.inflight_count(), 2);

        map.complete(r1.token, WorkCompletion::default());
        map.release(r1.token);
        assert_eq!(map.inflight_count(), 1);

        map.complete(r2.token, WorkCompletion::default());
        map.release(r2.token);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn test_duplicate_completion_rejected() {
        let map = InflightMap::new(4);
        let reg = map.register().unwrap();

        // First completion — accepted
        assert!(map.complete(reg.token, WorkCompletion::default()));

        // Second completion for the same live token — rejected (duplicate)
        assert!(!map.complete(reg.token, WorkCompletion::default()));

        map.release(reg.token);
    }

    #[test]
    fn test_capacity_accessor() {
        let map = InflightMap::new(42);
        assert_eq!(map.capacity(), 42);
    }
}
