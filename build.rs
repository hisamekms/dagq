//! Embeds the build identifier (ADR-0045 decision 2) as `DAGQ_BUILD_ID`,
//! which `dagq::VERSION` and `dagq --version` report, by the rule of
//! `dagq_broker_protocol::build_id` that the broker's binaries share, and
//! lists the queue's migrations for `src/infrastructure/schema.rs`
//! (ADR-0067 decision 1), and embeds the material of the broker's image
//! (ADR-t827-1 decision 6, `src/broker_material.rs`).

use std::path::{Path, PathBuf};

// Only the listing is used here; `integrate` uses the rest.
#[allow(dead_code)]
#[path = "src/migration_numbers.rs"]
mod migration_numbers;

// Only collecting and writing the tar are used here; the binary unpacks it.
#[allow(dead_code)]
#[path = "src/broker_material.rs"]
mod broker_material;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=src/migration_numbers.rs");
    // A migration added, removed or renamed changes the list, whatever the
    // version (a directory is watched with everything under it). The build
    // identifier watches it too for a development version.
    println!("cargo:rerun-if-changed={}", migration_numbers::DIRECTORY);
    list_migrations(&manifest_dir);
    // The rule the broker's server and client embed too (ADR-t827-1).
    dagq_broker_protocol::build_id::emit(".");
    embed_broker_material(&manifest_dir);
}

/// Write `$OUT_DIR/broker-image.tar`, the material of the broker's image
/// that `src/infrastructure/broker_image.rs` embeds, and set
/// `DAGQ_BROKER_RUST_VERSION` (the build stage's Rust, the channel of
/// `rust-toolchain.toml`) and `DAGQ_BROKER_MATERIAL` (`source` or
/// `release`).
fn embed_broker_material(manifest_dir: &Path) {
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let material = broker_material::collect(manifest_dir, &version)
        .unwrap_or_else(|error| panic!("the broker's image material: {error}"));
    let toolchain = manifest_dir.join("rust-toolchain.toml");
    println!("cargo:rerun-if-changed={}", toolchain.display());
    for path in &material.watched {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let rust = std::fs::read_to_string(&toolchain)
        .ok()
        .and_then(|text| broker_material::rust_version(&text))
        .unwrap_or_else(|| {
            std::env::var("CARGO_PKG_RUST_VERSION").expect("cargo sets CARGO_PKG_RUST_VERSION")
        });
    println!("cargo:rustc-env=DAGQ_BROKER_RUST_VERSION={rust}");
    println!(
        "cargo:rustc-env=DAGQ_BROKER_MATERIAL={}",
        material.kind.as_str()
    );
    let bytes = broker_material::tar(&material.files)
        .unwrap_or_else(|error| panic!("the broker's image material: {error}"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join(broker_material::FILE), bytes).expect("write broker-image.tar");
}

/// Write `$OUT_DIR/migrations.rs`, the array of the migrations in order of
/// their number, each through `include_str!`, which
/// `src/infrastructure/schema.rs` includes as `MIGRATIONS`. Numbers that do
/// not run from 0001 without a gap or a repeat fail the build, naming the
/// files.
fn list_migrations(manifest_dir: &Path) {
    let directory = manifest_dir.join(migration_numbers::DIRECTORY);
    let entries = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    let names: Vec<String> = entries
        .map(|entry| {
            let entry = entry
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
            entry.file_name().to_string_lossy().into_owned()
        })
        .collect();
    let ordered = match migration_numbers::ordered(&names) {
        Ok(ordered) => ordered,
        Err(problems) => {
            for problem in problems.lines() {
                println!("cargo:warning={problem}");
            }
            panic!(
                "the migrations under {} are not numbered from 0001 without a gap or a repeat:\n{problems}",
                directory.display()
            );
        }
    };
    let mut source = String::from("&[\n");
    for name in ordered {
        let path = directory.join(name);
        source.push_str(&format!(
            "    include_str!({:?}),\n",
            path.display().to_string()
        ));
    }
    source.push_str("]\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join("migrations.rs"), source).expect("write migrations.rs");
}
