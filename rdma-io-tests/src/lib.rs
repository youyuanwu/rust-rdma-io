//! Integration tests for rdma-io-sys and rdma-io against SoftIWarp (siw).
//!
//! These tests require a siw device to be loaded:
//! ```bash
//! sudo modprobe siw
//! sudo rdma link add siw0 type siw netdev eth0
//! ```

#[cfg(test)]
mod cm_tests;
#[cfg(test)]
mod safe_api_tests;
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod sys_tests;
