//! Run-measurement plumbing: per-run field bag, JSON rendering (the result
//! document the harness collects), a short human line for the operator,
//! index-wise medians and latency percentiles.

/// One measured run. Static fields describe the workload; numeric fields
/// are the measurements. Field layout is identical across runs of one
/// mode, so medians can be taken index-wise.
pub(crate) struct Metrics {
    pub(crate) statics: Vec<(&'static str, String)>,
    pub(crate) nums: Vec<(&'static str, f64)>,
}

/// Workload fields that are numbers / booleans in the JSON document (every
/// other static is a string).
const INT_KEYS: [&str; 6] = ["conns", "duration", "ramp", "chunk", "size", "window"];
const BOOL_KEYS: [&str; 2] = ["keepalive", "short"];

/// A JSON string literal.
pub(crate) fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A measurement as a JSON number with three decimals (what the figures
/// have always been rounded to); NaN and infinities are `null`.
pub(crate) fn json_num(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.3}")
    } else {
        "null".to_string()
    }
}

impl Metrics {
    /// The workload description as a JSON object, `err_kinds` left out
    /// (it is a result, not a parameter).
    pub(crate) fn workload_json(&self) -> String {
        let fields: Vec<String> = self
            .statics
            .iter()
            .filter(|(k, _)| *k != "err_kinds")
            .map(|(k, v)| {
                let bare = (INT_KEYS.contains(k) && v.parse::<u64>().is_ok())
                    || (BOOL_KEYS.contains(k) && (v == "true" || v == "false"));
                let val = if bare { v.clone() } else { json_str(v) };
                format!("{}:{val}", json_str(k))
            })
            .collect();
        format!("{{{}}}", fields.join(","))
    }

    /// The measurements as a JSON object.
    pub(crate) fn nums_json(&self) -> String {
        let fields: Vec<String> = self
            .nums
            .iter()
            .map(|(k, v)| format!("{}:{}", json_str(k), json_num(*v)))
            .collect();
        format!("{{{}}}", fields.join(","))
    }

    /// The `err_kinds` static (client-side error kinds), if the mode has one.
    pub(crate) fn err_kinds(&self) -> Option<&str> {
        self.statics
            .iter()
            .find(|(k, _)| *k == "err_kinds")
            .map(|(_, v)| v.as_str())
    }

    /// One line for the operator on stderr; nothing downstream parses it.
    pub(crate) fn human(&self, tag: &str) -> String {
        let mut s = tag.to_string();
        for (k, v) in &self.statics {
            s.push_str(&format!(" {k}={v}"));
        }
        for (k, v) in &self.nums {
            s.push_str(&format!(" {k}={v:.3}"));
        }
        s
    }
}

pub(crate) fn median_of(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    // NaN-tolerant: a run with zero successful connects yields NaN
    // percentiles; unwrap() on NaN.partial_cmp(NaN) would panic.
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[(v.len() - 1) / 2]
}

pub(crate) fn median_metrics(runs: Vec<Metrics>) -> Metrics {
    let statics = runs[0].statics.clone();
    let mut nums = Vec::with_capacity(runs[0].nums.len());
    for (i, (key, _)) in runs[0].nums.iter().enumerate() {
        let vals: Vec<f64> = runs.iter().map(|r| r.nums[i].1).collect();
        nums.push((*key, median_of(vals)));
    }
    Metrics { statics, nums }
}

/// (p50, p95, p99) in milliseconds from a latency vector in seconds.
pub(crate) fn pcts_ms(v: &[f64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = |q: f64| -> f64 {
        if s.is_empty() {
            f64::NAN
        } else {
            let idx = ((s.len() - 1) as f64 * q).round() as usize;
            s[idx] * 1e3
        }
    };
    (p(0.50), p(0.95), p(0.99))
}

/// Log-linear latency histogram (nanoseconds, 128 sub-buckets per power of
/// two: < 1 % error). One per worker thread, merged at the end: recording
/// is an array increment, no lock, no per-request allocation.
pub(crate) struct Hist {
    counts: Vec<u64>,
    n: u64,
}

const SUB: usize = 128;
const SUB_BITS: u32 = 7;

impl Hist {
    pub(crate) fn new() -> Self {
        Self {
            counts: vec![0; 64 * SUB],
            n: 0,
        }
    }

    fn index(ns: u64) -> usize {
        if ns < SUB as u64 {
            return ns as usize;
        }
        let e = 63 - ns.leading_zeros(); // >= SUB_BITS
        let sub = ((ns >> (e - SUB_BITS)) as usize) & (SUB - 1);
        (e - SUB_BITS + 1) as usize * SUB + sub
    }

    fn value(idx: usize) -> u64 {
        if idx < SUB {
            return idx as u64;
        }
        let e = (idx / SUB) as u32 - 1 + SUB_BITS;
        ((SUB + idx % SUB) as u64) << (e - SUB_BITS)
    }

    #[inline]
    pub(crate) fn record(&mut self, d: std::time::Duration) {
        let ns = d.as_nanos().min(u64::MAX as u128) as u64;
        self.counts[Self::index(ns)] += 1;
        self.n += 1;
    }

    pub(crate) fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        self.n += o.n;
    }

    pub(crate) fn len(&self) -> u64 {
        self.n
    }

    /// (p50, p95, p99) in milliseconds.
    pub(crate) fn pcts_ms(&self) -> (f64, f64, f64) {
        let q = |q: f64| -> f64 {
            if self.n == 0 {
                return f64::NAN;
            }
            let rank = ((self.n - 1) as f64 * q).round() as u64 + 1;
            let mut acc = 0;
            for (i, c) in self.counts.iter().enumerate() {
                acc += c;
                if acc >= rank {
                    return Self::value(i) as f64 / 1e6;
                }
            }
            f64::NAN
        };
        (q(0.50), q(0.95), q(0.99))
    }
}

#[cfg(test)]
mod hist_tests {
    use super::*;

    #[test]
    fn percentiles_within_one_percent() {
        let mut h = Hist::new();
        for us in 1..=10_000u64 {
            h.record(std::time::Duration::from_micros(us));
        }
        let (p50, p95, p99) = h.pcts_ms();
        for (got, want) in [(p50, 5.0), (p95, 9.5), (p99, 9.9)] {
            assert!((got - want).abs() / want < 0.01, "{got} vs {want}");
        }
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;

    fn http() -> Metrics {
        Metrics {
            statics: vec![
                ("mode", "http".to_string()),
                ("err_kinds", "status-426=2".to_string()),
                ("url", "http://upstream:8000/1kb".to_string()),
                ("conns", "64".to_string()),
                ("budget", "15s".to_string()),
                ("keepalive", "true".to_string()),
            ],
            nums: vec![("rps", 55257.5949), ("p99_ms", f64::NAN), ("errors", 0.0)],
        }
    }

    #[test]
    fn workload_is_typed_and_leaves_out_err_kinds() {
        assert_eq!(
            http().workload_json(),
            r#"{"mode":"http","url":"http://upstream:8000/1kb","conns":64,"budget":"15s","keepalive":true}"#
        );
    }

    #[test]
    fn numbers_have_three_decimals_and_nan_is_null() {
        assert_eq!(
            http().nums_json(),
            r#"{"rps":55257.595,"p99_ms":null,"errors":0.000}"#
        );
        assert_eq!(http().err_kinds(), Some("status-426=2"));
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(json_str("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }
}
