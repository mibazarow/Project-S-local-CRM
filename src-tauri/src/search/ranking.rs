// Reciprocal rank fusion: each leg contributes 1 / (RRF_K + rank), rank starts at 1.
pub(crate) const RRF_K: f64 = 60.0;

// RRF contribution of a result at the given zero-based rank.
pub(crate) fn rrf_score(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64 + 1.0)
}
