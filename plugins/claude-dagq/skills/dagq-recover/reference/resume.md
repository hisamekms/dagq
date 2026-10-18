# needs_session runs: what the runtime's resume does

A `needs_session` run (its landing conflicted, its verification failed after the rebase, required evidence was missing, or a person answered `send_back` to an `approve_landing` ask) is resumed by the supervisor itself (`resuming (runtime)`) and taken on from there. None of it is yours: never open a resume workspace, never send anything to the session by hand, and do not `recover`, `ready` or `integrate` the run while the runtime holds it.

What reaches the inbox is an ask or an attention, handled where it is described: a `decide` ask once the resumes ran out and the recovery job escalated (the supervisor applies the answer), a `stalled` ask on a resumed session (`reference/stalled.md`), and an `answer_prompt` or `send_unconfirmed` ask on an older resume, which is a past record (`reference/session.md`).
