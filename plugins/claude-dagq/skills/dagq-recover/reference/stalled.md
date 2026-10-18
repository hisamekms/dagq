# A `stalled` ask: `wait`, `stop`, `propose`, and an instruction

Read this to carry out, on the person's word, the answer to a `stalled` ask (the `dagq-recover` skill, section 7).

## A headless run's `stalled` ask

A run's session is headless: a background process with no screen and no workspace. Nothing is typed into that session by hand, by any command or by `cmux`: the supervisor hands it the answer as its next turn. `run screen` is refused and names the run's `turns/`; `run send` refuses every run.

Before answering, read what the turns did (the question's last turns, `"$DAGQ" run log RUN`, `"$DAGQ" timeline RUN`, `"$DAGQ" events --run RUN --full`) and bring it to the person. Then answer with the person's choice; the supervisor applies it and closes the ask itself:

- **`wait`**: leave the session alone; the idle is counted again from the answer, so the same ask may come back.
- **`propose`** (or `propose: <why>`): a runtime planner proposes a remedy for the cause; the session is left alone as for `wait`.
- **`stop`**: the supervisor has the session exit; the run ends without a receipt and goes to its recovery job. Nobody ends the wrapper or signals a process by hand.
- **An instruction** (any other text): sent as the session's next turn.
- An option the job added: as its text says.

There is no `intervene`. One that still arrives is not sent: the supervisor opens a new `stalled` ask with the options above; answer that one, and do not `ask close` it by hand.

## Closing it

With no supervisor holding the run, a `wait` or `propose` answer shows `read the answer of ask <id> and close it`: `"$DAGQ" ask close <id>`. The supervisor also closes the ask itself once the session moves on, exits or its run is triaged; `answer` then reports it is not open. `recover` does not close it.

A past `stalled` ask with `reason: idle_without_receipt` or `send_unconfirmed` came from the retired interactive session: it is a record, and nothing is carried out for it.
