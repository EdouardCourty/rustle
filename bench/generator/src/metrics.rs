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
