//! Crate-private owned submission and event boundary for protocol drivers.

use std::any::Any;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use futures_util::task::AtomicWaker;
use tokio::sync::OwnedSemaphorePermit;

#[cfg(test)]
use super::EngineFrontendRoot;
use super::io_core::{self, ConnectionIoState, EstablishedIoConnection, IoState};
use super::reactor::ReactorActions;
use super::registry::{OperationToken, lock_unpoison};
use super::session::connection::ConnectionState;
use super::session::connection::RdmaConnection;
use crate::v2::error::{Error, Result};
use crate::v2::mr::{AccessIntent, Mr};
use crate::v2::op::Completion;

#[derive(Clone)]
pub(super) struct MemoryRegistrar {
    pd: Option<Weak<crate::pd::ProtectionDomain>>,
}

impl MemoryRegistrar {
    pub(super) fn from_pd(pd: Option<crate::v2::Pd>) -> Self {
        Self {
            pd: pd.map(|pd| Arc::downgrade(pd.raw_pd())),
        }
    }

    pub(super) fn register(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        if len == 0 || u32::try_from(len).is_err() {
            return Err(Error::InvalidConfig(
                "engine MR length must be in 1..=u32::MAX".into(),
            ));
        }
        let pd = self
            .pd
            .as_ref()
            .and_then(Weak::upgrade)
            .map(crate::v2::Pd::new)
            .ok_or_else(|| {
                Error::InvalidConfig("engine shared protection domain is unavailable".into())
            })?;
        pd.reg_mr(len, access)
    }
}

/// Resource-free protocol frontend for one engine-owned connection.
#[derive(Clone)]
pub(crate) struct IoConnection {
    memory: MemoryRegistrar,
    connection: super::registry::ConnectionToken,
    close: Arc<super::session::SessionCloseState>,
    events: IoEventSender,
    commands: Weak<super::reactor::CommandIngress>,
    manager: Weak<super::session::SessionFrontend>,
    admission: Arc<ProtocolAdmissionState>,
}

impl IoConnection {
    pub(crate) fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        self.memory.register(len, access)
    }

    pub(crate) fn post_recv_batch(&self, requests: Vec<IoRecvRequest>) -> IoSubmissionDisposition {
        self.submit_protocol(ProtocolCommand::recv(
            self.connection,
            self.events.clone(),
            Arc::clone(&self.admission.open),
            requests,
        ))
    }

    pub(crate) fn post_recv(&self, request: IoRecvRequest) -> IoSubmissionDisposition {
        self.post_recv_batch(vec![request])
    }

    pub(crate) fn post_send(&self, request: IoSendRequest) -> IoSubmissionDisposition {
        self.submit_protocol(ProtocolCommand::send(
            self.connection,
            self.events.clone(),
            Arc::clone(&self.admission.open),
            request,
        ))
    }

    pub(crate) fn request_close(&self) {
        if self.close.is_retired() {
            return;
        }
        if let (Some(manager), Some(commands)) = (self.manager.upgrade(), self.commands.upgrade()) {
            commands.request_connection_close(&manager, self.connection);
        }
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.request_close();
        loop {
            let notify = self.close.notify();
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.close.outcome() {
                return outcome.into_result();
            }
            if let Some(manager) = self.manager.upgrade()
                && let Some(outcome) = manager.engine_outcome()
            {
                return outcome.into_result();
            }
            notified.await;
        }
    }

    pub(super) fn from_connection(
        connection: &RdmaConnection,
        state: &mut ConnectionState,
    ) -> Result<(Self, IoEventReceiver)> {
        let (events, receiver) = event_port();
        let pending = state.install_io_event_sender(events.clone())?;
        if let Some(pending) = pending {
            pending.deliver();
        }
        Ok((
            Self {
                memory: connection.memory.clone(),
                connection: connection.session_token(),
                close: connection.close_state(),
                events,
                commands: connection.command_ingress(),
                manager: connection.frontend(),
                admission: Arc::new(ProtocolAdmissionState::default()),
            },
            receiver,
        ))
    }

    #[cfg_attr(test, allow(dead_code))]
    fn submit_protocol(&self, command: ProtocolCommand) -> IoSubmissionDisposition {
        let count = command.len();
        match self.submit(command) {
            Ok(()) => IoSubmissionDisposition::Admitted { operations: count },
            Err((error, command)) => command.reject(error),
        }
    }

    pub(crate) fn poll_protocol_admission(&self, cx: &mut Context<'_>, budget: usize) -> usize {
        self.poll_admission(cx, budget)
    }

    pub(crate) fn has_unpolled_protocol_admission(&self) -> bool {
        lock_unpoison(&self.admission.pending)
            .front()
            .is_some_and(|pending| !pending.polled)
    }

    pub(crate) fn discard_pending_protocol(&self) {
        self.admission.open.store(false, Ordering::Release);
        let pending = std::mem::take(&mut *lock_unpoison(&self.admission.pending));
        drop(pending);
    }

    #[allow(
        clippy::result_large_err,
        reason = "failed bounded admission returns the command with all owned MRs intact"
    )]
    fn submit(
        &self,
        command: ProtocolCommand,
    ) -> std::result::Result<(), (Error, ProtocolCommand)> {
        if !self.admission.open.load(Ordering::Acquire) {
            return Err((Error::TransportClosed, command));
        }
        let Some(commands) = self.commands.upgrade() else {
            return Err((Error::DriverShutdown, command));
        };
        if let Err(error) = commands.validate_operation_batch(command.len()) {
            return Err((error, command));
        }
        let permit = Box::pin(commands.operation_batch_acquire(command.len()));
        let mut pending = lock_unpoison(&self.admission.pending);
        if !self.admission.open.load(Ordering::Acquire) {
            return Err((Error::TransportClosed, command));
        }
        pending.push_back(ProtocolAdmission {
            command: Some(command),
            permit,
            polled: false,
        });
        drop(pending);
        self.admission.waker.wake();
        Ok(())
    }

    fn poll_admission(&self, cx: &mut Context<'_>, budget: usize) -> usize {
        self.admission.waker.register(cx.waker());
        let mut progressed = 0;
        while progressed < budget {
            let Some(mut admission) = lock_unpoison(&self.admission.pending).pop_front() else {
                break;
            };
            let command = admission
                .command
                .take()
                .expect("pending protocol admission owns its command");
            if command.receiver_lost() {
                drop(command);
                progressed += 1;
                continue;
            }
            if command.is_cancelled() {
                command.cancel();
                progressed += 1;
                continue;
            }
            admission.command = Some(command);
            admission.polled = true;
            match admission.permit.as_mut().poll(cx) {
                Poll::Pending => {
                    lock_unpoison(&self.admission.pending).push_front(admission);
                    break;
                }
                Poll::Ready(None) => {
                    let command = admission
                        .command
                        .take()
                        .expect("closed protocol admission owns its command");
                    let error = self
                        .manager
                        .upgrade()
                        .and_then(|manager| manager.admission_error())
                        .unwrap_or(Error::DriverShutdown);
                    command.reject(error);
                    progressed += 1;
                }
                Poll::Ready(Some(permit)) => {
                    let command = admission
                        .command
                        .take()
                        .expect("ready protocol admission owns its command");
                    let Some(commands) = self.commands.upgrade() else {
                        command.reject(Error::DriverShutdown);
                        progressed += 1;
                        continue;
                    };
                    let Some(manager) = self.manager.upgrade() else {
                        command.reject(Error::DriverShutdown);
                        progressed += 1;
                        continue;
                    };
                    match commands.enqueue_protocol(&manager, command, permit) {
                        Ok(()) => commands.publish_command_work(),
                        Err((error, command)) => {
                            command.reject(error);
                        }
                    }
                    progressed += 1;
                }
            }
        }
        progressed
    }
}

struct ProtocolAdmission {
    command: Option<ProtocolCommand>,
    permit: Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>>,
    polled: bool,
}

struct ProtocolAdmissionState {
    pending: Mutex<VecDeque<ProtocolAdmission>>,
    waker: AtomicWaker,
    open: Arc<AtomicBool>,
}

impl Default for ProtocolAdmissionState {
    fn default() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            waker: AtomicWaker::new(),
            open: Arc::new(AtomicBool::new(true)),
        }
    }
}

#[cfg(test)]
pub(in crate::v2::engine) struct ProtocolTestAdmission {
    commands: Weak<super::reactor::CommandIngress>,
    manager: Weak<super::session::SessionFrontend>,
    state: ProtocolAdmissionState,
}

#[cfg(test)]
impl ProtocolTestAdmission {
    pub(in crate::v2::engine) fn new(
        commands: Weak<super::reactor::CommandIngress>,
        manager: Weak<super::session::SessionFrontend>,
    ) -> Arc<Self> {
        Arc::new(Self {
            commands,
            manager,
            state: ProtocolAdmissionState::default(),
        })
    }

    #[allow(
        clippy::result_large_err,
        reason = "the test seam preserves production command ownership on rejection"
    )]
    pub(in crate::v2::engine) fn submit(
        &self,
        command: ProtocolCommand,
    ) -> std::result::Result<(), (Error, ProtocolCommand)> {
        let connection = IoConnectionTestAdmission {
            commands: &self.commands,
            manager: &self.manager,
            state: &self.state,
        };
        connection.submit(command)
    }

    pub(in crate::v2::engine) fn poll(&self, cx: &mut Context<'_>, budget: usize) -> usize {
        IoConnectionTestAdmission {
            commands: &self.commands,
            manager: &self.manager,
            state: &self.state,
        }
        .poll(cx, budget)
    }

    pub(in crate::v2::engine) fn open_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.state.open)
    }
}

#[cfg(test)]
struct IoConnectionTestAdmission<'a> {
    commands: &'a Weak<super::reactor::CommandIngress>,
    manager: &'a Weak<super::session::SessionFrontend>,
    state: &'a ProtocolAdmissionState,
}

#[cfg(test)]
impl IoConnectionTestAdmission<'_> {
    #[allow(
        clippy::result_large_err,
        reason = "the test seam preserves production command ownership on rejection"
    )]
    fn submit(
        &self,
        command: ProtocolCommand,
    ) -> std::result::Result<(), (Error, ProtocolCommand)> {
        if !self.state.open.load(Ordering::Acquire) {
            return Err((Error::TransportClosed, command));
        }
        let Some(commands) = self.commands.upgrade() else {
            return Err((Error::DriverShutdown, command));
        };
        if let Err(error) = commands.validate_operation_batch(command.len()) {
            return Err((error, command));
        }
        let permit = Box::pin(commands.operation_batch_acquire(command.len()));
        lock_unpoison(&self.state.pending).push_back(ProtocolAdmission {
            command: Some(command),
            permit,
            polled: false,
        });
        self.state.waker.wake();
        Ok(())
    }

    fn poll(&self, cx: &mut Context<'_>, budget: usize) -> usize {
        self.state.waker.register(cx.waker());
        let mut progressed = 0;
        while progressed < budget {
            let Some(mut admission) = lock_unpoison(&self.state.pending).pop_front() else {
                break;
            };
            let command = admission.command.take().expect("pending command");
            if command.receiver_lost() || command.is_cancelled() {
                command.cancel();
                progressed += 1;
                continue;
            }
            admission.command = Some(command);
            admission.polled = true;
            match admission.permit.as_mut().poll(cx) {
                Poll::Pending => {
                    lock_unpoison(&self.state.pending).push_front(admission);
                    break;
                }
                Poll::Ready(None) => {
                    admission.command.take().expect("pending command").reject(
                        self.manager
                            .upgrade()
                            .and_then(|manager| manager.admission_error())
                            .unwrap_or(Error::DriverShutdown),
                    );
                    progressed += 1;
                }
                Poll::Ready(Some(permit)) => {
                    let command = admission.command.take().expect("pending command");
                    let Some(commands) = self.commands.upgrade() else {
                        command.reject(Error::DriverShutdown);
                        progressed += 1;
                        continue;
                    };
                    let Some(manager) = self.manager.upgrade() else {
                        command.reject(Error::DriverShutdown);
                        progressed += 1;
                        continue;
                    };
                    match commands.enqueue_protocol(&manager, command, permit) {
                        Ok(()) => commands.publish_command_work(),
                        Err((error, command)) => {
                            command.reject(error);
                        }
                    }
                    progressed += 1;
                }
            }
        }
        progressed
    }
}

/// Borrow-scoped protocol I/O used only while connection setup already holds
/// exclusive reactor access before `rdma_connect`/`rdma_accept`.
pub(crate) struct BorrowedSetupIo<'a> {
    io_core: &'a mut IoState,
    io: Arc<EstablishedIoConnection>,
    io_ledger: &'a mut ConnectionIoState,
    poster: &'a super::session::connection::ConnectionPoster,
    connection: IoConnection,
}

impl BorrowedSetupIo<'_> {
    pub(crate) fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        self.connection.register_memory(len, access)
    }

    pub(crate) fn post_recv_batch(
        &mut self,
        requests: Vec<IoRecvRequest>,
    ) -> IoSubmissionDisposition {
        io_core::post_io_recv_batch(
            self.io_core,
            &self.io,
            self.io_ledger,
            self.poster,
            &self.connection.events,
            requests,
        )
    }

    pub(crate) fn into_connection(self) -> IoConnection {
        self.connection
    }
}

impl<'a> BorrowedSetupIo<'a> {
    pub(super) fn from_connection(
        connection: &'a RdmaConnection,
        state: &'a mut ConnectionState,
        io_core: &'a mut IoState,
    ) -> Result<(Self, IoEventReceiver)> {
        let (owned, receiver) = IoConnection::from_connection(connection, state)?;
        let (io, io_ledger, poster) = state.io_parts_mut();
        Ok((
            Self {
                io_core,
                io: Arc::clone(io),
                io_ledger,
                poster,
                connection: owned,
            },
            receiver,
        ))
    }
}

#[cfg_attr(test, allow(dead_code))]
pub(in crate::v2::engine) enum ProtocolBatch {
    Recv(Vec<IoRecvRequest>),
    Send(IoSendRequest),
    #[cfg(test)]
    Test(ProtocolTestPayload),
}

pub(in crate::v2::engine) struct ProtocolCommand {
    connection: super::registry::ConnectionToken,
    events: IoEventSender,
    owner_open: Arc<AtomicBool>,
    batch: ProtocolBatch,
}

#[cfg(test)]
#[derive(Clone)]
pub(in crate::v2::engine) struct ProtocolTestProbe {
    pub(in crate::v2::engine) cancelled: Arc<AtomicBool>,
    pub(in crate::v2::engine) executed: Arc<std::sync::atomic::AtomicUsize>,
    pub(in crate::v2::engine) resolved: Arc<std::sync::atomic::AtomicUsize>,
    pub(in crate::v2::engine) dropped: Arc<std::sync::atomic::AtomicUsize>,
    pub(in crate::v2::engine) published: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
pub(in crate::v2::engine) struct ProtocolTestPayload {
    operations: usize,
    actions: usize,
    probe: ProtocolTestProbe,
}

#[cfg(test)]
impl Drop for ProtocolTestPayload {
    fn drop(&mut self) {
        self.probe.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg_attr(test, allow(dead_code))]
impl ProtocolCommand {
    fn recv(
        connection: super::registry::ConnectionToken,
        events: IoEventSender,
        owner_open: Arc<AtomicBool>,
        requests: Vec<IoRecvRequest>,
    ) -> Self {
        Self {
            connection,
            events,
            owner_open,
            batch: ProtocolBatch::Recv(requests),
        }
    }

    fn send(
        connection: super::registry::ConnectionToken,
        events: IoEventSender,
        owner_open: Arc<AtomicBool>,
        request: IoSendRequest,
    ) -> Self {
        Self {
            connection,
            events,
            owner_open,
            batch: ProtocolBatch::Send(request),
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn for_test(
        operations: usize,
        actions: usize,
        events: IoEventSender,
        owner_open: Arc<AtomicBool>,
        probe: ProtocolTestProbe,
    ) -> Self {
        Self {
            connection: super::registry::ConnectionToken {
                slot: 0,
                generation: 1,
            },
            events,
            owner_open,
            batch: ProtocolBatch::Test(ProtocolTestPayload {
                operations,
                actions,
                probe,
            }),
        }
    }

    pub(in crate::v2::engine) fn len(&self) -> usize {
        match &self.batch {
            ProtocolBatch::Recv(requests) => requests.len(),
            ProtocolBatch::Send(_) => 1,
            #[cfg(test)]
            ProtocolBatch::Test(payload) => payload.operations,
        }
    }

    pub(in crate::v2::engine) fn is_cancelled(&self) -> bool {
        match &self.batch {
            ProtocolBatch::Recv(_) => false,
            ProtocolBatch::Send(request) => request.is_cancelled(),
            #[cfg(test)]
            ProtocolBatch::Test(payload) => payload.probe.cancelled.load(Ordering::Acquire),
        }
    }

    pub(in crate::v2::engine) fn receiver_lost(&self) -> bool {
        !self.owner_open.load(Ordering::Acquire) || !self.events.is_open()
    }

    pub(in crate::v2::engine) fn execute_into(
        self,
        connections: &mut super::session::registry::ConnectionRegistry,
        io_core: &mut IoState,
        actions: &mut ReactorActions,
    ) {
        #[cfg(test)]
        if let ProtocolBatch::Test(payload) = &self.batch {
            payload.probe.executed.fetch_add(1, Ordering::AcqRel);
            for _ in 0..payload.actions {
                let published = Arc::clone(&payload.probe.published);
                actions.push_operation(move || {
                    published.fetch_add(1, Ordering::AcqRel);
                });
            }
            return;
        }
        if !connections.io_is_open(self.connection) {
            self.reject_into(Error::TransportClosed, actions);
            return;
        }
        match self.batch {
            ProtocolBatch::Recv(requests) => {
                connections
                    .with_connection_io_mut(self.connection, |connection, connection_io, poster| {
                        io_core::post_io_recv_batch_into(
                            io_core,
                            connection,
                            connection_io,
                            poster,
                            &self.events,
                            requests,
                            actions,
                        );
                    })
                    .expect("open protocol connection retains its I/O bundle");
            }
            ProtocolBatch::Send(request) => {
                connections
                    .with_connection_io_mut(self.connection, |connection, connection_io, poster| {
                        io_core::post_io_send_into(
                            io_core,
                            connection,
                            connection_io,
                            poster,
                            &self.events,
                            request,
                            actions,
                        );
                    })
                    .expect("open protocol connection retains its I/O bundle");
            }
            #[cfg(test)]
            ProtocolBatch::Test(_) => unreachable!("test protocol command returned above"),
        }
    }

    pub(in crate::v2::engine) fn reject(self, error: Error) -> IoSubmissionDisposition {
        let count = self.len();
        self.rejected_events(error.clone())
            .into_iter()
            .for_each(PendingIoEvent::deliver);
        IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: count,
            error,
        }
    }

    pub(in crate::v2::engine) fn cancel(self) -> IoSubmissionDisposition {
        let count = self.len();
        self.cancelled_events()
            .into_iter()
            .for_each(PendingIoEvent::deliver);
        IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: count,
            error: Error::DriverShutdown,
        }
    }

    pub(in crate::v2::engine) fn reject_into(self, error: Error, actions: &mut ReactorActions) {
        for event in self.rejected_events(error) {
            actions.push_event(event);
        }
    }

    pub(in crate::v2::engine) fn cancel_into(self, actions: &mut ReactorActions) {
        for event in self.cancelled_events() {
            actions.push_event(event);
        }
    }

    fn rejected_events(self, error: Error) -> Vec<PendingIoEvent> {
        match self.batch {
            ProtocolBatch::Recv(requests) => requests
                .into_iter()
                .map(|request| {
                    let (mr, context) = request.into_parts();
                    IoEventDestination::new(self.events.clone(), context).unaccepted(
                        None,
                        error.clone(),
                        mr,
                    )
                })
                .collect(),
            ProtocolBatch::Send(request) => {
                let (mr, _, context, _) = request.into_parts();
                vec![IoEventDestination::new(self.events, context).unaccepted(None, error, mr)]
            }
            #[cfg(test)]
            ProtocolBatch::Test(payload) => {
                payload.probe.resolved.fetch_add(1, Ordering::AcqRel);
                Vec::new()
            }
        }
    }

    fn cancelled_events(self) -> Vec<PendingIoEvent> {
        match self.batch {
            ProtocolBatch::Recv(requests) => requests
                .into_iter()
                .map(|request| {
                    let (mr, context) = request.into_parts();
                    IoEventDestination::new(self.events.clone(), context).cancelled(mr)
                })
                .collect(),
            ProtocolBatch::Send(request) => {
                let (mr, _, context, _) = request.into_parts();
                vec![IoEventDestination::new(self.events, context).cancelled(mr)]
            }
            #[cfg(test)]
            ProtocolBatch::Test(payload) => {
                payload.probe.resolved.fetch_add(1, Ordering::AcqRel);
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
impl IoConnection {
    pub(crate) fn with_delayed_close_event_for_test() -> (Self, IoEventReceiver, impl FnOnce()) {
        use super::config::{EngineConfig, RdmaConnectionConfig};
        use super::registry::ConnectionToken;
        use super::session::connection::TestConnectionProvider;
        use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
        use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

        struct TestPoster;

        impl TestConnectionProvider for TestPoster {
            fn qp_num(&self) -> u32 {
                1
            }

            fn capabilities(&self) -> Option<QpCapabilities> {
                None
            }

            fn post_send(&self, _: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
                Ok(BatchPostOutcome::AllAccepted)
            }

            fn post_recv(&self, _: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
                Ok(BatchPostOutcome::AllAccepted)
            }

            fn to_error(&self) -> Result<()> {
                Ok(())
            }

            fn destroy_qp(&self) -> Result<bool> {
                Ok(false)
            }

            fn disconnect(&self) -> Result<()> {
                Ok(())
            }
        }

        let (shared, _session) = EngineFrontendRoot::new(
            EngineConfig::new("test0".into()),
            None,
            MemoryRegistrar::from_pd(None),
        )
        .expect("test engine state");
        let shared = shared.into_shared();
        let connection = ConnectionState::new_for_test(
            ConnectionToken {
                slot: 0,
                generation: 1,
            },
            Arc::new(TestPoster),
            RdmaConnectionConfig::default(),
            None,
            None,
            None,
        );
        let (sender, receiver) = event_port();
        assert!(
            connection
                .install_io_event_sender(sender.clone())
                .expect("test I/O sender installation")
                .is_none()
        );
        let delayed = sender.terminal(IoTerminalEvent::Closed(Ok(())));
        (
            Self {
                memory: MemoryRegistrar::from_pd(None),
                commands: Arc::downgrade(&shared.commands),
                manager: Arc::downgrade(&shared.session),
                admission: Arc::new(ProtocolAdmissionState::default()),
                connection: connection.token,
                close: connection.close_state(),
                events: sender,
            },
            receiver,
            move || delayed.deliver(),
        )
    }
}

/// Protocol-owned context returned unchanged with an operation completion.
pub(crate) struct IoOperationContext(Box<dyn Any + Send + 'static>);

impl IoOperationContext {
    pub(crate) fn new<T: Any + Send + 'static>(value: T) -> Self {
        Self(Box::new(value))
    }

    pub(crate) fn downcast<T: Any + Send + 'static>(self) -> std::result::Result<T, Self> {
        match self.0.downcast::<T>() {
            Ok(value) => Ok(*value),
            Err(value) => Err(Self(value)),
        }
    }
}

/// Owned receive request submitted through [`IoConnection`].
pub(crate) struct IoRecvRequest {
    mr: Mr,
    context: IoOperationContext,
}

impl IoRecvRequest {
    pub(crate) fn new(mr: Mr, context: IoOperationContext) -> Self {
        Self { mr, context }
    }

    pub(super) fn into_parts(self) -> (Mr, IoOperationContext) {
        (self.mr, self.context)
    }
}

/// Owned send request submitted through [`IoConnection`].
pub(crate) struct IoSendRequest {
    mr: Mr,
    len: usize,
    context: IoOperationContext,
    cancellation: Option<IoCancellation>,
}

impl IoSendRequest {
    pub(crate) fn new(mr: Mr, len: usize, context: IoOperationContext) -> Self {
        Self {
            mr,
            len,
            context,
            cancellation: None,
        }
    }

    pub(crate) fn with_cancellation(mut self, cancellation: IoCancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(IoCancellation::is_cancelled)
    }

    pub(super) fn into_parts(self) -> (Mr, usize, IoOperationContext, Option<IoCancellation>) {
        (self.mr, self.len, self.context, self.cancellation)
    }
}

#[derive(Clone)]
pub(crate) struct IoCancellation {
    cancelled: Arc<AtomicBool>,
}

impl IoCancellation {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Exact post-reconciliation ownership classification.
#[derive(Debug)]
pub(crate) enum IoSubmissionDisposition {
    Admitted {
        operations: usize,
    },
    AllAccepted {
        accepted: usize,
    },
    ExactPrefix {
        accepted: usize,
        proven_unaccepted: usize,
        error: Error,
    },
    FullyUnaccepted {
        proven_unaccepted: usize,
        error: Error,
    },
    RetainedAmbiguous {
        retained: usize,
        error: Error,
    },
    RetainedAfterEarlyCompletion {
        retained: usize,
        error: Error,
    },
}

impl IoSubmissionDisposition {
    pub(crate) fn all_accepted(&self) -> bool {
        matches!(self, Self::Admitted { .. } | Self::AllAccepted { .. })
    }

    pub(crate) fn accepted(&self) -> usize {
        match self {
            Self::Admitted { operations } => *operations,
            Self::AllAccepted { accepted } | Self::ExactPrefix { accepted, .. } => *accepted,
            Self::FullyUnaccepted { .. } => 0,
            Self::RetainedAmbiguous { retained, .. }
            | Self::RetainedAfterEarlyCompletion { retained, .. } => *retained,
        }
    }

    pub(crate) fn potentially_accepted(&self) -> bool {
        match self {
            Self::Admitted { operations } => *operations != 0,
            Self::AllAccepted { accepted } => *accepted != 0,
            Self::ExactPrefix { accepted, .. } => *accepted != 0,
            Self::FullyUnaccepted { .. } => false,
            Self::RetainedAmbiguous { retained, .. }
            | Self::RetainedAfterEarlyCompletion { retained, .. } => *retained != 0,
        }
    }

    pub(crate) fn error(&self) -> Option<&Error> {
        match self {
            Self::Admitted { .. } | Self::AllAccepted { .. } => None,
            Self::ExactPrefix { error, .. }
            | Self::FullyUnaccepted { error, .. }
            | Self::RetainedAmbiguous { error, .. }
            | Self::RetainedAfterEarlyCompletion { error, .. } => Some(error),
        }
    }

    pub(crate) fn proven_unaccepted(&self) -> usize {
        match self {
            Self::ExactPrefix {
                proven_unaccepted, ..
            }
            | Self::FullyUnaccepted {
                proven_unaccepted, ..
            } => *proven_unaccepted,
            Self::Admitted { .. }
            | Self::AllAccepted { .. }
            | Self::RetainedAmbiguous { .. }
            | Self::RetainedAfterEarlyCompletion { .. } => 0,
        }
    }
}

/// Opaque operation identity attached only after registry allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IoOperationIdentity {
    slot: u32,
    generation: u32,
}

impl IoOperationIdentity {
    pub(super) fn from_token(token: OperationToken) -> Self {
        Self {
            slot: token.slot,
            generation: token.generation,
        }
    }
}

/// Owned operation completion delivered to the protocol driver.
pub(crate) struct IoCompletionEvent {
    identity: Option<IoOperationIdentity>,
    context: IoOperationContext,
    result: Result<Completion>,
    mr: Option<Mr>,
    proven_unaccepted: bool,
    cancelled_before_submit: bool,
}

impl IoCompletionEvent {
    pub(crate) fn into_parts(
        self,
    ) -> (
        Option<IoOperationIdentity>,
        IoOperationContext,
        Result<Completion>,
        Option<Mr>,
        bool,
    ) {
        (
            self.identity,
            self.context,
            self.result,
            self.mr,
            self.proven_unaccepted,
        )
    }

    pub(crate) fn into_parts_with_cancellation(
        self,
    ) -> (
        Option<IoOperationIdentity>,
        IoOperationContext,
        Result<Completion>,
        Option<Mr>,
        bool,
        bool,
    ) {
        (
            self.identity,
            self.context,
            self.result,
            self.mr,
            self.proven_unaccepted,
            self.cancelled_before_submit,
        )
    }
}

/// Owned connection lifecycle notification.
pub(crate) enum IoTerminalEvent {
    Disconnected,
    Terminal(Error),
    Closed(Result<()>),
}

/// One event from the connection-scoped I/O port.
pub(crate) enum IoEvent {
    Completion(IoCompletionEvent),
    Terminal(IoTerminalEvent),
}

struct IoEventPort {
    queue: Mutex<IoEventQueue>,
    waker: AtomicWaker,
    receiver_open: AtomicBool,
}

struct IoEventQueue {
    open: bool,
    events: VecDeque<IoEvent>,
}

#[derive(Clone)]
pub(super) struct IoEventSender {
    port: Arc<IoEventPort>,
}

impl IoEventSender {
    fn send(&self, event: IoEvent) {
        let mut event = Some(event);
        let queued = {
            let mut queue = lock_unpoison(&self.port.queue);
            if queue.open {
                queue
                    .events
                    .push_back(event.take().expect("I/O event is present"));
                true
            } else {
                false
            }
        };
        drop(event);
        if queued {
            self.port.waker.wake();
        }
    }

    fn is_open(&self) -> bool {
        self.port.receiver_open.load(Ordering::Acquire)
    }

    pub(super) fn completion(
        &self,
        identity: Option<IoOperationIdentity>,
        context: IoOperationContext,
        result: Result<Completion>,
        mr: Option<Mr>,
        proven_unaccepted: bool,
    ) -> PendingIoEvent {
        PendingIoEvent {
            sender: self.clone(),
            event: IoEvent::Completion(IoCompletionEvent {
                identity,
                context,
                result,
                mr,
                proven_unaccepted,
                cancelled_before_submit: false,
            }),
        }
    }

    pub(super) fn terminal(&self, event: IoTerminalEvent) -> PendingIoEvent {
        PendingIoEvent {
            sender: self.clone(),
            event: IoEvent::Terminal(event),
        }
    }
}

pub(super) struct IoEventDestination {
    sender: IoEventSender,
    context: IoOperationContext,
}

impl IoEventDestination {
    pub(super) fn new(sender: IoEventSender, context: IoOperationContext) -> Self {
        Self { sender, context }
    }

    pub(super) fn complete(
        self,
        identity: IoOperationIdentity,
        result: Result<Completion>,
        mr: Option<Mr>,
    ) -> PendingIoEvent {
        self.sender
            .completion(Some(identity), self.context, result, mr, false)
    }

    pub(super) fn unaccepted(
        self,
        identity: Option<IoOperationIdentity>,
        error: Error,
        mr: Mr,
    ) -> PendingIoEvent {
        self.sender
            .completion(identity, self.context, Err(error), Some(mr), true)
    }

    pub(super) fn cancelled(self, mr: Mr) -> PendingIoEvent {
        PendingIoEvent {
            sender: self.sender,
            event: IoEvent::Completion(IoCompletionEvent {
                identity: None,
                context: self.context,
                result: Err(Error::DriverShutdown),
                mr: Some(mr),
                proven_unaccepted: true,
                cancelled_before_submit: true,
            }),
        }
    }
}

pub(super) struct PendingIoEvent {
    sender: IoEventSender,
    event: IoEvent,
}

impl PendingIoEvent {
    pub(super) fn deliver(self) {
        self.sender.send(self.event);
    }
}

/// Sole receiver for one connection-scoped I/O event port.
pub(crate) struct IoEventReceiver {
    port: Arc<IoEventPort>,
}

impl IoEventReceiver {
    pub(crate) fn pop(&self) -> Option<IoEvent> {
        lock_unpoison(&self.port.queue).events.pop_front()
    }

    pub(crate) fn has_events(&self) -> bool {
        !lock_unpoison(&self.port.queue).events.is_empty()
    }

    pub(crate) fn register(&self, waker: &Waker) {
        self.port.waker.register(waker);
    }

    pub(crate) fn drain(&self) -> Vec<IoEvent> {
        std::mem::take(&mut lock_unpoison(&self.port.queue).events)
            .into_iter()
            .collect()
    }

    pub(crate) fn close(&self) -> Vec<IoEvent> {
        let events = {
            let mut queue = lock_unpoison(&self.port.queue);
            queue.open = false;
            std::mem::take(&mut queue.events)
        };
        self.port.receiver_open.store(false, Ordering::Release);
        events.into_iter().collect()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn queued_len(&self) -> usize {
        lock_unpoison(&self.port.queue).events.len()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn queued_owned_completions(&self) -> usize {
        lock_unpoison(&self.port.queue)
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    IoEvent::Completion(IoCompletionEvent { mr: Some(_), .. })
                )
            })
            .count()
    }
}

impl Drop for IoEventReceiver {
    fn drop(&mut self) {
        drop(self.close());
    }
}

pub(super) fn event_port() -> (IoEventSender, IoEventReceiver) {
    let port = Arc::new(IoEventPort {
        queue: Mutex::new(IoEventQueue {
            open: true,
            events: VecDeque::new(),
        }),
        waker: AtomicWaker::new(),
        receiver_open: AtomicBool::new(true),
    });
    (
        IoEventSender {
            port: Arc::clone(&port),
        },
        IoEventReceiver { port },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Barrier};
    use std::task::{Wake, Waker};

    use super::*;

    struct QueueCheckingWake {
        port: Arc<IoEventPort>,
        wakes: AtomicUsize,
        lock_was_free: AtomicBool,
    }

    impl Wake for QueueCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::AcqRel);
            self.lock_was_free
                .store(self.port.queue.try_lock().is_ok(), Ordering::Release);
        }
    }

    fn terminal() -> IoEvent {
        IoEvent::Terminal(IoTerminalEvent::Disconnected)
    }

    #[test]
    fn dispositions_preserve_exact_post_reconciliation_counts() {
        let all = IoSubmissionDisposition::AllAccepted { accepted: 4 };
        assert!(all.all_accepted());
        assert_eq!(all.accepted(), 4);
        assert_eq!(all.proven_unaccepted(), 0);

        let partial = IoSubmissionDisposition::ExactPrefix {
            accepted: 2,
            proven_unaccepted: 2,
            error: Error::PostFailed(std::io::Error::from_raw_os_error(libc::ENOMEM)),
        };
        assert_eq!(partial.accepted(), 2);
        assert_eq!(partial.proven_unaccepted(), 2);

        let rejected = IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: 4,
            error: Error::PostFailed(std::io::Error::from_raw_os_error(libc::ENOMEM)),
        };
        assert!(!rejected.potentially_accepted());
        assert_eq!(rejected.proven_unaccepted(), 4);

        for retained in [
            IoSubmissionDisposition::RetainedAmbiguous {
                retained: 4,
                error: Error::PostFailed(std::io::Error::from_raw_os_error(libc::EIO)),
            },
            IoSubmissionDisposition::RetainedAfterEarlyCompletion {
                retained: 4,
                error: Error::PostFailed(std::io::Error::from_raw_os_error(libc::ENOMEM)),
            },
        ] {
            assert!(retained.potentially_accepted());
            assert_eq!(retained.accepted(), 4);
            assert_eq!(retained.proven_unaccepted(), 0);
        }
    }

    #[test]
    fn event_publication_before_and_after_registration_is_observable() {
        let (sender, receiver) = event_port();
        sender.send(terminal());
        let wake = Arc::new(QueueCheckingWake {
            port: Arc::clone(&sender.port),
            wakes: AtomicUsize::new(0),
            lock_was_free: AtomicBool::new(false),
        });
        receiver.register(&Waker::from(Arc::clone(&wake)));
        assert!(receiver.has_events());
        assert!(matches!(receiver.pop(), Some(IoEvent::Terminal(_))));

        sender.send(terminal());
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
        assert!(wake.lock_was_free.load(Ordering::Acquire));
        assert!(matches!(receiver.pop(), Some(IoEvent::Terminal(_))));
    }

    #[test]
    fn event_register_recheck_race_never_loses_work() {
        for _ in 0..128 {
            let (sender, receiver) = event_port();
            let barrier = Arc::new(Barrier::new(2));
            let publisher_barrier = Arc::clone(&barrier);
            let publisher = std::thread::spawn(move || {
                publisher_barrier.wait();
                sender.send(terminal());
            });
            let wake = Arc::new(QueueCheckingWake {
                port: Arc::clone(&receiver.port),
                wakes: AtomicUsize::new(0),
                lock_was_free: AtomicBool::new(false),
            });
            barrier.wait();
            let observed = receiver.has_events();
            receiver.register(&Waker::from(Arc::clone(&wake)));
            let rechecked = receiver.has_events();
            publisher.join().unwrap();
            assert!(
                observed || rechecked || wake.wakes.load(Ordering::Acquire) != 0,
                "publication was neither observed nor followed by a wake"
            );
            assert!(receiver.has_events());
        }
    }

    #[test]
    fn dropped_receiver_discards_future_owned_events_without_waking() {
        let (sender, receiver) = event_port();
        let wake = Arc::new(QueueCheckingWake {
            port: Arc::clone(&sender.port),
            wakes: AtomicUsize::new(0),
            lock_was_free: AtomicBool::new(false),
        });
        receiver.register(&Waker::from(Arc::clone(&wake)));
        drop(receiver);
        sender.send(terminal());
        assert_eq!(wake.wakes.load(Ordering::Acquire), 0);
        assert!(!sender.port.receiver_open.load(Ordering::Acquire));
    }
}
