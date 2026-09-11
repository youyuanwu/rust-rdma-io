use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

pub(super) struct CoalescedQueue<T> {
    queue: VecDeque<T>,
    members: HashSet<T>,
    capacity: usize,
}

impl<T: Copy + Eq + Hash> CoalescedQueue<T> {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            members: HashSet::new(),
            capacity,
        }
    }

    pub(super) fn push(&mut self, value: T) -> bool {
        if self.members.contains(&value) {
            return false;
        }
        assert!(
            self.queue.len() < self.capacity,
            "coalesced control live-identity bound exceeded"
        );
        assert!(self.members.insert(value));
        self.queue.push_back(value);
        true
    }

    pub(super) fn pop(&mut self) -> Option<T> {
        let value = self.queue.pop_front()?;
        assert!(self.members.remove(&value));
        Some(value)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::CoalescedQueue;

    #[test]
    fn duplicate_at_capacity_is_a_coalesced_noop() {
        let mut queue = CoalescedQueue::new(1);
        assert!(queue.push(7));
        assert!(!queue.push(7));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop(), Some(7));
        assert!(queue.is_empty());
    }

    #[test]
    #[should_panic(expected = "coalesced control live-identity bound exceeded")]
    fn distinct_identity_beyond_capacity_panics() {
        let mut queue = CoalescedQueue::new(1);
        assert!(queue.push(7));
        queue.push(8);
    }

    #[test]
    fn drop_originated_distinct_overflow_uses_the_same_fail_fast_contract() {
        struct DropRequest<'a> {
            queue: &'a mut CoalescedQueue<u8>,
            value: u8,
        }

        impl Drop for DropRequest<'_> {
            fn drop(&mut self) {
                self.queue.push(self.value);
            }
        }

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut queue = CoalescedQueue::new(1);
            assert!(queue.push(7));
            drop(DropRequest {
                queue: &mut queue,
                value: 8,
            });
        }));
        assert!(panic.is_err());
    }

    #[test]
    fn pop_removes_membership_before_reentrant_insert() {
        let mut queue = CoalescedQueue::new(1);
        assert!(queue.push(7));
        assert_eq!(queue.pop(), Some(7));
        assert!(queue.push(7));
    }

    #[test]
    fn distinct_identities_preserve_fifo_order() {
        let mut queue = CoalescedQueue::new(3);
        assert!(queue.push(7));
        assert!(queue.push(8));
        assert!(queue.push(9));
        assert_eq!(queue.pop(), Some(7));
        assert_eq!(queue.pop(), Some(8));
        assert_eq!(queue.pop(), Some(9));
    }
}
