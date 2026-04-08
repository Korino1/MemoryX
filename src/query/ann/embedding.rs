//! Cosine similarity computation with SIMD optimization
//!
//! Provides efficient computation of cosine similarity between
//! embedding vectors for semantic search ranking.

/// Compute cosine similarity between two vectors.
///
/// Returns value in range [-1.0, 1.0] where:
/// - 1.0: vectors are identical in direction
/// - 0.0: vectors are orthogonal
/// - -1.0: vectors are opposite
///
/// Returns 0.0 if vectors have different lengths or are empty.
#[inline]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot_product = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    // Process in chunks of 8 for potential SIMD auto-vectorization
    let chunks = a.len() / 8;
    let _remainder = a.len() % 8;

    // Chunked processing
    for i in 0..chunks {
        let offset = i * 8;
        let a_chunk = &a[offset..offset + 8];
        let b_chunk = &b[offset..offset + 8];

        dot_product += a_chunk[0] * b_chunk[0]
            + a_chunk[1] * b_chunk[1]
            + a_chunk[2] * b_chunk[2]
            + a_chunk[3] * b_chunk[3]
            + a_chunk[4] * b_chunk[4]
            + a_chunk[5] * b_chunk[5]
            + a_chunk[6] * b_chunk[6]
            + a_chunk[7] * b_chunk[7];

        norm_a += a_chunk[0] * a_chunk[0]
            + a_chunk[1] * a_chunk[1]
            + a_chunk[2] * a_chunk[2]
            + a_chunk[3] * a_chunk[3]
            + a_chunk[4] * a_chunk[4]
            + a_chunk[5] * a_chunk[5]
            + a_chunk[6] * a_chunk[6]
            + a_chunk[7] * a_chunk[7];

        norm_b += b_chunk[0] * b_chunk[0]
            + b_chunk[1] * b_chunk[1]
            + b_chunk[2] * b_chunk[2]
            + b_chunk[3] * b_chunk[3]
            + b_chunk[4] * b_chunk[4]
            + b_chunk[5] * b_chunk[5]
            + b_chunk[6] * b_chunk[6]
            + b_chunk[7] * b_chunk[7];
    }

    // Process remainder
    for i in (chunks * 8)..a.len() {
        dot_product += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-8 {
        0.0
    } else {
        dot_product / denom
    }
}

/// Compute L2 (Euclidean) distance between two vectors.
///
/// Returns 0.0 if vectors have different lengths.
#[inline]
pub fn l2_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return f32::MAX;
    }

    let mut sum = 0.0f32;
    for i in 0..a.len() {
        let diff = a[i] - b[i];
        sum += diff * diff;
    }
    sum.sqrt()
}

/// Compute dot product between two vectors.
#[inline]
pub fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let mut result = 0.0f32;
    for i in 0..a.len() {
        result += a[i] * b[i];
    }
    result
}

/// Normalize a vector in-place.
#[inline]
pub fn normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
}
