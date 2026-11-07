//! Event writers use the execution port's SqlWrite boundary.
use super::source;

fn check(rust: &[(String, String)]) -> Vec<String> {
    let (files, mut errors) = source::production_files(rust);
    for (path, file) in files {
        if !source::name_uses(&file, "EventTx").is_empty() {
            errors.insert(format!(
                "{path}: EventTx is retired; use sql_exec::SqlWrite"
            ));
        }
    }
    errors.into_iter().collect()
}

#[test]
fn production_code_does_not_name_event_tx() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let errors = check(&super::files(root, "src", "rs"));
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn a_fixture_naming_event_tx_fails() {
    let errors = check(&[(
        "src/store.rs".into(),
        "fn write(tx: &impl EventTx) {}".into(),
    )]);
    assert_eq!(
        errors,
        ["src/store.rs: EventTx is retired; use sql_exec::SqlWrite"]
    );
    assert!(
        check(&[(
            "src/store.rs".into(),
            "#[cfg(test)] mod tests { fn write(tx: &impl EventTx) {} }".into()
        )])
        .is_empty()
    );
}
