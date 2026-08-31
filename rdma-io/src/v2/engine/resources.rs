use std::os::unix::io::RawFd;
use std::sync::Arc;

use tokio::io::unix::AsyncFd;

use crate::cm::{EventChannel, RdmaCmDeviceList};

use super::super::context::Context;
use super::super::cq::{Cq, CqBuilder};
use super::super::error::{Error, Result};
use super::super::pd::Pd;
use super::config::{CompletionMode, EngineConfig, ProviderLimits};

pub(super) struct EngineResources {
    #[expect(dead_code, reason = "consumed by the Phase 2 readiness driver")]
    pub(super) cq_async_fd: Option<AsyncFd<RawFd>>,
    #[expect(dead_code, reason = "consumed by the Phase 2 readiness driver")]
    pub(super) cm_async_fd: Option<AsyncFd<RawFd>>,
    pub(super) cq: Cq,
    #[expect(dead_code, reason = "shared by engine connections in later phases")]
    pub(super) pd: Pd,
    #[expect(dead_code, reason = "consumed by the Phase 2 CM driver")]
    pub(super) cm_event_channel: Arc<EventChannel>,
    #[expect(dead_code, reason = "keeps the selected anchored context facade alive")]
    pub(super) context: Context,
}

#[derive(Clone, Copy)]
pub(super) struct ResourceSummary {
    pub(super) contexts: usize,
    pub(super) protection_domains: usize,
    pub(super) completion_queues: usize,
    pub(super) completion_channels: usize,
    pub(super) cm_event_channels: usize,
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
        let cq = match config.completion_mode {
            CompletionMode::Readiness => CqBuilder::new(&context, cq_entries)
                .with_channel()
                .build()?,
            CompletionMode::Polling => CqBuilder::new(&context, cq_entries).build()?,
        };
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
                    Some(AsyncFd::new(cq_fd).map_err(Error::Verbs)?),
                    Some(AsyncFd::new(cm_event_channel.fd()).map_err(Error::Verbs)?),
                )
            }
            CompletionMode::Polling => (None, None),
        };

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
            contexts: 1,
            protection_domains: 1,
            completion_queues: 1,
            completion_channels: usize::from(self.cq.has_channel()),
            cm_event_channels: 1,
        }
    }
}

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
}
