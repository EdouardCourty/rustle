//! Self-signed certificates (rcgen) and rustls configs for both ends.

use std::sync::{Arc, OnceLock};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct Cert {
    pub cert_pem: String,
    pub key_pem: String,
    pub der: CertificateDer<'static>,
    key_der: Vec<u8>,
}

/// One certificate for `localhost` / 127.0.0.1 / ::1, shared by the proxy's
/// TLS listener and the TLS origin.
pub fn localhost() -> &'static Cert {
    static CERT: OnceLock<Cert> = OnceLock::new();
    CERT.get_or_init(|| {
        let names = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
        let ck = rcgen::generate_simple_self_signed(names).expect("rcgen");
        Cert {
            cert_pem: ck.cert.pem(),
            key_pem: ck.signing_key.serialize_pem(),
            der: ck.cert.der().clone(),
            key_der: ck.signing_key.serialize_der(),
        }
    })
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Client config trusting [`localhost`] only (certificate verified).
pub fn client(alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(localhost().der.clone()).expect("root");
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(cfg)
}

/// Same, limited to TLS 1.2 (kTLS TLS 1.2 path).
pub fn client_tls12(alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(localhost().der.clone()).expect("root");
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(cfg)
}

pub fn server() -> Arc<rustls::ServerConfig> {
    let c = localhost();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(c.key_der.clone()));
    let cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![c.der.clone()], key)
        .expect("server cert");
    Arc::new(cfg)
}
