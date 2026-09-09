//! Bounded CM shutdown issuance, cursoring, and terminalization.

use std::collections::HashSet;

use super::{CmState, MemoizedTerminalResult, SessionManager};
use crate::v2::engine::registry::ListenerToken;
use crate::v2::engine::session::registry::ConnectionRegistry;
use crate::v2::error::Error;

pub(in crate::v2::engine) struct CmShutdownCursor {
    route_slot: usize,
    listener_slot: usize,
    routes_complete: bool,
    listeners_complete: bool,
    destruction_listeners_remaining: Option<usize>,
    destruction_listeners_complete: bool,
    terminalized_listeners: HashSet<ListenerToken>,
}

impl Default for CmShutdownCursor {
    fn default() -> Self {
        Self {
            route_slot: 0,
            listener_slot: 0,
            routes_complete: false,
            listeners_complete: false,
            destruction_listeners_remaining: None,
            destruction_listeners_complete: false,
            terminalized_listeners: HashSet::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum CmShutdownClass {
    PendingOutbound,
    Routes,
    PendingListen,
    Listeners,
    RetainedListeners,
}

impl CmShutdownClass {
    #[cfg(test)]
    pub(in crate::v2::engine) const ALL: [Self; 5] = [
        Self::PendingOutbound,
        Self::Routes,
        Self::PendingListen,
        Self::Listeners,
        Self::RetainedListeners,
    ];

    const fn index(self) -> usize {
        match self {
            Self::PendingOutbound => 0,
            Self::Routes => 1,
            Self::PendingListen => 2,
            Self::Listeners => 3,
            Self::RetainedListeners => 4,
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::v2::engine) struct CmShutdownSnapshot {
    limits: [usize; 5],
}

impl CmShutdownSnapshot {
    pub(in crate::v2::engine) fn count(self, class: CmShutdownClass) -> usize {
        self.limits[class.index()]
    }
}

#[cfg(test)]
pub(super) fn begin(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    _shared: &SessionManager,
    outcome: &MemoizedTerminalResult,
) {
    if std::mem::replace(&mut state.shutting_down, true) {
        return;
    }

    if outcome.is_success() {
        return;
    }
    let pending: Vec<_> = connections.drain_pending_outbound().collect();
    for (request, _reservation) in pending {
        request.cancel(terminal_error(outcome));
    }
    let requests: Vec<_> = connections
        .outbound_routes()
        .into_iter()
        .filter_map(|token| connections.outbound_request(token))
        .collect();
    for request in requests {
        request.cancel(terminal_error(outcome));
        connections.enqueue_cancellation(request);
    }
    let pending_listens: Vec<_> = state.pending_listens.drain(..).collect();
    for (token, request) in pending_listens {
        request.complete(Err(terminal_error(outcome)));
        state.listeners.release(token, false);
    }
    let listeners = state.listeners.occupied();
    for token in listeners {
        let enqueue = state.listeners.get_mut(token).is_some_and(|listener| {
            listener.close_accept_admission(terminal_error(outcome));
            listener.request_close()
        });
        if enqueue {
            state.enqueue_listener_work(token);
        }
    }
}

pub(super) fn start(state: &mut CmState) {
    state.shutting_down = true;
}

pub(super) fn snapshot(
    state: &CmState,
    connections: &ConnectionRegistry,
    terminalize_listeners: bool,
    cursor: &CmShutdownCursor,
    budget: usize,
) -> CmShutdownSnapshot {
    let retained_listener_count = if terminalize_listeners && !cursor.destruction_listeners_complete
    {
        cursor
            .destruction_listeners_remaining
            .unwrap_or(state.cm_destructions.len())
            .max(1)
    } else {
        0
    };
    CmShutdownSnapshot {
        limits: [
            connections.pending_outbound_count().min(budget),
            usize::from(!cursor.routes_complete).saturating_mul(budget),
            state.pending_listens.len().min(budget),
            usize::from(!cursor.listeners_complete).saturating_mul(budget),
            retained_listener_count.min(budget),
        ],
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "bounded shutdown keeps ownership inputs explicit"
)]
pub(super) fn service_class(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    _shared: &SessionManager,
    outcome: &MemoizedTerminalResult,
    terminalize_listeners: bool,
    cursor: &mut CmShutdownCursor,
    class: CmShutdownClass,
    budget: usize,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> usize {
    let mut processed = 0;
    if !terminalize_listeners {
        cursor.destruction_listeners_complete = true;
    }
    while processed < budget && actions.remaining() >= 8 {
        match class {
            CmShutdownClass::PendingOutbound => {
                let pending = connections.pop_outbound();
                let Some((request, _reservation)) = pending else {
                    break;
                };
                request.cancel_into(terminal_error(outcome), actions);
            }
            CmShutdownClass::Routes if !cursor.routes_complete => {
                let (routes, next, complete, scanned) =
                    connections.scan_outbound_routes(cursor.route_slot, 1);
                cursor.route_slot = next;
                cursor.routes_complete = complete;
                if scanned == 0 {
                    break;
                }
                for token in routes {
                    if let Some(request) = connections.outbound_request(token) {
                        request.cancel_into(terminal_error(outcome), actions);
                        connections.enqueue_cancellation(request);
                    }
                }
            }
            CmShutdownClass::PendingListen => {
                let pending = state.pending_listens.pop_front();
                let Some((token, request)) = pending else {
                    break;
                };
                request.complete_into(Err(terminal_error(outcome)), actions);
                state.listeners.release(token, false);
            }
            CmShutdownClass::Listeners if !cursor.listeners_complete => {
                let (tokens, next, complete, scanned) =
                    state.listeners.scan_occupied(cursor.listener_slot, 1);
                cursor.listener_slot = next;
                cursor.listeners_complete = complete;
                if scanned == 0 {
                    break;
                }
                for token in tokens {
                    let Some(listener) = state.listeners.get_mut(token) else {
                        continue;
                    };
                    listener.close_accept_admission(shutdown_listener_error(outcome));
                    if terminalize_listeners && !cursor.terminalized_listeners.contains(&token) {
                        // One listener-owned waiter is one shutdown work unit.
                        // Keep the outer rotating source budget authoritative
                        // instead of hiding a fixed inner drain behind it.
                        listener.terminalize_waiters_into(outcome, actions, 1);
                        if listener.has_waiters() {
                            cursor.listener_slot = token.slot as usize;
                            cursor.listeners_complete = false;
                            processed += 1;
                            break;
                        }
                        cursor.terminalized_listeners.insert(token);
                    }
                    let enqueue = listener.request_close() || listener.has_work();
                    if enqueue {
                        state.enqueue_listener_work(token);
                    }
                }
            }
            CmShutdownClass::RetainedListeners
                if terminalize_listeners && !cursor.destruction_listeners_complete =>
            {
                let remaining = cursor
                    .destruction_listeners_remaining
                    .get_or_insert(state.cm_destructions.len());
                if *remaining == 0 {
                    cursor.destruction_listeners_complete = true;
                    break;
                }
                let listener = {
                    let Some(pending) = state.cm_destructions.pop_front() else {
                        cursor.destruction_listeners_complete = true;
                        *remaining = 0;
                        break;
                    };
                    let listener = pending.listener();
                    state.cm_destructions.push_back(pending);
                    listener
                };
                *remaining -= 1;
                if *remaining == 0 {
                    cursor.destruction_listeners_complete = true;
                }
                if let Some(listener) = listener {
                    if !cursor.terminalized_listeners.contains(&listener) {
                        let complete = state.listeners.get_mut(listener).is_none_or(|listener| {
                            listener.terminalize_waiters_into(outcome, actions, 1);
                            !listener.has_waiters()
                        });
                        if complete {
                            cursor.terminalized_listeners.insert(listener);
                        } else {
                            *remaining += 1;
                            processed += 1;
                            break;
                        }
                    }
                }
            }
            _ => break,
        }
        processed += 1;
    }
    processed
}

pub(super) fn complete(
    state: &CmState,
    connections: &ConnectionRegistry,
    cursor: &CmShutdownCursor,
) -> bool {
    cursor.routes_complete
        && cursor.listeners_complete
        && cursor.destruction_listeners_complete
        && connections.pending_outbound_count() == 0
        && state.pending_listens.is_empty()
}

pub(super) fn terminalize_into(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    outcome: &MemoizedTerminalResult,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) {
    if outcome.is_success() {
        return;
    }
    let pending: Vec<_> = connections.drain_pending_outbound().collect();
    let requests: Vec<_> = connections
        .outbound_routes()
        .into_iter()
        .filter_map(|token| connections.outbound_request(token))
        .chain(pending.into_iter().map(|(request, _reservation)| request))
        .collect();
    for request in requests {
        request.cancel_into(terminal_error(outcome), actions);
    }
    let pending_listens: Vec<_> = state.pending_listens.drain(..).collect();
    for (token, request) in pending_listens {
        request.complete_into(Err(terminal_error(outcome)), actions);
        state.listeners.release(token, false);
    }
    let listeners = state.listeners.occupied();
    for token in listeners {
        let enqueue = {
            let Some(listener) = state.listeners.get_mut(token) else {
                continue;
            };
            listener.terminalize_waiters_into(outcome, actions, usize::MAX);
            while listener.has_waiters() {
                listener.terminalize_waiters_into(outcome, actions, usize::MAX);
            }
            listener.request_close() || listener.has_work()
        };
        if enqueue {
            state.enqueue_listener_work(token);
        };
    }
}

fn terminal_error(outcome: &MemoizedTerminalResult) -> Error {
    match outcome.clone().into_result() {
        Err(error) => error,
        Ok(()) => unreachable!("successful engine outcome was filtered"),
    }
}

fn shutdown_listener_error(outcome: &MemoizedTerminalResult) -> Error {
    outcome
        .clone()
        .into_result()
        .err()
        .unwrap_or(Error::DriverShutdown)
}
