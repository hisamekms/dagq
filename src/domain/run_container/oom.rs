//! Diagnostics from Podman's died event, independent of failure handling.
use crate::domain::verify_failure::VerifyFailure;
use serde_json::{Value, json};

/// M5 reads only the externally written died event's OOMKilled field.
/// Missing events (including events_logger=none), malformed output and a
/// failed call are unreadable, never evidence of absence. A marker before
/// the field distinguishes an event without the optional field from no event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OomState {
    Observed,
    Absent,
    Unreadable(String),
}

/// Arguments for a bounded historical read, retaining --rm on the run.
/// `since` and `until` are Unix milliseconds supplied by the control side;
/// RFC 3339 retains subsecond precision when a name is reused between commands.
pub fn events_args(container: &str, since: i64, until: i64) -> Vec<String> {
    [
        "events".to_owned(),
        "--since".into(),
        crate::domain::transcript::millis_text(since),
        "--until".into(),
        crate::domain::transcript::millis_text(until),
        "--filter".into(),
        format!("container={container}"),
        "--filter".into(),
        "event=died".into(),
        "--format".into(),
        "died:{{.OOMKilled}}".into(),
    ]
    .into()
}

pub fn read_events(success: bool, output: &str) -> OomState {
    if !success {
        return OomState::Unreadable("podman events failed".into());
    }
    let lines: Vec<_> = output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if lines.is_empty() {
        return OomState::Unreadable("no died event (events_logger may be none)".into());
    }
    if lines.iter().any(|line| {
        !matches!(
            *line,
            "died:true" | "died:false" | "died:" | "died:<nil>" | "died:<no value>"
        )
    }) {
        return OomState::Unreadable("unrecognized died event output".into());
    }
    if lines.contains(&"died:true") {
        OomState::Observed
    } else {
        OomState::Absent
    }
}

/// Facts and possible causes stay separate: OOMKilled does not establish
/// that a process died, the cause of the command's failure, or which limit
/// was exhausted. It may miss an occurrence. Limits are the configuration
/// used for this container; machine memory is optional when unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OomObservation {
    pub state: OomState,
    pub container: String,
    pub memory_mib: u64,
    pub machine_memory_mib: Option<u64>,
}

impl OomObservation {
    pub fn unreadable(container: &str, memory_mib: u64) -> Self {
        Self {
            state: OomState::Unreadable("this port cannot read podman events".into()),
            container: container.into(),
            memory_mib,
            machine_memory_mib: None,
        }
    }
    pub fn text(&self) -> Option<String> {
        (self.state == OomState::Observed).then(|| format!("{} {}", self.fact(), self.causes()))
    }
    pub fn fact(&self) -> &'static str {
        "container の cgroup で OOM の事態が起きた（podman の died の OOMKilled。process の終了を断定せず、取りこぼしもありうる）。"
    }
    pub fn causes(&self) -> String {
        let machine = self
            .machine_memory_mib
            .map_or_else(|| "メモリの値は読めない".into(), |m| format!("{m} MiB"));
        format!(
            "原因の候補: container の上限（--memory {}m）か、machine 全体のメモリ不足（{machine}）。",
            self.memory_mib
        )
    }
    /// `oom` distinguishes the three observations; `unreadable` gives
    /// the read's failure rather than treating it as absence. `fact` and
    /// `possible_causes` are populated only for a positive observation.
    /// The name and configured limits identify its container; command and
    /// the pre-existing failure class/evidence identify a verification.
    fn event(&self, command: Option<&str>, failure: Option<&VerifyFailure>) -> Value {
        json!({"container": self.container, "oom": match self.state { OomState::Observed => "observed", OomState::Absent => "absent", OomState::Unreadable(_) => "unreadable" },
        "unreadable": if let OomState::Unreadable(why) = &self.state { Some(why) } else { None },
        "fact": self.text().map(|_| self.fact()), "possible_causes": self.text().map(|_| self.causes()),
        "memory_mib": self.memory_mib, "machine_memory_mib": self.machine_memory_mib,
        "command": command, "failure": failure.map(VerifyFailure::to_json), "reason": self.text()})
    }
}

/// A successful command or a failure to create/start its container has
/// no diagnostic read. Exit codes only decide whether a command ran;
/// they never decide whether OOM occurred.
pub fn should_read(success: bool, code: Option<i32>) -> bool {
    !success && !super::podman_failed(code)
}

/// Diagnostic effects only; no resume/retry/held/handoff/flaky policy.
/// Every failed verification records its observation, even unreadability.
/// Only the first observed OOM of a run opens inbox attention, deduplicated
/// by the run's existing container_oom_attention event, including resumes.
pub struct VerificationDiagnostic {
    pub event: Value,
    pub attention: bool,
    pub annotation: Option<String>,
}
pub fn verification(
    observation: &OomObservation,
    command: &str,
    failure: &VerifyFailure,
    attention_exists: bool,
) -> VerificationDiagnostic {
    VerificationDiagnostic {
        event: observation.event(Some(command), Some(failure)),
        attention: observation.state == OomState::Observed && !attention_exists,
        annotation: observation.text(),
    }
}

/// A turn records only a positive observation, with no continuation policy.
pub fn turn(observation: &OomObservation) -> Option<Value> {
    (observation.state == OomState::Observed).then(|| observation.event(None, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::verify_failure::FailureClass;
    #[test]
    fn historical_arguments_bound_the_named_died_event_and_read_its_field() {
        assert_eq!(
            events_args("c-123", 10, 20),
            [
                "events",
                "--since",
                "1970-01-01T00:00:00.010Z",
                "--until",
                "1970-01-01T00:00:00.020Z",
                "--filter",
                "container=c-123",
                "--filter",
                "event=died",
                "--format",
                "died:{{.OOMKilled}}"
            ]
        );
    }
    #[test]
    fn event_presence_is_distinct_from_absence_and_unreadability() {
        for (success, text, expected) in [
            (true, "died:true\n", OomState::Observed),
            (true, "died:\n", OomState::Absent),
            (true, "died:false\n", OomState::Absent),
        ] {
            assert_eq!(read_events(success, text), expected);
        }
        for (success, text) in [
            (false, "died:true"),
            (true, ""),
            (true, "true"),
            (true, "died:garbage"),
        ] {
            assert!(matches!(
                read_events(success, text),
                OomState::Unreadable(_)
            ));
        }
    }
    #[test]
    fn diagnostic_effects_preserve_the_class_and_only_deduplicate_attention() {
        let failure = VerifyFailure {
            class: FailureClass::TestFailure,
            evidence: "test failed".into(),
            profile: None,
        };
        let mut o = OomObservation {
            state: OomState::Observed,
            container: "c".into(),
            memory_mib: 456,
            machine_memory_mib: Some(789),
        };
        for class in [
            FailureClass::TestFailure,
            FailureClass::Killed,
            FailureClass::Timeout,
            FailureClass::DiskFull,
            FailureClass::Flaky,
        ] {
            let f = VerifyFailure {
                class,
                ..failure.clone()
            };
            for exists in [false, true] {
                let d = verification(&o, "check", &f, exists);
                assert_eq!(d.event["failure"]["class"], class.as_str());
                assert_eq!(d.event["container"], "c");
                assert_eq!(d.event["command"], "check");
                assert_eq!(d.attention, !exists);
                let text = d.annotation.unwrap();
                for wanted in [
                    "container の cgroup で OOM の事態が起きた",
                    "--memory 456m",
                    "machine 全体のメモリ不足",
                    "789 MiB",
                    "取りこぼし",
                ] {
                    assert!(text.contains(wanted));
                }
                for forbidden in ["殺された", "OOM kill"] {
                    assert!(!text.contains(forbidden));
                }
            }
        }
        let event = turn(&o).unwrap();
        assert_eq!(event["container"], "c");
        assert_eq!(event["oom"], "observed");
        assert!(event["command"].is_null());
        assert!(event["failure"].is_null());
        assert!(
            event["fact"]
                .as_str()
                .unwrap()
                .contains("container の cgroup で OOM の事態が起きた")
        );
        assert!(
            event["possible_causes"]
                .as_str()
                .unwrap()
                .contains("--memory 456m")
        );
        for state in [OomState::Absent, OomState::Unreadable("no event".into())] {
            o.state = state;
            let d = verification(&o, "check", &failure, false);
            assert!(!d.attention);
            assert!(d.annotation.is_none());
            assert!(turn(&o).is_none());
        }
        assert_eq!(
            verification(&o, "check", &failure, false).event["oom"],
            "unreadable"
        );
        o.state = OomState::Observed;
        o.machine_memory_mib = None;
        assert!(o.causes().contains("メモリの値は読めない"));
    }
    #[test]
    fn only_failed_commands_that_started_have_a_read() {
        for code in [Some(0), Some(1), Some(101), Some(137), None] {
            assert!(!should_read(true, code));
        }
        assert!(!should_read(false, Some(125)));
        assert!(should_read(false, Some(1)));
        assert!(should_read(false, Some(137)));
    }
    #[test]
    fn diagnostics_leave_the_original_log_classification_unchanged() {
        use crate::domain::{toolchain::Toolchains, verify_failure::classify};
        let profiles = Toolchains::this_repository();
        for (code, output, expected) in [
            (
                1,
                "test example ... FAILED\ntest result: FAILED. 0 passed; 1 failed",
                FailureClass::TestFailure,
            ),
            (137, "", FailureClass::Killed),
            (
                101,
                "error[E0308]: mismatched types",
                FailureClass::BuildError,
            ),
        ] {
            for state in [OomState::Observed, OomState::Absent] {
                let failure = classify(&profiles, "cargo test", Some(code), None, output);
                assert_eq!(failure.class, expected);
                let observation = OomObservation {
                    state,
                    container: "c".into(),
                    memory_mib: 1024,
                    machine_memory_mib: None,
                };
                let diagnostic = verification(&observation, "cargo test", &failure, false);
                assert_eq!(diagnostic.event["failure"]["class"], expected.as_str());
                assert_eq!(failure.class, expected);
            }
        }
    }
}
