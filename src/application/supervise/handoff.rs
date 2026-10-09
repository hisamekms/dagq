//! The handoff of a supervisor to another binary (ADR-0045 decision 10):
//! asked through its registration, the supervisor starts no new work, lets
//! the validations and landings in progress finish, stops its headless
//! jobs, and ends its loop so the entry point can exec the new binary under
//! the same pid. The new process keeps the token, the registration and
//! every lease, and rebuilds each run's slot the way an adoption does
//! (ADR-0039), from the queue and the run files, without writing
//! `run_adopted`. What the queue does not hold — a resumed session's
//! request, an `/exit` a rejected run waits for — goes through a
//! `handoff.json` in the run directory ([`write_snapshot`],
//! [`take_snapshot`]).
//!
//! This module holds host運用's side: the registration of this process and
//! the state of its handoff and stop ([`Registration`]), with the decision
//! on a handoff request read again ([`handoff_change`]). The slots are
//! 実行と着地's: the loop snapshots them before the exec and rebuilds them
//! after it (`handoff_slots`).

use super::*;
use crate::application::SupervisorRegistry;
use crate::domain::EventKind;
use crate::domain::LeaseToken;
use serde::Deserialize;

/// The run event of each run the process that took a registration over
/// after an exec found leased to it.
pub const SUPERVISOR_HANDED_OFF: &str =
    crate::domain::event_kind::EventKind::SupervisorHandedOff.as_str();

/// What a run directory's `handoff.json` holds for the next process.
const SNAPSHOT: &str = "handoff.json";

/// host運用's registration of this supervisor: its token and heartbeat, and
/// the handoff and stop it drains for.
pub(super) struct Registration {
    /// The registration's token, which every lease of this process carries.
    pub(super) token: LeaseToken,
    pub(super) heartbeat: Heartbeat,
    /// The binary a handoff asked this process to exec (ADR-0045 decision
    /// 10): no new work starts, and the loop ends once every slot rests at
    /// a point the next process rebuilds it from.
    pub(super) handoff: Option<String>,
    /// Set when the loop ended for that exec: the registration stays.
    pub(super) exec: Option<String>,
    /// This pass drains (a stop, a handoff, or claiming stopped after a
    /// provisioning failure): nothing may wait for the program to appear.
    pub(super) draining: bool,
    /// Whether this process recorded `supervisor_draining` for its stop
    /// request (task 1277): once, on the first pass that saw it.
    pub(super) stop_recorded: bool,
}

/// What the queue's handoff request, read again, makes of the handoff this
/// process drains for (task 1286).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HandoffChange {
    /// The same binary is asked: the drain goes on.
    Same,
    /// Another binary is asked: the drain goes on for it.
    Replaced(String),
    /// The request was withdrawn: normal work resumes.
    Withdrawn,
}

/// [`HandoffChange`] of the handoff to `binary` given the request read now
/// (`requested`).
pub(super) fn handoff_change(binary: &str, requested: Option<String>) -> HandoffChange {
    match requested {
        Some(now) if now == binary => HandoffChange::Same,
        Some(now) => HandoffChange::Replaced(now),
        None => HandoffChange::Withdrawn,
    }
}

/// Whether a pass drains: on a stop, while claiming stopped (a
/// provisioning failure), and while a handoff is asked.
pub(super) const fn drains(stopping: bool, claiming: bool, handing_off: bool) -> bool {
    stopping || !claiming || handing_off
}

impl Registration {
    /// The handoff request this process drains for was replaced by one for
    /// `now`: the drain goes on for the new binary (task 1286).
    pub(super) fn replace_handoff(&mut self, binary: &str, now: String) {
        info!(
            "supervisor {} handoff to {binary} was replaced by a handoff to {now}: no new work starts; it execs {now} once the validations and landings in progress are done",
            self.token
        );
        self.handoff = Some(now);
    }

    /// Record, once, that this process drains for a stop request (SIGINT /
    /// SIGTERM, from `down`, the drain of `up` or `install
    /// --allow-breaking`, or launchd's bootout) (task 1277), with the runs
    /// its slots hold: a handoff waiting for it fails at once instead of at
    /// its timeout, and the observer reads the stop behind the claims and
    /// resumes it holds. A failed write is only logged and tried again on
    /// the next pass.
    pub(super) fn record_stop_request(
        &mut self,
        queue: &(impl SupervisorRegistry + RunLog + ?Sized),
        build: &str,
        runs: &[RunId],
    ) {
        let handoff = match &self.handoff {
            Some(binary) => Some(binary.clone()),
            None => queue.handoff_request(&self.token).ok().flatten(),
        };
        let payload = json!({
            "supervisor": self.token,
            "pid": std::process::id(),
            "build": build,
            "reason": "stop_requested",
            "handoff_binary": handoff,
            "runs": runs,
        });
        match queue.record_queue_event(EventKind::SupervisorDraining, payload) {
            Ok(_) => {
                self.stop_recorded = true;
                info!(
                    "supervisor {} asked to stop: it drains the runs in progress; a stop wins over any handoff",
                    self.token
                );
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the supervisor's stop request could not be recorded: {error:#}");
            }
        }
    }

    /// The end of a loop not ended for an exec, whose heartbeat holds: stop
    /// the heartbeat, and remove the registration with the record of the
    /// stop (ADR-0051 decision 10; `ok` whether the loop ended without an
    /// error) in one transaction (ADR-t1662-1 decision 6). A record that
    /// fails keeps the row, which goes stale for the next `up` or `down` to
    /// prune. Whether the loop ended so; an exec, or a lost heartbeat,
    /// keeps the registration.
    pub(super) fn deregister(
        &mut self,
        queue: &(impl SupervisorRegistry + ?Sized),
        version: &str,
        ok: bool,
    ) -> bool {
        if self.exec.is_some() || self.heartbeat.check().is_err() {
            return false;
        }
        // The mark of the stop; an exec leaves it to the next process's
        // handoff mark.
        let stopped = json!({
            "supervisor": self.token,
            "dagq_version": version,
            "outcome": if ok { "stopped" } else { "failed" },
        });
        self.heartbeat.stop();
        if let Err(error) =
            queue.prune_supervisor(&self.token, EventKind::SupervisorStopped, &|_| {
                stopped.clone()
            })
        {
            warn!(error = %format_args!("{error:#}"), "the supervisor's stop could not be recorded, and its registration is left for the next prune: {error:#}");
        }
        true
    }
}

/// `handoff.json`: the part of a slot the queue does not hold, written
/// before the exec by the supervisor `token`. Only that supervisor's next
/// process reads it; a file another process left behind is ignored.
#[derive(Debug, Serialize, Deserialize)]
struct Stamped {
    token: LeaseToken,
    #[serde(flatten)]
    snapshot: Snapshot,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(super) enum Snapshot {
    /// A resumed session of a `needs_session` run.
    Resume {
        workspace: String,
        attempt: usize,
        started_at: f64,
        message: String,
        /// What `message` took when it was built (ADR-t2072-1); a file
        /// written by an older binary has none.
        #[serde(default)]
        message_bytes: Option<PromptBytes>,
        message_sent_at: Option<f64>,
        not_ready_asked: bool,
        exit_requested: bool,
        #[serde(default)]
        exit_typed: bool,
        exit_for_silence: bool,
        approved: bool,
    },
    /// The `/exit` of a run that rests after its validation.
    Exit {
        workspace: Option<String>,
        resume: Option<usize>,
        close: bool,
        requested: bool,
        timed_out: bool,

        exit_for_silence: bool,
    },
}

pub(super) fn seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub(super) fn time(seconds: f64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(seconds.max(0.0))
}

/// Write `snapshot` into `dir`'s `handoff.json`, stamped with `token`.
pub(super) fn write_snapshot(
    files: &dyn RunFiles,
    token: &LeaseToken,
    dir: &Path,
    snapshot: Snapshot,
) -> Result<()> {
    let text = serde_json::to_vec(&Stamped {
        token: token.clone(),
        snapshot,
    })?;
    files.write(&dir.join(SNAPSHOT), &text).map_err(Into::into)
}

/// Read and remove `run`'s `handoff.json`; `None` without one, when it does
/// not parse, or when another token wrote it.
pub(super) fn take_snapshot(
    files: &dyn RunFiles,
    token: &LeaseToken,
    run: &TaskRun,
) -> Option<Snapshot> {
    let path = Path::new(run.run_dir()?).join(SNAPSHOT);
    if !files.is_file(&path) {
        return None;
    }
    let text = files.read(&path).ok();
    let _ = files.remove_file(&path);
    text.and_then(|text| serde_json::from_slice::<Stamped>(&text).ok())
        .filter(|stamped| stamped.token == *token)
        .map(|stamped| stamped.snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handoff request read again keeps, replaces or withdraws the
    /// handoff this process drains for; a pass drains on a stop, a stopped
    /// claim or a handoff.
    #[test]
    fn a_handoff_request_read_again_keeps_replaces_or_withdraws_the_drain() {
        assert_eq!(
            handoff_change("/bin/a", Some("/bin/a".to_owned())),
            HandoffChange::Same
        );
        assert_eq!(
            handoff_change("/bin/a", Some("/bin/b".to_owned())),
            HandoffChange::Replaced("/bin/b".to_owned())
        );
        assert_eq!(handoff_change("/bin/a", None), HandoffChange::Withdrawn);
        assert!(!drains(false, true, false));
        assert!(drains(true, true, false));
        assert!(drains(false, false, false));
        assert!(drains(false, true, true));
    }

    #[test]
    fn old_resume_snapshot_defaults_exit_typed_to_false() {
        let mut value = json!({
            "phase": "resume", "workspace": "w", "attempt": 1,
            "started_at": 0.0, "message": "resume", "message_sent_at": null,
            "not_ready_asked": false, "exit_requested": true,
            "exit_for_silence": false, "approved": true
        });
        for expected in [false, true] {
            let snapshot: Snapshot = serde_json::from_value(value.clone()).unwrap();
            assert!(
                matches!(snapshot, Snapshot::Resume { exit_typed, .. } if exit_typed == expected)
            );
            value["exit_typed"] = json!(true);
        }
    }
}
