//! Shared plumbing for the protocol benchmark modes (`grpc`, `h1`).
//!
//! Holds the option bundles the client/server binaries build from their CLI
//! args and hand to the per-protocol modules, plus the connection-retry helper
//! and the RDMA setup-error hint used across those modules.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Client-side run parameters shared by all gRPC/HTTP benchmark modes.
///
/// The binary builds this once from its CLI `Args` (after selecting and
/// constructing the RDMA transport) and passes it to the per-protocol client
/// runners, mirroring how `echo` takes explicit parameters instead of the
/// binary-local `Args` struct.
#[derive(Clone, Debug)]
pub struct ClientOpts {
    pub connect: SocketAddr,
    pub connections: usize,
    pub threads: usize,
    /// Concurrent in-flight RPCs per connection (h2/h3). HTTP/1.1 has no
    /// multiplexing and treats this as 1.
    pub in_flight: usize,
    pub warmup: u64,
    pub duration: u64,
    /// Payload size in bytes; the runner builds the request body from this.
    pub payload: usize,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub report: String,
}

/// Server-side run parameters shared by all gRPC/HTTP benchmark modes.
#[derive(Clone, Debug)]
pub struct ServerOpts {
    pub bind: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Wrap an RDMA setup error with a hint when a ring transport hits EINVAL,
/// which on Azure NICs means Memory Windows are unsupported (`max_mw=0`).
pub fn transport_setup_error(transport: &str, err: rdma_io::Error) -> Box<dyn std::error::Error> {
    if transport != "send-recv" {
        format!(
            "transport '{transport}' failed during RDMA setup: {err}\n\
             hint: read-ring and credit-ring auto-detect Memory Window support and \
             fall back to the MR rkey on NICs that report max_mw=0 (e.g. Azure MANA). \
             If you passed --require-mw on such a NIC, drop it to allow the fallback."
        )
        .into()
    } else {
        format!("transport '{transport}' failed during RDMA setup: {err}").into()
    }
}

/// Establish one connection with bounded retries and backoff.
///
/// read-ring's RDMA-CM is flaky under connect churn on the Azure MANA RoCEv2
/// preview: a connection can transiently fail the handshake ("expected
/// Established, got Unreachable") or hang mid-handshake. A single-attempt connect
/// would abort the entire benchmark whenever any one of the N connections trips,
/// masking data-path results as setup flakiness. Retry each connection up to
/// `MAX_ATTEMPTS` times with linear backoff; bound every attempt with
/// `per_attempt` so a hung handshake is retried rather than blocking forever.
pub async fn connect_with_retry<T, F, Fut>(
    what: &str,
    per_attempt: Duration,
    mut make: F,
) -> Result<T, Box<dyn std::error::Error>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Box<dyn std::error::Error>>>,
{
    const MAX_ATTEMPTS: u32 = 5;
    let mut last_err: Option<Box<dyn std::error::Error>> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match tokio::time::timeout(per_attempt, make()).await {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => {
                eprintln!("{what}: connect attempt {attempt}/{MAX_ATTEMPTS} failed: {e}");
                last_err = Some(e);
            }
            Err(_) => {
                eprintln!(
                    "{what}: connect attempt {attempt}/{MAX_ATTEMPTS} timed out after {per_attempt:?}"
                );
                last_err = Some(format!("connect timed out after {per_attempt:?}").into());
            }
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
        }
    }
    Err(last_err.unwrap_or_else(|| "connect failed".into()))
}
