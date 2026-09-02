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
    /// Return code from a fallible destruction primitive, when it has one.
    ///
    /// Void primitives such as the ordinary `rdma_destroy_qp` drop path and
    /// `rdma_free_devices` leave this as `None`. The engine's result-aware
    /// `ibv_destroy_qp` path records its return code.
    pub result: Option<i32>,
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
    CqReadinessAdapter,
    CmReadinessAdapter,
    CmEventAck,
    CmDrainToWouldBlock,
    CmFinalDrainToWouldBlock,
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
/// Only one recorder is armed process-wide at a time, so one test can never
/// clear or consume another test's observations. Arming never blocks: a second
/// concurrent request is reported as [`RecorderArmError::Busy`] instead of
/// waiting, which keeps the recorder usable from executor threads. Test
/// binaries that use it therefore rely on the workspace-wide
/// `RUST_TEST_THREADS=1` setting in `.cargo/config.toml` to serialize their own
/// runs.
#[derive(Debug)]
pub struct DestructionRecorder {
    id: u64,
}

/// Failure to arm the process-wide destruction recorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderArmError {
    /// A process-wide recorder was already armed.
    Busy,
    /// A zero-capacity recorder could not retain any evidence.
    ZeroCapacity,
    /// The recorder identity space was exhausted.
    IdentityExhausted,
}

impl fmt::Display for RecorderArmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("a destruction recorder is already armed"),
            Self::ZeroCapacity => {
                f.write_str("destruction recorder capacity must be greater than zero")
            }
            Self::IdentityExhausted => f.write_str("destruction recorder identity exhausted"),
        }
    }
}

impl std::error::Error for RecorderArmError {}

impl DestructionRecorder {
    /// Arm a recorder that retains at most `capacity` events.
    ///
    /// A zero capacity is rejected because it could silently prove nothing.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero or when another recorder is already
    /// armed process-wide. Use [`Self::try_arm`] to handle contention.
    pub fn arm(capacity: usize) -> Self {
        Self::try_arm(capacity).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Try to arm without blocking an executor or test thread.
    ///
    /// Returns a distinct error for contention, zero capacity, or identity
    /// exhaustion.
    pub fn try_arm(capacity: usize) -> Result<Self, RecorderArmError> {
        if capacity == 0 {
            return Err(RecorderArmError::ZeroCapacity);
        }
        let mut state = state().lock().unwrap_or_else(|error| error.into_inner());
        if state.active.is_some() {
            return Err(RecorderArmError::Busy);
        }
        let id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(RecorderArmError::IdentityExhausted)?;
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
        active.events.push(DestructionEvent {
            kind,
            address,
            result: None,
        });
    } else {
        active.overflowed = true;
    }
}

pub(crate) fn record_result(kind: DestructionKind, address: usize, result: i32) {
    let mut state = state().lock().unwrap_or_else(|error| error.into_inner());
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if let Some(event) = active
        .events
        .iter_mut()
        .rev()
        .find(|event| event.kind == kind && event.address == address && event.result.is_none())
    {
        event.result = Some(result);
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
        assert_eq!(
            DestructionRecorder::try_arm(1).unwrap_err(),
            RecorderArmError::Busy
        );
        drop(recorder);
        assert!(DestructionRecorder::try_arm(1).is_ok());
    }

    #[test]
    fn recorder_zero_capacity_is_not_reported_as_contention() {
        let error = DestructionRecorder::try_arm(0).unwrap_err();
        assert_eq!(error, RecorderArmError::ZeroCapacity);
        assert_eq!(
            error.to_string(),
            "destruction recorder capacity must be greater than zero"
        );
    }
}
