//! TLS termination for the HTTPS proxy listener (client -> proxy leg).
//!
//! rustls (ring backend). ALPN offers `h2` (when enabled) and `http/1.1`;
//! the server dispatches on the negotiated protocol after the handshake.

use std::sync::Arc;

use anyhow::Context;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::config::TlsConfig;

/// Builds the acceptor from the PEM certificate chain and private key.
/// `ktls`: the session secrets must be extractable for the kernel offload.
pub fn acceptor(cfg: &TlsConfig, ktls: bool) -> anyhow::Result<TlsAcceptor> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cfg.cert)
        .with_context(|| format!("reading certificate {}", cfg.cert))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing certificate {}", cfg.cert))?;
    anyhow::ensure!(!certs.is_empty(), "no certificate in {}", cfg.cert);
    let key = PrivateKeyDer::from_pem_file(&cfg.key)
        .with_context(|| format!("reading private key {}", cfg.key))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate/key mismatch")?;
    server.enable_secret_extraction = ktls;
    server.alpn_protocols = if cfg.http2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    Ok(TlsAcceptor::from(Arc::new(server)))
}
