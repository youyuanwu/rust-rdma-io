use rdma_io::v2::test_support::{
    DestructionEvent, DestructionKind, DestructionRecorder, RecorderArmError,
    TestAcceptedOperation, TestAdmissionBarrier, TestConnectionCqeSuppression,
    TestContextIdentity, TestCqArmWindowControl, TestCqeSuppression, TestEngineInstrumentation,
    TestEngineQp, TestEngineResources, TestHelloAttachHook, TestHelloOverride, TestProviderLimits,
    TestReadyWorkControl, TestRouteHandle, TestSteadyFrame,
};
use rdma_io::v2::Result;
use rdma_io::cm::CmId;

fn hook_signatures(resources: &TestEngineResources, cm_id: &CmId) -> Result<()> {
    resources.require_context(cm_id)?;
    let limits = resources.provider_limits()?;
    let _ = (
        limits.max_qp(),
        limits.max_qp_wr(),
        limits.max_sge(),
        limits.max_cqe(),
        limits.max_qp_rd_atom(),
        limits.max_qp_init_rd_atom(),
    );
    let identity = resources.context_identity()?;
    let _ = identity.matches_independently_opened("rxe0")?;
    Ok(())
}

fn main() {
    let _: Option<DestructionEvent> = None;
    let _: Option<DestructionKind> = None;
    let _: Option<DestructionRecorder> = None;
    let _: Option<RecorderArmError> = None;
    let _: Option<TestAcceptedOperation> = None;
    let _: Option<TestAdmissionBarrier> = None;
    let _: Option<TestConnectionCqeSuppression> = None;
    let _: Option<TestContextIdentity> = None;
    let _: Option<TestCqArmWindowControl> = None;
    let _: Option<TestCqeSuppression> = None;
    let _: Option<TestEngineInstrumentation> = None;
    let _: Option<TestEngineQp> = None;
    let _: Option<TestEngineResources> = None;
    let _: Option<TestHelloAttachHook> = None;
    let _: Option<TestHelloOverride> = None;
    let _: Option<TestProviderLimits> = None;
    let _: Option<TestReadyWorkControl> = None;
    let _: Option<TestRouteHandle> = None;
    let _: Option<TestSteadyFrame> = None;
    let _: fn(usize) -> std::result::Result<DestructionRecorder, RecorderArmError> =
        DestructionRecorder::try_arm;
    let _ = hook_signatures;
}
