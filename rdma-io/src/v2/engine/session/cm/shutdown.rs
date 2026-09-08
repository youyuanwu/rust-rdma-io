//! Bounded CM shutdown issuance, cursoring, and terminalization.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{CmState, MemoizedTerminalResult, PendingCmDestruction, SessionManager, lock_unpoison};
use crate::v2::error::Error;

pub(in crate::v2::engine) struct CmShutdownCursor {
    next_class: usize,
    route_slot: usize,
    listener_token: u64,
    routes_complete: bool,
    listeners_complete: bool,
    destruction_listeners_remaining: Option<usize>,
    destruction_listeners_complete: bool,
    terminalized_listeners: HashSet<usize>,
}

impl Default for CmShutdownCursor {
    fn default() -> Self {
        Self {
            next_class: 0,
            route_slot: 0,
            listener_token: 1,
            routes_complete: false,
            listeners_complete: false,
            destruction_listeners_remaining: None,
            destruction_listeners_complete: false,
            terminalized_listeners: HashSet::new(),
        }
    }
}

#[cfg(test)]
pub(super) fn begin(state: &CmState, shared: &SessionManager, outcome: &MemoizedTerminalResult) {
    if state.shutting_down.swap(true, Ordering::AcqRel) {
        return;
    }

    if outcome.is_success() {
        return;
    }
    let pending: Vec<_> = lock_unpoison(&state.pending).drain(..).collect();
    for request in pending {
        request.cancel(terminal_error(outcome));
        drop(request.take_reservation());
    }
    let requests: Vec<_> = state
        .routes
        .occupied_cloned()
        .into_iter()
        .filter_map(|route| route.request())
        .collect();
    for request in requests {
        request.cancel(terminal_error(outcome));
        state.enqueue_cancellation(request);
    }
    let pending_listens: Vec<_> = lock_unpoison(&state.pending_listens).drain(..).collect();
    for request in pending_listens {
        request.complete(Err(terminal_error(outcome)));
    }
    let listeners: Vec<_> = lock_unpoison(&state.listeners).values().cloned().collect();
    for listener in listeners {
        listener.request_close(shared);
    }
}

pub(super) fn start(state: &CmState) {
    state.shutting_down.store(true, Ordering::Release);
}

pub(super) fn service(
    state: &CmState,
    shared: &SessionManager,
    outcome: &MemoizedTerminalResult,
    terminalize_listeners: bool,
    cursor: &mut CmShutdownCursor,
    budget: usize,
) -> usize {
    let mut processed = 0;
    if !terminalize_listeners {
        cursor.destruction_listeners_complete = true;
    }
    while processed < budget {
        let mut selected = false;
        for offset in 0..5 {
            let class = (cursor.next_class + offset) % 5;
            match class {
                0 => {
                    let request = { lock_unpoison(&state.pending).pop_front() };
                    let Some(request) = request else {
                        continue;
                    };
                    request.cancel(terminal_error(outcome));
                    drop(request.take_reservation());
                }
                1 if !cursor.routes_complete => {
                    let (routes, next, complete, scanned) =
                        state.routes.scan_occupied_cloned(cursor.route_slot, 1);
                    cursor.route_slot = next;
                    cursor.routes_complete = complete;
                    if scanned == 0 {
                        continue;
                    }
                    for route in routes {
                        if let Some(request) = route.request() {
                            request.cancel(terminal_error(outcome));
                            state.enqueue_cancellation(request);
                        }
                    }
                }
                2 => {
                    let request = { lock_unpoison(&state.pending_listens).pop_front() };
                    let Some(request) = request else {
                        continue;
                    };
                    request.complete(Err(terminal_error(outcome)));
                }
                3 if !cursor.listeners_complete => {
                    let upper = state.next_listener_token.load(Ordering::Acquire);
                    if cursor.listener_token >= upper {
                        cursor.listeners_complete = true;
                        continue;
                    }
                    let token = cursor.listener_token;
                    cursor.listener_token = cursor.listener_token.saturating_add(1);
                    let listener = { lock_unpoison(&state.listeners).get(&token).cloned() };
                    if let Some(listener) = listener {
                        if terminalize_listeners {
                            let identity = Arc::as_ptr(&listener) as usize;
                            if cursor.terminalized_listeners.insert(identity) {
                                listener.terminalize(outcome);
                            }
                        } else {
                            listener.request_close(shared);
                        }
                    }
                }
                4 if terminalize_listeners && !cursor.destruction_listeners_complete => {
                    let remaining = cursor
                        .destruction_listeners_remaining
                        .get_or_insert_with(|| lock_unpoison(&state.cm_destructions).len());
                    if *remaining == 0 {
                        cursor.destruction_listeners_complete = true;
                        continue;
                    }
                    let listener = {
                        let mut destructions = lock_unpoison(&state.cm_destructions);
                        let Some(pending) = destructions.pop_front() else {
                            cursor.destruction_listeners_complete = true;
                            *remaining = 0;
                            continue;
                        };
                        let listener = pending.listener().cloned();
                        destructions.push_back(pending);
                        listener
                    };
                    *remaining -= 1;
                    if *remaining == 0 {
                        cursor.destruction_listeners_complete = true;
                    }
                    if let Some(listener) = listener {
                        let identity = Arc::as_ptr(&listener) as usize;
                        if cursor.terminalized_listeners.insert(identity) {
                            listener.terminalize(outcome);
                        }
                    }
                }
                _ => continue,
            }
            cursor.next_class = (class + 1) % 5;
            processed += 1;
            selected = true;
            break;
        }
        if !selected {
            break;
        }
    }
    processed
}

pub(super) fn complete(state: &CmState, cursor: &CmShutdownCursor) -> bool {
    cursor.routes_complete
        && cursor.listeners_complete
        && cursor.destruction_listeners_complete
        && lock_unpoison(&state.pending).is_empty()
        && lock_unpoison(&state.pending_listens).is_empty()
}

pub(super) fn terminalize(state: &CmState, outcome: &MemoizedTerminalResult) {
    if outcome.is_success() {
        return;
    }
    let pending: Vec<_> = lock_unpoison(&state.pending).drain(..).collect();
    let requests: Vec<_> = state
        .routes
        .occupied_cloned()
        .into_iter()
        .filter_map(|route| route.request())
        .chain(pending)
        .collect();
    for request in requests {
        drop(request.take_reservation());
        request.cancel(terminal_error(outcome));
    }
    let pending_listens: Vec<_> = lock_unpoison(&state.pending_listens).drain(..).collect();
    for request in pending_listens {
        request.complete(Err(terminal_error(outcome)));
    }
    let mut listeners: Vec<_> = lock_unpoison(&state.listeners).values().cloned().collect();
    let pending_listeners: Vec<_> = lock_unpoison(&state.cm_destructions)
        .iter()
        .filter_map(PendingCmDestruction::listener)
        .cloned()
        .collect();
    for listener in pending_listeners {
        if !listeners
            .iter()
            .any(|active| Arc::ptr_eq(active, &listener))
        {
            listeners.push(listener);
        }
    }
    for listener in listeners {
        listener.terminalize(outcome);
    }
}

fn terminal_error(outcome: &MemoizedTerminalResult) -> Error {
    match outcome.clone().into_result() {
        Err(error) => error,
        Ok(()) => unreachable!("successful engine outcome was filtered"),
    }
}
