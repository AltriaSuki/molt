//! The few statistics a benchmark report needs.

/// The 95% Wilson score interval for `k` successes in `n` trials, which
/// stays sensible at 0 and `n` successes and for small `n`. `(0, 1)` when
/// there are no trials.
pub fn wilson(k: u32, n: u32) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    const Z: f64 = 1.959_963_984_540_054;
    let (k, n) = (f64::from(k), f64::from(n));
    let p = k / n;
    let z2 = Z * Z;
    let center = (p + z2 / (2.0 * n)) / (1.0 + z2 / n);
    let half = Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / (1.0 + z2 / n);
    ((center - half).max(0.0), (center + half).min(1.0))
}

/// The two-sided p-value of the exact McNemar test: of the pairs where the
/// two arms disagree, `b` went one way and `c` the other. Under the null
/// hypothesis each disagreement goes either way with probability one half.
/// 1 when they never disagree.
pub fn mcnemar(b: u32, c: u32) -> f64 {
    let n = b + c;
    if n == 0 {
        return 1.0;
    }
    // P(X <= min(b, c)) for X ~ Binomial(n, 1/2), summed in log space so a
    // large n does not underflow.
    let ln_half_n = f64::from(n) * 0.5f64.ln();
    let mut ln_choose = 0.0;
    let mut tail = 0.0;
    for i in 0..=b.min(c) {
        if i > 0 {
            ln_choose += f64::from(n - i + 1).ln() - f64::from(i).ln();
        }
        tail += (ln_choose + ln_half_n).exp();
    }
    (2.0 * tail).min(1.0)
}

/// The median, or `None` for no values.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 { sorted[mid] } else { (sorted[mid - 1] + sorted[mid]) / 2.0 })
}

/// The mean, or `None` for no values.
pub fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn wilson_matches_known_intervals() {
        let (lo, hi) = wilson(8, 10);
        assert!(close(lo, 0.4902) && close(hi, 0.9433), "{lo} {hi}");
        let (lo, hi) = wilson(0, 10);
        assert!(close(lo, 0.0) && close(hi, 0.2775), "{lo} {hi}");
        let (lo, hi) = wilson(10, 10);
        assert!(close(lo, 0.7225) && close(hi, 1.0), "{lo} {hi}");
        assert_eq!(wilson(0, 0), (0.0, 1.0));
    }

    #[test]
    fn mcnemar_is_the_exact_binomial_test() {
        assert_eq!(mcnemar(0, 0), 1.0);
        assert!(close(mcnemar(1, 0), 1.0));
        // 2 * (1/2)^6
        assert!(close(mcnemar(6, 0), 0.03125));
        // 2 * P(X <= 2), X ~ Bin(12, 1/2) = 2 * 79/4096
        assert!(close(mcnemar(10, 2), 0.038_574));
        assert!(close(mcnemar(2, 10), mcnemar(10, 2)));
        assert!(close(mcnemar(5, 5), 1.0));
        let p = mcnemar(1200, 800);
        assert!(p > 0.0 && p < 1e-15, "{p}");
    }

    #[test]
    fn medians_and_means() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(mean(&[1.0, 2.0, 6.0]), Some(3.0));
        assert_eq!(mean(&[]), None);
    }
}
