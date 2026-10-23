//! The conformance suite of the delivery an executor sends through
//! (ADR-t1662-3 decisions 1, 2, 6 and 9,
//! docs/design/measurement-environment.md "適合test"): whatever carries a [`TelemetrySink`]'s reports to the
//! queue's stores, each `(session_id, seq)` is kept once, a seq that did
//! not come in is a gap, what did not arrive is sent again by the executor
//! and kept once, the samples an executor measured from inside go in as
//! sent, and a version the service does not read records nothing. The
//! host's delivery (the queue service's `telemetry_report` on its unix
//! socket) passes it; another delivery passes [`conforms`] with a
//! [`Delivery`] of its own before it goes in.

use crate::common;
use common::service::{Queue, queue, start};

use dagq::{
    application::{
        SessionOwners, SessionResourceStore, SessionStepStore, TelemetrySink,
        queue_service::TelemetryTokens, step_report::StepReporter,
    },
    domain::{
        session_step::{ResourceSample, SeqRange, Step, StepKind, StepTarget, StepsRecorded},
        telemetry::{MIN_TELEMETRY_VERSION, ReportRecorded, TELEMETRY_VERSION, TelemetryReport},
    },
    infrastructure::{
        queue_service::{self as service, ServiceTelemetrySink, SystemServiceAccess},
        sqlite::SqliteQueue,
    },
};
use std::path::PathBuf;

/// The stores the reports go in: the queue's own, the record of truth.
pub trait Stores: SessionStepStore + SessionResourceStore {}

impl<T: SessionStepStore + SessionResourceStore> Stores for T {}

/// What a delivery's test gives the suite: one target its executor holds
/// a telemetry token for, and the sinks of that target.
pub trait Delivery {
    /// The target the control side issued the executor's token for.
    fn target(&self) -> StepTarget;
    /// Register `session` to the target, as the control side does before
    /// its executor reports.
    fn register(&self, session: &str);
    /// The sink the target's executor sends with.
    fn sink(&self) -> Box<dyn TelemetrySink>;
    /// A sink of the same target whose reports do not arrive (its route is
    /// gone).
    fn unreachable(&self) -> Box<dyn TelemetrySink>;
    /// The stores the reports go in.
    fn stores(&self) -> Box<dyn Stores>;
}

const fn recorded(added: usize, duplicates: usize) -> StepsRecorded {
    StepsRecorded { added, duplicates }
}

fn steps(seqs: &[i64]) -> Vec<Step> {
    seqs.iter()
        .map(|&seq| Step::new(seq, StepKind::ModelTurn, seq * 1000, 500))
        .collect()
}

fn held(delivery: &dyn Delivery, session: &str) -> Vec<i64> {
    delivery
        .stores()
        .target_steps(&delivery.target())
        .unwrap()
        .into_iter()
        .filter(|step| step.session_id == session)
        .map(|step| step.step.seq)
        .collect()
}

/// Run the whole suite on `delivery`.
pub fn conforms(delivery: &dyn Delivery) {
    keeps_each_step_once(delivery);
    finds_the_gaps(delivery);
    what_did_not_arrive_is_sent_again(delivery);
    keeps_the_samples_measured_from_inside(delivery);
    records_nothing_of_a_version_it_does_not_read(delivery);
}

/// A report sent twice adds its steps once: the second answer says they
/// were held.
fn keeps_each_step_once(delivery: &dyn Delivery) {
    delivery.register("once");
    let report = TelemetryReport::new(delivery.target(), "once", steps(&[1, 2]));
    let sink = delivery.sink();
    assert_eq!(sink.report(&report).unwrap().steps, recorded(2, 0));
    assert_eq!(sink.report(&report).unwrap().steps, recorded(0, 2));
    assert_eq!(held(delivery, "once"), [1, 2]);
}

/// A seq below the largest that did not come in is a gap, left as one
/// until its step comes in.
fn finds_the_gaps(delivery: &dyn Delivery) {
    delivery.register("gaps");
    let sink = delivery.sink();
    let send = |seqs: &[i64]| {
        sink.report(&TelemetryReport::new(
            delivery.target(),
            "gaps",
            steps(seqs),
        ))
        .unwrap()
    };
    send(&[1, 2, 5]);
    let stores = delivery.stores();
    assert_eq!(
        stores.session_gaps("gaps").unwrap(),
        [SeqRange { from: 3, to: 4 }]
    );
    assert_eq!(send(&[3, 4]).steps, recorded(2, 0));
    assert!(stores.session_gaps("gaps").unwrap().is_empty());
}

/// What the executor could not send it keeps and sends again, and what
/// arrived though its answer did not is kept once when sent again.
fn what_did_not_arrive_is_sent_again(delivery: &dyn Delivery) {
    delivery.register("again");
    let mut reporter = StepReporter::new(delivery.target(), Default::default(), 0);
    reporter.push("again", 0, steps(&[1, 2, 3]));
    let lost = reporter.flush(Some(delivery.unreachable().as_ref()), 30_000);
    assert_eq!((lost.sent, lost.kept), (0, 3));
    assert!(held(delivery, "again").is_empty());
    let sent = reporter.flush(Some(delivery.sink().as_ref()), 60_000);
    assert_eq!((sent.sent, sent.kept), (3, 0));
    assert_eq!(held(delivery, "again"), [1, 2, 3]);

    // Delivered, but its answer lost: the reporter keeps the steps and its
    // next flush finds them held.
    reporter.push("again", 60_000, steps(&[1, 2]));
    let answer_lost = AnswerLost(delivery.sink());
    let lost = reporter.flush(Some(&answer_lost), 90_000);
    assert_eq!((lost.sent, lost.kept), (0, 2));
    assert_eq!(held(delivery, "again"), [1, 2, 3, 4, 5]);
    let again = TelemetryReport::new(delivery.target(), "again", reporter_pending(&reporter));
    assert_eq!(
        delivery.sink().report(&again).unwrap().steps,
        recorded(0, 2)
    );
    let sent = reporter.flush(Some(delivery.sink().as_ref()), 120_000);
    assert_eq!((sent.sent, sent.kept), (2, 0));
    assert_eq!(held(delivery, "again"), [1, 2, 3, 4, 5]);
}

/// The steps `reporter` keeps of the session "again".
fn reporter_pending(reporter: &StepReporter) -> Vec<Step> {
    reporter.state().sessions["again"].pending.clone()
}

/// A sink whose report arrives but whose answer is lost.
struct AnswerLost(Box<dyn TelemetrySink>);

impl TelemetrySink for AnswerLost {
    fn report(&self, report: &TelemetryReport) -> anyhow::Result<ReportRecorded> {
        self.0.report(report)?;
        anyhow::bail!("the answer was lost")
    }
}

/// The samples an executor measured from inside (where the environment
/// does not measure from outside) go in as sent, in the common units,
/// once each.
fn keeps_the_samples_measured_from_inside(delivery: &dyn Delivery) {
    delivery.register("inside");
    let mut sample = ResourceSample::new(1, 0, 30_000);
    sample.cpu_seconds = Some(4.5);
    sample.cpu_limit = Some(2.0);
    sample.throttled_seconds = Some(0.25);
    sample.memory_max_bytes = Some(1 << 30);
    sample.node_id = Some("node-a".into());
    let report = TelemetryReport::new(delivery.target(), "inside", Vec::new())
        .with_resources(vec![sample.clone()]);
    let sink = delivery.sink();
    assert_eq!(
        sink.report(&report).unwrap(),
        ReportRecorded {
            steps: recorded(0, 0),
            resources: recorded(1, 0),
        }
    );
    assert_eq!(sink.report(&report).unwrap().resources, recorded(0, 1));
    let kept: Vec<ResourceSample> = delivery
        .stores()
        .target_resources(&delivery.target())
        .unwrap()
        .into_iter()
        .filter(|row| row.session_id == "inside")
        .map(|row| row.sample)
        .collect();
    assert_eq!(kept, [sample]);
}

/// A report of a version the service does not read, newer or older, is
/// refused and records nothing; the same report of this version goes in.
fn records_nothing_of_a_version_it_does_not_read(delivery: &dyn Delivery) {
    delivery.register("version");
    let sink = delivery.sink();
    let report = TelemetryReport::new(delivery.target(), "version", steps(&[1]));
    for version in [TELEMETRY_VERSION + 1, MIN_TELEMETRY_VERSION - 1] {
        let other = TelemetryReport {
            version,
            ..report.clone()
        };
        let refused = sink.report(&other).unwrap_err().to_string();
        assert!(
            refused.contains(&format!("version {version} is not one of")),
            "{refused}"
        );
    }
    assert!(held(delivery, "version").is_empty());
    assert_eq!(sink.report(&report).unwrap().steps, recorded(1, 0));
}

/// The host's delivery: the queue service on its unix socket, with the
/// telemetry token the control side issued for the queue's run.
struct HostService {
    queue: Queue,
    target: StepTarget,
    credential: PathBuf,
}

impl HostService {
    fn start() -> Self {
        let queue = queue();
        assert_eq!(start(&queue)["service"]["state"], "running");
        let target = StepTarget::run(queue.run.id());
        let credential = SystemServiceAccess
            .issue_telemetry(&queue.db, &target, Some(queue.run.task_id()), None)
            .unwrap();
        Self {
            queue,
            target,
            credential,
        }
    }
}

impl Delivery for HostService {
    fn target(&self) -> StepTarget {
        self.target.clone()
    }
    fn register(&self, session: &str) {
        SqliteQueue::open(&self.queue.db)
            .unwrap()
            .register_session(&self.target, session, 1)
            .unwrap();
    }
    fn sink(&self) -> Box<dyn TelemetrySink> {
        Box::new(ServiceTelemetrySink::new(
            service::socket_path(self.queue.dir()),
            self.credential.clone(),
        ))
    }
    fn unreachable(&self) -> Box<dyn TelemetrySink> {
        Box::new(ServiceTelemetrySink::new(
            self.queue.dir().join("gone.sock"),
            self.credential.clone(),
        ))
    }
    fn stores(&self) -> Box<dyn Stores> {
        Box::new(SqliteQueue::open(&self.queue.db).unwrap())
    }
}

/// The host's delivery passes the suite every delivery passes.
#[test]
fn the_hosts_delivery_passes_the_conformance_suite_of_every_delivery() {
    conforms(&HostService::start());
}
