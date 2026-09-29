//! The material of the broker's image embedded in this binary (ADR-t827-1
//! decision 6, docs/design/broker.md "配布と版"): `build.rs` writes it as a
//! tar ([`crate::broker_material`]), and [`EmbeddedSource`] unpacks it into
//! the image's build context, so the image never reads a checkout's working
//! files. [`image`] is the image of this build: its tag is the build
//! identifier's, with the material's hash for a `.dirty` build.

use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::application::broker::{BrokerFailure, BrokerResult, FailureCode, ImageSource};
use crate::broker_material;

/// The material `build.rs` collected.
pub static MATERIAL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/broker-image.tar"));
/// The Rust of the image's build stage: the channel of `rust-toolchain.toml`
/// for a build from a checkout, else the package's `rust-version`.
pub const RUST_VERSION: &str = env!("DAGQ_BROKER_RUST_VERSION");
/// `source` (the server's sources of a checkout) or `release` (the
/// Containerfile alone, which installs the server from crates.io).
pub const KIND: &str = env!("DAGQ_BROKER_MATERIAL");

/// The SHA-256 of `bytes`, in lower-case hex.
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The hash of what the image is built from: the SHA-256 of [`MATERIAL`]
/// followed by [`RUST_VERSION`] (a build-arg of the image, so another Rust
/// is another image).
pub fn material_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| material_digest(MATERIAL, RUST_VERSION))
}

/// The hash of `material` built with `rust_version`.
pub fn material_digest(material: &[u8], rust_version: &str) -> String {
    let mut bytes = material.to_vec();
    bytes.extend_from_slice(b"\nrust ");
    bytes.extend_from_slice(rust_version.as_bytes());
    digest(&bytes)
}

/// The image of this build, `localhost/dagq-broker:<tag>`.
pub fn image() -> String {
    crate::application::broker::image_name(crate::VERSION, material_hash())
}

/// The image's build context from embedded material.
#[derive(Debug, Clone)]
pub struct EmbeddedSource {
    pub material: Cow<'static, [u8]>,
    pub rust_version: String,
}

impl EmbeddedSource {
    /// The material this binary embeds.
    pub fn of_this_build() -> Self {
        Self {
            material: Cow::Borrowed(MATERIAL),
            rust_version: RUST_VERSION.to_owned(),
        }
    }
}

impl ImageSource for EmbeddedSource {
    fn stage(&self, dir: &Path) -> BrokerResult<String> {
        unpack(&self.material, dir).map_err(|error| {
            BrokerFailure::new(
                FailureCode::ImageSourceMissing,
                format!(
                    "unpack the broker's embedded image material into {}: {error}",
                    dir.display()
                ),
            )
        })?;
        Ok(self.rust_version.clone())
    }
}

/// Put the files of `material` in `dir`, emptied first.
pub fn unpack(material: &[u8], dir: &Path) -> Result<(), String> {
    let files = broker_material::untar(material)?;
    let io = |what: &Path, error: std::io::Error| format!("{}: {error}", what.display());
    if dir.exists() {
        fs::remove_dir_all(dir).map_err(|error| io(dir, error))?;
    }
    fs::create_dir_all(dir).map_err(|error| io(dir, error))?;
    for (name, content) in files {
        let path = dir.join(&name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        }
        fs::write(&path, content).map_err(|error| io(&path, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker_material::{CONTAINERFILE, Kind, collect, tar};

    /// The files under `dir`, as relative paths.
    fn files(dir: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let name = path.strip_prefix(root).unwrap().to_string_lossy();
                    out.push((name.into_owned(), fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    /// A copy of the checkout's material sources in `root`.
    fn copy_checkout(root: &Path) {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
        let material = collect(checkout, "0.4.0-dev").unwrap();
        for (name, content) in &material.files {
            let from = match name.as_str() {
                "Containerfile" => CONTAINERFILE.to_owned(),
                "Cargo.toml" => continue,
                other => other.to_owned(),
            };
            let path = root.join(from);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    #[test]
    fn the_embedded_material_is_this_checkout_s_and_unpacks_without_it() {
        // This test binary was built from a checkout: the material is the
        // server's sources.
        assert_eq!(KIND, Kind::Source.as_str());
        assert!(RUST_VERSION.starts_with("1."), "{RUST_VERSION}");
        let embedded = crate::broker_material::untar(MATERIAL).unwrap();
        let names: Vec<&str> = embedded.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"Containerfile"), "{names:?}");
        assert!(
            names.contains(&"crates/dagq-broker/src/main.rs"),
            "{names:?}"
        );
        assert_eq!(material_hash(), material_digest(MATERIAL, RUST_VERSION));
        assert_ne!(
            material_digest(MATERIAL, "1.0.0"),
            material_digest(MATERIAL, "1.0.1")
        );
        assert_ne!(material_digest(b"a", "1"), material_digest(b"b", "1"));
        assert_eq!(material_hash().len(), 64);

        // A binary built from a checkout whose files then change or go
        // unpacks what it was built with, and only that.
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        copy_checkout(&checkout);
        let at_build = collect(&checkout, "0.4.0-dev").unwrap();
        let built = tar(&at_build.files).unwrap();
        fs::write(checkout.join("crates/dagq-broker/src/main.rs"), "changed").unwrap();
        fs::remove_dir_all(checkout.join("crates/dagq-broker-protocol")).unwrap();
        fs::remove_file(checkout.join(CONTAINERFILE)).unwrap();
        let source = EmbeddedSource {
            material: Cow::Owned(built),
            rust_version: "1.2.3".to_owned(),
        };
        let context = dir.path().join("context");
        fs::create_dir_all(context.join("stale")).unwrap();
        assert_eq!(source.stage(&context).unwrap(), "1.2.3");
        assert_eq!(files(&context), at_build.files);
        assert!(!context.join("stale").exists());

        // So does this binary's own.
        let own = dir.path().join("own");
        EmbeddedSource::of_this_build().stage(&own).unwrap();
        assert_eq!(files(&own), embedded);

        // Material that cannot be read is a structured failure.
        let broken = EmbeddedSource {
            material: Cow::Borrowed(b"not a tar"),
            rust_version: "1".to_owned(),
        };
        assert_eq!(
            broken.stage(&context).unwrap_err().code,
            FailureCode::ImageSourceMissing
        );
    }

    #[test]
    fn the_image_of_this_build_carries_its_tag() {
        let tag = crate::application::broker::image_tag(crate::VERSION, material_hash());
        assert_eq!(image(), format!("localhost/dagq-broker:{tag}"));
        if crate::VERSION.ends_with(".dirty") {
            assert!(tag.ends_with(&material_hash()[..12]), "{tag}");
        } else {
            assert!(!tag.contains(&material_hash()[..12]), "{tag}");
        }
    }
}
