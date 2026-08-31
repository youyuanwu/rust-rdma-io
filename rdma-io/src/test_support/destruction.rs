//! Test-only destruction event recording.

use std::fmt;
use std::sync::{Mutex, OnceLock};

/// An actual resource destruction/free call observed by a test hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestructionEvent {
    /// The resource operation that was invoked.
    pub kind: DestructionKind,
    /// Address of the resource passed to the underlying FFI call.
    pub address: usize,
}

/// Resource destruction/free operations instrumented at their FFI call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestructionKind {
    IbvCloseDevice,
    ContextFacade,
    MemoryRegion,
    QueuePair,
    CompletionQueue,
    CompletionChannel,
    ProtectionDomain,
    CmId,
    CmEventChannel,
    RdmaFreeDevices,
}

struct ActiveRecorder {
    id: u64,
    capacity: usize,
    events: Vec<DestructionEvent>,
    overflowed: bool,
}

#[derive(Default)]
struct RecorderState {
    next_id: u64,
    active: Option<ActiveRecorder>,
}

fn state() -> &'static Mutex<RecorderState> {
    static STATE: OnceLock<Mutex<RecorderState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(RecorderState::default()))
}

/// A bounded, explicitly armed destruction recorder.
///
/// Only one recorder is armed process-wide at a time. Concurrent tests wait
/// for the current recorder to be dropped, preventing one test from clearing
/// or consuming another test's observations.
#[derive(Debug)]
pub struct DestructionRecorder {
    id: u64,
}

/// A process-wide recorder was already armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecorderBusy;

impl fmt::Display for RecorderBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a destruction recorder is already armed")
    }
}

impl std::error::Error for RecorderBusy {}

impl DestructionRecorder {
    /// Arm a recorder that retains at most `capacity` events.
    ///
    /// A zero capacity is rejected because it could silently prove nothing.
    pub fn arm(capacity: usize) -> Self {
        Self::try_arm(capacity).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Try to arm without blocking an executor or test thread.
    pub fn try_arm(capacity: usize) -> Result<Self, RecorderBusy> {
        if capacity == 0 {
            return Err(RecorderBusy);
        }
        let mut state = state().lock().unwrap_or_else(|error| error.into_inner());
        if state.active.is_some() {
            return Err(RecorderBusy);
        }
        let id = state.next_id;
        state.next_id = state.next_id.checked_add(1).ok_or(RecorderBusy)?;
        state.active = Some(ActiveRecorder {
            id,
            capacity,
            events: Vec::with_capacity(capacity.min(64)),
            overflowed: false,
        });
        Ok(Self { id })
    }

    /// Return a copy of events recorded since this recorder was armed.
    pub fn snapshot(&self) -> Vec<DestructionEvent> {
        let state = state().lock().unwrap_or_else(|error| error.into_inner());
        state
            .active
            .as_ref()
            .filter(|active| active.id == self.id)
            .map(|active| active.events.clone())
            .unwrap_or_default()
    }

    /// Remove and return events recorded since this recorder was armed.
    pub fn take(&self) -> Vec<DestructionEvent> {
        let mut state = state().lock().unwrap_or_else(|error| error.into_inner());
        let Some(active) = state.active.as_mut().filter(|active| active.id == self.id) else {
            return Vec::new();
        };
        std::mem::take(&mut active.events)
    }

    /// Whether more events occurred than the configured bounded capacity.
    pub fn overflowed(&self) -> bool {
        let state = state().lock().unwrap_or_else(|error| error.into_inner());
        state
            .active
            .as_ref()
            .filter(|active| active.id == self.id)
            .is_some_and(|active| active.overflowed)
    }
}

impl Drop for DestructionRecorder {
    fn drop(&mut self) {
        let mut state = state().lock().unwrap_or_else(|error| error.into_inner());
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.id == self.id)
        {
            state.active = None;
        }
    }
}

pub(crate) fn record(kind: DestructionKind, address: usize) {
    let mut state = state().lock().unwrap_or_else(|error| error.into_inner());
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if active.events.len() < active.capacity {
        active.events.push(DestructionEvent { kind, address });
    } else {
        active.overflowed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn recorder_is_bounded_and_armed() {
        record(DestructionKind::QueuePair, 1);
        let recorder = DestructionRecorder::arm(2);
        record(DestructionKind::QueuePair, 2);
        record(DestructionKind::CmId, 3);
        record(DestructionKind::MemoryRegion, 4);
        assert_eq!(recorder.snapshot().len(), 2);
        assert!(recorder.overflowed());
    }

    #[test]
    fn recorder_accepts_parallel_producers() {
        let recorder = DestructionRecorder::arm(32);
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let mut threads = Vec::new();
        for address in 0..8 {
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                barrier.wait();
                record(DestructionKind::QueuePair, address);
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(recorder.take().len(), 8);
        assert!(!recorder.overflowed());
    }

    #[test]
    fn recorder_contention_is_reported_without_blocking() {
        let recorder = DestructionRecorder::arm(1);
        assert_eq!(DestructionRecorder::try_arm(1).unwrap_err(), RecorderBusy);
        drop(recorder);
        assert!(DestructionRecorder::try_arm(1).is_ok());
    }
}
