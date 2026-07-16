//! Loud skip for the converter stress suite.
//!
//! `tests/stress.rs` is gated on all the converter features, so under a
//! bare `cargo test -p kaiv` (default features only) it compiles to zero
//! tests — silently, indistinguishable from success. This sentinel is
//! compiled in exactly that case and fails with a directive, so a
//! feature-less run cannot masquerade as a passing stress run. A
//! workspace-root `cargo test` (where kaiv-cli unifies the converter
//! features) does not compile it and runs the real suite.

#[cfg(not(all(
    feature = "yaml",
    feature = "toml",
    feature = "xml",
    feature = "cbor",
    feature = "avro",
    feature = "proto",
    feature = "asn1"
)))]
#[test]
fn stress_suite_skipped_without_converter_features() {
    panic!(
        "stress suite compiled to zero tests: build with `cargo test --all-features` \
         or run at the workspace root where kaiv-cli unifies the converter features"
    );
}
