//! Session-owned connection registry and QP index.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::super::registry::{
    ConnectionToken, LiveIoConnectionProof, Lookup, PagedRegistry, lock_unpoison,
};
use super::connection::ConnectionState;
use crate::v2::error::{Error, Result};

pub(in crate::v2::engine) struct ConnectionRegistry {
    slots: PagedRegistry<ConnectionToken, Arc<ConnectionState>>,
    qp_index: Mutex<HashMap<u32, ConnectionToken>>,
}

impl ConnectionRegistry {
    pub(in crate::v2::engine) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            slots: PagedRegistry::new(capacity)?,
            qp_index: Mutex::new(HashMap::new()),
        })
    }

    pub(in crate::v2::engine) fn register(
        &self,
        qp_num: u32,
        make: impl FnOnce(ConnectionToken) -> Arc<ConnectionState>,
    ) -> std::result::Result<(ConnectionToken, Arc<ConnectionState>), ConnectionRegistrationFailure>
    {
        if qp_num == 0 {
            return Err(ConnectionRegistrationFailure {
                error: Error::InvalidConfig("provider returned zero qp_num".into()),
                retained: None,
            });
        }
        let (token, state) =
            self.slots
                .allocate_with(make)
                .map_err(|error| ConnectionRegistrationFailure {
                    error,
                    retained: None,
                })?;
        let mut index = lock_unpoison(&self.qp_index);
        match index.entry(qp_num) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(token);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                drop(index);
                return Err(ConnectionRegistrationFailure {
                    error: Error::InvalidConfig(format!("qp_num {qp_num} is already registered")),
                    retained: Some((token, state)),
                });
            }
        }
        Ok((token, state))
    }

    pub(in crate::v2::engine) fn release_unindexed(
        &self,
        token: ConnectionToken,
    ) -> Option<Arc<ConnectionState>> {
        self.slots.release(token, true)
    }

    pub(in crate::v2::engine) fn lookup(
        &self,
        token: ConnectionToken,
    ) -> Lookup<Arc<ConnectionState>> {
        self.slots.lookup_cloned(token)
    }

    pub(in crate::v2::engine) fn lookup_qp(&self, qp_num: u32) -> Option<ConnectionToken> {
        lock_unpoison(&self.qp_index).get(&qp_num).copied()
    }

    pub(in crate::v2::engine) fn prove_live_io(
        &self,
        connection: ConnectionToken,
        qp_num: u32,
    ) -> Option<LiveIoConnectionProof> {
        if !matches!(self.lookup(connection), Lookup::Occupied(_))
            || self.lookup_qp(qp_num) != Some(connection)
        {
            return None;
        }
        Some(LiveIoConnectionProof::new(connection, qp_num))
    }

    pub(in crate::v2::engine) fn release(
        &self,
        token: ConnectionToken,
        qp_num: u32,
    ) -> Option<Arc<ConnectionState>> {
        let mut index = lock_unpoison(&self.qp_index);
        if index.get(&qp_num).copied() == Some(token) {
            index.remove(&qp_num);
        }
        drop(index);
        self.slots.release(token, true)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn detach_qp_index(
        &self,
        token: ConnectionToken,
        qp_num: u32,
    ) -> bool {
        let mut index = lock_unpoison(&self.qp_index);
        if index.get(&qp_num).copied() != Some(token) {
            return false;
        }
        index.remove(&qp_num);
        true
    }

    pub(in crate::v2::engine) fn live(&self) -> usize {
        self.slots.live()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn free(&self) -> usize {
        self.slots.free()
    }

    pub(in crate::v2::engine) fn occupied(&self) -> Vec<Arc<ConnectionState>> {
        self.slots.occupied_cloned()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn set_qp_mapping_for_test(
        &self,
        qp_num: u32,
        token: ConnectionToken,
    ) {
        lock_unpoison(&self.qp_index).insert(qp_num, token);
    }
}

pub(in crate::v2::engine) struct ConnectionRegistrationFailure {
    pub(in crate::v2::engine) error: Error,
    pub(in crate::v2::engine) retained: Option<(ConnectionToken, Arc<ConnectionState>)>,
}
