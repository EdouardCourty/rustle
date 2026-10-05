use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use subtle::ConstantTimeEq;

/// Validates `Proxy-Authorization: Basic <base64(user:key)>` against a set of
/// API keys. Any username is accepted; the password must match one of the keys.
/// When `api_keys` is empty, authentication is disabled and everything passes.
#[derive(Clone)]
pub struct Authenticator {
    keys: Vec<Vec<u8>>,
    enabled: bool,
}

impl Authenticator {
    pub fn new(api_keys: &[String]) -> Self {
        Self {
            keys: api_keys.iter().map(|k| k.as_bytes().to_vec()).collect(),
            enabled: !api_keys.is_empty(),
        }
    }

    /// `header` is the raw `Proxy-Authorization` header value, if present.
    pub fn check(&self, header: Option<&[u8]>) -> bool {
        if !self.enabled {
            return true;
        }
        let Some(value) = header else {
            return false;
        };
        // "Basic " prefix, case-insensitive.
        if value.len() < 6 || !value[..6].eq_ignore_ascii_case(b"Basic ") {
            return false;
        }
        let b64 = value[6..].trim_ascii();
        // Decode into a stack buffer — no per-request allocation in
        // the hot path. The capacity estimate (`len/4*3`) is an upper bound
        // of the decoded size (padding only shrinks it), so the decode
        // cannot overflow the scratch. Only absurdly long tokens take the
        // allocating fallback.
        const MAX_DECODED: usize = 192;
        if b64.len() / 4 * 3 <= MAX_DECODED {
            let mut scratch = [0u8; MAX_DECODED];
            return match B64.decode_slice(b64, &mut scratch) {
                Ok(n) => self.match_key(&scratch[..n]),
                Err(_) => false,
            };
        }
        let Ok(decoded) = B64.decode(b64) else {
            return false;
        };
        self.match_key(&decoded)
    }

    /// Key is the part after the first ':' (or the whole value).
    fn match_key(&self, decoded: &[u8]) -> bool {
        let key = match decoded.iter().position(|&b| b == b':') {
            Some(i) => &decoded[i + 1..],
            None => decoded,
        };
        self.match_key_raw(key)
    }

    /// SOCKS5 username/password auth: the password is the API key.
    pub fn check_key(&self, key: &[u8]) -> bool {
        !self.enabled || self.match_key_raw(key)
    }

    /// Every key is compared (no short-circuit): the timing does not
    /// reveal which key matched.
    fn match_key_raw(&self, key: &[u8]) -> bool {
        self.keys
            .iter()
            .fold(subtle::Choice::from(0), |acc, k| acc | k.ct_eq(key))
            .into()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        B64.encode(s)
    }

    #[test]
    fn disabled_accepts_all() {
        let a = Authenticator::new(&[]);
        assert!(a.check(None));
        assert!(a.check(Some(b"whatever")));
    }

    #[test]
    fn valid_key_with_user() {
        let a = Authenticator::new(&["secret1".to_string()]);
        let header = format!("Basic {}", b64("bench:secret1"));
        assert!(a.check(Some(header.as_bytes())));
    }

    #[test]
    fn valid_key_without_user() {
        let a = Authenticator::new(&["secret1".to_string()]);
        let header = format!("Basic {}", b64("secret1"));
        assert!(a.check(Some(header.as_bytes())));
    }

    #[test]
    fn wrong_key_rejected() {
        let a = Authenticator::new(&["secret1".to_string()]);
        let header = format!("Basic {}", b64("bench:wrong"));
        assert!(!a.check(Some(header.as_bytes())));
    }

    #[test]
    fn malformed_rejected() {
        let a = Authenticator::new(&["secret1".to_string()]);
        assert!(!a.check(None));
        assert!(!a.check(Some(b"Bearer token")));
        assert!(!a.check(Some(b"Basic !!!not-base64!!!")));
    }
}
