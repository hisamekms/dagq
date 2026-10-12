//! The container image on the host (ADR-t2257-1 decision 2): the
//! runtime's base Containerfile ([`BASE_CONTAINERFILE`], kept in
//! `container_image/base.Containerfile` beside this file and built into
//! the binary), the images of a queue as its main checkout decides them
//! ([`CheckoutImages`]), the podman commands on the images inside the dagq
//! machine ([`PodmanImages`]) and the queue's records of them
//! ([`QueueImageRecords`]).
//!
//! A build's context is a fresh directory with the image's files alone,
//! which the podman client sends to the machine: nothing else of the host
//! (its paths, the checkout, a secret) reaches the build.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::podman_machine::PodmanCli;
use super::run_env::load_project_image;
use super::sqlite::SqliteQueue;
use crate::application::EventStore;
use crate::application::container_image::{ImageBuilder, ImageRecords, ImageSpecs};
use crate::domain::container_image::{
    BaseInputs, CLAUDE_CODE_VERSION, CONTAINER_IMAGE_KINDS, ContextFile, ImageBuild, Images,
    client_source, context_paths,
};
use crate::domain::podman_machine::QueueKey;
use crate::domain::{ActorContext, EventKind};

/// The runtime's base Containerfile: Claude Code, the Linux dagq client
/// and a non-root user, nothing of a language.
pub const BASE_CONTAINERFILE: &str = include_str!("container_image/base.Containerfile");

/// The images of the queue whose directory is `queue_dir`, decided from
/// the runtime's build identifier `build` and the `dagq.toml` of
/// `checkout`, the main checkout, with the files it names read from there.
#[derive(Debug, Clone)]
pub struct CheckoutImages {
    pub checkout: PathBuf,
    pub queue_dir: PathBuf,
    pub build: String,
    /// Where the client's source is: dagq's repository.
    pub repository: String,
}

impl CheckoutImages {
    /// The images of the running dagq for the queue at `queue_dir`.
    pub fn new(checkout: &Path, queue_dir: &Path) -> Self {
        Self {
            checkout: checkout.to_path_buf(),
            queue_dir: queue_dir.to_path_buf(),
            build: crate::VERSION.to_owned(),
            repository: env!("CARGO_PKG_REPOSITORY").to_owned(),
        }
    }
}

impl ImageSpecs for CheckoutImages {
    fn images(&self) -> Result<Images, String> {
        let client = client_source(&self.build, &self.repository)?;
        let queue = self
            .queue_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{} names no queue", self.queue_dir.display()))
            .and_then(QueueKey::new)?;
        let project = load_project_image(&self.checkout).map_err(|error| {
            format!("read [container.image] of the main checkout's dagq.toml: {error:#}")
        })?;
        let files = match &project {
            None => None,
            Some(image) => Some(
                context_paths(image)
                    .into_iter()
                    .map(|path| {
                        read_inside(&self.checkout, path)
                            .map(|content| ContextFile {
                                path: path.to_owned(),
                                content,
                            })
                            .map_err(|error| {
                                format!(
                                    "dagq.toml's [container.image] names {path}, which the main checkout {} cannot give: {error:#}",
                                    self.checkout.display()
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, String>>()?,
            ),
        };
        let base = BaseInputs {
            build: self.build.clone(),
            claude_code: CLAUDE_CODE_VERSION.to_owned(),
            containerfile: BASE_CONTAINERFILE.to_owned(),
            client,
        };
        Ok(Images::of(&base, project.as_ref().zip(files), &queue))
    }
}

/// The content of the file at the repository path `path` of `checkout`,
/// refused when it resolves outside the checkout (through a symbolic
/// link, say), so nothing else of the host reaches a build's context.
fn read_inside(checkout: &Path, path: &str) -> Result<Vec<u8>> {
    let root = checkout
        .canonicalize()
        .with_context(|| format!("resolve {}", checkout.display()))?;
    let file = checkout
        .join(path)
        .canonicalize()
        .with_context(|| format!("resolve {path}"))?;
    anyhow::ensure!(
        file.starts_with(&root),
        "{path} resolves to {}, outside the checkout",
        file.display()
    );
    fs::read(&file).with_context(|| format!("read {path}"))
}

/// The images inside the dagq machine, through `podman --connection dagq`.
#[derive(Debug, Clone, Default)]
pub struct PodmanImages {
    pub podman: PodmanCli,
}

#[derive(Deserialize)]
struct ListedImage {
    #[serde(rename = "Names", default)]
    names: Option<Vec<String>>,
}

/// The references of `podman images --format json`.
pub fn parse_image_list(text: &str) -> Result<Vec<String>> {
    let listed: Option<Vec<ListedImage>> =
        serde_json::from_str(text).context("read podman images")?;
    Ok(listed
        .unwrap_or_default()
        .into_iter()
        .flat_map(|image| image.names.unwrap_or_default())
        .collect())
}

/// `podman build` of `image` with `context` as its directory.
pub fn build_args(image: &ImageBuild, context: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "build".into(),
        "--tag".into(),
        image.reference.clone().into(),
        "--file".into(),
        context.join(&image.containerfile).into(),
    ];
    for (name, value) in &image.args {
        args.push("--build-arg".into());
        args.push(format!("{name}={value}").into());
    }
    args.push(context.into());
    args
}

/// A directory under the temporary directory, removed when dropped.
struct BuildContext {
    dir: PathBuf,
}

impl BuildContext {
    fn new(image: &ImageBuild) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("dagq-image-{}", uuid::Uuid::new_v4()));
        let context = Self { dir };
        for file in &image.files {
            let path = context.dir.join(&file.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::write(&path, &file.content).with_context(|| format!("write {}", path.display()))?;
        }
        Ok(context)
    }
}

impl Drop for BuildContext {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl ImageBuilder for PodmanImages {
    fn images(&self) -> Result<Vec<String>> {
        parse_image_list(&self.podman.on_machine(&[
            "images".into(),
            "--format".into(),
            "json".into(),
        ])?)
    }

    fn build(&self, image: &ImageBuild) -> Result<()> {
        let context = BuildContext::new(image)?;
        self.podman
            .on_machine(&build_args(image, &context.dir))
            .map(drop)
    }

    fn remove(&self, reference: &str) -> Result<()> {
        self.podman
            .on_machine(&["rmi".into(), reference.into()])
            .map(drop)
    }
}

/// The queue at `db`'s records of the image, written as `actor`.
#[derive(Debug, Clone)]
pub struct QueueImageRecords {
    pub db: PathBuf,
    pub actor: Option<ActorContext>,
}

impl QueueImageRecords {
    fn open(&self) -> Result<SqliteQueue> {
        let queue = SqliteQueue::open(&self.db)?;
        Ok(match &self.actor {
            Some(actor) => queue.with_actor(actor.clone()),
            None => queue,
        })
    }
}

impl ImageRecords for QueueImageRecords {
    fn record(&self, kind: EventKind, payload: Value) -> Result<()> {
        EventStore::record_queue_event(&self.open()?, kind, payload).map(drop)
    }

    fn latest(&self) -> Result<Option<(String, Value)>> {
        Ok(
            EventStore::latest_queue_event(&self.open()?, &CONTAINER_IMAGE_KINDS)?
                .map(|event| (event.kind, event.payload)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_has_claude_code_the_client_and_a_user_and_no_language_or_secret() {
        let base = BASE_CONTAINERFILE;
        assert!(base.contains("@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}"));
        assert!(base.contains("COPY --from=client /client/bin/dagq /usr/local/bin/dagq"));
        assert!(base.contains("useradd --uid 10001"));
        assert!(base.trim_end().lines().any(|line| line == "USER dagq"));
        // The Rust that builds the client stays in its stage: the image's
        // stage starts after it and installs no toolchain.
        let (_, image) = base.rsplit_once("\nFROM ").unwrap();
        for tool in ["cargo", "rustup", "rustc", "sccache", "nextest"] {
            assert!(!image.contains(tool), "the base keeps {tool}");
        }
        for secret in ["TOKEN", "API_KEY", "SECRET", "/Users/", "$HOME"] {
            assert!(!base.contains(secret), "the base names {secret}");
        }
    }

    #[test]
    fn images_are_listed_by_their_names_and_built_from_the_context_alone() {
        let listed = parse_image_list(
            r#"[{"Id":"1","Names":["localhost/dagq-base:ab"]},{"Id":"2","Names":null},{"Id":"3"}]"#,
        )
        .unwrap();
        assert_eq!(listed, ["localhost/dagq-base:ab"]);
        assert_eq!(parse_image_list("null").unwrap(), Vec::<String>::new());

        let image = ImageBuild {
            reference: "localhost/dagq-q-image:cd".into(),
            containerfile: "c/Containerfile".into(),
            files: vec![],
            args: vec![("DAGQ_BASE_IMAGE".into(), "localhost/dagq-base:ab".into())],
        };
        let args = build_args(&image, Path::new("/tmp/ctx"));
        assert_eq!(
            args,
            [
                "build",
                "--tag",
                "localhost/dagq-q-image:cd",
                "--file",
                "/tmp/ctx/c/Containerfile",
                "--build-arg",
                "DAGQ_BASE_IMAGE=localhost/dagq-base:ab",
                "/tmp/ctx",
            ]
            .map(OsString::from)
        );
    }
}
