//! What the supervisor asks of a session: a request or an exit, written
//! as the session's next turn (ADR-t813-1). Nothing is typed into a
//! terminal and no screen is read (ADR-t1433-5 decision 2).

use super::*;

/// What the supervisor asks of a session.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Input<'a> {
    /// A request or an answer, as the session's next turn.
    Text(&'a str),
    /// A next-turn message held to its limits (ADR-t2072-1): `text` as
    /// [`Input::Text`], and `bytes` recorded as `prompt_bytes` on the
    /// `turn_requested` of a run's session.
    Prompt {
        text: &'a str,
        bytes: &'a PromptBytes,
    },
    /// The exit request, never written twice.
    Exit,
}

impl<'a> From<&'a FittedPrompt> for Input<'a> {
    fn from(prompt: &'a FittedPrompt) -> Self {
        Self::Prompt {
            text: &prompt.text,
            bytes: &prompt.bytes,
        }
    }
}

/// Where a request ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Submission {
    /// Written as the next request (or the exit request) of a headless
    /// session (ADR-t813-1): the turn that runs it is its sign.
    Queued,
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
