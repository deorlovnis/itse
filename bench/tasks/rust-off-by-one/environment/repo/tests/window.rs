use window::{mean, window};

#[test]
fn window_includes_both_ends() {
    assert_eq!(window(&[1, 2, 3, 4, 5], 1..=3), &[2, 3, 4]);
}

#[test]
fn mean_of_a_window() {
    assert_eq!(mean(&[10, 20, 30, 40], 0..=1), Some(15.0));
}
