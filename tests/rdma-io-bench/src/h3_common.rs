use std::path::Path;
use std::sync::Arc;

use quinn::rustls::pki_types::pem::PemObject;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Load cert and key from PEM files for H3/QUIC (rustls format).
pub fn load_certs_from_pem(
    cert_path: &Path,
    key_path: &Path,
) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let cert_pem = std::fs::read(cert_path).expect("read cert PEM");
    let key_pem = std::fs::read(key_path).expect("read key PEM");

    let certs: Vec<CertificateDer<'static>> =
        quinn::rustls::pki_types::pem::PemObject::pem_slice_iter(&cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .expect("parse cert PEM");
    let key = PrivateKeyDer::from_pem_slice(&key_pem).expect("parse key PEM");

    (certs, key)
}

pub fn make_server_config(
    certs: &[CertificateDer<'static>],
    key: PrivateKeyDer<'static>,
) -> quinn::ServerConfig {
    let mut tls_config = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs.to_vec(), key)
        .expect("server TLS config");
    tls_config.alpn_protocols = vec![b"h3".to_vec()];
    tls_config.max_early_data_size = u32::MAX;

    quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config).expect("QUIC server config"),
    ))
}

pub fn make_client_config(server_certs: &[CertificateDer<'static>]) -> quinn::ClientConfig {
    let mut roots = quinn::rustls::RootCertStore::empty();
    for cert in server_certs {
        roots.add(cert.clone()).expect("add root cert");
    }
    let mut tls_config = quinn::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls_config).expect("QUIC client config"),
    ))
}
