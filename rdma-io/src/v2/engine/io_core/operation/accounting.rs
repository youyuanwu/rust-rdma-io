//! Reactor-owned generational operation registry and CQ accounting.
//!
//! Neither type uses interior synchronization: the driver-owned reactor is the
//! only backend mutator and reaches both values through an exclusive
//! `&mut IoState` borrow.

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::v2::engine::registry::{Lookup, OperationToken};
use crate::v2::error::{Error, Result};

use super::state::{OperationState, UnacceptedRelease};

const PAGE_SIZE: usize = 256;

pub(in crate::v2::engine::io_core) struct OperationRegistry {
    capacity: usize,
    pages: Vec<Option<Box<[OperationSlot]>>>,
    recycled: Vec<u32>,
    next_unused: u32,
    live: usize,
    #[cfg(test)]
    fail_next_page_allocation: AtomicBool,
}

struct OperationSlot {
    generation: u32,
    state: SlotState,
    last_completed_generation: Option<u32>,
}

enum SlotState {
    Vacant,
    Occupied(OperationState),
    Retired,
}

impl OperationSlot {
    fn vacant() -> Self {
        Self {
            generation: 1,
            state: SlotState::Vacant,
            last_completed_generation: None,
        }
    }
}

impl OperationRegistry {
    pub(in crate::v2::engine::io_core) fn new(capacity: usize) -> Result<Self> {
        let page_count = capacity
            .checked_add(PAGE_SIZE - 1)
            .and_then(|value| value.checked_div(PAGE_SIZE))
            .ok_or_else(|| Error::InvalidConfig("registry page-directory overflow".into()))?;
        let mut pages = Vec::new();
        pages.try_reserve_exact(page_count).map_err(|_| {
            Error::InvalidConfig("registry page-directory allocation failed".into())
        })?;
        pages.resize_with(page_count, || None);
        Ok(Self {
            capacity,
            pages,
            recycled: Vec::new(),
            next_unused: 0,
            live: 0,
            #[cfg(test)]
            fail_next_page_allocation: AtomicBool::new(false),
        })
    }

    pub(super) fn allocate(
        &mut self,
        make: impl FnOnce(OperationToken) -> OperationState,
    ) -> Result<OperationToken> {
        let (slot, recycled) = if let Some(slot) = self.recycled.pop() {
            (slot, true)
        } else {
            let next = self.next_unused as usize;
            if next >= self.capacity {
                return Err(Error::CapacityExhausted);
            }
            self.next_unused = self
                .next_unused
                .checked_add(1)
                .ok_or(Error::CapacityExhausted)?;
            (next as u32, false)
        };
        let entry = match self.slot_mut(slot, true) {
            Ok(entry) => entry,
            Err(error) => {
                if recycled {
                    self.recycled.push(slot);
                } else {
                    self.next_unused = slot;
                }
                return Err(error);
            }
        };
        if !matches!(entry.state, SlotState::Vacant) {
            if recycled {
                self.recycled.push(slot);
            } else {
                self.next_unused = slot;
            }
            return Err(Error::InvalidConfig(
                "registry allocator selected a non-vacant slot".into(),
            ));
        }
        let token = OperationToken {
            slot,
            generation: entry.generation,
        };
        entry.state = SlotState::Occupied(make(token));
        self.live += 1;
        Ok(token)
    }

    pub(in crate::v2::engine::io_core) fn lookup(
        &self,
        token: OperationToken,
    ) -> Lookup<&OperationState> {
        let Some(entry) = self.slot_ref(token.slot) else {
            return Lookup::Unknown;
        };
        if entry.last_completed_generation == Some(token.generation) {
            return Lookup::Duplicate;
        }
        if entry.generation != token.generation {
            return Lookup::Stale;
        }
        match &entry.state {
            SlotState::Occupied(value) => Lookup::Occupied(value),
            SlotState::Vacant => Lookup::Unknown,
            SlotState::Retired => Lookup::Retired,
        }
    }

    pub(in crate::v2::engine::io_core) fn lookup_mut(
        &mut self,
        token: OperationToken,
    ) -> Lookup<&mut OperationState> {
        let Some(entry) = self.slot_mut(token.slot, false).ok() else {
            return Lookup::Unknown;
        };
        if entry.last_completed_generation == Some(token.generation) {
            return Lookup::Duplicate;
        }
        if entry.generation != token.generation {
            return Lookup::Stale;
        }
        match &mut entry.state {
            SlotState::Occupied(value) => Lookup::Occupied(value),
            SlotState::Vacant => Lookup::Unknown,
            SlotState::Retired => Lookup::Retired,
        }
    }

    pub(super) fn release(
        &mut self,
        token: OperationToken,
        completed: bool,
    ) -> Option<OperationState> {
        let entry = self.slot_mut(token.slot, false).ok()?;
        if entry.generation != token.generation {
            return None;
        }
        let value = match std::mem::replace(&mut entry.state, SlotState::Vacant) {
            SlotState::Occupied(value) => value,
            other => {
                entry.state = other;
                return None;
            }
        };
        if completed {
            entry.last_completed_generation = Some(token.generation);
        }
        if entry.generation == u32::MAX {
            entry.state = SlotState::Retired;
        } else {
            entry.generation += 1;
            self.recycled.push(token.slot);
        }
        self.live -= 1;
        Some(value)
    }

    pub(in crate::v2::engine::io_core) fn live(&self) -> usize {
        self.live
    }

    pub(super) fn occupied_tokens(&self) -> Vec<OperationToken> {
        (0..self.next_unused)
            .filter_map(|slot| {
                let entry = self.slot_ref(slot)?;
                matches!(entry.state, SlotState::Occupied(_)).then_some(OperationToken {
                    slot,
                    generation: entry.generation,
                })
            })
            .collect()
    }

    pub(super) fn scan_occupied_tokens(
        &self,
        start: usize,
        budget: usize,
    ) -> (Vec<OperationToken>, usize, bool, usize) {
        let end = (start.saturating_add(budget)).min(self.next_unused as usize);
        let tokens = (start..end)
            .filter_map(|slot| {
                let entry = self.slot_ref(slot as u32)?;
                matches!(entry.state, SlotState::Occupied(_)).then_some(OperationToken {
                    slot: slot as u32,
                    generation: entry.generation,
                })
            })
            .collect();
        (tokens, end, end >= self.next_unused as usize, end - start)
    }

    pub(super) fn take_proven_unaccepted_batch(
        &mut self,
        tokens: &[OperationToken],
        error: Error,
    ) -> Option<Vec<UnacceptedRelease>> {
        if tokens.iter().any(|token| {
            !matches!(
                self.lookup(*token),
                Lookup::Occupied(operation) if operation.can_release_unaccepted()
            )
        }) {
            return None;
        }
        Some(
            tokens
                .iter()
                .map(|token| {
                    let Lookup::Occupied(operation) = self.lookup_mut(*token) else {
                        unreachable!("validated unaccepted operation remains registered")
                    };
                    operation
                        .take_unaccepted(error.clone())
                        .expect("validated unaccepted operation has no completion")
                })
                .collect(),
        )
    }

    fn slot_ref(&self, slot: u32) -> Option<&OperationSlot> {
        let index = slot as usize;
        if index >= self.capacity {
            return None;
        }
        self.pages
            .get(index / PAGE_SIZE)?
            .as_ref()?
            .get(index % PAGE_SIZE)
    }

    fn slot_mut(&mut self, slot: u32, allocate_page: bool) -> Result<&mut OperationSlot> {
        let index = slot as usize;
        if index >= self.capacity {
            return Err(Error::CapacityExhausted);
        }
        let page_index = index / PAGE_SIZE;
        if self.pages[page_index].is_none() {
            if !allocate_page {
                return Err(Error::CapacityExhausted);
            }
            #[cfg(test)]
            if self.fail_next_page_allocation.swap(false, Ordering::AcqRel) {
                return Err(Error::InvalidConfig(
                    "registry page allocation failed".into(),
                ));
            }
            let mut page = Vec::new();
            page.try_reserve_exact(PAGE_SIZE)
                .map_err(|_| Error::InvalidConfig("registry page allocation failed".into()))?;
            page.resize_with(PAGE_SIZE, OperationSlot::vacant);
            self.pages[page_index] = Some(page.into_boxed_slice());
        }
        self.pages[page_index]
            .as_mut()
            .and_then(|page| page.get_mut(index % PAGE_SIZE))
            .ok_or_else(|| {
                Error::InvalidConfig("registry page was not allocated after reservation".into())
            })
    }
}

pub(in crate::v2::engine::io_core) struct CqCreditPool {
    capacity: usize,
    used: usize,
    retained: usize,
}

impl CqCreditPool {
    pub(in crate::v2::engine::io_core) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: 0,
            retained: 0,
        }
    }

    pub(super) fn reserve(&mut self) -> bool {
        if self.used >= self.capacity {
            return false;
        }
        self.used += 1;
        true
    }

    pub(super) fn release(&mut self) {
        debug_assert!(
            self.used > 0,
            "CQ admission release must have a reservation"
        );
        self.used -= 1;
    }

    pub(super) fn retain(&mut self) {
        self.retained += 1;
    }

    pub(super) fn release_retained(&mut self) {
        debug_assert!(self.retained > 0, "retained CQ credit must exist");
        self.retained -= 1;
    }

    pub(in crate::v2::engine::io_core) fn free(&self) -> usize {
        self.capacity.saturating_sub(self.used)
    }

    pub(in crate::v2::engine::io_core) fn retained(&self) -> usize {
        self.retained
    }
}
