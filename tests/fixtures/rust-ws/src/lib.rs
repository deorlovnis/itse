//! Itse's cargo fixture: one failing and one passing unit test, and integration tests in a second
//! test binary, so a run shows that `--no-fail-fast` gets past the first failure.

/// The sum of two numbers.
pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

/// How many lines an order saves; it drops the last one.
pub fn saved_lines(order: &[u32]) -> usize {
    order.len() - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_two_numbers() {
        assert_eq!(add(2, 2), 4);
    }

    #[test]
    fn saves_every_line() {
        assert_eq!(saved_lines(&[1, 2, 3]), 3);
    }
}
