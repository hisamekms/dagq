//! Worker requests are queued as turns. Planner and inbox input submission
//! retains input checks and Enter retries.

use super::*;

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

/// Submit planner or inbox input, retrying Enter while it remains in the input box.
pub(crate) fn submit_input(
    cmux: &dyn WorkspaceBackend,
    signals: &dyn AgentSignals,
    workspace: &str,
    input: Input<'_>,
) -> Result<(Submission, usize)> {
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
    Ok(confirm_input(cmux, signals, workspace, input))
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

/// Queue a worker request or its exit file, without reading or typing into a screen.
pub(super) fn submit(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    input: Input<'_>,
    what: &str,
) -> Result<Submission> {
    request_turn(sv, run, workspace, input, what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{Queue, QueueOpener};
    use crate::application::{SupervisorEnvironment, WorkspaceTags};
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
        assert_eq!(submission, Submission::Dialog("dialog".into()));
        assert_eq!(sent, [TEXT]);
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
