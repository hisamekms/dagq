//! What the supervisor types into a live session, and whether it got there
//! (task 285): the Enter a long paste swallowed is sent again without the
//! text, a text still in the input box after that is recorded
//! (`submit_unconfirmed`), and a session that shows no sign of work after a
//! request or an answer ([`StartCheck`]) is sent it again or recorded
//! (`submit_not_started`). Either goes to the session's recovery job as the
//! `stalled` alert (ADR-0047 decision 31, `stall_recovery.rs`) before any
//! ask. Every `send_text` and `send_exit` of the supervisor goes through
//! [`submit`].

use super::*;
use crate::domain::EventKind;

/// What the supervisor types into a session.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Input<'a> {
    /// A request or an answer, typed and submitted with Enter.
    Text(&'a str),
    /// `/exit`, never typed twice: a second one could pick a dialog's
    /// option.
    Exit,
}

impl Input<'_> {
    fn text(&self) -> &str {
        match self {
            Input::Text(text) => text,
            Input::Exit => "/exit",
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Input::Text(_) => "text",
            Input::Exit => "exit",
        }
    }
}

/// Enter is sent again at most this many times after a submit.
pub(crate) const SUBMIT_RETRIES: usize = 3;

/// Where a submit ended, with the last screen read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Submission {
    /// The input left the input box; `None` when the screen could not be
    /// read, which is not held against the send.
    Submitted(Option<String>),
    /// A dialog is on the screen: no Enter is sent over it.
    Dialog(String),
    /// The input is still in the box after [`SUBMIT_RETRIES`] Enters.
    Stuck(String),
    /// A `/exit` timed out on every attempt, the screen showing each time
    /// that it did not get there (task 354): the session was not asked.
    Unsent,
    /// Written as the next request (or the exit request) of a headless
    /// session (ADR-t813-1): the turn that runs it is its sign.
    Queued,
}

impl Submission {
    /// The screen read after the submit, if any.
    pub(crate) fn screen(&self) -> Option<&str> {
        match self {
            Submission::Submitted(screen) => screen.as_deref(),
            Submission::Dialog(screen) | Submission::Stuck(screen) => Some(screen),
            Submission::Unsent | Submission::Queued => None,
        }
    }
}

/// Answers a known dialog on a session's screen by rule (ADR-0047 decision
/// 29): whether keys were sent to it; a screen without one gets nothing.
/// The supervisor's closes the Settings panel ([`answer_send_dialog`]),
/// which is no dialog to [`AgentSignals::detect_prompt`].
pub(crate) type DialogAnswerer<'b> = &'b mut dyn FnMut(&str) -> bool;

/// Type `input` into the session in `workspace` and read the screen
/// every `submit_check_interval`: while the input box still holds it (and
/// no dialog is up) Enter alone is sent again, at most
/// [`SUBMIT_RETRIES`] times. Returns the outcome and the Enters sent
/// again. An error is a failed typing of the input; an Enter that fails
/// after it leaves the input stuck in the box. A typing that timed out
/// with the input maybe typed is judged from the screen like one that
/// returned, and so is one that failed with the input still in the box
/// (its Enter failed, task 353). A `/exit` is typed again after a timeout
/// only while the screen shows the input box ready with no trace of it
/// (task 354); one that timed out on every attempt that way is
/// [`Submission::Unsent`].
pub(crate) fn submit_input(
    cmux: &dyn WorkspaceBackend,
    signals: &dyn AgentSignals,
    workspace: &str,
    input: Input<'_>,
) -> Result<(Submission, usize)> {
    submit_input_answering(cmux, signals, workspace, input, None)
}

/// [`submit_input`], with `answer` given the screens read (task 480): a
/// dialog up before the input is typed is answered first, so the input
/// does not go into it, and one that came up after it is answered once,
/// after which the input is confirmed as before (Enter alone while it is in
/// the box; a `/exit` is never typed again). A dialog `answer` sends
/// nothing to is [`Submission::Dialog`], as without it.
pub(crate) fn submit_input_answering(
    cmux: &dyn WorkspaceBackend,
    signals: &dyn AgentSignals,
    workspace: &str,
    input: Input<'_>,
    mut answer: Option<DialogAnswerer<'_>>,
) -> Result<(Submission, usize)> {
    if let Some(answer_now) = answer.as_deref_mut()
        && let Ok(screen) = cmux.capture(workspace)
        && answer_now(&screen)
    {
        info!(
            "a dialog in workspace {workspace} was answered before the {} was typed",
            input.name()
        );
        thread::sleep(cmux.submit_check_interval());
        // At most one answer per submit.
        answer = None;
    }
    let typed = match input {
        Input::Text(text) => cmux.send_text(workspace, text),
        Input::Exit => cmux.send_exit_when(workspace, &|screen| exit_unsent_on(signals, screen)),
    };
    match typed {
        Ok(()) => (),
        Err(error) if exit_unsent(&error) => {
            warn!(error = %format_args!("{error:#}"), "/exit for workspace {workspace} timed out on every attempt without reaching the session: {error:#}");
            return Ok((Submission::Unsent, 0));
        }
        Err(error) if timed_out_maybe_sent(&error) => {
            warn!(error = %format_args!("{error:#}"), "{} for workspace {workspace} timed out and may have been typed; reading the screen for it: {error:#}", input.name());
        }
        // The backend types the input and then presses Enter: an Enter
        // that failed leaves the input in the box (task 353), where it gets
        // Enter alone again like one the paste swallowed.
        Err(error) => match cmux.capture(workspace) {
            Ok(screen)
                if signals.detect_prompt(&screen).is_none()
                    && signals.input_pending(&screen, input.text()) =>
            {
                warn!(error = %format_args!("{error:#}"), "{} for workspace {workspace} failed but is in the input box; sending Enter again: {error:#}", input.name());
            }
            _ => return Err(error),
        },
    }
    Ok(confirm_input(cmux, signals, workspace, input, answer))
}

/// Read the screen after `input` was typed every `submit_check_interval`
/// and send Enter alone while the input box holds it, at most
/// [`SUBMIT_RETRIES`] times. The screens are given to `answer` until it
/// sends keys once, after which the reads go on; a dialog it did not
/// answer is [`Submission::Dialog`].
pub(crate) fn confirm_input(
    cmux: &dyn WorkspaceBackend,
    signals: &dyn AgentSignals,
    workspace: &str,
    input: Input<'_>,
    mut answer: Option<DialogAnswerer<'_>>,
) -> (Submission, usize) {
    let mut retries = 0;
    loop {
        thread::sleep(cmux.submit_check_interval());
        let screen = match cmux.capture(workspace) {
            Ok(screen) => screen,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "screen of workspace {workspace} could not be read after a submit: {error:#}");
                return (Submission::Submitted(None), retries);
            }
        };
        if answer.as_deref_mut().is_some_and(|answer| answer(&screen)) {
            info!(
                "a dialog in workspace {workspace} was answered after the {} was typed; reading the screen for it again",
                input.name()
            );
            answer = None;
            continue;
        }
        if signals.detect_prompt(&screen).is_some() {
            return (Submission::Dialog(screen), retries);
        }
        if !signals.input_pending(&screen, input.text()) {
            return (Submission::Submitted(Some(screen)), retries);
        }
        if retries == SUBMIT_RETRIES {
            return (Submission::Stuck(screen), retries);
        }
        if let Err(error) = cmux.send_enter(workspace) {
            warn!(error = %format_args!("{error:#}"), "Enter could not be sent again to workspace {workspace}: {error:#}");
            return (Submission::Stuck(screen), retries);
        }
        retries += 1;
    }
}

/// Whether `screen` shows that a `/exit` did not get there: the input box
/// is drawn with no dialog over it, and its last lines hold no trace of the
/// `/exit` (typed, or submitted into the transcript).
fn exit_unsent_on(signals: &dyn AgentSignals, screen: &str) -> bool {
    signals.input_ready(screen)
        && signals.detect_prompt(screen).is_none()
        && !text_on_screen(screen, Input::Exit.text())
}

/// [`submit_input`] into `run`'s session, `what` naming the input in the
/// records. Enters sent again are recorded as `submit_retried`; an input
/// still in the box as `submit_unconfirmed`, which for a text the session's
/// watch hands to its recovery job (a `/exit` becomes the `stuck_exit`
/// alert of its exit timeout). An error is only a failed typing: the input
/// was typed once it returns, so a record that fails after it is only
/// noted. The Settings panel over the input box, before or after the
/// typing, is closed by rule ([`answer_send_dialog`]; task 480).
pub(super) fn submit(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    input: Input<'_>,
    what: &str,
) -> Result<Submission> {
    // A headless session takes no keys: its next turn's request is
    // written instead (ADR-t813-1 decision 2).
    if headless(run) {
        return request_turn(sv, run, workspace, input, what);
    }
    let (cmux, signals) = (sv.cmux, sv.signals);
    let mut answer = |screen: &str| answer_send_dialog(sv, run, workspace, screen);
    let (submission, retries) =
        submit_input_answering(cmux, signals, workspace, input, Some(&mut answer))?;
    record_submission(sv, run, workspace, input, what, &submission, retries);
    Ok(submission)
}

/// Record what [`submit_input`] did with `input` into `run`'s session:
/// Enters sent again as `submit_retried` (and `auto_repaired` when they got
/// it through), an input still in the box as `submit_unconfirmed`. A
/// record that fails is only noted.
pub(super) fn record_submission(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    input: Input<'_>,
    what: &str,
    submission: &Submission,
    retries: usize,
) {
    let note = |sv: &mut Supervisor<'_>, kind: EventKind, payload: Value| {
        if let Err(error) = sv.queue.record_runtime_event(run.id(), kind, payload) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{kind} of {} could not be recorded: {error:#}", run.id());
        }
    };
    if retries > 0 {
        note(
            sv,
            EventKind::SubmitRetried,
            json!({
                "workspace_id": workspace,
                "input": input.name(),
                "what": what,
                "retries": retries,
                "submitted": !matches!(submission, Submission::Stuck(_)),
            }),
        );
        info!(run_id = %run.id(), "{what} stayed in the input box of workspace {workspace}; Enter sent again {retries} times");
        // Enters that got the input through, as the screen shows, are a
        // repair (ADR-0047 decision 38); ones that did not go on to the ask
        // below, and a dialog or an unread screen confirms nothing.
        if matches!(submission, Submission::Submitted(Some(_))) {
            note(
                sv,
                EventKind::AutoRepaired,
                json!({
                    "layer": "runtime",
                    "repair": "submit_enter_retry",
                    "conditions": {"input": input.name(), "retries": retries, "submitted": true},
                    "detail": {"workspace_id": workspace, "what": what},
                }),
            );
        }
    }
    if let Submission::Stuck(screen) = submission {
        let excerpt = sv.signals.screen_excerpt(screen);
        note(
            sv,
            EventKind::SubmitUnconfirmed,
            json!({
                "workspace_id": workspace,
                "input": input.name(),
                "what": what,
                "retries": retries,
                "excerpt": excerpt,
            }),
        );
        warn!(run_id = %run.id(), "{what} is still in the input box of workspace {workspace} after {retries} Enters");
    }
}

/// Raise a resumed session whose input box never got ready for its request
/// as an `answer_prompt` ask to the inbox, the way a dialog is (the open ask
/// of the run is not registered twice). The runtime sends nothing more: the
/// person has the key or text sent, and the ask closes itself once the
/// session exits. A failed ask is only noted. A send the session did not
/// take is not asked here: it goes to its recovery job first.
pub(super) fn ask_unsubmitted(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    situation: &str,
    excerpt: &str,
) {
    let question = format!(
        "The session of run {run_id} (task {task_id}) in workspace {workspace}: {situation}. Answer with what to send to it (for example `enter` to press Enter, or the text to type), or what to do instead; it is done in that workspace, and this ask closes itself once the session exits.\n\nLast lines of the screen:\n{excerpt}",
        run_id = run.id(),
        task_id = run.task_id(),
    );
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.main_checkout,
        NewAsk {
            kind: AskKind::AnswerPrompt,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options: Vec::new(),
            asked_by: SessionRole::Supervisor.as_str().into(),
            reason_category: AskReason::RecoveryFailed,
            topics: Vec::new(),
            finding_id: None,
        },
        sv.cmux,
    );
    match outcome {
        Ok(outcome) => {
            info!(ask_id = %outcome["id"], run_id = %run.id(), "answer_prompt ask {} for {}: {situation} (notified: {})", outcome["id"], run.id(), outcome["notified"])
        }
        Err(error) => {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "answer_prompt ask for {} could not be opened: {error:#}", run.id())
        }
    }
}

/// What a session's screen says, [`StartCheck::wait`] after it was sent a
/// request or an answer, of whether it took it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartSign {
    /// At work, or the transcript moved on since the submit.
    Started,
    /// A dialog holds the session.
    Dialog(&'static str),
    /// The input box is drawn and no longer holds the text, but nothing
    /// happened: the text was lost.
    Lost,
    /// The text is still in the input box, or there is no input box.
    Held,
}

/// Judge `screen`, read a while after `text` was submitted, against
/// `submitted` (the screen read right after the submit, if any). Only a
/// change of the transcript ([`AgentSignals::transcript`]) is a sign of
/// work: a status line whose clock or cost ticks, or a notification under
/// the input box, does not hide a lost text (task 319).
pub(super) fn start_sign(
    signals: &dyn AgentSignals,
    screen: &str,
    submitted: Option<&str>,
    text: &str,
) -> StartSign {
    if let Some(kind) = signals.detect_prompt(screen) {
        return StartSign::Dialog(kind);
    }
    if signals.working(screen)
        || submitted.is_some_and(|before| signals.transcript(before) != signals.transcript(screen))
    {
        return StartSign::Started;
    }
    if signals.input_ready(screen) && !signals.input_pending(screen, text) {
        StartSign::Lost
    } else {
        StartSign::Held
    }
}

/// How long a sent text may show no sign of being taken before the
/// supervisor reads the screen for it: `[stall].send_confirm_secs`
/// (ADR-0043 decision 2).
pub(super) fn confirm_wait(stall: &StallConfig) -> Duration {
    stall.send_confirm()
}

/// `wait` in whole seconds, rounded up, as events record it: a wait a test
/// set below a second (task 1045) is one second, as its setting's seconds
/// are.
fn whole_secs(wait: Duration) -> u64 {
    wait.as_secs() + u64::from(wait.subsec_nanos() > 0)
}

/// The files whose writing after a send shows the session took it: its
/// idle marker and its receipt. The input marker (the agent's hook writes
/// it as the input is taken; a provider without one never has it) is read
/// apart: a notice the agent put in by itself is not the text taken.
fn taken_marks(run: &TaskRun, idle_marker: &Path) -> Vec<PathBuf> {
    let mut marks = vec![idle_marker.to_path_buf()];
    if let Some(receipt) = run.receipt_path() {
        marks.push(PathBuf::from(receipt));
    }
    marks
}

/// Watches a request or an answer sent to a live session until the session
/// shows a sign of work: its idle marker, input marker or receipt written
/// after it, the agent at work, or its transcript changed. With none after
/// [`confirm_wait`], a text the input box lost is sent once more
/// (`submit_resent`); otherwise, or when that is lost too, the run records
/// `submit_not_started` (as for a dialog that came up) and the session's
/// recovery job looks at it (the `stalled` alert), instead of waiting out
/// the resume timeout.
#[derive(Debug, Clone)]
pub(super) struct StartCheck {
    what: String,
    text: String,
    sent: Instant,
    /// The idle marker is compared with this, on the files' wall clock.
    sent_at: SystemTime,
    submitted: Option<String>,
    resent: bool,
    /// A sign was seen, or `submit_not_started` was recorded: nothing more
    /// to check.
    done: bool,
}

impl StartCheck {
    /// Watch `text`, submitted at `sent_at` as `submission`: a text stuck
    /// in the box was raised by [`submit`] already.
    pub(super) fn new(
        what: &str,
        text: &str,
        sent_at: SystemTime,
        submission: &Submission,
    ) -> Self {
        Self {
            what: what.to_owned(),
            text: text.to_owned(),
            sent: Instant::now(),
            sent_at,
            submitted: submission.screen().map(str::to_owned),
            resent: false,
            // A request of a headless session needs no check: its turn
            // starts and ends.
            done: matches!(submission, Submission::Stuck(_) | Submission::Queued),
        }
    }

    /// Watch `text` that a supervisor this one adopted the run from
    /// recorded as sent at `sent_at` (task 546): it may have stopped before
    /// typing it, and no screen after the submit was read. The wait runs
    /// from now; a mark written after `sent_at`, or the agent at work, shows
    /// it was typed, and an empty input box with neither has it sent once,
    /// unless it was `resent` already.
    pub(super) fn adopted(what: &str, text: &str, sent_at: SystemTime, resent: bool) -> Self {
        Self {
            resent,
            ..Self::new(what, text, sent_at, &Submission::Submitted(None))
        }
    }

    /// Whether the screen is to be read for a sign: the check is not done,
    /// `wait` passed since the send, and none of `marks` nor the input
    /// marker `input` (unless a notice of the agent's own) was written
    /// after it. A mark written after it is the text taken: the check is
    /// done, with no Enter sent again and no ask.
    fn due(
        &mut self,
        files: &dyn RunFiles,
        wait: Duration,
        marks: &[PathBuf],
        input: Option<InputMarker>,
    ) -> bool {
        if self.done || self.sent.elapsed() < wait {
            return false;
        }
        let input_taken = input.is_some_and(|input| {
            input.source != InputSource::Agent && input.modified > self.sent_at
        });
        if input_taken
            || marks.iter().any(|mark| {
                files
                    .modified(mark)
                    .is_ok_and(|modified| modified > self.sent_at)
            })
        {
            self.done = true;
            return false;
        }
        true
    }

    /// One observation of the session in `workspace`.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
    ) -> Result<()> {
        let wait = confirm_wait(&sv.stall);
        if self.done || self.sent.elapsed() < wait {
            return Ok(());
        }
        let input = InputMarker::read(&*sv.files, sv.signals, idle_marker)?;
        if !self.due(&*sv.files, wait, &taken_marks(run, idle_marker), input) {
            return Ok(());
        }
        let screen = match sv.cmux.capture(workspace) {
            Ok(screen) => screen,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for a sign of work: {error:#}", run.id());
                return Ok(());
            }
        };
        // A known dialog is answered by rule (ADR-0047 decisions 29 and 31,
        // task 480): the text is then confirmed again (Enter alone while it
        // is in the box) and the wait starts over.
        if answer_known_dialog(sv, run, workspace, &screen, false, None)? {
            return self.confirm_after_dialog(sv, run, workspace);
        }
        let sign = start_sign(sv.signals, &screen, self.submitted.as_deref(), &self.text);
        let excerpt = sv.signals.screen_excerpt(&screen);
        match sign {
            StartSign::Started => self.done = true,
            StartSign::Dialog(kind) => {
                return self.not_started_at_dialog(sv, run, workspace, kind, &excerpt);
            }
            StartSign::Lost if !self.resent => {
                sv.queue.record_runtime_event(
                    run.id(),
                    EventKind::SubmitResent,
                    json!({
                        "workspace_id": workspace,
                        "what": self.what,
                        "waited_secs": whole_secs(wait),
                    }),
                )?;
                info!(run_id = %run.id(), "session of {} showed no sign of the {} within {}s and its input box is empty; sending it again", run.id(), self.what, whole_secs(wait));
                let sent_at = sv.files.now();
                let text = self.text.clone();
                let what = self.what.clone();
                let submission = submit(sv, run, workspace, Input::Text(&text), &what)?;
                *self = Self::new(&what, &text, sent_at, &submission);
                self.resent = true;
            }
            StartSign::Lost | StartSign::Held => {
                self.done = true;
                sv.queue.record_runtime_event(
                    run.id(),
                    EventKind::SubmitNotStarted,
                    json!({
                        "workspace_id": workspace,
                        "what": self.what,
                        "waited_secs": whole_secs(wait),
                        "resent": self.resent,
                        "excerpt": excerpt,
                    }),
                )?;
                warn!(run_id = %run.id(), "session of {} showed no sign of the {} within {}s; its recovery job looks at it", run.id(), self.what, whole_secs(wait));
            }
        }
        Ok(())
    }

    /// Confirm the text again once a known dialog over the session was
    /// answered: Enter alone while it is in the box ([`confirm_input`]),
    /// recorded as [`submit`] records it. A dialog still up is
    /// `submit_not_started`; otherwise the check starts over from now, a
    /// text the dialog took then sent once more like any lost one.
    fn confirm_after_dialog(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
    ) -> Result<()> {
        let text = self.text.clone();
        let what = self.what.clone();
        let input = Input::Text(&text);
        let (submission, retries) = confirm_input(sv.cmux, sv.signals, workspace, input, None);
        record_submission(sv, run, workspace, input, &what, &submission, retries);
        if let Submission::Dialog(after) = &submission {
            let kind = sv.signals.detect_prompt(after).unwrap_or("dialog");
            let excerpt = sv.signals.screen_excerpt(after);
            return self.not_started_at_dialog(sv, run, workspace, kind, &excerpt);
        }
        // Work the transcript shows since the send, under the dialog, is the
        // text taken.
        if let Submission::Submitted(Some(after)) = &submission
            && start_sign(sv.signals, after, self.submitted.as_deref(), &text) == StartSign::Started
        {
            self.done = true;
            return Ok(());
        }
        info!(run_id = %run.id(), "the dialog over the session of {} after the {what} was answered; waiting again for a sign of it", run.id());
        let resent = self.resent;
        *self = Self::new(&what, &text, sv.files.now(), &submission);
        self.resent = resent;
        Ok(())
    }

    /// A dialog holds the session after the send: `submit_not_started` with
    /// its kind, which the session's recovery job looks at.
    fn not_started_at_dialog(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        kind: &str,
        excerpt: &str,
    ) -> Result<()> {
        self.done = true;
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::SubmitNotStarted,
            json!({
                "workspace_id": workspace,
                "what": self.what,
                "waited_secs": whole_secs(confirm_wait(&sv.stall)),
                "resent": self.resent,
                "dialog": kind,
                "excerpt": excerpt,
            }),
        )?;
        warn!(run_id = %run.id(), "a {kind} dialog came up in the session of {} after the {}; its recovery job looks at it", run.id(), self.what);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{Queue, QueueOpener};
    use crate::application::{SupervisorEnvironment, WorkspaceTags};
    use crate::domain::{Task, TaskRun};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    /// A session whose screen is one of `screens` per capture (the last
    /// one repeats), recording what was sent. The first `send_timeouts`
    /// texts and `capture_timeouts` captures time out as cmux does under
    /// load, and so does every `/exit` with `exit_times_out`.
    struct Backend {
        screens: Mutex<Vec<String>>,
        sent: Mutex<Vec<String>>,
        send_timeouts: AtomicUsize,
        capture_timeouts: AtomicUsize,
        exit_times_out: bool,
        /// Every `/exit` is typed but its Enter fails, not by a timeout.
        exit_enter_fails: bool,
        /// Every text is typed but its Enter fails, not by a timeout.
        text_enter_fails: bool,
    }

    impl Backend {
        fn new(screens: &[&str]) -> Self {
            Self {
                screens: Mutex::new(screens.iter().rev().map(|s| (*s).to_owned()).collect()),
                sent: Mutex::new(Vec::new()),
                send_timeouts: AtomicUsize::new(0),
                capture_timeouts: AtomicUsize::new(0),
                exit_times_out: false,
                exit_enter_fails: false,
                text_enter_fails: false,
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl WorkspaceBackend for Backend {
        fn preflight(&self) -> Result<()> {
            unimplemented!()
        }
        fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
            unimplemented!()
        }
        fn create(&self, _: &Task, _: &TaskRun, _: &str, _: &WorkspaceTags) -> Result<String> {
            unimplemented!()
        }
        fn create_resume(
            &self,
            _: &Task,
            _: &TaskRun,
            _: &str,
            _: &WorkspaceTags,
        ) -> Result<String> {
            unimplemented!()
        }
        fn send_text(&self, _: &str, text: &str) -> Result<()> {
            self.sent.lock().unwrap().push(text.to_owned());
            timeout(&self.send_timeouts, "cmux send failed")?;
            ensure!(!self.text_enter_fails, "cmux send-key failed: broken pipe");
            Ok(())
        }
        fn send_enter(&self, _: &str) -> Result<()> {
            self.sent.lock().unwrap().push("<enter>".to_owned());
            Ok(())
        }
        fn capture(&self, _: &str) -> Result<String> {
            timeout(&self.capture_timeouts, "cmux read-screen failed")?;
            let mut screens = self.screens.lock().unwrap();
            match screens.len() {
                0 => bail!("no screen"),
                1 => Ok(screens[0].clone()),
                _ => Ok(screens.pop().unwrap()),
            }
        }
        fn close(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn set_color(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn pin(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn send_exit(&self, _: &str) -> Result<()> {
            self.sent.lock().unwrap().push("/exit".to_owned());
            ensure!(!self.exit_times_out, "cmux send failed: Command timed out");
            ensure!(!self.exit_enter_fails, "cmux send-key failed: broken pipe");
            Ok(())
        }
        fn exists(&self, _: &str) -> Result<bool> {
            unimplemented!()
        }
        fn listed_workspace_ids(&self) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn create_named(&self, _: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
            unimplemented!()
        }
        fn ensure_group(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn notify(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        fn submit_check_interval(&self) -> Duration {
            Duration::ZERO
        }
        fn retry_backoff(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// Fails with cmux's timeout while `left` counts down.
    fn timeout(left: &AtomicUsize, what: &str) -> Result<()> {
        match left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)) {
            Ok(_) => bail!("{what}: Command timed out"),
            Err(_) => Ok(()),
        }
    }

    /// No queue: the failures [`RecordingBackend`] records are dropped.
    struct NoQueue;

    impl QueueOpener for NoQueue {
        fn open(&self) -> Result<Box<dyn Queue + Send>> {
            bail!("no queue")
        }
    }

    /// [`submit_input`] through the [`RecordingBackend`] the supervisor
    /// uses, which retries what timed out.
    fn submitted_through(backend: &Backend, input: Input<'_>) -> Result<(Submission, usize)> {
        let recording = RecordingBackend::over(backend, Arc::new(NoQueue), None, || None);
        submit_input(&recording, &Signals, "ws", input)
    }

    /// Screens as words: `ready`, `pending:<text>` (in the box), `dialog`,
    /// `working`, `boot`, each maybe followed by `|<status line>` under
    /// the input box, which is not transcript. A `|dialog` status line is a
    /// dialog drawn over a box that still holds its input.
    struct Signals;

    /// The screen word of `screen`, its status line cut off.
    fn word(screen: &str) -> &str {
        screen.split('|').next().unwrap_or_default()
    }

    impl AgentSignals for Signals {
        fn detect_prompt(&self, screen: &str) -> Option<&'static str> {
            screen
                .split('|')
                .any(|part| part == "dialog")
                .then_some("choice")
        }
        fn transcript(&self, screen: &str) -> String {
            word(screen).to_owned()
        }
        fn screen_excerpt(&self, screen: &str) -> String {
            screen.to_owned()
        }
        fn idle_hook(&self, _: &[u8]) -> IdleHook {
            IdleHook::default()
        }
        fn input_ready(&self, screen: &str) -> bool {
            word(screen) == "ready" || screen.starts_with("pending:")
        }
        fn input_pending(&self, screen: &str, text: &str) -> bool {
            word(screen).strip_prefix("pending:") == Some(text)
        }
        fn working(&self, screen: &str) -> bool {
            word(screen) == "working"
        }
    }

    const TEXT: &str = "please rebase";

    fn submitted(screens: &[&str], input: Input<'_>) -> (Submission, usize, Vec<String>) {
        let backend = Backend::new(screens);
        let (submission, retries) = submit_input(&backend, &Signals, "ws", input).unwrap();
        (submission, retries, backend.sent())
    }

    #[test]
    fn a_text_that_left_the_box_is_submitted_once() {
        let (submission, retries, sent) = submitted(&["ready"], Input::Text(TEXT));
        assert_eq!(submission, Submission::Submitted(Some("ready".into())));
        assert_eq!((retries, sent), (0, vec![TEXT.to_owned()]));
        // A screen that cannot be read does not count against the send.
        let (submission, retries, _) = submitted(&[], Input::Text(TEXT));
        assert_eq!((submission, retries), (Submission::Submitted(None), 0));
    }

    #[test]
    fn a_text_left_in_the_box_gets_enter_alone_again() {
        let pending = format!("pending:{TEXT}");
        let (submission, retries, sent) =
            submitted(&[&pending, &pending, "working"], Input::Text(TEXT));
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        assert_eq!(retries, 2);
        // The text is typed once; only Enter goes again.
        assert_eq!(sent, [TEXT, "<enter>", "<enter>"]);
        // Past the retries it is stuck.
        let (submission, retries, sent) = submitted(&[&pending], Input::Text(TEXT));
        assert_eq!(submission, Submission::Stuck(pending.clone()));
        assert_eq!(retries, SUBMIT_RETRIES);
        assert_eq!(sent.iter().filter(|s| *s == TEXT).count(), 1);
        assert_eq!(sent.len(), 1 + SUBMIT_RETRIES);
    }

    #[test]
    fn exit_left_in_the_box_gets_enter_but_is_never_typed_again() {
        let (submission, retries, sent) = submitted(&["pending:/exit", "ready"], Input::Exit);
        assert_eq!(submission, Submission::Submitted(Some("ready".into())));
        assert_eq!(retries, 1);
        assert_eq!(sent, ["/exit", "<enter>"]);
        let (submission, _, sent) = submitted(&["pending:/exit"], Input::Exit);
        assert!(matches!(submission, Submission::Stuck(_)));
        assert_eq!(sent.iter().filter(|s| *s == "/exit").count(), 1);
    }

    #[test]
    fn no_enter_goes_over_a_dialog() {
        let (submission, retries, sent) = submitted(&["dialog"], Input::Exit);
        assert_eq!(submission, Submission::Dialog("dialog".into()));
        assert_eq!((retries, sent), (0, vec!["/exit".to_owned()]));
        let (submission, _, sent) = submitted(&["dialog"], Input::Text(TEXT));
        assert_eq!(submission.screen(), Some("dialog"));
        assert_eq!(sent, [TEXT]);
    }

    /// [`submit_input_answering`] over `screens` whose answerer closes a
    /// `settings` screen with `<escape>` (a known dialog that
    /// [`AgentSignals::detect_prompt`] does not see): the outcome, the
    /// Enters sent again, what was sent and the screens given to it.
    fn answered(
        screens: &[&str],
        input: Input<'_>,
    ) -> (Submission, usize, Vec<String>, Vec<String>) {
        let backend = Backend::new(screens);
        let mut seen = Vec::new();
        let mut answer = |screen: &str| {
            seen.push(screen.to_owned());
            let panel = word(screen) == "settings";
            if panel {
                backend.sent.lock().unwrap().push("<escape>".to_owned());
            }
            panel
        };
        let (submission, retries) =
            submit_input_answering(&backend, &Signals, "ws", input, Some(&mut answer)).unwrap();
        (submission, retries, backend.sent(), seen)
    }

    #[test]
    fn a_settings_panel_over_the_box_is_closed_before_the_text_is_typed() {
        let (submission, retries, sent, seen) =
            answered(&["settings", "working"], Input::Text(TEXT));
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        assert_eq!(retries, 0);
        assert_eq!(sent, ["<escape>", TEXT]);
        // Answered, it is given no more screens.
        assert_eq!(seen, ["settings"]);
        // A screen with no known dialog gets nothing before the typing.
        let (submission, _, sent, seen) = answered(&["ready", "working"], Input::Text(TEXT));
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        assert_eq!(sent, [TEXT]);
        assert_eq!(seen, ["ready", "working"]);
    }

    #[test]
    fn a_settings_panel_over_a_typed_input_is_closed_and_the_input_confirmed() {
        let pending = format!("pending:{TEXT}");
        let (submission, retries, sent, _) = answered(
            &["ready", "settings", &pending, "working"],
            Input::Text(TEXT),
        );
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        assert_eq!(retries, 1);
        // Enter alone goes again once the panel is gone; the text is typed once.
        assert_eq!(sent, [TEXT, "<escape>", "<enter>"]);
        // `/exit` the same, and never typed again.
        let (submission, retries, sent, _) = answered(
            &["ready", "settings", "pending:/exit", "ready"],
            Input::Exit,
        );
        assert_eq!(submission, Submission::Submitted(Some("ready".into())));
        assert_eq!(retries, 1);
        assert_eq!(sent, ["/exit", "<escape>", "<enter>"]);
    }

    #[test]
    fn a_dialog_is_answered_once_and_an_unknown_one_is_left_as_a_dialog() {
        // The panel back after Escape is left to the reads: at most one
        // answer per submit.
        let (submission, _, sent, _) =
            answered(&["ready", "settings", "settings|dialog"], Input::Text(TEXT));
        assert_eq!(submission, Submission::Dialog("settings|dialog".into()));
        assert_eq!(sent, [TEXT, "<escape>"]);
        // A dialog the answerer does not know gets no key and no Enter.
        let (submission, retries, sent, seen) = answered(&["ready", "dialog"], Input::Exit);
        assert_eq!(submission, Submission::Dialog("dialog".into()));
        assert_eq!(retries, 0);
        assert_eq!(sent, ["/exit"]);
        assert_eq!(seen, ["ready", "dialog"]);
    }

    #[test]
    fn start_sign_tells_work_from_a_lost_or_held_text() {
        let pending = format!("pending:{TEXT}");
        assert_eq!(
            start_sign(&Signals, "working", Some("working"), TEXT),
            StartSign::Started
        );
        // The screen moved on since the submit.
        assert_eq!(
            start_sign(&Signals, "ready", Some("boot"), TEXT),
            StartSign::Started
        );
        assert_eq!(
            start_sign(&Signals, "dialog", Some("ready"), TEXT),
            StartSign::Dialog("choice")
        );
        assert_eq!(
            start_sign(&Signals, "ready", Some("ready"), TEXT),
            StartSign::Lost
        );
        assert_eq!(start_sign(&Signals, "ready", None, TEXT), StartSign::Lost);
        assert_eq!(
            start_sign(&Signals, &pending, Some(&pending), TEXT),
            StartSign::Held
        );
        assert_eq!(start_sign(&Signals, "boot", None, TEXT), StartSign::Held);
    }

    #[test]
    fn start_sign_takes_no_ticking_status_line_for_work() {
        // Only the status line's clock and cost moved: the text was lost.
        assert_eq!(
            start_sign(
                &Signals,
                "ready|12:05 $1.86",
                Some("ready|12:04 $1.84"),
                TEXT
            ),
            StartSign::Lost
        );
        assert_eq!(
            start_sign(&Signals, "boot|12:05", Some("boot|12:04"), TEXT),
            StartSign::Held
        );
        // The transcript above the box moved: a sign of work.
        assert_eq!(
            start_sign(&Signals, "ready|12:05", Some("boot|12:04"), TEXT),
            StartSign::Started
        );
    }

    #[test]
    fn a_start_check_starts_from_the_submitted_screen() {
        let at = SystemTime::UNIX_EPOCH;
        let check = StartCheck::new("request", TEXT, at, &Submission::Submitted(None));
        assert!(!check.done && check.submitted.is_none());
        let check = StartCheck::new("request", TEXT, at, &Submission::Stuck("s".into()));
        assert!(check.done);
        assert_eq!(check.submitted.as_deref(), Some("s"));
        assert_eq!(Input::Exit.name(), "exit");
    }

    /// ADR-0043 decision 2: the check waits `[stall].send_confirm_secs`
    /// (the value of `dagq.toml` the supervisor loaded, or its default),
    /// not a wait of the backend's.
    #[test]
    fn a_start_check_waits_send_confirm_secs() {
        assert_eq!(
            confirm_wait(&StallConfig::default()),
            Duration::from_secs(60)
        );
        let mut stall = StallConfig::default();
        stall.set("send_confirm_secs", 5).unwrap();
        let wait = confirm_wait(&stall);
        assert_eq!(wait, Duration::from_secs(5));
        // A test may set it below a second (task 1045).
        assert_eq!(
            confirm_wait(&stall.with_millis("send_confirm_secs", 200)),
            Duration::from_millis(200)
        );
        assert_eq!(whole_secs(Duration::from_millis(200)), 1);
        assert_eq!(whole_secs(wait), 5);
        assert_eq!(whole_secs(Duration::ZERO), 0);
        let files = crate::application::memory_files::MemoryFiles::default();
        let mut check = StartCheck::new("request", TEXT, files.now(), &Submission::Submitted(None));
        // Within the wait the screen is not read.
        assert!(!check.due(&files, wait, &[], None));
        // Past it, with no mark written, it is.
        check.sent = Instant::now().checked_sub(Duration::from_secs(6)).unwrap();
        assert!(check.due(&files, wait, &[], None));
        assert!(!check.done);
    }

    /// A send is taken once the session's input marker (its
    /// `prompt-submit.json`), idle marker or receipt is written after it:
    /// the check ends with no screen read, no Enter sent again and no ask,
    /// even when only the input marker moved (the session works on it). A
    /// notice the agent put in by itself (its background work ended) is
    /// not the text taken.
    #[test]
    fn a_send_is_taken_by_any_mark_written_after_it() {
        use crate::application::memory_files::MemoryFiles;
        let idle = Path::new("/run/idle.json");
        let receipt = Path::new("/run/receipt.json");
        assert_eq!(
            input_marker_path(idle),
            Path::new("/run/prompt-submit.json")
        );
        let marks = [idle.to_path_buf(), receipt.to_path_buf()];
        let input = |at: SystemTime, source: InputSource| {
            Some(InputMarker {
                modified: at,
                source,
                text: None,
            })
        };
        for written in [None, Some(idle), Some(receipt)] {
            let files = MemoryFiles::default();
            let sent_at = files.now();
            let before = sent_at - Duration::from_secs(30);
            let after = sent_at + Duration::from_secs(1);
            // Marks from before the send do not count.
            for mark in &marks {
                files.put(mark, before, "{}");
            }
            let mut check = StartCheck::new("request", TEXT, sent_at, &Submission::Submitted(None));
            assert!(check.due(
                &files,
                Duration::ZERO,
                &marks,
                input(before, InputSource::Typed)
            ));
            // A notice of the agent's own after the send is no sign.
            assert!(check.due(
                &files,
                Duration::ZERO,
                &marks,
                input(after, InputSource::Agent)
            ));
            assert!(!check.done);
            let taken = match written {
                // Only the input marker moved.
                None => input(after, InputSource::Typed),
                Some(mark) => {
                    files.put(mark, after, "{}");
                    None
                }
            };
            assert!(
                !check.due(&files, Duration::ZERO, &marks, taken),
                "{written:?}"
            );
            assert!(check.done, "{written:?}");
            // Nothing more is checked.
            assert!(!check.due(&files, Duration::ZERO, &marks, taken));
        }
        // A provider without the input marker: an unknown source counts.
        let files = MemoryFiles::default();
        let sent_at = files.now();
        let mut check = StartCheck::new("request", TEXT, sent_at, &Submission::Submitted(None));
        assert!(!check.due(
            &files,
            Duration::ZERO,
            &marks,
            input(sent_at + Duration::from_secs(1), InputSource::Unknown)
        ));
    }

    #[test]
    fn a_text_that_timed_out_without_reaching_the_screen_is_typed_again() {
        let backend = Backend::new(&["ready", "ready", "working"]);
        backend.send_timeouts.store(2, Ordering::SeqCst);
        let (submission, retries) = submitted_through(&backend, Input::Text(TEXT)).unwrap();
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        assert_eq!(retries, 0);
        assert_eq!(backend.sent(), [TEXT, TEXT, TEXT]);
        // Past the attempts it fails as before, known not to have been sent.
        let backend = Backend::new(&["ready"]);
        backend.send_timeouts.store(9, Ordering::SeqCst);
        let error = submitted_through(&backend, Input::Text(TEXT)).unwrap_err();
        assert!(!timed_out_maybe_sent(&error), "{error:#}");
        assert_eq!(backend.sent().len(), 3);
    }

    #[test]
    fn a_text_that_timed_out_but_reached_the_box_is_not_typed_again() {
        let pending = format!("pending:{TEXT}");
        let backend = Backend::new(&[&pending, &pending, "working"]);
        backend.send_timeouts.store(1, Ordering::SeqCst);
        let (submission, retries) = submitted_through(&backend, Input::Text(TEXT)).unwrap();
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        // The Enter the timeout may have cost is sent alone.
        assert_eq!(
            (retries, backend.sent()),
            (1, vec![TEXT.into(), "<enter>".into()])
        );
        // A screen that cannot be read is no reason to type it again.
        let backend = Backend::new(&[]);
        backend.send_timeouts.store(1, Ordering::SeqCst);
        let (submission, _) = submitted_through(&backend, Input::Text(TEXT)).unwrap();
        assert_eq!(submission, Submission::Submitted(None));
        assert_eq!(backend.sent(), [TEXT]);
    }

    #[test]
    fn an_exit_that_timed_out_but_got_there_is_never_typed_again_and_does_not_fail() {
        // Its trace on the screen, or a dialog over the box: not typed again.
        for (screens, expected) in [
            (
                &["pending:/exit", "ready"][..],
                Submission::Submitted(Some("ready".into())),
            ),
            (&["dialog"][..], Submission::Dialog("dialog".into())),
        ] {
            let mut backend = Backend::new(screens);
            backend.exit_times_out = true;
            let (submission, _) = submitted_through(&backend, Input::Exit).unwrap();
            assert_eq!(submission, expected);
            assert_eq!(backend.sent().iter().filter(|s| *s == "/exit").count(), 1);
        }
        // Nor on a screen that cannot be read.
        let mut backend = Backend::new(&[]);
        backend.exit_times_out = true;
        let (submission, _) = submitted_through(&backend, Input::Exit).unwrap();
        assert_eq!(submission, Submission::Submitted(None));
        assert_eq!(backend.sent(), ["/exit"]);
    }

    #[test]
    fn an_exit_that_timed_out_before_getting_there_is_typed_again_up_to_the_attempts() {
        let mut backend = Backend::new(&["ready"]);
        backend.exit_times_out = true;
        let (submission, retries) = submitted_through(&backend, Input::Exit).unwrap();
        assert_eq!((submission, retries), (Submission::Unsent, 0));
        assert_eq!(backend.sent(), ["/exit", "/exit", "/exit"]);
        assert_eq!(Submission::Unsent.screen(), None);
    }

    #[test]
    fn an_exit_whose_enter_failed_in_the_box_gets_enter_again() {
        let mut backend = Backend::new(&["pending:/exit", "pending:/exit", "ready"]);
        backend.exit_enter_fails = true;
        let (submission, retries) = submitted_through(&backend, Input::Exit).unwrap();
        assert_eq!(submission, Submission::Submitted(Some("ready".into())));
        // `/exit` is typed once; only Enter goes again.
        assert_eq!(
            (retries, backend.sent()),
            (1, vec!["/exit".into(), "<enter>".into()])
        );
        // Without a trace of it on the screen, or over a dialog, the
        // failure stands and nothing more is sent.
        for screen in ["ready", "dialog"] {
            let mut backend = Backend::new(&[screen]);
            backend.exit_enter_fails = true;
            let error = submitted_through(&backend, Input::Exit).unwrap_err();
            assert!(
                format!("{error:#}").contains("send-key failed"),
                "{error:#}"
            );
            assert_eq!(backend.sent(), ["/exit"]);
        }
    }

    #[test]
    fn a_text_whose_enter_failed_in_the_box_gets_enter_again() {
        let pending = format!("pending:{TEXT}");
        let mut backend = Backend::new(&[&pending, &pending, "working"]);
        backend.text_enter_fails = true;
        let (submission, retries) = submitted_through(&backend, Input::Text(TEXT)).unwrap();
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        // The text is typed once; only Enter goes again.
        assert_eq!(
            (retries, backend.sent()),
            (1, vec![TEXT.into(), "<enter>".into()])
        );
        // Without the text in the box, or over a dialog, the failure
        // stands and nothing more is sent.
        let pending_under_dialog = format!("{pending}|dialog");
        for screen in [
            "ready",
            "working",
            "dialog",
            &pending_under_dialog,
            "pending:other text",
        ] {
            let mut backend = Backend::new(&[screen]);
            backend.text_enter_fails = true;
            let error = submitted_through(&backend, Input::Text(TEXT)).unwrap_err();
            assert!(
                format!("{error:#}").contains("send-key failed"),
                "{screen}: {error:#}"
            );
            assert!(!timed_out_maybe_sent(&error), "{screen}: {error:#}");
            assert_eq!(backend.sent(), [TEXT], "{screen}");
        }
        // Nor on a screen that cannot be read.
        let mut backend = Backend::new(&[]);
        backend.text_enter_fails = true;
        submitted_through(&backend, Input::Text(TEXT)).unwrap_err();
        assert_eq!(backend.sent(), [TEXT]);
    }

    /// Every `/exit` of the supervisor, whatever the path (a finished
    /// worker session, a resumed one, one kept through review and revise,
    /// a silent wrapper, a runtime planner), goes through [`submit_input`]
    /// and so gets its Enter confirmed (task 353): nothing else calls
    /// `send_exit`.
    #[test]
    fn every_exit_goes_through_submit_input() {
        fn scan(dir: &Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    scan(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for (n, line) in source.lines().enumerate() {
                        if line.contains(".send_exit(") {
                            found.push(format!("{}:{}", path.display(), n + 1));
                        }
                    }
                }
            }
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        scan(&src, &mut found);
        let allowed = [
            // submit_input itself.
            "application/supervise/deliver.rs",
            // The recording wrapper hands it to the backend.
            "application/recording.rs",
            // `send_exit_when`'s default, which submit_input calls (task
            // 354), hands it to a backend that retries nothing.
            "application/ports.rs",
        ];
        let stray: Vec<_> = found
            .iter()
            .filter(|at| !allowed.iter().any(|file| at.contains(file)))
            .collect();
        assert!(
            stray.is_empty(),
            "send_exit outside submit_input: {stray:?}"
        );
    }

    #[test]
    fn a_capture_that_timed_out_is_read_again() {
        let backend = Backend::new(&["ready"]);
        backend.capture_timeouts.store(2, Ordering::SeqCst);
        let recording = RecordingBackend::over(&backend, Arc::new(NoQueue), None, || None);
        assert_eq!(recording.capture("ws").unwrap(), "ready");
        backend.capture_timeouts.store(3, Ordering::SeqCst);
        let error = recording.capture("ws").unwrap_err();
        assert_eq!(
            reason_of_error(&error, ReasonCode::Other).code,
            ReasonCode::BackendTimeout
        );
        assert!(!timed_out_maybe_sent(&error));
    }
}
