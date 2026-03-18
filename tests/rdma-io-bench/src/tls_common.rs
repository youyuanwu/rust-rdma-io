use std::path::Path;

use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::X509;

pub fn build_acceptor(cert_path: &Path, key_path: &Path) -> SslAcceptor {
    let mut builder =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).expect("SslAcceptor");
    builder
        .set_certificate_file(cert_path, openssl::ssl::SslFiletype::PEM)
        .expect("set server cert");
    builder
        .set_private_key_file(key_path, openssl::ssl::SslFiletype::PEM)
        .expect("set server key");
    builder.set_alpn_protos(b"\x02h2").expect("set ALPN h2");
    // No client verification for benchmarking
    builder.set_verify(SslVerifyMode::NONE);
    builder.build()
}

pub fn build_connector(cert_path: &Path) -> SslConnector {
    let cert_pem = std::fs::read(cert_path).expect("read cert PEM");
    let mut builder = SslConnector::builder(SslMethod::tls_client()).expect("SslConnector");
    // Direct-trust: add the server's self-signed cert to the trust store
    builder
        .cert_store_mut()
        .add_cert(X509::from_pem(&cert_pem).expect("parse cert"))
        .expect("add cert to store");
    builder.set_alpn_protos(b"\x02h2").expect("set ALPN h2");
    builder.build()
}
