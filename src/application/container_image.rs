//! The container image's use cases (ADR-t2257-1 decision 2): decide the
//! images of a queue from the main checkout ([`ImageSpecs`]), build the
//! missing ones in the dagq machine and remove the old ones ([`prepare`],
//! called once the machine runs, before a container starts), and answer
//! the container runtime as unusable while the image cannot be had
//! ([`ImageGate`]). Every failure is recorded on the queue
//! (`container_image_failed`) and is the gate's answer for a while
//! ([`failure_holds`]).
//!
//! What is decided is [`crate::domain::container_image`]'s; podman, the
//! files and the queue are the adapters'.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::domain::container_image::{
    ImageBuild, Images, built_payload, failure_holds, failure_payload, steps,
};
use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::{EventKind, RecordedKind};

/// The images of the queue as they are decided now: from the runtime's
/// build, and from `dagq.toml` and the files it names in the main
/// checkout (never a run's clone). `Err` says why they cannot be decided:
/// the client cannot be had, or the file or a file it names cannot be
/// read.
pub trait ImageSpecs {
    fn images(&self) -> Result<Images, String>;
}

/// The podman commands on the images inside the dagq machine.
pub trait ImageBuilder {
    /// The references of every image in the machine.
    fn images(&self) -> Result<Vec<String>>;
    /// Build `image` from its context's files and build arguments alone.
    fn build(&self, image: &ImageBuild) -> Result<()>;
    /// Remove `reference`, refused while a container or an image uses it.
    fn remove(&self, reference: &str) -> Result<()>;
}

/// The queue's records of the image.
pub trait ImageRecords {
    fn record(&self, kind: EventKind, payload: Value) -> Result<()>;
    /// The latest event of the queue of
    /// [`crate::domain::container_image::CONTAINER_IMAGE_KINDS`], its kind
    /// and payload.
    fn latest(&self) -> Result<Option<(RecordedKind, Value)>>;
}

/// An image record kept by the process that made it, for the queue to
/// record later ([`Self::recorded`]), named by its kind
/// ([`crate::domain::container_image::CONTAINER_IMAGE_KINDS`]): a failure's
/// image (none when it could not be decided) and why, or the image a
/// build made ready with what it built and removed. It carries no time:
/// the queue stamps a failure with its own clock, so that no sender sets
/// how long the failure holds the container
/// ([`crate::domain::container_image::failure_holds`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeptImageRecord {
    ContainerImageFailed {
        image: Option<String>,
        reason: String,
    },
    ContainerImageBuilt {
        image: String,
        built: Vec<String>,
        removed: Vec<String>,
    },
}

impl KeptImageRecord {
    /// The record of `kind` with `payload` as [`prepare`] makes it, its
    /// time left out.
    fn of(kind: EventKind, payload: &Value) -> Result<Self> {
        let mut fields = payload.clone();
        let Some(map) = fields.as_object_mut() else {
            anyhow::bail!("an image record's payload is no object: {payload}");
        };
        map.remove("at");
        map.insert("kind".to_owned(), Value::from(kind.as_str()));
        serde_json::from_value(fields).map_err(|error| {
            anyhow::anyhow!("an image record of no known shape ({error}): {payload}")
        })
    }

    /// The kind and payload the queue records at `now` (Unix seconds).
    pub fn recorded(&self, now: i64) -> (EventKind, Value) {
        match self {
            Self::ContainerImageFailed { image, reason } => (
                EventKind::ContainerImageFailed,
                failure_payload(image.as_deref(), reason, now),
            ),
            Self::ContainerImageBuilt {
                image,
                built,
                removed,
            } => (
                EventKind::ContainerImageBuilt,
                built_payload(
                    image,
                    &built.iter().map(String::as_str).collect::<Vec<_>>(),
                    removed,
                ),
            ),
        }
    }
}

/// The records of the images a process that opens no queue makes ready
/// (a host's `dagq` whose command goes to the queue service), kept here
/// until it sends them to the service ([`Self::take`]); reading them is
/// not one of its uses.
#[derive(Debug, Default)]
pub struct KeptImageRecords(std::sync::Mutex<Vec<KeptImageRecord>>);

impl KeptImageRecords {
    /// The records kept so far, in the order they were made, and none
    /// kept after.
    pub fn take(&self) -> Vec<KeptImageRecord> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl ImageRecords for KeptImageRecords {
    fn record(&self, kind: EventKind, payload: Value) -> Result<()> {
        let record = KeptImageRecord::of(kind, &payload)?;
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
        Ok(())
    }

    fn latest(&self) -> Result<Option<(RecordedKind, Value)>> {
        anyhow::bail!("the kept image records are sent to the queue, not read")
    }
}

/// Make the image a container of the queue runs be there, at `now` (Unix
/// seconds): decide the images, build the missing ones (the base, then the
/// project's on it), and remove the old ones. A failure is recorded and
/// returned as why the container cannot be used; a removal that fails is
/// left for the next time (an image a container runs is not removed).
/// The reference of the image to run is returned.
pub fn prepare(
    specs: &dyn ImageSpecs,
    builder: &dyn ImageBuilder,
    records: &dyn ImageRecords,
    now: i64,
) -> Result<String, String> {
    let fail = |image: Option<&str>, why: String| {
        if let Err(error) = records.record(
            EventKind::ContainerImageFailed,
            failure_payload(image, &why, now),
        ) {
            tracing::warn!(error = %format_args!("{error:#}"), "the image's failure could not be recorded: {error:#}");
        }
        Err(why)
    };
    let images = match specs.images() {
        Ok(images) => images,
        Err(why) => return fail(None, why),
    };
    let run = images.run().to_owned();
    let listed = match builder.images() {
        Ok(listed) => listed,
        Err(error) => {
            return fail(
                Some(&run),
                format!("list the images of the dagq machine: {error:#}"),
            );
        }
    };
    let steps = steps(&images, &listed);
    let mut built: Vec<&str> = Vec::new();
    let to_build = [
        steps.build_base.then_some(&images.base),
        images.project.as_ref().filter(|_| steps.build_project),
    ];
    for image in to_build.into_iter().flatten() {
        if let Err(error) = builder.build(image) {
            return fail(
                Some(&run),
                format!("build the container image {}: {error:#}", image.reference),
            );
        }
        built.push(&image.reference);
    }
    let mut removed = Vec::new();
    for old in steps.remove {
        match builder.remove(&old) {
            Ok(()) => removed.push(old),
            Err(error) => {
                tracing::info!(error = %format_args!("{error:#}"), image = %old, "an old container image is kept for now: {error:#}");
            }
        }
    }
    if (!built.is_empty() || !removed.is_empty())
        && let Err(error) = records.record(
            EventKind::ContainerImageBuilt,
            built_payload(&run, &built, &removed),
        )
    {
        tracing::warn!(error = %format_args!("{error:#}"), "the image's build could not be recorded: {error:#}");
    }
    Ok(run)
}

/// The container runtime as it can be used with its image: while the
/// images cannot be decided, or the latest attempt at this image failed a
/// short while ago, no target set to the container can run, and the
/// answer says why; else `container` answers. Only looks: building is
/// [`prepare`]'s, before a container starts.
pub struct ImageGate<C> {
    pub specs: Arc<dyn ImageSpecs + Send + Sync>,
    pub records: Arc<dyn ImageRecords + Send + Sync>,
    /// The Unix seconds now.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub container: C,
}

impl<C: super::ContainerRuntime> super::ContainerRuntime for ImageGate<C> {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        let images = match self.specs.images() {
            Ok(images) => images,
            Err(why) => return Availability::Unavailable(why),
        };
        let last = match self.records.latest() {
            Ok(last) => last,
            Err(error) => {
                return Availability::Unavailable(format!(
                    "read the container image's records: {error:#}"
                ));
            }
        };
        let last = last.as_ref().map(|(kind, payload)| (kind, payload));
        match failure_holds(last, images.run(), (self.now)()) {
            Some(why) => Availability::Unavailable(why),
            None => self.container.availability(target),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::application::ContainerRuntime;
    use crate::domain::container_image::{
        BaseInputs, ClientSource, ContextFile, FAILURE_HOLD_SECS, ProjectImage,
    };
    use crate::domain::podman_machine::QueueKey;

    /// The records kept by a process that opens no queue are taken once,
    /// in the order made, and are not read; the queue records each as it
    /// was made but a failure's time, which is the queue's when it records
    /// it. A payload of another shape is not kept.
    #[test]
    fn kept_image_records_are_taken_once_in_order_and_not_read() {
        let kept = KeptImageRecords::default();
        kept.record(
            EventKind::ContainerImageFailed,
            failure_payload(Some("img:1"), "no client", 5),
        )
        .unwrap();
        kept.record(
            EventKind::ContainerImageBuilt,
            built_payload("img:2", &["img:2"], &["img:1".to_owned()]),
        )
        .unwrap();
        assert!(
            kept.record(EventKind::ContainerImageFailed, serde_json::json!({"a": 1}))
                .is_err()
        );
        assert!(kept.latest().is_err());
        let recorded: Vec<(EventKind, Value)> = kept
            .take()
            .iter()
            .map(|record| record.recorded(900))
            .collect();
        assert_eq!(
            recorded,
            [
                (
                    EventKind::ContainerImageFailed,
                    failure_payload(Some("img:1"), "no client", 900)
                ),
                (
                    EventKind::ContainerImageBuilt,
                    built_payload("img:2", &["img:2"], &["img:1".to_owned()])
                ),
            ]
        );
        assert!(kept.take().is_empty());
    }

    struct Specs(Result<Images, String>);

    impl ImageSpecs for Specs {
        fn images(&self) -> Result<Images, String> {
            self.0.clone()
        }
    }

    #[derive(Default)]
    struct Machine {
        listed: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
        fail_build: bool,
        keep: Vec<String>,
    }

    impl ImageBuilder for Machine {
        fn images(&self) -> Result<Vec<String>> {
            Ok(self.listed.lock().unwrap().clone())
        }
        fn build(&self, image: &ImageBuild) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("build {}", image.reference));
            if self.fail_build {
                return Err(anyhow!("exit 1"));
            }
            self.listed.lock().unwrap().push(image.reference.clone());
            Ok(())
        }
        fn remove(&self, reference: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("rmi {reference}"));
            if self.keep.iter().any(|kept| kept == reference) {
                return Err(anyhow!("in use"));
            }
            self.listed.lock().unwrap().retain(|each| each != reference);
            Ok(())
        }
    }

    #[derive(Default)]
    struct Records(Mutex<Vec<(RecordedKind, Value)>>);

    impl ImageRecords for Records {
        fn record(&self, kind: EventKind, payload: Value) -> Result<()> {
            self.0.lock().unwrap().push((kind.into(), payload));
            Ok(())
        }
        fn latest(&self) -> Result<Option<(RecordedKind, Value)>> {
            Ok(self.0.lock().unwrap().last().cloned())
        }
    }

    fn images(project: bool) -> Images {
        let image = ProjectImage {
            containerfile: "Containerfile".into(),
            inputs: vec![],
        };
        Images::of(
            &BaseInputs {
                build: "1.0.0".into(),
                claude_code: "2".into(),
                containerfile: "FROM x".into(),
                client: ClientSource::Release {
                    version: "1.0.0".into(),
                },
            },
            project.then(|| {
                (
                    &image,
                    vec![ContextFile {
                        path: "Containerfile".into(),
                        content: b"FROM y".to_vec(),
                    }],
                )
            }),
            &QueueKey::new("q").unwrap(),
        )
    }

    #[test]
    fn missing_images_are_built_base_first_and_old_ones_removed_and_recorded() {
        let images = images(true);
        let machine = Machine {
            listed: Mutex::new(vec![
                "localhost/dagq-base:old".into(),
                "localhost/dagq-q-image:old".into(),
            ]),
            keep: vec!["localhost/dagq-base:old".into()],
            ..Machine::default()
        };
        let records = Records::default();
        let run = prepare(&Specs(Ok(images.clone())), &machine, &records, 5).unwrap();
        assert_eq!(run, images.run());
        assert_eq!(
            *machine.calls.lock().unwrap(),
            [
                format!("build {}", images.base.reference),
                format!("build {run}"),
                "rmi localhost/dagq-base:old".to_owned(),
                "rmi localhost/dagq-q-image:old".to_owned(),
            ]
        );
        let recorded = records.0.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, EventKind::ContainerImageBuilt);
        // The image in use is kept and not reported as removed.
        assert_eq!(
            recorded[0].1["removed"],
            serde_json::json!(["localhost/dagq-q-image:old"])
        );

        // Everything there: nothing built, nothing recorded.
        let machine = Machine {
            listed: Mutex::new(vec![images.base.reference.clone(), run.clone()]),
            ..Machine::default()
        };
        assert_eq!(prepare(&Specs(Ok(images)), &machine, &records, 6), Ok(run));
        assert!(machine.calls.lock().unwrap().is_empty());
        assert_eq!(records.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn without_a_project_image_the_base_alone_is_built_and_run() {
        let images = images(false);
        let machine = Machine::default();
        let run = prepare(&Specs(Ok(images.clone())), &machine, &Records::default(), 0).unwrap();
        assert_eq!(run, images.base.reference);
        assert_eq!(*machine.calls.lock().unwrap(), [format!("build {run}")]);
    }

    #[test]
    fn a_failed_build_or_an_unobtainable_client_is_recorded_and_holds_the_gate() {
        struct Usable;
        impl ContainerRuntime for Usable {
            fn availability(&self, _: RuntimeTarget) -> Availability {
                Availability::Available
            }
        }
        let images = images(true);
        let machine = Machine {
            fail_build: true,
            ..Machine::default()
        };
        let records = Arc::new(Records::default());
        let why = prepare(&Specs(Ok(images.clone())), &machine, &*records, 100).unwrap_err();
        assert!(why.contains("exit 1"), "{why}");
        // The base failed: the project image is not tried on nothing.
        assert_eq!(machine.calls.lock().unwrap().len(), 1);
        let recorded = records.0.lock().unwrap().clone();
        assert_eq!(recorded[0].0, EventKind::ContainerImageFailed);
        assert_eq!(recorded[0].1["image"], images.run());

        let gate = |now: i64, specs: Result<Images, String>| ImageGate {
            specs: Arc::new(Specs(specs)),
            records: records.clone(),
            now: Arc::new(move || now),
            container: Usable,
        };
        let Availability::Unavailable(held) =
            gate(101, Ok(images.clone())).availability(RuntimeTarget::Worker)
        else {
            panic!("usable right after a failed build");
        };
        assert!(held.contains("exit 1"), "{held}");
        assert_eq!(
            gate(100 + FAILURE_HOLD_SECS, Ok(images.clone())).availability(RuntimeTarget::Worker),
            Availability::Available
        );
        // Changed inputs are another image, tried at once.
        assert_eq!(
            gate(101, Ok(self::images(false))).availability(RuntimeTarget::Verification),
            Availability::Available
        );

        // The client cannot be had: unusable at once, and recorded by the
        // attempt.
        let no_client = Err("the Linux dagq client cannot be had".to_owned());
        assert_eq!(
            gate(101, no_client.clone()).availability(RuntimeTarget::Worker),
            Availability::Unavailable("the Linux dagq client cannot be had".into())
        );
        let why = prepare(&Specs(no_client), &Machine::default(), &*records, 102).unwrap_err();
        assert!(why.contains("cannot be had"), "{why}");
        let last = records.latest().unwrap().unwrap();
        assert_eq!(last.0, EventKind::ContainerImageFailed);
        assert_eq!(last.1["image"], Value::Null);
    }
}
