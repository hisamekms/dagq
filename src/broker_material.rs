//! The material of the broker's image that dagq embeds (ADR-t827-1
//! decision 6, docs/design/broker.md "配布と版"): `build.rs` collects it
//! ([`collect`]) and writes it as a tar ([`tar`]) that the binary embeds
//! with `include_bytes!`, and `infrastructure::broker_image` unpacks it
//! ([`untar`]) into the image's build context. Shared by `build.rs`
//! (through `#[path]`) and the library, so it uses `std` only.
//!
//! - A build from a checkout (`crates/dagq-broker` is there, [`is_source`]):
//!   the Containerfile, the sources of dagq-broker-protocol and
//!   dagq-broker, the workspace manifest narrowed to those two, and
//!   `Cargo.lock` ([`Kind::Source`]).
//! - A release (the crates.io package of dagq, which has no `crates/`): the
//!   Containerfile alone, its build stage `cargo install --locked
//!   dagq-broker@<version>` ([`Kind::Release`], [`release_containerfile`]).

use std::path::{Path, PathBuf};

/// The Containerfile in a dagq checkout and in dagq's package.
pub const CONTAINERFILE: &str = "containers/broker/Containerfile";
/// The crates the image builds from a checkout.
pub const CRATES: [&str; 2] = ["crates/dagq-broker-protocol", "crates/dagq-broker"];
/// The file under `OUT_DIR` that `build.rs` writes and the binary embeds.
pub const FILE: &str = "broker-image.tar";
/// The line that opens the Containerfile's build from the sources; a
/// release replaces it through [`BUILD_END`] with `cargo install`.
pub const BUILD_BEGIN: &str = "# dagq:build-from-source begin";
/// The line that closes it.
pub const BUILD_END: &str = "# dagq:build-from-source end";
/// The cache mounts of the build stage's compile (task 1451): cargo's
/// registry and target directory, kept by the machine between builds and
/// not images, so the start's prune of images keeps them. The Containerfile
/// of a checkout and of a release both use them, so a build whose
/// `Cargo.lock` and Rust are the same compiles no dependency again. Cargo
/// never removes stale outputs, so each new dependency, Rust or release
/// version leaves the old ones in the target cache, which grows on the
/// machine's disk with no bound and no cleanup. A lost cache only makes
/// cargo compile again.
pub const CACHE_MOUNTS: [&str; 2] = [
    "--mount=type=cache,id=dagq-broker-cargo-registry,target=/usr/local/cargo/registry,sharing=locked",
    "--mount=type=cache,id=dagq-broker-cargo-target,target=/src/target,sharing=locked",
];

/// What the material builds the server from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The sources of a checkout.
    Source,
    /// `cargo install --locked dagq-broker@<version>` from crates.io.
    Release,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Release => "release",
        }
    }
}

/// The material: its files (relative paths with `/`, in order) and the
/// paths under the root it was read from, for `rerun-if-changed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Material {
    pub kind: Kind,
    pub files: Vec<(String, Vec<u8>)>,
    pub watched: Vec<PathBuf>,
}

/// Whether `root` has the broker's crates, as a checkout does.
pub fn is_source(root: &Path) -> bool {
    CRATES
        .iter()
        .all(|name| root.join(name).join("Cargo.toml").is_file())
}

/// The material under `root` (the manifest dir of dagq) for dagq at
/// `version`.
pub fn collect(root: &Path, version: &str) -> Result<Material, String> {
    let containerfile = read(&root.join(CONTAINERFILE))?;
    let mut watched = vec![root.join(CONTAINERFILE)];
    if !is_source(root) {
        // Missing, so the build script runs again: `cargo package` verifies
        // its copy (without crates/) with the checkout's target directory,
        // and the checkout's next build must not keep this material.
        watched.push(root.join(CRATES[1]).join("Cargo.toml"));
        let text = String::from_utf8(containerfile)
            .map_err(|_| format!("{CONTAINERFILE} is not UTF-8"))?;
        return Ok(Material {
            kind: Kind::Release,
            files: vec![(
                "Containerfile".to_owned(),
                release_containerfile(&text, version)?.into_bytes(),
            )],
            watched,
        });
    }
    let mut files = vec![
        ("Containerfile".to_owned(), containerfile),
        ("Cargo.toml".to_owned(), narrowed_manifest().into_bytes()),
        ("Cargo.lock".to_owned(), read(&root.join("Cargo.lock"))?),
    ];
    watched.push(root.join("Cargo.lock"));
    for name in CRATES {
        let dir = root.join(name);
        watched.push(dir.clone());
        files.push((format!("{name}/Cargo.toml"), read(&dir.join("Cargo.toml"))?));
        // The server embeds its build identifier through its build script.
        if dir.join("build.rs").is_file() {
            files.push((format!("{name}/build.rs"), read(&dir.join("build.rs"))?));
        }
        tree(&dir.join("src"), &format!("{name}/src"), &mut files)?;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Material {
        kind: Kind::Source,
        files,
        watched,
    })
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

/// The regular files under `dir`, as `prefix/<relative path>`.
fn tree(dir: &Path, prefix: &str, files: &mut Vec<(String, Vec<u8>)>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", dir.display()))?;
        let kind = entry
            .file_type()
            .map_err(|error| format!("read {}: {error}", entry.path().display()))?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        // Hidden files (`.DS_Store`, editor state) are not the sources.
        if file_name.starts_with('.') {
            continue;
        }
        let name = format!("{prefix}/{file_name}");
        if kind.is_dir() {
            tree(&entry.path(), &name, files)?;
        } else if kind.is_file() {
            files.push((name, read(&entry.path())?));
        }
    }
    Ok(())
}

/// The workspace manifest of the image's build context: the two crates the
/// server needs and nothing else.
pub fn narrowed_manifest() -> String {
    let members = CRATES
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# Written by dagq for the broker's image: the workspace narrowed to the server.\n\
         [workspace]\nmembers = [{members}]\nresolver = \"3\"\n"
    )
}

/// The Containerfile of a release: the lines from [`BUILD_BEGIN`] to
/// [`BUILD_END`] (the build from the sources) become one `cargo install
/// --locked dagq-broker@<version>`, with the [`CACHE_MOUNTS`] and its
/// target directory in the cached one.
pub fn release_containerfile(text: &str, version: &str) -> Result<String, String> {
    let (before, rest) = text
        .split_once(&format!("{BUILD_BEGIN}\n"))
        .ok_or_else(|| format!("{CONTAINERFILE} has no line `{BUILD_BEGIN}`"))?;
    let (_, after) = rest
        .split_once(&format!("{BUILD_END}\n"))
        .ok_or_else(|| format!("{CONTAINERFILE} has no line `{BUILD_END}`"))?;
    let [registry, target] = CACHE_MOUNTS;
    Ok(format!(
        "{before}# A release: the server of dagq's own version from crates.io.\n\
         RUN {registry} \\\n    {target} \\\n    \
         cargo install --locked dagq-broker@{version} --root /usr/local/dagq-broker \
         --target-dir /src/target \\\n \
         && install -m 0755 /usr/local/dagq-broker/bin/dagq-broker /dagq-broker\n{after}"
    ))
}

/// The `channel` of a `rust-toolchain.toml`.
pub fn rust_version(toolchain: &str) -> Option<String> {
    toolchain
        .lines()
        .filter_map(|line| line.trim().strip_prefix("channel"))
        .filter_map(|rest| rest.trim().strip_prefix('='))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .find(|value| !value.is_empty())
}

const BLOCK: usize = 512;

/// A ustar archive of `files` (regular files only, mode 0644, owner 0 and
/// time 0), so the same files make the same bytes.
pub fn tar(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (path, content) in files {
        let mut header = [0u8; BLOCK];
        let (prefix, name) = split_path(path)?;
        header[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut header[100..108], 0o644);
        octal(&mut header[108..116], 0);
        octal(&mut header[116..124], 0);
        octal(&mut header[124..136], content.len() as u64);
        octal(&mut header[136..148], 0);
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        header[148..156].fill(b' ');
        let sum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        octal(&mut header[148..155], sum);
        header[155] = b' ';
        out.extend_from_slice(&header);
        out.extend_from_slice(content);
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

/// A path as ustar's prefix (up to 155 bytes) and name (up to 100).
fn split_path(path: &str) -> Result<(&str, &str), String> {
    if path.len() <= 100 {
        return Ok(("", path));
    }
    path.char_indices()
        .filter(|(_, c)| *c == '/')
        .map(|(at, _)| (&path[..at], &path[at + 1..]))
        .find(|(prefix, name)| prefix.len() <= 155 && name.len() <= 100 && !name.is_empty())
        .ok_or_else(|| format!("{path} is too long for the image's material"))
}

/// Zero-padded octal digits and a NUL in `field`.
fn octal(field: &mut [u8], value: u64) {
    let last = field.len() - 1;
    let digits = format!("{value:0last$o}");
    field[..last].copy_from_slice(digits.as_bytes());
    field[last] = 0;
}

/// The regular files of a ustar archive that [`tar`] wrote, as relative
/// paths; a path that is absolute or has `..` is refused.
pub fn untar(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut at = 0;
    while at + BLOCK <= bytes.len() {
        let header = &bytes[at..at + BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            return Ok(files);
        }
        let text = |range: std::ops::Range<usize>| {
            let field = &header[range];
            let end = field
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(field.len());
            String::from_utf8_lossy(&field[..end]).into_owned()
        };
        let size = u64::from_str_radix(text(124..136).trim(), 8)
            .map_err(|_| format!("the material's entry at {at} has no size"))?;
        let size = usize::try_from(size).map_err(|_| "the material is too large".to_owned())?;
        let prefix = text(345..500);
        let name = text(0..100);
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.starts_with('/') || path.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err(format!("the material names the path {path:?}"));
        }
        let start = at + BLOCK;
        let content = bytes
            .get(start..start + size)
            .ok_or_else(|| format!("the material ends inside {path}"))?;
        if header[156] == b'0' || header[156] == 0 {
            files.push((path, content.to_vec()));
        }
        at = start + size.div_ceil(BLOCK) * BLOCK;
    }
    Err("the material has no end".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn a_tar_reads_back_as_it_was_written_and_tar_reads_it_too() {
        let long = format!("{}/{}.rs", "d".repeat(80), "f".repeat(60));
        let files = vec![
            ("Containerfile".to_owned(), b"FROM x\n".to_vec()),
            ("crates/a/src/lib.rs".to_owned(), vec![7u8; 1000]),
            ("empty".to_owned(), Vec::new()),
            (long.clone(), b"long".to_vec()),
        ];
        let bytes = tar(&files).unwrap();
        assert_eq!(bytes.len() % BLOCK, 0);
        assert_eq!(untar(&bytes).unwrap(), files);
        assert_eq!(
            tar(&files).unwrap(),
            bytes,
            "the same files, the same bytes"
        );
        // The system's tar reads it as ustar.
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("m.tar");
        std::fs::write(&archive, &bytes).unwrap();
        let listed = std::process::Command::new("tar")
            .arg("-tf")
            .arg(&archive)
            .output()
            .unwrap();
        assert!(listed.status.success(), "{listed:?}");
        let listed = String::from_utf8(listed.stdout).unwrap();
        let names: Vec<&str> = listed.lines().collect();
        assert_eq!(
            names,
            [
                "Containerfile",
                "crates/a/src/lib.rs",
                "empty",
                long.as_str()
            ]
        );
        assert!(tar(&[("x".repeat(300), Vec::new())]).is_err());
    }

    #[test]
    fn untar_refuses_paths_out_of_the_context_and_broken_archives() {
        for path in ["/etc/passwd", "../up", "a/../../b", "a//b"] {
            let bytes = tar(&[(path.to_owned(), b"x".to_vec())]).unwrap();
            assert!(untar(&bytes).is_err(), "{path}");
        }
        let bytes = tar(&[("a".to_owned(), vec![1u8; 600])]).unwrap();
        assert!(untar(&bytes[..700]).unwrap_err().contains("ends inside"));
        let bytes = tar(&[("a".to_owned(), vec![1u8; 10])]).unwrap();
        assert!(untar(&bytes[..BLOCK * 2]).unwrap_err().contains("no end"));
        assert_eq!(untar(&[0u8; BLOCK]).unwrap(), Vec::new());
    }

    #[test]
    fn a_checkout_embeds_the_server_s_sources_and_the_narrowed_workspace() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let material = collect(root, "0.4.0-dev").unwrap();
        assert_eq!(material.kind, Kind::Source);
        let names: Vec<&str> = material.files.iter().map(|(n, _)| n.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        for name in [
            "Containerfile",
            "Cargo.toml",
            "Cargo.lock",
            "crates/dagq-broker/Cargo.toml",
            "crates/dagq-broker/build.rs",
            "crates/dagq-broker/src/main.rs",
            "crates/dagq-broker-protocol/Cargo.toml",
            "crates/dagq-broker-protocol/src/build_id.rs",
        ] {
            assert!(names.contains(&name), "{name} in {names:?}");
        }
        assert!(
            names.iter().all(|name| !name.contains("/tests/")
                && !name.starts_with("src/")
                && !name.starts_with("crates/dagq-broker-client")),
            "{names:?}"
        );
        let manifest = &material
            .files
            .iter()
            .find(|(n, _)| n == "Cargo.toml")
            .unwrap()
            .1;
        assert_eq!(manifest, narrowed_manifest().as_bytes());
        let containerfile = &material
            .files
            .iter()
            .find(|(n, _)| n == "Containerfile")
            .unwrap()
            .1;
        assert_eq!(
            containerfile,
            &std::fs::read(root.join(CONTAINERFILE)).unwrap()
        );
        build_stage_reuses_the_compiled_dependencies(std::str::from_utf8(containerfile).unwrap());
        assert!(material.watched.contains(&root.join(CONTAINERFILE)));
        assert!(material.watched.contains(&root.join("crates/dagq-broker")));
    }

    /// The checkout's Containerfile compiles in the [`CACHE_MOUNTS`], so
    /// a build whose build identifier alone changed compiles no dependency
    /// again (task 1451): the identifier's ARG comes after the layers that do
    /// not use it, and the context's crates are touched so they compile
    /// again from these sources.
    fn build_stage_reuses_the_compiled_dependencies(text: &str) {
        let lines: Vec<&str> = text.lines().collect();
        let at = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} in {text}"))
        };
        let begin = at(BUILD_BEGIN);
        let end = at(BUILD_END);
        let run = at("RUN --mount=type=cache");
        assert!(begin < run && run < end, "{text}");
        let block = lines[run..end].join("\n");
        for mount in CACHE_MOUNTS {
            assert!(block.contains(mount), "{mount} in {block}");
        }
        assert!(
            block.contains("find crates -type f -exec touch {} +"),
            "{block}"
        );
        assert!(
            block.contains("cargo build --release -p dagq-broker"),
            "{block}"
        );
        // The cached target is where cargo builds: /src is the WORKDIR.
        assert!(lines[begin..run].contains(&"WORKDIR /src"), "{text}");
        assert!(CACHE_MOUNTS[1].contains("target=/src/target"));
        // The build identifier's ARG: after `apk add`, before the build, and
        // outside the markers, so a release declares it too.
        let arg = at("ARG DAGQ_BROKER_IMAGE_BUILD=");
        assert!(
            at("RUN apk add --no-cache musl-dev") < arg && arg < begin,
            "{text}"
        );
    }

    #[test]
    fn a_release_package_embeds_the_containerfile_alone_that_installs_the_server() {
        // dagq's crates.io package: the Containerfile, no crates/.
        let dir = tempfile::tempdir().unwrap();
        let checkout =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(CONTAINERFILE))
                .unwrap();
        write(dir.path(), CONTAINERFILE, &checkout);
        write(dir.path(), "Cargo.lock", "");
        let material = collect(dir.path(), "0.5.0").unwrap();
        assert_eq!(material.kind, Kind::Release);
        assert_eq!(material.files.len(), 1);
        assert!(
            material
                .watched
                .contains(&dir.path().join("crates/dagq-broker/Cargo.toml"))
        );
        let (name, content) = &material.files[0];
        assert_eq!(name, "Containerfile");
        let text = String::from_utf8(content.clone()).unwrap();
        assert!(
            text.contains("    cargo install --locked dagq-broker@0.5.0 "),
            "{text}"
        );
        assert!(!text.contains("COPY . ."), "{text}");
        assert!(!text.contains("cargo build"), "{text}");
        assert!(!text.contains(BUILD_BEGIN) && !text.contains(BUILD_END));
        // The same caches as a checkout's build: cargo install compiles in
        // the cached target, so a release of the same dependencies and Rust
        // compiles none of them again.
        for mount in CACHE_MOUNTS {
            assert!(
                text.contains(&format!("RUN {mount}")) || text.contains(&format!("    {mount}")),
                "{text}"
            );
        }
        assert!(
            text.contains("--root /usr/local/dagq-broker --target-dir /src/target \\\n"),
            "{text}"
        );
        assert!(text.contains("ARG DAGQ_BROKER_IMAGE_BUILD="), "{text}");
        // The run stage stays as it is.
        let run_stage = checkout.split_once(BUILD_END).unwrap().1;
        assert!(text.ends_with(run_stage.trim_start_matches('\n')), "{text}");
        assert!(text.contains("FROM docker.io/library/alpine:3"));
        // Without the markers, or without a Containerfile, it fails.
        assert!(release_containerfile("FROM x\n", "1").is_err());
        assert!(release_containerfile(&format!("{BUILD_BEGIN}\nx\n"), "1").is_err());
        assert!(collect(&dir.path().join("none"), "1").is_err());
        // One of the crates alone is not a checkout.
        write(dir.path(), "crates/dagq-broker/Cargo.toml", "");
        assert!(!is_source(dir.path()));
    }

    #[test]
    fn rust_version_reads_the_channel() {
        assert_eq!(
            rust_version("[toolchain]\n# channel = \"old\"\nchannel = \"1.2.3\"\n").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(rust_version("[toolchain]\n"), None);
    }
}
