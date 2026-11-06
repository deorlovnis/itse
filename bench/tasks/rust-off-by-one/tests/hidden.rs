use window::{mean, window};

#[test]
fn a_one_reading_window_holds_that_reading() {
    assert_eq!(window(&[7, 8, 9], 2..=2), &[9]);
}

#[test]
fn the_whole_series_is_a_window() {
    assert_eq!(window(&[7, 8, 9], 0..=2), &[7, 8, 9]);
    assert_eq!(mean(&[7, 8, 9], 0..=2), Some(8.0));
}
