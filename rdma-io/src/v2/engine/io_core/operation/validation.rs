//! Shared validation and work-request inputs for operation submission.
//!
//! `ValidatedOperation` is the single place that turns a caller's MR, optional
//! remote MR, and byte range into a checked local SGE plus the completion
//! opcode that exact CQE routing later demands. Both batch submission and the
//! scalar operation future consume it, so neither re-derives the range, length,
//! or remote-MR rules and neither has to depend on the other.
//!
//! The module is deliberately effect-free: it performs no posting, no
//! reservation, no registry or CQ-credit accounting, and no event publication,
//! so a validation failure is always a zero-call rollback for its caller. The
//! fields stay private behind copy accessors, so a validated value cannot be
//! edited apart from the checks that produced it.

use crate::v2::error::{Error, Result};
use crate::v2::mr::{Mr, RemoteMr};
use crate::wc::WcOpcode;
use crate::wr::{Sge, WrOpcode};

use super::super::OperationKind;

pub(super) struct ValidatedOperation {
    kind: OperationKind,
    sge: Sge,
    remote: Option<RemoteMr>,
    expected_opcode: WcOpcode,
}

impl ValidatedOperation {
    pub(super) fn new(
        kind: OperationKind,
        mr: &Mr,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    ) -> Result<Self> {
        let (offset, len) = range.unwrap_or((0, mr.len()));
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::InvalidConfig("operation range overflow".into()))?;
        if end > mr.len() {
            return Err(Error::InvalidConfig(format!(
                "operation range {offset}..{end} exceeds MR length {}",
                mr.len()
            )));
        }
        let length = u32::try_from(len)
            .map_err(|_| Error::InvalidConfig("operation length does not fit u32".into()))?;
        let address = mr
            .addr()
            .checked_add(offset as u64)
            .ok_or_else(|| Error::InvalidConfig("local SGE address overflow".into()))?;
        let expected_opcode = kind.expected_completion_opcode();
        match kind {
            OperationKind::Write | OperationKind::Read => {
                let remote = remote.ok_or_else(|| {
                    Error::InvalidConfig("RDMA read/write requires a remote MR".into())
                })?;
                if len > remote.len as usize {
                    return Err(Error::InvalidConfig(format!(
                        "operation length {len} exceeds remote MR length {}",
                        remote.len
                    )));
                }
                remote
                    .addr
                    .checked_add(len as u64)
                    .ok_or_else(|| Error::InvalidConfig("remote address range overflow".into()))?;
            }
            OperationKind::Send | OperationKind::Recv if remote.is_some() => {
                return Err(Error::InvalidConfig(
                    "SEND/RECV must not carry a remote MR".into(),
                ));
            }
            OperationKind::Send | OperationKind::Recv => {}
        }
        Ok(Self {
            kind,
            sge: Sge::new(address, length, mr.lkey()),
            remote,
            expected_opcode,
        })
    }

    pub(super) const fn kind(&self) -> OperationKind {
        self.kind
    }

    pub(super) const fn sge(&self) -> Sge {
        self.sge
    }

    pub(super) const fn remote(&self) -> Option<RemoteMr> {
        self.remote
    }

    pub(super) const fn expected_opcode(&self) -> WcOpcode {
        self.expected_opcode
    }
}

impl OperationKind {
    pub(super) const fn expected_completion_opcode(self) -> WcOpcode {
        match self {
            Self::Send => WcOpcode::Send,
            Self::Recv => WcOpcode::Recv,
            Self::Write => WcOpcode::RdmaWrite,
            Self::Read => WcOpcode::RdmaRead,
        }
    }

    pub(super) const fn send_wr_opcode(self) -> Option<WrOpcode> {
        match self {
            Self::Send => Some(WrOpcode::Send),
            Self::Write => Some(WrOpcode::RdmaWrite),
            Self::Read => Some(WrOpcode::RdmaRead),
            Self::Recv => None,
        }
    }
}
