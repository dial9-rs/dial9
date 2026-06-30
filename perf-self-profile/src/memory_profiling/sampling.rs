//! Geometric/Poisson sampling primitive

/// Minimal splitmix64 PRNG. Fast, no dependencies, good enough for sampling.
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Draw from an exponential distribution with the given mean. Returns at
    /// least 1 to avoid immediate re-trigger when the counter goes to zero.
    /// The unit is whatever the caller treats `mean` as (here: bytes).
    pub(crate) fn draw_exponential(&mut self, mean: u64) -> u64 {
        // Uniform float in (0, 1] — avoid exact 0 to prevent ln(0).
        let u = (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64);
        let u = if u == 0.0 { f64::MIN_POSITIVE } else { u };
        let sample = -u.ln() * (mean as f64);
        (sample as u64).max(1)
    }
}
