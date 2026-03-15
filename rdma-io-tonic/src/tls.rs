//! TLS integration for tonic gRPC over RDMA via [`tonic-tls`] and OpenSSL.
//!
//! Provides [`RdmaTransport`] (client) and an [`Incoming`] implementation for
//! [`RdmaIncoming`] (server), enabling TLS-encrypted gRPC over RDMA using
//! [`tonic_tls::openssl`].
//!
//! # Client
//!
//! ```no_run
//! use rdma_io::rdma_transport::TransportConfig;
//! use rdma_io_tonic::tls::RdmaTransport;
//! use tonic::transport::Endpoint;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let transport = RdmaTransport::new(TransportConfig::stream());
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
//! use rdma_io::rdma_transport::TransportConfig;
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
//! let incoming = RdmaIncoming::bind(
//!     &"0.0.0.0:50051".parse().unwrap(),
//!     TransportConfig::stream(),
//! )?;
//! let tls_incoming = tonic_tls::openssl::TlsIncoming::new(incoming, acceptor);
//! // Server::builder().add_service(svc).serve_with_incoming(tls_incoming).await?;
//! # Ok(())
//! # }
//! ```

use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::transport::TransportBuilder;
use tonic::transport::Uri;

use crate::RdmaIncoming;
use crate::connector::uri_to_socket_addr;
use crate::stream::TokioRdmaStream;

/// RDMA transport for [`tonic_tls`].
///
/// Implements [`tonic_tls::Transport`] by establishing raw RDMA connections
/// using the given transport builder. Pass this to
/// [`tonic_tls::openssl::TlsConnector::new`] to get a TLS-wrapped tonic
/// client connector.
#[derive(Clone, Debug)]
pub struct RdmaTransport<B: TransportBuilder> {
    builder: B,
}

impl<B: TransportBuilder> RdmaTransport<B> {
    /// Create a transport with the given builder.
    pub fn new(builder: B) -> Self {
        Self { builder }
    }
}

impl<B: TransportBuilder> tonic_tls::Transport for RdmaTransport<B> {
    type Io = TokioRdmaStream<B::Transport>;
    type Error = rdma_io::Error;

    async fn connect(&self, uri: &Uri) -> Result<Self::Io, Self::Error> {
        let addr = uri_to_socket_addr(uri)?;
        let transport = self.builder.connect(&addr).await?;
        let stream = AsyncRdmaStream::new(transport);
        Ok(TokioRdmaStream::new(stream))
    }
}

impl<B: TransportBuilder> tonic_tls::Incoming for RdmaIncoming<B> {
    type Io = TokioRdmaStream<B::Transport>;
    type Error = rdma_io::Error;
}
