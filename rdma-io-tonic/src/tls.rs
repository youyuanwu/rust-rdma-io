//! TLS integration for tonic gRPC over RDMA via [`tonic-tls`] and OpenSSL.
//!
//! Provides [`RdmaTransport`] (client) and an [`Incoming`] implementation for
//! [`RdmaIncoming`] (server), enabling TLS-encrypted gRPC over RDMA using
//! [`tonic_tls::openssl`].
//!
//! # Client
//!
//! ```no_run
//! use rdma_io_tonic::RdmaTransport;
//! use tonic::transport::Endpoint;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let transport = RdmaTransport::new();
//! let ssl = openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls_client())?
//!     .build();
//! let connector = tonic_tls::openssl::TlsConnector::new(
//!     transport, ssl, "localhost".to_string(),
//! );
//! let channel = Endpoint::from_static("https://10.0.0.1:50051")
//!     .connect_with_connector(connector)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Server
//!
//! ```no_run
//! use rdma_io_tonic::RdmaIncoming;
//! use tonic::transport::Server;
//!
//! # async fn example(
//! #     svc: impl tonic::server::NamedService + Clone + Send + Sync + 'static
//! #         + tower_service::Service<http::Request<tonic::body::Body>,
//! #             Response = http::Response<tonic::body::Body>,
//! #             Error = core::convert::Infallible,
//! #             Future: Send + 'static>,
//! #     acceptor: openssl::ssl::SslAcceptor,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! let incoming = RdmaIncoming::bind(&"0.0.0.0:50051".parse().unwrap())?;
//! let tls_incoming = tonic_tls::openssl::TlsIncoming::new(incoming, acceptor);
//! // Server::builder().add_service(svc).serve_with_incoming(tls_incoming).await?;
//! # Ok(())
//! # }
//! ```

use rdma_io::async_stream::AsyncRdmaStream;
use tonic::transport::Uri;

use crate::RdmaIncoming;
use crate::connector::uri_to_socket_addr;
use crate::stream::TokioRdmaStream;

/// Default buffer size (matches AsyncRdmaStream default).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

/// RDMA transport for [`tonic_tls`].
///
/// Implements [`tonic_tls::Transport`] by establishing raw RDMA connections.
/// Pass this to [`tonic_tls::openssl::TlsConnector::new`] to get a TLS-wrapped
/// tonic client connector.
#[derive(Clone, Debug)]
pub struct RdmaTransport {
    buf_size: usize,
}

impl RdmaTransport {
    /// Create a transport with the default buffer size (64 KiB).
    pub fn new() -> Self {
        Self {
            buf_size: DEFAULT_BUF_SIZE,
        }
    }

    /// Create a transport with a custom buffer size.
    pub fn with_buf_size(buf_size: usize) -> Self {
        Self { buf_size }
    }
}

impl Default for RdmaTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl tonic_tls::Transport for RdmaTransport {
    type Io = TokioRdmaStream;
    type Error = rdma_io::Error;

    async fn connect(&self, uri: &Uri) -> Result<Self::Io, Self::Error> {
        let addr = uri_to_socket_addr(uri)?;
        let stream = AsyncRdmaStream::connect_with_buf_size(&addr, self.buf_size).await?;
        Ok(TokioRdmaStream::new(stream))
    }
}

impl tonic_tls::Incoming for RdmaIncoming {
    type Io = TokioRdmaStream;
    type Error = rdma_io::Error;
}
