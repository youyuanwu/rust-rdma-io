use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Duration;

use tokio::sync::Notify;

use crate::async_cm::AsyncCmId;
use crate::cm::CmId;
use crate::v2::engine::reactor::completion::CommandCompletion;
use crate::v2::engine::resources::TestResourceObservers;
use crate::v2::engine::session::SessionContext;
#[cfg(test)]
use crate::v2::engine::session::connection::TestConnectionProvider;
#[cfg(test)]
use crate::v2::engine::session::connection::install_connection;
use crate::v2::engine::session::connection::{
    ConnectionPoster, ConnectionTestAccess, VerbsConnectionResources,
    install_admitted_test_connection,
};
use crate::v2::engine::session::registry::ConnectionRegistry;
#[cfg(test)]
use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
use crate::v2::{
    AccessIntent, Completion, Mr, Qp, QpBuilder, RdmaConnection, RdmaConnectionConfig,
};
use crate::wc::{WcOpcode, WcStatus, WorkCompletion};
#[cfg(test)]
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

type TestConnectionInstallInput = (
    ConnectionPoster,
    RdmaConnectionConfig,
    Option<std::net::SocketAddr>,
    Option<std::net::SocketAddr>,
);

pub(in crate::v2::engine) struct TestConnectionInstallRequest {
    input: Mutex<Option<TestConnectionInstallInput>>,
    completion: CommandCompletion<RdmaConnection>,
}

impl TestConnectionInstallRequest {
    fn new(
        poster: impl Into<ConnectionPoster>,
        config: RdmaConnectionConfig,
        local_addr: Option<std::net::SocketAddr>,
        peer_addr: Option<std::net::SocketAddr>,
    ) -> Arc<Self> {
        Arc::new(Self {
            input: Mutex::new(Some((poster.into(), config, local_addr, peer_addr))),
            completion: CommandCompletion::new(),
        })
    }

    pub(in crate::v2::engine) fn execute_into(
        &self,
        manager: &SessionContext,
        connections: &mut ConnectionRegistry,
        reservation: crate::v2::engine::session::connection::ConnectionReservation,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let Some((poster, config, local_addr, peer_addr)) = lock_unpoison(&self.input).take()
        else {
            return;
        };
        let result = install_admitted_test_connection(
            manager,
            connections,
            poster,
            config,
            local_addr,
            peer_addr,
            reservation,
        );
        self.completion.complete_into(result, false, actions);
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn complete_failure(&self, error: Error) {
        self.completion.complete(Err(error));
    }

    pub(in crate::v2::engine) fn complete_failure_into(
        &self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.completion.complete_into(Err(error), false, actions);
    }

    async fn wait(&self) -> Result<RdmaConnection> {
        std::future::poll_fn(|cx| {
            if let Some(result) = self.completion.take_result() {
                return std::task::Poll::Ready(result);
            }
            self.completion.register(cx.waker());
            match self.completion.take_result() {
                Some(result) => std::task::Poll::Ready(result),
                None => std::task::Poll::Pending,
            }
        })
        .await
    }
}

use super::{EngineFrontendRoot, Error, Result};
use crate::v2::engine::io_core::CqeReject;
use crate::v2::engine::registry::{OperationToken, lock_unpoison};

/// Test-only connection identity used by the Phase 2 routing gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RouteIdentity {
    pub slot: u32,
    pub generation: u32,
    pub qp_num: u32,
}

/// Test-only accepted operation installed in the engine route table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestAcceptedOperation {
    pub wr_id: u64,
    pub expected_opcode: WcOpcode,
}

/// Exact class of a CQE rejected before it could mutate live ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestCqeRejection {
    StaleConnection,
    StaleOperation,
    RetiredOperation,
    Unknown,
    Duplicate,
    WrongConnection,
    WrongQpNum,
    UnexpectedOpcode,
}

impl From<CqeReject> for TestCqeRejection {
    fn from(value: CqeReject) -> Self {
        match value {
            CqeReject::StaleConnection => Self::StaleConnection,
            CqeReject::StaleOperation => Self::StaleOperation,
            CqeReject::RetiredOperation => Self::RetiredOperation,
            CqeReject::Unknown => Self::Unknown,
            CqeReject::Duplicate => Self::Duplicate,
            CqeReject::WrongConnection => Self::WrongConnection,
            CqeReject::WrongQpNum => Self::WrongQpNum,
            CqeReject::UnexpectedOpcode => Self::UnexpectedOpcode,
        }
    }
}

impl TestAcceptedOperation {
    pub fn new(wr_id: u64, expected_opcode: WcOpcode) -> Self {
        Self {
            wr_id,
            expected_opcode,
        }
    }
}

/// Minimal routing and CM-ownership observation for safety tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestEngineInstrumentation {
    /// CM requests and routes that still require driver progress.
    pub cm_pending_routes: usize,
    /// CM routes, listeners, or deferred destructions retaining ownership.
    pub cm_retained_owners: usize,
    /// CQEs rejected before they could affect a live operation.
    pub cqes_rejected: u64,
    /// CM events rejected before they could mutate a live route.
    pub cm_events_rejected: u64,
}

/// Safe test-only observer for shared resources and bounded driver fixtures.
///
/// It retains only weak provider identities; each fixture operation upgrades
/// them transiently while the reactor is active. It exposes no raw pointers,
/// file descriptors, or CQ consumer.
#[doc(hidden)]
#[derive(Clone)]
pub struct TestEngineResources {
    shared: Weak<EngineFrontendRoot>,
    observers: TestResourceObservers,
}

/// Opaque equality-only identity for the engine's anchored context.
pub struct TestContextIdentity {
    raw_context: usize,
}

/// Test-only identity of one engine's shared RDMA resource set.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TestSharedResourceIdentity {
    context: usize,
    protection_domain: usize,
    completion_queue: usize,
    cm_event_channel: usize,
    completion_channel: Option<usize>,
}

/// Read-only provider-capability projection for validation fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestProviderLimits {
    max_qp: usize,
    max_qp_wr: usize,
    max_sge: usize,
    max_cqe: usize,
    max_qp_rd_atom: usize,
    max_qp_init_rd_atom: usize,
}

/// Controller that pauses the driver after CQ notification arming and
/// before its mandatory post-arm CQ poll.
#[doc(hidden)]
pub struct TestCqArmWindowControl {
    shared: Weak<EngineFrontendRoot>,
    point: CqArmRacePoint,
    active: bool,
}

/// Controller for one exact production connection CQE held after polling.
#[doc(hidden)]
pub struct TestConnectionCqeSuppression {
    shared: Weak<EngineFrontendRoot>,
    connection: super::super::registry::ConnectionToken,
    qp_num: u32,
    active: bool,
}

/// Controller for one deterministic engine-admission shutdown race.
#[doc(hidden)]
pub struct TestAdmissionBarrier {
    shared: Weak<EngineFrontendRoot>,
    point: AdmissionPausePoint,
    active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum AdmissionPausePoint {
    ConnectBeforeEnqueue,
    OperationBeforeRegister,
}

#[derive(Clone, Copy)]
enum CqArmRacePoint {
    BeforeArm,
    AfterArm,
}

struct AdmissionBarrierState {
    control: Mutex<Option<AdmissionControl>>,
    changed: Condvar,
}

struct AdmissionControl {
    point: AdmissionPausePoint,
    paused: bool,
    shutdown_attempted: bool,
    released: bool,
}

/// Non-clonable test QP that must be consumed by route installation.
#[doc(hidden)]
pub struct TestEngineQp {
    qp: Qp,
}

#[cfg(test)]
struct TestIdlePoster {
    qp_num: u32,
}

#[cfg(test)]
impl TestConnectionProvider for TestIdlePoster {
    fn qp_num(&self) -> u32 {
        self.qp_num
    }

    fn capabilities(&self) -> Option<QpCapabilities> {
        None
    }

    fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        unreachable!("idle registry fixtures never post")
    }

    fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        unreachable!("idle registry fixtures never post")
    }

    fn to_error(&self) -> Result<()> {
        Ok(())
    }

    fn destroy_qp(&self) -> Result<bool> {
        Ok(true)
    }

    fn disconnect(&self) -> Result<()> {
        Ok(())
    }
}

fn raw_wc_opcode(opcode: WcOpcode) -> Result<u32> {
    match opcode {
        WcOpcode::Send => Ok(rdma_io_sys::ibverbs::IBV_WC_SEND),
        WcOpcode::RdmaWrite => Ok(rdma_io_sys::ibverbs::IBV_WC_RDMA_WRITE),
        WcOpcode::RdmaRead => Ok(rdma_io_sys::ibverbs::IBV_WC_RDMA_READ),
        WcOpcode::Recv => Ok(rdma_io_sys::ibverbs::IBV_WC_RECV),
        _ => Err(Error::InvalidConfig(
            "test completion opcode is not supported".into(),
        )),
    }
}

impl TestEngineResources {
    fn pd(&self) -> Result<crate::v2::Pd> {
        self.observers
            .pd
            .upgrade()
            .map(crate::v2::Pd::new)
            .ok_or(Error::DriverShutdown)
    }

    fn cq(&self) -> Result<Arc<crate::v2::Cq>> {
        self.observers.cq.upgrade().ok_or(Error::DriverShutdown)
    }

    fn context(&self) -> Result<Arc<crate::device::Context>> {
        self.observers
            .context
            .upgrade()
            .ok_or(Error::DriverShutdown)
    }

    fn cm_event_channel(&self) -> Result<Arc<crate::cm::EventChannel>> {
        self.observers
            .cm_event_channel
            .upgrade()
            .ok_or(Error::DriverShutdown)
    }

    fn require_owned_connection(
        &self,
        shared: &Arc<EngineFrontendRoot>,
        connection: &RdmaConnection,
    ) -> Result<Arc<ConnectionTestAccess>> {
        let state = connection.require_session_state()?;
        let Some(frontend) = connection.frontend().upgrade() else {
            return Err(Error::TransportClosed);
        };
        if Arc::ptr_eq(&frontend, &shared.session) {
            Ok(state)
        } else {
            Err(Error::InvalidConfig(
                "connection belongs to another engine".into(),
            ))
        }
    }

    pub(in crate::v2::engine) fn new(
        shared: &Arc<EngineFrontendRoot>,
        observers: TestResourceObservers,
    ) -> Self {
        Self {
            shared: Arc::downgrade(shared),
            observers,
        }
    }

    /// Verify that a CM route selected the engine's exact anchored context.
    pub fn require_context(&self, cm_id: &CmId) -> Result<()> {
        let context = self.context()?;
        cm_id.require_context(&context).map_err(Error::from_v1)
    }

    /// Return copied numeric provider limits without exposing validation internals.
    pub fn provider_limits(&self) -> Result<TestProviderLimits> {
        let shared = self.ensure_active()?;
        let limits = shared
            .session
            .provider_limits()
            .ok_or_else(|| Error::InvalidConfig("engine has no provider limits snapshot".into()))?;
        Ok(TestProviderLimits {
            max_qp: limits.max_qp,
            max_qp_wr: limits.max_qp_wr,
            max_sge: limits.max_sge,
            max_cqe: limits.max_cqe,
            max_qp_rd_atom: limits.max_qp_rd_atom,
            max_qp_init_rd_atom: limits.max_qp_init_rd_atom,
        })
    }

    /// Return an opaque equality-only identity for the anchored context.
    pub fn context_identity(&self) -> Result<TestContextIdentity> {
        self.ensure_active()?;
        Ok(TestContextIdentity {
            raw_context: self.context()?.as_raw() as usize,
        })
    }

    /// Return identities for the one shared Context/PD/CQ/CM resource set.
    pub fn shared_resource_identity(&self) -> Result<TestSharedResourceIdentity> {
        self.ensure_active()?;
        let context = self.context()?;
        let pd = self.pd()?;
        let cq = self.cq()?;
        let cm_event_channel = self.cm_event_channel()?;
        Ok(TestSharedResourceIdentity {
            context: context.as_raw() as usize,
            protection_domain: pd.raw_pd().as_raw() as usize,
            completion_queue: cq.raw_cq().as_raw() as usize,
            cm_event_channel: cm_event_channel.as_raw() as usize,
            completion_channel: cq
                .completion_channel()
                .map(|channel| channel.as_raw() as usize),
        })
    }

    /// Verify that a connection posts through this engine's shared PD/CQ.
    pub fn connection_uses_shared_resources(&self, connection: &RdmaConnection) -> Result<bool> {
        let shared = self.ensure_active()?;
        let state = match self.require_owned_connection(&shared, connection) {
            Ok(state) => state,
            Err(_) => return Ok(false),
        };
        let pd = self.pd()?;
        let cq = self.cq()?;
        Ok(state.uses_resources_for_test(&pd, &cq))
    }

    /// Verify that the connection's exact generational CM route is live.
    pub fn connection_route_is_live(&self, connection: &RdmaConnection) -> Result<bool> {
        let shared = self.ensure_active()?;
        let state = match self.require_owned_connection(&shared, connection) {
            Ok(state) => state,
            Err(_) => return Ok(false),
        };
        let route = connection
            .cm_route()
            .ok_or_else(|| Error::InvalidConfig("connection has no CM route".into()))?;
        Ok(matches!(
            route,
            super::super::session::connection::ConnectionCmRoute::Outbound(_)
                | super::super::session::connection::ConnectionCmRoute::Inbound(_)
        ) && state.token == connection.session_token()
            && !connection.close_state().is_retired())
    }

    /// Snapshot the exact rejection classes observed by CQE routing.
    pub fn cqe_rejections(&self) -> Result<Vec<TestCqeRejection>> {
        let shared = self.ensure_active()?;
        Ok(lock_unpoison(&shared.io_rejections)
            .iter()
            .copied()
            .map(TestCqeRejection::from)
            .collect())
    }

    /// Register owned test memory through the engine's shared PD.
    pub fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        self.ensure_active()?;
        self.pd()?.reg_mr(len, access)
    }

    /// Create a test QP against the engine's exact shared PD and CQ.
    pub fn create_qp(
        &self,
        cm_id: &CmId,
        max_send_wr: u32,
        max_recv_wr: u32,
    ) -> Result<TestEngineQp> {
        self.ensure_active()?;
        self.require_context(cm_id)?;
        let pd = self.pd()?;
        let cq = self.cq()?;
        Ok(TestEngineQp {
            qp: QpBuilder::new(&pd, &cq, &cq)
                .max_send_wr(max_send_wr)
                .max_recv_wr(max_recv_wr)
                .build_with_cm(cm_id)?,
        })
    }

    /// Install a verified test-only route and its exact accepted set.
    pub fn install_route(
        &self,
        qp: TestEngineQp,
        operations: impl IntoIterator<Item = TestAcceptedOperation>,
    ) -> Result<TestRouteHandle> {
        let shared = self.ensure_active()?;
        let pd = self.pd()?;
        let cq = self.cq()?;
        if !qp.qp.uses_resources(&pd, &cq) {
            return Err(Error::InvalidConfig(
                "test QP was not created from the leased engine PD/shared CQ".into(),
            ));
        }
        shared
            .test_driver
            .install(&shared, Arc::new(qp.qp), operations)
    }

    /// Convert a connected test QP into the real Phase 3 connection path.
    pub async fn install_connection(
        &self,
        qp: TestEngineQp,
        cm: AsyncCmId,
        config: RdmaConnectionConfig,
    ) -> Result<RdmaConnection> {
        let shared = self.ensure_active()?;
        let pd = self.pd()?;
        let cq = self.cq()?;
        if !qp.qp.uses_resources(&pd, &cq) {
            return Err(Error::InvalidConfig(
                "test QP was not created from the leased engine PD/shared CQ".into(),
            ));
        }
        let local_addr = cm.cm_id().local_addr();
        let peer_addr = cm.cm_id().peer_addr();
        let request = TestConnectionInstallRequest::new(
            VerbsConnectionResources::new(qp.qp, cm),
            config,
            local_addr,
            peer_addr,
        );
        let lane = shared
            .commands
            .acquire_connect()
            .await
            .ok_or_else(|| shared.admission_error().unwrap_or(Error::DriverShutdown))?;
        let permit = shared.commands.reserve_connect(lane).map_err(|error| {
            if matches!(error, Error::DriverShutdown) {
                shared.admission_error().unwrap_or(error)
            } else {
                error
            }
        })?;
        shared
            .commands
            .enqueue_test_connection_install(Arc::clone(&request), permit);
        shared.commands.notify_reactor();
        super::super::reactor::CommandIngress::yield_after_admission().await;
        request.wait().await
    }

    /// Explicitly transition an installed Phase 3 connection to QP ERR.
    pub fn transition_connection_to_error(&self, connection: &RdmaConnection) -> Result<()> {
        let shared = self.ensure_not_terminal()?;
        self.require_owned_connection(&shared, connection)?;
        shared
            .commands
            .request_connection_error(connection.session_token());
        Ok(())
    }

    /// Request an RDMA-CM disconnect for an outbound engine connection.
    pub fn disconnect_connection(&self, connection: &RdmaConnection) -> Result<()> {
        let shared = self.ensure_active()?;
        self.require_owned_connection(&shared, connection)?;
        shared
            .commands
            .request_connection_disconnect(connection.session_token());
        Ok(())
    }

    /// Make the next result-aware destruction of this connection's QP fail.
    pub fn fail_next_connection_qp_destroy(&self, connection: &RdmaConnection) -> Result<()> {
        let shared = self.ensure_active()?;
        self.require_owned_connection(&shared, connection)?;
        shared
            .commands
            .request_fail_next_qp_destroy(connection.session_token());
        Ok(())
    }

    /// Fail the next newly created connection installation and its QP rollback.
    ///
    /// A real MR is retained by the failed connection state so tests can
    /// prove rollback ownership is not released before the failed QP
    /// destruction boundary.
    pub fn fail_next_setup_rollback_qp_destroy(&self, error: Error) -> Result<()> {
        let shared = self.ensure_active()?;
        shared
            .test_driver
            .inject_setup_rollback_failure(error, || self.pd()?.reg_mr(64, AccessIntent::LocalOnly))
    }

    /// Terminate the real driver on its next poll with an exact test error.
    pub fn inject_driver_failure(&self, error: Error) -> Result<()> {
        let shared = self.ensure_active()?;
        shared.test_driver.inject_failure(error)?;
        shared.work_signal.notify_reactor();
        Ok(())
    }

    /// Pause the next CQ arm-to-post-poll window until released.
    ///
    /// Only one controller may be active for an engine. Dropping it
    /// cancels an unobserved request or releases the paused arm.
    pub fn pause_next_cq_arm_window(&self) -> Result<TestCqArmWindowControl> {
        let shared = self.ensure_active()?;
        shared
            .test_driver
            .start_cq_arm_control(CqArmRacePoint::AfterArm)?;
        Ok(TestCqArmWindowControl {
            shared: Arc::downgrade(&shared),
            point: CqArmRacePoint::AfterArm,
            active: true,
        })
    }

    /// Pause after the initial empty CQ poll and before notification arm.
    pub fn pause_next_cq_pre_arm_window(&self) -> Result<TestCqArmWindowControl> {
        let shared = self.ensure_active()?;
        shared
            .test_driver
            .start_cq_arm_control(CqArmRacePoint::BeforeArm)?;
        Ok(TestCqArmWindowControl {
            shared: Arc::downgrade(&shared),
            point: CqArmRacePoint::BeforeArm,
            active: true,
        })
    }

    /// Pause the next accepted connect after its shutdown check and before
    /// its request becomes visible to the driver.
    pub fn pause_next_connect_before_enqueue(&self) -> Result<TestAdmissionBarrier> {
        self.pause_next_admission(AdmissionPausePoint::ConnectBeforeEnqueue)
    }

    /// Pause the next accepted operation after its shutdown check and
    /// before operation registration and provider posting.
    pub fn pause_next_operation_before_register(&self) -> Result<TestAdmissionBarrier> {
        self.pause_next_admission(AdmissionPausePoint::OperationBeforeRegister)
    }

    /// Hold the next real CQE routed to `connection` after it is polled.
    ///
    /// The held CQE remains an actual provider completion and can later be
    /// released back through the normal exact production router.
    pub fn suppress_next_connection_cqe(
        &self,
        connection: &RdmaConnection,
    ) -> Result<TestConnectionCqeSuppression> {
        self.suppress_next_connection_cqe_matching(connection, None, false)
    }

    /// Hold the next real CQE with the requested opcode for `connection`.
    pub fn suppress_next_connection_cqe_with_opcode(
        &self,
        connection: &RdmaConnection,
        opcode: WcOpcode,
    ) -> Result<TestConnectionCqeSuppression> {
        self.suppress_next_connection_cqe_matching(connection, Some(opcode), false)
    }

    /// Hold the next real flush CQE for `connection` after polling.
    ///
    /// If the provider omits the flush CQE, the controller remains
    /// unobserved while the production QP-destruction fallback proceeds.
    pub fn suppress_next_connection_flush_cqe(
        &self,
        connection: &RdmaConnection,
    ) -> Result<TestConnectionCqeSuppression> {
        self.suppress_next_connection_cqe_matching(connection, None, true)
    }

    fn suppress_next_connection_cqe_matching(
        &self,
        connection: &RdmaConnection,
        expected_opcode: Option<WcOpcode>,
        require_flush: bool,
    ) -> Result<TestConnectionCqeSuppression> {
        let shared = self.ensure_active()?;
        let state = self.require_owned_connection(&shared, connection)?;
        shared.test_driver.start_connection_cqe_suppression(
            state.token,
            connection.identity().qp_num(),
            expected_opcode,
            require_flush,
        )?;
        Ok(TestConnectionCqeSuppression {
            shared: Arc::downgrade(&shared),
            connection: state.token,
            qp_num: connection.identity().qp_num(),
            active: true,
        })
    }

    /// Return the exact accepted WR IDs currently owned by `connection`.
    pub fn accepted_operation_wr_ids(&self, connection: &RdmaConnection) -> Result<Vec<u64>> {
        let shared = self.ensure_active()?;
        let state = self.require_owned_connection(&shared, connection)?;
        Ok(state
            .accepted_tokens()
            .into_iter()
            .map(OperationToken::encode)
            .collect())
    }

    /// Return the private registry slot and generation for a connection.
    pub fn connection_registry_identity(&self, connection: &RdmaConnection) -> Result<(u32, u32)> {
        let shared = self.ensure_active()?;
        self.require_owned_connection(&shared, connection)?;
        let identity = connection.identity();
        Ok((identity.registry_slot(), identity.registration_generation()))
    }

    /// Decode a test-observed WR ID into its operation slot and generation.
    pub fn operation_registry_identity(&self, wr_id: u64) -> Result<(u32, u32)> {
        self.ensure_active()?;
        let token = OperationToken::decode(wr_id);
        Ok((token.slot, token.generation))
    }

    /// Snapshot minimal CM ownership and rejected-CQE observations.
    pub fn instrumentation(&self) -> Result<TestEngineInstrumentation> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        Ok(shared.test_driver.instrumentation(&shared))
    }

    /// Inject a synthetic CQE through the production exact router.
    pub fn inject_completion(&self, wr_id: u64, qp_num: u32, opcode: WcOpcode) -> Result<()> {
        let shared = self.ensure_active()?;
        let mut completion = WorkCompletion::default();
        completion.inner.wr_id = wr_id;
        completion.inner.qp_num = qp_num;
        completion.inner.status = rdma_io_sys::ibverbs::IBV_WC_SUCCESS;
        completion.inner.opcode = raw_wc_opcode(opcode)?;
        shared.test_driver.queue_released_connection_cqe(completion);
        shared.work_signal.notify_reactor();
        Ok(())
    }

    fn pause_next_admission(&self, point: AdmissionPausePoint) -> Result<TestAdmissionBarrier> {
        let shared = self.ensure_active()?;
        shared.test_driver.start_admission_control(point)?;
        Ok(TestAdmissionBarrier {
            shared: Arc::downgrade(&shared),
            point,
            active: true,
        })
    }

    fn ensure_active(&self) -> Result<Arc<EngineFrontendRoot>> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        if shared.outcome().is_some() || shared.commands.is_closed() {
            return Err(Error::DriverShutdown);
        }
        Ok(shared)
    }

    fn ensure_not_terminal(&self) -> Result<Arc<EngineFrontendRoot>> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        if shared.outcome().is_some() {
            return Err(Error::DriverShutdown);
        }
        Ok(shared)
    }
}

impl TestContextIdentity {
    /// Compare with one independently verbs-opened context of the same device.
    pub fn matches_independently_opened(&self, device_name: &str) -> Result<bool> {
        let independent =
            crate::device::open_device_by_name(device_name).map_err(Error::from_v1)?;
        let matches = independent.as_raw() as usize == self.raw_context;
        drop(independent);
        Ok(matches)
    }
}

impl TestProviderLimits {
    pub fn max_qp(&self) -> usize {
        self.max_qp
    }

    pub fn max_qp_wr(&self) -> usize {
        self.max_qp_wr
    }

    pub fn max_sge(&self) -> usize {
        self.max_sge
    }

    pub fn max_cqe(&self) -> usize {
        self.max_cqe
    }

    pub fn max_qp_rd_atom(&self) -> usize {
        self.max_qp_rd_atom
    }

    pub fn max_qp_init_rd_atom(&self) -> usize {
        self.max_qp_init_rd_atom
    }
}

impl TestAdmissionBarrier {
    /// Wait until the accepted request is paused while holding admission.
    pub fn wait_until_paused(&self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        shared.test_driver.wait_for_admission(self.point, false)
    }

    /// Wait until shutdown has reached the admission write barrier.
    pub fn wait_until_shutdown_attempted(&self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        shared.test_driver.wait_for_admission(self.point, true)
    }

    /// Release the accepted request to publish or post before shutdown.
    pub fn release(mut self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        shared.test_driver.release_admission(self.point)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TestAdmissionBarrier {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(shared) = self.shared.upgrade() {
            shared.test_driver.stop_admission_control(self.point);
        }
        self.active = false;
    }
}

impl TestCqArmWindowControl {
    /// Wait until the driver is paused in an arm-to-post-poll window.
    pub async fn wait_for_pause_after(&self, previous: u64) -> Result<u64> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        loop {
            let notified = shared.test_driver.cq_arm_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let current = shared.test_driver.paused_generation(self.point);
            if current > previous {
                return Ok(current);
            }
            if shared.outcome().is_some() {
                return Err(Error::DriverShutdown);
            }
            notified.await;
        }
    }

    /// Resume the exact arm generation after the test posts its CQE.
    pub fn release(mut self, generation: u64) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        shared.test_driver.release_cq_arm(self.point, generation)?;
        shared.work_signal.notify_reactor();
        self.active = false;
        Ok(())
    }
}

impl Drop for TestCqArmWindowControl {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(shared) = self.shared.upgrade() {
            shared.test_driver.stop_cq_arm_control(self.point);
            shared.work_signal.notify_reactor();
        }
        self.active = false;
    }
}

impl TestConnectionCqeSuppression {
    /// Wait until the engine has polled and held the matching real CQE.
    pub async fn wait_observed(&self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        loop {
            let notified = shared.test_driver.connection_cqe_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if shared
                .test_driver
                .connection_cqe_is_observed(self.connection, self.qp_num)
            {
                return Ok(());
            }
            if shared.outcome().is_some() {
                return Err(Error::DriverShutdown);
            }
            notified.await;
        }
    }

    /// Return the held real CQE as a typed completion.
    pub fn completion(&self) -> Result<Completion> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        shared
            .test_driver
            .connection_cqe(self.connection, self.qp_num)
    }

    /// Release the held real CQE through normal exact production routing.
    pub fn release(mut self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
        shared
            .test_driver
            .release_connection_cqe(self.connection, self.qp_num)?;
        shared.work_signal.notify_reactor();
        self.active = false;
        Ok(())
    }
}

impl Drop for TestConnectionCqeSuppression {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(shared) = self.shared.upgrade() {
            shared
                .test_driver
                .abandon_connection_cqe(self.connection, self.qp_num);
        }
        self.active = false;
    }
}

/// Take-once handle for one test-only route.
#[doc(hidden)]
pub struct TestRouteHandle {
    shared: Weak<EngineFrontendRoot>,
    route: Arc<TestRouteState>,
    removed: bool,
}

impl TestRouteHandle {
    pub fn qp_num(&self) -> u32 {
        self.route.identity.qp_num
    }

    pub fn qp(&self) -> &Qp {
        &self.route.qp
    }

    pub fn accepted_outstanding(&self) -> usize {
        self.route.remaining()
    }

    /// Retain a posted MR, CM owner, or other dependency with this route.
    pub fn retain<T: Send + 'static>(&self, value: T) {
        self.route
            .retained
            .lock()
            .expect("test route retained resources poisoned")
            .push(Box::new(value));
    }

    /// Retain a resource only until the exact operation CQE is consumed.
    pub fn retain_until_completion<T: Send + 'static>(&self, wr_id: u64, value: T) {
        self.route
            .operation_retained
            .lock()
            .expect("test route operation resources poisoned")
            .insert(wr_id, Box::new(value));
    }

    pub async fn wait_until_drained(&self) {
        loop {
            let notified = self.route.drained.notified();
            if self.route.remaining() == 0 {
                return;
            }
            notified.await;
        }
    }

    pub fn completions(&self) -> Vec<Completion> {
        self.route
            .completions
            .lock()
            .expect("test route completions poisoned")
            .iter()
            .copied()
            .map(Completion::from_raw)
            .collect()
    }

    pub async fn wait_for_completion_count(&self, expected: usize) {
        loop {
            let notified = self.route.drained.notified();
            if self
                .route
                .completions
                .lock()
                .expect("test route completions poisoned")
                .len()
                >= expected
            {
                return;
            }
            notified.await;
        }
    }

    /// Arm deterministic suppression of the next exact CQE for `wr_id`.
    pub fn suppress_next(&self, wr_id: u64) -> Result<TestCqeSuppression> {
        self.route.arm_suppression(wr_id)?;
        Ok(TestCqeSuppression {
            shared: self.shared.clone(),
            route: Arc::clone(&self.route),
            wr_id,
        })
    }

    /// Remove this route after its exact accepted set reaches zero.
    pub fn remove(mut self) -> Result<Vec<Completion>> {
        if self.route.remaining() != 0 {
            return Err(Error::InvalidConfig(
                "cannot remove a test route with accepted WRs outstanding".into(),
            ));
        }
        if let Some(shared) = self.shared.upgrade() {
            shared
                .test_driver
                .remove(self.route.identity.qp_num, &self.route);
        }
        self.removed = true;
        Ok(self.completions())
    }
}

impl Drop for TestRouteHandle {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        self.route.detached.store(true, Ordering::Release);
        if let Some(shared) = self.shared.upgrade() {
            if self.route.remaining() == 0 {
                shared
                    .test_driver
                    .remove(self.route.identity.qp_num, &self.route);
            }
        } else if self.route.remaining() != 0 {
            quarantine_routes()
                .lock()
                .expect("test route quarantine poisoned")
                .push(Arc::clone(&self.route));
        }
    }
}

/// Armed deterministic CQE suppression fixture.
#[doc(hidden)]
pub struct TestCqeSuppression {
    shared: Weak<EngineFrontendRoot>,
    route: Arc<TestRouteState>,
    wr_id: u64,
}

impl TestCqeSuppression {
    pub async fn wait_observed(&self) {
        loop {
            let notified = self.route.suppression_observed.notified();
            if self
                .route
                .suppressed_completions
                .lock()
                .expect("test route suppressed completions poisoned")
                .contains_key(&self.wr_id)
            {
                return;
            }
            notified.await;
        }
    }

    /// Release the recorded CQE back into normal exact-route processing.
    pub fn release(self) -> Result<()> {
        let completion = self
            .route
            .suppressed_completions
            .lock()
            .expect("test route suppressed completions poisoned")
            .remove(&self.wr_id)
            .ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "suppressed operation token {} has not been observed",
                    self.wr_id
                ))
            })?;
        let drained = self.route.accept_completion(completion);
        if drained
            && self.route.detached.load(Ordering::Acquire)
            && let Some(shared) = self.shared.upgrade()
        {
            shared
                .test_driver
                .remove(self.route.identity.qp_num, &self.route);
        }
        Ok(())
    }
}

pub(in crate::v2::engine) struct TestDriverState {
    routes: Mutex<HashMap<u32, Arc<TestRouteState>>>,
    next_slot: AtomicU32,
    #[cfg(test)]
    next_idle_qp: AtomicU32,
    cq_arms: AtomicU64,
    cq_arm_controller_active: AtomicBool,
    cq_pre_arm_controlled: AtomicBool,
    cq_pre_arm_paused: AtomicU64,
    cq_arm_controlled: AtomicBool,
    cq_arm_paused: AtomicU64,
    cq_arm_notify: Notify,
    admission_barrier: AdmissionBarrierState,
    connection_cqe_suppression: Mutex<Option<ConnectionCqeSuppressionState>>,
    released_connection_cqes: Mutex<VecDeque<WorkCompletion>>,
    connection_cqe_notify: Notify,
    injected_failure: Mutex<Option<Error>>,
    setup_rollback_failure: Mutex<Option<SetupRollbackFailure>>,
}

struct ConnectionCqeSuppressionState {
    connection: super::super::registry::ConnectionToken,
    qp_num: u32,
    expected_opcode: Option<WcOpcode>,
    require_flush: bool,
    completion: Option<WorkCompletion>,
    abandoned: bool,
}

pub(in crate::v2::engine) struct SetupRollbackFailure {
    pub(in crate::v2::engine) error: Error,
    pub(in crate::v2::engine) retained_mr: Mr,
}

impl TestDriverState {
    pub(in crate::v2::engine) fn new() -> Self {
        Self {
            routes: Mutex::new(HashMap::new()),
            next_slot: AtomicU32::new(0),
            #[cfg(test)]
            next_idle_qp: AtomicU32::new(0x7000_0000),
            cq_arms: AtomicU64::new(0),
            cq_arm_controller_active: AtomicBool::new(false),
            cq_pre_arm_controlled: AtomicBool::new(false),
            cq_pre_arm_paused: AtomicU64::new(0),
            cq_arm_controlled: AtomicBool::new(false),
            cq_arm_paused: AtomicU64::new(0),
            cq_arm_notify: Notify::new(),
            admission_barrier: AdmissionBarrierState {
                control: Mutex::new(None),
                changed: Condvar::new(),
            },
            connection_cqe_suppression: Mutex::new(None),
            released_connection_cqes: Mutex::new(VecDeque::new()),
            connection_cqe_notify: Notify::new(),
            injected_failure: Mutex::new(None),
            setup_rollback_failure: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn next_idle_qp(&self) -> Result<u32> {
        self.next_idle_qp
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| Error::CapacityExhausted)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn install_idle_connections(
        &self,
        session: &mut super::super::session::SessionReactorSources,
        count: usize,
    ) -> Result<Vec<RdmaConnection>> {
        let mut connections = Vec::new();
        connections
            .try_reserve_exact(count)
            .map_err(|_| Error::CapacityExhausted)?;
        for _ in 0..count {
            let qp_num = self.next_idle_qp()?;
            connections.push(install_connection(
                &session.manager,
                &mut session.connections,
                Arc::new(TestIdlePoster { qp_num }),
                RdmaConnectionConfig::default(),
                None,
                None,
            )?);
        }
        Ok(connections)
    }

    fn instrumentation(&self, shared: &EngineFrontendRoot) -> TestEngineInstrumentation {
        let diagnostics = lock_unpoison(&shared.diagnostics).clone();
        let cqes_rejected = lock_unpoison(&shared.io_rejections).len() as u64;
        TestEngineInstrumentation {
            cm_pending_routes: diagnostics.engine.live_connections + diagnostics.cm_pending_routes,
            cm_retained_owners: diagnostics
                .engine
                .live_connections
                .max(diagnostics.cm_retained_owners),
            cqes_rejected,
            cm_events_rejected: shared.cm_rejections.load(Ordering::Acquire),
        }
    }

    fn inject_failure(&self, error: Error) -> Result<()> {
        let mut pending = self
            .injected_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.is_some() {
            return Err(Error::InvalidConfig(
                "a driver failure is already pending".into(),
            ));
        }
        *pending = Some(error);
        Ok(())
    }

    pub(super) fn take_injected_failure(&self) -> Option<Error> {
        self.injected_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn inject_setup_rollback_failure(
        &self,
        error: Error,
        register_mr: impl FnOnce() -> Result<Mr>,
    ) -> Result<()> {
        let mut pending = self
            .setup_rollback_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.is_some() {
            return Err(Error::InvalidConfig(
                "a setup rollback failure is already pending".into(),
            ));
        }
        let retained_mr = register_mr()?;
        *pending = Some(SetupRollbackFailure { error, retained_mr });
        Ok(())
    }

    pub(in crate::v2::engine) fn take_setup_rollback_failure(
        &self,
    ) -> Option<SetupRollbackFailure> {
        self.setup_rollback_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn start_connection_cqe_suppression(
        &self,
        connection: super::super::registry::ConnectionToken,
        qp_num: u32,
        expected_opcode: Option<WcOpcode>,
        require_flush: bool,
    ) -> Result<()> {
        let mut control = self
            .connection_cqe_suppression
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if control.is_some() {
            return Err(Error::InvalidConfig(
                "connection CQE suppression is already active".into(),
            ));
        }
        *control = Some(ConnectionCqeSuppressionState {
            connection,
            qp_num,
            expected_opcode,
            require_flush,
            completion: None,
            abandoned: false,
        });
        Ok(())
    }

    pub(in crate::v2::engine) fn suppress_connection_cqe(
        &self,
        completion: WorkCompletion,
    ) -> bool {
        let mut guard = self
            .connection_cqe_suppression
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(control) = guard.as_mut() else {
            return false;
        };
        if control.completion.is_some()
            || control.qp_num != completion.qp_num()
            || control
                .expected_opcode
                .is_some_and(|opcode| opcode != completion.opcode())
            || (control.require_flush && completion.status() != WcStatus::WrFlushErr)
            || control.abandoned
        {
            return false;
        }
        control.completion = Some(completion);
        drop(guard);
        self.connection_cqe_notify.notify_waiters();
        true
    }

    fn connection_cqe_is_observed(
        &self,
        connection: super::super::registry::ConnectionToken,
        qp_num: u32,
    ) -> bool {
        self.connection_cqe_suppression
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|control| {
                control.connection == connection
                    && control.qp_num == qp_num
                    && control.completion.is_some()
            })
    }

    fn connection_cqe(
        &self,
        connection: super::super::registry::ConnectionToken,
        qp_num: u32,
    ) -> Result<Completion> {
        let control = self
            .connection_cqe_suppression
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let active = control.as_ref().ok_or_else(|| {
            Error::InvalidConfig("connection CQE suppression is not active".into())
        })?;
        if active.connection != connection || active.qp_num != qp_num {
            return Err(Error::InvalidConfig(
                "connection CQE suppression identity changed".into(),
            ));
        }
        let completion = active
            .completion
            .ok_or_else(|| Error::InvalidConfig("connection CQE has not been observed".into()))?;
        Ok(Completion::from_raw(completion))
    }

    fn release_connection_cqe(
        &self,
        connection: super::super::registry::ConnectionToken,
        qp_num: u32,
    ) -> Result<()> {
        let mut control = self
            .connection_cqe_suppression
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(active) = control.as_ref() else {
            return Err(Error::InvalidConfig(
                "connection CQE suppression is not active".into(),
            ));
        };
        if active.connection != connection || active.qp_num != qp_num {
            return Err(Error::InvalidConfig(
                "connection CQE suppression identity changed".into(),
            ));
        }
        let completion = active
            .completion
            .ok_or_else(|| Error::InvalidConfig("connection CQE has not been observed".into()))?;
        *control = None;
        drop(control);
        self.released_connection_cqes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(completion);
        Ok(())
    }

    fn abandon_connection_cqe(
        &self,
        connection: super::super::registry::ConnectionToken,
        qp_num: u32,
    ) {
        if let Some(control) = self
            .connection_cqe_suppression
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            && control.connection == connection
            && control.qp_num == qp_num
        {
            control.abandoned = true;
        }
    }

    pub(in crate::v2::engine) fn take_released_connection_cqe(&self) -> Option<WorkCompletion> {
        self.released_connection_cqes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
    }

    pub(in crate::v2::engine) fn queue_released_connection_cqe(&self, completion: WorkCompletion) {
        self.released_connection_cqes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(completion);
    }

    pub(in crate::v2::engine) fn pause_admission(&self, point: AdmissionPausePoint) {
        let mut control = self
            .admission_barrier
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(active) = control.as_mut() else {
            return;
        };
        if active.point != point || active.released {
            return;
        }
        active.paused = true;
        self.admission_barrier.changed.notify_all();
        while control
            .as_ref()
            .is_some_and(|active| active.point == point && !active.released)
        {
            control = self
                .admission_barrier
                .changed
                .wait(control)
                .unwrap_or_else(|error| error.into_inner());
        }
        if control
            .as_ref()
            .is_some_and(|active| active.point == point && active.released)
        {
            *control = None;
            self.admission_barrier.changed.notify_all();
        }
    }

    pub(in crate::v2::engine) fn record_shutdown_attempt(&self) {
        let mut control = self
            .admission_barrier
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(active) = control.as_mut() else {
            return;
        };
        if active.paused && !active.released {
            active.shutdown_attempted = true;
            self.admission_barrier.changed.notify_all();
        }
    }

    fn start_admission_control(&self, point: AdmissionPausePoint) -> Result<()> {
        let mut control = self
            .admission_barrier
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if control.is_some() {
            return Err(Error::InvalidConfig(
                "engine admission barrier is already active".into(),
            ));
        }
        *control = Some(AdmissionControl {
            point,
            paused: false,
            shutdown_attempted: false,
            released: false,
        });
        Ok(())
    }

    fn wait_for_admission(
        &self,
        point: AdmissionPausePoint,
        shutdown_attempted: bool,
    ) -> Result<()> {
        let mut control = self
            .admission_barrier
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loop {
            match control.as_ref() {
                Some(active)
                    if active.point == point
                        && active.paused
                        && (!shutdown_attempted || active.shutdown_attempted) =>
                {
                    return Ok(());
                }
                Some(active) if active.point != point || active.released => {
                    return Err(Error::InvalidConfig(
                        "engine admission barrier changed before observation".into(),
                    ));
                }
                None => {
                    return Err(Error::InvalidConfig(
                        "engine admission barrier is not active".into(),
                    ));
                }
                Some(_) => {}
            }
            let (next, timeout) = self
                .admission_barrier
                .changed
                .wait_timeout(control, Duration::from_secs(10))
                .unwrap_or_else(|error| error.into_inner());
            control = next;
            if timeout.timed_out() {
                return Err(Error::InvalidConfig(
                    "timed out waiting for engine admission barrier".into(),
                ));
            }
        }
    }

    fn release_admission(&self, point: AdmissionPausePoint) -> Result<()> {
        let mut control = self
            .admission_barrier
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(active) = control.as_mut() else {
            return Err(Error::InvalidConfig(
                "engine admission barrier is not active".into(),
            ));
        };
        if active.point != point || !active.paused {
            return Err(Error::InvalidConfig(
                "engine admission barrier is not paused at the requested point".into(),
            ));
        }
        active.released = true;
        self.admission_barrier.changed.notify_all();
        Ok(())
    }

    fn stop_admission_control(&self, point: AdmissionPausePoint) {
        let mut control = self
            .admission_barrier
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(active) = control.as_mut()
            && active.point == point
        {
            active.released = true;
            self.admission_barrier.changed.notify_all();
            if !active.paused {
                *control = None;
            }
        }
    }

    fn install(
        &self,
        shared: &Arc<EngineFrontendRoot>,
        qp: Arc<Qp>,
        operations: impl IntoIterator<Item = TestAcceptedOperation>,
    ) -> Result<TestRouteHandle> {
        let qp_num = qp.qp_num();
        if qp_num == 0 {
            return Err(Error::InvalidConfig(
                "provider returned zero qp_num for test route".into(),
            ));
        }
        let mut accepted = HashMap::new();
        for operation in operations {
            if accepted
                .insert(operation.wr_id, operation.expected_opcode)
                .is_some()
            {
                return Err(Error::InvalidConfig(format!(
                    "duplicate test operation token {}",
                    operation.wr_id
                )));
            }
        }
        let slot = self
            .next_slot
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |slot| {
                slot.checked_add(1)
            })
            .map_err(|_| Error::CapacityExhausted)?;
        let route = Arc::new(TestRouteState {
            identity: RouteIdentity {
                slot,
                generation: 1,
                qp_num,
            },
            qp,
            retained: Mutex::new(Vec::new()),
            operation_retained: Mutex::new(HashMap::new()),
            accepted: Mutex::new(accepted),
            completions: Mutex::new(Vec::new()),
            suppressed: Mutex::new(HashSet::new()),
            suppressed_completions: Mutex::new(HashMap::new()),
            drained: Notify::new(),
            suppression_observed: Notify::new(),
            detached: AtomicBool::new(false),
        });
        let mut routes = self.routes.lock().expect("test route table poisoned");
        if routes.contains_key(&qp_num) {
            return Err(Error::InvalidConfig(format!(
                "test route already installed for qp_num {qp_num}"
            )));
        }
        routes.insert(qp_num, Arc::clone(&route));
        drop(routes);
        shared.work_signal.notify_reactor();
        Ok(TestRouteHandle {
            shared: Arc::downgrade(shared),
            route,
            removed: false,
        })
    }

    pub(in crate::v2::engine) fn dispatch(&self, completion: WorkCompletion) {
        let route = self
            .routes
            .lock()
            .expect("test route table poisoned")
            .get(&completion.qp_num())
            .cloned();
        let Some(route) = route else {
            return;
        };
        let drained = route.complete(completion);
        if drained && route.detached.load(Ordering::Acquire) {
            self.remove(completion.qp_num(), &route);
        }
    }

    fn remove(&self, qp_num: u32, route: &Arc<TestRouteState>) {
        let mut routes = self.routes.lock().expect("test route table poisoned");
        if routes
            .get(&qp_num)
            .is_some_and(|current| Arc::ptr_eq(current, route))
        {
            routes.remove(&qp_num);
        }
    }

    pub(in crate::v2::engine) fn record_cq_arm(&self, generation: u64) -> bool {
        let previous = self.cq_arms.swap(generation, Ordering::AcqRel);
        debug_assert!(generation > previous, "CQ arm generations must increase");
        self.cq_arm_notify.notify_waiters();
        if !self.cq_arm_controlled.swap(false, Ordering::AcqRel) {
            return false;
        }
        if self
            .cq_arm_paused
            .compare_exchange(0, generation, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            debug_assert!(false, "a CQ arm was already paused");
            return false;
        }
        self.cq_arm_notify.notify_waiters();
        true
    }

    pub(in crate::v2::engine) fn record_cq_pre_arm(&self, generation: u64) -> bool {
        if !self.cq_pre_arm_controlled.swap(false, Ordering::AcqRel) {
            return false;
        }
        if self
            .cq_pre_arm_paused
            .compare_exchange(0, generation, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            debug_assert!(false, "a CQ pre-arm window was already paused");
            return false;
        }
        self.cq_arm_notify.notify_waiters();
        true
    }

    fn start_cq_arm_control(&self, point: CqArmRacePoint) -> Result<()> {
        self.cq_arm_controller_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::InvalidConfig("CQ arm-window control is already active".into()))?;
        match point {
            CqArmRacePoint::BeforeArm => {
                self.cq_pre_arm_controlled.store(true, Ordering::Release);
            }
            CqArmRacePoint::AfterArm => {
                self.cq_arm_controlled.store(true, Ordering::Release);
            }
        }
        Ok(())
    }

    fn paused_generation(&self, point: CqArmRacePoint) -> u64 {
        match point {
            CqArmRacePoint::BeforeArm => self.cq_pre_arm_paused.load(Ordering::Acquire),
            CqArmRacePoint::AfterArm => self.cq_arm_paused.load(Ordering::Acquire),
        }
    }

    fn release_cq_arm(&self, point: CqArmRacePoint, generation: u64) -> Result<()> {
        let paused = match point {
            CqArmRacePoint::BeforeArm => &self.cq_pre_arm_paused,
            CqArmRacePoint::AfterArm => &self.cq_arm_paused,
        };
        paused
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|observed| {
                Error::InvalidConfig(format!(
                    "CQ arm generation {generation} is not paused (observed {observed})"
                ))
            })?;
        self.cq_arm_controller_active
            .store(false, Ordering::Release);
        Ok(())
    }

    fn stop_cq_arm_control(&self, point: CqArmRacePoint) {
        match point {
            CqArmRacePoint::BeforeArm => {
                self.cq_pre_arm_controlled.store(false, Ordering::Release);
                self.cq_pre_arm_paused.store(0, Ordering::Release);
            }
            CqArmRacePoint::AfterArm => {
                self.cq_arm_controlled.store(false, Ordering::Release);
                self.cq_arm_paused.store(0, Ordering::Release);
            }
        }
        self.cq_arm_controller_active
            .store(false, Ordering::Release);
        self.cq_arm_notify.notify_waiters();
    }

    pub(super) fn retain_unresolved(&self) {
        let unresolved: Vec<_> = self
            .routes
            .lock()
            .expect("test route table poisoned")
            .values()
            .filter(|route| route.remaining() != 0)
            .cloned()
            .collect();
        if unresolved.is_empty() {
            return;
        }
        quarantine_routes()
            .lock()
            .expect("test route quarantine poisoned")
            .extend(unresolved);
    }
}

struct TestRouteState {
    identity: RouteIdentity,
    qp: Arc<Qp>,
    retained: Mutex<Vec<Box<dyn Any + Send>>>,
    operation_retained: Mutex<HashMap<u64, Box<dyn Any + Send>>>,
    accepted: Mutex<HashMap<u64, WcOpcode>>,
    completions: Mutex<Vec<WorkCompletion>>,
    suppressed: Mutex<HashSet<u64>>,
    suppressed_completions: Mutex<HashMap<u64, WorkCompletion>>,
    drained: Notify,
    suppression_observed: Notify,
    detached: AtomicBool,
}

impl TestRouteState {
    fn remaining(&self) -> usize {
        self.accepted
            .lock()
            .expect("test route accepted set poisoned")
            .len()
    }

    fn arm_suppression(&self, wr_id: u64) -> Result<()> {
        if !self
            .accepted
            .lock()
            .expect("test route accepted set poisoned")
            .contains_key(&wr_id)
        {
            return Err(Error::InvalidConfig(format!(
                "cannot suppress unknown operation token {wr_id}"
            )));
        }
        let mut suppressed = self
            .suppressed
            .lock()
            .expect("test route suppression set poisoned");
        if !suppressed.insert(wr_id) {
            return Err(Error::InvalidConfig(format!(
                "operation token {wr_id} is already armed for suppression"
            )));
        }
        Ok(())
    }

    fn complete(&self, completion: WorkCompletion) -> bool {
        let wr_id = completion.wr_id();
        let suppressed = self
            .suppressed
            .lock()
            .expect("test route suppression set poisoned")
            .remove(&wr_id);
        if suppressed {
            self.suppressed_completions
                .lock()
                .expect("test route suppressed completions poisoned")
                .insert(wr_id, completion);
            self.suppression_observed.notify_waiters();
            return false;
        }

        self.accept_completion(completion)
    }

    fn accept_completion(&self, completion: WorkCompletion) -> bool {
        let wr_id = completion.wr_id();
        let removed = {
            let mut accepted = self
                .accepted
                .lock()
                .expect("test route accepted set poisoned");
            match accepted.get(&wr_id) {
                Some(expected)
                    // Providers may leave the opcode field unspecified on
                    // error/flush CQEs; exact token and QP identity remain
                    // mandatory, while successful CQEs also match opcode.
                    if !completion.is_success() || *expected == completion.opcode() =>
                {
                    accepted.remove(&wr_id);
                    true
                }
                _ => false,
            }
        };
        if !removed {
            return false;
        }
        self.completions
            .lock()
            .expect("test route completions poisoned")
            .push(completion);
        let retained = self
            .operation_retained
            .lock()
            .expect("test route operation resources poisoned")
            .remove(&wr_id);
        drop(retained);
        let drained = self.remaining() == 0;
        self.drained.notify_waiters();
        drained
    }
}

fn quarantine_routes() -> &'static Mutex<Vec<Arc<TestRouteState>>> {
    static ROUTES: OnceLock<Mutex<Vec<Arc<TestRouteState>>>> = OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(Vec::new()))
}

impl Drop for EngineFrontendRoot {
    fn drop(&mut self) {
        self.test_driver.retain_unresolved();
    }
}
