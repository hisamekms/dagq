---
id: design-domain-model
type: design
title: Domain model
status: current
created: 2026-09-21
scope: domain
related:
  - adr-t1704-1
  - adr-t1811-1
  - adr-t1639-1
  - adr-t1394-1
  - adr-t1632-1
  - adr-t1340-1
  - adr-t946-1
  - adr-t807-1
  - adr-t791-1
  - adr-0067
  - adr-t813-1
  - adr-t813-2
  - adr-0063
  - adr-0044
  - adr-0038
  - adr-0003
  - adr-0004
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0016
  - adr-0019
  - adr-0040
  - adr-0024
  - adr-0029
  - adr-0034
  - adr-0037
  - design-persistence
  - adr-t947-4
  - adr-t1992-1
---

# Domain model

queueが扱う業務の型（task・goal・proposal・run・finding・計画の依頼）と、その状態遷移と拒否の規則の概念と地図。
欄・既定値・エラー文・各コマンドが許すstatusは`src/domain/`の定義のそばのdoc commentと`--help`が持ち、決定の理由はリンクしたADRが持つ。
保存の形は[persistence](persistence.md)、runを動かす流れは[supervisor-lifecycle](supervisor-lifecycle.md)が持つ。

## 目的

- 業務上の判断（遷移してよいか、拒むか、次に何をするか）を、I/Oを持たない1か所の純粋な関数に集める。
- 同じ判断をCLI・supervisor・`integrate`・queue serviceのどこから呼んでも同じ結果と同じ文にする。
- 人とAIが「この状態から何ができ、なぜ拒まれたか」をコードの入口から辿れるようにする。

## 全体の流れ

taskは計画（draft）から始まり、plan reviewを通ってready、claimでrunを持ち、runの着地でcompletedになる。

```text
task:  draft ──submit──▶ submitted ──approve（plan review）──▶ ready ──claim──▶ in_progress ──runの着地──▶ completed
submitted / ready ──draft──▶ draft、ready ──reopen（plan review）──▶ submitted
in_progress（runが全てfailed / interrupted）──retry──▶ ready、──draft──▶ draft
draft / submitted / ready / in_progress（未完了のrunが無い）──cancel──▶ canceled
draft / submitted ──人のbypass──▶ ready
in_progress（runを再計画が保留）──trusted runtimeの置換の適用──▶ replaced
```

runはclaimから着地までの1回の試行で、supervisorが進め、`integrate`だけが着地させる。

```text
claimed ──▶ starting ──▶ running ──▶ validating ──accept──▶ awaiting_integration ──▶ integrating ──▶ integrated
                                         │ reject                 │ review・landingの判断       │ 衝突・再検証の失敗
                                         ▼                        ▼                             ▼
                                failed / needs_session ◀──────────┴─────────────────────── needs_session ──▶（resume）
claimed〜validating ──interrupt──▶ interrupted、integrating ──interrupt──▶ awaiting_integration
failed / interrupted ──復旧jobのresume──▶ needs_session ──resumeを使い切る──▶ failed
validating（already_satisfied）──close──▶ succeeded（commitなし）、──retry / cancel──▶ failed
needs_session（再計画の保留）──置換の適用──▶ replaced
```

goalはtaskの束が解く上位の課題で、draft → openの状態と、それと独立な1回のclose（verdict）を持つ。
proposalはplannerがplan reviewに出すtaskとgoalの束で、submitted → accepted / revising / canceledと進む。

## 責務と境界

- domain（`src/domain/`）は状態と遷移の判断、拒否の分類（`DomainError`）、記録するeventを決め、I/Oをしない（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)）。
- applicationとinfrastructureは「読む → domainの関数に渡す → 返った値を保存する」だけで、SQLの中で業務の判断をしない（[persistence](persistence.md#集約の読み書き)）。
- 集約の欄は非公開で、作る・変える入口はそのmoduleの関数だけで、保存済みの行の復元（`restore`）は作成時の規則を当て直さない。
- runの状態のうちstatusの外のもの（承認・回数・待ち）は列にせず、`run_events`を`RunHistory`に畳み込んで読む。
- CLIのJSONの欄名とeventのkind名・理由のコードは公開の契約で、名前を変えない（ADR-0016、ADR-0034）。

## 不変条件

- taskは自分自身にも自分の属するgoalにも依存できない。
- 依存のグラフ（task依存の辺、goal依存の辺、goalから`canceled`でない所属taskへの暗黙の辺、置換元から置換先への辺）は循環しない（[ADR-0038](../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)）。
- taskは高々1つのgoalに属し、閉じたgoalに属するtaskは増えない。
- goalの`closed_at`と`verdict`は同時にnullか同時に非nullで、verdictを消すのは人の`reopen`だけで、そのときも`goal_closed`のeventは残る。
- `in_progress`はclaimしたtaskだけが持ち、終端（`completed`・`canceled`・`replaced`）のtaskは変わらない。
- `replaced`は置換の適用だけが書き、依存を満たさない（後続は適用で子を待つ辺に付け替わる。goalのreviewとcloseは置換先を再置換も辿った全てのtaskが終わるまで待つ。[ADR-t1521-2](../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定5）。
- taskが`completed`になるのは、`integrate`によるrunの着地と、`already_satisfied`の主張を閉じたときだけ。
  receiptの自己申告だけでは成功せず、着地はrebase後に1回だけ流す検証コマンドの成功（[ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定1）、閉じるのはsnapshotの検証と人の答え。
- mainはtaskごとに1つのsquash commitの直線で、`integrated`のrunはtaskごとに1件、`integrating`のrunはqueueごとに1件。
- 1 runの失敗・中断・復旧は他のrunの状態・lease・process・資源を変えない。
- agentの異常終了だけでtaskを自動で再実行しない（次の一手は復旧jobのverdictか人が決める）。
- sessionのwrapperを止めたことを`workspace_closed_at`に記録するまで、runは閉じていない扱いにする。
- 同じdraftの束と同じ計画の依頼に、runtimeのplannerは同時に2つ立たない（storeが`BEGIN IMMEDIATE`で再検査してから記録する）。

## Entities

pathは`src/domain/`から。

| 知りたいこと | コードの入口 |
| --- | --- |
| task・goal・proposal・run・receipt・finding | `task.rs`の`Task`、`goal.rs`の`Goal`、`proposal.rs`、`run.rs`の`TaskRun`、`receipt.rs`、`finding.rs` |
| 入力型・復元用・読み取り用の型 | `input.rs`、`views.rs` |
| provider・変更の種類・goalのラベル・path | `worker.rs`、`change.rs`、`goal_tag.rs`、`scope.rs` |
| IDとcommitのnewtype、業務上の拒否 | `ids.rs`、`error.rs`の`DomainError` |

- sessionとworkspaceは独立のentityにせず、`TaskRun`のIDと`workspace_id`で表す。

## Current operations

| 知りたいこと | コードの入口 |
| --- | --- |
| taskの遷移と編集 | `TaskStatus::transition`、`task::edit`、`task::edit_ended_verify`（ADR-t883-1）、`task::duplicate_refusal` |
| 優先度 | `domain::base_priority`（ADR-t1639-1・ADR-t1811-1）、誰が置いたかは`PriorityBy`、由来は`RecordedOrigin`、`task::set_goal`、`application::effective_priority`（ADR-t791-1） |
| slotとclaimの順 | `domain::slot_order`、`dependency_graph`の`candidates`、`domain::claim_order` |
| proposalとplan review | `proposal.rs`、`TaskStore::submit`、`infrastructure::proposals::approve`、`domain::lint::lint`、`domain::plan_review`、follow_upの所属は[所属の判断](follow-up-membership.md) |
| claimと候補 | `task::claim`、`infrastructure::sqlite`の`READY_QUERY`、buildを待つclaimは`domain::build_wait`、読む口は`application::claim_view`（ADR-t1992-1） |
| goalのclose・開き直し・依存 | `goal::close`、`goal::check_follow_ups`、`goal::reopen`、`goal::check_accepts_dependents`、acceptanceの版は`GoalDetail` |
| goal review | [Goal review](supervisor-lifecycle/goal-review.md) |
| cancelの理由の分類コード | `domain::cancel_reason`、`dagq cancel --help`（[ADR-t947-4](../adr/2026-09-28-t947-4-cancel-carries-a-reason-code.md)） |
| 取り残された依存 | `goal::StrandedDependency`、`infrastructure::stranded` |
| taskのhold | `domain::task_hold`（予定は[taskのhold](../plans/task-hold.md)） |
| attentionと表示 | `domain::AttentionNext`、`src/view.rs` |
| CIの見張り | [CI watch](supervisor-lifecycle/ci-watch.md) |
| 役割の柵 | `domain::authorization`、[Authorization](authorization.md)の「Policy」 |

## 集約: TaskとGoal

follow_upの深さは集約でなくstoreの列（`DraftPlannerStore::follow_up_depth`）。

## 集約: TaskRun

| 知りたいこと | コードの入口 |
| --- | --- |
| statusの遷移の判断 | `src/domain/run.rs`のコマンド |
| 遷移が記録するevent | `run::Recorded`（`run/recorded.rs`） |
| `last_error` | `TaskRun`の欄、コードは`domain::reason::last_error_code` |
| runのfileのpath | `RunPaths` |
| 復旧のラウンド | `domain::triage_state`（[triage](supervisor-lifecycle/triage.md)） |
| receiptの検証 | `domain::validation::judge`、`already_satisfied`は`domain::satisfied`、着地の検証は[Validation](supervisor-lifecycle/validation.md) |
| 履歴と回数の上限 | `run/history.rs`の`RunHistory`、`domain::resume`（ADR-t946-1） |
| payloadとeventのkind名 | `run/payload.rs`、`domain::event_kind` |

- resume・review・reviseの進みは新しい状態を足さず、eventで表す（[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。

## Draft planners

runtimeやjobが作ったdraftの束ごとにruntimeが立てるplannerで、規則・store・流れは[Draft planners](supervisor-lifecycle/draft-planners.md)が持つ。

## Planning requests

人がinboxに頼んだ計画の依頼で、規則と型は`src/domain/plan_request.rs`、流れは[inboxからの計画の依頼](supervisor-lifecycle/plan-planners.md#inboxからの計画の依頼)。

## providerの切り替えの理由（`SwitchReason`）

- 入口は`domain::provider_switch`と`domain::headless_job`で、流れとADRは[Agent provider lifecycle](provider-lifecycle.md)が持つ。

## 理由の分類コード（`code`）

- 一覧と意味は`domain::ReasonCode`、置き方と数え方は`src/domain/reason.rs`（ADR-0034決定1）、backendの呼び出しの失敗の`backend_*`は`application::reason_of_error`。

