//! The areas of a run (ADR-t980-1 decisions 2 and 3): what the files its
//! landed commit changed are, by the repository's map in `dagq.toml`'s
//! `[areas]` (an area's name → globs, the rules of `--paths`). The globs may
//! overlap: a file counts in every area it matches, and a landing with a
//! file no area matches is in [`OTHER`] too. Nothing is stored: the areas
//! are derived whenever `stats` and `kpi` read, from the landed commits'
//! diffs in Git, so changing the map reclassifies every run.
use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use super::{RunEvent, RunId, scope};

/// The area of a landed file no area of the map matches.
pub const OTHER: &str = "other";
/// The value of a run without areas (not landed, or its commit unreadable)
/// in `kpi`'s strata, as a task without a change.
pub const UNKNOWN: &str = "unknown";
/// The name of every run together.
const ALL: &str = "all";
/// The longest area name, in bytes.
pub const MAX_NAME_LEN: usize = 64;

/// The `[areas]` map: each area's name and globs, in the file's order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AreaMap {
    areas: Vec<(String, Vec<String>)>,
}

/// Check an area's name: a lowercase slug (letters, digits, `-`, `_`) of
/// at most [`MAX_NAME_LEN`] bytes, none of the names the runtime gives
/// (`unknown`, `all`, `other`).
pub fn check_name(name: &str) -> Result<(), String> {
    let slug = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !slug {
        return Err(format!(
            "area {name:?} must be a lowercase slug (a-z, 0-9, - and _) of at most {MAX_NAME_LEN} bytes"
        ));
    }
    if [UNKNOWN, ALL, OTHER].contains(&name) {
        return Err(format!("area {name:?} is a name the runtime gives"));
    }
    Ok(())
}

impl AreaMap {
    /// The map of `areas` (name → globs), each name checked by
    /// [`check_name`] and given once, each with globs `--paths` accepts.
    pub fn new(areas: Vec<(String, Vec<String>)>) -> Result<Self, String> {
        for (index, (name, globs)) in areas.iter().enumerate() {
            check_name(name)?;
            if areas[..index].iter().any(|(seen, _)| seen == name) {
                return Err(format!("area {name} is defined twice"));
            }
            if globs.is_empty() {
                return Err(format!("area {name} has no glob"));
            }
            scope::validate_path_globs(globs).map_err(|error| format!("area {name}: {error}"))?;
        }
        Ok(Self { areas })
    }

    /// The areas' names, in the file's order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.areas.iter().map(|(name, _)| name.as_str())
    }

    /// The areas `paths` fall in, by name: every area a path matches, and
    /// [`OTHER`] when a path matches none. No paths, no areas.
    pub fn areas_of<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut hit: BTreeSet<&str> = BTreeSet::new();
        for path in paths {
            let mut matched = false;
            for (name, globs) in &self.areas {
                if globs.iter().any(|glob| scope::glob_matches(glob, path)) {
                    hit.insert(name);
                    matched = true;
                }
            }
            if !matched {
                hit.insert(OTHER);
            }
        }
        hit.into_iter().map(str::to_owned).collect()
    }
}

/// The commit each run landed, from its `run_integrated` (`commit`, or
/// `result_commit` of an older payload): the squash commit whose diff
/// against its first parent (`main_before`) is the run's change.
pub fn landed_commits(events: &[RunEvent]) -> Vec<(RunId, String)> {
    events
        .iter()
        .filter(|event| event.kind == "run_integrated")
        .filter_map(|event| {
            let commit = ["commit", "result_commit"]
                .iter()
                .find_map(|key| event.payload.get(*key).and_then(Value::as_str))
                .filter(|commit| !commit.is_empty())?;
            Some((event.run_id.clone()?, commit.to_owned()))
        })
        .collect()
}

/// Per run, the areas of what it landed. A run that did not land, or whose
/// commit `changed` (commit → the paths it changed) does not have, is not
/// listed: it is [`UNKNOWN`] to the strata.
pub type RunAreas = HashMap<RunId, Vec<String>>;

/// The areas of each landed run of `events` by `map`, its changes read in
/// `changed`.
pub fn run_areas(
    map: &AreaMap,
    events: &[RunEvent],
    changed: &HashMap<String, Vec<String>>,
) -> RunAreas {
    landed_commits(events)
        .into_iter()
        .filter_map(|(run, commit)| {
            let paths = changed.get(&commit)?;
            Some((run, map.areas_of(paths.iter().map(String::as_str))))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, TaskId};
    use serde_json::json;

    fn map() -> AreaMap {
        AreaMap::new(vec![
            (
                "runtime".into(),
                vec!["src/**".into(), "migrations/**".into()],
            ),
            ("stats".into(), vec!["src/domain/stats.rs".into()]),
            ("docs".into(), vec!["docs/**".into(), "*.md".into()]),
        ])
        .unwrap()
    }

    #[test]
    fn overlapping_globs_count_in_both_areas_and_unmatched_files_in_other() {
        let map = map();
        assert_eq!(
            map.areas_of(["src/domain/stats.rs", "README.md"]),
            ["docs", "runtime", "stats"]
        );
        assert_eq!(
            map.areas_of(["Cargo.toml", "src/lib.rs"]),
            ["other", "runtime"]
        );
        assert!(map.areas_of([]).is_empty());
        assert_eq!(
            map.names().collect::<Vec<_>>(),
            ["runtime", "stats", "docs"]
        );
    }

    #[test]
    fn refuses_names_and_globs_it_cannot_use() {
        let one = |name: &str, globs: &[&str]| {
            AreaMap::new(vec![(
                name.to_owned(),
                globs.iter().map(|glob| (*glob).to_owned()).collect(),
            )])
        };
        for name in ["Src", "", "all", "unknown", "other", "a.b", &"x".repeat(65)] {
            assert!(one(name, &["src/**"]).is_err(), "{name}");
        }
        assert!(one("src", &[]).unwrap_err().contains("no glob"));
        assert!(one("src", &["/src"]).unwrap_err().contains("area src"));
        let twice = AreaMap::new(vec![
            ("a".into(), vec!["x".into()]),
            ("a".into(), vec!["y".into()]),
        ]);
        assert!(twice.unwrap_err().contains("defined twice"));
    }

    #[test]
    fn landed_runs_get_the_areas_of_their_commit() {
        let event = |id: i64, run: &str, kind: &str, payload: Value| RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: "2026-09-29T00:00:00.000Z".into(),
            actor: None,
        };
        let events = [
            event(
                1,
                "r1",
                "run_integrated",
                json!({"commit": "c1", "main_before": "m"}),
            ),
            event(2, "r2", "run_integrated", json!({"result_commit": "c2"})),
            event(3, "r3", "run_integrated", json!({"commit": "gone"})),
            event(4, "r4", "run_claimed", json!({})),
        ];
        let changed = HashMap::from([
            ("c1".to_owned(), vec!["docs/a.md".to_owned()]),
            ("c2".to_owned(), vec!["build.rs".to_owned()]),
        ]);
        let areas = run_areas(&map(), &events, &changed);
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[&RunId::new("r1").unwrap()], ["docs"]);
        assert_eq!(areas[&RunId::new("r2").unwrap()], ["other"]);
        assert_eq!(landed_commits(&events).len(), 3);
    }
}
