use super::*;

impl CommandIngress {
    fn closed_admission_error(&self, frontend: &SessionFrontend) -> Error {
        frontend.admission_error().unwrap_or(Error::DriverShutdown)
    }

    /// Force the admitting poll to return `Pending` before observing completion.
    ///
    /// A driver on another executor thread may consume and complete a command
    /// immediately after enqueue. This one-poll boundary keeps frontend semantics
    /// deterministic: first poll admits only; a later poll observes the result.
    pub(in crate::v2::engine) async fn yield_after_admission() {
        let mut admitted = false;
        poll_fn(|cx| {
            if admitted {
                Poll::Ready(())
            } else {
                admitted = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    pub(in crate::v2::engine) async fn acquire_connect(
        self: &Arc<Self>,
    ) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.connect_permits).acquire_owned().await.ok()
    }

    pub(in crate::v2::engine) fn reserve_connect(
        self: &Arc<Self>,
        lane: OwnedSemaphorePermit,
    ) -> Result<ConnectAdmission, Error> {
        let permit_pool = Arc::clone(&self.connection_permits);
        let reservation = Arc::clone(&permit_pool)
            .try_acquire_owned()
            .map(|permit| {
                ConnectionReservation::new_with_frontend_diagnostics(
                    permit,
                    Arc::clone(&permit_pool),
                    self.connection_capacity,
                    Arc::clone(&self.diagnostics),
                )
            })
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::NoPermits => Error::CapacityExhausted,
                tokio::sync::TryAcquireError::Closed => Error::DriverShutdown,
            })?;
        Ok(ConnectAdmission { lane, reservation })
    }

    pub(in crate::v2::engine) async fn acquire_listen(
        self: &Arc<Self>,
    ) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.listen_permits).acquire_owned().await.ok()
    }

    pub(in crate::v2::engine) fn operation_acquire(
        self: &Arc<Self>,
    ) -> impl std::future::Future<Output = Option<OwnedSemaphorePermit>> + Send + 'static {
        let permits = Arc::clone(&self.operation_permits);
        async move { permits.acquire_owned().await.ok() }
    }

    pub(in crate::v2::engine) fn enqueue_connect(
        &self,
        request: Arc<OutboundRequest>,
        admission: ConnectAdmission,
    ) {
        let ConnectAdmission { lane, reservation } = admission;
        lock_unpoison(&self.queues)
            .connect
            .push_back(SessionCommand::Connect {
                request,
                reservation,
                _permit: lane,
            });
    }

    pub(in crate::v2::engine) fn enqueue_listen(
        &self,
        request: Arc<ListenRequest>,
        permit: OwnedSemaphorePermit,
    ) {
        lock_unpoison(&self.queues)
            .listen
            .push_back(SessionCommand::Listen {
                request,
                _permit: permit,
            });
    }

    pub(in crate::v2::engine) fn enqueue_accept(
        &self,
        listener: ListenerToken,
        request: Arc<AcceptRequest>,
    ) {
        lock_unpoison(&self.queues)
            .accept
            .push_back((listener, request));
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn enqueue_test_connection_install(
        &self,
        request: Arc<crate::v2::engine::driver::test_api::TestConnectionInstallRequest>,
        admission: ConnectAdmission,
    ) {
        let ConnectAdmission { lane, reservation } = admission;
        lock_unpoison(&self.queues)
            .connect
            .push_back(SessionCommand::TestInstall {
                request,
                reservation,
                _permit: lane,
            });
    }

    pub(in crate::v2::engine) fn notify_reactor(&self) {
        self.signal.notify_reactor();
    }

    pub(in crate::v2::engine) fn defer_setup_publication(
        &self,
        mut publication: crate::v2::engine::reactor::DeferredProtocolActions,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        publication.append_bounded_to(actions);
        if publication.is_empty() {
            return;
        }
        lock_unpoison(&self.queues)
            .protocol
            .push_back(ProtocolQueueEntry::Publication(publication));
        self.notify_reactor();
    }

    pub(in crate::v2::engine) fn cancel_connect(&self, target: &Arc<OutboundRequest>) -> bool {
        let command = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues.connect.iter().position(|command| {
                matches!(
                    command,
                    SessionCommand::Connect { request, .. }
                        if Arc::ptr_eq(request, target)
                )
            });
            position.and_then(|position| queues.connect.remove(position))
        };
        command.is_some()
    }

    pub(in crate::v2::engine) fn cancel_listen(&self, target: &Arc<ListenRequest>) -> bool {
        let command = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues.listen.iter().position(|command| {
                matches!(
                    command,
                    SessionCommand::Listen { request, .. }
                        if Arc::ptr_eq(request, target)
                )
            });
            position.and_then(|position| queues.listen.remove(position))
        };
        command.is_some()
    }

    pub(in crate::v2::engine) fn cancel_accept(&self, target: &Arc<AcceptRequest>) -> bool {
        let request = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues
                .accept
                .iter()
                .position(|(_, request)| Arc::ptr_eq(request, target));
            position.and_then(|position| queues.accept.remove(position))
        };
        if let Some((_, request)) = request {
            request.release_permit();
            true
        } else {
            false
        }
    }

    pub(in crate::v2::engine) fn request_listener_work(&self, token: ListenerToken) {
        let inserted = {
            let mut controls = lock_unpoison(&self.controls);
            controls.listener_work.push(token)
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn request_listener_close(
        &self,
        frontend: &SessionFrontend,
        token: ListenerToken,
        admission: &ListenerAdmission,
    ) {
        if admission.is_terminal() {
            return;
        }
        let inserted = {
            let _admission = write_unpoison(&frontend.admission);
            if admission.is_terminal() {
                return;
            }
            admission.close();
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            let mut controls = lock_unpoison(&self.controls);
            controls.listener_close.push(token)
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn enqueue_operation(
        &self,
        manager: &SessionFrontend,
        command: Arc<OperationCommand>,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), Error> {
        let _admission = read_unpoison(&manager.admission);
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_admission_error(manager));
        }
        lock_unpoison(&self.queues)
            .operation
            .push_back((command, permit));
        Ok(())
    }

    #[cfg_attr(test, allow(dead_code))]
    #[allow(
        clippy::result_large_err,
        reason = "failed bounded admission returns the command with all owned MRs intact"
    )]
    pub(in crate::v2::engine) fn enqueue_protocol(
        &self,
        manager: &SessionFrontend,
        command: ProtocolCommand,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), (Error, ProtocolCommand)> {
        let _admission = read_unpoison(&manager.admission);
        if self.closed.load(Ordering::Acquire) {
            return Err((self.closed_admission_error(manager), command));
        }
        lock_unpoison(&self.queues)
            .protocol
            .push_back(ProtocolQueueEntry::Command(command, permit));
        Ok(())
    }

    pub(in crate::v2::engine) fn validate_operation_batch(
        &self,
        count: usize,
    ) -> Result<u32, Error> {
        let count = u32::try_from(count).map_err(|_| {
            Error::InvalidConfig("protocol I/O batch length must be in 1..=u32::MAX".into())
        })?;
        if count == 0 || count as usize > self.operation_capacity {
            return Err(Error::InvalidConfig(format!(
                "protocol I/O batch length must be in 1..={}",
                self.operation_capacity
            )));
        }
        Ok(count)
    }

    pub(in crate::v2::engine) fn reserve_protocol_payload(
        &self,
        operations: usize,
    ) -> Result<ProtocolPayloadReservation, Error> {
        self.validate_operation_batch(operations)?;
        self.pending_protocol_operations
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending
                    .checked_add(operations)
                    .filter(|next| *next <= self.operation_capacity)
            })
            .map_err(|_| Error::CapacityExhausted)?;
        Ok(ProtocolPayloadReservation {
            pending: Arc::clone(&self.pending_protocol_operations),
            operations,
        })
    }

    pub(in crate::v2::engine) fn operation_batch_acquire(
        self: &Arc<Self>,
        count: usize,
    ) -> impl std::future::Future<Output = Option<OwnedSemaphorePermit>> + Send + 'static {
        let permits = Arc::clone(&self.operation_permits);
        let count = self
            .validate_operation_batch(count)
            .expect("protocol batch is validated before admission");
        async move { permits.acquire_many_owned(count).await.ok() }
    }

    pub(in crate::v2::engine) fn cancel_operation(&self, target: &Arc<OperationCommand>) -> bool {
        let command = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues
                .operation
                .iter()
                .position(|(command, _)| Arc::ptr_eq(command, target));
            position.and_then(|position| queues.operation.remove(position))
        };
        command.is_some()
    }

    pub(in crate::v2::engine) fn request_operation_cancel(&self, token: OperationToken) {
        let inserted = {
            let mut controls = lock_unpoison(&self.controls);
            controls.operation_cancel.push(token)
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn request_connection_close(
        &self,
        manager: &SessionFrontend,
        token: ConnectionToken,
    ) {
        let inserted = {
            let _admission = read_unpoison(&manager.admission);
            if self.closed.load(Ordering::Acquire) || manager.admission_error().is_some() {
                return;
            }

            let mut controls = lock_unpoison(&self.controls);
            controls.connection_close.push(token)
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn request_connect_cancel(&self, request: Arc<OutboundRequest>) {
        lock_unpoison(&self.controls)
            .connect_cancel
            .push_back(request);
        self.notify_reactor();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn request_connection_error(&self, token: ConnectionToken) {
        lock_unpoison(&self.controls)
            .connection_error
            .push_back(token);
        self.notify_reactor();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn request_connection_disconnect(&self, token: ConnectionToken) {
        lock_unpoison(&self.controls)
            .connection_disconnect
            .push_back(token);
        self.notify_reactor();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn request_fail_next_qp_destroy(&self, token: ConnectionToken) {
        lock_unpoison(&self.controls)
            .connection_fail_qp_destroy
            .push_back(token);
        self.notify_reactor();
    }
}
