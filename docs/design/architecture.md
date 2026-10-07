---
id: design-architecture
type: design
title: レイヤーとコンテキストの境界（contextごとの所有・判断・操作・公開するport・依存の向き・境界をまたぐtransaction・検査できる規則・今の違反）
status: current
created: 2026-10-04
scope: system
related:
  - adr-t1545-1
  - adr-0013
  - design-measurement
  - adr-0032
  - adr-0054
  - adr-t1410-1
  - adr-t1453-1
  - design-overview
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle-supervise
---

# レイヤーとコンテキストの境界

## 概念

### 目的

runtime（`src/`と`crates/`）の責務を、レイヤーとコンテキスト（context）の2軸で分ける今の境界と、それを守る規則の正本。
決めた理由は[ADR-t1545-1](../adr/2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)決定1をamends）が持つ。
moduleの説明は[overview](overview.md)、集約は[Domain model](domain-model.md)、tableは[Persistence](persistence.md)、supervisorのループは[`supervise`](supervisor-lifecycle/supervise.md)が持つ。
reviewのsubagentと禁止依存の検査のscriptは、この文書の規則のID（「[検査できる規則](#検査できる規則)」）と一覧（「[境界をまたぐtransaction](#境界をまたぐtransaction)」）を参照し、規則の本文を写さない（ADR-t1545-1決定4）。

### 全体の流れ

```text
起動部分（src/main.rs → src/compose.rs）: adapterを作り、use caseに注入する
   │
   ▼
infrastructure（src/infrastructure/）: portの実装（SQLite・Git・cmux・agent・process・file）
   │ implements
   ▼
application（src/application/）: use caseとport（trait）。時刻・I/Oはportを通す
   │ calls
   ▼
domain（src/domain/）: 集約・値・判断。I/Oを持たない

context: 計画管理 ──T1 claim──▶ 実行と着地 ──T2 着地 / T7 follow_ups──▶ 計画管理
         観測と分析は全てを読む（書かない）
         host運用が全体を起動・停止し、hostの資源を持つ
```

### 責務と境界

- **レイヤー**（ADR-0013決定1）: `domain`・`application`・`infrastructure`・起動部分（`src/compose.rs`と`src/main.rs`）。
  依存は外から内へだけ向く（起動部分 → infrastructure → application → domain）。
- **context**（ADR-t1545-1決定1）: 計画管理・実行と着地・観測と分析・host運用の4つと、どれにも属さない「[共有の部品](#共有の部品)」。
- 1つのmoduleは1つのレイヤーと1つのcontextに属し、contextはこの文書の各節で決まる（決められないものは「[混在しているmodule](#混在しているmodule)」）。

### 不変条件

- domainはI/O・時計・乱数を持たず、applicationはそれらをportで受ける（L1〜L5）。
- 各contextは自分の状態（table・file・eventの種類・`Supervisor`の欄）だけを書き、他のcontextの状態は公開したportと一覧のtransactionでだけ変える（C1・C5・X1）。
- 観測と分析は読むだけ（C2）。
- 今ある違反は「[今の違反と行き先](#今の違反と行き先)」と許可の一覧にだけ置く。

## 2つの軸

- レイヤーの入口は`src/lib.rs`のdoc comment（各レイヤーとレイヤーの外のmoduleの役割）。
- eventの種類の所有するcontextは`src/domain/event_kind.rs`のmoduleのdoc comment、`Supervisor`の欄の所有するcontextは`src/application/supervise/mod.rs`の`Supervisor`のdoc commentが持つ。

## 共有の部品

どのcontextにも属さず、全てのcontextが使ってよいもの。
業務の判断を持たせない（ADR-t1545-1決定1）。

- IDと値: `domain::ids`・`domain::error`・`domain::reason`・`domain::views`・`domain::input`。
- build識別子: `build_id`（`crates/dagq-broker-protocol`の再export）。
  runtimeのcontextが使うのはI/Oを持たない識別子の規則と`named_commit`だけで、gitを呼ぶ`emit`・`compute`はbuild scriptの側（`src/lib.rs`のdoc comment）。
- 時刻とID生成: `application::ports`の`Clock`（壁時計と単調時計）・`IdGenerator`、実装は`infrastructure::clock`。
- eventの記録: `RunLog::record_runtime_event`・`record_queue_event`と種類の`domain::event_kind::EventKind`。
- 人への問い合わせ: `asks`表と`AskStore`（`infrastructure::asks`）。
  askを開くのと、answerを自分の状態に適用するのは、そのaskの`kind`を持つcontextが行う（例: `worker_question`は実行と着地、`plan_review`は計画管理、`update`はhost運用）。
- actorと認可: `domain::actor`・`domain::actor_model`（headlessのjobの行き先の値を含む）・`domain::authorization`（[Authorization](authorization.md)）。
- 名前・出力・共有の規則: `application::naming`、`tracing`のマクロ、`domain::write_rules`（表をまたぐ書き込みの規則）、`domain::language`。
- `src/broker_material.rs`: brokerのimageの材料で`build.rs`と共有し、stdでファイルを読む。
  host運用の中で`infrastructure::broker_image`が使う。
- `src/migration_numbers.rs`: migrationのファイル名の規則で`build.rs`と共有するI/Oの無い関数。
  domainと同じ扱いで、applicationが参照してよい（L3の例外）。

## 計画管理

goal・task・proposalと、その検査と採否（plan review・goal review・runtimeのplanner・follow_upとfindingのproposal）。

**所有する状態**

- table: `tasks`・`goals`と依存、`proposals`・`plan_reviews`・`goal_reviews`・`planners`・draftと依頼の表、`follow_up_judgements`、検索の索引（`search_index`・`landed_commits`）。
- eventの種類: `task_*`・`dependency_*`・`goal_*`・`proposal_*`・`plan_*`・`planner_*`・`draft_*`・`follow_up_*`・`request_*`・`finding_planner_*`。

**判断**（domain）: `domain::task`・`goal`・`proposal`・`plan_review`・`goal_review`・`planner`・`follow_up`・`lint`・`prediction`・`search`ほか。

**操作**

- application: `application::commands::planning`・`commands::requests`、`application::planner`系、`application::supervise`のplan review・goal review・各plannerのsubmodule。
- infrastructure: `infrastructure::sqlite`（`TaskStore`の実装）と、上のtableと同じ名前のstoreのmodule（`proposals`・`plan_reviews`・`draft_planners`・`follow_up_membership`ほか）。

**公開するport**

- `TaskStore`の読み取りを全てのcontextに公開する。
- `TaskStore::claim`（と`RunTransitions::claim_for_supervisor_in_order`）は実行と着地だけに公開し、T1として扱う。
- `DraftPlannerStore::register_follow_ups`を実行と着地に公開する（T7）。
- `PlanReviewStore`・`GoalReviewStore`・`PlanRequestStore`・`RequestStore`・残りの`DraftPlannerStore`と`TaskStore`の書き込みは内部。

**許す依存の向き**

- 実行と着地の状態（`task_runs`・`run_leases`・`run_processes`）を書かない。
  runの結果は`RunLog`の読み取りとeventで読む。
  `ready --inherit`は実行と着地の`InheritStore`を呼ぶ（T10）。
- 観測と分析の`findings`を書くのはfindingのproposalの採否（T6）だけ。

**境界をまたぐtransaction**: T3・T5・T6・T7・T8・T9（書き手）、T1・T2・T4・T10（実行と着地が書く）。

## 実行と着地

taskをrunにして動かし、検証し、mainへ着地させること（claim・run・session・review・integrate・resume・triageと復旧・e2e）。

**所有する状態**

- table: `task_runs`・`run_leases`・`run_processes`・`session_workspaces`。
- ファイル: queueのdirの`runs/<run-id>/`（`RunFiles`）とrunのworktree。
- eventの種類: `run_*`・`lease_*`・`claim_*`・`worktree_*`・`workspace_*`・`wrapper_*`・`session_*`・`turn_*`・`review_*`・`revise_*`・`resume_*`・`triage_*`・`recovery_*`・`integration_*`・`landing_*`・`provider_*`・`push_*`ほか。

**判断**（domain）: `domain::run`（`TaskRun`と遷移、`run::history`・`run::payload`）を中心に、receipt・review・resume・復旧・claimの控え・slot・待ち・stall・e2e・着地の保留・backgroundのwrapperの各module。

**操作**

- application: `application::supervise`のループとslot（`mod.rs`）と工程のsubmodule、`application::session`・`headless_session`・`integrate`・`review`・`recording`ほか。
- infrastructure: `runtime_store`の`transitions`・`recovery`・`session_registry`・`run_log`と`coordination`のleaseとprocessの部分、`adapters`（`GitRepository`・`ClaudeCode`）・`claude`・`codex`・`run_files`・`process`・`background`ほか。

**公開するport**

- `RunLog`の読み取りを全てのcontextに公開する。
- `Verifier`の環境とprogramの読み取りをhost運用の診断に公開する。
  検証コマンドの実行は内部。
- `RunId`・`TaskRun`のview・型付きのeventを値として公開する。
- 着地先のbranchの解決（`Repository::landing_branch`）を観測と分析のCIの見張りに公開する。
  解決できなければ見張りは確かめず、着地先の保留に任せる。
- `application::inherit`の`InheritStore`・`CarriedBranches`を計画管理の`ready --inherit`に公開する（T10）。
- `RunTransitions`・`RunRecovery`・`SessionRegistry`のworkerの部分・`RunCoordination`のleaseとprocessの部分は内部。

**許す依存の向き**

- 計画管理のtaskを読むのは`TaskStore`の読み取りとT1だけ。
  taskの状態を変えるのはT2（着地）・T4（triage）・T10（引き継ぎ）だけ。
- follow_upsは`DraftPlannerStore::register_follow_ups`（T7）で渡し、`tasks`・`draft_origins`をSQLで書かない。
- 観測と分析・host運用の状態を書かない。

**境界をまたぐtransaction**: T1・T2・T4・T10（書き手）、T7（計画管理の公開する関数を呼ぶ）。

## 観測と分析

起きたことを読み、数え、予測し、知らせること（events・watch・stats・KPI・forecast・印・observer・スループットの見直し・CIの見張り）。
計測の作り直しの予定と、新しいportと台帳の係をこのcontextに置くことは[計測](measurement.md)が持つ（[ADR-t1662-2](../adr/2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定6）。

**所有する状態**

- table: `findings`。
- ファイル: queueのdirの日次のKPIのreportとKPIのpushの待ち。
- eventの種類: `observation`・`observe_*`・`finding_*`・`mark_*`・`forecast_recorded`・`kpi_*`・`report_written`・`throughput_review_*`・`candidates_sampled`・CIの見張りの`ci_*`（[CI watch](supervisor-lifecycle/ci-watch.md)）。

**判断**（domain）: `domain::stats`・`kpi`・`forecast`とその下、`marks`・`timeline`・`throughput_review`・`finding`・`ci_watch`ほか。

**操作**

- application: `application::stats`・`kpi`・`forecast`・`report`・`observer`・`watch`・`throughput_review`・`ci_watch`ほかと、`application::supervise`の同名のsubmodule。
- infrastructure: `findings`・`observer`・`throughput_review`・`runtime_store::queue_records`・`kpi_*`・`transcripts`・`ci_watch`・`ci_watch_store`ほか。

**公開するport**

- `QueueRecords`の読み取り（findingとreportとKPIの目標割れ、CIの見張りのeventと`ci_failure`のfinding）を全てのcontextに、`ci_watch::known_failures`を着地の検証に公開する。
  `QueueRecords`のうち計画管理の表を読むmethodは、portを分けるときに計画管理へ移す（[混在しているmodule](#混在しているmodule)）。
  書き込み（`record_*`）は内部。
- findingのIDと`finding_*`のeventを値として公開する（計画管理のfindingのplannerが読む）。
- CIの見張りの保留（`Supervisor::ci_watch_held`・`ci_watch_unreadable`）を実行と着地に読み取りとして公開し、状態を変えるのは`supervise::ci_watch`だけ。
- timerのjob（observer・スループットの見直し）の使えなかった終わりを、型付きの値（`domain::throughput_review::UnusableFinish`・`supervise::observer::UnusableTimerJob`）として実行と着地に公開する。
  控えるのは実行と着地の`supervise::provider`で、同じ終わりを1回だけ控える（[Provider lifecycle](provider-lifecycle.md)）。
- `watch`の`AskNotifier`を所有し、実装はhost運用のinboxの操作を使う。
- KPIのpushの待ち（`queue_pushes`）をhost運用の`inbox_nudge`に公開する。
- `ObserverLog`・`EventReads`は内部。

**許す依存の向き**

- 他のcontextの状態とeventを読むだけで、そのtableを書かず、操作（claim・遷移・answerの適用）を呼ばない（ADR-t1545-1決定2）。
- 書くのは自分の種類のeventと`findings`と、findingに紐づく`blocked`のask（共有の部品）だけ。

**境界をまたぐtransaction**: 無い（T6はこのcontextの`findings`を計画管理が書くもの）。

## host運用

runtime自身をhostで動かし続けること（up・down・install・自動更新・broker・queue service・compileの共有・diskの後始末・hostの計測・inbox）。

**所有する状態**

- table: `supervisors`・`queue_repository`・`schema_floor`・`binary_updates`・`headless_jobs`。
- ファイル: queueのdirの`service/`・`broker/`・`logs/`、launchdのplist、sccacheのserver。
- eventの種類: `supervisor_*`・`update_*`・`release_check*`・`broker_*`・`queue_service_*`・`sccache_*`・後始末の`build_outputs_removed`・`scratchpad_removed`・`run_tmp_removed`・`inbox_*`・`backend_call_failed`・`headless_job_stopped`・`provider_executable_relocated`。

**判断**（domain）: `domain::broker`・`disk`・`sccache`・`release_update`・`queue_service`・`host_metrics`ほか。

**操作**

- application: `application::lifecycle`（`up`・`down`）・`install`・`update`・`broker`系・`queue_service`・`sccache`・`actor_executor`・`execution`ほかと、`application::supervise`の同名のsubmoduleと`disk`・`cleanup`・`sweep`・`handoff`・`inbox_nudge`。
- infrastructure: `launchd`・`binaries`・`broker_*`・`queue_service`・`sccache`・`schema`・`telemetry`ほかと、`runtime_store::coordination`のsupervisorの登録と引き継ぎの部分。
  `crates/`のbrokerのcrateもこのcontext。

**公開するport**

- `RunCoordination`のsupervisorの登録と引き継ぎを実行と着地のループに公開する。
- `HeadlessJobStore`（jobのprocessの台帳）を、jobを起動する各contextに公開する。
- `QueueOpener`・`LaunchAgent`・`SccacheServer`・`ProcessControl`・`InstalledPlugin`、actorの起動（`actor_executor`）を他のcontextに公開する。
- `AuditFiles`とCIの見張りのpreflightは内部。

**hostの操作のport**

`WorkspaceBackend`（`src/application/ports.rs`）が次の2つの責務を持つ。
cmuxを使うのはinboxだけ（[ADR-t1433-1](../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。

- inboxのworkspace（cmux）の操作: workspaceを開く・閉じる・在るかを見る・画面を読む・文字とkeyを送る。
  所有する呼び手はinboxと`up` / `down`（`application::lifecycle`）と、inboxの中の`watch --role inbox`の通知（`notify`）。
  `resume_timeout`ほかの待ちの長さもこのtraitにある。
  実装は`infrastructure::adapters::Cmux`がcmuxのCLIを呼ぶ。
- 非対話のrunとruntimeのplannerのsession wrapperを、workspaceなしのbackgroundのprocessとして扱う操作: 起動（`launch_background`）・停止（`stop_background`、`WrapperStop`を返す）・生死確認（handleを渡した`exists`）。
  所有する呼び手はsupervisor（`supervise::background`・`reopen`、起動は`actor_executor`経由）とruntimeのplanner（`application::planner`）で、`stats`は最後のhandleの生死を`ProcessControl`で読むだけ。
  supervisorの実装`BackgroundSessions`は`BackgroundWrappers`に委ねcmuxは拒む。
  停止の記録（`wrapper_stopped`）は`application::recording::RecordingBackend`が書く。
- 落とし穴: backgroundのhandle（`BackgroundHandle`）はworkspaceのIDと同じ引数で渡り、画面とkeyの操作は拒まれ、色と印は何もしない。
- 決定は[ADR-t1404-1](../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)、停止の流れと記録は[非対話のworker](supervisor-lifecycle/headless-worker.md#workspaceなしのbackgroundのwrapper)の「停止の記録」。
- portを2つに分けたら、この項を分けた後のtraitの名前と分け方に直す（C7）。

**許す依存の向き**

- 他のcontextのuse caseを起動・停止してよいが、他のcontextの状態は公開したportで変える。
- brokerのcrateはrootの`dagq`のcrateに依存しない（`dagq-broker-protocol`だけを共有する。[ADR-t827-1](../adr/2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)）。

**境界をまたぐtransaction**: 無い。

## 混在しているmodule

contextを1つに決められず、分ける先を持つもの。

| module | 混ざっているcontext | 分ける先 |
| --- | --- | --- |
| `src/application/ports.rs` | 全てのcontextのportと`Queue`（storeのportのsupertrait）・`QueueOpener` | contextごとのmodule、use caseが要るportだけを取る形（C4） |
| `src/application/supervise/mod.rs`の`Supervisor` | 4つのcontextの欄を1つのstructで持つ（C3） | contextごとに分ける |
| `src/compose.rs` | 全てのcontextの組み立て | contextごとの組み立てのmodule |
| `runtime_store::session_registry`（`SessionRegistry`） | 実行と着地の`session_workspaces`と、計画管理の`planners` | portの分割 |
| `application::queue_reads` | 読み取りの入口で、各armが自分のcontextのportを読む | 未登録（follow_up） |
| `application::prompt` | workerのprompt（実行と着地）、inbox・plannerのprompt（計画管理）、observerとスループットの見直しのprompt（観測と分析） | 未登録（follow_up） |
| `application::health` | `status`・`doctor`とattention（観測と分析）、`recover`（実行と着地）、`doctor`のhostの部分（host運用） | 未登録（follow_up） |

## 境界をまたぐtransaction

複数のcontextの状態を1つの`BEGIN IMMEDIATE`のtransactionで変えてよいのは、この一覧のものだけ（ADR-t1545-1決定3）。
原子性を守るための例外で、[Persistence](persistence.md)のclaimとlease・着地の規則を弱めない。
一覧に足す・外す変更は同じ変更でこの表を直す。
変える状態の細目は表のコードの関数が持つ。

| ID | transaction | 書き手 | 主に変える状態 | 理由 | コード |
| --- | --- | --- | --- | --- | --- |
| T1 | claim | 実行と着地 | `tasks`を`in_progress`に、`task_runs`と`run_leases`を作る | 持ち主の無いrunとrunの無い`in_progress`を作らない（ADR-0054） | `runtime_store::transitions`→`sqlite::claim_task` |
| T2 | 着地の完了 | 実行と着地 | runを`integrated`、taskを`completed`に（triggerが検索の索引を書く） | mainへの着地とtaskの完了を食い違わせない | `runtime_store::transitions`の`finish_integration` |
| T3 | plan reviewのverdictの適用 | 計画管理 | askを閉じ、`proposals`とその`tasks`の状態を揃える | proposalとそのtaskを1回で揃える | `infrastructure::plan_reviews`の`decide_plan` |
| T4 | triageのanswerの適用 | 実行と着地 | askを閉じ、runとtaskの次の状態を決める | runの失敗の扱いとtaskの次の状態を1回で決める（ADR-0047決定40） | `runtime_store::recovery`の`decide_triage`→`sqlite::transition_task` |
| T5 | goal reviewのverdictの適用 | 計画管理 | askを閉じ、goalを閉じるか足りないtaskを登録する | goalの判定と隙間のtaskを1回で揃える | `infrastructure::goal_reviews`の`decide_goal` |
| T6 | findingのproposalの採否 | 計画管理 | proposalを作り`findings`を結び、採否を`findings`に戻す | 二重のproposalを作らず、採否をfindingに戻す | `infrastructure::finding_planners`の`submit_linking`・`settle_findings` |
| T7 | follow_upsのdraftの登録 | 計画管理（実行と着地が呼ぶ） | runを読み、draftの`tasks`と`draft_origins`を書く | 着地したrunのfollow_upsを由来つきで1回だけdraftにする | `infrastructure::draft_planners`の`register_follow_ups`（T2とは別のtransaction） |
| T8 | follow_upの所属の判断の記録 | 計画管理 | 判断の行・taskのgoalの移動・要るなら`correct_goal`のask | 判断と所属と人への問いを食い違わせない（ADR-t1504-2決定1・6・9） | `infrastructure::follow_up_membership`の`judge_follow_up` |
| T9 | achievedの後の訂正のanswerの適用 | 計画管理 | askを閉じ、goalを開き直すかfollow_upを移す | 答えとgoal・所属・残った問いを1回で揃える（ADR-t1504-2決定9） | `infrastructure::follow_up_membership`の`decide_correction` |
| T10 | 手での引き継ぎ | 実行と着地（`ready --inherit`が呼ぶ） | leaseを消しaskを閉じてtaskを`ready`に | 復旧jobと競合せず一度だけ引き継ぐ（ADR-t1962-1） | `runtime_store::recovery`の`inherit_by_hand` |

所属の判断の流れは[所属の判断](follow-up-membership.md)が持つ。

## 検査できる規則

規則のIDは検査のscriptとreviewのsubagentが参照するので変えない。
規則を変えるときはIDを足すか、古いIDを「廃止」と書いて残す。
各規則の「検査」は、script（機械）で見るかreviewで見るかを書く。

### レイヤーの規則

- **L1** `src/domain`のコードはapplication・infrastructure・起動部分とレイヤーの外のmoduleを参照しない。
  `#[cfg(test)]`の中も同じ。
  検査: script。
- **L2** `src/domain`の本番のコード（`test=false`のビルドに残りうるコード）はDB・ファイル・process・network・時計・乱数のID・`anyhow`を参照しない（ADR-0013のAlternativesの機械化）。
  検査: script（禁止するpathの一覧はscriptが持つ）。
- **L3** `src/application`のコードはinfrastructure・起動部分とレイヤーの外のmoduleを参照しない。
  `#[cfg(test)]`の中も同じ（testはapplicationのtest doubleを使う）。
  例外は共有の部品の`crate::migration_numbers`だけ。
  検査: script。
- **L4** `src/application`の本番のコードはDB・ファイル・外のcommand・壁時計・乱数のIDを直接使わず、portを通す。
  `#[cfg(test)]`の中でfixtureを作るファイル操作はよい。
  検査: script。
- **L5** `src/application`の状態の判断（遷移・回数と上限・送るか・待つか）は、時刻を`Clock`か値の引数で受け、判断の中で時計を読まない（[ADR-t1410-1](../adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)）。
  待ちの判断は、今の時刻と観測を引数に取り次の操作を値で返す副作用のない関数にし、境界の時刻（ちょうど閾値・その1 ms前）をunit testで確かめる。
  時計を読んで結果をportで実行するのは呼ぶ側の薄い処理にする（例: `supervise::session`・`reopen`の待ちの判断）。
  検査: review（残る`Instant::now`が多いので、減るまでscriptに入れない）。
- **L6** `src/infrastructure`のコードは`crate::compose`とレイヤーの外のmoduleを参照しない。
  検査: script。
- **L7** 起動部分（`src/compose.rs`とその下のmodule）はadapterを作ってuse caseに注入する配線だけを持ち、判断・時刻の読み取り・eventのpayloadの組み立てを持たない。
  検査: review（組み立てをcontextごとのmoduleに分けた後にscript）。
- **L8** レイヤーの外のmodule（`view`）は起動部分と同じ外側に置き、domain・applicationを使ってよいが、domain・application・infrastructureから参照されない（L1・L3・L6）。
  新しいmoduleをレイヤーの外に足さない。
  検査: script（L1・L3・L6として）とreview。

### コンテキストの規則

- **C1** あるcontextの`src/infrastructure`のstoreのmoduleは、自分のcontextのtable（各節の「所有する状態」）にだけ`INSERT`・`UPDATE`・`DELETE`を書く。
  他のcontextのtableを書くのは「[境界をまたぐtransaction](#境界をまたぐtransaction)」の一覧のものだけ。
  検査: review（portをcontextごとに分けた後にscript）。
- **C2** 観測と分析のコードは、他のcontextの状態を変えるportのmethod（`TaskStore`・`RunCoordination`・`DraftPlannerStore`の書き込み、`RunTransitions`・`RunRecovery`・`PlanReviewStore`・`GoalReviewStore`）を呼ばない。
  検査: review。
- **C3** `application::supervise`のsubmoduleは、自分のcontextの`Supervisor`の欄（`Supervisor`のdoc comment）だけを変える。
  他のcontextの欄は読むか、そのcontextの関数を呼ぶ。
  検査: review（`Supervisor`を分けた後にscript）。
- **C4** 新しく足す・変えるuse caseは`Box<dyn Queue>`・`&mut dyn Queue`・`QueueOpener`を取らず、要るportだけを取る（ADR-0013決定1の「portは原則applicationに定義する」のまま、幅を狭める）。
  検査: review（portを分けた後にscript）。
- **C5** 他のcontextの公開していないport（各節の「公開するport」で内部としたもの）を使わない。
  検査: review。
- **C6** 他のcontextのeventを読むときは、kindを`domain::event_kind`か所有するdomainのmoduleの定数で名指し、状態の判断に使うpayloadは文字列のkeyではなく型付きの復元の値で読む。
  古いeventの読み取りは保つ。
  検査: review。
- **C7** 新しいportは、どのcontextが所有し、どのcontextに公開するかをこの文書の該当の節に足してから置く。
  `application::ports`に足すときは、portをcontextごとのmoduleに分けた後はそのmoduleに置く。
  検査: review。

### transactionの規則

- **X1** 複数のcontextの状態を変える1つのtransactionは、上の一覧のIDを持つものだけ。
  新しく要るときは同じ変更で一覧に足す。
  検査: review。
- **X2** 境界の規則のためにtransactionを分けない。
  一覧のtransactionの述語（`WHERE status IN (...)`・`renew_lease`・`BEGIN IMMEDIATE`）を外へ出さない（ADR-0013決定9）。
  検査: review。
- **X3** 一覧のtransactionの中で、他のcontextの状態を変えるのは、そのcontextのinfrastructureが公開する関数（`infrastructure::sqlite::transition_task`など）を通す。
  SQLの文を書き写さない。
  検査: review。
  今のT1・T2・T6はこの形になっていない（「[今の違反と行き先](#今の違反と行き先)」）。

### 検査の範囲

- scriptは`scripts/check-layer-deps.sh`で、L1・L2・L3・L4・L6を`src/domain`・`src/application`・`src/infrastructure`の`.rs`に当てる。
  CIが流し、`src/`を変えるtaskのverifyに付ける（[taskの登録](../development/task-registration.md)の「推奨の組み合わせ」）。
- 数えるのは参照のpathで、コメント・docのlink・文字列の中は数えず、testの中はL1・L3・L6だけで数える。
  細目（`use`の組の展開、testとする`cfg`の形と範囲、`--self-test`）はscriptの先頭のコメントが持つ。
- SQLのtrigger（migrationが作る`search_*`）が書く`search_index`・`landed_commits`は、計画管理の検索の索引の書き込みで、C1の違反に数えない（trigger自体は計画管理が所有する）。
- 許可の一覧は`.config/layer-deps-allow.txt`で、1行1項目の`規則 | path | 参照 | 行き先のtask | 理由`。
  各欄の値・読まない行・落ちる条件（一覧に無い参照、もう無い項目など）は一覧の先頭のコメントが持つ。
- 今ある違反は、理由と行き先のtaskを持つ許可の一覧にだけ置く（ADR-t1545-1決定4）。
  違反を直す変更は、同じ変更で一覧の項目と「[今の違反と行き先](#今の違反と行き先)」の行を消す。
- 「検査: review」の規則と、境界を変えた差分がこの文書と許可の一覧を直しているかは、reviewのsubagent `architecture-boundaries`（[Review](supervisor-lifecycle/review.md#reviewのsubagent)）が見て、scriptの規則は見ない。

## 今の違反と行き先

scriptが検査する規則（L1・L2・L3・L4・L6）の行は、許可の一覧`.config/layer-deps-allow.txt`の項目と一致し、行き先のtaskは一覧の項目が持つ。
reviewで見る規則の行は、行き先をこの表の言葉で書く。

| 規則 | 場所 | 違反 | 行き先 |
| --- | --- | --- | --- |
| L5 | `src/application`の`Instant::now`（`lifecycle.rs`・`supervise/mod.rs`・`supervise/recovery.rs`・`supervise/jobs.rs`・`supervise/triage.rs`ほか） | 判断が実時間を読む | 注入した`Clock::monotonic`へ。残りは計測の後に判断 |
| L6 | `src/infrastructure/queue_service.rs`（`crate::view::task_detail`） | infrastructureがレイヤーの外を呼ぶ | 許可の一覧の項目 |
| C3 | `Supervisor`と、`impl Supervisor`を持つ`supervise/`のsubmodule | submoduleが他のcontextの欄を変える | `Supervisor`の分割 |
| C4 | `Box<dyn Queue>`などを取るuse case（`application::lifecycle`・`health`・`supervise`ほか） | 要るportだけを取っていない | portの分割 |
| C5 | `SessionRegistry`が計画管理の`planners`を書く | 実行と着地のportに計画管理の状態が混ざる | portの分割 |
| C6 | `src/application/supervise/resume.rs`・`supervise/recheck.rs`・`supervise/recovery.rs`ほか | 判断に使うeventのpayloadを文字列のkeyで読む | 型付きの復元の値（`domain::run::payload`の形）へ |
| X3・C1 | T1: `src/infrastructure/sqlite.rs`の`claim_task`（計画管理のstore）が`INSERT INTO task_runs`を書く | 計画管理のstoreが実行と着地の表をSQLで直接書く | 未登録（follow_up） |
| X3・C1 | T2: `src/infrastructure/runtime_store/transitions.rs`の`finish_integration`が`UPDATE tasks SET status='completed'`を書く | 実行と着地のstoreが計画管理の表を`transition_task`を通さず書く | 未登録（follow_up） |
| X3・C1 | T6: `src/infrastructure/finding_planners.rs`（`settle_findings`ほか）が`UPDATE findings`を書く | 計画管理のstoreが観測と分析の表を`infrastructure::findings`を通さず書く | 未登録（follow_up） |
