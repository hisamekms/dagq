//! A [`QueueRecords`] of the tests of the context modules: it records what
//! a read asked of it and answers no other method.

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::Result;

use crate::application::QueueRecords;
use crate::domain::related::RelatedPage;
use crate::domain::search::{SearchPage, SearchQuery};
use crate::domain::{
    DraftOrigin, EventId, EventKind, FindingQuery, FindingView, GoalId, RunEvent, TaskChange,
    TaskId,
};

/// What the reads asked: `related_tasks`' arguments and `findings`' query.
#[derive(Default)]
pub(super) struct Records {
    pub related: RefCell<Vec<(TaskId, Vec<String>, usize)>>,
    pub findings: RefCell<Vec<FindingQuery>>,
}

impl QueueRecords for Records {
    fn related_landed_commits(&self, _: TaskId, _: usize) -> Result<Vec<String>> {
        unimplemented!()
    }
    fn related_tasks(
        &self,
        task: TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<RelatedPage> {
        self.related
            .borrow_mut()
            .push((task, statuses.to_vec(), limit));
        Ok(RelatedPage {
            task_id: task.as_i64(),
            related: Vec::new(),
            total: 0,
        })
    }
    fn search_documents(&self, _: &SearchQuery) -> Result<SearchPage> {
        unimplemented!()
    }
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        unimplemented!()
    }
    fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        unimplemented!()
    }
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        unimplemented!()
    }
    fn findings(&self, query: &FindingQuery) -> Result<Vec<FindingView>> {
        self.findings.borrow_mut().push(query.clone());
        Ok(Vec::new())
    }
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>> {
        unimplemented!()
    }
    fn record_report_written(&self, _: serde_json::Value) -> Result<bool> {
        unimplemented!()
    }
    fn kpi_breaches_open(&self) -> Result<Vec<serde_json::Value>> {
        unimplemented!()
    }
    fn record_kpi_breach(
        &self,
        _: EventKind,
        _: serde_json::Value,
        _: Option<(i64, usize)>,
    ) -> Result<Option<serde_json::Value>> {
        unimplemented!()
    }
    fn record_kpi_push_abandoned(&self, _: serde_json::Value) -> Result<bool> {
        unimplemented!()
    }
    fn draft_origins(&self) -> Result<HashMap<TaskId, DraftOrigin>> {
        unimplemented!()
    }
    fn record_forecast(&self, _: serde_json::Value, _: Option<EventId>) -> Result<Option<EventId>> {
        unimplemented!()
    }
    fn ci_watch_events(&self) -> Result<Vec<RunEvent>> {
        unimplemented!()
    }
    fn record_ci_check(
        &self,
        _: crate::domain::ci_watch::CiCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>> {
        unimplemented!()
    }
    fn ci_failure_findings_of(&self, _: TaskId) -> Result<Vec<crate::domain::FindingId>> {
        unimplemented!()
    }
}
