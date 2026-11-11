use super::test_support::{APPS, MAIN_APP, RUNNER_APP, read, reference};
use super::*;
use crate::domain::{EventId, RunEvent};

fn missing(result: Result<(), Unverified>) -> Vec<(Prohibition, &'static str)> {
    match result {
        Err(Unverified::Missing(m)) => m.iter().map(|m| (m.prohibition, m.rule)).collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn all_six_prohibitions_verify_and_each_missing_one_is_named_with_the_values_read() {
    assert_eq!(judge(&read(&reference()), APPS), Ok(()));
    // A rule taken out.
    let mut rules = reference();
    rules[1]["rules"] = json!([{"type": "deletion"}]);
    let reason = judge(&read(&rules), APPS).unwrap_err();
    assert_eq!(
        missing(Err(reason.clone())),
        [(Prohibition::R2, "non_fast_forward")]
    );
    // The rulesets read are named even when none has the rule: R2's own,
    // with the rules left on it.
    let message = reason.message();
    assert!(message.contains("\"id\":2"), "{message}");
    assert!(message.contains("\"rules\":[\"deletion\"]"), "{message}");
    assert!(message.contains("does not have the rule"), "{message}");
    // Each prohibition's whole ruleset taken out.
    for (index, prohibition) in Prohibition::ALL.iter().enumerate() {
        let mut rulesets = reference();
        rulesets.remove(index);
        let names = missing(judge(&read(&rulesets), APPS));
        assert!(names.iter().all(|(p, _)| p == prohibition), "{names:?}");
        assert!(!names.is_empty());
    }
    // A target ref that differs, a target kind that differs, an exclusion
    // that cuts into the covered refs.
    let cases: [(usize, &str, Value, Prohibition); 4] = [
        (
            0,
            "/conditions/ref_name/include",
            json!(["refs/heads/master"]),
            Prohibition::R1,
        ),
        (3, "/target", json!("branch"), Prohibition::R4),
        (
            2,
            "/conditions/ref_name/exclude",
            json!(["refs/heads/**"]),
            Prohibition::R3,
        ),
        (
            4,
            "/conditions/ref_name/include",
            json!([RUN_NAMESPACE]),
            Prohibition::R5,
        ),
    ];
    for (index, pointer, value, prohibition) in cases {
        let mut rulesets = reference();
        *rulesets[index].pointer_mut(pointer).unwrap() = value.clone();
        let result = judge(&read(&rulesets), APPS);
        let reason = result.clone().unwrap_err();
        assert!(
            missing(result).iter().all(|(p, _)| *p == prohibition),
            "{pointer}"
        );
        // The read values are in the reason.
        let message = reason.message();
        assert!(message.contains(&value.to_string()), "{message}");
        assert!(
            message.contains(&format!("\"id\":{}", index + 1)),
            "{message}"
        );
    }
    // Enforcement other than active.
    for enforcement in [json!("evaluate"), json!("disabled"), Value::Null] {
        let mut rulesets = reference();
        rulesets[5]["enforcement"] = enforcement;
        let reason = judge(&read(&rulesets), APPS).unwrap_err();
        assert_eq!(
            missing(Err(reason.clone())),
            [(Prohibition::R6, "deletion")]
        );
        assert!(reason.message().contains("enforcement is not active"));
        assert_eq!(reason.code(), "ruleset_prohibition_missing");
    }
    // An update that takes the upstream's changes is not the prohibition.
    let mut rulesets = reference();
    rulesets[0]["rules"] =
        json!([{"type": "update", "parameters": {"update_allows_fetch_and_merge": true}}]);
    let reason = judge(&read(&rulesets), APPS).unwrap_err();
    assert_eq!(missing(Err(reason.clone())), [(Prohibition::R1, "update")]);
    let message = reason.message();
    assert!(
        message.contains("update allows fetch and merge"),
        "{message}"
    );
    assert!(
        message.contains("\"parameters\":{\"update_allows_fetch_and_merge\":true}"),
        "{message}"
    );
    rulesets[0]["rules"] =
        json!([{"type": "update", "parameters": {"update_allows_fetch_and_merge": false}}]);
    assert_eq!(judge(&read(&rulesets), APPS), Ok(()));
}

#[test]
fn rules_may_be_held_by_any_active_ruleset_and_all_refs_by_all() {
    // R5 and R6 in one ruleset over every branch but main, R2 split in two.
    let mut rulesets = reference();
    rulesets.truncate(4);
    rulesets[1]["rules"] = json!([{"type": "deletion"}]);
    rulesets.push(json!({
        "id": 9, "target": "branch", "enforcement": "active", "current_user_can_bypass": "never",
        "conditions": {"ref_name": {"include": [ALL], "exclude": [MAIN]}},
        "rules": [{"type": "non_fast_forward"}, {"type": "deletion"}],
        "bypass_actors": [],
    }));
    rulesets.push(json!({
        "id": 10, "target": "branch", "enforcement": "active", "current_user_can_bypass": "never",
        "conditions": {"ref_name": {"include": [MAIN]}},
        "rules": [{"type": "non_fast_forward"}],
        "bypass_actors": [],
    }));
    assert_eq!(judge(&read(&rulesets), APPS), Ok(()));
}

#[test]
fn a_reason_names_a_bounded_number_of_patterns() {
    let mut rulesets = reference();
    rulesets[0]["conditions"]["ref_name"]["include"] = json!(
        (0..1000)
            .map(|n| format!("refs/heads/x{n}"))
            .collect::<Vec<_>>()
    );
    let message = judge(&read(&rulesets), APPS).unwrap_err().message();
    assert!(message.contains("refs/heads/x4"));
    assert!(!message.contains("refs/heads/x5\""), "{message}");
}

#[test]
fn names_never_change_the_result() {
    for name in [
        json!("dagq-main-update"),
        json!("renamed by a person"),
        Value::Null,
    ] {
        let with = |mut rulesets: Vec<Value>| {
            for ruleset in &mut rulesets {
                ruleset["name"] = name.clone();
            }
            judge(&read(&rulesets), APPS)
        };
        assert_eq!(with(reference()), Ok(()));
        let mut short = reference();
        short.remove(0);
        assert_eq!(missing(with(short)), [(Prohibition::R1, "update")]);
    }
}

#[test]
fn a_ruleset_the_runner_app_may_bypass_holds_nothing() {
    // The reader's bypass: only `never` holds; absent or unknown does not.
    for value in [
        Some(json!("always")),
        Some(json!("pull_requests_only")),
        Some(json!("exempt")),
        Some(json!(3)),
        None,
    ] {
        let mut rulesets = reference();
        match &value {
            Some(value) => rulesets[2]["current_user_can_bypass"] = value.clone(),
            None => {
                rulesets[2]
                    .as_object_mut()
                    .unwrap()
                    .remove("current_user_can_bypass");
            }
        }
        let reason = judge(&read(&rulesets), APPS).unwrap_err();
        assert!(
            missing(Err(reason.clone()))
                .iter()
                .all(|(p, _)| *p == Prohibition::R3)
        );
        assert!(
            reason.message().contains("the runner App may bypass it"),
            "{value:?}"
        );
    }
    // The runner App among the bypass actors GitHub returned.
    let mut rulesets = reference();
    rulesets[3]["bypass_actors"] =
        json!([{"actor_type": "Integration", "actor_id": RUNNER_APP, "bypass_mode": "always"}]);
    let reason = judge(&read(&rulesets), APPS).unwrap_err();
    assert!(
        missing(Err(reason.clone()))
            .iter()
            .all(|(p, _)| *p == Prohibition::R4)
    );
    let message = reason.message();
    assert!(
        message.contains("the runner App is a bypass actor"),
        "{message}"
    );
    assert!(message.contains("\"actor_id\":11"), "{message}");
}

fn message_has(reason: &Unverified, text: &str) {
    let message = reason.message();
    assert!(message.contains(text), "{text} not in {message}");
}

/// Every actor's bypass is judged as ADR-t3987-1 decision 6 sets it, not
/// the runner App's alone.
#[test]
fn every_actor_keeps_to_the_bypass_of_its_prohibition() {
    let main =
        || json!({"actor_type": "Integration", "actor_id": MAIN_APP, "bypass_mode": "always"});
    let role = || json!({"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "always"});
    let runner = || json!({"actor_type": "Integration", "actor_id": RUNNER_APP});
    let key = || json!({"actor_type": "DeployKey", "actor_id": null});
    // (ruleset index, bypass_actors, held)
    let cases = [
        // R1 and R6: the main App alone, or nobody.
        (0, json!([main()]), true),
        (0, json!([]), true),
        (0, json!([main(), role()]), false),
        (0, json!([runner()]), false),
        (5, json!([key()]), false),
        (5, json!([main()]), true),
        // R2 and R5: nobody, the main App included.
        (1, json!([main()]), false),
        (4, json!([role()]), false),
        (4, json!([]), true),
        // R3 and R4: whom the project chooses, but never the runner App.
        (2, json!([role(), key(), main()]), true),
        (3, json!([role(), runner()]), false),
    ];
    for (index, actors, held) in cases {
        let mut rulesets = reference();
        rulesets[index]["bypass_actors"] = actors.clone();
        let result = judge(&read(&rulesets), APPS);
        assert_eq!(result.is_ok(), held, "ruleset {} with {actors}", index + 1);
        if let Err(reason) = result {
            assert!(
                missing(Err(reason.clone()))
                    .iter()
                    .all(|(p, _)| *p == Prohibition::ALL[index])
            );
            message_has(&reason, &format!("\"bypass_actors\":{actors}"));
        }
    }
    // With no main App named, R1 holds only with nobody to bypass it.
    let no_main = Apps {
        runner: RUNNER_APP,
        main: None,
    };
    assert!(judge(&read(&reference()), no_main).is_err());
    let mut rulesets = reference();
    rulesets[0]["bypass_actors"] = json!([]);
    rulesets[5]["bypass_actors"] = json!([]);
    assert_eq!(judge(&read(&rulesets), no_main), Ok(()));
    // A list not returned (a reader without write access) or unreadable
    // cannot be verified, so it holds nothing.
    let mut rulesets = reference();
    rulesets[1].as_object_mut().unwrap().remove("bypass_actors");
    let reason = judge(&read(&rulesets), APPS).unwrap_err();
    assert_eq!(missing(Err(reason.clone())).len(), 2);
    message_has(&reason, "bypass_actors is not returned");
    rulesets[1]["bypass_actors"] = json!("anything");
    message_has(
        &judge(&read(&rulesets), APPS).unwrap_err(),
        "bypass_actors cannot be read",
    );
}

#[test]
fn unreadable_or_unavailable_reads_are_unverified() {
    assert_eq!(judge(&Read::Unreadable, APPS), Err(Unverified::Unreadable));
    assert_eq!(
        judge(&Read::PlanUnavailable, APPS),
        Err(Unverified::PlanUnavailable)
    );
    // No ruleset at all is every prohibition missing.
    assert_eq!(missing(judge(&Read::Rulesets(vec![]), APPS)).len(), 11);
    for reason in [
        Unverified::NotConfigured,
        Unverified::Unreadable,
        Unverified::PlanUnavailable,
        Unverified::Stale,
    ] {
        assert!(!reason.message().is_empty());
        assert_eq!(reason.payload()["reason"], reason.code());
    }
}

#[test]
fn a_result_is_kept_until_its_period_ends() {
    assert!(reread_due(None, 1_000));
    assert!(!reread_due(Some((1_000, true)), 1_000 + VERIFIED_SECS - 1));
    assert!(reread_due(Some((1_000, true)), 1_000 + VERIFIED_SECS));
    assert!(!reread_due(
        Some((1_000, false)),
        1_000 + UNVERIFIED_SECS - 1
    ));
    assert!(reread_due(Some((1_000, false)), 1_000 + UNVERIFIED_SECS));
    assert!(
        reread_due(Some((1_000, true)), 999),
        "a clock gone back reads again"
    );
    // A kept result is used one more period past its own, then not.
    assert!(usable((1_000, true), 1_000 + 2 * VERIFIED_SECS - 1));
    assert!(!usable((1_000, true), 1_000 + 2 * VERIFIED_SECS));
    assert!(usable((1_000, false), 1_000 + 2 * UNVERIFIED_SECS - 1));
    assert!(!usable((1_000, false), 1_000 + 2 * UNVERIFIED_SECS));
    assert!(!usable((1_000, true), 999));
}

fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: None,
        goal_id: None,
        run_id: None,
        kind: kind.into(),
        payload,
        created_at: "2026-11-10T00:00:00Z".into(),
        actor: None,
    }
}

#[test]
fn the_hold_is_recorded_on_its_changes_and_asks_once_while_it_lasts() {
    let unreadable = Unverified::Unreadable;
    let start = transition(Err(&unreadable), None).unwrap();
    assert_eq!(start.kind, EventKind::RunnerRulesetsHeld);
    assert!(start.open_ask);
    assert_eq!(start.payload["reason"], "ruleset_unreadable");
    let held = event(1, start.kind, start.payload);
    assert_eq!(transition(Err(&unreadable), Some(&held)), None);
    // Another reason while held: recorded, but no second ask.
    let plan = Unverified::PlanUnavailable;
    let changed = transition(Err(&plan), Some(&held)).unwrap();
    assert_eq!(changed.kind, EventKind::RunnerRulesetsHeld);
    assert!(!changed.open_ask);
    let held = event(2, changed.kind, changed.payload);
    let resumed = transition(Ok(()), Some(&held)).unwrap();
    assert_eq!(resumed.kind, EventKind::RunnerRulesetsResumed);
    assert!(!resumed.open_ask);
    assert_eq!(resumed.payload["reason"], "ruleset_plan_unavailable");
    let after = event(3, resumed.kind, resumed.payload);
    assert_eq!(transition(Ok(()), Some(&after)), None);
    assert_eq!(transition(Ok(()), None), None);
    // A new hold after a resume asks again.
    assert!(transition(Err(&unreadable), Some(&after)).unwrap().open_ask);
    assert!(ask_question(&unreadable).contains("ruleset_unreadable"));
}
