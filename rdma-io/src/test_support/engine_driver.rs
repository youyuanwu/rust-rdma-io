//! Safe test-only access for shared-engine driver validation.

pub use crate::v2::engine::{
    TestAcceptedOperation, TestAdmissionBarrier, TestCompletionIdentity, TestConnectionIdentity,
    TestCqArmWindowControl, TestCqeSuppression, TestEngineInstrumentation, TestEngineQp,
    TestEngineResources, TestReadyWorkControl, TestRegistryProbe, TestRouteHandle,
    probe_connection_registry,
};
