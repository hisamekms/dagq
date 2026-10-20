//! The image the container runtime runs a worker, a resume and a
//! verification in (ADR-t2257-1 decision 2): a base the runtime builds with
//! only what the runtime needs (Claude Code, the Linux dagq client of the
//! installed dagq's version, a non-root user), and on top of it the image
//! of the project's own Containerfile that `dagq.toml` names
//! ([`ProjectImage`]), or the base alone without one.
//!
//! What is decided here: where the client comes from ([`client_source`]),
//! the tags from the inputs ([`Images::of`]), what to build and which old
//! images to remove ([`steps`]), and how long a failed build keeps the
//! container unusable ([`failure_holds`]).

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::podman_machine::QueueKey;
use super::{EventKind, RecordedKind};

/// The Claude Code the base installs. Pinned so the base's tag is decided
/// from the inputs without a network, and the same base is rebuilt the
/// same; a newer Claude Code comes with a newer dagq.
pub const CLAUDE_CODE_VERSION: &str = "2.1.296";

/// The repository of the base images in the dagq machine; one tag per
/// base ([`BaseInputs::reference`]). The bases are the host's, shared by
/// every queue on it: a queue that builds a new base removes the others,
/// and podman keeps any a container or a project image still uses.
pub const BASE_REPOSITORY: &str = "localhost/dagq-base";

/// The build argument a project's Containerfile takes the base by, to put
/// after its `FROM` (`ARG DAGQ_BASE_IMAGE` then `FROM ${DAGQ_BASE_IMAGE}`).
pub const BASE_ARG: &str = "DAGQ_BASE_IMAGE";

/// The project image `[container.image]` of `dagq.toml` names: its
/// `containerfile` (required) is the repository path of a Containerfile
/// that takes the base as [`BASE_ARG`] and puts it after its `FROM`, and
/// `inputs` (optional, an array of repository paths, none by default) the
/// files that Containerfile copies, which make up its build context with
/// it. Without the table the container runs the base alone. Both are read
/// from the main checkout, never from a run's clone, so a worker's change
/// before it lands does not change the image; a change of any of them
/// builds a new image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectImage {
    pub containerfile: String,
    pub inputs: Vec<String>,
}

/// Why `path` cannot be a repository path of [`ProjectImage`]: it must be
/// relative, inside the repository (no `..`), and name a file.
pub fn repository_path_problem(path: &str) -> Option<String> {
    if path.trim().is_empty() {
        return Some("a repository path cannot be empty".to_owned());
    }
    if path.starts_with('/') || path.contains('\\') {
        return Some(format!(
            "{path:?} is not a relative repository path with / between its parts"
        ));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Some(format!(
            "{path:?} must name a file inside the repository without ., .. or empty parts"
        ));
    }
    None
}

/// Where the base gets the Linux dagq client of the installed dagq's
/// version: built from source inside the dagq machine, in a stage the base
/// does not keep, so no checkout of dagq on the host is needed and no
/// language toolchain is left in the base. A commit not pushed to dagq's
/// repository cannot be fetched, and its base fails to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientSource {
    /// A release, from its crates.io source.
    Release { version: String },
    /// A development build, from the commit it was built from in dagq's
    /// repository.
    Commit { repository: String, commit: String },
}

impl ClientSource {
    /// The build arguments of the base that name the client.
    pub fn build_args(&self) -> Vec<(String, String)> {
        match self {
            Self::Release { version } => vec![("DAGQ_CLIENT_VERSION".into(), version.clone())],
            Self::Commit { repository, commit } => vec![
                ("DAGQ_CLIENT_GIT".into(), repository.clone()),
                ("DAGQ_CLIENT_REV".into(), commit.clone()),
            ],
        }
    }
}

/// Where the client of the installed dagq, whose build identifier is
/// `build`, comes from, with dagq's `repository`; `Err` says why it cannot
/// be had: a build with uncommitted changes or one that does not know its
/// commit has no source of the same version anywhere.
pub fn client_source(build: &str, repository: &str) -> Result<ClientSource, String> {
    if !crate::build_id::is_prerelease(build) {
        return Ok(ClientSource::Release {
            version: build.to_owned(),
        });
    }
    let unobtainable = |why: &str| {
        Err(format!(
            "the Linux dagq client of the installed dagq {build} cannot be had: {why}"
        ))
    };
    if build.ends_with(".dirty") {
        return unobtainable("it was built with uncommitted changes, which no source has");
    }
    match crate::build_id::named_commit(build) {
        Some(commit) if commit.chars().all(|c| c.is_ascii_hexdigit()) => Ok(ClientSource::Commit {
            repository: repository.to_owned(),
            commit: commit.to_owned(),
        }),
        Some(_) => unobtainable("its commit is not a Git commit ID"),
        None => unobtainable("it does not know the commit it was built from"),
    }
}

/// What the base is decided by: the runtime's build identifier, the
/// Claude Code it installs and the runtime's base Containerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseInputs {
    pub build: String,
    pub claude_code: String,
    pub containerfile: String,
    pub client: ClientSource,
}

impl BaseInputs {
    /// The base's image reference, its tag from every input.
    pub fn reference(&self) -> String {
        let tag = digest(&[
            b"dagq base",
            self.build.as_bytes(),
            self.claude_code.as_bytes(),
            self.containerfile.as_bytes(),
        ]);
        format!("{BASE_REPOSITORY}:{tag}")
    }
}

/// A file of a build context: its path in the context and its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: Vec<u8>,
}

/// One image to build: its reference, the context's files (the
/// Containerfile among them, at `containerfile`) and the build arguments.
/// Nothing else goes in: no secret, no host path, no environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBuild {
    pub reference: String,
    pub containerfile: String,
    pub files: Vec<ContextFile>,
    pub args: Vec<(String, String)>,
}

/// The images of one queue: the base, the project's image on top of it
/// when `dagq.toml` names one, and the repository the queue's project
/// images are kept in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Images {
    pub base: ImageBuild,
    pub project: Option<ImageBuild>,
    pub project_repository: String,
}

/// The repository of the project images of `queue`: its name carries the
/// queue, so one queue's cleanup never removes another's project image.
pub fn project_repository(queue: &QueueKey) -> String {
    format!(
        "localhost/dagq-{}-image",
        queue.as_str().to_ascii_lowercase()
    )
}

impl Images {
    /// The images of `queue` from the base's inputs and, when `dagq.toml`
    /// names a project image, its files as read from the main checkout:
    /// the Containerfile's content and each input's. The project image's
    /// tag is decided by the base's reference, the paths and every byte
    /// of the files, so a change of any of them is a new image.
    pub fn of(
        base: &BaseInputs,
        project: Option<(&ProjectImage, Vec<ContextFile>)>,
        queue: &QueueKey,
    ) -> Self {
        let base_reference = base.reference();
        let mut base_args = vec![("CLAUDE_CODE_VERSION".to_owned(), base.claude_code.clone())];
        base_args.extend(base.client.build_args());
        // What the client built must report as its version.
        base_args.push(("DAGQ_CLIENT_BUILD".to_owned(), base.build.clone()));
        let base_build = ImageBuild {
            reference: base_reference.clone(),
            containerfile: "Containerfile".to_owned(),
            files: vec![ContextFile {
                path: "Containerfile".to_owned(),
                content: base.containerfile.clone().into_bytes(),
            }],
            args: base_args,
        };
        let project_repository = project_repository(queue);
        let project = project.map(|(image, files)| {
            let mut parts: Vec<&[u8]> = vec![b"dagq project", base_reference.as_bytes()];
            for file in &files {
                parts.push(file.path.as_bytes());
                parts.push(&file.content);
            }
            ImageBuild {
                reference: format!("{project_repository}:{}", digest(&parts)),
                containerfile: image.containerfile.clone(),
                files,
                args: vec![(BASE_ARG.to_owned(), base_reference.clone())],
            }
        });
        Self {
            base: base_build,
            project,
            project_repository,
        }
    }

    /// The image a container runs: the project's, or the base without one.
    pub fn run(&self) -> &str {
        self.project
            .as_ref()
            .map_or(self.base.reference.as_str(), |project| {
                project.reference.as_str()
            })
    }
}

/// The paths of the files of a project image's context, in the order they
/// are hashed: the Containerfile, then each input once in the order given.
pub fn context_paths(image: &ProjectImage) -> Vec<&str> {
    let mut paths = vec![image.containerfile.as_str()];
    for input in &image.inputs {
        if !paths.contains(&input.as_str()) {
            paths.push(input);
        }
    }
    paths
}

/// What to do so the images of [`Images`] are there, given the references
/// the dagq machine has: build the base and the project image that are
/// missing (a project image is built on a base built now when the base is
/// missing), and remove the old ones: every other tag of the base's
/// repository, and every other tag of this queue's project repository
/// (all of them when no project image is named).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Steps {
    pub build_base: bool,
    pub build_project: bool,
    pub remove: Vec<String>,
}

pub fn steps(images: &Images, listed: &[String]) -> Steps {
    let has = |reference: &str| listed.iter().any(|each| each == reference);
    let build_base = !has(&images.base.reference);
    let build_project = images
        .project
        .as_ref()
        .is_some_and(|project| build_base || !has(&project.reference));
    let keep = |reference: &str| {
        reference == images.base.reference
            || images
                .project
                .as_ref()
                .is_some_and(|project| project.reference == reference)
    };
    let mut remove: Vec<String> = listed
        .iter()
        .filter(|reference| {
            let repository = reference
                .rsplit_once(':')
                .map_or(reference.as_str(), |r| r.0);
            (repository == BASE_REPOSITORY || repository == images.project_repository)
                && !keep(reference)
        })
        .cloned()
        .collect();
    remove.sort();
    remove.dedup();
    Steps {
        build_base,
        build_project,
        remove,
    }
}

/// Recorded on the queue when an image was built and is the one a
/// container runs.
pub const CONTAINER_IMAGE_BUILT: &str = EventKind::ContainerImageBuilt.as_str();
/// Recorded on the queue when the image could not be had: the client
/// cannot be had, a file `dagq.toml` names is missing, or a build failed.
pub const CONTAINER_IMAGE_FAILED: &str = EventKind::ContainerImageFailed.as_str();
/// The two kinds, for reading the latest of them.
pub const CONTAINER_IMAGE_KINDS: [&str; 2] = [CONTAINER_IMAGE_BUILT, CONTAINER_IMAGE_FAILED];

/// How long a failed build of an image keeps the container unusable, in
/// seconds: long enough that a broken Containerfile is not rebuilt on every
/// claim, short enough that a passing network failure heals by itself.
/// A change of the inputs is a new image and is tried at once.
pub const FAILURE_HOLD_SECS: i64 = 15 * 60;

/// The payload of [`CONTAINER_IMAGE_FAILED`]: the image that failed
/// (`null` when it could not be decided), why, and when (Unix seconds).
pub fn failure_payload(image: Option<&str>, reason: &str, at: i64) -> Value {
    json!({"image": image, "reason": reason, "at": at})
}

/// The payload of [`CONTAINER_IMAGE_BUILT`]: the image a container runs
/// and what was built for it.
pub fn built_payload(image: &str, built: &[&str], removed: &[String]) -> Value {
    json!({"image": image, "built": built, "removed": removed})
}

/// Why `image` cannot be used at `now` (Unix seconds) from the latest
/// image event of the queue (its kind and payload): a failure of the same
/// image within [`FAILURE_HOLD_SECS`].
pub fn failure_holds(
    last: Option<(&RecordedKind, &Value)>,
    image: &str,
    now: i64,
) -> Option<String> {
    let (kind, payload) = last?;
    if *kind != EventKind::ContainerImageFailed || payload["image"].as_str() != Some(image) {
        return None;
    }
    let at = payload["at"].as_i64()?;
    (now - at < FAILURE_HOLD_SECS).then(|| {
        format!(
            "the container image {image} failed to build {}s ago (tried again {FAILURE_HOLD_SECS}s after): {}",
            now - at,
            payload["reason"].as_str().unwrap_or("no reason recorded")
        )
    })
}

/// The hex of the first 8 bytes of the SHA-256 of `parts`, each prefixed
/// with its length so no two lists of parts hash alike.
fn digest(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BaseInputs {
        BaseInputs {
            build: "0.4.0-dev+abc123".into(),
            claude_code: "2.1.0".into(),
            containerfile: "FROM debian\n".into(),
            client: ClientSource::Commit {
                repository: "https://example.com/dagq".into(),
                commit: "abc123".into(),
            },
        }
    }

    fn queue() -> QueueKey {
        QueueKey::new("aaaa").unwrap()
    }

    fn file(path: &str, content: &str) -> ContextFile {
        ContextFile {
            path: path.into(),
            content: content.as_bytes().to_vec(),
        }
    }

    fn project() -> ProjectImage {
        ProjectImage {
            containerfile: ".dagq/container/Containerfile".into(),
            inputs: vec!["rust-toolchain.toml".into()],
        }
    }

    fn with_files(containerfile: &str, toolchain: &str) -> Images {
        let image = project();
        Images::of(
            &base(),
            Some((
                &image,
                vec![
                    file(".dagq/container/Containerfile", containerfile),
                    file("rust-toolchain.toml", toolchain),
                ],
            )),
            &queue(),
        )
    }

    #[test]
    fn the_client_is_the_installed_version_s_or_cannot_be_had() {
        let repository = "https://github.com/hisamekms/dagq";
        assert_eq!(
            client_source("0.4.0", repository),
            Ok(ClientSource::Release {
                version: "0.4.0".into()
            })
        );
        assert_eq!(
            client_source("0.4.0-dev+8b55cdb8", repository),
            Ok(ClientSource::Commit {
                repository: repository.into(),
                commit: "8b55cdb8".into()
            })
        );
        for (build, why) in [
            ("0.4.0-dev+8b55cdb8.dirty", "uncommitted changes"),
            ("0.4.0-dev+unknown", "does not know the commit"),
            ("0.4.0-dev", "does not know the commit"),
            ("0.4.0-dev+not;hex", "not a Git commit"),
        ] {
            let error = client_source(build, repository).unwrap_err();
            assert!(
                error.contains("cannot be had") && error.contains(why),
                "{build}: {error}"
            );
        }
        assert_eq!(
            ClientSource::Release {
                version: "1.0.0".into()
            }
            .build_args(),
            [("DAGQ_CLIENT_VERSION".to_owned(), "1.0.0".to_owned())]
        );
    }

    #[test]
    fn the_base_tag_changes_with_the_build_the_claude_code_and_its_containerfile() {
        let tag = base().reference();
        assert!(tag.starts_with("localhost/dagq-base:"), "{tag}");
        assert_eq!(tag, base().reference());
        for changed in [
            BaseInputs {
                build: "0.4.0-dev+def456".into(),
                ..base()
            },
            BaseInputs {
                claude_code: "2.1.1".into(),
                ..base()
            },
            BaseInputs {
                containerfile: "FROM ubuntu\n".into(),
                ..base()
            },
        ] {
            assert_ne!(changed.reference(), tag);
        }
    }

    #[test]
    fn the_project_tag_changes_with_the_base_its_containerfile_and_each_input() {
        let images = with_files("A", "1.98");
        let tag = images.run().to_owned();
        assert!(tag.starts_with("localhost/dagq-aaaa-image:"), "{tag}");
        assert_eq!(with_files("A", "1.98").run(), tag);
        assert_ne!(with_files("B", "1.98").run(), tag);
        assert_ne!(with_files("A", "1.99").run(), tag);
        // The same bytes split between the files otherwise are another
        // context.
        assert_ne!(with_files("A1", ".98").run(), tag);
        let image = project();
        let other_base = Images::of(
            &BaseInputs {
                claude_code: "9".into(),
                ..base()
            },
            Some((
                &image,
                vec![
                    file(".dagq/container/Containerfile", "A"),
                    file("rust-toolchain.toml", "1.98"),
                ],
            )),
            &queue(),
        );
        assert_ne!(other_base.run(), tag);
    }

    #[test]
    fn a_project_image_takes_the_base_as_its_build_argument_and_none_runs_the_base() {
        let images = with_files("A", "1.98");
        let project = images.project.as_ref().unwrap();
        assert_eq!(
            project.args,
            [(BASE_ARG.to_owned(), images.base.reference.clone())]
        );
        assert_eq!(project.containerfile, ".dagq/container/Containerfile");
        assert_eq!(project.files.len(), 2);
        assert_eq!(
            images.base.args,
            [
                ("CLAUDE_CODE_VERSION".to_owned(), "2.1.0".to_owned()),
                (
                    "DAGQ_CLIENT_GIT".to_owned(),
                    "https://example.com/dagq".to_owned()
                ),
                ("DAGQ_CLIENT_REV".to_owned(), "abc123".to_owned()),
                (
                    "DAGQ_CLIENT_BUILD".to_owned(),
                    "0.4.0-dev+abc123".to_owned()
                ),
            ]
        );

        let alone = Images::of(&base(), None, &queue());
        assert_eq!(alone.project, None);
        assert_eq!(alone.run(), base().reference());
    }

    #[test]
    fn missing_images_are_built_and_old_ones_of_the_base_and_this_queue_removed() {
        let images = with_files("A", "1.98");
        let base_ref = images.base.reference.clone();
        let project_ref = images.run().to_owned();
        // Nothing there: both are built, the project on the new base.
        assert_eq!(
            steps(&images, &[]),
            Steps {
                build_base: true,
                build_project: true,
                remove: vec![]
            }
        );
        // A base built before: the project image only.
        let s = steps(&images, std::slice::from_ref(&base_ref));
        assert!(!s.build_base && s.build_project);
        // A missing base rebuilds the project image on it, even when it
        // is listed.
        let s = steps(&images, std::slice::from_ref(&project_ref));
        assert!(s.build_base && s.build_project);
        // Both there: nothing to build; the old tags of the base and of
        // this queue go, another queue's and a person's images stay.
        let listed = vec![
            base_ref.clone(),
            project_ref.clone(),
            "localhost/dagq-base:0000".to_owned(),
            "localhost/dagq-aaaa-image:1111".to_owned(),
            "localhost/dagq-bbbb-image:2222".to_owned(),
            "docker.io/library/debian:bookworm".to_owned(),
        ];
        assert_eq!(
            steps(&images, &listed),
            Steps {
                build_base: false,
                build_project: false,
                remove: vec![
                    "localhost/dagq-aaaa-image:1111".to_owned(),
                    "localhost/dagq-base:0000".to_owned(),
                ]
            }
        );
        // No project image named any more: every one of this queue goes.
        let alone = Images::of(&base(), None, &queue());
        let mut expected = vec![
            "localhost/dagq-aaaa-image:1111".to_owned(),
            "localhost/dagq-base:0000".to_owned(),
            project_ref,
        ];
        expected.sort();
        assert_eq!(steps(&alone, &listed).remove, expected);
    }

    #[test]
    fn a_failure_of_the_same_image_holds_until_its_time_is_up() {
        let failed = failure_payload(Some("img:1"), "boom", 1_000);
        let holds = |kind: EventKind, image: &str, now: i64| {
            failure_holds(Some((&kind.into(), &failed)), image, now).is_some()
        };
        assert!(holds(EventKind::ContainerImageFailed, "img:1", 1_000));
        assert!(holds(
            EventKind::ContainerImageFailed,
            "img:1",
            1_000 + FAILURE_HOLD_SECS - 1
        ));
        assert!(!holds(
            EventKind::ContainerImageFailed,
            "img:1",
            1_000 + FAILURE_HOLD_SECS
        ));
        // Another image (the inputs changed) is tried at once.
        assert!(!holds(EventKind::ContainerImageFailed, "img:2", 1_000));
        assert!(!holds(EventKind::ContainerImageBuilt, "img:1", 1_000));
        assert_eq!(failure_holds(None, "img:1", 1_000), None);
        let why = failure_holds(
            Some((&EventKind::ContainerImageFailed.into(), &failed)),
            "img:1",
            1_010,
        )
        .unwrap();
        assert!(why.contains("boom") && why.contains("10s ago"), "{why}");
    }

    #[test]
    fn repository_paths_stay_inside_the_repository() {
        assert_eq!(
            repository_path_problem(".dagq/container/Containerfile"),
            None
        );
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(repository_path_problem(bad).is_some(), "{bad:?}");
        }
        let image = ProjectImage {
            containerfile: "C".into(),
            inputs: vec!["a".into(), "C".into(), "a".into(), "b".into()],
        };
        assert_eq!(context_paths(&image), ["C", "a", "b"]);
    }
}
