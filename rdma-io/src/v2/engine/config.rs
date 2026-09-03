use std::alloc::Layout;
use std::time::Duration;

use rdma_io_sys::ibverbs::ibv_device_attr;

use super::super::error::{Error, Result};
use crate::cm::ConnParam;

pub(crate) const DEFAULT_MAX_LIVE_CONNECTIONS: usize = 256;
pub(crate) const DEFAULT_MAX_INFLIGHT_OPERATIONS: usize = 16_384;
pub(crate) const DEFAULT_CQ_CAPACITY: usize = 16_384;
pub(crate) const DEFAULT_WORK_BUDGET: usize = 32;
pub(crate) const DEFAULT_MISSING_CQE_DEADLINE: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_CONNECTION_DRAIN_DEADLINE: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_ENGINE_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(30);

const MAX_LIVE_CONNECTIONS: usize = 1_048_576;
const MAX_INFLIGHT_OPERATIONS: usize = 16_777_216;
const MAX_CQ_CAPACITY: usize = 16_777_216;
const MAX_WORK_BUDGET: usize = 4_096;
const REGISTRY_PAGE_SIZE: usize = 256;

/// Completion integration used by an [`RdmaEngine`](super::RdmaEngine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionMode {
    /// Use one completion channel registered with Tokio I/O readiness.
    #[default]
    Readiness,
    /// Poll the shared CQ directly without a completion channel.
    Polling,
}

/// Per-connection QP capacity and RDMA-CM handshake configuration.
///
/// Defaults are 19 send WRs, 34 receive WRs, one SGE in each direction,
/// responder/initiator depth 1, and retry/RNR retry 7. The 19/34 values
/// compose exactly with the default message pools: `16 + 2 + 1` sends and
/// `32 + 2` receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdmaConnectionConfig {
    pub(crate) max_send_wr: usize,
    pub(crate) max_recv_wr: usize,
    pub(crate) max_send_sge: usize,
    pub(crate) max_recv_sge: usize,
    pub(crate) responder_resources: usize,
    pub(crate) initiator_depth: usize,
    pub(crate) retry_count: usize,
    pub(crate) rnr_retry_count: usize,
}

impl Default for RdmaConnectionConfig {
    fn default() -> Self {
        Self {
            max_send_wr: 19,
            max_recv_wr: 34,
            max_send_sge: 1,
            max_recv_sge: 1,
            responder_resources: 1,
            initiator_depth: 1,
            retry_count: 7,
            rnr_retry_count: 7,
        }
    }
}

impl RdmaConnectionConfig {
    /// Set maximum send WRs in `1..=1_048_576`.
    pub fn max_send_wr(mut self, value: usize) -> Self {
        self.max_send_wr = value;
        self
    }

    /// Set maximum receive WRs in `1..=1_048_576`.
    pub fn max_recv_wr(mut self, value: usize) -> Self {
        self.max_recv_wr = value;
        self
    }

    /// Set maximum send SGEs in `1..=32`.
    pub fn max_send_sge(mut self, value: usize) -> Self {
        self.max_send_sge = value;
        self
    }

    /// Set maximum receive SGEs in `1..=32`.
    pub fn max_recv_sge(mut self, value: usize) -> Self {
        self.max_recv_sge = value;
        self
    }

    /// Set responder resources in `0..=255`.
    pub fn responder_resources(mut self, value: usize) -> Self {
        self.responder_resources = value;
        self
    }

    /// Set initiator depth in `0..=255`.
    pub fn initiator_depth(mut self, value: usize) -> Self {
        self.initiator_depth = value;
        self
    }

    /// Set the RDMA-CM retry count in `0..=7`.
    pub fn retry_count(mut self, value: usize) -> Self {
        self.retry_count = value;
        self
    }

    /// Set the RDMA-CM RNR retry count in `0..=7`.
    ///
    /// Seven retains the verbs infinite-retry encoding.
    pub fn rnr_retry_count(mut self, value: usize) -> Self {
        self.rnr_retry_count = value;
        self
    }

    pub(crate) fn conn_param(&self) -> Result<ConnParam> {
        Ok(ConnParam {
            responder_resources: u8::try_from(self.responder_resources)
                .map_err(|_| invalid("responder resources do not fit u8"))?,
            initiator_depth: u8::try_from(self.initiator_depth)
                .map_err(|_| invalid("initiator depth does not fit u8"))?,
            retry_count: u8::try_from(self.retry_count)
                .map_err(|_| invalid("retry count does not fit u8"))?,
            rnr_retry_count: u8::try_from(self.rnr_retry_count)
                .map_err(|_| invalid("RNR retry count does not fit u8"))?,
        })
    }

    pub(crate) fn validate(
        &self,
        engine: &EngineConfig,
        provider: Option<&ProviderLimits>,
    ) -> Result<()> {
        validate_range("maximum send WRs", self.max_send_wr, 1, 1_048_576)?;
        validate_range("maximum receive WRs", self.max_recv_wr, 1, 1_048_576)?;
        validate_range("maximum send SGEs", self.max_send_sge, 1, 32)?;
        validate_range("maximum receive SGEs", self.max_recv_sge, 1, 32)?;
        validate_range("responder resources", self.responder_resources, 0, 255)?;
        validate_range("initiator depth", self.initiator_depth, 0, 255)?;
        validate_range("retry count", self.retry_count, 0, 7)?;
        validate_range("RNR retry count", self.rnr_retry_count, 0, 7)?;

        u32::try_from(self.max_send_wr).map_err(|_| invalid("maximum send WRs do not fit u32"))?;
        u32::try_from(self.max_recv_wr)
            .map_err(|_| invalid("maximum receive WRs do not fit u32"))?;
        u32::try_from(self.max_send_sge)
            .map_err(|_| invalid("maximum send SGEs do not fit u32"))?;
        u32::try_from(self.max_recv_sge)
            .map_err(|_| invalid("maximum receive SGEs do not fit u32"))?;
        u8::try_from(self.responder_resources)
            .map_err(|_| invalid("responder resources do not fit u8"))?;
        u8::try_from(self.initiator_depth)
            .map_err(|_| invalid("initiator depth does not fit u8"))?;
        u8::try_from(self.retry_count).map_err(|_| invalid("retry count does not fit u8"))?;
        u8::try_from(self.rnr_retry_count)
            .map_err(|_| invalid("RNR retry count does not fit u8"))?;

        let qp_positions = self
            .max_send_wr
            .checked_add(self.max_recv_wr)
            .ok_or_else(|| invalid("connection send-plus-receive capacity overflow"))?;
        if qp_positions > engine.max_inflight_operations {
            return Err(invalid(format!(
                "connection send-plus-receive capacity ({qp_positions}) exceeds engine in-flight capacity ({})",
                engine.max_inflight_operations
            )));
        }
        if qp_positions > engine.cq_capacity {
            return Err(invalid(format!(
                "connection send-plus-receive capacity ({qp_positions}) exceeds engine CQ capacity ({})",
                engine.cq_capacity
            )));
        }

        if let Some(provider) = provider {
            provider.validate_connection(self)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EngineConfig {
    pub(crate) device_name: String,
    pub(crate) completion_mode: CompletionMode,
    pub(crate) max_live_connections: usize,
    pub(crate) max_inflight_operations: usize,
    pub(crate) cq_capacity: usize,
    pub(crate) cq_completion_budget: usize,
    pub(crate) cm_event_budget: usize,
    pub(crate) reclamation_budget: usize,
    pub(crate) completion_dispatch_budget: usize,
    pub(crate) missing_cqe_deadline: Duration,
    pub(crate) connection_drain_deadline: Duration,
    pub(crate) shutdown_deadline: Duration,
}

impl EngineConfig {
    pub(crate) fn new(device_name: String) -> Self {
        Self {
            device_name,
            completion_mode: CompletionMode::Readiness,
            max_live_connections: DEFAULT_MAX_LIVE_CONNECTIONS,
            max_inflight_operations: DEFAULT_MAX_INFLIGHT_OPERATIONS,
            cq_capacity: DEFAULT_CQ_CAPACITY,
            cq_completion_budget: DEFAULT_WORK_BUDGET,
            cm_event_budget: DEFAULT_WORK_BUDGET,
            reclamation_budget: DEFAULT_WORK_BUDGET,
            completion_dispatch_budget: DEFAULT_WORK_BUDGET,
            missing_cqe_deadline: DEFAULT_MISSING_CQE_DEADLINE,
            connection_drain_deadline: DEFAULT_CONNECTION_DRAIN_DEADLINE,
            shutdown_deadline: DEFAULT_ENGINE_SHUTDOWN_DEADLINE,
        }
    }

    pub(crate) fn validate_without_provider(&self) -> Result<()> {
        if self.device_name.trim().is_empty() {
            return Err(invalid("RDMA device name must not be empty"));
        }
        validate_range(
            "maximum live connections",
            self.max_live_connections,
            1,
            MAX_LIVE_CONNECTIONS,
        )?;
        validate_range(
            "maximum in-flight operations",
            self.max_inflight_operations,
            2,
            MAX_INFLIGHT_OPERATIONS,
        )?;
        validate_range("CQ capacity", self.cq_capacity, 2, MAX_CQ_CAPACITY)?;
        validate_range(
            "CQ completion budget",
            self.cq_completion_budget,
            1,
            MAX_WORK_BUDGET,
        )?;
        validate_range("CM event budget", self.cm_event_budget, 1, MAX_WORK_BUDGET)?;
        validate_range(
            "reclamation budget",
            self.reclamation_budget,
            1,
            MAX_WORK_BUDGET,
        )?;
        validate_range(
            "completion dispatch budget",
            self.completion_dispatch_budget,
            1,
            MAX_WORK_BUDGET,
        )?;
        validate_duration(
            "missing-CQE deadline",
            self.missing_cqe_deadline,
            Duration::from_secs(1),
            Duration::from_secs(24 * 60 * 60),
        )?;
        validate_duration(
            "connection drain deadline",
            self.connection_drain_deadline,
            Duration::from_millis(1),
            Duration::from_secs(5 * 60),
        )?;
        validate_duration(
            "engine shutdown deadline",
            self.shutdown_deadline,
            Duration::from_millis(1),
            Duration::from_secs(10 * 60),
        )?;
        if self.max_inflight_operations > self.cq_capacity {
            return Err(invalid(format!(
                "maximum in-flight operations ({}) exceeds CQ capacity ({})",
                self.max_inflight_operations, self.cq_capacity
            )));
        }
        i32::try_from(self.cq_capacity)
            .map_err(|_| invalid("CQ capacity does not fit the provider ABI"))?;

        validate_registry_layout(self.max_live_connections, "connection")?;
        validate_registry_layout(self.max_inflight_operations, "operation")?;

        let defaults = RdmaConnectionConfig::default();
        let per_connection = defaults
            .max_send_wr
            .checked_add(defaults.max_recv_wr)
            .ok_or_else(|| invalid("default connection capacity overflow"))?;
        if per_connection != 53 {
            return Err(invalid("default connection capacity must equal 53"));
        }
        let occupied = self
            .max_live_connections
            .checked_mul(per_connection)
            .ok_or_else(|| invalid("default aggregate QP capacity overflow"))?;
        if self.max_live_connections == DEFAULT_MAX_LIVE_CONNECTIONS
            && self.max_inflight_operations == DEFAULT_MAX_INFLIGHT_OPERATIONS
        {
            if occupied != 13_568 {
                return Err(invalid("default aggregate QP capacity must equal 13,568"));
            }
            let headroom = self
                .max_inflight_operations
                .checked_sub(occupied)
                .ok_or_else(|| invalid("default engine capacity has no message headroom"))?;
            if headroom != 2_816 {
                return Err(invalid("default engine headroom must equal 2,816"));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_provider(&self, provider: &ProviderLimits) -> Result<()> {
        if self.max_live_connections > provider.max_qp {
            return Err(invalid(format!(
                "maximum live connections ({}) exceeds provider max_qp ({})",
                self.max_live_connections, provider.max_qp
            )));
        }
        if self.cq_capacity > provider.max_cqe {
            return Err(invalid(format!(
                "CQ capacity ({}) exceeds provider max_cqe ({})",
                self.cq_capacity, provider.max_cqe
            )));
        }
        provider.validate_connection(&RdmaConnectionConfig::default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderLimits {
    pub(crate) max_qp: usize,
    pub(crate) max_qp_wr: usize,
    pub(crate) max_sge: usize,
    pub(crate) max_cqe: usize,
    pub(crate) max_qp_rd_atom: usize,
    pub(crate) max_qp_init_rd_atom: usize,
}

impl ProviderLimits {
    pub(crate) fn from_device_attr(attr: &ibv_device_attr) -> Result<Self> {
        let max_qp = attr.max_qp;
        let max_qp_wr = attr.max_qp_wr;
        let max_sge = attr.max_sge;
        let max_cqe = attr.max_cqe;
        let max_qp_rd_atom = attr.max_qp_rd_atom;
        let max_qp_init_rd_atom = attr.max_qp_init_rd_atom;
        Ok(Self {
            max_qp: positive_provider_limit("max_qp", max_qp)?,
            max_qp_wr: positive_provider_limit("max_qp_wr", max_qp_wr)?,
            max_sge: positive_provider_limit("max_sge", max_sge)?,
            max_cqe: positive_provider_limit("max_cqe", max_cqe)?,
            max_qp_rd_atom: nonnegative_provider_limit("max_qp_rd_atom", max_qp_rd_atom)?,
            max_qp_init_rd_atom: nonnegative_provider_limit(
                "max_qp_init_rd_atom",
                max_qp_init_rd_atom,
            )?,
        })
    }

    fn validate_connection(&self, config: &RdmaConnectionConfig) -> Result<()> {
        validate_provider_bound("maximum send WRs", config.max_send_wr, self.max_qp_wr)?;
        validate_provider_bound("maximum receive WRs", config.max_recv_wr, self.max_qp_wr)?;
        validate_provider_bound("maximum send SGEs", config.max_send_sge, self.max_sge)?;
        validate_provider_bound("maximum receive SGEs", config.max_recv_sge, self.max_sge)?;
        validate_provider_bound(
            "responder resources",
            config.responder_resources,
            self.max_qp_rd_atom,
        )?;
        validate_provider_bound(
            "initiator depth",
            config.initiator_depth,
            self.max_qp_init_rd_atom,
        )
    }
}

fn validate_range(name: &str, value: usize, min: usize, max: usize) -> Result<()> {
    if !(min..=max).contains(&value) {
        return Err(invalid(format!(
            "{name} must be in the inclusive range {min}..={max}, got {value}"
        )));
    }
    Ok(())
}

fn validate_duration(name: &str, value: Duration, min: Duration, max: Duration) -> Result<()> {
    if value < min || value > max {
        return Err(invalid(format!(
            "{name} must be in the inclusive range {min:?}..={max:?}, got {value:?}"
        )));
    }
    Ok(())
}

fn validate_provider_bound(name: &str, value: usize, provider_max: usize) -> Result<()> {
    if value > provider_max {
        return Err(invalid(format!(
            "{name} ({value}) exceeds provider limit ({provider_max})"
        )));
    }
    Ok(())
}

fn validate_registry_layout(capacity: usize, name: &str) -> Result<()> {
    let pages = capacity
        .checked_add(REGISTRY_PAGE_SIZE - 1)
        .and_then(|value| value.checked_div(REGISTRY_PAGE_SIZE))
        .ok_or_else(|| invalid(format!("{name} registry page-directory overflow")))?;
    Layout::array::<usize>(pages)
        .map_err(|_| invalid(format!("{name} registry page-directory layout overflow")))?;
    Ok(())
}

fn positive_provider_limit(name: &str, value: i32) -> Result<usize> {
    if value <= 0 {
        return Err(invalid(format!(
            "provider reported invalid {name} value {value}"
        )));
    }
    Ok(value as usize)
}

fn nonnegative_provider_limit(name: &str, value: i32) -> Result<usize> {
    if value < 0 {
        return Err(invalid(format!(
            "provider reported invalid {name} value {value}"
        )));
    }
    Ok(value as usize)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidConfig(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_provider() -> ProviderLimits {
        ProviderLimits {
            max_qp: 1_048_560,
            max_qp_wr: 1_048_576,
            max_sge: 32,
            max_cqe: 32_767,
            max_qp_rd_atom: 128,
            max_qp_init_rd_atom: 128,
        }
    }

    #[test]
    fn defaults_match_specification_arithmetic() {
        let config = EngineConfig::new("rxe0".into());
        config.validate_without_provider().unwrap();
        config.validate_provider(&default_provider()).unwrap();
        let connection = RdmaConnectionConfig::default();
        assert_eq!(connection.max_send_wr, 19);
        assert_eq!(connection.max_recv_wr, 34);
        assert_eq!(19 + 34, 53);
        assert_eq!(256 * 53, 13_568);
        assert_eq!(16_384 - 13_568, 2_816);
    }

    #[test]
    fn validates_every_engine_bound_and_relationship() {
        let mut config = EngineConfig::new("rxe0".into());
        config.max_live_connections = 0;
        assert!(config.validate_without_provider().is_err());
        config.max_live_connections = 1_048_577;
        assert!(config.validate_without_provider().is_err());

        let mut config = EngineConfig::new("rxe0".into());
        config.max_inflight_operations = 1;
        assert!(config.validate_without_provider().is_err());
        config.max_inflight_operations = 16_777_217;
        assert!(config.validate_without_provider().is_err());

        let mut config = EngineConfig::new("rxe0".into());
        config.cq_capacity = 1;
        assert!(config.validate_without_provider().is_err());
        config.cq_capacity = 16_777_217;
        assert!(config.validate_without_provider().is_err());

        let mut config = EngineConfig::new("rxe0".into());
        config.max_inflight_operations = 65;
        config.cq_capacity = 64;
        assert!(config.validate_without_provider().is_err());

        let mut config = EngineConfig::new("rxe0".into());
        config.cq_completion_budget = 0;
        assert!(config.validate_without_provider().is_err());
        config.cq_completion_budget = 4_097;
        assert!(config.validate_without_provider().is_err());

        let mut minimum = EngineConfig::new("rxe0".into());
        minimum.max_live_connections = 1;
        minimum.max_inflight_operations = 2;
        minimum.cq_capacity = 2;
        minimum.cq_completion_budget = 1;
        minimum.cm_event_budget = 1;
        minimum.reclamation_budget = 1;
        minimum.completion_dispatch_budget = 1;
        minimum.validate_without_provider().unwrap();

        let mut maximum = EngineConfig::new("rxe0".into());
        maximum.max_live_connections = 1_048_576;
        maximum.max_inflight_operations = 16_777_216;
        maximum.cq_capacity = 16_777_216;
        maximum.cq_completion_budget = 4_096;
        maximum.cm_event_budget = 4_096;
        maximum.reclamation_budget = 4_096;
        maximum.completion_dispatch_budget = 4_096;
        maximum.validate_without_provider().unwrap();

        for mutate in [
            |config: &mut EngineConfig| config.cm_event_budget = 0,
            |config: &mut EngineConfig| config.reclamation_budget = 0,
            |config: &mut EngineConfig| config.completion_dispatch_budget = 0,
        ] {
            let mut config = EngineConfig::new("rxe0".into());
            mutate(&mut config);
            assert!(config.validate_without_provider().is_err());
        }
    }

    #[test]
    fn validates_deadline_bounds() {
        let mut config = EngineConfig::new("rxe0".into());
        config.missing_cqe_deadline = Duration::from_millis(999);
        assert!(config.validate_without_provider().is_err());
        config.missing_cqe_deadline = Duration::from_secs(24 * 60 * 60 + 1);
        assert!(config.validate_without_provider().is_err());

        let mut config = EngineConfig::new("rxe0".into());
        config.connection_drain_deadline = Duration::ZERO;
        assert!(config.validate_without_provider().is_err());
        config.connection_drain_deadline = Duration::from_secs(301);
        assert!(config.validate_without_provider().is_err());

        let mut config = EngineConfig::new("rxe0".into());
        config.shutdown_deadline = Duration::ZERO;
        assert!(config.validate_without_provider().is_err());
        config.shutdown_deadline = Duration::from_secs(601);
        assert!(config.validate_without_provider().is_err());
    }

    #[test]
    fn validates_connection_bounds_and_provider_limits() {
        let engine = EngineConfig::new("rxe0".into());
        let provider = default_provider();
        RdmaConnectionConfig::default()
            .validate(&engine, Some(&provider))
            .unwrap();

        assert!(
            RdmaConnectionConfig::default()
                .max_send_wr(0)
                .validate(&engine, Some(&provider))
                .is_err()
        );
        assert!(
            RdmaConnectionConfig::default()
                .max_recv_wr(1_048_577)
                .validate(&engine, Some(&provider))
                .is_err()
        );
        assert!(
            RdmaConnectionConfig::default()
                .max_send_sge(33)
                .validate(&engine, Some(&provider))
                .is_err()
        );
        assert!(
            RdmaConnectionConfig::default()
                .responder_resources(129)
                .validate(&engine, Some(&provider))
                .is_err()
        );
        assert!(
            RdmaConnectionConfig::default()
                .retry_count(8)
                .validate(&engine, Some(&provider))
                .is_err()
        );

        let mut minimum_engine = EngineConfig::new("rxe0".into());
        minimum_engine.max_inflight_operations = 2;
        minimum_engine.cq_capacity = 2;
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1)
            .max_send_sge(1)
            .max_recv_sge(1)
            .responder_resources(0)
            .initiator_depth(0)
            .retry_count(0)
            .rnr_retry_count(0)
            .validate(&minimum_engine, Some(&provider))
            .unwrap();

        let maximum = RdmaConnectionConfig::default()
            .max_send_wr(1_048_576)
            .max_recv_wr(1_048_576)
            .max_send_sge(32)
            .max_recv_sge(32)
            .responder_resources(255)
            .initiator_depth(255)
            .retry_count(7)
            .rnr_retry_count(7);
        let mut maximum_engine = EngineConfig::new("layout-only".into());
        maximum_engine.max_inflight_operations = 16_777_216;
        maximum_engine.cq_capacity = 16_777_216;
        maximum.validate(&maximum_engine, None).unwrap();

        for invalid in [
            RdmaConnectionConfig::default().max_recv_sge(0),
            RdmaConnectionConfig::default().initiator_depth(256),
            RdmaConnectionConfig::default().rnr_retry_count(8),
        ] {
            assert!(invalid.validate(&engine, Some(&provider)).is_err());
        }
    }

    #[test]
    fn provider_rejects_unreachable_public_maxima_without_clamping() {
        let mut config = EngineConfig::new("rxe0".into());
        config.max_live_connections = 1_048_576;
        assert!(config.validate_provider(&default_provider()).is_err());

        let mut config = EngineConfig::new("rxe0".into());
        config.cq_capacity = 32_768;
        config.max_inflight_operations = 32_768;
        assert!(config.validate_provider(&default_provider()).is_err());
    }

    #[test]
    fn rejects_empty_device_name() {
        assert!(
            EngineConfig::new("  ".into())
                .validate_without_provider()
                .is_err()
        );
    }
}
