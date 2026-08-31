//! Offline eval catalog. Live cases stay skipped.

use std::path::PathBuf;

use reve::eval::{self, Options};

fn catalog() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evals")
}

#[tokio::test]
async fn offline_eval_suite_is_green() {
    let report = eval::run(Options {
        catalog: catalog(),
        live: false,
        microvm: false,
        ..Default::default()
    })
    .await
    .expect("eval catalog loads");
    assert!(
        report.passed > 0,
        "expected offline cases, got {:?}",
        report.cases.iter().map(|c| &c.id).collect::<Vec<_>>()
    );
    assert_eq!(report.failed, 0, "{:?}", report);
    assert_eq!(report.errored, 0, "{:?}", report);
    assert!(report.skipped >= 1, "live cases should skip without --live");
}

#[tokio::test]
async fn list_includes_live_cases() {
    let cases = eval::list(&Options {
        catalog: catalog(),
        ..Default::default()
    })
    .unwrap();
    assert!(
        cases
            .iter()
            .any(|c| c.id.contains("chief-does-not-ask") || c.id.starts_with("live."))
    );
    assert!(
        cases
            .iter()
            .any(|c| c.id == "house.init-chief-of-staff" || c.id.ends_with("init-chief-of-staff"))
    );
}
