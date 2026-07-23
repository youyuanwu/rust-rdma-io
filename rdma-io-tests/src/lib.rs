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
        rdma_io::device::devices()
            .map(|ds| {
                ds.iter()
                    .any(|d| d.name().starts_with("siw") || d.name().starts_with("rxe"))
            })
            .unwrap_or(false)
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
        matches!(err, rdma_io::Error::Verbs(io)
            if matches!(io.raw_os_error(), Some(22) | Some(71) | Some(98)))
            || matches!(err, rdma_io::Error::InvalidArg(msg)
            if msg.starts_with("expected Established, got ")
                && matches!(
                    msg.strip_prefix("expected Established, got "),
                    Some("Rejected" | "Unreachable" | "ConnectError")
                ))
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

#[cfg(test)]
mod tests {
    use super::test_helpers::is_transient_cm_error;

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
