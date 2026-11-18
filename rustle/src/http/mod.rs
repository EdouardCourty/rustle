//! HTTP head parsing: request side (with proxy routing semantics) and
//! response side (body-length classification). These modules are pure
//! parsing layers — no pool, no relay, no policy.

pub mod request;
pub mod response;

pub use request::{HeadError, RequestHead, Span, Target, classify, read_head, rewrite_head_into};
pub use response::{RespError, ResponseHead, read_response_head};

/// Parser header-array capacity (proxy clients are small; 64 is generous).
pub(crate) const MAX_HEADERS: usize = 64;
