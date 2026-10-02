//! The KPIs of the reviews and the workers' questions: send-backs,
//! question waits and goal reviews.

use super::*;

/// The review verdicts that sent runs back per primary code (ADR-t947-1
/// decision 5): `review.sendback_rate` over the runs reviewed, all of
/// them, per `code=` and per `change=` (`unknown` without one).
#[test]
fn review_sendback_rate_is_split_by_code_and_change() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY + 10 * HOUR;
    let fix = Some("fix".parse::<TaskChange>().unwrap());
    let runs = [
        (1, fix.clone(), Some("adr_conflict")),
        (2, fix, None),
        (3, None, Some("test_gap")),
    ];
    for (task, change, code) in runs {
        let claimed = tuesday + task * HOUR;
        queue.run(&Run::new(task, change, claimed, 600));
        let id = format!("{task:08x}-0000-4000-8000-{claimed:012x}");
        let payload = match code {
            Some(code) => json!({"verdict": "concern", "reasons": ["x"],
                                 "reason_codes": [[code]], "primary_code": code}),
            None => json!({"verdict": "pass", "reasons": []}),
        };
        queue.push(
            Some(task),
            Some(&id),
            "review_finished",
            payload,
            claimed + 620,
        );
    }
    let kpi = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            ..KpiQuery::default()
        },
    );
    let day = &kpi.periods[0];
    let value = |stratum: &str| {
        let measure = measure(day, "review.sendback_rate", stratum);
        (measure.n, measure.value)
    };
    assert_eq!(value(ALL), (3, Some(0.667)));
    assert_eq!(value("code=adr_conflict"), (3, Some(0.333)));
    assert_eq!(value("code=test_gap"), (3, Some(0.333)));
    assert_eq!(value("change=fix"), (2, Some(0.5)));
    assert_eq!(value("change=unknown"), (1, Some(1.0)));
    assert_eq!(direction("review.sendback_rate"), Some(Direction::Lower));
    let review = &day.window.details["review_reasons"]["review"];
    assert_eq!(review["sent_back"], 2);
    assert_eq!(review["by_change"][0]["change"], "fix");
    assert_eq!(review["by_change"][0]["by_code"]["adr_conflict"], 1);
    assert_eq!(review["by_change"][1]["change"], Value::Null);
}

/// A person's answers to the workers' questions per primary topic
/// (ADR-t947-2): `ask.worker_question_wait` by `topic=`, by when the ask
/// was opened (`at=night` from 22:00 to 07:00 of the host's day,
/// `at=day`), and `stats`' `worker_question_topics` in the details.
#[test]
fn the_worker_question_waits_are_split_by_topic_and_night() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY;
    for (ask, topics, opened, waited) in [
        (
            1,
            json!(["adr_conflict", "task_overlap"]),
            tuesday + 23 * HOUR,
            600,
        ),
        (2, json!(["adr_conflict"]), tuesday + 12 * HOUR, 120),
        (3, Value::Null, tuesday + 13 * HOUR, 60),
    ] {
        let mut payload = json!({"ask_id": ask, "kind": "worker_question",
            "reason_category": "scope"});
        if !topics.is_null() {
            payload["topics"] = topics;
        }
        queue.push(Some(1), Some("r1"), "ask_opened", payload, opened);
        queue.push(
            Some(1),
            Some("r1"),
            "ask_answered",
            json!({"ask_id": ask, "kind": "worker_question", "answered_by": "inbox"}),
            opened + waited,
        );
    }
    let query = KpiQuery {
        last: 3,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 2 * DAY + HOUR, &KpiConfig::default(), &query);
    let second = &result.periods[1];
    let wait = |stratum: &str| {
        let measure = measure(second, "ask.worker_question_wait", stratum);
        (measure.n, measure.max)
    };
    assert_eq!(wait(ALL), (3, Some(600.0)));
    assert_eq!(wait("topic=adr_conflict"), (2, Some(600.0)));
    assert_eq!(wait("topic=unlabeled"), (1, Some(60.0)));
    assert_eq!(wait("at=night"), (1, Some(600.0)));
    assert_eq!(wait("at=day"), (2, Some(120.0)));
    let details = &second.window.details["worker_question_topics"];
    assert_eq!(details["asks"], 3);
    assert_eq!(details["codes"]["task_overlap"], 1);
    assert_eq!(details["by_topic"]["adr_conflict"]["night"]["total"], 600);
    assert_eq!(
        measure(&result.periods[0], "ask.worker_question_wait", ALL).n,
        0
    );
}

/// The headless jobs per kind (goal 73), split by the provider they ran on
/// (`claude` for a start that named none) and the model their session
/// used: Claude's and Codex's goal reviews side by side, how many, the
/// share failed, how long they took and the share of each verdict.
#[test]
fn the_goal_reviews_of_claude_and_codex_compare_by_provider_and_model() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY + 10 * HOUR;
    let goal_event = |queue: &mut Queue, kind: &str, payload: Value, secs: i64| {
        queue.queue_event(kind, payload, secs);
        queue.events.last_mut().unwrap().goal_id = Some(GoalId::new(9));
    };
    let reviews = [
        // (id, provider, model, verdict or failed, seconds)
        (1, None, None, Some("achieved"), 100),
        (
            2,
            Some("claude"),
            Some("claude-opus-5-5"),
            Some("gaps"),
            300,
        ),
        (3, Some("codex"), Some("gpt-5.5"), Some("achieved"), 60),
        (4, Some("codex"), Some("gpt-5.5"), None, 20),
        (5, Some("codex"), None, Some("gaps"), 80),
    ];
    for (id, provider, model, verdict, secs) in reviews {
        let at = tuesday + id * HOUR;
        let session = format!("s-{id}");
        let mut started = json!({"goal_review_id": id, "session_id": session});
        if let Some(provider) = provider {
            started["launch"] = json!({"role": "goal_review", "provider": provider});
        }
        goal_event(&mut queue, "goal_review_started", started, at);
        match verdict {
            Some(verdict) => goal_event(
                &mut queue,
                "goal_review_finished",
                json!({"goal_review_id": id, "verdict": verdict, "duration_secs": secs}),
                at + secs,
            ),
            None => goal_event(
                &mut queue,
                "goal_review_failed",
                json!({"goal_review_id": id, "duration_secs": secs}),
                at + secs,
            ),
        }
        if let Some(model) = model {
            queue.queue_event(
                "session_closed",
                json!({"kind": "goal_review", "session_id": session, "model": model}),
                at + secs,
            );
        }
    }
    let kpi = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            ..KpiQuery::default()
        },
    );
    let day = &kpi.periods[0];
    let count = |stratum: &str| measure(day, "job.count.goal_review", stratum).value;
    assert_eq!(count(ALL), Some(5.0));
    assert_eq!(count("provider=claude"), Some(2.0));
    assert_eq!(count("provider=codex"), Some(3.0));
    assert_eq!(count("model=gpt-5.5"), Some(2.0));
    assert_eq!(count("model=unknown"), Some(2.0));
    let failed = |stratum: &str| {
        let measure = measure(day, "job.failed_rate.goal_review", stratum);
        (measure.n, measure.value)
    };
    assert_eq!(failed("provider=claude"), (2, Some(0.0)));
    assert_eq!(failed("provider=codex"), (3, Some(0.333)));
    let secs = |stratum: &str| {
        let measure = measure(day, "job.secs.goal_review", stratum);
        (measure.median, measure.max)
    };
    assert_eq!(secs("provider=claude"), (Some(200.0), Some(300.0)));
    assert_eq!(secs("provider=codex"), (Some(60.0), Some(80.0)));
    // The share of the jobs that gave a verdict.
    let verdict = |name: &str, stratum: &str| {
        let measure = measure(day, name, stratum);
        (measure.n, measure.value)
    };
    assert_eq!(
        verdict("job.verdict.goal_review.achieved", "provider=claude"),
        (2, Some(0.5))
    );
    assert_eq!(
        verdict("job.verdict.goal_review.achieved", "provider=codex"),
        (2, Some(0.5))
    );
    assert_eq!(
        verdict("job.verdict.goal_review.gaps", "model=claude-opus-5-5"),
        (1, Some(1.0))
    );
    // Every kind is listed, with no job too.
    assert_eq!(measure(day, "job.count.review", ALL).value, Some(0.0));
    assert_eq!(direction("job.count.goal_review"), None);
    assert_eq!(direction("job.verdict.goal_review.gaps"), None);
    assert_eq!(
        direction("job.failed_rate.goal_review"),
        Some(Direction::Lower)
    );
    let details = &day.window.details["jobs"]["goal_review"];
    assert_eq!(details["by_provider"]["codex"]["failed"], 1);
    assert_eq!(details["by_provider"]["claude"]["verdicts"]["gaps"], 1);
    // A goal counts its own goal reviews only.
    let other = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            goal_id: Some(GoalId::new(8)),
            ..KpiQuery::default()
        },
    );
    assert_eq!(
        measure(&other.periods[0], "job.count.goal_review", ALL).value,
        Some(0.0)
    );
}
