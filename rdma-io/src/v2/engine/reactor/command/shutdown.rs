use std::sync::atomic::Ordering;

use super::CommandIngress;
use super::queues::{ProtocolQueueEntry, SessionCommand};
use crate::v2::engine::Error;
use crate::v2::engine::registry::lock_unpoison;

impl CommandIngress {
    pub(in crate::v2::engine) fn request_shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::AcqRel) {
            self.shutdown_command_pending.store(true, Ordering::Release);
            self.signal.notify_reactor();
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn close_admission(&self) {
        self.close_admission_with(Error::DriverShutdown);
    }

    pub(in crate::v2::engine) fn close_admission_with(&self, error: Error) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            *lock_unpoison(&self.closed_error) = Some(error);
            self.connect_permits.close();
            self.connection_permits.close();
            self.listen_permits.close();
            self.operation_permits.close();
        }
    }

    pub(in crate::v2::engine) fn admission_error(&self) -> Option<Error> {
        lock_unpoison(&self.closed_error).clone()
    }

    pub(in crate::v2::engine) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub(in crate::v2::engine) fn drain_ordinary(&self, error: Error) {
        let mut queues = lock_unpoison(&self.queues);
        let commands: Vec<_> = queues.connect.drain(..).collect();
        let listens: Vec<_> = queues.listen.drain(..).collect();
        let accepts: Vec<_> = queues.accept.drain(..).collect();
        let operations: Vec<_> = queues.operation.drain(..).collect();
        let protocols: Vec<_> = queues.protocol.drain(..).collect();
        drop(queues);
        for command in commands.into_iter().chain(listens) {
            match command {
                SessionCommand::Connect { request, .. } => {
                    request.complete_failure(error.clone());
                }
                SessionCommand::Listen { request, .. } => {
                    request.complete(Err(error.clone()));
                }
                #[cfg(any(test, feature = "test-hooks"))]
                SessionCommand::TestInstall { request, .. } => {
                    request.complete_failure(error.clone());
                }
            }
        }
        for (_, request) in accepts {
            request.release_permit();
            request.complete(Err(error.clone()));
        }
        for (command, _permit) in operations {
            command.cancel_before_execution(error.clone());
        }
        for entry in protocols {
            match entry {
                ProtocolQueueEntry::Command(command, _permit) => {
                    command.reject_after_pending(error.clone());
                }
                ProtocolQueueEntry::Publication(actions) => actions.publish_synchronously(),
            }
        }
    }

    pub(in crate::v2::engine) fn drain_ordinary_into(
        &self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let mut queues = lock_unpoison(&self.queues);
        let commands: Vec<_> = queues.connect.drain(..).collect();
        let listens: Vec<_> = queues.listen.drain(..).collect();
        let accepts: Vec<_> = queues.accept.drain(..).collect();
        let operations: Vec<_> = queues.operation.drain(..).collect();
        let protocols: Vec<_> = queues.protocol.drain(..).collect();
        drop(queues);
        for command in commands.into_iter().chain(listens) {
            match command {
                SessionCommand::Connect { request, .. } => {
                    request.complete_failure_into(error.clone(), actions);
                }
                SessionCommand::Listen { request, .. } => {
                    request.complete_into(Err(error.clone()), actions);
                }
                #[cfg(any(test, feature = "test-hooks"))]
                SessionCommand::TestInstall { request, .. } => {
                    request.complete_failure_into(error.clone(), actions);
                }
            }
        }
        for (_, request) in accepts {
            request.release_permit();
            request.complete_into(Err(error.clone()), actions);
        }
        for (command, _permit) in operations {
            command.cancel_before_execution_into(error.clone(), actions);
        }
        for entry in protocols {
            match entry {
                ProtocolQueueEntry::Command(command, _permit) => {
                    command.reject_into(error.clone(), actions);
                }
                ProtocolQueueEntry::Publication(mut pending) => {
                    pending.append_bounded_to(actions);
                    assert!(
                        pending.is_empty(),
                        "synchronous driver-drop actions have unbounded capacity"
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn close_ordinary(&self, error: Error) {
        self.close_admission();
        self.drain_ordinary(error);
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_listener_closes(&self) -> usize {
        lock_unpoison(&self.controls).listener_close.len()
    }
}
