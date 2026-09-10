use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;

use super::queues::{ProtocolQueueEntry, SessionCommand};
use crate::v2::engine::io_core::OperationCommand;
use crate::v2::engine::registry::ListenerToken;
use crate::v2::engine::session::listener::AcceptRequest;

pub(super) enum ReadyCommand {
    Session(SessionCommand),
    Accept(ListenerToken, Arc<AcceptRequest>),
    Operation(Arc<OperationCommand>, OwnedSemaphorePermit),
    Protocol(ProtocolQueueEntry),
}

use super::*;

impl CommandIngress {
    pub(in crate::v2::engine) fn service_turn_into(
        &self,
        shared: &Arc<EngineFrontendRoot>,
        io: &mut crate::v2::engine::io_core::IoState,
        session: &mut SessionReactorSources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> CommandTurn {
        let mut session_work = false;

        let shutdown_requested = self.shutdown_command_pending.swap(false, Ordering::AcqRel);
        if shutdown_requested {
            session_work = true;
        }

        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(token) = lock_unpoison(&self.controls)
            .connection_fail_qp_destroy
            .pop_front()
        {
            if let Some(Err(error)) = session.connections.with_connection(token, |connection| {
                connection.fail_next_qp_destroy_for_test()
            }) {
                session.manager.begin_driver_failure(error);
            }
            session_work = true;
        }

        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(token) = lock_unpoison(&self.controls)
            .connection_disconnect
            .pop_front()
        {
            if let Some(Err(error)) = session
                .connections
                .with_connection(token, |connection| connection.disconnect_for_test())
            {
                session.manager.begin_driver_failure(error);
            }
            session_work = true;
        }

        let close = if actions.remaining() >= 4 {
            let mut controls = lock_unpoison(&self.controls);
            controls.connection_close.pop()
        } else {
            None
        };
        if let Some(token) = close {
            SessionReactorSources::begin_connection_close_into(
                &session.manager,
                &mut session.cm,
                &mut session.connections,
                token,
                io,
                actions,
            );
            session_work = true;
        }

        let listener_close = {
            let mut controls = lock_unpoison(&self.controls);
            controls.listener_close.pop()
        };
        if let Some(token) = listener_close {
            session.cm.request_listener_close(token);
            session_work = true;
        }

        let listener_work = {
            let mut controls = lock_unpoison(&self.controls);
            controls.listener_work.pop()
        };
        if let Some(token) = listener_work {
            session.cm.enqueue_listener_work(token);
            session_work = true;
        }

        let connect_cancel = lock_unpoison(&self.controls).connect_cancel.pop_front();
        if let Some(request) = connect_cancel {
            session.connections.enqueue_cancellation(request);
            session_work = true;
        }

        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(token) = lock_unpoison(&self.controls).connection_error.pop_front() {
            if let Err(error) = session
                .manager
                .transition_connection_to_error_token(&mut session.connections, token)
            {
                session.manager.begin_driver_failure(error);
            }
            session_work = true;
        }

        let cancellation = {
            let mut controls = lock_unpoison(&self.controls);
            controls.operation_cancel.pop()
        };
        if let Some(token) = cancellation {
            io.cancel_operation(token);
        }

        let terminal_error = self.closed.load(Ordering::Acquire).then(|| {
            shared
                .session
                .admission_error()
                .unwrap_or(Error::DriverShutdown)
        });
        let listener_slot_available = session.cm.listener_slot_available();
        let command = if actions.remaining() != 0 {
            let mut queues = lock_unpoison(&self.queues);
            let mut selected = None;
            for offset in 0..5 {
                let class = (queues.next_class + offset) % 5;
                let required_actions = if class == 3 && terminal_error.is_none() {
                    // Starting an operation can synchronously consume an early
                    // CQE (event + operation wake + close wake) before the
                    // command-completion wake is detached.
                    4
                } else {
                    1
                };
                if !actions.can_accept(required_actions) {
                    continue;
                }
                selected = match class {
                    0 => queues.connect.pop_front().map(ReadyCommand::Session),
                    1 if listener_slot_available || terminal_error.is_some() => {
                        queues.listen.pop_front().map(ReadyCommand::Session)
                    }
                    1 => None,
                    2 => queues
                        .accept
                        .pop_front()
                        .map(|(listener, request)| ReadyCommand::Accept(listener, request)),
                    3 => queues
                        .operation
                        .pop_front()
                        .map(|(command, permit)| ReadyCommand::Operation(command, permit)),
                    4 => queues.protocol.pop_front().map(ReadyCommand::Protocol),
                    _ => unreachable!(),
                };
                if selected.is_some() {
                    queues.next_class = (class + 1) % 5;
                    break;
                }
            }
            selected
        } else {
            None
        };
        if let Some(command) = command {
            match command {
                ReadyCommand::Session(SessionCommand::Connect {
                    request,
                    reservation,
                    ..
                }) => {
                    if let Some(error) = terminal_error {
                        request.complete_failure_into(error, actions);
                    } else {
                        session.connections.enqueue_outbound(request, reservation);
                        session_work = true;
                    }
                }
                ReadyCommand::Session(SessionCommand::Listen {
                    request,
                    _permit: permit,
                }) => {
                    if let Some(error) = terminal_error {
                        request.complete_into(Err(error), actions);
                    } else {
                        match session.cm.reserve_listener(&request) {
                            Ok(token) => {
                                session.cm.enqueue_listen(token, request);
                                session_work = true;
                            }
                            Err(Error::CapacityExhausted) => {
                                lock_unpoison(&self.queues).listen.push_front(
                                    SessionCommand::Listen {
                                        request,
                                        _permit: permit,
                                    },
                                );
                            }
                            Err(error) => request.complete_into(Err(error), actions),
                        }
                    }
                }
                ReadyCommand::Accept(listener, request) => {
                    if let Some(error) = terminal_error {
                        request.release_permit();
                        request.complete_into(Err(error), actions);
                    } else {
                        let result = session.cm.admit_accept(listener, Arc::clone(&request));
                        match result {
                            Ok(()) => {
                                session.cm.enqueue_listener_work(listener);
                                session_work = true;
                            }
                            Err(error) => {
                                request.release_permit();
                                request.complete_into(Err(error), actions);
                            }
                        }
                    }
                }
                #[cfg(any(test, feature = "test-hooks"))]
                ReadyCommand::Session(SessionCommand::TestInstall {
                    request,
                    reservation,
                    ..
                }) => {
                    if let Some(error) = terminal_error {
                        request.complete_failure_into(error, actions);
                    } else {
                        request.execute_into(
                            &session.manager,
                            &mut session.connections,
                            reservation,
                            actions,
                        );
                    }
                }
                ReadyCommand::Operation(command, permit) => {
                    if let Some(error) = terminal_error {
                        command.cancel_before_execution_into(error, actions);
                    } else {
                        command.execute_into(&mut session.connections, io, actions);
                    }
                    drop(permit);
                }
                ReadyCommand::Protocol(entry) => {
                    let mut publication = match entry {
                        ProtocolQueueEntry::Command(command, permit) => {
                            let count = command.len();
                            let mut publication =
                                crate::v2::engine::reactor::DeferredProtocolActions::new(count);
                            if command.receiver_lost() {
                                drop(command);
                            } else if command.is_cancelled() {
                                command.cancel_into(publication.actions_mut());
                            } else if let Some(error) = terminal_error {
                                command.reject_into(error, publication.actions_mut());
                            } else {
                                command.execute_into(
                                    &mut session.connections,
                                    io,
                                    publication.actions_mut(),
                                );
                            }
                            drop(permit);
                            publication
                        }
                        ProtocolQueueEntry::Publication(publication) => publication,
                    };
                    publication.append_bounded_to(actions);
                    if !publication.is_empty() {
                        lock_unpoison(&self.queues)
                            .protocol
                            .push_front(ProtocolQueueEntry::Publication(publication));
                    }
                }
            }
        }

        if session_work {
            self.signal.notify_reactor();
        }
        let has_more = self.has_runnable(session.cm.listener_slot_available());
        if has_more {
            self.signal.notify_reactor();
        }
        CommandTurn {
            session_work,
            has_more,
            shutdown_requested,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_turn(
        &self,
        shared: &Arc<EngineFrontendRoot>,
        io: &mut crate::v2::engine::io_core::IoState,
        session: &mut SessionReactorSources,
    ) -> CommandTurn {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let report = self.service_turn_into(shared, io, session, &mut actions);
        actions.publish();
        report
    }
}
