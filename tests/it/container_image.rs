//! The container image through a fake `podman` (ADR-t2257-1 decision 2):
//! the base and the project image built on it from the main checkout, the
//! old images removed, and an unobtainable client or a failed build
//! answered as an unusable container and recorded on the queue. The real
//! `podman` is in the ignored test at the end.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dagq::application::ContainerRuntime;
use dagq::application::container_image::{ImageGate, ImageRecords, ImageSpecs, prepare};
use dagq::domain::container_image::{
    CONTAINER_IMAGE_BUILT, CONTAINER_IMAGE_FAILED, FAILURE_HOLD_SECS,
};
use dagq::domain::container_runtime::{Availability, RuntimeTarget};
use dagq::infrastructure::container_image::{CheckoutImages, PodmanImages, QueueImageRecords};
use dagq::infrastructure::podman_machine::PodmanCli;
use dagq::infrastructure::sqlite::SqliteQueue;

use crate::common::{self, shell_path};

/// A `podman` whose machine holds the images listed in `state/images`,
/// one reference a line. It writes each command line to `state/calls`,
/// copies each build's context to `state/context-<n>`, and fails a build
/// while `state/fail-build` is there.
fn fake_podman(state: &Path) -> PathBuf {
    fs::create_dir_all(state).unwrap();
    fs::write(state.join("images"), "").unwrap();
    let s = shell_path(state);
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
[ "$1 $2" = "--connection dagq" ] || exit 125
shift 2
case "$1" in
images)
  printf '['; sep=""
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    printf '%s{{"Names":["%s"]}}' "$sep" "$name"; sep=","
  done < "$S/images"
  printf ']' ;;
build)
  [ -f "$S/fail-build" ] && {{ echo "step 3/5 failed" >&2; exit 1; }}
  n=$(ls -d "$S"/context-* 2>/dev/null | wc -l | tr -d ' ')
  eval "context=\${{$#}}"
  cp -R "$context" "$S/context-$n"
  echo "$3" >> "$S/images" ;;
rmi)
  [ "$2" = "$(cat "$S/in-use" 2>/dev/null)" ] && {{ echo "image is in use" >&2; exit 2; }}
  grep -vxF "$2" "$S/images" > "$S/images.next"; mv "$S/images.next" "$S/images" ;;
*) exit 125 ;;
esac
"#
    );
    let program = state.join("podman");
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

fn calls(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A queue, the main checkout and a run's clone of the same repository,
/// and the fake `podman`.
struct Host {
    _dir: tempfile::TempDir,
    db: PathBuf,
    checkout: PathBuf,
    clone: PathBuf,
    state: PathBuf,
    builder: PodmanImages,
}

const BUILD: &str = "0.4.0-dev+8b55cdb84e13";
const DAGQ_TOML: &str = "[container.image]\ncontainerfile = \".dagq/container/Containerfile\"\ninputs = [\"rust-toolchain.toml\"]\n";

impl Host {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir
            .path()
            .join("data")
            .join("dagq")
            .join("aaaaaaaaaaaaaaaa");
        fs::create_dir_all(&queue_dir).unwrap();
        let db = queue_dir.join("queue.db");
        SqliteQueue::init(&db).unwrap();
        let checkout = dir.path().join("main");
        let clone = dir.path().join("clone");
        for (root, version) in [(&checkout, "1.98.1"), (&clone, "9.99.9")] {
            fs::create_dir_all(root.join(".dagq/container")).unwrap();
            fs::write(root.join("dagq.toml"), DAGQ_TOML).unwrap();
            fs::write(
                root.join(".dagq/container/Containerfile"),
                format!("ARG DAGQ_BASE_IMAGE\nFROM ${{DAGQ_BASE_IMAGE}}\n# {version}\n"),
            )
            .unwrap();
            fs::write(
                root.join("rust-toolchain.toml"),
                format!("[toolchain]\nchannel = \"{version}\"\n"),
            )
            .unwrap();
        }
        let state = dir.path().join("podman");
        let builder = PodmanImages {
            podman: PodmanCli {
                program: fake_podman(&state),
            },
        };
        Self {
            _dir: dir,
            db,
            checkout,
            clone,
            state,
            builder,
        }
    }

    fn specs(&self, build: &str) -> CheckoutImages {
        CheckoutImages {
            build: build.to_owned(),
            repository: "https://github.com/hisamekms/dagq".to_owned(),
            ..CheckoutImages::new(&self.checkout, self.db.parent().unwrap())
        }
    }

    fn records(&self) -> QueueImageRecords {
        QueueImageRecords {
            db: self.db.clone(),
            actor: None,
        }
    }

    fn gate(&self, build: &str, now: i64) -> ImageGate<Usable> {
        ImageGate {
            specs: Arc::new(self.specs(build)),
            records: Arc::new(self.records()),
            now: Arc::new(move || now),
            container: Usable,
        }
    }

    fn images(&self) -> Vec<String> {
        fs::read_to_string(self.state.join("images"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// The parts behind the image, all usable.
struct Usable;

impl ContainerRuntime for Usable {
    fn availability(&self, _: RuntimeTarget) -> Availability {
        Availability::Available
    }
}

#[test]
fn the_project_image_is_built_on_the_base_from_the_main_checkout_and_old_ones_go() {
    let _waiting = common::test();
    let host = Host::new();
    fs::write(
        host.state.join("images"),
        "localhost/dagq-base:0000000000000000\nlocalhost/dagq-aaaaaaaaaaaaaaaa-image:1111111111111111\nlocalhost/dagq-bbbbbbbbbbbbbbbb-image:2222222222222222\n",
    )
    .unwrap();
    // A container of the old base still runs.
    fs::write(
        host.state.join("in-use"),
        "localhost/dagq-base:0000000000000000",
    )
    .unwrap();

    let images = host.specs(BUILD).images().unwrap();
    let run = prepare(&host.specs(BUILD), &host.builder, &host.records(), 100).unwrap();
    assert_eq!(run, images.run());
    assert!(
        run.starts_with("localhost/dagq-aaaaaaaaaaaaaaaa-image:"),
        "{run}"
    );

    let calls = calls(&host.state);
    let builds: Vec<&String> = calls.iter().filter(|c| c.contains(" build ")).collect();
    assert_eq!(builds.len(), 2, "{calls:?}");
    // The base takes the installed dagq's commit for its client and the
    // pinned Claude Code, and nothing else.
    let base = builds[0];
    assert!(
        base.contains(&format!("--tag {}", images.base.reference)),
        "{base}"
    );
    assert!(
        base.contains("--build-arg DAGQ_CLIENT_REV=8b55cdb84e13"),
        "{base}"
    );
    assert!(
        base.contains("--build-arg DAGQ_CLIENT_GIT=https://github.com/hisamekms/dagq"),
        "{base}"
    );
    assert!(base.contains("--build-arg CLAUDE_CODE_VERSION="), "{base}");
    assert!(
        base.contains(&format!("--build-arg DAGQ_CLIENT_BUILD={BUILD}")),
        "{base}"
    );
    assert_eq!(base.matches("--build-arg").count(), 4, "{base}");
    let base_context = host.state.join("context-0");
    assert_eq!(
        fs::read_to_string(base_context.join("Containerfile")).unwrap(),
        dagq::infrastructure::container_image::BASE_CONTAINERFILE
    );
    // The project image takes the base as its argument, from a context of
    // the main checkout's files alone.
    let project = builds[1];
    assert!(project.contains(&format!("--tag {run}")), "{project}");
    assert!(
        project.contains(&format!(
            "--build-arg DAGQ_BASE_IMAGE={}",
            images.base.reference
        )),
        "{project}"
    );
    assert!(
        project.contains(".dagq/container/Containerfile"),
        "{project}"
    );
    let context = host.state.join("context-1");
    assert_eq!(
        fs::read_to_string(context.join("rust-toolchain.toml")).unwrap(),
        fs::read_to_string(host.checkout.join("rust-toolchain.toml")).unwrap()
    );
    assert!(
        fs::read_to_string(context.join(".dagq/container/Containerfile"))
            .unwrap()
            .contains("1.98.1")
    );
    let mut files: Vec<String> = walk(&context);
    files.sort();
    assert_eq!(
        files,
        [".dagq/container/Containerfile", "rust-toolchain.toml"]
    );

    // The old image of this queue goes; the base in use is kept for later,
    // and another queue's image is never touched.
    assert_eq!(
        host.images(),
        [
            "localhost/dagq-base:0000000000000000".to_owned(),
            "localhost/dagq-bbbbbbbbbbbbbbbb-image:2222222222222222".to_owned(),
            images.base.reference.clone(),
            run.clone(),
        ]
    );
    let (kind, payload) = host.records().latest().unwrap().unwrap();
    assert_eq!(kind, CONTAINER_IMAGE_BUILT);
    assert_eq!(payload["image"], run.as_str());
    assert_eq!(
        payload["removed"],
        serde_json::json!(["localhost/dagq-aaaaaaaaaaaaaaaa-image:1111111111111111"])
    );

    // Built: nothing more to do, and the gate asks on.
    let before = calls.len();
    assert_eq!(
        prepare(&host.specs(BUILD), &host.builder, &host.records(), 101),
        Ok(run.clone())
    );
    assert!(
        calls_since(&host.state, before)
            .iter()
            .all(|c| !c.contains(" build ")),
    );
    assert_eq!(
        host.gate(BUILD, 102).availability(RuntimeTarget::Worker),
        Availability::Available
    );

    // A worker's change in its clone changes nothing; a change landed on
    // main is a new image.
    fs::write(host.clone.join("rust-toolchain.toml"), "changed").unwrap();
    assert_eq!(host.specs(BUILD).images().unwrap().run(), run);
    fs::write(host.checkout.join("rust-toolchain.toml"), "changed").unwrap();
    assert_ne!(host.specs(BUILD).images().unwrap().run(), run);
}

#[test]
fn without_a_project_image_the_base_alone_is_built_and_this_queue_s_images_go() {
    let _waiting = common::test();
    let host = Host::new();
    fs::write(host.checkout.join("dagq.toml"), "").unwrap();
    fs::write(
        host.state.join("images"),
        "localhost/dagq-aaaaaaaaaaaaaaaa-image:1111111111111111\n",
    )
    .unwrap();
    let run = prepare(&host.specs("0.4.0"), &host.builder, &host.records(), 0).unwrap();
    assert!(run.starts_with("localhost/dagq-base:"), "{run}");
    let calls = calls(&host.state);
    let builds: Vec<&String> = calls.iter().filter(|c| c.contains(" build ")).collect();
    assert_eq!(builds.len(), 1, "{calls:?}");
    // A release's client comes from its crates.io source.
    assert!(
        builds[0].contains("--build-arg DAGQ_CLIENT_VERSION=0.4.0"),
        "{}",
        builds[0]
    );
    assert!(!builds[0].contains("DAGQ_BASE_IMAGE"), "{}", builds[0]);
    assert_eq!(host.images(), [run]);
}

#[test]
fn an_unobtainable_client_or_a_failed_build_is_unusable_and_recorded() {
    let _waiting = common::test();
    let host = Host::new();

    // A dirty build has no client of its version anywhere.
    let dirty = "0.4.0-dev+8b55cdb84e13.dirty";
    let Availability::Unavailable(why) = host.gate(dirty, 0).availability(RuntimeTarget::Worker)
    else {
        panic!("usable without a client");
    };
    assert!(
        why.contains("Linux dagq client") && why.contains("cannot be had"),
        "{why}"
    );
    let refused = prepare(&host.specs(dirty), &host.builder, &host.records(), 0).unwrap_err();
    assert_eq!(refused, why);
    assert!(
        calls(&host.state).is_empty(),
        "nothing is built without a client"
    );
    let (kind, payload) = host.records().latest().unwrap().unwrap();
    assert_eq!(kind, CONTAINER_IMAGE_FAILED);
    assert_eq!(payload["reason"], why.as_str());

    // A failed build: recorded with podman's words, and the gate answers
    // it until the hold is up.
    fs::write(host.state.join("fail-build"), "").unwrap();
    let failed = prepare(&host.specs(BUILD), &host.builder, &host.records(), 1_000).unwrap_err();
    assert!(failed.contains("step 3/5 failed"), "{failed}");
    let (kind, payload) = host.records().latest().unwrap().unwrap();
    assert_eq!(kind, CONTAINER_IMAGE_FAILED);
    assert_eq!(payload["image"], host.specs(BUILD).images().unwrap().run());
    let Availability::Unavailable(held) = host
        .gate(BUILD, 1_001)
        .availability(RuntimeTarget::Verification)
    else {
        panic!("usable right after a failed build");
    };
    assert!(held.contains("step 3/5 failed"), "{held}");
    assert_eq!(
        host.gate(BUILD, 1_000 + FAILURE_HOLD_SECS)
            .availability(RuntimeTarget::Worker),
        Availability::Available
    );

    // An input that leads outside the checkout is not read.
    let outside = host.checkout.parent().unwrap().join("secret");
    fs::write(&outside, "token").unwrap();
    fs::remove_file(host.checkout.join("rust-toolchain.toml")).unwrap();
    std::os::unix::fs::symlink(&outside, host.checkout.join("rust-toolchain.toml")).unwrap();
    let Availability::Unavailable(escaped) =
        host.gate(BUILD, 5_000).availability(RuntimeTarget::Worker)
    else {
        panic!("usable with an input outside the checkout");
    };
    assert!(escaped.contains("outside the checkout"), "{escaped}");

    // A file dagq.toml names that the main checkout does not have.
    fs::remove_file(host.checkout.join("rust-toolchain.toml")).unwrap();
    let Availability::Unavailable(missing) =
        host.gate(BUILD, 5_000).availability(RuntimeTarget::Worker)
    else {
        panic!("usable without its input");
    };
    assert!(missing.contains("rust-toolchain.toml"), "{missing}");
}

/// The real dagq machine builds the base and this repository's project
/// image. Needs podman and the network; run by hand.
#[test]
#[ignore = "needs podman, the dagq machine and the network"]
fn the_real_images_build_in_the_dagq_machine() {
    let _waiting = common::within(
        std::time::Duration::from_secs(3600),
        "the real images to build",
    );
    let dir = tempfile::tempdir().unwrap();
    let queue_dir = dir.path().join("aaaaaaaaaaaaaaaa");
    fs::create_dir_all(&queue_dir).unwrap();
    let db = queue_dir.join("queue.db");
    SqliteQueue::init(&db).unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::create_dir_all(checkout.path().join(".dagq/container")).unwrap();
    for path in [".dagq/container/Containerfile", "rust-toolchain.toml"] {
        fs::copy(root.join(path), checkout.path().join(path)).unwrap();
    }
    fs::write(checkout.path().join("dagq.toml"), DAGQ_TOML).unwrap();
    let specs = CheckoutImages::new(checkout.path(), &queue_dir);
    let records = QueueImageRecords { db, actor: None };
    let run = prepare(&specs, &PodmanImages::default(), &records, 0).unwrap();
    assert!(
        run.starts_with("localhost/dagq-aaaaaaaaaaaaaaaa-image:"),
        "{run}"
    );
}

fn calls_since(state: &Path, before: usize) -> Vec<String> {
    calls(state).split_off(before)
}

/// The files under `root`, relative to it.
fn walk(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    files
}
