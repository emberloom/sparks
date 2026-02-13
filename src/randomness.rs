use std::time::Duration;

use rand::Rng;

/// Return a duration of `base_secs` +/- `jitter_fraction` (0.0–1.0).
pub fn jitter_interval(base_secs: u64, jitter_fraction: f64) -> Duration {
    let mut rng = rand::thread_rng();
    let jitter = jitter_fraction.clamp(0.0, 1.0);
    let lo = base_secs as f64 * (1.0 - jitter);
    let hi = base_secs as f64 * (1.0 + jitter);
    let secs = rng.gen_range(lo..=hi);
    Duration::from_secs_f64(secs.max(1.0))
}

/// Stochastic gate: return true with probability proportional to `relevance * spontaneity`.
pub fn should_speak(relevance: f32, spontaneity: f32) -> bool {
    let mut rng = rand::thread_rng();
    let threshold = (relevance * spontaneity).clamp(0.0, 1.0);
    rng.gen::<f32>() < threshold
}

/// Weighted random selection — returns index into `weights`.
pub fn weighted_select(weights: &[f32]) -> usize {
    if weights.is_empty() {
        return 0;
    }
    let total: f32 = weights.iter().sum();
    if total <= 0.0 {
        return 0;
    }
    let mut rng = rand::thread_rng();
    let mut roll = rng.gen::<f32>() * total;
    for (i, &w) in weights.iter().enumerate() {
        roll -= w;
        if roll <= 0.0 {
            return i;
        }
    }
    weights.len() - 1
}

/// Sample `count` unique indices from `0..total` without replacement.
pub fn sample_indices(count: usize, total: usize) -> Vec<usize> {
    if count >= total {
        return (0..total).collect();
    }
    let mut rng = rand::thread_rng();
    let mut indices: Vec<usize> = (0..total).collect();
    // Fisher-Yates partial shuffle
    for i in 0..count {
        let j = rng.gen_range(i..total);
        indices.swap(i, j);
    }
    indices.truncate(count);
    indices
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jitter_interval_within_bounds() {
        for _ in 0..100 {
            let d = jitter_interval(100, 0.2);
            let secs = d.as_secs_f64();
            assert!(secs >= 80.0 && secs <= 120.0, "got {}", secs);
        }
    }

    #[test]
    fn test_jitter_zero_fraction() {
        let d = jitter_interval(60, 0.0);
        assert_eq!(d.as_secs(), 60);
    }

    #[test]
    fn test_should_speak_always_at_one() {
        // With relevance=1.0 and spontaneity=1.0, should almost always pass
        let mut passes = 0;
        for _ in 0..100 {
            if should_speak(1.0, 1.0) {
                passes += 1;
            }
        }
        assert!(passes > 90);
    }

    #[test]
    fn test_should_speak_never_at_zero() {
        for _ in 0..100 {
            assert!(!should_speak(0.0, 0.5));
            assert!(!should_speak(0.5, 0.0));
        }
    }

    #[test]
    fn test_weighted_select_deterministic_single() {
        assert_eq!(weighted_select(&[1.0]), 0);
    }

    #[test]
    fn test_sample_indices_count_exceeds_total() {
        let result = sample_indices(10, 3);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_sample_indices_uniqueness() {
        let result = sample_indices(5, 20);
        assert_eq!(result.len(), 5);
        let mut sorted = result.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 5);
    }

}
