//! A planner of the runtime's as the tests play it (ADR-t1433-2): its
//! wrapper parked in the background (`BackgroundWrappers::parked`), the
//! test reads the requests the supervisor writes to its `turns/`, takes
//! them as its turns and leaves it idle. Shared by the plan review,
//! planner and finding tests (task 1441).

use dagq::{
    application::planner_idle_marker,
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};

/// The requests the supervisor wrote to the `turns/` of headless planner
/// `planner`, taken or not, in order: each its `seq`, `what` and `prompt`.
pub fn turn_requests(db: &Path, planner: dagq::domain::PlannerId) -> Vec<Value> {
    let turns = planners_dir(db).join(planner.to_string()).join("turns");
    let mut requests: Vec<Value> = fs::read_dir(&turns)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .and_then(dagq::domain::turn::request_seq)
                        .is_some()
                        && path
                            .extension()
                            .is_some_and(|extension| extension == "json")
                })
                .map(|path| serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap())
                .collect()
        })
        .unwrap_or_default();
    requests.sort_by_key(|request| request["seq"].as_u64());
    requests
}

/// Play the parked wrapper of headless planner `planner` through one turn
/// per request waiting in its `turns/`: each is taken, its turn recorded as
/// started and finished on the queue, and the planner left idle (its marker
/// written after them).
pub fn take_turns(queue: &SqliteQueue, db: &Path, planner: dagq::domain::PlannerId) {
    let dir = planners_dir(db).join(planner.to_string());
    for request in turn_requests(db, planner) {
        let seq = request["seq"].as_u64().unwrap();
        let path = dagq::domain::turn::request_path(&dir, seq);
        if !path.exists() {
            continue;
        }
        fs::rename(&path, dagq::domain::turn::taken_path(&dir, seq)).unwrap();
        for (kind, payload) in [
            (
                dagq::domain::EventKind::TurnStarted,
                json!({"planner_id": planner, "turn": seq + 1, "request": seq, "what": request["what"]}),
            ),
            (
                dagq::domain::EventKind::TurnFinished,
                json!({"planner_id": planner, "turn": seq + 1, "outcome": "succeeded"}),
            ),
        ] {
            queue.record_queue_event(kind, payload).unwrap();
        }
    }
    std::thread::sleep(Duration::from_millis(20));
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
}

/// Whether the supervisor asked headless planner `planner` to exit: the
/// exit request in its `turns/`.
pub fn exit_requested(db: &Path, planner: dagq::domain::PlannerId) -> bool {
    dagq::domain::turn::exit_path(&planners_dir(db).join(planner.to_string())).is_file()
}

/// Make `planner` alive (its wrapper is this test process) and idle.
pub fn idle(queue: &SqliteQueue, db: &Path, planner: dagq::domain::PlannerId) {
    queue
        .register_planner_wrapper(planner, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planner, std::process::id(), std::process::id())
        .unwrap();
    let dir = planners_dir(db).join(planner.to_string());
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
}

/// Make writing the request `later` requests after the next to the
/// `turns/` of headless planner `planner` fail (0: the next): a directory
/// stands where the request is written before it is renamed into place.
/// The exit request is still written.
pub fn block_next_request(db: &Path, planner: dagq::domain::PlannerId, later: u64) {
    let dir = planners_dir(db).join(planner.to_string());
    let turns = dagq::domain::turn::turns_dir(&dir);
    fs::create_dir_all(&turns).unwrap();
    let names: Vec<String> = fs::read_dir(&turns)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let seq = dagq::domain::turn::next_seq(names.iter().map(String::as_str)) + later;
    fs::create_dir_all(dagq::domain::turn::request_path(&dir, seq).with_extension("json.tmp"))
        .unwrap();
}
