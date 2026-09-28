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
        }
    }

    /// The areas of every landed run of `events`; `None` without a map. A
    /// Git that cannot be read leaves every run without areas (`unknown`).
    pub fn run_areas(&self, events: &[RunEvent]) -> Option<RunAreas> {
        let map = self.map.as_ref()?;
        let commits: Vec<String> = landed_commits(events)
            .into_iter()
            .map(|(_, commit)| commit)
            .collect();
        let changed = if commits.is_empty() {
            HashMap::new()
        } else {
            (self.changed)(&commits).unwrap_or_default()
        };
        Some(run_areas(map, events, &changed))
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
        };
        let areas = reader.run_areas(&events).unwrap();
        assert_eq!(areas[&RunId::new("r").unwrap()], ["docs"]);
        let failing = AreaReader {
            map: Some(map),
            changed: Arc::new(|_: &[String]| anyhow::bail!("no git")),
        };
        assert!(failing.run_areas(&events).unwrap().is_empty());
        assert!(format!("{reader:?}").contains("AreaReader"));
        assert_eq!(AreaReader::none().run_areas(&[]), None);
    }
}
