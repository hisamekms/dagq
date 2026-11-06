//! Embeds the build identifier (ADR-0045 decision 2) as `DAGQ_BUILD_ID`,
//! which `dagq::VERSION` and `dagq --version` report, by the rule of
//! `src/build_id.rs`, lists the queue's migrations for
//! `src/infrastructure/schema.rs` (ADR-0067 decision 1), and lists the Postgres
//! series for `src/infrastructure/postgres/migrate.rs`, refusing a series that
//! does not match them (ADR-t3986-1 decision 7).

use std::path::{Path, PathBuf};

// Only the listing is used here; `integrate` uses the rest.
#[allow(dead_code)]
#[path = "src/migration_numbers.rs"]
mod migration_numbers;

// Only emitting is used here; the runtime reads the commit an identifier
// names.
#[allow(dead_code)]
#[path = "src/build_id.rs"]
mod build_id;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=src/migration_numbers.rs");
    // A migration added, removed or renamed changes the list, whatever the
    // version (a directory is watched with everything under it). The build
    // identifier watches it too for a development version.
    println!("cargo:rerun-if-changed={}", migration_numbers::DIRECTORY);
    println!(
        "cargo:rerun-if-changed={}",
        migration_numbers::POSTGRES_DIRECTORY
    );
    let ordered = list_migrations(&manifest_dir);
    list_postgres_series(&manifest_dir, &ordered);
    build_id::emit();
}

/// Write `$OUT_DIR/migrations.rs`, the array of the migrations in order of
/// their number, each through `include_str!`, which
/// `src/infrastructure/schema.rs` includes as `MIGRATIONS`, and return their
/// file names. Numbers that do not run from 0001 without a gap or a repeat
/// fail the build, naming the files.
fn list_migrations(manifest_dir: &Path) -> Vec<String> {
    let directory = manifest_dir.join(migration_numbers::DIRECTORY);
    let ordered = migration_numbers::ordered(&file_names(&directory)).unwrap_or_else(|problems| {
        refuse(
            &directory,
            "are not numbered from 0001 without a gap or a repeat",
            &problems,
        )
    });
    let mut source = String::from("&[\n");
    for name in &ordered {
        source.push_str(&format!("    {},\n", included(&directory.join(name))));
    }
    source.push_str("]\n");
    write_out("migrations.rs", &source);
    ordered
}

/// Write `$OUT_DIR/postgres_migrations.rs`, the `PostgresSeries` that
/// `src/infrastructure/postgres/migrate.rs` includes: the base's version and
/// text, and the file of each SQLite migration after it, in their order. A
/// series that does not match `sqlite` (the migrations' file names), or a file
/// of it that spells a migration's number, fails the build, naming the files.
fn list_postgres_series(manifest_dir: &Path, sqlite: &[String]) {
    let directory = manifest_dir.join(migration_numbers::POSTGRES_DIRECTORY);
    const DOES_NOT_MATCH: &str = "do not match the SQLite migrations";
    let base = directory.join(migration_numbers::POSTGRES_BASE);
    let version = std::fs::read_to_string(&base)
        .map_err(|error| format!("cannot read {}: {error}", base.display()))
        .and_then(|text| migration_numbers::postgres_base_version(&text))
        .unwrap_or_else(|problem| refuse(&directory, DOES_NOT_MATCH, &problem));
    let series = migration_numbers::postgres_series(sqlite, version, &file_names(&directory))
        .unwrap_or_else(|problems| refuse(&directory, DOES_NOT_MATCH, &problems));
    let mut problems = Vec::new();
    let own_numbers = sqlite[version..]
        .iter()
        .map(|migration| Some(&migration[..4]));
    let files = std::iter::once((migration_numbers::POSTGRES_BASE, None))
        .chain(series.iter().map(String::as_str).zip(own_numbers));
    for (name, own) in files {
        let path = directory.join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        if let Some(number) = migration_numbers::spelled_number(&text, own) {
            problems.push(format!(
                "{}/{name} spells the number {number}; name a migration by its name, \
                 since landing may renumber it",
                migration_numbers::POSTGRES_DIRECTORY
            ));
        }
    }
    if !problems.is_empty() {
        refuse(&directory, DOES_NOT_MATCH, &problems.join("\n"));
    }
    let mut source = format!(
        "PostgresSeries {{\n    base_version: {version},\n    base: {},\n    after: &[\n",
        included(&base)
    );
    for name in &series {
        source.push_str(&format!("        {},\n", included(&directory.join(name))));
    }
    source.push_str("    ],\n}\n");
    write_out("postgres_migrations.rs", &source);
}

/// The names of the entries of `directory`.
fn file_names(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .map(|entry| {
            let entry = entry
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
            entry.file_name().to_string_lossy().into_owned()
        })
        .collect()
}

/// Fail the build for the migrations under `directory`, naming `problems`.
fn refuse(directory: &Path, what: &str, problems: &str) -> ! {
    for problem in problems.lines() {
        println!("cargo:warning={problem}");
    }
    panic!(
        "the migrations under {} {what}:\n{problems}",
        directory.display()
    );
}

/// The `include_str!` of the file at `path`.
fn included(path: &Path) -> String {
    format!("include_str!({:?})", path.display().to_string())
}

/// Write `source` to `name` under `$OUT_DIR`.
fn write_out(name: &str, source: &str) {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join(name), source).unwrap_or_else(|error| panic!("write {name}: {error}"));
}
