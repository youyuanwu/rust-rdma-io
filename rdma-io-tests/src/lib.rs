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
        message.contains("RDMA CM")
            && (["Rejected", "Unreachable", "ConnectError"]
                .iter()
                .any(|event| message.contains(event))
                || [
                    "status 22",
                    "status -22",
                    "status 71",
                    "status -71",
                    "status 98",
                    "status -98",
                ]
                .iter()
                .any(|status| message.contains(status)))
    }

    /// Returns `true` for transient V2 engine CM errors raised before a test
    /// connection's protocol-specific work begins.
    pub fn is_transient_v2_engine_cm_error(err: &rdma_io::v2::Error) -> bool {
        matches!(err, rdma_io::v2::Error::Verbs(io) if is_transient_cm_io_error(io))
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
    use std::time::Duration;

    use rdma_io::async_cm::{AsyncCmId, AsyncCmListener};
    use rdma_io::cm::ConnParam;
    use rdma_io::v2::test_support::{TestEngineInstrumentation, TestEngineQp, TestEngineResources};
    use rdma_io::v2::{
        Error, MessageTransport, MessageTransportBuilder, RdmaEngine, RdmaListener,
        RdmaListenerConfig, Result,
    };

    use crate::test_helpers::{
        TRANSIENT_CM_HANDSHAKE_ATTEMPTS, connect_addr_for, connect_client_with_retry,
        is_transient_v2_engine_cm_error, transient_cm_retry_delay,
    };

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
            }
        }
    }

    async fn wait_for_engine_cleanup(
        server_engine: &RdmaEngine,
        server_resources: &TestEngineResources,
        server_baseline: &EngineCleanupBaseline,
        client_engine: &RdmaEngine,
        client_resources: &TestEngineResources,
        client_baseline: &EngineCleanupBaseline,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let server = EngineCleanupBaseline::capture(
                server_engine,
                server_resources.instrumentation().unwrap(),
            );
            let client = EngineCleanupBaseline::capture(
                client_engine,
                client_resources.instrumentation().unwrap(),
            );
            if &server == server_baseline && &client == client_baseline {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "V2 engine retry cleanup did not restore its exact baseline: \
                 server expected {server_baseline:?}, observed {server:?}; \
                 client expected {client_baseline:?}, observed {client:?}"
            );
            tokio::task::yield_now().await;
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

    /// Establish a ready message pair, retrying only known transient
    /// software-RDMA CM failures from listener creation, connect/accept, or
    /// the HELLO handshake.
    pub async fn establish_message_pair_with_retry<F>(
        server_engine: &RdmaEngine,
        client_engine: &RdmaEngine,
        make_builder: F,
    ) -> Result<(RdmaListener, MessageTransport, MessageTransport)>
    where
        F: FnMut() -> MessageTransportBuilder,
    {
        establish_message_pair_with_retry_after_ready(
            server_engine,
            client_engine,
            make_builder,
            |_| Ok(()),
        )
        .await
    }

    /// Variant with a post-ready check used to deterministically exercise the
    /// retry and cleanup path without injecting failures into production code.
    pub async fn establish_message_pair_with_retry_after_ready<F, C>(
        server_engine: &RdmaEngine,
        client_engine: &RdmaEngine,
        mut make_builder: F,
        mut after_ready: C,
    ) -> Result<(RdmaListener, MessageTransport, MessageTransport)>
    where
        F: FnMut() -> MessageTransportBuilder,
        C: FnMut(u64) -> Result<()>,
    {
        let server_resources = server_engine.test_resources().unwrap();
        let client_resources = client_engine.test_resources().unwrap();
        let mut last_transient = None;

        for attempt in 0..TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
            let server_baseline = EngineCleanupBaseline::capture(
                server_engine,
                server_resources.instrumentation().unwrap(),
            );
            let client_baseline = EngineCleanupBaseline::capture(
                client_engine,
                client_resources.instrumentation().unwrap(),
            );
            let listener = match server_engine
                .listen(
                    "0.0.0.0:0".parse().unwrap(),
                    RdmaListenerConfig::default().backlog(8),
                )
                .await
            {
                Ok(listener) => listener,
                Err(error) if is_transient_v2_engine_cm_error(&error) => {
                    if attempt + 1 == TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                        return Err(error);
                    }
                    wait_for_engine_cleanup(
                        server_engine,
                        &server_resources,
                        &server_baseline,
                        client_engine,
                        &client_resources,
                        &client_baseline,
                    )
                    .await;
                    tracing::warn!(
                        "V2 message listener attempt {attempt} failed transiently: {error}"
                    );
                    last_transient = Some(error);
                    // Cleanup is already complete. This bounded sleep is only
                    // for kernel/CM resource recovery, never state synchronization.
                    tokio::time::sleep(transient_cm_retry_delay(attempt)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let address = connect_addr_for(Some(listener.local_addr()?));
            let established = tokio::time::timeout(Duration::from_secs(15), async {
                tokio::try_join!(
                    make_builder().accept_on(&listener),
                    make_builder().connect_on(client_engine, address)
                )
            })
            .await;
            let (server, client, mut errors) = match established {
                Ok(Ok((server, client))) => (Some(server), Some(client), Vec::new()),
                Ok(Err(error)) => (None, None, vec![error]),
                Err(elapsed) => {
                    close_message_attempt(Some(listener), None, None).await;
                    wait_for_engine_cleanup(
                        server_engine,
                        &server_resources,
                        &server_baseline,
                        client_engine,
                        &client_resources,
                        &client_baseline,
                    )
                    .await;
                    panic!("V2 message establishment timed out: {elapsed}");
                }
            };

            if errors.is_empty() {
                let server_ref = server.as_ref().expect("successful server establishment");
                let client_ref = client.as_ref().expect("successful client establishment");
                let ready = tokio::time::timeout(Duration::from_secs(15), async {
                    tokio::try_join!(server_ref.ready(), client_ref.ready())
                })
                .await;
                match ready {
                    Ok(Ok(((), ()))) => {}
                    Ok(Err(error)) => errors.push(error),
                    Err(elapsed) => {
                        close_message_attempt(Some(listener), server, client).await;
                        wait_for_engine_cleanup(
                            server_engine,
                            &server_resources,
                            &server_baseline,
                            client_engine,
                            &client_resources,
                            &client_baseline,
                        )
                        .await;
                        panic!("V2 message HELLO timed out: {elapsed}");
                    }
                }
            }

            if errors.is_empty()
                && let Err(error) = after_ready(attempt)
            {
                errors.push(error);
            }
            if errors.is_empty() {
                return Ok((
                    listener,
                    server.expect("ready server transport"),
                    client.expect("ready client transport"),
                ));
            }

            close_message_attempt(Some(listener), server, client).await;
            wait_for_engine_cleanup(
                server_engine,
                &server_resources,
                &server_baseline,
                client_engine,
                &client_resources,
                &client_baseline,
            )
            .await;

            if let Some(error) = errors
                .iter()
                .find(|error| !is_transient_v2_engine_cm_error(error))
            {
                return Err(error.clone());
            }
            let error = errors
                .into_iter()
                .next()
                .expect("failed establishment must carry an error");
            if attempt + 1 == TRANSIENT_CM_HANDSHAKE_ATTEMPTS {
                return Err(error);
            }
            tracing::warn!(
                "V2 message establishment attempt {attempt} failed transiently: {error}"
            );
            last_transient = Some(error);
            // Cleanup is already complete. This bounded sleep is only for
            // kernel/CM resource recovery, never state synchronization.
            tokio::time::sleep(transient_cm_retry_delay(attempt)).await;
        }

        Err(last_transient.unwrap_or_else(|| {
            Error::InvalidConfig("V2 message establishment made no attempts".into())
        }))
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
    use super::test_helpers::{
        enforce_software_rdma_requirement, is_transient_cm_error, is_transient_v2_engine_cm_error,
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
                is_transient_v2_engine_cm_error(&err),
                "{event} should be retryable"
            );
        }
        let eproto = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM AddrError failed with status -71 for id=0x1 listen_id=0x0",
        ));
        assert!(is_transient_v2_engine_cm_error(&eproto));
    }

    #[test]
    fn v2_transient_cm_error_rejects_addr_and_protocol_failures() {
        let addr_error = rdma_io::v2::Error::Verbs(std::io::Error::other(
            "RDMA CM AddrError failed with status -110 for id=0x1 listen_id=0x0",
        ));
        assert!(!is_transient_v2_engine_cm_error(&addr_error));
        assert!(!is_transient_v2_engine_cm_error(
            &rdma_io::v2::Error::ProtocolViolation("bad magic".into())
        ));
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
