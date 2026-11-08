//! Shared store code reaches the database through named transaction entries.
//! Only SQLite connection setup, schema, entry implementations and FTS adapters
//! may name the queue connection or the write entry's configuration.
use super::source;

const FIELDS: [&str; 3] = ["conn", "write_hold_warn", "write_identity"];
// Each row is a SQLite adapter operation, never a whole store module.
#[rustfmt::skip]
const ADAPTERS: &[(&str, &str)] = &[
    ("src/infrastructure/related.rs", "SqliteQueue::related"),
    ("src/infrastructure/related.rs", "SqliteQueue::related_docs"),
    ("src/infrastructure/related.rs", "SqliteQueue::related_landed_commits"),
    ("src/infrastructure/related.rs", "SqliteQueue::title_matches"),
    ("src/infrastructure/search.rs", "SqliteQueue::missing_commit_messages"),
    ("src/infrastructure/search.rs", "SqliteQueue::search"),
    ("src/infrastructure/sqlite.rs", "QueueConnection::deref"),
    ("src/infrastructure/sqlite.rs", "QueueConnection::deref_mut"),
    ("src/infrastructure/sqlite.rs", "QueueConnection::drop"),
    ("src/infrastructure/sqlite.rs", "SqliteEntries::read_entry"),
    ("src/infrastructure/sqlite.rs", "SqliteEntries::write_entry"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::apply"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::initialize"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::is_read_only"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::migration_tx"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::read_tx"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::refuse_breaking_in_use"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::schema_version"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::set_busy_timeout"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::state"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::with_event_bodies"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::with_write_hold_warn"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::with_write_identity"),
    ("src/infrastructure/sqlite.rs", "SqliteQueue::write_tx"),
    ("src/infrastructure/sqlite.rs", "WriteConnection::lend"),
    ("src/infrastructure/sqlite.rs", "WriteTx::drop"),
    ("src/infrastructure/write_leader.rs", "LeaderRow::release"),
    ("src/infrastructure/write_leader.rs", "LeaderRow::renew"),
    ("src/infrastructure/write_leader.rs", "LeaderRow::take"),
];

fn check(rust: &[(String, String)], allowed: &[(&str, &str)]) -> Vec<String> {
    let (read, errors) = source::production_files(rust);
    let mut errors: Vec<_> = errors.into_iter().collect();
    let mut used = std::collections::BTreeSet::new();
    for (path, file) in read {
        let store = super::ownership::STORES
            .iter()
            .any(|store| super::covers(store.path, path))
            || [
                "SqliteQueue",
                "StoreQueue",
                "TransactionEntries",
                "SqlRead",
                "SqlWrite",
            ]
            .iter()
            .any(|name| !source::name_uses(&file, name).is_empty());
        // Other infrastructure has unrelated connections (e.g. rustls StreamOwned).
        if !path.starts_with("src/infrastructure/") || !store {
            continue;
        }
        for field in FIELDS {
            for function in source::field_uses(&file, field) {
                if allowed.contains(&(path, function.as_str())) {
                    used.insert((path.to_owned(), function));
                } else {
                    errors.push(format!(
                        "{path}: {function} names .{field}; use named transaction entries"
                    ));
                }
            }
        }
    }
    for (path, function) in allowed {
        if !used.contains(&(path.to_string(), function.to_string())) {
            errors.push(format!("stale adapter exception: {path} {function}"));
        }
    }
    errors
}

#[test]
fn shared_stores_use_only_named_entries() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let errors = check(&super::files(root, "src/infrastructure", "rs"), ADAPTERS);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn a_fixture_naming_connection_or_write_configuration_fails() {
    for field in FIELDS {
        for text in [
            format!("impl Store {{ fn read(&self) {{ let _ = &self.{field}; }} }}"),
            format!("fn read(queue: &Store) {{ visit!(queue . {field}); }}"),
        ] {
            let errors = check(&[("src/infrastructure/sessions.rs".into(), text)], &[]);
            assert_eq!(errors.len(), 1, "{errors:?}");
        }
        let ignored = format!("fn read() {{ #[cfg(test)] {{ let _ = queue.{field}; }} }}");
        assert!(check(&[("src/infrastructure/sessions.rs".into(), ignored)], &[]).is_empty());
    }
    let fixture = "impl Store { fn read(&self) { let _ = self.conn; } }";
    let files = [("src/infrastructure/sessions.rs".into(), fixture.into())];
    assert!(check(&files, &[("src/infrastructure/sessions.rs", "Store::read")]).is_empty());
    assert!(!check(&files, &[("src/infrastructure/sqlite.rs", "Store::read")]).is_empty());
}
