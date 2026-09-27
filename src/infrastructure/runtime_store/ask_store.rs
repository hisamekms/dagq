//! The asks of the queue ([`AskStore`]).

use super::*;

impl AskStore for SqliteQueue {
    fn open_update_ask(
        &mut self,
        kind: crate::domain::AskKind,
        question: &str,
        options: &[&str],
        asked_by: &str,
        subject: Option<&str>,
    ) -> Result<crate::domain::Ask> {
        SqliteQueue::open_update_ask(self, kind, question, options, asked_by, subject)
    }
    fn release_updates_on(&self) -> bool {
        SqliteQueue::release_updates_on(self)
    }
    fn update_answers(&self, kind: &crate::domain::AskKind) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::update_answers(self, kind)
    }
    fn asks(&self, query: crate::application::AskQuery) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::asks(self, query)
    }
    fn has_unclosed_ask(&self, run_id: &RunId, kind: crate::domain::AskKind) -> Result<bool> {
        SqliteQueue::has_unclosed_ask(self, run_id, kind)
    }
    fn ask(&mut self, ask: crate::domain::NewAsk) -> Result<crate::domain::AskOutcome> {
        SqliteQueue::ask(self, ask)
    }
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome> {
        SqliteQueue::hold(self, hold)
    }
    fn hold_of(&self, run_id: &RunId) -> Result<Option<crate::domain::Ask>> {
        SqliteQueue::hold_of(self, run_id)
    }
    fn hold_unclosed(&self, run_id: &RunId) -> Result<bool> {
        SqliteQueue::hold_unclosed(self, run_id)
    }
    fn close_hold_asks(
        &mut self,
        reason: crate::domain::AskReason,
        subject: Option<&str>,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_hold_asks(self, reason, subject, answer)
    }
    fn read_ask(&self, id: AskId) -> Result<crate::domain::Ask> {
        SqliteQueue::read_ask(self, id)
    }
    fn answer_as(
        &mut self,
        id: AskId,
        text: &str,
        answerer: crate::domain::Answerer,
    ) -> Result<crate::domain::Ask> {
        SqliteQueue::answer_as(self, id, text, answerer)
    }
    fn close_ask(&mut self, id: AskId) -> Result<crate::domain::Ask> {
        SqliteQueue::close_ask(self, id)
    }
    fn landing_answers(&self) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::landing_answers(self)
    }
    fn triage_answers(&self) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::triage_answers(self)
    }
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::undelivered_answers(self, run_id)
    }
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<crate::domain::Ask> {
        SqliteQueue::ask_delivered(self, id, workspace_id)
    }
    fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>> {
        SqliteQueue::last_worker_question_closed(self, run_id)
    }
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool> {
        SqliteQueue::has_unclosed_worker_question(self, run_id)
    }
    fn has_unclosed_worker_question_since(
        &self,
        run_id: &RunId,
        created_from: i64,
    ) -> Result<bool> {
        SqliteQueue::has_unclosed_worker_question_since(self, run_id, created_from)
    }
    fn close_stuck_exit_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_stuck_exit_asks(self, run_id, answer)
    }
    fn close_answer_prompt_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_answer_prompt_asks(self, run_id, answer)
    }
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<crate::domain::Ask>> {
        SqliteQueue::unclosed_stalled_ask(self, run_id)
    }
    fn close_stalled_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_stalled_asks(self, run_id, answer)
    }
    fn end_stalled_detections(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::end_stalled_detections(self, run_id, answer)
    }
    fn note_on_asks(
        &mut self,
        run_id: &RunId,
        note: &str,
        why: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::note_on_asks(self, run_id, note, why)
    }
    fn close_approve_landing_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_approve_landing_asks(self, run_id, answer)
    }
    fn close_blocked_asks(
        &mut self,
        run_id: &RunId,
        task_id: crate::domain::TaskId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_blocked_asks(self, run_id, task_id, answer)
    }
}
