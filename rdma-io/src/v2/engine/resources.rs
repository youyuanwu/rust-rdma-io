use std::os::unix::io::RawFd;
use std::sync::Arc;

use tokio::io::{Interest, unix::AsyncFd};

use crate::cm::{EventChannel, RdmaCmDeviceList};

use super::super::context::Context;
use super::super::cq::{Cq, CqBuilder};
use super::super::error::{Error, Result};
use super::super::pd::Pd;
use super::config::{CompletionMode, EngineConfig, ProviderLimits};
#[cfg(panic = "unwind")]
use super::{RuntimeProbe, probe_runtime};

pub(super) struct EngineResources {
    // Rust drops fields in declaration order. Keep both Tokio adapters before
    // the CQ/channel owners whose raw descriptors they register.
    pub(super) cq_async_fd: Option<AsyncFd<RawFd>>,
    pub(super) cm_async_fd: Option<AsyncFd<RawFd>>,
    pub(super) cq: Arc<Cq>,
    pub(super) pd: Pd,
    pub(super) cm_event_channel: Arc<EventChannel>,
    pub(super) context: Context,
}

#[derive(Clone)]
pub(super) struct EngineResourceRefs {
    #[allow(dead_code, reason = "retains the shared CQ for connection descendants")]
    pub(super) cq: Arc<Cq>,
    pub(super) pd: Pd,
    #[allow(
        dead_code,
        reason = "keeps the shared CM fd alive until after CQ and PD teardown"
    )]
    pub(super) cm_event_channel: Arc<EventChannel>,
    #[allow(
        dead_code,
        reason = "retains the anchored context for connection descendants"
    )]
    pub(super) context: Context,
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone)]
pub(super) struct TestResourceRefs {
    pub(super) cq: Arc<Cq>,
    pub(super) pd: Pd,
    #[allow(
        dead_code,
        reason = "keeps canonical root order while safe test leases are live"
    )]
    pub(super) cm_event_channel: Arc<EventChannel>,
    pub(super) context: Context,
}

#[derive(Clone, Copy)]
pub(super) struct ResourceSummary {
    pub(super) contexts: usize,
    pub(super) protection_domains: usize,
    pub(super) completion_queues: usize,
    pub(super) completion_channels: usize,
    pub(super) cq_notification_fds: usize,
    pub(super) cm_event_channels: usize,
    pub(super) cm_event_fds: usize,
    pub(super) explicit_drivers: usize,
    pub(super) library_owned_tasks: usize,
}

impl EngineResources {
    pub(super) fn build(config: &EngineConfig) -> Result<(Self, ProviderLimits)> {
        let device_list = RdmaCmDeviceList::new().map_err(Error::from)?;
        let inner_context = device_list
            .context_by_name(&config.device_name)
            .map_err(Error::from)?;
        debug_assert!(device_list.contains_context(&inner_context));
        drop(device_list);

        let attr = inner_context.query_device().map_err(Error::from)?;
        let provider = ProviderLimits::from_device_attr(&attr)?;
        config.validate_provider(&provider)?;

        let context = Context::from_inner(inner_context);
        let pd = context.alloc_pd()?;
        let cq_entries = i32::try_from(config.cq_capacity)
            .map_err(|_| Error::InvalidConfig("CQ capacity does not fit i32".into()))?;
        let cq = Arc::new(match config.completion_mode {
            CompletionMode::Readiness => CqBuilder::new(&context, cq_entries)
                .with_channel()
                .build()?,
            CompletionMode::Polling => CqBuilder::new(&context, cq_entries).build()?,
        });
        let cm_event_channel = Arc::new(EventChannel::new().map_err(Error::from)?);
        cm_event_channel.set_nonblocking().map_err(Error::from)?;

        let (cq_async_fd, cm_async_fd) = match config.completion_mode {
            CompletionMode::Readiness => {
                let cq_fd = cq.fd().ok_or_else(|| {
                    Error::InvalidConfig(
                        "readiness mode did not create a CQ completion channel".into(),
                    )
                })?;
                (
                    Some(try_async_fd(cq_fd, "CQ completion channel")?),
                    Some(try_async_fd(cm_event_channel.fd(), "CM event channel")?),
                )
            }
            CompletionMode::Polling => (None, None),
        };

        debug_assert_eq!(
            cq_async_fd.is_some(),
            config.completion_mode == CompletionMode::Readiness
        );
        debug_assert_eq!(
            cm_async_fd.is_some(),
            config.completion_mode == CompletionMode::Readiness
        );

        Ok((
            Self {
                cq_async_fd,
                cm_async_fd,
                cq,
                pd,
                cm_event_channel,
                context,
            },
            provider,
        ))
    }

    pub(super) fn summary(&self) -> ResourceSummary {
        ResourceSummary {
            contexts: usize::from(!self.context.inner().as_raw().is_null()),
            protection_domains: usize::from(!self.pd.inner().as_raw().is_null()),
            completion_queues: usize::from(!self.cq.inner().as_raw().is_null()),
            completion_channels: usize::from(self.cq.has_channel()),
            cq_notification_fds: usize::from(self.cq.fd().is_some()),
            cm_event_channels: usize::from(!self.cm_event_channel.as_raw().is_null()),
            cm_event_fds: usize::from(self.cm_event_channel.fd() >= 0),
            // These are declarative construction invariants rather than
            // runtime-observed task counts. `build()` returns one driver, and
            // v2_no_hidden_spawn independently rejects internal task creation.
            explicit_drivers: 1,
            library_owned_tasks: 0,
        }
    }

    pub(super) fn connection_resource_refs(&self) -> EngineResourceRefs {
        EngineResourceRefs {
            cq: Arc::clone(&self.cq),
            pd: self.pd.clone(),
            cm_event_channel: Arc::clone(&self.cm_event_channel),
            context: self.context.clone(),
        }
    }

    pub(super) fn drop_readiness_adapters(&mut self) {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(adapter) = self.cq_async_fd.as_ref() {
            crate::test_support::destruction::record(
                crate::test_support::destruction::DestructionKind::CqReadinessAdapter,
                *adapter.get_ref() as usize,
            );
        }
        self.cq_async_fd.take();
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(adapter) = self.cm_async_fd.as_ref() {
            crate::test_support::destruction::record(
                crate::test_support::destruction::DestructionKind::CmReadinessAdapter,
                *adapter.get_ref() as usize,
            );
        }
        self.cm_async_fd.take();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn test_resource_refs(&self) -> TestResourceRefs {
        TestResourceRefs {
            cq: Arc::clone(&self.cq),
            pd: self.pd.clone(),
            cm_event_channel: Arc::clone(&self.cm_event_channel),
            context: self.context.clone(),
        }
    }
}

fn try_async_fd(fd: RawFd, resource: &str) -> Result<AsyncFd<RawFd>> {
    #[cfg(panic = "unwind")]
    {
        match probe_runtime(|| AsyncFd::with_interest(fd, Interest::READABLE)) {
            RuntimeProbe::Completed(Ok(async_fd)) => Ok(async_fd),
            RuntimeProbe::Completed(Err(error)) => Err(Error::InvalidConfig(format!(
                "failed to register {resource} with Tokio I/O: {error}"
            ))),
            RuntimeProbe::Panicked => Err(Error::InvalidConfig(format!(
                "readiness mode requires an active Tokio I/O driver for {resource}"
            ))),
        }
    }
    #[cfg(not(panic = "unwind"))]
    {
        // The runtime-presence check has already used Handle::try_current.
        // Tokio has no safe optional-I/O capability query, so do only the
        // required registration and preserve errors from its fallible path.
        AsyncFd::with_interest(fd, Interest::READABLE).map_err(|error| {
            Error::InvalidConfig(format!(
                "failed to register {resource} with Tokio I/O: {error}"
            ))
        })
    }
}

#[cfg(test)]
mod drop_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use rdma_io_sys::ibverbs::ibv_device_attr;

    #[test]
    fn provider_limits_are_copied_before_validation() {
        let attr = ibv_device_attr {
            max_qp: 1_048_560,
            max_qp_wr: 1_048_576,
            max_sge: 32,
            max_cqe: 32_767,
            max_qp_rd_atom: 128,
            max_qp_init_rd_atom: 128,
            ..Default::default()
        };
        let limits = ProviderLimits::from_device_attr(&attr).unwrap();
        assert_eq!(limits.max_cqe, 32_767);
        EngineConfig::new("rxe0".into())
            .validate_provider(&limits)
            .unwrap();
    }

    #[test]
    fn invalid_provider_limits_fail_before_resource_construction() {
        let attr = ibv_device_attr::default();
        assert!(ProviderLimits::from_device_attr(&attr).is_err());
    }

    #[cfg(not(panic = "unwind"))]
    #[test]
    fn abort_build_preserves_fallible_async_fd_registration_errors() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let error = try_async_fd(-1, "invalid test descriptor").unwrap_err();
        assert!(error.to_string().contains("failed to register"));
    }
}
