const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Stable FNV-1a 64-bit hash, the format of `NewLesson::provenance_hash`.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a64_update(FNV_OFFSET, bytes)
}

pub(crate) fn fnv1a64_start() -> u64 {
    FNV_OFFSET
}

pub(crate) fn fnv1a64_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub(crate) fn row_id(parts: &[&str]) -> i64 {
    let mut hash = FNV_OFFSET;
    for part in parts {
        hash = fnv1a64_update(hash, part.as_bytes());
        hash = fnv1a64_update(hash, &[0]);
    }
    hash as i64
}

/// Cost-weighted tokens: input + 0.1 x cache_read + 1.25 x cache_write + 5 x output.
pub fn weighted_tokens(input: u64, cache_read: u64, cache_write: u64, output: u64) -> f64 {
    input as f64 + 0.1 * cache_read as f64 + 1.25 * cache_write as f64 + 5.0 * output as f64
}

pub(crate) fn is_holdout(run_id: &str) -> bool {
    // FNV low bits correlate on sequential ids, so finalise before taking the modulus.
    let mut h = fnv1a64(run_id.as_bytes());
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    h % 100 < 20
}

fn ln_gamma(x: f64) -> f64 {
    const COEF: [f64; 6] = [
        76.180_091_729_471_46,
        -86.505_320_329_416_77,
        24.014_098_240_830_91,
        -1.231_739_572_450_155,
        0.001_208_650_973_866_179,
        -0.000_005_395_239_384_953,
    ];
    let tmp = x + 5.5;
    let tmp = tmp - (x + 0.5) * tmp.ln();
    let mut ser = 1.000_000_000_190_015;
    let mut y = x;
    for coef in COEF {
        y += 1.0;
        ser += coef / y;
    }
    -tmp + (2.506_628_274_631_000_5 * ser / x).ln()
}

fn guard_tiny(value: f64) -> f64 {
    const TINY: f64 = 1e-30;
    if value.abs() < TINY {
        TINY
    } else {
        value
    }
}

fn beta_cf(x: f64, a: f64, b: f64) -> f64 {
    let (qab, qap, qam) = (a + b, a + 1.0, a - 1.0);
    let mut c = 1.0;
    let mut d = 1.0 / guard_tiny(1.0 - qab * x / qap);
    let mut h = d;
    for m in 1..300 {
        let m = f64::from(m);
        let m2 = 2.0 * m;
        let even = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 / guard_tiny(1.0 + even * d);
        c = guard_tiny(1.0 + even / c);
        h *= d * c;
        let odd = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 / guard_tiny(1.0 + odd * d);
        c = guard_tiny(1.0 + odd / c);
        let delta = d * c;
        h *= delta;
        if (delta - 1.0).abs() < 1e-10 {
            break;
        }
    }
    h
}

fn beta_cdf(x: f64, a: f64, b: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front =
        (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        front * beta_cf(x, a, b) / a
    } else {
        1.0 - front * beta_cf(1.0 - x, b, a) / b
    }
}

fn beta_quantile(p: f64, a: f64, b: f64) -> f64 {
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..50 {
        let mid = 0.5 * (lo + hi);
        if beta_cdf(mid, a, b) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

pub(crate) fn beta_lower(a: f64, b: f64) -> f64 {
    beta_quantile(0.1, a, b)
}

pub(crate) fn beta_upper(a: f64, b: f64) -> f64 {
    beta_quantile(0.9, a, b)
}

pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    // Open interval: callers take logarithms, so exactly 0 would produce infinities.
    pub(crate) fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }

    fn gamma(&mut self, shape: f64) -> f64 {
        if shape < 1.0 {
            let u = self.uniform();
            return self.gamma(shape + 1.0) * u.powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = (9.0 * d).sqrt().recip();
        loop {
            let x = self.normal();
            let v = 1.0 + c * x;
            if v <= 0.0 {
                continue;
            }
            let v = v * v * v;
            if self.uniform().ln() < 0.5 * x * x + d - d * v + d * v.ln() {
                return d * v;
            }
        }
    }

    pub(crate) fn beta(&mut self, a: f64, b: f64) -> f64 {
        let x = self.gamma(a);
        let y = self.gamma(b);
        x / (x + y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_matches_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn beta_quantile_matches_closed_form() {
        // Beta(1, b) has CDF 1 - (1 - x)^b, so its quantile is exact.
        let expected = 1.0 - 0.1f64.powf(1.0 / 6.0);
        assert!((beta_upper(1.0, 6.0) - expected).abs() < 1e-6);
        assert!((beta_lower(1.0, 1.0) - 0.1).abs() < 1e-6);
    }

    #[test]
    fn beta_sampler_mean_matches_parameters() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let n = 20_000;
        let mean: f64 = (0..n).map(|_| rng.beta(2.0, 6.0)).sum::<f64>() / f64::from(n);
        assert!((mean - 0.25).abs() < 0.01, "mean {mean}");
        let small: f64 = (0..n).map(|_| rng.beta(0.5, 0.5)).sum::<f64>() / f64::from(n);
        assert!((small - 0.5).abs() < 0.02, "mean {small}");
    }

    #[test]
    fn weights_follow_cost_formula() {
        assert!((weighted_tokens(100, 1000, 100, 10) - 375.0).abs() < 1e-9);
    }
}
