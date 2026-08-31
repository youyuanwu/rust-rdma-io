//! Test-only destruction event recording.

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

fn events() -> &'static Mutex<Vec<DestructionEvent>> {
    static EVENTS: OnceLock<Mutex<Vec<DestructionEvent>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Remove all previously recorded events.
pub fn clear() {
    events()
        .lock()
        .expect("destruction recorder poisoned")
        .clear();
}

/// Return a copy of the currently recorded events.
pub fn snapshot() -> Vec<DestructionEvent> {
    events()
        .lock()
        .expect("destruction recorder poisoned")
        .clone()
}

/// Remove and return all currently recorded events.
pub fn take() -> Vec<DestructionEvent> {
    std::mem::take(&mut *events().lock().expect("destruction recorder poisoned"))
}

pub(crate) fn record(kind: DestructionKind, address: usize) {
    events()
        .lock()
        .expect("destruction recorder poisoned")
        .push(DestructionEvent { kind, address });
}
