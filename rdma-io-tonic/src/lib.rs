//! Tonic gRPC transport over RDMA.
//!
//! Provides adapter types to run [tonic](https://docs.rs/tonic) gRPC services
//! over RDMA instead of TCP, using `rdma-io`'s async stream primitives.
//!
//! # Server
//!
//! Use [`RdmaIncoming`] with `Server::serve_with_incoming`:
//!
//! ```no_run
//! use rdma_io_tonic::{RdmaIncoming, RdmaConnectInfo};
//! use tonic::transport::Server;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let incoming = RdmaIncoming::bind(&"0.0.0.0:50051".parse().unwrap())?;
//! // Server::builder().add_service(svc).serve_with_incoming(incoming).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Client
//!
//! Use [`RdmaConnector`] with `Endpoint::connect_with_connector`:
//!
//! ```no_run
//! use rdma_io_tonic::RdmaConnector;
//! use tonic::transport::Endpoint;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let connector = RdmaConnector::new();
//! let channel = Endpoint::from_static("http://10.0.0.1:50051")
//!     .connect_with_connector(connector)
//!     .await?;
//! # Ok(())
//! # }
//! ```

mod connector;
mod incoming;
mod stream;

pub use connector::RdmaConnector;
pub use incoming::RdmaIncoming;
pub use stream::{RdmaConnectInfo, TokioRdmaStream};
