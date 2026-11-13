//! The rest of (ii) Spike 4756 left unresolved, by its prototype widened
//! (docs/plans/ledger-window-earlier-visibility-spike.md, section "追試 (ii)
//! の残り（Spike 4811）"): (A) the answers of the final shapes with 3000
//! settled units, (B) the baselines' latest marks, (C) main's record read
//! as summary rows and sha seeks counted apart, (D) what befell a landing
//! with its counterexamples, (E) a goal's latest seek and (F) the
//! proposals a finding links. Not wired: each test compares the answers
//! with those of every row and counts what the read returns.

use super::*;

/// The settled units the answers of the rest are compared with besides
/// 30: 300 by default, `DAGQ_SPIKE_4811_LARGE` for another number (3000 is
/// the run of record).
fn rest_large() -> usize {
    std::env::var("DAGQ_SPIKE_4811_LARGE")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(300)
}

fn before_ms(e: &RunEvent, from_ms: i64) -> bool {
    crate::domain::stats::timestamp_millis(&e.created_at).is_some_and(|at| at < from_ms)
}

fn at_or_before(e: &RunEvent, end_ms: i64) -> bool {
    crate::domain::stats::timestamp_millis(&e.created_at).is_some_and(|at| at <= end_ms)
}

/// Whether the supervisor `token` is a settled unit's (`u<k>`).
fn unit_supervisor(e: &RunEvent) -> bool {
    e.payload["supervisor"].as_str().is_some_and(|token| {
        token.len() > 1 && token.starts_with('u') && token[1..].bytes().all(|b| b.is_ascii_digit())
    })
}

/// `log` with the settled units' supervisors made one (`u`): its first
/// start and last stop kept, every unit's heartbeat, waits and samples
/// its own. A whole read's `kpi` reads every start's life from every
/// event, so a start of each unit takes minutes for 3000 in a debug
/// build; one life lets the answers of 3000 units be compared.
fn shared_lives(mut log: Log) -> Log {
    let of = |kind: EventKind| {
        let ids = log
            .events
            .iter()
            .filter(|e| e.kind == kind && unit_supervisor(e))
            .map(|e| e.id);
        ids.collect::<Vec<_>>()
    };
    let first = of(EventKind::SupervisorStarted).into_iter().min();
    let last = of(EventKind::SupervisorStopped).into_iter().max();
    log.events.retain(|e| {
        !(unit_supervisor(e)
            && ((e.kind == EventKind::SupervisorStarted && Some(e.id) != first)
                || (e.kind == EventKind::SupervisorStopped && Some(e.id) != last)))
    });
    for e in log.events.iter_mut().filter(|e| unit_supervisor(e)) {
        e.payload["supervisor"] = json!("u");
    }
    log
}

fn shared_near_units(settled: usize) -> Log {
    shared_lives(near_snapshot_units(settled))
}

fn shared_units(settled: usize) -> Log {
    shared_lives(queue_units_only(settled))
}

/// [`check_part_of`] with `large` units of `answers` for the answers.
fn final_part(
    name: &str,
    small: fn(usize) -> Log,
    units: fn(usize) -> Log,
    answers: fn(usize) -> Log,
    large: usize,
) -> [usize; 2] {
    let part = QUEUE_PARTS.iter().find(|(n, _)| *n == name).unwrap().1;
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let kinds: BTreeSet<String> = part.iter().map(|kind| (*kind).to_owned()).collect();
    let count = |rows: &[LedgerRow]| {
        part.iter()
            .map(|kind| queue_count(rows, kind))
            .sum::<usize>()
    };
    let small = rows_of(&small(30));
    let (misses, _) = part_report(&small, part, QUEUE_SHAPED);
    let small_read = count(&queue_ii(&small, from_ms, &kinds, QUEUE_SHAPED));
    let units = rows_of(&units(3000));
    let units_read = count(&queue_ii(&units, from_ms, &kinds, QUEUE_SHAPED));
    let shared_small = count(&queue_ii(
        &rows_of(&answers(30)),
        from_ms,
        &kinds,
        QUEUE_SHAPED,
    ));
    let rows = rows_of(&answers(large));
    let read = count(&queue_ii(&rows, from_ms, &kinds, QUEUE_SHAPED));
    let (large_misses, _) = queue_misses(&rows, &queue_ii(&rows, from_ms, &kinds, QUEUE_SHAPED));
    eprintln!(
        "{name}: read {small_read}/{units_read} (30/3000), shared lives {shared_small}/{read} \
         (30/{large}) of {}, misses {misses:?} {large_misses:?}",
        count(&rows)
    );
    assert_eq!(misses, Vec::<String>::new(), "{name} 30");
    assert_eq!(large_misses, Vec::<String>::new(), "{name} {large}");
    assert_eq!(small_read, units_read, "{name}");
    assert_eq!(shared_small, read, "{name}");
    [small_read, units_read]
}

#[test]
fn the_final_shapes_of_the_waits_lives_samples_and_failures_answer_as_every_row_with_many_units() {
    let large = rest_large();
    final_part(
        "ii-a",
        near_snapshot,
        near_snapshot_units,
        shared_near_units,
        large,
    );
    final_part("ii-f", history_ii, queue_units_only, shared_units, large);
    final_part("ii-e", history_ii, queue_units_only, shared_units, large);
}

// The histories of (B)–(E): each settled unit adds a baseline's group of
// a goal and of a finding, a change of goal 700, main's paths and a check
// and a failure of CI on a second branch.

const GOAL: i64 = 700;

/// The task whose baselines each settled unit's group holds (task 64,
/// landed in the range).
const BASELINE_TASK: i64 = 64;

fn unit_secs(k: usize, settled: usize) -> i64 {
    11 * DAY + i64::try_from(k).unwrap() * 139 * DAY / i64::try_from(settled).unwrap()
}

fn goal_record(title: &str, priority: &str) -> Value {
    json!({"title": title, "status": "open", "priority": priority})
}

fn add_of_goal(log: &mut Log, goal: i64, kind: &str, payload: Value, secs: i64) {
    log.add(None, None, kind, payload, secs);
    log.events.last_mut().unwrap().goal_id = Some(crate::domain::GoalId::new(goal));
}

fn unit_finding(task: i64, status: &str) -> Value {
    let mut finding = crate::domain::ledger::testkit::finding_record(status);
    let id = 10_000 + task;
    finding["finding_id"] = json!(id);
    finding[crate::domain::ledger::findings::VALUES]["id"] = json!(id);
    finding
}

/// The events of (B)–(E) of each of `settled` units and their cases.
fn add_rest(log: &mut Log, settled: usize) {
    let near = (READ_AT_DAY - 1) * DAY;
    // Main's paths stay as many: each unit's commit adds its file and
    // deletes the previous unit's (the summary holds the paths main has).
    for e in &mut log.events {
        let commits = e.payload["commits"].as_array().cloned().unwrap_or_default();
        if e.kind != EventKind::MainCommitsRecorded || commits.len() != 1 {
            continue;
        }
        let Some(k) = commits[0]["sha"]
            .as_str()
            .filter(|sha| sha.len() == 40)
            .and_then(|sha| usize::from_str_radix(sha, 16).ok())
        else {
            continue;
        };
        let mut changes = vec![json!({"path": format!("src/u{k}.rs")})];
        if k > 0 {
            changes.push(json!({"path": format!("src/u{}.rs", k - 1), "deleted": true}));
        }
        e.payload["commits"][0]["changes"] = Value::Array(changes);
    }
    for k in 0..settled {
        let t = unit_secs(k, settled);
        let task = 100_000 + i64::try_from(k).unwrap();
        let goal = 600_000 + i64::try_from(k).unwrap();
        let priority = if k % 2 == 0 { "high" } else { "normal" };
        // (B) A group of each baseline: a goal's and its mark, a finding's
        // (its state, resolved by the unit) and its mark.
        add_of_goal(
            log,
            goal,
            "attributes_baseline",
            json!({"goal": goal_record(&format!("goal {goal}"), "low")}),
            t + 1220,
        );
        // A task's baseline in the same group: task 64, which the range
        // lands, so its row is read whole and its records meet the mark.
        log.add(
            Some(BASELINE_TASK),
            None,
            "attributes_baseline",
            json!({"title": format!("t64 v{k}"), "status": "ready", "priority": priority}),
            t + 1225,
        )
        .add(None, None, "attributes_baseline", json!({}), t + 1230)
        .add(
            None,
            None,
            "finding_baseline",
            unit_finding(task, "resolved"),
            t + 1240,
        )
        .add(None, None, "finding_baseline", json!({}), t + 1250);
        // (E) Goal 700 changed by each unit.
        add_of_goal(
            log,
            GOAL,
            "goal_updated",
            json!({"new": goal_record(&format!("goal 700 v{k}"), priority)}),
            t + 1215,
        );
        // (C) Main's paths at the unit's commit.
        log.add(
            None,
            None,
            "main_paths_recorded",
            json!({"head": unit_commit(k), "since": T0 + 10 * DAY, "part": 0, "parts": 1,
                   "paths": ["src/base.rs", "src/lib.rs", format!("src/u{k}.rs")]}),
            t + 1405,
        );
        // (D) CI on a second branch every second unit.
        if k % 2 == 1 {
            log.add(
                None,
                None,
                "ci_checked",
                json!({"branch": "release"}),
                t + 1425,
            )
            .add(
                None,
                None,
                "ci_turned_red",
                json!({"sha": unit_commit(k), "branch": "release"}),
                t + 1435,
            );
        }
    }
    // (D) CI checked `release` before every unit; run 60's `c60` turned
    // red before the range, then another sha did; a revert of `c60` before
    // the range.
    log.add(
        None,
        None,
        "ci_checked",
        json!({"branch": "release"}),
        10 * DAY + 20,
    )
    .add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c60", "branch": "main"}),
        156 * DAY + 30,
    )
    .add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "x1", "branch": "main"}),
        157 * DAY,
    )
    .add(
        None,
        None,
        "main_commits_recorded",
        json!({"after": "c60", "commits": [{"sha": "rv60b", "at": T0 + 157 * DAY + 100,
               "changes": [], "reverts": "c60"}]}),
        157 * DAY + 100,
    );
    // (E) Goal 700's status after its last change; (B) its baseline before
    // the range in a group whose mark is in it (after the `--at` window).
    add_of_goal(
        log,
        GOAL,
        "goal_status_changed",
        json!({"from": "open", "to": "draft"}),
        156 * DAY + 500,
    );
    add_of_goal(
        log,
        GOAL,
        "attributes_baseline",
        json!({"goal": goal_record("goal 700 baseline", "low")}),
        157 * DAY + 600,
    );
    log.add(
        Some(BASELINE_TASK),
        None,
        "attributes_baseline",
        json!({"title": "t64 baseline", "status": "ready", "priority": "low"}),
        157 * DAY + 650,
    );
    log.add(None, None, "attributes_baseline", json!({}), near + 3100);
}

/// [`near_snapshot`] with the events of [`add_rest`].
fn rest_log(settled: usize) -> Log {
    let fix = |log: &mut Log| {
        fix_queue_units(log);
        // Each settled unit's run lands the unit's commit.
        for e in &mut log.events {
            if e.kind == EventKind::RunIntegrated
                && let Some(k) = e.run_id.as_ref().and_then(|run| {
                    run.as_str()
                        .strip_suffix("-2222-4222-8222-222222222222")
                        .and_then(|k| usize::from_str_radix(k, 16).ok())
                })
            {
                e.payload["commit"] = json!(unit_commit(k));
            }
        }
    };
    let mut log = history_with(
        settled,
        |log| {
            add_queue_units(settled)(log);
            add_rest(log, settled);
            log.add(
                None,
                None,
                "forecast_recorded",
                json!({"at_secs": T0 + 158 * DAY, "method": 1, "goals": [],
                       "tasks": [{"id": 2, "p50_secs": 3600, "p90_secs": 7200},
                                 {"id": 1, "p50_secs": 3600, "p90_secs": 7200}]}),
                158 * DAY,
            );
        },
        fix,
    );
    for event in &mut log.events {
        if event.kind == EventKind::ForecastRecorded && timestamp_secs(event) < 10 * DAY {
            event.payload["tasks"] = json!([]);
        }
    }
    log
}

/// [`rest_log`] with one life of the units' supervisors.
fn rest_full(settled: usize) -> Log {
    shared_lives(rest_log(settled))
}

/// [`rest_full`] without the units' tasks and runs.
fn rest_units(settled: usize) -> Log {
    shared_lives(without_unit_tasks(rest_log(settled)))
}

/// What the read of the rest reads besides [`queue_ii`]'s shapes.
#[derive(Debug, Clone, Copy, Default)]
struct Rest {
    /// (B) Of each baseline's kind, the latest mark before the range.
    marks: bool,
    /// (C) Main's record before the range as rows the builder saves (the
    /// paths, the reach, Git's reading, CI's first and last check of each
    /// branch) and the commits the range names by their sha.
    main: bool,
    /// (D) With `main`, the reverts whose target and the failures whose sha
    /// is a landed commit of the range's runs.
    landing: bool,
    /// (E) Of each goal the range names, its latest record that replaces
    /// it (by the earliest window's end) and the goal's records after.
    goals: bool,
    /// (F) The findings whose `proposed` names a proposal the range names.
    links: bool,
}

const REST: Rest = Rest {
    marks: true,
    main: true,
    landing: true,
    goals: true,
    links: true,
};

/// The earliest end of the windows compared (the `--at` question's).
fn earliest_end_ms() -> i64 {
    (NOW - 2 * DAY) * 1000
}

fn is_attributes_mark(e: &RunEvent) -> bool {
    e.kind == EventKind::AttributesBaseline && e.task_id.is_none() && e.goal_id.is_none()
}

fn is_finding_mark(e: &RunEvent) -> bool {
    e.kind == EventKind::FindingBaseline
        && e.payload
            .get(crate::domain::ledger::findings::VALUES)
            .is_none()
}

/// The landed commit of a `run_integrated`.
fn landed_sha(e: &RunEvent) -> Option<String> {
    e.payload["result_commit"]
        .as_str()
        .or_else(|| e.payload["commit"].as_str())
        .map(str::to_owned)
}

/// The queue rows of `rows` read for `kinds` as [`queue_ii`] reads them
/// (the supervisors, snapshots and asks), and what `rest` adds.
fn queue_rest(
    rows: &[LedgerRow],
    from_ms: i64,
    kinds: &BTreeSet<String>,
    rest: Rest,
) -> Vec<LedgerRow> {
    let base = QueueShape {
        across: true,
        named_lives: true,
        snapshots: true,
        asks: true,
        ..QueueShape::default()
    };
    let before = |e: &RunEvent| before_ms(e, from_ms);
    let queue = || rows.iter().filter(|row| row.kind == LedgerKind::Queue);
    let reaches = |row: &LedgerRow| {
        row.ended_at.is_none_or(|end| end >= from_ms) || row.records.iter().any(|e| !before(e))
    };
    // What the range's rows of runs, tasks and sessions name.
    let range = || {
        rows.iter()
            .filter(|row| row.kind != LedgerKind::Queue && reaches(row))
            .flat_map(|row| row.records.iter().map(move |e| (row, e)))
    };
    let mut keep: BTreeSet<String> = BTreeSet::new();
    if rest.marks {
        for mark in [is_attributes_mark, is_finding_mark] {
            keep.extend(
                queue()
                    .flat_map(|row| row.records.iter().map(move |e| (row, e)))
                    .filter(|(_, e)| mark(e) && before(e))
                    .max_by_key(|(_, e)| e.id)
                    .map(|(row, _)| row.key.clone()),
            );
        }
    }
    // The goals the range's rows name.
    let named_goals: BTreeSet<i64> = range()
        .filter_map(|(_, e)| {
            e.payload["goal_id"].as_i64().or_else(|| {
                (e.kind == EventKind::TaskGoalChanged)
                    .then(|| e.payload["to"].as_i64())
                    .flatten()
            })
        })
        .collect();
    if rest.goals {
        let mut marks: Vec<(&LedgerRow, &RunEvent)> = queue()
            .flat_map(|row| row.records.iter().map(move |e| (row, e)))
            .filter(|(_, e)| is_attributes_mark(e))
            .collect();
        marks.sort_by_key(|(_, e)| e.id);
        let end = earliest_end_ms();
        // A baseline counts by its group's mark (the next after it).
        let taken = |e: &RunEvent| {
            marks
                .iter()
                .find(|(_, mark)| mark.id > e.id)
                .map_or(at_or_before(e, end), |(_, mark)| at_or_before(mark, end))
        };
        let replaces = |e: &RunEvent| match e.kind.known() {
            Some(EventKind::GoalCreated) => at_or_before(e, end),
            Some(EventKind::GoalUpdated) => e.payload["new"].is_object() && at_or_before(e, end),
            Some(EventKind::AttributesBaseline) => taken(e),
            _ => false,
        };
        for &goal in &named_goals {
            let mut of_goal: Vec<(&LedgerRow, &RunEvent)> = queue()
                .flat_map(|row| row.records.iter().map(move |e| (row, e)))
                .filter(|(_, e)| e.goal_id.is_some_and(|g| g.as_i64() == goal))
                .collect();
            of_goal.sort_by_key(|(_, e)| e.id);
            let from = of_goal.iter().rposition(|(_, e)| replaces(e)).unwrap_or(0);
            for (row, e) in &of_goal[from..] {
                keep.insert(row.key.clone());
                // A baseline is read with its group's mark (the next after it).
                if e.kind == EventKind::AttributesBaseline
                    && let Some((mark, _)) = marks.iter().find(|(_, mark)| mark.id > e.id)
                {
                    keep.insert(mark.key.clone());
                }
            }
        }
    }
    let landed: BTreeSet<String> = range()
        .filter(|(row, e)| row.kind == LedgerKind::Run && e.kind == EventKind::RunIntegrated)
        .filter_map(|(_, e)| landed_sha(e))
        .collect();
    if rest.landing {
        keep.extend(
            queue()
                .filter(|row| {
                    row.records.iter().any(|e| {
                        e.kind == EventKind::CiTurnedRed
                            && e.payload["sha"]
                                .as_str()
                                .is_some_and(|s| landed.contains(s))
                    })
                })
                .map(|row| row.key.clone()),
        );
    }
    if rest.links {
        // The proposals the range names, and those a `goal_submitted` of a
        // goal it names names (a proposal runs by its goal's tasks too).
        let proposals: BTreeSet<i64> = range()
            .map(|(_, e)| e)
            .chain(queue().flat_map(|row| &row.records).filter(|e| !before(e)))
            .chain(rows.iter().flat_map(|row| &row.records).filter(|e| {
                e.kind == EventKind::GoalSubmitted
                    && e.goal_id.is_some_and(|g| named_goals.contains(&g.as_i64()))
            }))
            .filter_map(|e| e.payload["proposal_id"].as_i64())
            .collect();
        keep.extend(
            queue()
                .filter(|row| {
                    row.records.iter().any(|e| {
                        e.kind == EventKind::FindingStatusChanged
                            && e.payload["to"] == "proposed"
                            && e.payload["proposal_id"]
                                .as_i64()
                                .is_some_and(|p| proposals.contains(&p))
                    })
                })
                .map(|row| row.key.clone()),
        );
    }
    let mut out = queue_ii(rows, from_ms, kinds, base);
    let have: BTreeSet<String> = out
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .map(|row| row.key.clone())
        .collect();
    out.extend(
        queue()
            .filter(|row| keep.contains(&row.key) && !have.contains(&row.key))
            .cloned(),
    );
    if rest.main {
        out = main_rows(rows, out, from_ms, rest.landing.then_some(&landed));
    }
    out
}

const SUMMARY: &str = "main_summary/";
const BY_SHA: &str = "main_sha/";

/// `narrowed` with main's record before `from_ms` replaced by the rows the
/// builder saves for it (one each: the paths main has, the reach of the
/// record, Git's reading, CI's first and last check of each branch) and
/// the rows of main's commits the range names (a landed commit of a run
/// row read, the sha, the `after` or a revert's target of the range's
/// records; with `landed`, also the commits whose revert targets one of
/// its shas), one per event, as a table of the commits by sha gives them.
fn main_rows(
    whole: &[LedgerRow],
    narrowed: Vec<LedgerRow>,
    from_ms: i64,
    landed: Option<&BTreeSet<String>>,
) -> Vec<LedgerRow> {
    use crate::domain::stats::main_log::fold_main_log;
    let before = |e: &RunEvent| before_ms(e, from_ms);
    let is_main = |row: &LedgerRow| {
        row.kind == LedgerKind::Queue
            && MAIN_KINDS.contains(&subject_kind(row))
            && row.records.iter().all(&before)
    };
    let mut old: Vec<&LedgerRow> = whole.iter().filter(|row| is_main(row)).collect();
    old.sort_by_key(|row| row.records.first().map(|e| e.id));
    let events: Vec<RunEvent> = old.iter().flat_map(|row| row.records.clone()).collect();
    let mut named: BTreeSet<String> = BTreeSet::new();
    let reaches = |row: &LedgerRow| {
        row.ended_at.is_none_or(|end| end >= from_ms) || row.records.iter().any(|e| !before(e))
    };
    for row in narrowed.iter().filter(|row| !is_main(row)) {
        for e in &row.records {
            // The landed commits of the run rows the range reads.
            if row.kind == LedgerKind::Run && reaches(row) && e.kind == EventKind::RunIntegrated {
                named.extend(landed_sha(e));
            }
            // Only the range's records name a sha: rows of a kind read
            // whole bring their records before the range too.
            if before(e) && row.kind == LedgerKind::Queue {
                continue;
            }
            if e.kind == EventKind::MainCommitsRecorded {
                named.extend(e.payload["after"].as_str().map(str::to_owned));
                for commit in e.payload["commits"].as_array().into_iter().flatten() {
                    named.extend(commit["sha"].as_str().map(str::to_owned));
                    named.extend(commit["reverts"].as_str().map(str::to_owned));
                }
            }
            if e.kind == EventKind::CiTurnedRed {
                named.extend(e.payload["sha"].as_str().map(str::to_owned));
            }
        }
    }
    let by_sha: Vec<RunEvent> = events
        .iter()
        .filter(|e| {
            e.kind == EventKind::MainCommitsRecorded
                && e.payload["commits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|commit| {
                        commit["sha"]
                            .as_str()
                            .is_some_and(|sha| named.contains(sha))
                            || landed.is_some_and(|landed| {
                                commit["reverts"]
                                    .as_str()
                                    .is_some_and(|target| landed.contains(target))
                            })
                    })
        })
        .cloned()
        .collect();
    let log = fold_main_log(&events);
    let mut saved: Vec<(String, Vec<RunEvent>)> = Vec::new();
    let mut made: Vec<(&str, Value)> = Vec::new();
    if log.paths_known {
        let mut paths: Vec<&String> = log.history.paths.iter().collect();
        paths.sort();
        made.push((
            "main_paths_recorded",
            json!({"head": log.history.commits.last().map(|c| c.sha.clone()),
                   "since": log.since, "part": 0, "parts": 1, "paths": paths}),
        ));
    }
    if let Some(at) = log.recorded_through {
        made.push((
            "main_observed",
            json!({"head": log.head, "at": at, "since": log.since, "reverts": true}),
        ));
    }
    if let Some(reading) = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::MainReadFailed || e.kind == EventKind::MainReadRecovered)
    {
        saved.push((format!("{SUMMARY}reading"), vec![reading.clone()]));
    }
    let mut branches: BTreeMap<String, Vec<&RunEvent>> = BTreeMap::new();
    for e in events.iter().filter(|e| e.kind == EventKind::CiChecked) {
        let branch = e.payload["branch"].as_str().unwrap_or_default().to_owned();
        branches.entry(branch).or_default().push(e);
    }
    for (branch, checks) in &branches {
        let mut ends = vec![checks[0].clone()];
        ends.extend((checks.len() > 1).then(|| checks[checks.len() - 1].clone()));
        saved.push((format!("{SUMMARY}ci/{branch}"), ends));
    }
    let used: BTreeSet<EventId> = saved
        .iter()
        .flat_map(|(_, records)| records.iter().map(|e| e.id))
        .chain(by_sha.iter().map(|e| e.id))
        .collect();
    let mut spare_ids: Vec<EventId> = events
        .iter()
        .map(|e| e.id)
        .filter(|id| !used.contains(id))
        .collect();
    for (kind, payload) in made {
        let mut event = events.last().unwrap().clone();
        event.id = spare_ids.pop().unwrap();
        event.kind = kind.into();
        event.payload = payload;
        saved.push((format!("{SUMMARY}{kind}"), vec![event]));
    }
    let mut out: Vec<LedgerRow> = narrowed.into_iter().filter(|row| !is_main(row)).collect();
    let Some(template) = old.last() else {
        return out;
    };
    for (key, mut records) in saved {
        records.sort_by_key(|e| e.id);
        let mut row = (*template).clone();
        row.key = key;
        row.records = records;
        out.push(row);
    }
    for e in by_sha {
        let mut row = (*template).clone();
        row.key = format!("{BY_SHA}{}", e.id);
        row.records = vec![e];
        out.push(row);
    }
    out
}

/// The goal ids [`queue_misses`] compares the report's goal summaries of.
const GOALS: [i64; 1] = [GOAL];

/// [`queue_misses`], with the report's goal summaries of [`GOALS`] and
/// its near-term diagram of the day's question.
fn rest_misses(whole: &[LedgerRow], got: &[LedgerRow]) -> Vec<String> {
    let (mut misses, _) = queue_misses(whole, got);
    let every = select(whole.to_vec(), &whole_query());
    let (window, _) = narrowed_shaped(got, &time_query(&kpi_questions()[0]), &PARTS, SHAPED);
    let report = |window: &LedgerWindow| {
        let goals = match window.attributes() {
            crate::domain::ledger::attributes::Folded::Attributes(known) => known
                .goal_summaries()
                .into_iter()
                .filter(|goal| GOALS.contains(&goal.id.as_i64()))
                .map(|goal| format!("{goal:?}"))
                .collect::<Vec<_>>(),
            crate::domain::ledger::attributes::Folded::BeforeBaseline => Vec::new(),
        };
        let task = match window.attributes() {
            crate::domain::ledger::attributes::Folded::Attributes(known) => format!(
                "{:?}",
                known.tasks.get(&crate::domain::TaskId::new(BASELINE_TASK))
            ),
            crate::domain::ledger::attributes::Folded::BeforeBaseline => String::new(),
        };
        json!({
            "task": task,
            "goal_summaries": goals,
            "diagram": serde_json::to_value(crate::application::report::diagram_draft(window))
                .unwrap(),
        })
    };
    let expected = report(&every);
    eprintln!(
        "report: {} goal summaries, task {}, diagram {}",
        expected["goal_summaries"].as_array().map_or(0, Vec::len),
        expected["task"],
        expected["diagram"]["draft"]
    );
    differences(&report(&window), &expected, "report", &mut misses);
    misses
}

/// What a read of the rest returned of `kinds`: the queue rows of the
/// kinds besides main's (and of main's, those of the range), main's saved
/// rows, and main's commits by sha.
fn rest_reads(rows: &[LedgerRow], kinds: &[&str]) -> [usize; 3] {
    let prefixed = |row: &LedgerRow, prefix: &str| row.key.starts_with(prefix);
    let queue: usize = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .filter(|row| !prefixed(row, SUMMARY) && !prefixed(row, BY_SHA))
        .filter(|row| kinds.contains(&subject_kind(row)))
        .count();
    let saved = rows.iter().filter(|row| prefixed(row, SUMMARY)).count();
    let by_sha = rows.iter().filter(|row| prefixed(row, BY_SHA)).count();
    [queue, saved, by_sha]
}

/// Check a part of the rest: with 30 units of `small` the read of Spike
/// 4756 ([`QUEUE_SHAPED`]) and that of `rest` against the whole read, the
/// answers of `rest` with [`rest_large`] units of `units`, and its reads
/// with 30 and 3000 units of `units`. Returns the misses of Spike 4756's
/// read and the reads with 30 and 3000.
fn rest_part(
    name: &str,
    kinds: &[&str],
    rest: Rest,
    small: fn(usize) -> Log,
    units: fn(usize) -> Log,
) -> (Vec<String>, [[usize; 3]; 2]) {
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let set: BTreeSet<String> = kinds.iter().map(|kind| (*kind).to_owned()).collect();
    let rows = rows_of(&small(30));
    // Spike 4756's read of the kinds (its main summary only when main's
    // kinds are narrowed: otherwise main is read whole).
    let shape_4756 = QueueShape {
        main: kinds.iter().any(|kind| MAIN_KINDS.contains(kind)),
        ..QUEUE_SHAPED
    };
    let (of_4756, _) = queue_misses(&rows, &queue_ii(&rows, from_ms, &set, shape_4756));
    let small_narrowed = queue_rest(&rows, from_ms, &set, rest);
    let misses = rest_misses(&rows, &small_narrowed);
    let small_reads = rest_reads(&small_narrowed, kinds);
    let large = rest_large();
    let mut reads = Vec::new();
    let mut whole = Vec::new();
    let mut sizes = vec![30, large, 3000];
    sizes.dedup();
    for settled in sizes {
        let rows = rows_of(&units(settled));
        let narrowed = queue_rest(&rows, from_ms, &set, rest);
        if settled != 3000 || large == 3000 {
            let misses = rest_misses(&rows, &narrowed);
            assert_eq!(misses, Vec::<String>::new(), "{name} {settled}");
        }
        if settled == 30 || settled == 3000 {
            reads.push(rest_reads(&narrowed, kinds));
            whole.push(rest_reads(&rows, kinds)[0]);
        }
    }
    eprintln!(
        "{name}: 4756 misses {} {:?}; reads [queue, saved, by sha] {reads:?} (30/3000) of {whole:?}, \
         with the units' runs {small_reads:?}, answers with 30 and {large}, misses {misses:?}",
        of_4756.len(),
        of_4756.iter().take(6).collect::<Vec<_>>()
    );
    assert_eq!(misses, Vec::<String>::new(), "{name} 30");
    assert_eq!(reads[0], reads[1], "{name}");
    // With the settled units' tasks and runs (their landed commits) too.
    assert_eq!(small_reads, reads[1], "{name} with the units' runs");
    assert!(whole[1] > 50 * whole[0] / 2, "{name} {whole:?}");
    (of_4756, [reads[0], reads[1]])
}

const BASELINE_KINDS: [&str; 2] = ["attributes_baseline", "finding_baseline"];
const GOAL_KINDS: [&str; 3] = ["goal_created", "goal_updated", "goal_status_changed"];

#[test]
fn the_baselines_read_from_their_latest_marks_answer_as_every_row() {
    let rest = Rest {
        marks: true,
        goals: true,
        ..Rest::default()
    };
    let (of_4756, _) = rest_part("B", &BASELINE_KINDS, rest, rest_full, rest_units);
    assert!(of_4756.is_empty(), "{of_4756:?}");
}

#[test]
fn main_s_saved_rows_and_its_commits_by_sha_are_counted_apart_and_stay_as_many() {
    let rest = Rest {
        main: true,
        landing: true,
        ..Rest::default()
    };
    let (of_4756, reads) = rest_part("C", &MAIN_KINDS, rest, rest_full, rest_units);
    assert!(reads[0][1] > 0 && reads[0][2] > 0, "{reads:?}");
    assert!(!of_4756.is_empty());
}

#[test]
fn what_befell_a_landing_reads_its_failures_and_reverts_by_its_commit() {
    let mut kinds = MAIN_KINDS.to_vec();
    kinds.push("ci_turned_red");
    let rest = Rest {
        main: true,
        landing: true,
        ..Rest::default()
    };
    let (of_4756, _) = rest_part("D", &kinds, rest, rest_full, rest_units);
    // Spike 4756's read: the earlier failure, the revert before the range
    // and CI's record of main are lost.
    assert!(
        of_4756.iter().any(|miss| miss.contains("befell.60")),
        "{of_4756:?}"
    );
}

#[test]
fn a_goal_is_read_from_its_latest_record_that_replaces_it() {
    let mut kinds = GOAL_KINDS.to_vec();
    kinds.extend(BASELINE_KINDS);
    let rest = Rest {
        marks: true,
        goals: true,
        ..Rest::default()
    };
    let (of_4756, _) = rest_part("E", &kinds, rest, rest_full, rest_units);
    assert!(of_4756.is_empty(), "{of_4756:?}");
}

// (F) The proposals a finding links: each settled unit's finding proposed
// (to the unit's proposal, its task landed) and resolved; finding 51
// proposed to proposal 9 and resolved before the range, whose task 68
// is open.

fn add_links(settled: usize) -> impl FnOnce(&mut Log) {
    move |log: &mut Log| {
        for k in 0..settled {
            let task = 100_000 + i64::try_from(k).unwrap();
            let mut proposed = unit_finding(task, "proposed");
            proposed["from"] = json!("open");
            proposed["to"] = json!("proposed");
            proposed["proposal_id"] = json!(400_000 + task);
            proposed["by"] = json!("planner");
            log.add(
                None,
                None,
                "finding_status_changed",
                proposed,
                unit_secs(k, settled) + 997,
            );
        }
        let finding = |id: i64, status: &str| {
            let mut f = crate::domain::ledger::testkit::finding_record(status);
            f["finding_id"] = json!(id);
            f[crate::domain::ledger::findings::VALUES]["id"] = json!(id);
            f
        };
        let changed = |id: i64, from: &str, to: &str| {
            let mut f = finding(id, to);
            f["from"] = json!(from);
            f["to"] = json!(to);
            f
        };
        let mut proposed = changed(51, "open", "proposed");
        proposed["proposal_id"] = json!(9);
        proposed["by"] = json!("planner");
        let mut proposed10 = changed(53, "open", "proposed");
        proposed10["proposal_id"] = json!(10);
        proposed10["by"] = json!("planner");
        log.add(
            None,
            None,
            "goal_submitted",
            json!({"proposal_id": 10}),
            152 * DAY + 9,
        );
        log.events.last_mut().unwrap().goal_id = Some(crate::domain::GoalId::new(701));
        log.add(
            None,
            None,
            "finding_recorded",
            finding(51, "open"),
            152 * DAY,
        )
        .add(
            None,
            None,
            "finding_status_changed",
            proposed,
            152 * DAY + 1,
        )
        .add(
            Some(68),
            None,
            "task_created",
            json!({"status": "ready", "title": "t68"}),
            152 * DAY + 2,
        )
        .add(
            Some(68),
            None,
            "task_submitted",
            json!({"proposal_id": 9, "owner_origin": "planner"}),
            152 * DAY + 3,
        )
        .add(
            None,
            None,
            "finding_status_changed",
            changed(51, "proposed", "resolved"),
            153 * DAY,
        )
        .add(
            None,
            None,
            "finding_recorded",
            finding(52, "open"),
            154 * DAY,
        )
        // Proposal 10 runs by its goal 701 only: the goal submitted to it,
        // its open task 69, finding 53 proposed to it and resolved before
        // the range.
        .add(
            Some(69),
            None,
            "task_created",
            json!({"status": "ready", "title": "t69", "goal_id": 701}),
            152 * DAY + 10,
        )
        .add(
            None,
            None,
            "finding_recorded",
            finding(53, "open"),
            152 * DAY + 11,
        )
        .add(
            None,
            None,
            "finding_status_changed",
            proposed10,
            152 * DAY + 12,
        )
        .add(
            None,
            None,
            "finding_status_changed",
            changed(53, "proposed", "resolved"),
            153 * DAY + 10,
        )
        .add(
            None,
            None,
            "finding_status_changed",
            changed(52, "open", "resolved"),
            154 * DAY + 1,
        );
    }
}

fn links_log(settled: usize) -> Log {
    history_with(settled, add_links(settled), |_| {})
}

#[test]
fn a_finding_settled_before_the_range_links_the_proposal_the_range_names() {
    let rest = Rest {
        links: true,
        ..Rest::default()
    };
    let (of_4756, _) = rest_part("F", &["finding"], rest, links_log, links_log);
    // Spike 4756's read of the findings (open by the range or with a
    // record in it) loses finding 51's link to proposal 9.
    assert!(
        of_4756
            .iter()
            .any(|miss| miss.contains("improvement_proposals")),
        "{of_4756:?}"
    );
}

#[test]
fn every_queue_kind_read_by_the_final_shapes_together_answers_as_every_row() {
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let large = rest_large();
    let mut counts = Vec::new();
    for (settled, log) in [
        (30, rest_full(30)),
        (30, rest_units(30)),
        (large, rest_units(large)),
        (3000, rest_units(3000)),
    ] {
        let rows = rows_of(&log);
        let kinds: BTreeSet<String> = rows
            .iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .map(|row| subject_kind(row).to_owned())
            .collect();
        let narrowed = queue_rest(&rows, from_ms, &kinds, REST);
        if settled < 3000 || large == 3000 {
            let misses = rest_misses(&rows, &narrowed);
            assert_eq!(misses, Vec::<String>::new(), "{settled}");
        }
        let all: Vec<&str> = kinds.iter().map(String::as_str).collect();
        let read = rest_reads(&narrowed, &all);
        eprintln!(
            "{settled}: [queue, saved, by sha] {read:?} of {} queue rows",
            queue_count_all(&rows)
        );
        counts.push((read, queue_count_all(&rows)));
    }
    assert_eq!(counts[1].0, counts[3].0, "{counts:?}");
    assert!(counts[3].1 > 50 * counts[1].1 / 2, "{counts:?}");
}

fn queue_count_all(rows: &[LedgerRow]) -> usize {
    rows.iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .count()
}

/// Promise 6 plus (A)'s run-less ask seeks do not cover application in the
/// range of an answer recorded before S. A later settled ask displaces both
/// last-before-S rows, and an answered ask is absent from the open-ask view.
#[test]
fn a_run_less_ask_answered_before_s_and_applied_in_range_is_a_counterexample() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "ask_opened",
        json!({"ask_id": 81, "kind": "planner_question"}),
        (READ_AT_DAY - 25) * DAY,
    )
    .add(
        None,
        None,
        "ask_answered",
        json!({"ask_id": 81}),
        (READ_AT_DAY - 24) * DAY,
    )
    .add(
        None,
        None,
        "ask_opened",
        json!({"ask_id": 82, "kind": "planner_question"}),
        (READ_AT_DAY - 23) * DAY,
    )
    .add(
        None,
        None,
        "ask_answered",
        json!({"ask_id": 82}),
        (READ_AT_DAY - 22) * DAY,
    )
    .add(
        None,
        None,
        "auto_repaired",
        json!({"ask_id": 81, "repair": "stale_lock", "layer": "runtime"}),
        (READ_AT_DAY - 1) * DAY,
    );
    let rows = rows_of(&log);
    let from_ms = (T0 + (READ_AT_DAY - 10) * DAY) * 1000;
    let kinds = ["ask_opened", "ask_answered", "auto_repaired"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let narrowed = queue_ii(
        &rows,
        from_ms,
        &kinds,
        QueueShape {
            asks: true,
            ..QueueShape::default()
        },
    );
    let records = |rows: &[LedgerRow]| {
        let mut events: Vec<RunEvent> = rows.iter().flat_map(|row| row.records.clone()).collect();
        events.sort_by_key(|e| e.id);
        events.dedup_by_key(|e| e.id);
        events
    };
    let after = log.events[3].id;
    let whole =
        crate::domain::stats::asks::human_waits(&records(&rows), after, log.last(), |_| true);
    let got =
        crate::domain::stats::asks::human_waits(&records(&narrowed), after, log.last(), |_| true);
    assert_eq!(whole, (vec![], vec![24 * DAY]));
    assert_eq!(got, (vec![], vec![]));
    let (misses, _) = queue_misses(&rows, &narrowed);
    assert!(
        misses.iter().any(|path| path.contains("ask_apply_wait")),
        "{misses:?}"
    );
}
