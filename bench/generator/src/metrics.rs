//! Run-measurement plumbing: per-run field bag, RESULT-line rendering,
//! index-wise medians and latency percentiles.

/// One measured run. Static fields describe the workload; numeric fields
/// are the measurements. Field layout is identical across runs of one
/// mode, so medians can be taken index-wise.
pub(crate) struct Metrics {
    pub(crate) statics: Vec<(&'static str, String)>,
    pub(crate) nums: Vec<(&'static str, f64)>,
}

impl Metrics {
    pub(crate) fn line(&self, tag: &str) -> String {
        let mut s = format!("RESULT {tag}");
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
