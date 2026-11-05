//! A second test binary: it runs although the lib's tests failed first.

#[test]
fn integration_adds() {
    assert_eq!(rust_ws::add(1, 1), 2);
}

#[test]
fn integration_rejects_an_empty_order() {
    let lines = rust_ws::add(0, 0);
    assert!(lines > 0, "an order needs lines\nsecond line of the message");
}

#[test]
#[ignore = "needs a database"]
fn integration_saves_to_a_database() {}
