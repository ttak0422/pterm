// Compile the vendored parser regressions in the root workspace's test suite.
use vt100::{Color, Parser, Screen};

mod reflow_tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/vendor/vt100/src/reflow_tests.rs"
    ));
}
