// This crate's tests live in the `tests/` directory as integration tests.
// The build.rs generates protobuf code used by tonic_tests.rs.

/// Shared test helpers for RDMA integration tests.
pub mod test_helpers {
    use std::net::SocketAddr;

    /// Returns `true` if **any** device is iWARP (e.g. siw present alongside rxe).
    pub fn any_iwarp() -> bool {
        rdma_io::device::any_device_is_iwarp()
    }

    /// Skip test if ANY device is iWARP. Used for tests that need
    /// InfiniBand/RoCE features unsupported on iWARP (atomics, RDMA Write
    /// with Immediate Data, etc.). Tests binding to 0.0.0.0 may have the
    /// CM pick an iWARP device even if rxe is also present.
    #[macro_export]
    macro_rules! require_no_iwarp {
        () => {
            if rdma_io_tests::test_helpers::any_iwarp() {
                tracing::warn!("SKIPPED: test requires no iWARP devices (siw detected)");
                return;
            }
        };
    }

    /// Returns `true` if a software RDMA device (siw or rxe) is present.
    ///
    /// The low-level verbs smoke tests (`sys_tests`, `safe_api_tests`) open the
    /// first/software device directly (no `rdma_cm` routing). That works on the
    /// siw/rxe devices used in CI, but on real multi-device NICs (e.g. Azure
    /// MANA exposes both an `rdmaP*` and a `roceP*` device) `open_first_device`
    /// can pick a device that rejects raw PD/QP/GID operations (ENODEV/EPERM).
    /// Those tests gate on this so they run in CI and skip on such hardware.
    pub fn has_software_rdma() -> bool {
        let present = rdma_io::device::devices()
            .map(|ds| {
                ds.iter()
                    .any(|d| d.name().starts_with("siw") || d.name().starts_with("rxe"))
            })
            .unwrap_or(false);
        enforce_software_rdma_requirement(
            present,
            std::env::var_os("RDMA_REQUIRE_PROVIDER").is_some_and(|value| value == "1"),
        )
    }

    pub(crate) fn enforce_software_rdma_requirement(present: bool, required: bool) -> bool {
        assert!(
            present || !required,
            "RDMA_REQUIRE_PROVIDER=1 but no rxe/siw software RDMA device is available"
        );
        present
    }

    /// Returns `true` if any local device advertises RDMA atomic support
    /// (`atomic_cap != IBV_ATOMIC_NONE`).
    ///
    /// Some RoCE NICs (e.g. the Azure MANA RoCEv2 preview) do not support RDMA
    /// atomics (CompareAndSwap / FetchAndAdd); tests exercising them gate on
    /// this so they run on atomic-capable devices (rxe) and skip elsewhere.
    pub fn supports_atomics() -> bool {
        let devices = match rdma_io::device::devices() {
            Ok(d) => d,
            Err(_) => return false,
        };
        devices.iter().any(|d| {
            d.open()
                .and_then(|ctx| ctx.query_device())
                .map(|attr| attr.atomic_cap != rdma_io_sys::ibverbs::IBV_ATOMIC_NONE)
                .unwrap_or(false)
        })
    }

    /// Skip test if no software RDMA device (siw/rxe) is present. See
    /// [`has_software_rdma`]. Used by the low-level verbs smoke tests that open
    /// the first/software device without `rdma_cm` routing.
    #[macro_export]
    macro_rules! require_software_rdma {
        () => {
            if !rdma_io_tests::test_helpers::has_software_rdma() {
                tracing::warn!("SKIPPED: test requires a software RDMA device (siw/rxe)");
                return;
            }
        };
    }

    /// Skip test if no local device supports RDMA atomics. See
    /// [`supports_atomics`].
    #[macro_export]
    macro_rules! require_atomics {
        () => {
            if !rdma_io_tests::test_helpers::supports_atomics() {
                tracing::warn!(
                    "SKIPPED: test requires RDMA atomic support (device atomic_cap == NONE)"
                );
                return;
            }
        };
    }

    /// Discover the first non-loopback IPv4 address (for siw0 over eth0).
    ///
    /// Uses the UDP connect trick: binding to 0.0.0.0 then "connecting" to an
    /// external address causes the kernel to pick the outgoing interface IP.
    pub fn local_ip() -> String {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
        sock.connect("8.8.8.8:80").unwrap();
        sock.local_addr().unwrap().ip().to_string()
    }

    /// Return a `0.0.0.0:0` bind address for RDMA listeners.
    ///
    /// Callers should bind an RDMA listener to this address, then call
    /// `connect_addr_for()` with the listener to get the connect address
    /// with the actual assigned port.
    pub fn bind_addr() -> SocketAddr {
        "0.0.0.0:0".parse().unwrap()
    }

    /// Build a connect address from a bound listener's actual port.
    ///
    /// Combines `local_ip()` with the port assigned by RDMA CM, avoiding
    /// the TCP→RDMA port reuse race that caused EADDRINUSE flakiness.
    pub fn connect_addr_for(listener_addr: Option<SocketAddr>) -> SocketAddr {
        let port = listener_addr.expect("listener has no local address").port();
        format!("{}:{port}", local_ip()).parse().unwrap()
    }

    /// Returns `true` if the error is an `EADDRINUSE` (errno 98) from verbs.
    fn is_addr_in_use(err: &rdma_io::Error) -> bool {
        matches!(err, rdma_io::Error::Verbs(io) if io.raw_os_error() == Some(98))
    }

    /// Number of attempts used for transient software-RDMA CM handshakes.
    ///
    /// siw can take noticeably longer than rxe to settle after a rejected
    /// async-CM handshake, so the async-CQ tests need a wider retry window than
    /// the generic EADDRINUSE port-release helpers below.
    pub const TRANSIENT_CM_HANDSHAKE_ATTEMPTS: u64 = 10;

    /// Backoff used between transient software-RDMA CM handshake retries.
    pub fn transient_cm_retry_delay(attempt: u64) -> std::time::Duration {
        std::time::Duration::from_millis(200 * (attempt + 1))
    }

    /// Returns `true` for transient CM errors that software RDMA can raise
    /// during the connect handshake.
    ///
    /// Covers:
    /// - `EADDRINUSE` (98): async CM port release
    /// - `EINVAL` (22): stale/half-resolved route on a fresh CM ID
    /// - `EPROTO` (71): protocol-level CM failure on the first connection
    ///   attempt (observed on ARM/RXE — the device's internal state settles
    ///   after the initial failure and subsequent attempts succeed)
    /// - async CM event races surfaced as `InvalidArg("expected Established,
    ///   got Rejected|Unreachable|ConnectError")`
    ///
    /// These failures are typically cleared by retrying the handshake with a
    /// fresh CM ID.
    pub fn is_transient_cm_error(err: &rdma_io::Error) -> bool {
        matches!(err, rdma_io::Error::Verbs(io) if is_transient_cm_io_error(io))
            || matches!(err, rdma_io::Error::InvalidArg(msg)
            if msg.starts_with("expected Established, got ")
                && matches!(
                    msg.strip_prefix("expected Established, got "),
                    Some("Rejected" | "Unreachable" | "ConnectError")
                ))
    }

    fn is_transient_cm_io_error(error: &std::io::Error) -> bool {
        error
            .raw_os_error()
            .is_some_and(|code| matches!(code.checked_abs(), Some(22 | 71 | 98)))
            || is_transient_cm_event_message(&error.to_string())
    }

    fn is_transient_cm_event_message(message: &str) -> bool {
        transient_cm_event(message).is_some_and(|(event, status)| {
            matches!(
                event,
                "AddrError" | "RouteError" | "Rejected" | "Unreachable" | "ConnectError"
            ) && is_transient_cm_status(status)
        })
    }

    fn is_transient_v2_listener_message(message: &str) -> bool {
        const PREFIX: &str = "listen on ";
        const BACKLOG: &str = " with requested kernel backlog 2147483647: ";

        let Some((address, source)) = message
            .strip_prefix(PREFIX)
            .and_then(|message| message.split_once(BACKLOG))
        else {
            return false;
        };
        let Ok(address) = address.parse::<std::net::SocketAddr>() else {
            return false;
        };
        let expected_source = std::io::Error::from_raw_os_error(98).to_string();
        message == format!("{PREFIX}{address}{BACKLOG}{expected_source}")
            && source == expected_source
    }

    fn transient_cm_event(message: &str) -> Option<(&str, i32)> {
        const ANCHOR: &str = "RDMA CM ";

        let mut anchors = message.match_indices(ANCHOR);
        let (anchor, _) = anchors.next()?;
        if anchors.next().is_some()
            || (anchor != 0 && !message.as_bytes()[anchor - 1].is_ascii_whitespace())
        {
            return None;
        }

        let prefix = &message[..anchor];
        let has_listen_id = match prefix {
            "" => true,
            "inbound " => true,
            _ => {
                let address = prefix
                    .strip_prefix("listener ")?
                    .strip_suffix(' ')?
                    .parse::<std::net::SocketAddr>()
                    .ok()?;
                if prefix != format!("listener {address} ") {
                    return None;
                }
                false
            }
        };

        let message = &message[anchor + ANCHOR.len()..];
        let (event, status_and_ids) = message.split_once(" failed with status ")?;
        let (status_token, ids) = status_and_ids.split_once(" for id=")?;
        let status = status_token.parse::<i32>().ok()?;
        if status.to_string() != status_token {
            return None;
        }

        if has_listen_id {
            let (id, listen_id) = ids.split_once(" listen_id=")?;
            if !is_canonical_lower_hex(id) || !is_canonical_lower_hex(listen_id) {
                return None;
            }
        } else if !is_canonical_lower_hex(ids) {
            return None;
        }

        Some((event, status))
    }

    fn is_canonical_lower_hex(value: &str) -> bool {
        let Some(digits) = value.strip_prefix("0x") else {
            return false;
        };
        !digits.is_empty()
            && (digits == "0" || !digits.starts_with('0'))
            && digits
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    }

    fn is_transient_cm_status(status: i32) -> bool {
        matches!(status.checked_abs(), Some(22 | 71 | 98))
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum V2EngineCmSetupStage {
        Listen,
        ConnectAccept,
        Ready,
    }

    /// Returns `true` only for known software-provider failures from an
    /// explicitly identified V2 listener/connect/accept setup stage.
    pub(crate) fn is_transient_v2_engine_cm_setup_error(
        stage: V2EngineCmSetupStage,
        err: &rdma_io::v2::Error,
    ) -> bool {
        let rdma_io::v2::Error::Verbs(error) = err else {
            return false;
        };
        match stage {
            V2EngineCmSetupStage::Listen => {
                error.raw_os_error().is_some_and(is_transient_cm_status)
                    || is_transient_cm_event_message(&error.to_string())
                    || is_transient_v2_listener_message(&error.to_string())
            }
            V2EngineCmSetupStage::ConnectAccept => {
                error.raw_os_error().is_some_and(is_transient_cm_status)
                    || is_transient_cm_event_message(&error.to_string())
            }
            V2EngineCmSetupStage::Ready => is_transient_cm_event_message(&error.to_string()),
        }
    }

    pub(crate) fn are_v2_engine_cm_setup_errors_retryable(
        stage: V2EngineCmSetupStage,
        errors: &[rdma_io::v2::Error],
        reject_event_counters_unchanged: bool,
    ) -> bool {
        if !reject_event_counters_unchanged || errors.is_empty() {
            return false;
        }
        match stage {
            V2EngineCmSetupStage::Listen | V2EngineCmSetupStage::ConnectAccept => errors
                .iter()
                .all(|error| is_transient_v2_engine_cm_setup_error(stage, error)),
            V2EngineCmSetupStage::Ready => {
                errors
                    .iter()
                    .any(|error| is_transient_v2_engine_cm_setup_error(stage, error))
                    && errors.iter().all(|error| {
                        is_transient_v2_engine_cm_setup_error(stage, error)
                            || matches!(error, rdma_io::v2::Error::TransportClosed)
                    })
            }
        }
    }

    /// Bind an [`AsyncCmListener`] to `0.0.0.0:0`, retrying on `EADDRINUSE`.
    ///
    /// siw releases RDMA CM ports asynchronously, so a fresh bind — even to
    /// port 0 — can transiently fail with `EADDRINUSE` right after a previous
    /// test tore down its connections. Retries up to 5 times with backoff.
    pub async fn bind_listener_with_retry() -> rdma_io::async_cm::AsyncCmListener {
        use rdma_io::async_cm::AsyncCmListener;
        let addr = bind_addr();
        let mut last_err = None;
        for attempt in 0u64..5 {
            match AsyncCmListener::bind(&addr) {
                Ok(l) => return l,
                Err(e) => {
                    if is_addr_in_use(&e) && attempt < 4 {
                        tracing::warn!("bind attempt {attempt} EADDRINUSE, retrying...");
                        tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1)))
                            .await;
                        last_err = Some(e);
                        continue;
                    }

                    panic!("listener bind failed: {e}");
                }
            }
        }
        panic!("listener bind failed after 5 attempts: {last_err:?}");
    }

    /// Connect a [`TransportBuilder`] client to `addr`, retrying on `EADDRINUSE`.
    ///
    /// The client side also allocates a local RDMA CM port, so it is subject
    /// to the same siw async port-release race as the listener bind. Retries
    /// up to 5 times with backoff.
    pub async fn connect_with_retry<B>(builder: &B, addr: &SocketAddr) -> B::Transport
    where
        B: rdma_io::transport::TransportBuilder,
    {
        let mut last_err = None;
        for attempt in 0u64..5 {
            match builder.connect(addr).await {
                Ok(t) => return t,
                Err(e) => {
                    if is_addr_in_use(&e) && attempt < 4 {
                        tracing::warn!("connect attempt {attempt} EADDRINUSE, retrying...");
                        tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1)))
                            .await;
                        last_err = Some(e);
                        continue;
                    }
                    panic!("connect failed: {e}");
                }
            }
        }
        panic!("connect failed after 5 attempts: {last_err:?}");
    }

    /// Create an [`AsyncCmId`] client, resolve address+route, retrying on
    /// `EADDRINUSE`.
    ///
    /// `resolve_addr(None, ..)` is where librdmacm binds the client's local
    /// ephemeral CM port, so it is subject to the same siw async port-release
    /// race as the listener bind. A fresh CM ID is created per attempt.
    pub async fn connect_client_cm_with_retry(
        connect_addr: &SocketAddr,
    ) -> rdma_io::async_cm::AsyncCmId {
        use rdma_io::async_cm::AsyncCmId;
        use rdma_io::cm::PortSpace;
        let mut last_err = None;
        for attempt in 0u64..5 {
            let cm = AsyncCmId::new(PortSpace::Tcp).unwrap();
            let resolved = async {
                cm.resolve_addr(None, connect_addr, 2000).await?;
                cm.resolve_route(2000).await?;
                Ok::<(), rdma_io::Error>(())
            }
            .await;
            match resolved {
                Ok(()) => return cm,
                Err(e) => {
                    if is_addr_in_use(&e) && attempt < 4 {
                        tracing::warn!("client resolve attempt {attempt} EADDRINUSE, retrying...");
                        tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1)))
                            .await;
                        last_err = Some(e);
                        continue;
                    }
                    panic!("client resolve failed: {e}");
                }
            }
        }
        panic!("client resolve failed after 5 attempts: {last_err:?}");
    }

    /// Establish a client [`AsyncCmId`] connection, retrying the entire
    /// handshake on transient errors.
    ///
    /// `rdma_connect` cannot be retried on a rejected CM ID, so each attempt
    /// builds a fresh CM ID (via [`connect_client_cm_with_retry`]), runs
    /// `setup_qp` to create the QP (and any per-attempt resources) on it, then
    /// connects. On a transient failure (`EADDRINUSE`, or `EINVAL` from a stale
    /// route) the CM ID and QP state are dropped and the handshake is retried.
    /// Returns the connected CM ID and the value produced by `setup_qp`.
    pub async fn connect_client_with_retry<F, T>(
        connect_addr: &SocketAddr,
        mut setup_qp: F,
    ) -> (rdma_io::async_cm::AsyncCmId, T)
    where
        F: FnMut(&rdma_io::async_cm::AsyncCmId) -> T,
    {
        use rdma_io::cm::ConnParam;
        let mut last_err = None;
        for attempt in 0..TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
            let cm = connect_client_cm_with_retry(connect_addr).await;
            let qp_state = setup_qp(&cm);
            match cm.connect(&ConnParam::default()).await {
                Ok(()) => return (cm, qp_state),
                Err(e) => {
                    if is_transient_cm_error(&e) && attempt + 1 < TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                        tracing::warn!("client connect attempt {attempt} {e}, retrying...");
                        drop(qp_state);
                        drop(cm);
                        tokio::time::sleep(transient_cm_retry_delay(attempt)).await;
                        last_err = Some(e);
                        continue;
                    }
                    panic!("client connect failed: {e}");
                }
            }
        }
        panic!(
            "client connect failed after {TRANSIENT_CM_HANDSHAKE_ATTEMPTS} attempts: {last_err:?}"
        );
    }

    /// Create a synchronous [`CmId`] client on `ch` and call `resolve_addr`,
    /// retrying on `EADDRINUSE`.
    ///
    /// `rdma_resolve_addr` binds the client's local ephemeral CM port and can
    /// return `EADDRINUSE` synchronously under siw. A fresh CM ID is created
    /// per attempt; the caller then drives the resulting events on `ch`.
    pub fn connect_client_cm_id_with_retry(
        ch: &rdma_io::cm::EventChannel,
        connect_addr: &SocketAddr,
    ) -> rdma_io::cm::CmId {
        use rdma_io::cm::{CmId, PortSpace};
        let mut last_err = None;
        for attempt in 0u64..5 {
            let id = CmId::new(ch, PortSpace::Tcp).unwrap();
            match id.resolve_addr(None, connect_addr, 2000) {
                Ok(()) => return id,
                Err(e) => {
                    if is_addr_in_use(&e) && attempt < 4 {
                        tracing::warn!(
                            "client resolve_addr attempt {attempt} EADDRINUSE, retrying..."
                        );
                        drop(id);
                        std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
                        last_err = Some(e);
                        continue;
                    }
                    panic!("client resolve_addr failed: {e}");
                }
            }
        }
        panic!("client resolve_addr failed after 5 attempts: {last_err:?}");
    }

    /// Bind an [`RdmaUdpSocket`](rdma_io_quinn::RdmaUdpSocket) to `0.0.0.0:0`,
    /// retrying on `EADDRINUSE`.
    ///
    /// Same siw async port-release race as [`bind_listener_with_retry`], but
    /// for the Quinn abstract-socket layer.
    pub async fn bind_socket_with_retry<B>(
        builder: B,
        label: &str,
    ) -> rdma_io_quinn::RdmaUdpSocket<B>
    where
        B: rdma_io::transport::TransportBuilder,
    {
        let addr = bind_addr();
        let mut last_err = None;
        for attempt in 0u64..5 {
            match rdma_io_quinn::RdmaUdpSocket::bind(&addr, builder.clone()) {
                Ok(s) => return s,
                Err(e) => {
                    if is_addr_in_use(&e) && attempt < 4 {
                        tracing::warn!("{label} bind attempt {attempt} EADDRINUSE, retrying...");
                        tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1)))
                            .await;
                        last_err = Some(e);
                        continue;
                    }
                    panic!("{label} bind failed: {e}");
                }
            }
        }
        panic!("{label} bind failed after 5 attempts: {last_err:?}");
    }

    /// Pre-connect an [`RdmaUdpSocket`](rdma_io_quinn::RdmaUdpSocket) to a peer,
    /// retrying on `EADDRINUSE`.
    ///
    /// `connect_to` allocates the client's local CM port, so it is subject to
    /// the same siw async port-release race as the listener bind.
    pub async fn connect_socket_with_retry<B>(
        socket: &rdma_io_quinn::RdmaUdpSocket<B>,
        addr: &SocketAddr,
        label: &str,
    ) where
        B: rdma_io::transport::TransportBuilder,
    {
        let mut last_err = None;
        for attempt in 0u64..5 {
            match socket.connect_to(addr).await {
                Ok(()) => return,
                Err(e) => {
                    if is_addr_in_use(&e) && attempt < 4 {
                        tracing::warn!("{label} connect attempt {attempt} EADDRINUSE, retrying...");
                        tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1)))
                            .await;
                        last_err = Some(e);
                        continue;
                    }
                    panic!("{label} pre-connect failed: {e}");
                }
            }
        }
        panic!("{label} connect failed after 5 attempts: {last_err:?}");
    }
}

/// Shared setup for tests that attach QPs to the v2 engine's test-only lease.
pub mod engine_test_helpers {
    use std::error::Error as StdError;
    use std::fmt;
    use std::time::Duration;

    use rdma_io::async_cm::{AsyncCmId, AsyncCmListener};
    use rdma_io::cm::ConnParam;
    use rdma_io::v2::test_support::{TestEngineInstrumentation, TestEngineQp, TestEngineResources};
    use rdma_io::v2::{
        Error, MessageTransport, MessageTransportBuilder, RdmaEngine, RdmaListener,
        RdmaListenerConfig, Result,
    };

    use crate::test_helpers::{
        TRANSIENT_CM_HANDSHAKE_ATTEMPTS, V2EngineCmSetupStage,
        are_v2_engine_cm_setup_errors_retryable, connect_addr_for, connect_client_with_retry,
        is_transient_v2_engine_cm_setup_error, transient_cm_retry_delay,
    };

    const V2_SETUP_STAGE_TIMEOUT: Duration = Duration::from_secs(15);
    const V2_SETUP_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
    const V2_SETUP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
    const V2_SETUP_CLEANUP_MIN_BACKOFF: Duration = Duration::from_millis(1);
    const V2_SETUP_CLEANUP_MAX_BACKOFF: Duration = Duration::from_millis(10);

    #[derive(Debug)]
    struct ExhaustedV2SetupRetries {
        attempts: u64,
        last_transient: Error,
    }

    impl fmt::Display for ExhaustedV2SetupRetries {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "V2 software-provider CM setup retries exhausted after {} attempts; \
                 last transient cause: {}",
                self.attempts, self.last_transient
            )
        }
    }

    impl StdError for ExhaustedV2SetupRetries {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&self.last_transient)
        }
    }

    pub struct EngineTestEndpoint {
        pub qp: Option<TestEngineQp>,
        pub cm: Option<AsyncCmId>,
    }

    pub struct EngineTestPair {
        pub server: EngineTestEndpoint,
        pub client: EngineTestEndpoint,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct EngineCleanupBaseline {
        live_connection_reservations: usize,
        establishing_connection_reservations: usize,
        established_connection_reservations: usize,
        draining_connection_reservations: usize,
        registered_live_qps: usize,
        free_connection_slots: usize,
        registered_operations: usize,
        free_operation_slots: usize,
        accepted_outstanding_operations: usize,
        free_cq_credits: usize,
        retained_cq_credits: usize,
        pending_reclamations: usize,
        quarantined_operations: usize,
        quarantined_mrs: usize,
        quarantined_bytes: usize,
        quarantined_bundles: usize,
        ready_queue_depth: usize,
        listener_count: usize,
        queued_inbound_requests: usize,
        pending_accepts: usize,
        selected_accepts: usize,
        connection_details: usize,
        listener_details: usize,
        cm_pending_routes: usize,
        cm_retained_owners: usize,
        inbound_requests_rejected: u64,
        inbound_rejected_backlog_full: u64,
        inbound_rejected_connection_capacity: u64,
        inbound_rejected_admission_closed: u64,
        inbound_rejected_listener_closed: u64,
        inbound_rejected_context_mismatch: u64,
        inbound_rejected_setup_failure: u64,
        cm_events_rejected: u64,
        stale_cm_events: u64,
        duplicate_cm_events: u64,
        unknown_cm_events: u64,
        wrong_id_cm_events: u64,
        unexpected_cm_events: u64,
    }

    impl EngineCleanupBaseline {
        fn capture(engine: &RdmaEngine, instrumentation: TestEngineInstrumentation) -> Self {
            let diagnostics = engine.diagnostics();
            Self {
                live_connection_reservations: diagnostics.live_connection_reservations,
                establishing_connection_reservations: diagnostics
                    .establishing_connection_reservations,
                established_connection_reservations: diagnostics
                    .established_connection_reservations,
                draining_connection_reservations: diagnostics.draining_connection_reservations,
                registered_live_qps: diagnostics.registered_live_qps,
                free_connection_slots: diagnostics.free_connection_slots,
                registered_operations: diagnostics.registered_operations,
                free_operation_slots: diagnostics.free_operation_slots,
                accepted_outstanding_operations: diagnostics.accepted_outstanding_operations,
                free_cq_credits: diagnostics.free_cq_credits,
                retained_cq_credits: diagnostics.retained_cq_credits,
                pending_reclamations: diagnostics.pending_reclamations,
                quarantined_operations: diagnostics.quarantined_operations,
                quarantined_mrs: diagnostics.quarantined_mrs,
                quarantined_bytes: diagnostics.quarantined_bytes,
                quarantined_bundles: diagnostics.quarantined_bundles,
                ready_queue_depth: diagnostics.ready_queue_depth,
                listener_count: diagnostics.listener_count,
                queued_inbound_requests: diagnostics.queued_inbound_requests,
                pending_accepts: diagnostics.pending_accepts,
                selected_accepts: diagnostics.selected_accepts,
                connection_details: diagnostics.connections().len(),
                listener_details: diagnostics.listeners().len(),
                cm_pending_routes: instrumentation.cm_pending_routes,
                cm_retained_owners: instrumentation.cm_retained_owners,
                inbound_requests_rejected: diagnostics.inbound_requests_rejected,
                inbound_rejected_backlog_full: diagnostics.inbound_rejected_backlog_full,
                inbound_rejected_connection_capacity: diagnostics
                    .inbound_rejected_connection_capacity,
                inbound_rejected_admission_closed: diagnostics.inbound_rejected_admission_closed,
                inbound_rejected_listener_closed: diagnostics.inbound_rejected_listener_closed,
                inbound_rejected_context_mismatch: diagnostics.inbound_rejected_context_mismatch,
                inbound_rejected_setup_failure: diagnostics.inbound_rejected_setup_failure,
                cm_events_rejected: diagnostics.cm_events_rejected,
                stale_cm_events: diagnostics.stale_cm_events,
                duplicate_cm_events: diagnostics.duplicate_cm_events,
                unknown_cm_events: diagnostics.unknown_cm_events,
                wrong_id_cm_events: diagnostics.wrong_id_cm_events,
                unexpected_cm_events: diagnostics.unexpected_cm_events,
            }
        }

        fn cleanup_gauges_match(&self, expected: &Self) -> bool {
            self.live_connection_reservations == expected.live_connection_reservations
                && self.establishing_connection_reservations
                    == expected.establishing_connection_reservations
                && self.established_connection_reservations
                    == expected.established_connection_reservations
                && self.draining_connection_reservations
                    == expected.draining_connection_reservations
                && self.registered_live_qps == expected.registered_live_qps
                && self.free_connection_slots == expected.free_connection_slots
                && self.registered_operations == expected.registered_operations
                && self.free_operation_slots == expected.free_operation_slots
                && self.accepted_outstanding_operations == expected.accepted_outstanding_operations
                && self.free_cq_credits == expected.free_cq_credits
                && self.retained_cq_credits == expected.retained_cq_credits
                && self.pending_reclamations == expected.pending_reclamations
                && self.quarantined_operations == expected.quarantined_operations
                && self.quarantined_mrs == expected.quarantined_mrs
                && self.quarantined_bytes == expected.quarantined_bytes
                && self.quarantined_bundles == expected.quarantined_bundles
                && self.ready_queue_depth == expected.ready_queue_depth
                && self.listener_count == expected.listener_count
                && self.queued_inbound_requests == expected.queued_inbound_requests
                && self.pending_accepts == expected.pending_accepts
                && self.selected_accepts == expected.selected_accepts
                && self.connection_details == expected.connection_details
                && self.listener_details == expected.listener_details
                && self.cm_pending_routes == expected.cm_pending_routes
                && self.cm_retained_owners == expected.cm_retained_owners
        }

        fn reject_event_counters_match(&self, expected: &Self) -> bool {
            self.inbound_requests_rejected == expected.inbound_requests_rejected
                && self.inbound_rejected_backlog_full == expected.inbound_rejected_backlog_full
                && self.inbound_rejected_connection_capacity
                    == expected.inbound_rejected_connection_capacity
                && self.inbound_rejected_admission_closed
                    == expected.inbound_rejected_admission_closed
                && self.inbound_rejected_listener_closed
                    == expected.inbound_rejected_listener_closed
                && self.inbound_rejected_context_mismatch
                    == expected.inbound_rejected_context_mismatch
                && self.inbound_rejected_setup_failure == expected.inbound_rejected_setup_failure
                && self.cm_events_rejected == expected.cm_events_rejected
                && self.stale_cm_events == expected.stale_cm_events
                && self.duplicate_cm_events == expected.duplicate_cm_events
                && self.unknown_cm_events == expected.unknown_cm_events
                && self.wrong_id_cm_events == expected.wrong_id_cm_events
                && self.unexpected_cm_events == expected.unexpected_cm_events
        }
    }

    async fn wait_for_engine_cleanup(
        server_engine: &RdmaEngine,
        server_resources: &TestEngineResources,
        server_baseline: &EngineCleanupBaseline,
        client_engine: &RdmaEngine,
        client_resources: &TestEngineResources,
        client_baseline: &EngineCleanupBaseline,
        failure_context: &str,
    ) -> (EngineCleanupBaseline, EngineCleanupBaseline) {
        let deadline = std::time::Instant::now() + V2_SETUP_CLEANUP_TIMEOUT;
        let mut backoff = V2_SETUP_CLEANUP_MIN_BACKOFF;
        loop {
            let server = EngineCleanupBaseline::capture(
                server_engine,
                server_resources.instrumentation().unwrap(),
            );
            let client = EngineCleanupBaseline::capture(
                client_engine,
                client_resources.instrumentation().unwrap(),
            );
            if server.cleanup_gauges_match(server_baseline)
                && client.cleanup_gauges_match(client_baseline)
            {
                return (server, client);
            }
            let now = std::time::Instant::now();
            assert!(
                now < deadline,
                "V2 engine retry cleanup did not restore its exact baseline: \
                 pending failure {failure_context}; \
                 server expected {server_baseline:?}, observed {server:?}; \
                 client expected {client_baseline:?}, observed {client:?}"
            );
            tokio::time::sleep(backoff.min(deadline.saturating_duration_since(now))).await;
            backoff = (backoff * 2).min(V2_SETUP_CLEANUP_MAX_BACKOFF);
        }
    }

    async fn close_message_attempt(
        listener: Option<RdmaListener>,
        server: Option<MessageTransport>,
        client: Option<MessageTransport>,
    ) {
        tokio::time::timeout(Duration::from_secs(15), async {
            match (&server, &client) {
                (Some(server), Some(client)) => {
                    let _ = tokio::join!(server.close(), client.close());
                }
                (Some(server), None) => {
                    let _ = server.close().await;
                }
                (None, Some(client)) => {
                    let _ = client.close().await;
                }
                (None, None) => {}
            }
            if let Some(listener) = &listener {
                let _ = listener.close().await;
            }
        })
        .await
        .expect("V2 engine retry attempt cleanup timed out");
        drop(server);
        drop(client);
        drop(listener);
    }

    pub(crate) fn retry_exhausted(last_transient: Error) -> Error {
        Error::Verbs(std::io::Error::other(ExhaustedV2SetupRetries {
            attempts: TRANSIENT_CM_HANDSHAKE_ATTEMPTS,
            last_transient,
        }))
    }

    fn setup_timeout(stage: &str, elapsed: tokio::time::error::Elapsed) -> Error {
        Error::Verbs(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("V2 message {stage} timed out: {elapsed}"),
        ))
    }

    fn attempt_has_no_engine_rejects(
        server: &EngineCleanupBaseline,
        server_baseline: &EngineCleanupBaseline,
        client: &EngineCleanupBaseline,
        client_baseline: &EngineCleanupBaseline,
    ) -> bool {
        server.reject_event_counters_match(server_baseline)
            && client.reject_event_counters_match(client_baseline)
    }

    /// Establish a ready message pair, retrying only known transient
    /// software-RDMA failures from listener, connect/accept, or readiness CM
    /// setup.
    ///
    /// Readiness retries require the exact production CM-event grammar; raw
    /// errno, HELLO, protocol, and data-operation errors are never retried.
    /// All attempts share a 60-second wall-clock budget; cancellation of that
    /// outer budget is followed by at most 15 seconds of exact cleanup
    /// verification.
    pub async fn establish_message_pair_with_retry<F>(
        server_engine: &RdmaEngine,
        client_engine: &RdmaEngine,
        make_builder: F,
    ) -> Result<(RdmaListener, MessageTransport, MessageTransport)>
    where
        F: FnMut() -> MessageTransportBuilder,
    {
        establish_message_pair_with_retry_before_connect_accept(
            server_engine,
            client_engine,
            make_builder,
            |_| Ok(()),
        )
        .await
    }

    /// Variant with a pre-connect/accept check used to exercise setup retry
    /// and listener cleanup without injecting failures into production code.
    pub async fn establish_message_pair_with_retry_before_connect_accept<F, C>(
        server_engine: &RdmaEngine,
        client_engine: &RdmaEngine,
        make_builder: F,
        before_connect_accept: C,
    ) -> Result<(RdmaListener, MessageTransport, MessageTransport)>
    where
        F: FnMut() -> MessageTransportBuilder,
        C: FnMut(u64) -> Result<()>,
    {
        establish_message_pair_with_retry_before_stages(
            server_engine,
            client_engine,
            make_builder,
            before_connect_accept,
            |_| Ok(()),
        )
        .await
    }

    /// Variant with test-only stage hooks for proving exact retry policy and
    /// cleanup without changing production engine behavior.
    pub async fn establish_message_pair_with_retry_before_stages<F, C, R>(
        server_engine: &RdmaEngine,
        client_engine: &RdmaEngine,
        mut make_builder: F,
        mut before_connect_accept: C,
        mut before_ready: R,
    ) -> Result<(RdmaListener, MessageTransport, MessageTransport)>
    where
        F: FnMut() -> MessageTransportBuilder,
        C: FnMut(u64) -> Result<()>,
        R: FnMut(u64) -> Result<()>,
    {
        let server_resources = server_engine.test_resources().unwrap();
        let client_resources = client_engine.test_resources().unwrap();
        let total_server_baseline = EngineCleanupBaseline::capture(
            server_engine,
            server_resources.instrumentation().unwrap(),
        );
        let total_client_baseline = EngineCleanupBaseline::capture(
            client_engine,
            client_resources.instrumentation().unwrap(),
        );
        let mut attempt_history = Vec::new();

        let establish = async {
            for attempt in 0..TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                let server_baseline = EngineCleanupBaseline::capture(
                    server_engine,
                    server_resources.instrumentation().unwrap(),
                );
                let client_baseline = EngineCleanupBaseline::capture(
                    client_engine,
                    client_resources.instrumentation().unwrap(),
                );
                let listener = match tokio::time::timeout(
                    V2_SETUP_STAGE_TIMEOUT,
                    server_engine.listen(
                        "0.0.0.0:0".parse().unwrap(),
                        RdmaListenerConfig::default().backlog(8),
                    ),
                )
                .await
                {
                    Ok(Ok(listener)) => listener,
                    Ok(Err(error)) => {
                        let context = format!("listener setup failed: {error}");
                        attempt_history.push(format!("attempt {attempt}: {context}"));
                        let (server, client) = wait_for_engine_cleanup(
                            server_engine,
                            &server_resources,
                            &server_baseline,
                            client_engine,
                            &client_resources,
                            &client_baseline,
                            &context,
                        )
                        .await;
                        let no_engine_rejects = attempt_has_no_engine_rejects(
                            &server,
                            &server_baseline,
                            &client,
                            &client_baseline,
                        );
                        let retry = are_v2_engine_cm_setup_errors_retryable(
                            V2EngineCmSetupStage::Listen,
                            std::slice::from_ref(&error),
                            no_engine_rejects,
                        );
                        if !retry {
                            return Err(error);
                        }
                        if attempt + 1 == TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                            return Err(retry_exhausted(error));
                        }
                        tracing::warn!(
                            "V2 message listener attempt {attempt} failed transiently: {error}"
                        );
                        tokio::time::sleep(transient_cm_retry_delay(attempt)).await;
                        continue;
                    }
                    Err(elapsed) => {
                        let error = setup_timeout("listener setup", elapsed);
                        let context = error.to_string();
                        attempt_history.push(format!("attempt {attempt}: {context}"));
                        wait_for_engine_cleanup(
                            server_engine,
                            &server_resources,
                            &server_baseline,
                            client_engine,
                            &client_resources,
                            &client_baseline,
                            &context,
                        )
                        .await;
                        return Err(error);
                    }
                };
                if let Err(error) = before_connect_accept(attempt) {
                    let context = format!("injected connect/accept setup failure: {error}");
                    attempt_history.push(format!("attempt {attempt}: {context}"));
                    close_message_attempt(Some(listener), None, None).await;
                    let (server, client) = wait_for_engine_cleanup(
                        server_engine,
                        &server_resources,
                        &server_baseline,
                        client_engine,
                        &client_resources,
                        &client_baseline,
                        &context,
                    )
                    .await;
                    let no_engine_rejects = attempt_has_no_engine_rejects(
                        &server,
                        &server_baseline,
                        &client,
                        &client_baseline,
                    );
                    if !are_v2_engine_cm_setup_errors_retryable(
                        V2EngineCmSetupStage::ConnectAccept,
                        std::slice::from_ref(&error),
                        no_engine_rejects,
                    ) {
                        return Err(error);
                    }
                    if attempt + 1 == TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                        return Err(retry_exhausted(error));
                    }
                    tracing::warn!(
                        "V2 message connect/accept attempt {attempt} failed transiently: {error}"
                    );
                    tokio::time::sleep(transient_cm_retry_delay(attempt)).await;
                    continue;
                }
                let address = match listener.local_addr() {
                    Ok(address) => connect_addr_for(Some(address)),
                    Err(error) => {
                        let context = format!("listener local address failed: {error}");
                        attempt_history.push(format!("attempt {attempt}: {context}"));
                        close_message_attempt(Some(listener), None, None).await;
                        wait_for_engine_cleanup(
                            server_engine,
                            &server_resources,
                            &server_baseline,
                            client_engine,
                            &client_resources,
                            &client_baseline,
                            &context,
                        )
                        .await;
                        return Err(error);
                    }
                };
                let established = tokio::time::timeout(V2_SETUP_STAGE_TIMEOUT, async {
                    tokio::join!(
                        make_builder().accept_on(&listener),
                        make_builder().connect_on(client_engine, address)
                    )
                })
                .await;
                let (server, client, setup_errors) = match established {
                    Ok((server, client)) => {
                        let mut errors = Vec::new();
                        let server = match server {
                            Ok(server) => Some(server),
                            Err(error) => {
                                errors.push(error);
                                None
                            }
                        };
                        let client = match client {
                            Ok(client) => Some(client),
                            Err(error) => {
                                errors.push(error);
                                None
                            }
                        };
                        (server, client, errors)
                    }
                    Err(elapsed) => {
                        close_message_attempt(Some(listener), None, None).await;
                        let error = setup_timeout("connect/accept setup", elapsed);
                        let context = error.to_string();
                        attempt_history.push(format!("attempt {attempt}: {context}"));
                        wait_for_engine_cleanup(
                            server_engine,
                            &server_resources,
                            &server_baseline,
                            client_engine,
                            &client_resources,
                            &client_baseline,
                            &context,
                        )
                        .await;
                        return Err(error);
                    }
                };

                if !setup_errors.is_empty() {
                    let context = format!("connect/accept setup failed: {setup_errors:?}");
                    attempt_history.push(format!("attempt {attempt}: {context}"));
                    close_message_attempt(Some(listener), server, client).await;
                    let (server, client) = wait_for_engine_cleanup(
                        server_engine,
                        &server_resources,
                        &server_baseline,
                        client_engine,
                        &client_resources,
                        &client_baseline,
                        &context,
                    )
                    .await;
                    let no_engine_rejects = attempt_has_no_engine_rejects(
                        &server,
                        &server_baseline,
                        &client,
                        &client_baseline,
                    );
                    if !no_engine_rejects {
                        tracing::warn!(
                            "V2 setup retry disabled by engine reject/event counter delta: \
                             server before {server_baseline:?}, after {server:?}; \
                             client before {client_baseline:?}, after {client:?}"
                        );
                    }
                    if !are_v2_engine_cm_setup_errors_retryable(
                        V2EngineCmSetupStage::ConnectAccept,
                        &setup_errors,
                        no_engine_rejects,
                    ) {
                        return Err(setup_errors[0].clone());
                    }
                    let error = setup_errors
                        .into_iter()
                        .next()
                        .expect("failed setup must carry an error");
                    if attempt + 1 == TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                        return Err(retry_exhausted(error));
                    }
                    tracing::warn!(
                        "V2 message connect/accept attempt {attempt} failed transiently: {error}"
                    );
                    tokio::time::sleep(transient_cm_retry_delay(attempt)).await;
                    continue;
                }

                let server = server.expect("successful server establishment");
                let client = client.expect("successful client establishment");
                let injected_ready_error = before_ready(attempt).err();
                let mut ready = tokio::time::timeout(V2_SETUP_STAGE_TIMEOUT, async {
                    tokio::join!(server.ready(), client.ready())
                })
                .await;
                if matches!(&ready, Ok((Ok(()), Ok(()))))
                    && let Some(error) = injected_ready_error
                {
                    ready = Ok((Err(error), Ok(())));
                }
                match ready {
                    Ok((Ok(()), Ok(()))) => return Ok((listener, server, client)),
                    Ok((server_ready, client_ready)) => {
                        let ready_errors = [server_ready.err(), client_ready.err()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>();
                        let context = format!("message readiness failed: {ready_errors:?}");
                        attempt_history.push(format!("attempt {attempt}: {context}"));
                        let server_at_failure = EngineCleanupBaseline::capture(
                            server_engine,
                            server_resources.instrumentation().unwrap(),
                        );
                        let client_at_failure = EngineCleanupBaseline::capture(
                            client_engine,
                            client_resources.instrumentation().unwrap(),
                        );
                        let no_engine_rejects = attempt_has_no_engine_rejects(
                            &server_at_failure,
                            &server_baseline,
                            &client_at_failure,
                            &client_baseline,
                        );
                        close_message_attempt(Some(listener), Some(server), Some(client)).await;
                        let (server, client) = wait_for_engine_cleanup(
                            server_engine,
                            &server_resources,
                            &server_baseline,
                            client_engine,
                            &client_resources,
                            &client_baseline,
                            &context,
                        )
                        .await;
                        if !no_engine_rejects {
                            tracing::warn!(
                                "V2 ready retry disabled by engine reject/event counter delta: \
                                 server before {server_baseline:?}, at failure {server_at_failure:?}; \
                                 client before {client_baseline:?}, at failure {client_at_failure:?}; \
                                 post-cleanup server {server:?}, client {client:?}"
                            );
                        }
                        if !are_v2_engine_cm_setup_errors_retryable(
                            V2EngineCmSetupStage::Ready,
                            &ready_errors,
                            no_engine_rejects,
                        ) {
                            let error = ready_errors
                                .iter()
                                .find(|error| {
                                    !is_transient_v2_engine_cm_setup_error(
                                        V2EngineCmSetupStage::Ready,
                                        error,
                                    ) && !matches!(error, Error::TransportClosed)
                                })
                                .unwrap_or_else(|| {
                                    ready_errors
                                        .first()
                                        .expect("failed readiness must carry an error")
                                })
                                .clone();
                            return Err(error);
                        }
                        let error = ready_errors
                            .into_iter()
                            .find(|error| {
                                is_transient_v2_engine_cm_setup_error(
                                    V2EngineCmSetupStage::Ready,
                                    error,
                                )
                            })
                            .expect("retryable readiness must carry an exact CM event error");
                        if attempt + 1 == TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                            return Err(retry_exhausted(error));
                        }
                        tracing::warn!(
                            "V2 message readiness attempt {attempt} failed transiently: {error}"
                        );
                        tokio::time::sleep(transient_cm_retry_delay(attempt)).await;
                        continue;
                    }
                    Err(elapsed) => {
                        let error = setup_timeout("HELLO", elapsed);
                        let context = error.to_string();
                        attempt_history.push(format!("attempt {attempt}: {context}"));
                        close_message_attempt(Some(listener), Some(server), Some(client)).await;
                        wait_for_engine_cleanup(
                            server_engine,
                            &server_resources,
                            &server_baseline,
                            client_engine,
                            &client_resources,
                            &client_baseline,
                            &context,
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
            unreachable!("the fixed nonzero retry count returns from every terminal attempt")
        };

        match tokio::time::timeout(V2_SETUP_TOTAL_TIMEOUT, establish).await {
            Ok(result) => result,
            Err(elapsed) => {
                let error = Error::Verbs(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "V2 message overall setup retry budget timed out: {elapsed}; \
                         attempt history: {attempt_history:?}"
                    ),
                ));
                let context = error.to_string();
                wait_for_engine_cleanup(
                    server_engine,
                    &server_resources,
                    &total_server_baseline,
                    client_engine,
                    &client_resources,
                    &total_client_baseline,
                    &context,
                )
                .await;
                Err(error)
            }
        }
    }

    pub async fn setup_engine_pair(resources: &TestEngineResources) -> EngineTestPair {
        let listener = crate::test_helpers::bind_listener_with_retry().await;
        let connect_addr = connect_addr_for(listener.local_addr());

        let server_resources = resources.clone();
        let server = tokio::spawn(async move {
            let conn_id = listener.get_request().await.unwrap();
            server_resources.require_context(&conn_id).unwrap();
            let qp = server_resources.create_qp(&conn_id, 64, 64).unwrap();
            conn_id.accept(&ConnParam::default()).unwrap();
            listener.await_established().await.unwrap();
            let cm = AsyncCmListener::migrate_accepted(conn_id).unwrap();
            EngineTestEndpoint {
                qp: Some(qp),
                cm: Some(cm),
            }
        });

        let client_resources = resources.clone();
        let client = tokio::spawn(async move {
            let (cm, qp) = connect_client_with_retry(&connect_addr, |cm| {
                client_resources.require_context(cm.cm_id()).unwrap();
                client_resources.create_qp(cm.cm_id(), 64, 64).unwrap()
            })
            .await;
            EngineTestEndpoint {
                qp: Some(qp),
                cm: Some(cm),
            }
        });

        let (server, client) = tokio::join!(server, client);
        EngineTestPair {
            server: server.unwrap(),
            client: client.unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;

    use super::engine_test_helpers::retry_exhausted;
    use super::test_helpers::{
        V2EngineCmSetupStage, are_v2_engine_cm_setup_errors_retryable,
        enforce_software_rdma_requirement, is_transient_cm_error,
        is_transient_v2_engine_cm_setup_error,
    };

    #[test]
    #[should_panic(expected = "RDMA_REQUIRE_PROVIDER=1")]
    fn required_provider_absence_is_a_hard_failure() {
        enforce_software_rdma_requirement(false, true);
    }

    #[test]
    fn optional_provider_absence_still_allows_skip_guards() {
        assert!(!enforce_software_rdma_requirement(false, false));
    }

    #[test]
    fn transient_cm_error_accepts_retryable_async_cm_events() {
        for event in ["Rejected", "Unreachable", "ConnectError"] {
            let err = rdma_io::Error::InvalidArg(format!("expected Established, got {event}"));
            assert!(is_transient_cm_error(&err), "{event} should be retryable");
        }
    }

    #[test]
    fn transient_cm_error_accepts_eproto() {
        // EPROTO (71) is returned by rdma_get_cm_event on ARM/RXE on the first
        // connection attempt; subsequent attempts succeed once the device settles.
        let err = rdma_io::Error::Verbs(std::io::Error::from_raw_os_error(71));
        assert!(is_transient_cm_error(&err));
    }

    #[test]
    fn transient_cm_error_accepts_all_retryable_errnos() {
        for errno in [22, 71, 98] {
            let err = rdma_io::Error::Verbs(std::io::Error::from_raw_os_error(errno));
            assert!(is_transient_cm_error(&err), "errno {errno} should retry");
        }
    }

    #[test]
    fn v2_transient_cm_error_accepts_known_software_provider_events() {
        for event in ["Rejected", "Unreachable", "ConnectError"] {
            let err = rdma_io::v2::Error::Verbs(std::io::Error::other(format!(
                "RDMA CM {event} failed with status -22 for id=0x1 listen_id=0x0"
            )));
            assert!(
                is_transient_v2_engine_cm_setup_error(V2EngineCmSetupStage::ConnectAccept, &err),
                "{event} should be retryable"
            );
        }
        let eproto = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM AddrError failed with status -71 for id=0x1 listen_id=0x0",
        ));
        assert!(is_transient_v2_engine_cm_setup_error(
            V2EngineCmSetupStage::Listen,
            &eproto
        ));
    }

    #[test]
    fn v2_transient_cm_error_accepts_exact_production_message_formats() {
        for (stage, message) in [
            (
                V2EngineCmSetupStage::ConnectAccept,
                "RDMA CM ConnectError failed with status -22 for id=0x1 listen_id=0x0",
            ),
            (
                V2EngineCmSetupStage::Listen,
                "listener 0.0.0.0:0 RDMA CM AddrError failed with status -98 for id=0x2",
            ),
            (
                V2EngineCmSetupStage::ConnectAccept,
                "inbound RDMA CM Rejected failed with status -71 for id=0x3 listen_id=0x2",
            ),
            (
                V2EngineCmSetupStage::Ready,
                "RDMA CM ConnectError failed with status -22 for id=0x4 listen_id=0x0",
            ),
        ] {
            let error = rdma_io::v2::Error::Verbs(std::io::Error::other(message.to_owned()));
            assert!(
                is_transient_v2_engine_cm_setup_error(stage, &error),
                "{message:?} should be retryable"
            );
        }
        let listener_busy = format!(
            "listen on 0.0.0.0:0 with requested kernel backlog 2147483647: {}",
            std::io::Error::from_raw_os_error(98)
        );
        let error = rdma_io::v2::Error::Verbs(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            listener_busy,
        ));
        assert!(is_transient_v2_engine_cm_setup_error(
            V2EngineCmSetupStage::Listen,
            &error,
        ));
    }

    #[test]
    fn v2_transient_cm_error_rejects_non_production_message_grammar() {
        for message in [
            "XRDMA CM ConnectError failed with status -22 for id=0x1 listen_id=0x0",
            "RDMA  CM ConnectError failed with status -22 for id=0x1 listen_id=0x0",
            "inbound RDMA CMConnectError failed with status -22 for id=0x1 listen_id=0x0",
            "protocol violation: peer said RDMA CM ConnectError failed with status -22 for id=0x1 listen_id=0x0",
            "listener not-an-address RDMA CM AddrError failed with status -98 for id=0x1",
            "RDMA CM Unknown failed with status -22 for id=0x1 listen_id=0x0",
            "RDMA CM ConnectError failed with status -110 for id=0x1 listen_id=0x0",
            "RDMA CM ConnectError failed with status -220 for id=0x1 listen_id=0x0",
            "RDMA CM ConnectError failed with status -22suffix for id=0x1 listen_id=0x0",
            "RDMA CM ConnectError failed with status +22 for id=0x1 listen_id=0x0",
            "RDMA CM ConnectError failed with status -22 extra for id=0x1 listen_id=0x0",
            "RDMA CM ConnectError failed with status -22 for id=0x1 listen_id=0x0 trailing",
            "RDMA CM ConnectError failed with status -22 for id=0x1",
            "listener 0.0.0.0:0 RDMA CM AddrError failed with status -98 for id=0x1 listen_id=0x0",
            "RDMA CM RDMA CM ConnectError failed with status -22 for id=0x1 listen_id=0x0",
            "inbound RDMA CM ConnectError failed with status -22 for id=0x1 listen_id=0x0 RDMA CM Rejected failed with status -22 for id=0x2 listen_id=0x0",
        ] {
            let error = rdma_io::v2::Error::Verbs(std::io::Error::other(message.to_owned()));
            assert!(
                !is_transient_v2_engine_cm_setup_error(V2EngineCmSetupStage::ConnectAccept, &error),
                "{message:?} should not be retryable"
            );
        }
        for message in [
            "listen on not-an-address with requested kernel backlog 2147483647: Address already in use (os error 98)",
            "listen on 0.0.0.0:0 with requested kernel backlog 8: Address already in use (os error 98)",
            "listen on 0.0.0.0:0 with requested kernel backlog 2147483647: Invalid argument (os error 22)",
            "prefix listen on 0.0.0.0:0 with requested kernel backlog 2147483647: Address already in use (os error 98)",
        ] {
            let error = rdma_io::v2::Error::Verbs(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                message,
            ));
            assert!(!is_transient_v2_engine_cm_setup_error(
                V2EngineCmSetupStage::Listen,
                &error,
            ));
        }
    }

    #[test]
    fn v2_transient_cm_error_requires_exact_setup_status_and_error_type() {
        let addr_error = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM AddrError failed with status -110 for id=0x1 listen_id=0x0",
        ));
        let permanent_reject = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM Rejected failed with status -13 for id=0x1 listen_id=0x0",
        ));
        let substring_trap = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM ConnectError failed with status -220 for id=0x1 listen_id=0x0",
        ));
        let event_without_status = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM Rejected failed for id=0x1 listen_id=0x0",
        ));
        for error in [
            &addr_error,
            &permanent_reject,
            &substring_trap,
            &event_without_status,
        ] {
            assert!(!is_transient_v2_engine_cm_setup_error(
                V2EngineCmSetupStage::ConnectAccept,
                error
            ));
        }
        assert!(!is_transient_v2_engine_cm_setup_error(
            V2EngineCmSetupStage::ConnectAccept,
            &rdma_io::v2::Error::ProtocolViolation("bad magic".into())
        ));
        assert!(is_transient_v2_engine_cm_setup_error(
            V2EngineCmSetupStage::Listen,
            &rdma_io::v2::Error::Verbs(std::io::Error::from_raw_os_error(98))
        ));
    }

    #[test]
    fn v2_ready_retry_rejects_raw_errno_and_protocol_errors() {
        for error in [
            rdma_io::v2::Error::Verbs(std::io::Error::from_raw_os_error(22)),
            rdma_io::v2::Error::Verbs(std::io::Error::from_raw_os_error(71)),
            rdma_io::v2::Error::Verbs(std::io::Error::from_raw_os_error(98)),
            rdma_io::v2::Error::ProtocolViolation("bad HELLO magic".into()),
        ] {
            assert!(!is_transient_v2_engine_cm_setup_error(
                V2EngineCmSetupStage::Ready,
                &error
            ));
            assert!(!are_v2_engine_cm_setup_errors_retryable(
                V2EngineCmSetupStage::Ready,
                std::slice::from_ref(&error),
                true,
            ));
        }
    }

    #[test]
    fn v2_ready_retry_allows_only_exact_cm_error_with_transport_close_companion() {
        let transient = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM ConnectError failed with status 71 for id=0x1 listen_id=0x0",
        ));
        assert!(are_v2_engine_cm_setup_errors_retryable(
            V2EngineCmSetupStage::Ready,
            &[transient.clone(), rdma_io::v2::Error::TransportClosed],
            true,
        ));
        assert!(!are_v2_engine_cm_setup_errors_retryable(
            V2EngineCmSetupStage::Ready,
            &[
                transient,
                rdma_io::v2::Error::ProtocolViolation("HELLO mismatch".into()),
            ],
            true,
        ));
    }

    #[test]
    fn v2_setup_retry_requires_zero_engine_reject_event_deltas() {
        let transient = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM Rejected failed with status -22 for id=0x1 listen_id=0x0",
        ));
        assert!(are_v2_engine_cm_setup_errors_retryable(
            V2EngineCmSetupStage::ConnectAccept,
            std::slice::from_ref(&transient),
            true,
        ));
        assert!(!are_v2_engine_cm_setup_errors_retryable(
            V2EngineCmSetupStage::ConnectAccept,
            std::slice::from_ref(&transient),
            false,
        ));
        assert!(!are_v2_engine_cm_setup_errors_retryable(
            V2EngineCmSetupStage::ConnectAccept,
            &[rdma_io::v2::Error::ProtocolViolation(
                "duplicate HELLO".into()
            )],
            true,
        ));
    }

    #[test]
    fn exhausted_v2_setup_retry_preserves_the_last_transient_source() {
        let exhausted = retry_exhausted(rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM ConnectError failed with status -71 for id=0x1 listen_id=0x0",
        )));
        assert!(exhausted.to_string().contains("retries exhausted"));
        let io_source = StdError::source(&exhausted).expect("V2 verbs wrapper source");
        let exhausted_source = io_source.source().expect("exhausted retry source");
        let last_transient = exhausted_source
            .source()
            .expect("last transient setup cause");
        assert!(last_transient.to_string().contains("status -71"));
    }

    #[test]
    fn transient_cm_error_rejects_non_retryable_async_cm_events() {
        let err = rdma_io::Error::InvalidArg("expected Established, got Established".into());
        assert!(!is_transient_cm_error(&err));
    }
}

/// Greeter gRPC service implementation for tonic integration tests.
///
/// Re-exports the generated proto types and provides `MyGreeter`,
/// a simple test service with unary, server-streaming, client-streaming,
/// and bidirectional-streaming RPCs.
pub mod greeter_service {
    pub mod greeter {
        tonic::include_proto!("greeter");
    }

    pub use greeter::greeter_client::GreeterClient;
    pub use greeter::greeter_server::{Greeter, GreeterServer};
    pub use greeter::{HelloReply, HelloRequest};

    use std::pin::Pin;
    use tokio_stream::{Stream, StreamExt};
    use tonic::{Request, Response, Status, Streaming};

    #[derive(Debug, Default)]
    pub struct MyGreeter;

    #[tonic::async_trait]
    impl Greeter for MyGreeter {
        async fn say_hello(
            &self,
            request: Request<HelloRequest>,
        ) -> Result<Response<HelloReply>, Status> {
            let name = request.into_inner().name;
            Ok(Response::new(HelloReply {
                message: format!("Hello {name}!"),
            }))
        }

        type ServerStreamStream =
            Pin<Box<dyn Stream<Item = Result<HelloReply, Status>> + Send + 'static>>;

        async fn server_stream(
            &self,
            request: Request<HelloRequest>,
        ) -> Result<Response<Self::ServerStreamStream>, Status> {
            let name = request.into_inner().name;
            let stream = tokio_stream::iter(0..5).map(move |i| {
                Ok(HelloReply {
                    message: format!("{name}-{i}"),
                })
            });
            Ok(Response::new(Box::pin(stream)))
        }

        async fn client_stream(
            &self,
            request: Request<Streaming<HelloRequest>>,
        ) -> Result<Response<HelloReply>, Status> {
            let mut stream = request.into_inner();
            let mut names = Vec::new();
            while let Some(req) = stream.next().await {
                names.push(req?.name);
            }
            Ok(Response::new(HelloReply {
                message: format!("Hello {}!", names.join(", ")),
            }))
        }

        type BidiStreamStream =
            Pin<Box<dyn Stream<Item = Result<HelloReply, Status>> + Send + 'static>>;

        async fn bidi_stream(
            &self,
            request: Request<Streaming<HelloRequest>>,
        ) -> Result<Response<Self::BidiStreamStream>, Status> {
            let mut stream = request.into_inner();
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                while let Some(Ok(req)) = stream.next().await {
                    let reply = HelloReply {
                        message: format!("echo: {}", req.name),
                    };
                    if tx.send(Ok(reply)).await.is_err() {
                        break;
                    }
                }
            });
            Ok(Response::new(Box::pin(
                tokio_stream::wrappers::ReceiverStream::new(rx),
            )))
        }
    }
}
