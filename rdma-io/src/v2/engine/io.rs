//! Crate-private owned submission and event boundary for protocol drivers.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use futures_util::task::AtomicWaker;

use super::EngineShared;
use super::io_core::{self, EstablishedIoConnection, IoCore};
use super::registry::{OperationToken, lock_unpoison};
use super::session::SessionConnection;
#[cfg(test)]
use super::session::connection::ConnectionState;
use super::session::connection::RdmaConnection;
use crate::v2::error::{Error, Result};
use crate::v2::mr::{AccessIntent, Mr};
use crate::v2::op::Completion;

#[derive(Clone)]
pub(super) struct MemoryRegistrar {
    pd: Option<crate::v2::Pd>,
}

impl MemoryRegistrar {
    pub(super) fn from_engine(shared: &EngineShared) -> Self {
        Self {
            pd: shared
                .resource_refs
                .as_ref()
                .map(|resources| resources.pd.clone()),
        }
    }

    pub(super) fn register(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        if len == 0 || u32::try_from(len).is_err() {
            return Err(Error::InvalidConfig(
                "engine MR length must be in 1..=u32::MAX".into(),
            ));
        }
        let pd = self.pd.as_ref().ok_or_else(|| {
            Error::InvalidConfig("engine shared protection domain is unavailable".into())
        })?;
        pd.reg_mr(len, access)
    }
}

/// Opaque authority for protocol I/O on one engine-owned connection.
#[derive(Clone)]
pub(crate) struct IoConnection {
    io_core: Arc<IoCore>,
    io: Arc<EstablishedIoConnection>,
    memory: MemoryRegistrar,
    session: SessionConnection,
    events: IoEventSender,
}

impl IoConnection {
    #[cfg(test)]
    pub(super) fn new(
        shared: Arc<EngineShared>,
        connection: Arc<ConnectionState>,
    ) -> Result<(Self, IoEventReceiver)> {
        let (events, receiver) = event_port();
        let pending = connection.install_io_event_sender(events.clone())?;
        if let Some(pending) = pending {
            pending.deliver();
        }
        Ok((
            Self {
                io_core: Arc::clone(&shared.io_core),
                io: Arc::clone(&connection.io),
                memory: MemoryRegistrar::from_engine(&shared),
                session: shared.session.connection_capability(&connection),
                events,
            },
            receiver,
        ))
    }

    pub(crate) fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        self.memory.register(len, access)
    }

    pub(crate) fn post_recv_batch(&self, requests: Vec<IoRecvRequest>) -> IoSubmissionDisposition {
        io_core::post_io_recv_batch(&self.io_core, &self.io, &self.events, requests)
    }

    pub(crate) fn post_recv(&self, request: IoRecvRequest) -> IoSubmissionDisposition {
        io_core::post_io_recv_batch(&self.io_core, &self.io, &self.events, vec![request])
    }

    pub(crate) fn post_send(&self, request: IoSendRequest) -> IoSubmissionDisposition {
        io_core::post_io_send(&self.io_core, &self.io, &self.events, request)
    }

    pub(crate) fn request_close(&self) {
        self.session.request_close();
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.session.close().await
    }

    pub(super) fn from_connection(connection: &RdmaConnection) -> Result<(Self, IoEventReceiver)> {
        let (events, receiver) = event_port();
        let state = connection.session_state().ok_or(Error::TransportClosed)?;
        let pending = state.install_io_event_sender(events.clone())?;
        if let Some(pending) = pending {
            pending.deliver();
        }
        Ok((
            Self {
                io_core: Arc::clone(&connection.io_core),
                io: Arc::clone(&connection.io),
                memory: connection.memory.clone(),
                session: connection.session.clone(),
                events,
            },
            receiver,
        ))
    }
}

#[cfg(test)]
impl IoConnection {
    pub(crate) fn with_delayed_close_event_for_test() -> (Self, IoEventReceiver, impl FnOnce()) {
        use super::config::{EngineConfig, RdmaConnectionConfig};
        use super::registry::ConnectionToken;
        use super::session::connection::WorkRequestPoster;
        use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
        use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

        struct TestPoster;

        impl WorkRequestPoster for TestPoster {
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

        let shared = EngineShared::new(EngineConfig::new("test0".into()), None, None)
            .expect("test engine state")
            .into_shared();
        let connection = Arc::new(ConnectionState::new(
            ConnectionToken {
                slot: 0,
                generation: 1,
            },
            Arc::new(TestPoster),
            RdmaConnectionConfig::default(),
            None,
            None,
            None,
            None,
        ));
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
                io_core: Arc::clone(&shared.io_core),
                io: Arc::clone(&connection.io),
                memory: MemoryRegistrar { pd: None },
                session: shared.session.connection_capability(&connection),
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
}

impl IoSendRequest {
    pub(crate) fn new(mr: Mr, len: usize, context: IoOperationContext) -> Self {
        Self { mr, len, context }
    }

    pub(super) fn into_parts(self) -> (Mr, usize, IoOperationContext) {
        (self.mr, self.len, self.context)
    }
}

/// Exact post-reconciliation ownership classification.
#[derive(Debug)]
pub(crate) enum IoSubmissionDisposition {
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
        matches!(self, Self::AllAccepted { .. })
    }

    pub(crate) fn accepted(&self) -> usize {
        match self {
            Self::AllAccepted { accepted } | Self::ExactPrefix { accepted, .. } => *accepted,
            Self::FullyUnaccepted { .. } => 0,
            Self::RetainedAmbiguous { retained, .. }
            | Self::RetainedAfterEarlyCompletion { retained, .. } => *retained,
        }
    }

    pub(crate) fn potentially_accepted(&self) -> bool {
        match self {
            Self::AllAccepted { accepted } => *accepted != 0,
            Self::ExactPrefix { accepted, .. } => *accepted != 0,
            Self::FullyUnaccepted { .. } => false,
            Self::RetainedAmbiguous { retained, .. }
            | Self::RetainedAfterEarlyCompletion { retained, .. } => *retained != 0,
        }
    }

    pub(crate) fn error(&self) -> Option<&Error> {
        match self {
            Self::AllAccepted { .. } => None,
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
            Self::AllAccepted { .. }
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
