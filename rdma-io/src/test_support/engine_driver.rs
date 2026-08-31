//! Safe test-only access for shared-engine driver validation.

pub use crate::v2::engine::{
    TestAcceptedOperation, TestAdmissionBarrier, TestConnectionIdentity, TestCqArmWindowControl,
    TestCqeSuppression, TestEngineQp, TestEngineResources, TestReadyWorkControl, TestRouteHandle,
};
