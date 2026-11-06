# durations

Parses human-written durations such as `90s` or `2h` into a `std::time::Duration`.

```rust
assert_eq!(durations::parse_duration("2m"), Some(std::time::Duration::from_secs(120)));
```
