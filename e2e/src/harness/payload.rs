//! Deterministic payloads: byte `i` of payload `seed` is a pure function of
//! (seed, i), so either side can regenerate or check any slice without
//! storing it.

/// Byte at offset `i` of payload `seed` (xorshift of the position: no
/// period shorter than the payload, sensitive to shifts and swaps).
#[inline]
pub fn byte(seed: u64, i: u64) -> u8 {
    let mut x = i.wrapping_add(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ 0xD1B5_4A32_D192_ED03;
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 29;
    x as u8
}

/// Fills `out` with bytes `[from, from + out.len())` of payload `seed`.
pub fn fill(seed: u64, from: u64, out: &mut [u8]) {
    for (k, b) in out.iter_mut().enumerate() {
        *b = byte(seed, from + k as u64);
    }
}

pub fn make(seed: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    fill(seed, 0, &mut v);
    v
}

/// Checks `got` against payload `seed` of `len` bytes; the error names the
/// first differing offset (truncation, corruption or extra bytes).
pub fn check(seed: u64, len: usize, got: &[u8]) -> anyhow::Result<()> {
    for (i, &b) in got.iter().enumerate().take(len) {
        if b != byte(seed, i as u64) {
            anyhow::bail!(
                "payload corrupted at byte {i} of {len} (got {} bytes)",
                got.len()
            );
        }
    }
    anyhow::ensure!(
        got.len() == len,
        "payload length {} != expected {len}",
        got.len()
    );
    Ok(())
}

/// Size label for scenario names: 0b, 8193b, 64kb, 1mb...
pub fn label(n: usize) -> String {
    match n {
        n if n >= 1 << 20 && n % (1 << 20) == 0 => format!("{}mb", n >> 20),
        n if n >= 1 << 10 && n % (1 << 10) == 0 => format!("{}kb", n >> 10),
        n => format!("{n}b"),
    }
}
