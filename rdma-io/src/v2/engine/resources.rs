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
use crate::v2::runtime::{RuntimeProbe, probe_runtime};

pub(super) struct EngineResources {
    // I/O progress is extracted into the driver before polling begins.
    io_progress: Option<IoProgressResources>,
    // Rust drops fields in declaration order. Keep the Tokio adapter before
    // the CM channel owner whose raw descriptor it registers.
    pub(super) cm_async_fd: Option<AsyncFd<RawFd>>,
    pub(super) cq: Arc<Cq>,
    pub(super) pd: Pd,
    pub(super) cm_event_channel: Arc<EventChannel>,
    pub(super) context: Context,
}

/// CQ resources owned by the bounded I/O progress component.
pub(super) struct IoProgressResources {
    // Keep the Tokio adapter before the CQ owner whose descriptor it registers.
    pub(super) cq_async_fd: Option<AsyncFd<RawFd>>,
    pub(super) cq: Arc<Cq>,
}

/// CM and root resources owned by the bounded session progress component.
pub(super) struct SessionProgressResources {
    engine: EngineResources,
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

impl EngineResources {
    pub(super) fn build(config: &EngineConfig) -> Result<(Self, ProviderLimits)> {
        let device_list = RdmaCmDeviceList::new().map_err(Error::from_v1)?;
        let inner_context = device_list
            .context_by_name(&config.device_name)
            .map_err(Error::from_v1)?;
        debug_assert!(device_list.contains_context(&inner_context));
        drop(device_list);

        let attr = inner_context.query_device().map_err(Error::from_v1)?;
        let provider = ProviderLimits::from_device_attr(&attr)?;
        config.validate_provider(&provider)?;

        let context = Context::from_anchored(inner_context);
        let pd = context.alloc_pd()?;
        let cq_entries = i32::try_from(config.cq_capacity)
            .map_err(|_| Error::InvalidConfig("CQ capacity does not fit i32".into()))?;
        let cq = Arc::new(match config.completion_mode {
            CompletionMode::Readiness => CqBuilder::new(&context, cq_entries)
                .with_channel()
                .build()?,
            CompletionMode::Polling => CqBuilder::new(&context, cq_entries).build()?,
        });
        let cm_event_channel = Arc::new(EventChannel::new().map_err(Error::from_v1)?);
        cm_event_channel.set_nonblocking().map_err(Error::from_v1)?;

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
                io_progress: Some(IoProgressResources {
                    cq_async_fd,
                    cq: Arc::clone(&cq),
                }),
                cm_async_fd,
                cq,
                pd,
                cm_event_channel,
                context,
            },
            provider,
        ))
    }

    pub(super) fn connection_resource_refs(&self) -> EngineResourceRefs {
        EngineResourceRefs {
            cq: Arc::clone(&self.cq),
            pd: self.pd.clone(),
            cm_event_channel: Arc::clone(&self.cm_event_channel),
            context: self.context.clone(),
        }
    }

    pub(super) fn take_io_progress_resources(&mut self) -> IoProgressResources {
        self.io_progress
            .take()
            .expect("I/O progress resources are taken exactly once")
    }

    pub(super) fn into_session_progress(self) -> SessionProgressResources {
        assert!(
            self.io_progress.is_none(),
            "I/O progress resources must be extracted before session ownership transfer"
        );
        SessionProgressResources { engine: self }
    }

    pub(super) fn drop_readiness_adapters(&mut self) {
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

impl IoProgressResources {
    pub(super) fn drop_readiness_adapter(&mut self) {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(adapter) = self.cq_async_fd.as_ref() {
            crate::test_support::destruction::record(
                crate::test_support::destruction::DestructionKind::CqReadinessAdapter,
                *adapter.get_ref() as usize,
            );
        }
        self.cq_async_fd.take();
    }
}

impl SessionProgressResources {
    pub(super) fn engine(&self) -> &EngineResources {
        &self.engine
    }

    pub(super) fn drop_readiness_adapter(&mut self) {
        self.engine.drop_readiness_adapters();
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
