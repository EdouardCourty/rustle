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
        let Ok(decoded) = B64.decode(b64) else {
            return false;
        };
        // Key is the part after the first ':' (or the whole value).
        let key = match decoded.iter().position(|&b| b == b':') {
            Some(i) => &decoded[i + 1..],
            None => &decoded[..],
        };
        self.keys.iter().any(|k| bool::from(k.ct_eq(key)))
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
