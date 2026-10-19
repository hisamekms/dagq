# A `stalled` ask: `wait`, `stop`, `propose`, and an instruction

Read this to carry out, on the person's word, the answer to a `stalled` ask (the `dagq-recover` skill, section 7).

## A headless run's `stalled` ask

A run's session is headless: a background process with no screen and no workspace. Nothing is typed into that session, by hand or by any command: the supervisor hands it an instruction as its next turn, has it exit for `stop`, and sends it nothing for the other answers. `run screen` is refused and names the run's `turns/`; `run send` refuses every run.

Before answering, read what the turns did (the question's last turns, `"$DAGQ" run log RUN`, `"$DAGQ" timeline RUN`, `"$DAGQ" events --run RUN --full`) and bring it to the person. Then answer with the person's choice; the supervisor applies it and closes the ask itself:

- **`wait`**: leave the session alone; the idle is counted again from the answer, so the same ask may come back.
- **`propose`** (or `propose: <why>`): a runtime planner proposes a remedy for the cause; the session is left alone as for `wait`.
- **`stop`**: the supervisor has the session exit; the run ends without a receipt and goes to its recovery job. Nobody ends the wrapper or signals a process by hand.
- **An option the job added** (marked `[goes back to the recovery job ...]`; the answer may carry the mark or leave it out): nothing is sent to the session and the runtime moves nothing. The supervisor closes the ask, and the alert's next recovery job, when the alert comes again, runs with the person's answer. Those jobs count against the alert's limit; once it is used up, an ask opens again. A job's option that points at a runtime answer (`wait`, `stop`) is merged into that option, and one that reads as another runtime operation or as the task's completion or cancel is left out: neither is on the ask as the job's, and the question gives the reason.
- **An instruction** (any other text, not an option of the ask): sent as the session's next turn.

There is no `intervene`. One that still arrives is not sent: the supervisor opens a new `stalled` ask with the options above; answer that one, and do not `ask close` it by hand.

## Closing it

With no supervisor holding the run, a `wait` or `propose` answer shows `read the answer of ask <id> and close it`: `"$DAGQ" ask close <id>`. The supervisor also closes the ask itself once the session moves on, exits or its run is triaged; `answer` then reports it is not open. `recover` does not close it.

A past `stalled` ask with `reason: idle_without_receipt` or `send_unconfirmed` came from the retired interactive session: it is a record, and nothing is carried out for it.
