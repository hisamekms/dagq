//! The areas of the landed runs (ADR-t980-1) as `stats`, `kpi` and the
//! reports read them: the `[areas]` map of the main checkout's `dagq.toml`
//! and the landed commits' changes read from Git, derived on every read and
//! never stored.
use std::{collections::HashMap, sync::Arc};

use anyhow::Result;

use crate::domain::{
    RunEvent,
    areas::{AreaMap, RunAreas, landed_commits, run_areas},
};

/// Reads the paths each of the commits changed (commit → paths); a
/// commit it cannot find is left out.
pub type ChangedPaths =
    Arc<dyn Fn(&[String]) -> Result<HashMap<String, Vec<String>>> + Send + Sync>;

/// The map and where the landed commits' changes are read.
#[derive(Clone)]
pub struct AreaReader {
    /// `None`: the repository has no `[areas]`, and no run has areas.
    pub map: Option<AreaMap>,
    pub changed: ChangedPaths,
    /// The areas of each landed commit (commit → areas) as a caller judged
    /// them with the map and Git itself (the queue service's client,
    /// ADR-t3102-3 decision 5): `map` and `changed` are not read then.
    pub judged: Option<Arc<HashMap<String, Vec<String>>>>,
}

impl std::fmt::Debug for AreaReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AreaReader")
            .field("map", &self.map)
            .finish_non_exhaustive()
    }
}

impl AreaReader {
    /// No `[areas]`: no run has areas and nothing is read.
    pub fn none() -> Self {
        Self {
            map: None,
            changed: Arc::new(|_| Ok(HashMap::new())),
            judged: None,
        }
    }

    /// The areas of the landed commits a caller judged (commit → areas);
    /// none judged is [`Self::none`]. Nothing is read.
    pub fn judged(areas: Option<HashMap<String, Vec<String>>>) -> Self {
        Self {
            judged: areas.map(Arc::new),
            ..Self::none()
        }
    }

    /// The areas of every landed run of `events`; `None` without a map. A
    /// Git that cannot be read leaves every run without areas (`unknown`).
    pub fn run_areas(&self, events: &[RunEvent]) -> Option<RunAreas> {
        if let Some(judged) = &self.judged {
            return Some(
                landed_commits(events)
                    .into_iter()
                    .filter_map(|(run, commit)| Some((run, judged.get(&commit)?.clone())))
                    .collect(),
            );
        }
        let map = self.map.as_ref()?;
        let commits: Vec<String> = landed_commits(events)
            .into_iter()
            .map(|(_, commit)| commit)
            .collect();
        Some(run_areas(map, events, &self.changed_of(&commits)))
    }

    /// The areas of each of `commits` (commit → areas), as [`Self::run_areas`]
    /// judges a landed run's from its commit: `None` without a map. A Git
    /// that cannot be read leaves every commit out, and none is read for
    /// no commit. The judged areas a caller gave are not read.
    pub fn commit_areas(&self, commits: &[String]) -> Option<HashMap<String, Vec<String>>> {
        let map = self.map.as_ref()?;
        Some(
            self.changed_of(commits)
                .into_iter()
                .map(|(commit, paths)| (commit, map.areas_of(paths.iter().map(String::as_str))))
                .collect(),
        )
    }

    /// The paths each of `commits` changed, none when Git cannot be read
    /// or there is no commit to read.
    fn changed_of(&self, commits: &[String]) -> HashMap<String, Vec<String>> {
        if commits.is_empty() {
            HashMap::new()
        } else {
            (self.changed)(commits).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, RunId, TaskId};
    use serde_json::json;

    #[test]
    fn reads_the_landed_commits_once_and_nothing_without_a_map() {
        let events = [RunEvent {
            id: EventId::new(1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("r").unwrap()),
            kind: "run_integrated".into(),
            payload: json!({"commit": "c"}),
            created_at: "2026-09-29T00:00:00.000Z".into(),
            actor: None,
        }];
        assert_eq!(AreaReader::none().run_areas(&events), None);
        let map = AreaMap::new(vec![("docs".into(), vec!["docs/**".into()])]).unwrap();
        let reader = AreaReader {
            map: Some(map.clone()),
            changed: Arc::new(|commits: &[String]| {
                assert_eq!(commits, ["c"]);
                Ok(HashMap::from([(
                    "c".to_owned(),
                    vec!["docs/a.md".to_owned()],
                )]))
            }),
            judged: None,
        };
        let areas = reader.run_areas(&events).unwrap();
        assert_eq!(areas[&RunId::new("r").unwrap()], ["docs"]);
        let failing = AreaReader {
            map: Some(map),
            changed: Arc::new(|_: &[String]| anyhow::bail!("no git")),
            judged: None,
        };
        assert!(failing.run_areas(&events).unwrap().is_empty());
        assert!(format!("{reader:?}").contains("AreaReader"));
        assert_eq!(AreaReader::none().run_areas(&[]), None);
    }

    /// Each commit's areas are the map's for the paths it changed, as a
    /// landed run's are; none without a map, every commit out when Git
    /// cannot be read, and Git is not asked for no commit.
    #[test]
    fn a_commit_s_areas_are_the_map_s_for_its_changes() {
        let map = AreaMap::new(vec![
            ("docs".into(), vec!["docs/**".into()]),
            ("runtime".into(), vec!["src/**".into()]),
        ])
        .unwrap();
        let reader = AreaReader {
            map: Some(map.clone()),
            changed: Arc::new(|commits: &[String]| {
                assert!(!commits.is_empty());
                Ok(HashMap::from([(
                    "c".to_owned(),
                    vec!["docs/a.md".to_owned(), "src/b.rs".to_owned()],
                )]))
            }),
            judged: None,
        };
        assert_eq!(
            reader.commit_areas(&["c".to_owned()]),
            Some(HashMap::from([(
                "c".to_owned(),
                vec!["docs".to_owned(), "runtime".to_owned()]
            )]))
        );
        assert_eq!(reader.commit_areas(&[]), Some(HashMap::new()));
        assert_eq!(AreaReader::none().commit_areas(&["c".to_owned()]), None);
        let failing = AreaReader {
            map: Some(map),
            changed: Arc::new(|_: &[String]| anyhow::bail!("no git")),
            judged: None,
        };
        assert_eq!(
            failing.commit_areas(&["c".to_owned()]),
            Some(HashMap::new())
        );
    }

    /// Areas a caller judged are the runs' as they are, with no map read
    /// nor Git asked; a commit it did not judge leaves its run out.
    #[test]
    fn judged_areas_are_taken_as_they_are() {
        let landed = |run: &str, commit: &str| RunEvent {
            id: EventId::new(1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: "run_integrated".into(),
            payload: json!({"commit": commit}),
            created_at: "2026-09-29T00:00:00.000Z".into(),
            actor: None,
        };
        let events = [landed("r", "c"), landed("s", "d")];
        let judged = AreaReader::judged(Some(HashMap::from([(
            "c".to_owned(),
            vec!["docs".to_owned(), "runtime".to_owned()],
        )])));
        let areas = judged.run_areas(&events).unwrap();
        assert_eq!(areas[&RunId::new("r").unwrap()], ["docs", "runtime"]);
        assert!(!areas.contains_key(&RunId::new("s").unwrap()));
        assert_eq!(AreaReader::judged(None).run_areas(&events), None);
    }
}
