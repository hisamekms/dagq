---
id: design-authorization
type: design
title: Authorization
status: current
created: 2026-09-27
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - adr-t728-1
  - adr-t728-2
  - adr-t728-3
  - design-supervisor-lifecycle-roles
---

# Authorization

actorが何をしてよいかは、`src/domain/authorization.rs`の`Authorizer`（`authorize(actor, capability, resource) -> Result<(), AuthorizationError>`）が決める（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定5）。実装は静的なpolicyの`StaticPolicy`で、roleごとの許可の一覧（`grants(role)`）と、いくつかのroleのresourceの規則でできている。一覧に無い組み合わせは拒む（default deny）。resourceの持ち主や状態を規則が要るのに呼び出し元が知らない（`None`、`Resource::Unresolved`）ときも拒む（fail closed）。判定はClaudeを起動せずにunit testできる。

host実行ではこの判定は助言的（advisory）で、sandboxでも隔離でもない。どのプロセスも`DAGQ_ROLE`を偽れる（ADR-t728-1の決定6）。actorの型と環境変数は[Roles](supervisor-lifecycle/roles.md#actors)。

## Capability

| 群 | capability | 対応するCLI |
| --- | --- | --- |
| 読み取り | `queue.read` | `locate` `list` `show` `candidates` `graph`（`--out`なし） `status` `asks` `events` `timeline` `stats` `kpi` `forecast` `doctor` `notes` `marks` `findings` `search` `related` `proposal list/show` `planners` `lint` `goal list/show` `observe --history` |
| | `queue.watch` | `watch` |
| | `queue.export` | `graph --out` `report`（ファイルを書く） |
| 計画 | `goal.write` | `goal add` `goal edit` |
| | `goal.ready` | `goal ready` |
| | `goal.close` | `goal close` |
| | `goal.review_request` | `goal review` |
| | `task.write` | `add` `edit` `draft` `set-goal` `set-paths` `set-priority` `dependency add/remove` |
| | `task.cancel` | `cancel` |
| | `task.ready` / `task.ready_bypass_review` | `ready` / `ready --bypass-review` |
| | `proposal.submit` / `proposal.withdraw` | `submit` / `proposal withdraw` |
| | `note.write` / `mark.write` | `note` / `mark` |
| workerの通信 | `ask.open` | `ask`（`blocked`をfindingに紐づけるもの以外） |
| | `session.run` | `session`（runのsession wrapper） `planner-session` |
| | `session.record` | `session-event` |
| review / triage | `review.submit` / `triage.submit` | CLIには無い。jobのverdictはデータとしてsupervisorが読み、遷移に写す |
| | `review.prepare` | `review`（review.mdを書く） |
| 観察 | `finding.record` / `finding.resolve` / `finding.dismiss` | `finding record` / `finding resolve` / `finding dismiss` |
| | `finding.ask` | `ask --kind blocked --finding` |
| | `observe.run` | `observe`（`--history`を除く） |
| 人との対話 | `ask.answer` / `ask.close` | `answer` / `ask close` |
| | `planner.open` | `plan` |
| schedulerの遷移 | `scheduler.supervise` | `supervise` |
| | `run.recover` | `recover` |
| | `service.lifecycle` | `up` `down` |
| | `service.install` | `install` `auto-update` |
| | `queue.admin` | `init` `migrate` `rebind` |
| 着地 | `landing.request` | `integrate`（Integratorへの依頼。[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)） |
| | `landing.land` / `landing.push` | Integratorの内側 |
| 予約 | `reserved.filesystem_read` `reserved.filesystem_write` `reserved.network` `reserved.secret_read` | 後の段（sandbox）。この段では誰にも与えない |

CLIのコマンドからcapabilityとresourceへの写しは`src/main.rs`の`requests`で、matchに既定の枝を置かないので、コマンドを足すと写しを書くまでcompileが通らない。`queue.read`だけのコマンドはqueueをread-onlyの接続で開く（`reads_only`）。

## Resource

`Queue`・`Goal`・`Task`（idと、分かれば状態）・`Run`（idと、分かればtask）・`Ask`（idと、分かればrun）・`Proposal`（idと、分かれば出したplannerのactor id）・`Finding`・`Planner`・`Unresolved`（読めなかったid）。CLIの写しは引数にあるものだけを入れ、持ち主と状態は`None`のまま渡す。持ち主をqueueから読んで埋めるのは、判定をapplicationの層に移す後続のtask（埋めないままplannerに強制すると、状態の分からないtaskとproposalの操作が全て拒まれる）。

## Policy

| role | 許すcapability | resourceの規則 |
| --- | --- | --- |
| user | 予約と`review.submit` `triage.submit` `landing.land` `landing.push`を除く全て | なし |
| inbox | userと同じ（人の言葉での代行。[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)の決定1） | なし。人自身の操作との区別は記録が持つ |
| planner | 読み取り・`queue.watch`・`queue.export`・`goal.write`・`goal.close`・`task.write`・`task.cancel`・`proposal.submit`・`proposal.withdraw`・`note.write`・`mark.write`・`ask.open`・`session.run`・`session.record`・`finding.resolve`・`finding.dismiss`・`planner.open`・`service.lifecycle`・`service.install`・`queue.admin` | runとrunのaskには何もできない。taskはdraft・submitted・readyのものだけ（状態が不明なら拒む）。`proposal.withdraw`は自分（actor id）が出したproposalだけ。`planner-session`は自分のplannerだけ |
| worker | 読み取り・`ask.open`・`note.write`・`session.run` | 読み取り以外は自分のrun（`DAGQ_RUN_ID`）・そのtask（`DAGQ_TASK_ID`）・自分のrunのaskだけ |
| review-job | 読み取り・`review.submit` | `review.submit`は自分のrunだけ |
| recovery-job | 読み取り・`triage.submit` | `triage.submit`は自分のrunだけ |
| plan-review-job・goal-review-job | 読み取りだけ | |
| observer | 読み取り・`queue.watch`・`finding.record`・`finding.resolve`・`finding.ask` | `finding.resolve`と`finding.ask`はfindingだけ |
| supervisor | 読み取り・`queue.watch`・`queue.export`・`goal.close`・`task.cancel`・`task.ready`・`note.write`・`ask.open`・`ask.close`・`session.run`・`review.prepare`・`finding.record`・`finding.resolve`・`finding.dismiss`・`observe.run`・`planner.open`・`scheduler.supervise`・`run.recover`・`service.lifecycle`・`service.install`・`landing.request` | なし。`ask.answer`・`task.ready_bypass_review`・`landing.land`・`landing.push`は持たない |
| wrapper | 読み取り・`session.run`・`session.record` | なし |
| integrator | 読み取り・`landing.land`・`landing.push` | なし |

旧値の`DAGQ_ROLE=reviewer`はreview-jobとして読む（[Roles](supervisor-lifecycle/roles.md#cliでの解釈)）。jobの環境には今`DAGQ_RUN_ID`が無いので、`review.submit`・`triage.submit`は持ち主が分からず拒まれる。verdictは今までどおりデータとしてsupervisorが読む。

`AuthorizationError`はroleと拒んだcapabilityと理由（`not granted`・`reserved`・`not on this resource`）だけを出し、actor id（session idを含みうる）やresourceのid・本文を出さない。

## 適用の範囲

今CLIで判定を強制するのは、observerと4つのjob（と旧値`reviewer`）だけで、`src/main.rs`の`check_access`が`requests`の全てを`StaticPolicy`に通す。拒むときのerrorは今までどおり`observer may not change queue state`と`reviewer may not change queue state`。observerに許すもの（読み取り、`watch`、`finding record` / `finding resolve`、findingに紐づく`ask --kind blocked`）とjobに許すもの（読み取りだけ）は、以前の`observer_access` / `reviewer_access`の一覧と同じ。ほかのroleの判定と、状態を変える全てのコマンドをapplicationの層でmutationの前に通すことは、goal 55の後続のtaskが行う。
