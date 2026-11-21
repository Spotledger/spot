//! The shared conformance suite, run for a native reference controller and the TypeScript
//! equivalent: both must pass, and the table prints any behavioural difference.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{NativeFactory, ts_factory, ts_reference};
use spot_ts_controllers::{ConformanceFixture, run_conformance};

#[tokio::test]
async fn a_typescript_controller_passes_the_same_suite_as_a_native_one() {
    let fixture = ConformanceFixture::note();

    let native = run_conformance(
        &spot_controllers::ControllerRef::native("note"),
        &NativeFactory,
        &fixture,
    )
    .await;
    let typescript = run_conformance(&ts_reference(), &ts_factory(), &fixture).await;

    println!("--- native ---\n{}", native.table());
    println!("--- typescript ---\n{}", typescript.table());

    assert!(
        native.passed(),
        "the native reference controller failed:\n{}",
        native.table()
    );
    assert!(
        typescript.passed(),
        "the TypeScript controller failed:\n{}",
        typescript.table()
    );
    let differences = native.differences(&typescript);
    assert!(
        differences.is_empty(),
        "the controllers differ:\n{}",
        differences.join("\n")
    );
}
