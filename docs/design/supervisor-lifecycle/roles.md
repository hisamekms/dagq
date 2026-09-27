---
id: design-supervisor-lifecycle-roles
type: design
title: "Roles"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0044
  - adr-t728-1
---

# Roles

- **supervisor**: runtimeの`supervise`プロセス。taskをclaimし、runごとにworktreeとworkspaceを作って監視し、receiptを検証する。`up`がlaunchdのLaunchAgentとして常駐させるか（既定）、`--in-cmux`なら`[<repo>]supervisor` workspaceの中で動かす。
- **worker**: run session。runごとのcmux workspace `[<repo>]worker#<task-id> - <task title>`（descriptionは`dagq role=worker queue=<queue hash> run=<run-id> task=<id>`）で動くClaude session。`needs_session`のrunをresumeするworkspaceも同じtitleで、descriptionは`run <run-id> resume`。
- **planner**: goal / taskを書いてproposalとしてsubmitするsession。proposalごとのオンデマンドのworkspaceで、常駐しない（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6）。人が`dagq plan`で開くもの（何度打っても新しいworkspaceが開き、複数同時に開ける）と、runtimeが立てるもの（差し戻し先のplannerが閉じていたproposalなど）がある。workspaceは`[<repo>]planner#<planner-id>`（`DAGQ_ROLE=planner`）で、session wrapper `planner-session`がClaude sessionを`planner_prompt`（runtimeが立てたものは`runtime_planner_prompt`）付きで起動する（[`plan` / `planners`](plan-planners.md#plan--planners)）。人に頼まれれば`up` / `down`も打つ。
- **inbox**: 人に届くものすべての窓口になるsession。openなask（worker・supervisor・job・observerの質問）を人に見せてanswerを書き戻し、それ以外のattentionを人に知らせ、人の指示があるときだけ`dagq-recover` skillの手順（`up` / `down`、手でのreviewと`integrate`、`recover`、runのworkspaceへのキーと`/exit`）を実行する。唯一の常駐sessionで、`up`が`[<repo>]inbox`のworkspace（`DAGQ_ROLE=inbox`）に、`inbox_prompt`付きで起動する（[Session prompts](session-prompts.md#session-prompts)）。
- **observer**: supervisorのtimerが起動するheadlessのjob（`DAGQ_ROLE=observer`、workspaceは持たない）。stats・note・openなask・graphを読み、note・`blocked`のask・draftのgoalだけを書く（[Observer](observer.md#observer)）。

役割はこの5つ（ADR-0044の決定1。ADR-0024の決定1を引き継ぐ）で、review・recovery（triage）・plan review・goal reviewのjobはsupervisorが起動するheadlessのjob（workspaceは持たない）。runtimeの中で人が打つ`/exit`や復旧は「人（person）」と書き、inboxかplannerのsessionから打つ（`src/`に`operator`も退役した役割名も残らない）。

## Actors

runtimeはactorを型で表す（[ADR-t728-1](../../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定2・4。`src/domain/actor.rs`）。

- `ActorRole`: `user`・`inbox`・`planner`・`worker`・`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`・`observer`・`supervisor`・`wrapper`・`integrator`（`desk`はgoal 48のtask 504が作るときに足す）。workspaceを持つ役割の`SessionRole`（`supervisor`・`worker`・`planner`・`inbox`・`observer`）は`SessionRole::actor_role`で`ActorRole`に写る。
- `TrustLevel`: roleだけから決まる。`user`は`Human`、`supervisor`・`wrapper`・`integrator`は`TrustedControlPlane`、それ以外（AI actor）は`UntrustedAgent`。promptや名前から推し量らない。
- `ActorContext`: `actor_id`・`role`・`trust`・`run_id`・`task_id`。

### 環境変数

runtimeが起動するAI actorは全て、環境にroleとactor idを持つ。

| actor | 起動するところ | `DAGQ_ROLE` | `DAGQ_ACTOR_ID` | ほか |
| --- | --- | --- | --- | --- |
| worker（resumeも） | runのworkspace | `worker` | `worker:<run id>` | `DAGQ_RUN_ID`・`DAGQ_TASK_ID` |
| planner（人・runtime・draft・finding） | plannerのworkspace | `planner` | `planner:<planner id>` | |
| inbox | `up` | `inbox` | `inbox` | |
| supervisor（in-cmux） | `up --in-cmux` | `supervisor` | `supervisor` | |
| review job | runのreview | `review-job` | `review-job:<run id>:<attempt>` | |
| recovery job | 終わったrunと生きているrunの復旧 | `recovery-job` | `recovery-job:<run id>:<alert>:<attempt>` | |
| plan review job | proposalのplan review | `plan-review-job` | `plan-review-job:<proposal id>:<attempt>` | |
| goal review job | goalのgoal review | `goal-review-job` | `goal-review-job:<goal id>:<attempt>` | |
| observer | `dagq observe`が起動するagent | `observer` | `observer:<session id>` | |

どれも`DAGQ_QUEUE`も持つ。`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`は`[run.env]`で上書きできない（`DAGQ_`の予約）。

### CLIでの解釈

`execute()`はコマンドの前に環境を`ActorContext::from_env`で読む。

- `DAGQ_ROLE`が無いか空なら`user`（人）。host実行の互換のためで、助言的（advisory）でありsandboxではない（ADR-t728-1の決定6）。
- `DAGQ_ROLE`が上の表の値（`user`を除く）でなければ`unknown DAGQ_ROLE: <値>`のerrorで止まり、queueを開かない（fail closed）。`DAGQ_RUN_ID`・`DAGQ_TASK_ID`が読めないときも同じ。
- 旧値`reviewer`（入れ替え前のバイナリが起動したjob）は移行の間だけ`review-job`として読む。
- `DAGQ_ACTOR_ID`が無ければ（それより前に開いたsession）actor idは`DAGQ_ROLE`の値。
- 4つのjob（と`reviewer`）は今までのreviewerと同じ読み取りだけの制限を受け、状態を変えるコマンドは`reviewer may not change queue state`で拒まれる。observerの制限（findingの記録・解決と、findingに紐づく`blocked`のaskだけ）も変わらない。Authorizer（goal 55）が置き換えるまで、どのjobにも書き込みを与えない。
- noteの`by`・markの`by`・findingの`recorded_by`と状態変更の`by`・askの`asked_by`は`ActorContext::written_by`（roleの名前、userは`human`）、answerの`answered_by`は`ActorContext::answered_by`（roleの名前、userは`person`）から作り、書かれる値は型を入れる前と同じ。
