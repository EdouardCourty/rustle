//! Scenario suites, one file per feature. Each suite exposes `scenarios()`;
//! names are `<suite>/<...>` and the first segment is the report category.

pub mod connect;
pub mod h2;
pub mod http;
pub mod interop_curl;
pub mod limits;
pub mod logs;
pub mod matrix;
pub mod process;
pub mod robustness;
pub mod security;
pub mod socks5;
pub mod uploads;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::clients::http1::Client;
use crate::harness::{certs, Proxy};
use crate::runner::Scenario;

pub fn all() -> Vec<Scenario> {
    let mut v = Vec::new();
    v.extend(matrix::scenarios());
    v.extend(http::scenarios());
    v.extend(connect::scenarios());
    v.extend(h2::scenarios());
    v.extend(socks5::scenarios());
    v.extend(uploads::scenarios());
    v.extend(limits::scenarios());
    v.extend(security::scenarios());
    v.extend(robustness::scenarios());
    v.extend(process::scenarios());
    v.extend(logs::scenarios());
    v.extend(interop_curl::scenarios());
    v
}

static QUICK: AtomicBool = AtomicBool::new(false);

/// `--quick`: scenarios that loop over sizes use the short list.
pub fn set_quick(q: bool) {
    QUICK.store(q, Ordering::Relaxed);
}

pub fn quick_mode() -> bool {
    QUICK.load(Ordering::Relaxed)
}

/// HTTP/1.1 client transports of the proxy.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum H1 {
    Plain,
    Tls,
    /// TLS 1.2 only (kTLS TLS 1.2 code path).
    Tls12,
}

impl H1 {
    pub fn name(self) -> &'static str {
        match self {
            H1::Plain => "h1",
            H1::Tls => "tls",
            H1::Tls12 => "tls12",
        }
    }

    pub async fn client(self, p: &Proxy) -> anyhow::Result<Client> {
        match self {
            H1::Plain => Client::plain(p.addr).await,
            H1::Tls => Client::tls(p.tls(), true).await,
            H1::Tls12 => {
                let t = crate::clients::tls_connect(p.tls(), certs::client_tls12(&[])).await?;
                anyhow::ensure!(
                    t.get_ref().1.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_2),
                    "TLS 1.2 not negotiated"
                );
                Ok(Client::over(Box::new(t)))
            }
        }
    }
}
