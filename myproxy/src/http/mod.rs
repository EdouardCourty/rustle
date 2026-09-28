//! HTTP head parsing: request side (with proxy routing semantics) and
//! response side (body-length classification). These modules are pure
//! parsing layers — no pool, no relay, no policy.

pub mod request;
pub mod response;

pub use request::{classify, read_head, rewrite_head, HeadError, RequestHead, Target};
pub use response::{read_response_head, RespError};

/// Parser header-array capacity (proxy clients are small; 64 is generous).
pub(crate) const MAX_HEADERS: usize = 64;
