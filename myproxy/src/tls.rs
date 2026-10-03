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
    // AES-128-GCM first, and the server's order wins: ring lists AES-256
    // first and rustls follows the client's preference by default, so
    // sessions ran AES-256 (14 rounds vs 10) for no practical gain.
    let mut provider = rustls::crypto::ring::default_provider();
    provider.cipher_suites.sort_by_key(|s| match s.suite() {
        rustls::CipherSuite::TLS13_AES_128_GCM_SHA256
        | rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
        | rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => 0,
        _ => 1,
    });
    let provider = Arc::new(provider);
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate/key mismatch")?;
    server.ignore_client_order = true;
    server.enable_secret_extraction = ktls;
    // Stateless resumption tickets (key shared by every shard, read under
    // an RwLock) instead of the default stateful cache, a Mutex taken by
    // every full handshake on every shard.
    server.ticketer = rustls::crypto::ring::Ticketer::new()?;
    server.alpn_protocols = if cfg.http2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    Ok(TlsAcceptor::from(Arc::new(server)))
}
