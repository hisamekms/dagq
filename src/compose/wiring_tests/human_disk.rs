//! A person's integrate reads its candidate's files and the other runs
//! through the same production approval boundary as an ordinary landing.

use std::{path::Path, time::Duration};

use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::{
        RUN_TMP_DIR, TaskReads, TaskStore, host_verification_tmp_path, integrate::DiskRoom,
    },
    domain::{ClaimOutcome, RunStatus},
};

/// The refusal and subsequent approval both go through begin/check_disk_room.
/// The measured candidate uses 14 + 3 + 1 GiB: reserve 5 + unused I (30 - 18)
/// needs 17 GiB. Another claimed run prevents the idle progress exception.
#[test]
fn a_persons_integrate_measures_its_run_and_explains_the_stage_reservations() {
    const GIB: u64 = 1 << 30;
    let wiring = Wiring::new(
        "a_persons_integrate_measures_its_run_and_explains_the_stage_reservations",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("concern", "a person must approve")],
        },
    )
    .with_tick(Duration::from_millis(1));
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let run = queue.show(task).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let other = wiring.ready_task(&[]);
    let ClaimOutcome::Claimed { run: worker } = queue.claim(&wiring.git.main()).unwrap() else {
        panic!("claim the other run");
    };
    assert_eq!(worker.task_id(), other);
    wiring
        .files
        .set_tree_size(Path::new(run.worktree_path().unwrap()), 14 * GIB);
    wiring.files.set_tree_size(
        &Path::new(run.run_dir().unwrap()).join(RUN_TMP_DIR),
        3 * GIB,
    );
    wiring
        .files
        .set_tree_size(&host_verification_tmp_path(run.id()), GIB);
    let room = DiskRoom {
        config: crate::infrastructure::run_env::parse_config(
            "[disk]\nestimate = \"fixed\"\nrun_size = \"20GiB\"\nlanding_size = \"30GiB\"\n",
        )
        .unwrap()
        .disk,
        free: Some(16 * GIB),
        total: None,
    };
    let error = format!(
        "{:#}",
        wiring.try_integrate_outcome(task, Some(room)).unwrap_err()
    );
    for text in [
        "16.0 GiB free",
        "below the 17.0 GiB",
        "expected 20.0 GiB (fixed)",
        "stages worker 1, waiting 0, integrator 0; W 20.0 GiB, I 30.0 GiB, U 20.0 GiB",
    ] {
        assert!(error.contains(text), "{error}");
    }
    let refused = queue.show(task).unwrap();
    assert_eq!(refused.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(!refused.events.iter().any(|event| matches!(
        event.kind.as_str(),
        "integration_approved" | "integration_started"
    )));
    // 17 GiB is below the unmeasured requirement (35 GiB): the measured
    // worktree and both TMPDIRs must reach the gate, not just its message.
    let outcome = wiring
        .try_integrate_outcome(
            task,
            Some(DiskRoom {
                free: Some(17 * GIB),
                ..room
            }),
        )
        .unwrap();
    assert_eq!(outcome["outcome"], "integrated");
    assert_eq!(
        queue.show(task).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
    assert_eq!(
        queue.show(other).unwrap().runs[0].status(),
        RunStatus::Claimed
    );
}
