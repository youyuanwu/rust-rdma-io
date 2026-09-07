use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::v2::engine::registry::{Lookup, OperationToken, PagedRegistry};
use crate::v2::error::Result;

use super::state::OperationState;

pub(in crate::v2::engine::io_core) struct OperationRegistry {
    slots: PagedRegistry<OperationToken, Arc<OperationState>>,
}

impl OperationRegistry {
    pub(in crate::v2::engine::io_core) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            slots: PagedRegistry::new(capacity)?,
        })
    }

    pub(super) fn allocate(
        &self,
        make: impl FnOnce(OperationToken) -> Arc<OperationState>,
    ) -> Result<(OperationToken, Arc<OperationState>)> {
        self.slots.allocate_with(make)
    }

    pub(super) fn lookup(&self, token: OperationToken) -> Lookup<Arc<OperationState>> {
        self.slots.lookup_cloned(token)
    }

    pub(super) fn release(
        &self,
        token: OperationToken,
        completed: bool,
    ) -> Option<Arc<OperationState>> {
        self.slots.release(token, completed)
    }

    pub(in crate::v2::engine::io_core) fn live(&self) -> usize {
        self.slots.live()
    }

    pub(super) fn occupied(&self) -> Vec<Arc<OperationState>> {
        self.slots.occupied_cloned()
    }

    pub(super) fn scan_occupied(
        &self,
        start: usize,
        budget: usize,
    ) -> (Vec<Arc<OperationState>>, usize, bool, usize) {
        self.slots.scan_occupied_cloned(start, budget)
    }

    #[cfg(test)]
    pub(super) fn force_generation_for_test(
        &self,
        token: OperationToken,
        generation: u32,
    ) -> OperationToken {
        self.slots.force_generation_for_test(token, generation)
    }
}

pub(in crate::v2::engine::io_core) struct CqCreditPool {
    capacity: usize,
    used: AtomicUsize,
    retained: AtomicUsize,
}

impl CqCreditPool {
    pub(in crate::v2::engine::io_core) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: AtomicUsize::new(0),
            retained: AtomicUsize::new(0),
        }
    }

    pub(super) fn reserve(&self) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            if used >= self.capacity {
                return false;
            }
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => used = observed,
            }
        }
    }

    pub(super) fn release(&self) {
        let previous = self.used.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "CQ admission release must have a reservation");
    }

    pub(super) fn retain(&self) {
        self.retained.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn release_retained(&self) {
        let previous = self.retained.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "retained CQ credit must exist");
    }

    pub(in crate::v2::engine::io_core) fn free(&self) -> usize {
        self.capacity
            .saturating_sub(self.used.load(Ordering::Acquire))
    }

    pub(in crate::v2::engine::io_core) fn retained(&self) -> usize {
        self.retained.load(Ordering::Acquire)
    }
}
