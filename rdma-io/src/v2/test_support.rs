//! V2-owned deterministic validation hooks.
//!
//! This non-default, doc-hidden namespace owns V2 lifecycle observations even
//! though private instrumentation is placed at shared resource destructor
//! call sites. It is not a V1 consumer API and owns no progress path.

pub use super::engine::{
    TestAcceptedOperation, TestAdmissionBarrier, TestConnectionCqeSuppression, TestContextIdentity,
    TestCqArmWindowControl, TestCqeSuppression, TestEngineInstrumentation, TestEngineQp,
    TestEngineResources, TestProviderLimits, TestRouteHandle,
};
pub use super::message_transport::{TestHelloOverride, TestSteadyFrame};
pub use crate::test_support::destruction::{
    DestructionEvent, DestructionKind, DestructionRecorder, RecorderArmError,
};
