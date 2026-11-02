//! The trial of the provider pair (ADR-t4005-1): which provider writes a
//! task (its worker) and which reviews its run. Off unless a person turns
//! it on in `dagq.toml` ([`ProviderTrial`]). When on, the hash of the
//! trial's name and the task's ID draws each subject into the control (A:
//! the worker on the task's provider, the review as configured) or the
//! treatment (B: the worker on `treatment_worker`, the review on
//! `treatment_review`), with the draw every trial shares
//! ([`worker_model::draw`]), so this trial and the model and effort trial
//! draw independently. A task that had a group keeps it in its later runs.
//!
//! A task that names a provider other than the default, a claim of a worker
//! that is not headless and a Spike are outside the trial: their claim
//! records no group, and why ([`PairOutside`]). A treatment's claim is
//! outside the model and effort trial (decision 4).
//!
//! The claim records the choice in `run_claimed` ([`PairChoice::fields`]),
//! whose open set of attributes the ledger and `kpi` read; the review reads
//! a run's back with [`RunPair::of_run`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{
    DomainError, EventKind, Provider, RunEvent, TaskId,
    execution_class::ExecutionClass,
    task_hold::Released,
    worker::{Worker, WorkerMode},
    worker_model::{self, TrialGroup},
};

/// The fields of `run_claimed` a claim under the trial records: the group
/// (null outside it), the trial's name, the provider of the worker and of
/// the review it assigned, and why the task is outside.
pub const GROUP: &str = "pair_group";
pub const TRIAL: &str = "pair_trial";
pub const WORKER: &str = "pair_worker";
pub const REVIEW: &str = "pair_review";
pub const OUTSIDE: &str = "pair_outside";

/// `[worker.provider_trial]` of `dagq.toml` (ADR-t4005-1): off unless a
/// person turns it on. Its keys:
///
/// - `enabled`: whether the trial assigns groups; `false` by default.
/// - `name`: the trial's name, hashed with a task's ID to draw its group and
///   recorded next to it; `provider-pair` by default. Once the trial is
///   on, it must differ from the name of `[worker.trial]`, so the two
///   trials draw independently.
///   Renaming it redraws only the tasks without a group yet.
/// - `treatment_percent`: the share of the subjects drawn into the
///   treatment, a whole percent from 0 to 100; 10 by default.
/// - `treatment_worker`: the provider the treatment's worker is claimed
///   on; `codex` by default.
/// - `treatment_review`: the provider the treatment's run is reviewed on;
///   `claude` by default. It must differ from `treatment_worker`
///   ([`Self::check`]), so another provider reviews what one wrote.
///
/// The control's worker runs on the task's provider and its review as
/// configured. A treatment's worker falls back as any worker does
/// (`[provider_fallback]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderTrial {
    pub enabled: bool,
    pub name: String,
    pub treatment_percent: u8,
    pub treatment_worker: Provider,
    pub treatment_review: Provider,
}

impl Default for ProviderTrial {
    fn default() -> Self {
        Self {
            enabled: false,
            name: "provider-pair".to_owned(),
            treatment_percent: 10,
            treatment_worker: Provider::Codex,
            treatment_review: Provider::Claude,
        }
    }
}

string_enum!(PairOutside {
    /// The task names a provider other than the default one.
    Provider => "task_provider",
    /// The task's worker is not headless.
    NotHeadless => "not_headless",
    /// A Spike (execution class `spike`).
    Spike => "spike",
    /// A person chose the task's provider when releasing its hold.
    Released => "released_provider",
});

impl ProviderTrial {
    pub const KEYS: [&'static str; 5] = [
        "enabled",
        "name",
        "treatment_percent",
        "treatment_worker",
        "treatment_review",
    ];

    /// Why the trial cannot be taken: a percent above 100, a blank name, or
    /// the treatment's worker and review on the same provider.
    pub fn check(&self) -> Result<(), String> {
        if self.treatment_percent > 100 {
            return Err(format!(
                "treatment_percent {} is not between 0 and 100",
                self.treatment_percent
            ));
        }
        if self.name.trim().is_empty() {
            return Err("name is blank".to_owned());
        }
        if self.treatment_worker == self.treatment_review {
            return Err(format!(
                "treatment_worker and treatment_review are both {}: another provider must review what one wrote",
                self.treatment_worker.as_str()
            ));
        }
        Ok(())
    }

    /// The group `task` is drawn into ([`worker_model::draw`]).
    pub fn draw(&self, task: TaskId) -> TrialGroup {
        worker_model::draw(&self.name, task, self.treatment_percent)
    }

    /// The pair `task` is claimed with now, `worker` being the worker its
    /// task asks for and `class` its execution class; `None` with the trial
    /// off, which claims as before. `claims` are the task's `run_claimed`
    /// and `task_released` (others are ignored), oldest first. A release
    /// that chose a provider takes the task out of the trial: a person's
    /// choice is kept over any group. Otherwise a task that had a group keeps
    /// it, and the name it was drawn in, whatever the share or the name now
    /// (ADR-t4005-1 decision 2), as a resume, a revise and a retry claim
    /// again. Otherwise a task outside the trial gets no group and why,
    /// and a subject is [`Self::draw`]n.
    pub fn choose(
        &self,
        task: TaskId,
        worker: Worker,
        class: ExecutionClass,
        claims: &[RunEvent],
    ) -> Option<PairChoice> {
        if !self.enabled {
            return None;
        }
        let outside = |outside| PairChoice {
            group: None,
            trial: self.name.clone(),
            outside: Some(outside),
            worker: worker.provider,
            review: None,
        };
        if claims.iter().any(|event| {
            event.task_id == Some(task)
                && Released::of_event(event).is_some_and(|released| released.provider.is_some())
        }) {
            return Some(outside(PairOutside::Released));
        }
        let kept = claims
            .iter()
            .filter(|event| event.kind == EventKind::RunClaimed && event.task_id == Some(task))
            .find_map(|event| {
                let group = group_of(&event.payload)?;
                let name = event.payload[TRIAL].as_str().map(str::to_owned);
                Some((group, name))
            });
        if let Some((group, name)) = kept {
            return Some(self.assigned(group, name.unwrap_or_else(|| self.name.clone()), worker));
        }
        let why = if worker.provider != Worker::DEFAULT.provider {
            Some(PairOutside::Provider)
        } else if worker.mode != WorkerMode::Headless {
            Some(PairOutside::NotHeadless)
        } else if class.is_spike() {
            Some(PairOutside::Spike)
        } else {
            None
        };
        if let Some(why) = why {
            return Some(outside(why));
        }
        Some(self.assigned(self.draw(task), self.name.clone(), worker))
    }

    /// The worker a claim of `task` requests, `worker` being the one its
    /// task asks for: the pair's ([`Self::choose`], [`PairChoice::requested`])
    /// with the trial on, `worker` otherwise. The claim and the deferrals
    /// before it judge the task by it.
    pub fn requested(
        &self,
        task: TaskId,
        worker: Worker,
        class: ExecutionClass,
        events: &[RunEvent],
    ) -> Worker {
        self.choose(task, worker, class, events)
            .map_or(worker, |choice| choice.requested(worker))
    }

    fn assigned(&self, group: TrialGroup, trial: String, worker: Worker) -> PairChoice {
        let (worker, review) = match group {
            TrialGroup::Control => (worker.provider, None),
            TrialGroup::Treatment => (self.treatment_worker, Some(self.treatment_review)),
        };
        PairChoice {
            group: Some(group),
            trial,
            outside: None,
            worker,
            review,
        }
    }
}

/// The pair a claim chose for its task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairChoice {
    /// `None` outside the trial.
    pub group: Option<TrialGroup>,
    /// The name of the trial the group was drawn in.
    pub trial: String,
    /// Why the task is outside the trial.
    pub outside: Option<PairOutside>,
    /// The provider the worker is claimed on (before any fallback).
    pub worker: Provider,
    /// The provider the run is reviewed on; `None` for the review as
    /// configured (the control's, and outside the trial).
    pub review: Option<Provider>,
}

impl PairChoice {
    /// Whether the claim is the treatment's: outside the model and effort
    /// trial (ADR-t4005-1 decision 4).
    pub fn is_treatment(&self) -> bool {
        self.group == Some(TrialGroup::Treatment)
    }

    /// The worker the claim requests for a task that asks for `worker`: the
    /// assigned provider, headless when it is not the task's.
    pub fn requested(&self, worker: Worker) -> Worker {
        if self.worker == worker.provider {
            worker
        } else {
            Worker {
                provider: self.worker,
                mode: WorkerMode::Headless,
            }
        }
    }

    /// The fields of `run_claimed` that record the choice ([`GROUP`] and
    /// the rest): the group (null outside), the trial's name, and either
    /// the assigned worker and review (none for the review as configured)
    /// or why the task is outside.
    pub fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert(GROUP.to_owned(), json!(self.group));
        fields.insert(TRIAL.to_owned(), json!(self.trial));
        match self.outside {
            Some(outside) => {
                fields.insert(OUTSIDE.to_owned(), json!(outside));
            }
            None => {
                fields.insert(WORKER.to_owned(), json!(self.worker));
                if let Some(review) = self.review {
                    fields.insert(REVIEW.to_owned(), json!(review));
                }
            }
        }
        fields
    }
}

/// A run's pair, as its claim recorded it: what the review's launch reads
/// to follow the run's group (ADR-t4005-1 decision 5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunPair {
    /// `None` outside the trial, or for a run claimed with it off.
    pub group: Option<TrialGroup>,
    pub trial: Option<String>,
    /// The provider the worker was assigned (before any fallback).
    pub worker: Option<Provider>,
    /// The provider the run's review is assigned; `None` for the review as
    /// configured.
    pub review: Option<Provider>,
}

impl RunPair {
    /// The pair of the run whose events are `events`: from its first
    /// `run_claimed`; the default (no group, the review as configured)
    /// without one or for a claim that recorded none.
    pub fn of_run(events: &[RunEvent]) -> Self {
        let Some(claimed) = events
            .iter()
            .find(|event| event.kind == EventKind::RunClaimed)
        else {
            return Self::default();
        };
        let payload = &claimed.payload;
        let provider = |field: &str| payload[field].as_str().and_then(|text| text.parse().ok());
        Self {
            group: group_of(payload),
            trial: payload[TRIAL].as_str().map(str::to_owned),
            worker: provider(WORKER),
            review: provider(REVIEW),
        }
    }
}

fn group_of(payload: &Value) -> Option<TrialGroup> {
    payload[GROUP].as_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn on() -> ProviderTrial {
        ProviderTrial {
            enabled: true,
            ..ProviderTrial::default()
        }
    }

    fn claimed(id: i64, task: i64, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: "run_claimed".into(),
            payload,
            created_at: "2027-01-15T08:00:00.000Z".to_owned(),
            actor: None,
        }
    }

    const CODEX: Worker = Worker {
        provider: Provider::Codex,
        mode: WorkerMode::Headless,
    };

    /// The first task from 1 that `trial` draws into `group`.
    fn drawn(trial: &ProviderTrial, group: TrialGroup) -> i64 {
        (1..)
            .find(|&id| trial.draw(TaskId::new(id)) == group)
            .unwrap()
    }

    #[test]
    fn the_default_is_off_and_the_check_refuses_a_bad_share_or_one_provider() {
        let default = ProviderTrial::default();
        assert!(!default.enabled);
        assert_eq!(default.check(), Ok(()));
        assert_eq!(
            (
                default.treatment_worker,
                default.treatment_review,
                default.treatment_percent
            ),
            (Provider::Codex, Provider::Claude, 10)
        );
        // Off, the claim is as before: no choice at all.
        assert_eq!(
            default.choose(
                TaskId::new(1),
                Worker::DEFAULT,
                ExecutionClass::default(),
                &[]
            ),
            None
        );
        let over = ProviderTrial {
            treatment_percent: 101,
            ..on()
        };
        assert!(over.check().unwrap_err().contains("between 0 and 100"));
        let same = ProviderTrial {
            treatment_review: Provider::Codex,
            ..on()
        };
        assert!(same.check().unwrap_err().contains("both codex"));
        let blank = ProviderTrial {
            name: " ".into(),
            ..on()
        };
        assert!(blank.check().unwrap_err().contains("blank"));
    }

    #[test]
    fn the_draw_is_deterministic_near_the_share_and_independent_of_the_model_trial() {
        let trial = on();
        let model = worker_model::WorkerTrial {
            treatment_percent: 10,
            ..worker_model::WorkerTrial::default()
        };
        let tasks = 1..=4000;
        let mut treatment = 0;
        let mut both = 0;
        let mut model_treatment = 0;
        for id in tasks.clone() {
            let task = TaskId::new(id);
            assert_eq!(trial.draw(task), trial.draw(task));
            let pair = trial.draw(task) == TrialGroup::Treatment;
            let other = model.draw(task) == TrialGroup::Treatment;
            treatment += usize::from(pair);
            model_treatment += usize::from(other);
            both += usize::from(pair && other);
        }
        // 10% of 4000 is 400.
        assert!((330..=470).contains(&treatment), "{treatment}");
        assert!((330..=470).contains(&model_treatment), "{model_treatment}");
        // Independent: about 1% of the tasks are in both treatments, not
        // the 10% one draw shared by both would give.
        assert!((15..=70).contains(&both), "{both}");
        // The same name and share draw as the model trial does.
        let same = worker_model::WorkerTrial {
            name: trial.name.clone(),
            treatment_percent: 10,
            ..worker_model::WorkerTrial::default()
        };
        assert!(
            tasks
                .map(TaskId::new)
                .all(|task| same.draw(task) == trial.draw(task))
        );
    }

    #[test]
    fn a_treatment_task_is_claimed_on_codex_and_reviewed_on_claude() {
        let trial = on();
        let (a, b) = (
            drawn(&trial, TrialGroup::Control),
            drawn(&trial, TrialGroup::Treatment),
        );
        let class = ExecutionClass::default();
        let control = trial
            .choose(TaskId::new(a), Worker::DEFAULT, class, &[])
            .unwrap();
        assert_eq!(control.group, Some(TrialGroup::Control));
        assert_eq!(control.requested(Worker::DEFAULT), Worker::DEFAULT);
        assert!(!control.is_treatment());
        assert_eq!(
            Value::Object(control.fields()),
            json!({"pair_group": "control", "pair_trial": "provider-pair", "pair_worker": "claude"})
        );
        let treatment = trial
            .choose(TaskId::new(b), Worker::DEFAULT, class, &[])
            .unwrap();
        assert!(treatment.is_treatment());
        assert_eq!(treatment.requested(Worker::DEFAULT), CODEX);
        // What the claim and the deferrals before it run each task as.
        assert_eq!(
            trial.requested(TaskId::new(b), Worker::DEFAULT, class, &[]),
            CODEX
        );
        assert_eq!(
            trial.requested(TaskId::new(a), Worker::DEFAULT, class, &[]),
            Worker::DEFAULT
        );
        assert_eq!(
            ProviderTrial::default().requested(TaskId::new(b), Worker::DEFAULT, class, &[]),
            Worker::DEFAULT
        );
        assert_eq!(
            Value::Object(treatment.fields()),
            json!({"pair_group": "treatment", "pair_trial": "provider-pair", "pair_worker": "codex", "pair_review": "claude"})
        );
    }

    #[test]
    fn a_task_keeps_its_group_in_its_later_runs() {
        let trial = on();
        let b = drawn(&trial, TrialGroup::Treatment);
        let first = trial
            .choose(
                TaskId::new(b),
                Worker::DEFAULT,
                ExecutionClass::default(),
                &[],
            )
            .unwrap();
        let mut claims = vec![claimed(1, b, Value::Object(first.fields()))];
        // Another task's claim is not this one's.
        claims.insert(0, claimed(0, b + 1, json!({"pair_group": "control"})));
        // A retry, a resume or a revise claims again after the share, the
        // name and the task's class changed: the group and its name stay.
        let moved = ProviderTrial {
            treatment_percent: 0,
            name: "renamed".into(),
            ..trial.clone()
        };
        let again = moved
            .choose(
                TaskId::new(b),
                Worker::DEFAULT,
                ExecutionClass::Spike,
                &claims,
            )
            .unwrap();
        assert_eq!(again, first);
        // A control task stays the control at a share of 100.
        let a = drawn(&trial, TrialGroup::Control);
        let all = ProviderTrial {
            treatment_percent: 100,
            ..trial.clone()
        };
        let kept = [claimed(2, a, json!({"pair_group": "control"}))];
        let control = all
            .choose(
                TaskId::new(a),
                Worker::DEFAULT,
                ExecutionClass::default(),
                &kept,
            )
            .unwrap();
        assert_eq!(control.group, Some(TrialGroup::Control));
        // Recorded without a name: the current one.
        assert_eq!(control.trial, "provider-pair");
        // A claim outside the trial keeps no group.
        let outside = [claimed(
            3,
            a,
            json!({"pair_group": null, "pair_outside": "spike"}),
        )];
        assert_eq!(
            all.choose(
                TaskId::new(a),
                Worker::DEFAULT,
                ExecutionClass::default(),
                &outside
            )
            .unwrap()
            .group,
            Some(TrialGroup::Treatment)
        );
    }

    #[test]
    fn a_task_on_another_provider_a_worker_not_headless_and_a_spike_are_outside() {
        let trial = ProviderTrial {
            treatment_percent: 100,
            ..on()
        };
        let task = TaskId::new(1);
        let class = ExecutionClass::default();
        for (worker, class, why) in [
            (CODEX, class, PairOutside::Provider),
            (Worker::CLAUDE_INTERACTIVE, class, PairOutside::NotHeadless),
            (Worker::DEFAULT, ExecutionClass::Spike, PairOutside::Spike),
        ] {
            let choice = trial.choose(task, worker, class, &[]).unwrap();
            assert_eq!(choice.group, None);
            assert_eq!(choice.outside, Some(why));
            assert_eq!(choice.requested(worker), worker);
            assert!(!choice.is_treatment());
            assert_eq!(
                Value::Object(choice.fields()),
                json!({"pair_group": null, "pair_trial": "provider-pair", "pair_outside": why.as_str()})
            );
        }
    }

    #[test]
    fn a_release_that_chose_a_provider_takes_the_task_out() {
        let trial = on();
        let b = drawn(&trial, TrialGroup::Treatment);
        let first = trial
            .choose(
                TaskId::new(b),
                Worker::DEFAULT,
                ExecutionClass::default(),
                &[],
            )
            .unwrap();
        let mut released = claimed(2, b, json!({"provider": "claude"}));
        released.kind = "task_released".into();
        let mut no_provider = claimed(3, b, json!({}));
        no_provider.kind = "task_released".into();
        let kept = [claimed(1, b, Value::Object(first.fields())), no_provider];
        assert_eq!(
            trial.choose(
                TaskId::new(b),
                Worker::DEFAULT,
                ExecutionClass::default(),
                &kept
            ),
            Some(first)
        );
        let events = [kept[0].clone(), released];
        let choice = trial
            .choose(
                TaskId::new(b),
                Worker::DEFAULT,
                ExecutionClass::default(),
                &events,
            )
            .unwrap();
        assert_eq!(
            (choice.group, choice.outside),
            (None, Some(PairOutside::Released))
        );
        assert_eq!(choice.requested(Worker::DEFAULT), Worker::DEFAULT);
    }

    #[test]
    fn the_run_pair_reads_the_group_and_the_assigned_providers_of_the_claim() {
        let treatment = claimed(
            1,
            1,
            json!({"pair_group": "treatment", "pair_trial": "p", "pair_worker": "codex", "pair_review": "claude"}),
        );
        let later = claimed(2, 1, json!({"pair_group": "control"}));
        assert_eq!(
            RunPair::of_run(&[treatment, later]),
            RunPair {
                group: Some(TrialGroup::Treatment),
                trial: Some("p".into()),
                worker: Some(Provider::Codex),
                review: Some(Provider::Claude),
            }
        );
        let control = claimed(
            1,
            1,
            json!({"pair_group": "control", "pair_trial": "p", "pair_worker": "claude"}),
        );
        let pair = RunPair::of_run(&[control]);
        assert_eq!(
            (pair.group, pair.worker, pair.review),
            (Some(TrialGroup::Control), Some(Provider::Claude), None)
        );
        // Claimed with the trial off, or no claim: the review as configured.
        assert_eq!(
            RunPair::of_run(&[claimed(1, 1, json!({"group": "treatment"}))]),
            RunPair::default()
        );
        assert_eq!(RunPair::of_run(&[]), RunPair::default());
    }
}
