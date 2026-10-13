#[allow(
    clippy::double_must_use,
    reason = "tonic's generated async_trait methods add redundant must_use attributes"
)]
pub mod greeter {
    tonic::include_proto!("greeter");
}

pub mod common;
pub mod echo;
pub mod grpc;
pub mod h1;
pub mod h3_common;
pub mod load;
pub mod metrics;
pub mod report;
pub mod tls_common;
