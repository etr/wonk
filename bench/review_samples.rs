//! Percentile calculation preserves the pairing of timed review phases.
pub fn percentiles(samples: &[f64]) -> (f64, f64, f64, f64) {
    let mut samples = samples.to_vec();
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len();
    let at = |q: f64| samples[((q * (n - 1) as f64).round()) as usize];
    (at(0.50), at(0.95), at(0.99), at(1.0))
}

pub fn phase_samples(full: &[f64], detect: &[f64]) -> Vec<f64> {
    assert_eq!(
        full.len(),
        detect.len(),
        "timed review phases must stay paired"
    );
    full.iter().zip(detect.iter()).map(|(f, d)| f - d).collect()
}

pub fn verify_pairing() {
    let full = [101.0, 12.0, 53.0];
    let detect = [10.0, 100.0, 50.0];
    // Exercise the actual benchmark's order: calculate each phase's
    // percentiles before computing their paired delta distribution.
    let _ = percentiles(&full);
    let _ = percentiles(&detect);
    assert_eq!(phase_samples(&full, &detect), vec![91.0, -88.0, 3.0]);
}

#[cfg(test)]
mod tests {
    #[test]
    fn skewed_phase_samples_preserve_iteration_pairing() {
        super::verify_pairing();
    }
}
