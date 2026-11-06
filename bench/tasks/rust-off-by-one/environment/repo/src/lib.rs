//! Windows over a series of readings.

use std::ops::RangeInclusive;

/// The readings whose index lies in `range`, both ends included.
pub fn window(readings: &[u32], range: RangeInclusive<usize>) -> &[u32] {
    let (start, end) = range.into_inner();
    &readings[start..end]
}

/// The mean of the readings in `range`, or None for an empty window.
pub fn mean(readings: &[u32], range: RangeInclusive<usize>) -> Option<f64> {
    let w = window(readings, range);
    if w.is_empty() {
        return None;
    }
    Some(w.iter().map(|&r| f64::from(r)).sum::<f64>() / w.len() as f64)
}
