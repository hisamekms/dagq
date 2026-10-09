use serde_json::json;

use super::*;
use crate::domain::stats::conflicts::MainChange;

const T: i64 = 1_800_000_000;

fn event(id: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: None,
        goal_id: None,
        run_id: None,
        kind: kind.to_owned(),
        payload,
        created_at: utc_text(secs * 1000),
        actor: None,
    }
}

fn change(path: &str) -> MainChange {
    MainChange {
        path: path.to_owned(),
        from: None,
        deleted: false,
    }
}

fn commit(sha: &str, at: i64, changes: Vec<MainChange>) -> MainCommit {
    MainCommit {
        sha: sha.to_owned(),
        at,
        changes,
    }
}

/// The events of `payloads` of `kind`, numbered from `id`, at `secs`.
fn events_of(kind: &str, payloads: Vec<Value>, id: i64, secs: i64) -> Vec<RunEvent> {
    payloads
        .into_iter()
        .zip(id..)
        .map(|(payload, id)| event(id, kind, payload, secs))
        .collect()
}

/// A record of two commits, the base at the second and its reach.
fn recorded() -> Vec<RunEvent> {
    let commits = vec![
        commit("a", T, vec![change("x.rs"), change("y.rs")]),
        commit(
            "b",
            T + 10,
            vec![
                MainChange {
                    path: "z.rs".into(),
                    from: Some("y.rs".into()),
                    deleted: false,
                },
                MainChange {
                    path: "x.rs".into(),
                    from: None,
                    deleted: true,
                },
            ],
        ),
    ];
    let mut events = events_of(
        MAIN_COMMITS_RECORDED,
        commits_payloads(None, &commits),
        1,
        T + 20,
    );
    events.extend(events_of(
        MAIN_PATHS_RECORDED,
        paths_payloads("b", T - 1, &["z.rs".to_owned()]),
        2,
        T + 20,
    ));
    events.push(event(
        3,
        MAIN_OBSERVED,
        observed_payload("b", T + 20, T - 1),
        T + 20,
    ));
    events
}

/// The commits, the base paths with the later commits' changes, each
/// commit's changed files (both names of a rename), the head, the reach
/// and the start; a commit recorded twice counts once.
#[test]
fn the_record_folds_into_the_history_its_files_and_its_reach() {
    let mut events = recorded();
    let later = vec![commit("c", T + 30, vec![change("w.rs")])];
    events.extend(events_of(
        MAIN_COMMITS_RECORDED,
        commits_payloads(Some("b"), &later),
        4,
        T + 40,
    ));
    // The same range again, as after a pass that failed before its reach.
    events.extend(events_of(
        MAIN_COMMITS_RECORDED,
        commits_payloads(Some("b"), &later),
        5,
        T + 41,
    ));
    events.push(event(
        6,
        MAIN_OBSERVED,
        observed_payload("c", T + 41, T - 1),
        T + 41,
    ));
    let log = fold_main_log(&events);
    let shas: Vec<&str> = log.history.commits.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(shas, ["a", "b", "c"]);
    assert_eq!(
        log.history.paths,
        ["z.rs", "w.rs"].into_iter().map(str::to_owned).collect()
    );
    assert_eq!(log.changed["b"], ["x.rs", "y.rs", "z.rs"]);
    assert_eq!(log.changed["c"], ["w.rs"]);
    assert_eq!(log.head.as_deref(), Some("c"));
    assert_eq!(
        (log.recorded_through, log.since),
        (Some(T + 41), Some(T - 1))
    );
    assert_eq!(log.reading, MainReading::Readable);
    assert!(log.paths_known);
    // The commits a base's head already has are not applied to it again.
    let base_only = fold_main_log(&recorded());
    assert_eq!(
        base_only.history.paths,
        ["z.rs".to_owned()].into_iter().collect()
    );
}

/// The payloads are cut by their commits, their changes and their paths,
/// a commit never split, each part naming the commit it goes on from; a
/// base not complete is not one.
#[test]
fn the_payloads_are_cut_and_a_base_counts_once_complete() {
    let many: Vec<MainCommit> = (0..COMMITS_PER_EVENT + 1)
        .map(|n| commit(&format!("s{n}"), T + n as i64, vec![change("a")]))
        .collect();
    let payloads = commits_payloads(Some("r"), &many);
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0]["after"], "r");
    assert_eq!(payloads[1]["after"], format!("s{}", COMMITS_PER_EVENT - 1));
    let wide = |sha: &str| {
        commit(
            sha,
            T,
            (0..CHANGES_PER_EVENT)
                .map(|n| change(&format!("f{n}")))
                .collect(),
        )
    };
    let payloads = commits_payloads(None, &[commit("n", T, vec![change("a")]), wide("w")]);
    assert_eq!(payloads.len(), 2, "the wide commit has its own event");
    assert_eq!(payloads[1]["commits"][0]["sha"], "w");
    assert!(commits_payloads(None, &[]).is_empty());
    let paths: Vec<String> = (0..=PATHS_PER_EVENT).map(|n| format!("p{n:05}")).collect();
    let parts = paths_payloads("h", T, &paths);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1]["parts"], 2);
    assert_eq!(paths_payloads("h", T, &[]).len(), 1);
    let first = events_of(MAIN_PATHS_RECORDED, parts[..1].to_vec(), 1, T);
    let half = fold_main_log(&first);
    assert!(!half.paths_known && half.since.is_none());
    let whole = fold_main_log(&events_of(MAIN_PATHS_RECORDED, parts, 1, T));
    assert_eq!(whole.history.paths.len(), PATHS_PER_EVENT + 1);
    assert_eq!(whole.since, Some(T));
}

/// A rewrite drops the commits after the merge base and the paths until
/// the new base; without a merge base it drops them all.
#[test]
fn a_rewrite_goes_on_from_the_merge_base() {
    let mut events = recorded();
    events.push(event(
        4,
        MAIN_REWRITTEN,
        rewritten_payload("b", "b2", Some("a"), T + 50),
        T + 50,
    ));
    let after = fold_main_log(&events);
    assert_eq!(after.history.commits.len(), 1);
    assert!(!after.paths_known);
    let again = vec![commit("b2", T + 45, vec![change("v.rs")])];
    events.extend(events_of(
        MAIN_COMMITS_RECORDED,
        commits_payloads(Some("a"), &again),
        5,
        T + 50,
    ));
    events.extend(events_of(
        MAIN_PATHS_RECORDED,
        paths_payloads("b2", T - 1, &["x.rs".into(), "y.rs".into(), "v.rs".into()]),
        6,
        T + 50,
    ));
    let log = fold_main_log(&events);
    let shas: Vec<&str> = log.history.commits.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(shas, ["a", "b2"]);
    assert_eq!(log.history.paths.len(), 3);
    assert_eq!(log.since, Some(T - 1), "the start stays the first base's");
    let mut unrelated = recorded();
    unrelated.push(event(
        4,
        MAIN_REWRITTEN,
        rewritten_payload("b", "q", None, T + 50),
        T + 50,
    ));
    assert!(fold_main_log(&unrelated).history.commits.is_empty());
}

/// Two supervisors' first records interleaved: a base at an older head
/// read after the commits past it still gets their changes.
#[test]
fn a_base_read_late_takes_the_commits_folded_past_its_head() {
    let mut events = recorded();
    let later = vec![commit("c", T + 30, vec![change("w.rs")])];
    events.insert(
        1,
        event(
            10,
            MAIN_COMMITS_RECORDED,
            commits_payloads(Some("b"), &later).remove(0),
            T + 20,
        ),
    );
    let log = fold_main_log(&events);
    assert_eq!(
        log.history.paths,
        ["z.rs", "w.rs"].into_iter().map(str::to_owned).collect()
    );
}

fn supervisor(id: i64, kind: &str, secs: i64) -> RunEvent {
    event(id, kind, json!({"supervisor": "s"}), secs)
}

/// The record covers a window that starts after its start and ends
/// within the grace of its reach; past that, the reason tells a failure
/// to read Git, no supervisor, and a reach gone stale apart; before the
/// first record, nothing.
#[test]
fn a_window_past_the_reach_has_no_record_with_its_reason() {
    let mut events = recorded();
    events.insert(0, supervisor(0, "supervisor_started", T - 1));
    let log = fold_main_log(&events);
    let through = T + 20;
    let edge = (through + MAIN_RECORD_GRACE_SECS) * 1000;
    assert_eq!(log.covers(&events, Some(T * 1000), Some(edge)), Ok(()));
    assert_eq!(
        log.covers(&events, Some((T - 2) * 1000), Some(T * 1000)),
        Err(MainMissing::BeforeRecord { since: T - 1 })
    );
    // The supervisor is silent at the edge: its life ended.
    assert_eq!(
        log.covers(&events, Some(T * 1000), Some(edge + 1)),
        Err(MainMissing::SupervisorStopped { through })
    );
    let mut living = events.clone();
    living.push(supervisor(
        10,
        "supervisor_alive",
        through + MAIN_RECORD_GRACE_SECS,
    ));
    assert_eq!(
        log.covers(&living, Some(T * 1000), Some(edge + 1)),
        Err(MainMissing::Stale { through })
    );
    let mut stopped = events.clone();
    stopped.push(supervisor(10, "supervisor_stopped", through + 60));
    stopped.push(supervisor(
        11,
        "supervisor_alive",
        through + MAIN_RECORD_GRACE_SECS,
    ));
    assert_eq!(
        fold_main_log(&stopped).covers(&stopped, Some(T * 1000), Some(edge + 1)),
        Err(MainMissing::SupervisorStopped { through })
    );
    living.push(event(
        12,
        MAIN_READ_FAILED,
        read_failed_payload("no git", through + 30),
        through + 30,
    ));
    let failing = fold_main_log(&living);
    let missing = failing
        .covers(&living, Some(T * 1000), Some(edge + 1))
        .unwrap_err();
    assert_eq!(
        missing,
        MainMissing::GitUnreadable {
            since: through + 30,
            reason: "no git".into()
        }
    );
    assert!(missing.to_string().contains("Git could not be read"));
    assert_eq!(
        fold_main_log(&[]).covers(&[], None, None),
        Err(MainMissing::NotRecorded)
    );
    // As conflict_hotspots' window: unavailable with the reason's text.
    let window = vec![event(
        20,
        "run_claimed",
        json!({}),
        through + MAIN_RECORD_GRACE_SECS + 1,
    )];
    let all: Vec<RunEvent> = living.iter().cloned().chain(window).collect();
    assert_eq!(
        failing.history_for(&all, EventId::new(19), EventId::new(20)),
        History::Unavailable(missing.to_string())
    );
    assert_eq!(
        failing.history_for(&all, EventId::new(0), EventId::new(3)),
        History::Read(failing.history.clone())
    );
    for missing in [
        MainMissing::NotRecorded,
        MainMissing::BeforeRecord { since: T },
        MainMissing::SupervisorStopped { through: T },
        MainMissing::Stale { through: T },
    ] {
        assert!(!missing.to_string().is_empty());
    }
    assert_eq!(MainReading::of(None), MainReading::Readable);
}
