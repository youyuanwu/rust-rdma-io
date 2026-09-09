//! Lazily paged generational registries used by the shared engine.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::v2::error::{Error, Result};

const PAGE_SIZE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ConnectionToken {
    pub(super) slot: u32,
    pub(super) generation: u32,
}

/// Proof that the session registry currently owns an exact connection/QP pair.
///
/// Only the reactor-owned connection registry can construct this value. The I/O core consumes
/// it while validating a copied CQE; it carries no connection resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LiveIoConnectionProof {
    connection: ConnectionToken,
    qp_num: u32,
    _private: (),
}

impl LiveIoConnectionProof {
    pub(in crate::v2::engine) const fn issue_live_io_proof(
        connection: ConnectionToken,
        qp_num: u32,
    ) -> Self {
        Self {
            connection,
            qp_num,
            _private: (),
        }
    }

    pub(super) fn proves(self, connection: ConnectionToken, qp_num: u32) -> bool {
        self.connection == connection && self.qp_num == qp_num
    }

    #[cfg(test)]
    pub(super) fn for_test(identity: super::io_core::EstablishedIoIdentity) -> Self {
        Self {
            connection: identity.connection,
            qp_num: identity.qp_num,
            _private: (),
        }
    }
}

impl ConnectionToken {
    pub(super) const fn encode(self) -> u64 {
        ((self.generation as u64) << 32) | self.slot as u64
    }

    pub(super) const fn decode(value: u64) -> Self {
        Self {
            slot: value as u32,
            generation: (value >> 32) as u32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct OperationToken {
    pub(super) slot: u32,
    pub(super) generation: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ListenerToken {
    pub(super) slot: u32,
    pub(super) generation: u32,
}

impl OperationToken {
    pub(super) const fn encode(self) -> u64 {
        ((self.generation as u64) << 32) | self.slot as u64
    }

    pub(super) const fn decode(value: u64) -> Self {
        Self {
            slot: value as u32,
            generation: (value >> 32) as u32,
        }
    }
}

impl ListenerToken {
    pub(super) const fn encode(self) -> u64 {
        ((self.generation as u64) << 32) | self.slot as u64
    }

    #[cfg(test)]
    pub(super) const fn decode(value: u64) -> Self {
        Self {
            slot: value as u32,
            generation: (value >> 32) as u32,
        }
    }
}

pub(super) trait RegistryToken: Copy {
    fn from_parts(slot: u32, generation: u32) -> Self;
    fn slot(self) -> u32;
    fn generation(self) -> u32;
}

impl RegistryToken for ConnectionToken {
    fn from_parts(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    fn slot(self) -> u32 {
        self.slot
    }

    fn generation(self) -> u32 {
        self.generation
    }
}

impl RegistryToken for OperationToken {
    fn from_parts(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    fn slot(self) -> u32 {
        self.slot
    }

    fn generation(self) -> u32 {
        self.generation
    }
}

impl RegistryToken for ListenerToken {
    fn from_parts(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    fn slot(self) -> u32 {
        self.slot
    }

    fn generation(self) -> u32 {
        self.generation
    }
}

#[derive(Clone)]
pub(super) enum Lookup<T> {
    Occupied(T),
    Duplicate,
    Stale,
    Unknown,
    Retired,
}

pub(super) struct PagedRegistry<K, T> {
    capacity: usize,
    inner: RegistryInner<T>,
    live: usize,
    fail_next_page_allocation: bool,
    _token: std::marker::PhantomData<fn() -> K>,
}

struct RegistryInner<T> {
    pages: Vec<Option<Box<[RegistrySlot<T>]>>>,
    recycled: Vec<u32>,
    next_unused: u32,
}

struct RegistrySlot<T> {
    generation: u32,
    state: SlotState<T>,
    last_completed_generation: Option<u32>,
}

enum SlotState<T> {
    Vacant,
    Occupied(T),
    Retired,
}

impl<T> RegistrySlot<T> {
    fn vacant() -> Self {
        Self {
            generation: 1,
            state: SlotState::Vacant,
            last_completed_generation: None,
        }
    }
}

impl<K: RegistryToken, T> PagedRegistry<K, T> {
    pub(super) fn new(capacity: usize) -> Result<Self> {
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
            inner: RegistryInner {
                pages,
                recycled: Vec::new(),
                next_unused: 0,
            },
            live: 0,
            fail_next_page_allocation: false,
            _token: std::marker::PhantomData,
        })
    }

    #[cfg(test)]
    pub(super) fn allocate_with(&mut self, make: impl FnOnce(K) -> T) -> Result<(K, T)>
    where
        T: Clone,
    {
        let inner = &mut self.inner;
        let (slot, recycled) = if let Some(slot) = inner.recycled.pop() {
            (slot, true)
        } else {
            let next = inner.next_unused as usize;
            if next >= self.capacity {
                return Err(Error::CapacityExhausted);
            }
            inner.next_unused = inner
                .next_unused
                .checked_add(1)
                .ok_or(Error::CapacityExhausted)?;
            (next as u32, false)
        };
        let entry = match Self::slot_mut(
            self.capacity,
            inner,
            slot,
            true,
            &mut self.fail_next_page_allocation,
        ) {
            Ok(entry) => entry,
            Err(error) => {
                if recycled {
                    inner.recycled.push(slot);
                } else {
                    inner.next_unused = slot;
                }
                return Err(error);
            }
        };
        if !matches!(entry.state, SlotState::Vacant) {
            if recycled {
                inner.recycled.push(slot);
            } else {
                inner.next_unused = slot;
            }
            return Err(Error::InvalidConfig(
                "registry allocator selected a non-vacant slot".into(),
            ));
        }
        let token = K::from_parts(slot, entry.generation);
        let value = make(token);
        entry.state = SlotState::Occupied(value.clone());
        self.live += 1;
        Ok((token, value))
    }

    pub(super) fn allocate_owned(&mut self, make: impl FnOnce(K) -> T) -> Result<K> {
        let inner = &mut self.inner;
        let (slot, recycled) = if let Some(slot) = inner.recycled.pop() {
            (slot, true)
        } else {
            let next = inner.next_unused as usize;
            if next >= self.capacity {
                return Err(Error::CapacityExhausted);
            }
            inner.next_unused = inner
                .next_unused
                .checked_add(1)
                .ok_or(Error::CapacityExhausted)?;
            (next as u32, false)
        };
        let entry = match Self::slot_mut(
            self.capacity,
            inner,
            slot,
            true,
            &mut self.fail_next_page_allocation,
        ) {
            Ok(entry) => entry,
            Err(error) => {
                if recycled {
                    inner.recycled.push(slot);
                } else {
                    inner.next_unused = slot;
                }
                return Err(error);
            }
        };
        if !matches!(entry.state, SlotState::Vacant) {
            if recycled {
                inner.recycled.push(slot);
            } else {
                inner.next_unused = slot;
            }
            return Err(Error::InvalidConfig(
                "registry allocator selected a non-vacant slot".into(),
            ));
        }
        let token = K::from_parts(slot, entry.generation);
        entry.state = SlotState::Occupied(make(token));
        self.live += 1;
        Ok(token)
    }

    pub(super) fn lookup_ref(&self, token: K) -> Lookup<&T> {
        let Some(entry) = self.slot_ref(token.slot()) else {
            return Lookup::Unknown;
        };
        if entry.last_completed_generation == Some(token.generation()) {
            return Lookup::Duplicate;
        }
        if entry.generation != token.generation() {
            return Lookup::Stale;
        }
        match &entry.state {
            SlotState::Occupied(value) => Lookup::Occupied(value),
            SlotState::Vacant => Lookup::Unknown,
            SlotState::Retired => Lookup::Retired,
        }
    }

    #[cfg(test)]
    pub(super) fn lookup_cloned(&self, token: K) -> Lookup<T>
    where
        T: Clone,
    {
        let Some(entry) = self.slot_ref(token.slot()) else {
            return Lookup::Unknown;
        };
        if entry.last_completed_generation == Some(token.generation()) {
            return Lookup::Duplicate;
        }

        if entry.generation != token.generation() {
            return Lookup::Stale;
        }
        match &entry.state {
            SlotState::Occupied(value) => Lookup::Occupied(value.clone()),
            SlotState::Vacant => Lookup::Unknown,
            SlotState::Retired => Lookup::Retired,
        }
    }

    pub(super) fn get_mut(&mut self, token: K) -> Option<&mut T> {
        let entry = Self::slot_mut(
            self.capacity,
            &mut self.inner,
            token.slot(),
            false,
            &mut self.fail_next_page_allocation,
        )
        .ok()?;
        if entry.generation != token.generation() {
            return None;
        }
        match &mut entry.state {
            SlotState::Occupied(value) => Some(value),
            SlotState::Vacant | SlotState::Retired => None,
        }
    }

    pub(super) fn release(&mut self, token: K, completed: bool) -> Option<T> {
        let inner = &mut self.inner;
        let entry = Self::slot_mut(
            self.capacity,
            inner,
            token.slot(),
            false,
            &mut self.fail_next_page_allocation,
        )
        .ok()?;
        if entry.generation != token.generation() {
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
            entry.last_completed_generation = Some(token.generation());
        }
        if entry.generation == u32::MAX {
            entry.state = SlotState::Retired;
        } else {
            entry.generation += 1;
            inner.recycled.push(token.slot());
        }
        self.live -= 1;
        Some(value)
    }

    pub(super) fn live(&self) -> usize {
        self.live
    }

    pub(super) fn has_capacity(&self) -> bool {
        !self.inner.recycled.is_empty() || (self.inner.next_unused as usize) < self.capacity
    }

    #[cfg(test)]
    pub(super) fn retired(&self) -> usize {
        self.inner
            .pages
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|page| page.iter())
            .filter(|entry| matches!(entry.state, SlotState::Retired))
            .count()
    }

    #[cfg(test)]
    pub(super) fn free(&self) -> usize {
        self.capacity
            .saturating_sub(self.live())
            .saturating_sub(self.retired())
    }

    #[cfg(test)]
    fn allocated_pages(&self) -> usize {
        self.inner
            .pages
            .iter()
            .filter(|page| page.is_some())
            .count()
    }

    #[cfg(test)]
    fn fail_next_page_allocation(&mut self) {
        self.fail_next_page_allocation = true;
    }

    pub(super) fn occupied_tokens(&self) -> Vec<K> {
        self.inner
            .pages
            .iter()
            .enumerate()
            .flat_map(|(page_index, page)| {
                page.iter().flat_map(move |page| {
                    page.iter()
                        .enumerate()
                        .filter_map(move |(slot_index, slot)| {
                            matches!(slot.state, SlotState::Occupied(_)).then(|| {
                                K::from_parts(
                                    (page_index * PAGE_SIZE + slot_index) as u32,
                                    slot.generation,
                                )
                            })
                        })
                })
            })
            .collect()
    }

    pub(super) fn scan_occupied_tokens(
        &self,
        start: usize,
        budget: usize,
    ) -> (Vec<K>, usize, bool, usize) {
        let end = (start.saturating_add(budget)).min(self.inner.next_unused as usize);
        let tokens = (start..end)
            .filter_map(|slot| {
                let entry = self.slot_ref(slot as u32)?;
                matches!(entry.state, SlotState::Occupied(_))
                    .then(|| K::from_parts(slot as u32, entry.generation))
            })
            .collect();
        (
            tokens,
            end,
            end >= self.inner.next_unused as usize,
            end - start,
        )
    }

    #[cfg(test)]
    fn insert_at_for_test(&mut self, slot: u32, value: T) -> K {
        assert!((slot as usize) < self.capacity);
        let entry = Self::slot_mut(
            self.capacity,
            &mut self.inner,
            slot,
            true,
            &mut self.fail_next_page_allocation,
        )
        .unwrap();
        assert!(matches!(entry.state, SlotState::Vacant));
        let token = K::from_parts(slot, entry.generation);
        entry.state = SlotState::Occupied(value);
        self.live += 1;
        token
    }

    #[cfg(test)]
    pub(super) fn force_generation_for_test(&mut self, token: K, generation: u32) -> K {
        let entry = Self::slot_mut(
            self.capacity,
            &mut self.inner,
            token.slot(),
            false,
            &mut self.fail_next_page_allocation,
        )
        .unwrap();
        assert_eq!(entry.generation, token.generation());
        entry.generation = generation;
        K::from_parts(token.slot(), generation)
    }

    fn slot_ref(&self, slot: u32) -> Option<&RegistrySlot<T>> {
        let index = slot as usize;
        if index >= self.capacity {
            return None;
        }
        let page = self.inner.pages.get(index / PAGE_SIZE)?.as_ref()?;
        page.get(index % PAGE_SIZE)
    }

    fn slot_mut<'a>(
        capacity: usize,
        inner: &'a mut RegistryInner<T>,
        slot: u32,
        allocate_page: bool,
        _fail_next_page_allocation: &mut bool,
    ) -> Result<&'a mut RegistrySlot<T>> {
        let index = slot as usize;
        if index >= capacity {
            return Err(Error::CapacityExhausted);
        }
        let page_index = index / PAGE_SIZE;
        if inner.pages[page_index].is_none() {
            if !allocate_page {
                return Err(Error::CapacityExhausted);
            }
            #[cfg(test)]
            if std::mem::replace(_fail_next_page_allocation, false) {
                return Err(Error::InvalidConfig(
                    "registry page allocation failed".into(),
                ));
            }
            let mut page = Vec::new();
            page.try_reserve_exact(PAGE_SIZE)
                .map_err(|_| Error::InvalidConfig("registry page allocation failed".into()))?;
            page.resize_with(PAGE_SIZE, RegistrySlot::vacant);
            inner.pages[page_index] = Some(page.into_boxed_slice());
        }
        let page = inner.pages[page_index].as_mut().ok_or_else(|| {
            Error::InvalidConfig("registry page was not allocated after reservation".into())
        })?;
        Ok(&mut page[index % PAGE_SIZE])
    }
}

pub(super) fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

pub(super) fn read_unpoison<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|error| error.into_inner())
}

pub(super) fn write_unpoison<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestRegistry = PagedRegistry<ConnectionToken, usize>;

    #[test]
    fn registry_allocates_pages_lazily_at_representative_scale() {
        for capacity in [1, 1_024, 1_048_576] {
            let mut registry = TestRegistry::new(capacity).unwrap();
            assert_eq!(registry.allocated_pages(), 0);
            let mut slots = vec![0, capacity / 2, capacity - 1];
            slots.sort_unstable();
            slots.dedup();
            let tokens: Vec<_> = slots
                .into_iter()
                .enumerate()
                .map(|(value, slot)| registry.insert_at_for_test(slot as u32, value + 10))
                .collect();
            for (offset, token) in tokens.iter().copied().enumerate() {
                assert!(matches!(
                    registry.lookup_cloned(token),
                    Lookup::Occupied(value) if value == offset + 10
                ));
            }
            assert!(
                registry.allocated_pages() <= 3,
                "only touched pages may be allocated"
            );
            let inner = &registry.inner;
            let resident_slot_capacity = inner
                .pages
                .iter()
                .filter_map(Option::as_ref)
                .map(|page| page.len())
                .sum::<usize>();
            let estimated_heap_bytes = inner.pages.capacity()
                * std::mem::size_of::<Option<Box<[RegistrySlot<usize>]>>>()
                + resident_slot_capacity * std::mem::size_of::<RegistrySlot<usize>>()
                + inner.recycled.capacity() * std::mem::size_of::<u32>();
            assert!(
                resident_slot_capacity <= 3 * PAGE_SIZE,
                "representative slots must allocate at most three pages"
            );
            assert!(
                estimated_heap_bytes < 256 * 1024,
                "representative million-slot registry allocated {estimated_heap_bytes} bytes"
            );
        }
    }

    #[test]
    fn page_allocation_failure_restores_the_exact_fresh_slot() {
        let mut registry = TestRegistry::new(1).unwrap();
        registry.fail_next_page_allocation();

        assert!(matches!(
            registry.allocate_with(|_| 7),
            Err(Error::InvalidConfig(message)) if message == "registry page allocation failed"
        ));
        assert_eq!(registry.live(), 0);
        assert_eq!(registry.free(), 1);
        assert_eq!(registry.allocated_pages(), 0);

        let (token, value) = registry.allocate_with(|_| 9).unwrap();
        assert_eq!(token.slot, 0);
        assert_eq!(token.generation, 1);
        assert_eq!(value, 9);
        assert_eq!(registry.live(), 1);
        assert_eq!(registry.free(), 0);
    }

    #[test]
    fn release_invalidates_before_reuse_and_duplicate_is_distinct() {
        let mut registry = TestRegistry::new(1).unwrap();
        let (first, _) = registry.allocate_with(|_| 7).unwrap();
        assert_eq!(registry.release(first, true), Some(7));
        assert!(matches!(registry.lookup_cloned(first), Lookup::Duplicate));
        let (second, _) = registry.allocate_with(|_| 9).unwrap();
        assert_eq!(first.slot, second.slot);
        assert_eq!(second.generation, first.generation + 1);
        assert!(matches!(registry.lookup_cloned(first), Lookup::Duplicate));
        assert!(matches!(
            registry.lookup_cloned(second),
            Lookup::Occupied(9)
        ));
    }

    #[test]
    fn maximum_generation_retires_without_wrapping() {
        let mut registry = TestRegistry::new(1).unwrap();
        let (token, _) = registry.allocate_with(|_| 1).unwrap();
        let exhausted = registry.force_generation_for_test(token, u32::MAX);
        assert_eq!(registry.release(exhausted, true), Some(1));
        assert_eq!(registry.retired(), 1);
        assert_eq!(registry.free(), 0);
        assert!(matches!(
            registry.lookup_cloned(exhausted),
            Lookup::Duplicate
        ));
        assert!(matches!(
            registry.allocate_with(|_| 2),
            Err(Error::CapacityExhausted)
        ));

        let mut retired_lookup = TestRegistry::new(1).unwrap();
        let (token, _) = retired_lookup.allocate_with(|_| 2).unwrap();
        let exhausted = retired_lookup.force_generation_for_test(token, u32::MAX);
        assert_eq!(retired_lookup.release(exhausted, false), Some(2));
        assert!(matches!(
            retired_lookup.lookup_cloned(exhausted),
            Lookup::Retired
        ));
    }

    #[test]
    fn exclusive_registrations_fill_and_release_exact_capacity() {
        let mut registry = TestRegistry::new(8).unwrap();
        let entries = (0..8)
            .map(|value| registry.allocate_with(|_| value).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(registry.live(), 8);
        assert_eq!(registry.free(), 0);
        assert!(matches!(
            registry.allocate_with(|_| usize::MAX),
            Err(Error::CapacityExhausted)
        ));
        for (token, value) in entries {
            assert_eq!(registry.release(token, true), Some(value));
        }
        assert_eq!(registry.live(), 0);
        assert_eq!(registry.free(), 8);
    }

    #[test]
    fn operation_tokens_round_trip_without_connection_bit_packing() {
        let token = OperationToken {
            slot: 0x00ff_ee11,
            generation: 0xaabb_ccdd,
        };
        assert_eq!(OperationToken::decode(token.encode()), token);
    }
}
