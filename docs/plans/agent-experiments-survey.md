---
id: plan-agent-experiments-survey
type: plan
title: Agent比較実験の入力となる現行実装の調査
status: active
created: 2026-11-17
related:
  - plan-agent-experiments-brief
  - design-supervisor-lifecycle
  - design-measurement
  - design-agent-eval
---

# Agent比較実験の入力となる現行実装の調査

[人の設計依頼の原文](agent-experiments-brief.md)の「調査事項」の7項目に対応する調査資料。
この文書は設計の決定を行わず、後続のADR・design・実装前の契約・段階計画の入力にする。
根拠の基準は2026-11-17のローカルの `main` / `origin/main` / 作業開始時HEADが一致した `9479ceafa33e8accec206f67ee84483933aa419f`。
ネットワークのmainを更新した調査でも、本番queueの実測でもない。
設定値はこのcommitの `dagq.toml` であり、稼働中のprocessに適用された値までは断定しない。

「事実」はコードで確認したもの、「決定・未実装」は受理済みADRにあるが入口に実装が無いもの、「仮説」は検証が要る推論、「提案」は後続の判断の候補を表す。
各節末の再利用・拡張・作り直しは、変更量でなく長期の保守と変更容易性を基準にする。
コードのpathはrepository rootからの相対pathで、型と関数を検索の入口として添える。

## 1. 役割とAgentの定義、provider・model・effort・promptの設定

### 事実：役割・provider・agent定義は別の軸

`src/domain/actor_model.rs` の `ModelRole` / `RoleModel` / `RoleModels` はworker以外の役割の起動設定を表す。
依頼のplannerは実運用では `runtime_planner`、plan reviewerは `plan_review`、reviewerは `review` に対応する。
`planner` の設定名は残るが、人が開くplannerは廃止されている（[Actor model](../design/supervisor-lifecycle/actor-model.md)、ADR-t1394-1）。
workerはこのenumの要素ではなく、`src/domain/worker.rs` の `Worker` と `src/domain/worker_model.rs` の `WorkerSession` が持つ。
したがって4役を覆う単一の「Actor→Agent割当」型が既にあるとは言えない。

`src/domain/actor.rs` の `ActorContext` はeventの主体や権限の文脈であり、比較条件としてのAgentカタログではない。
`src/domain/actor_model.rs` の `AgentModel` はreviewのagentが明示する起動の組である。
`.dagq/agents/<name>/AGENT.md` はreviewの検査定義で、Claude CodeやCodexという実行providerそのものを指す名前ではない。
組み込みの定義は `src/domain/review_builtins.rs` が持ち、project側の規則との境界は[Review built-in](../design/supervisor-lifecycle/review-builtin.md)が説明する（ADR-t2259-1）。
この3つの「actor / agent」を同じ識別子として扱うと、権限・検査内容・実行条件が混ざる。

### 事実：設定から起動までの経路

`src/infrastructure/run_env.rs` の `parse_config` → `Config` → `RoleModels::check` / `RoleModels::launch` が `[roles.<role>]` を解決する。
役割の表が無い場合の起動と、model / effortを片方だけ書いた場合の補完は `RoleModels::launch` に分岐がある。
`RoleModel::check` と `runs_on` / `CODEX_ROLES` がproviderと役割の組の可否を判じる。
現時点でruntime plannerはCodex対応の役割に含まれず、`src/application/planner.rs` の `launch_planner` はClaudeのwrapperを起こす。
役割のprovider設定だけでplannerをClaude/Codexの50:50にできる、と解釈してはいけない。

`src/domain/actor_model.rs` の `job_route` / `job_start_route` と `ActorLaunch::switched` が、予定providerと利用不能時の移動を扱う。
`RoleModels::review_plan` はrunのprovider群とagent別設定を適用した `ReviewPlan` を返し、fallback前の予定providerを記録する。
`[review.subagents.<name>]` のmodel / effortと役割の設定の継承は `written_agent_launch` / `checked_agent_launch` が検査する。
権限とCLIへの変換は `src/application/actor_executor.rs` とprovider adapterの境界で行う（[Provider lifecycle](../design/provider-lifecycle.md)）。

promptは `src/application/prompt/` のworker・planners・plan_review・run_review等の組み立てが持つ。
worker向けはrepository指示やpluginも読み、reviewの規則は信頼するlanding branchから選ぶ。
起動の実行ファイルは `src/infrastructure/adapters.rs` の `provider_executable_on` / `provider_at_entry` で解決し、消えたpathの解決し直しを記録する（[Provider executables](../design/provider-executables.md)、ADR-t2079-1）。
実行ファイルpathを固定しても、CLIの実体の更新やpromptの変更を固定したことにはならない。

### 事実：今の2つの試しと決定・未実装

`src/domain/worker_model.rs` の `WorkerTrial` は `[worker.trial]` を受け、Claudeのmodel・effortの候補を `LADDER` 上の組に限る。
`choose` は予測の有無と、限定時にはmechanical・予測tokenの下位3分の1を条件にし、`draw` でcontrol / treatmentを選ぶ。
Codexはworkerにmodelを指定するこの試しの対象でなく、effortを受け取り、実際のmodelはturnの記録から読む。
ADR-0079決定4はADR-t3957-1でhash割当に改訂されている。

`src/domain/provider_trial.rs` の `ProviderTrial` / `PairChoice` は `[worker.provider_trial]` を受け、worker×reviewのproviderの組を割り当てる。
`ProviderTrial::check` はtreatmentのworkerとreviewが同じproviderになる設定を拒む。
`src/infrastructure/task_store.rs` の `claim_task_filtered` はpairのBをmodel・effortの試しの外にし、BがfallbackでClaudeになってもその除外を保つ。
これは独立した任意の多群実験でも、依頼にある同種の2群を含む4通りの組でもない（ADR-t4005-1決定1・4・5）。

基準commitの `dagq.toml` は `[worker.trial]` を宣言せず、`WorkerTrial::default` によりその試しは無効。
`[worker.provider_trial]` は有効でBが100%、worker=Codex・review=Claude。
`[roles.review]` もClaudeなので、Aのreviewも設定上Claudeである。
依頼に添付された事前調査の「B 50%」は基準commitの値ではなく、A/Bの新規割当比較は現在の設定では成立しない。
過去にAを割り当てたtaskがBへ振り直される意味でもない。

ADR-t3957-1決定3のruntime plannerのrequest単位の試しは決定・未実装。
`src/application/planner.rs` の `open_runtime_planner` / `open_draft_planner` は `RoleModels::launch` とrevise時のeffort段上げを使い、試しの群を選ばない。
[Actor model](../design/supervisor-lifecycle/actor-model.md)も実装前と明記している。

### 仮説・提案

**仮説**：役割ごとにもう1つ試しの表を足す方式では、候補の追加がclaim・job・設定・KPIの全層へ波及し続ける。
**提案**：役割、起動の組、検査定義、権限を分け、起動の組の解決だけを共通化する候補を比較する。
同種providerの組を許す案と現行の異種を要求する決定の差は、後続ADRで明示的に扱う必要がある。

### 再利用・拡張・作り直しの候補

- **再利用**：`ActorLaunch`、provider adapter、prompt組み立て、既存の設定検査。起動と権限の境界を保てる。
- **拡張**：workerと役割jobの起動の組を共通の入力で表す口。modelのprovider依存の検査を一度で保守できる。
- **作り直し**：2つの専用trialを実験の宣言から解決する層。固定の二群・段の制約・reviewの特例を新しい役割へ複製しないため。
- **goal 114との接続**：実験の宣言と割当は一般A/Bと重なり、actor別・組の起動の解決は追加部分。promptの節の上書きだけではprovider・modelの選択を表せない。

## 2. Planner→Plan Review→Worker→Reviewの制御、Supervisor・queue・差し戻し

### 事実：1本の同期パイプラインではない

全体の地図は[Supervisor lifecycle](../design/supervisor-lifecycle.md)。
`src/application/supervise/` が常駐の周回を持ち、planningとexecutionの状態を別々に進める。
plannerがproposalをsubmitし、plan reviewの判定を適用してtaskがreadyになった後、依存を満たすtaskをclaimする。
実行中のrunからplannerのprocessへ直接受け渡す形ではない（ADR-0041・ADR-0044）。

plan reviewの入口は `src/application/supervise/planning/plan_review.rs` の `PlanningState::start_plan_review`。
結果は `src/infrastructure/plan_reviews.rs` の記録と `PlanReviewStore::finish_plan_review` の経路を通り、pass / revise / concern等を適用する。
reviseの配送は `PlanningState::tend_planners` で、生きたplannerにはturnを追加し、居なければ `src/application/planner.rs` から開き直す。
同じplannerへの配送ではeffortを変えず、新しく開くplannerでは `ActorLaunch::escalated` を使う（[Plan review](../design/supervisor-lifecycle/plan-review.md)、ADR-0079決定7）。

`src/application/supervise/execution/claim.rs` の `Supervisor::fill_slots` → `claim_tasks` → `provision_claimed` が実行枠を埋める。
`src/infrastructure/task_store.rs` の `claim_task_filtered` は、候補のprovider群を選び、利用可能なrouteで絞って、task・run・event・leaseを一つのtransactionに確定する。
provisionでworktreeとwrapperを用意し、turnの依頼をrunの `turns/` に置く。
実行の詳細は[非対話のworker](../design/supervisor-lifecycle/headless-worker.md)と `src/domain/turn.rs` にある。

### 事実：検証とreviewと着地の違い

`src/domain/validation.rs` の `judge` はreceiptとcleanなHEAD・変更path・受け入れ条件の証拠を照合する。
`validating` はtaskのverifyコマンドを流す工程ではない。
verifyの正式な実行はrebase後の `src/application/integrate.rs`（[Validation](../design/supervisor-lifecycle/validation.md)、ADR-0049）。

reviewは `src/application/supervise/execution/landing.rs` の `begin_review` / `tend_review` と `jobs.rs` が進める。
programのreview → 被覆の確認 → 全体とagent別job → 全結果の機械的な集約の順で、`src/domain/review_jobs.rs` が判定をまとめる。
review jobは読むだけで、直すのはworkerの次のturn。
`revise.rs` の `ReviseWatch` が書き直したreceiptを待ち、validationから再び進む（[Review](../design/supervisor-lifecycle/review.md)）。
run全体のrevise上限と、答えの無いjobの1回の再起動は別の数え方である。

### 事実：復旧と引き継ぎは割当の入口を増やす

`src/application/supervise/execution/triage.rs` の `triage_candidates` / `act_on_recovery` は、失敗したrunをretry・retry_inherit・resume・wait・人の判断へ振り分ける。
retryは新しいrun、resumeは同じrunを続けるので、run開始で毎回抽選する案では結果が変わる（[Triage](../design/supervisor-lifecycle/triage.md)）。
`waiting.rs` は人の答え待ちでslotを空け、eventから待ちを復元する（[Waiting](../design/supervisor-lifecycle/waiting.md)）。
`handoff_slots.rs` / `adopt.rs` は引き継ぎ・adoptの復元を担い、走るworkerのwrapperと、止めてやり直すheadless jobでは境界が異なる（[Handoff](../design/supervisor-lifecycle/handoff.md)）。

goalの完了はworkerの成功の和だけで確定しない。
`src/application/supervise/planning/goal_review.rs` の `goal_review_pass` と `src/domain/goal_review.rs` の `reviewable` がgoalの受け入れを検査し、gapやaskへ進める（[Goal review](../design/supervisor-lifecycle/goal-review.md)）。

### 仮説・提案

**仮説**：reviewの試行ごとに別providerを引くと、worker群の効果とreviewの手直しの効果を分離しにくい。
**提案**：割当の決定を起動と分け、再起動は永続の割当を読む入口にする候補を優先する。
利用不能のfallback、上限による待ち、人の答えを待つ時間は実験群の変更と区別して記録する。

### 再利用・拡張・作り直しの候補

- **再利用**：supervisorの工程、lease、turn配送、validation、review集約。実験用の別supervisorを置かず復旧の約束を共有できる。
- **拡張**：claimとplanner・jobの開始で割当参照を渡す口。全入口で同じ保持規則を適用するため。
- **作り直し**：pair専用のreview上書きの判断の置き方。役割の組の一般的な解決へ移すと特例の増殖を避けられる。
- **goal 114との接続**：switchbackは周回や時間窓に関わるが、taskに固定する2試しと同じ制御ではない。一般契約の割当の方式と、差し戻しで保持する境界を混同しない。

## 3. Task・Goal・Run・Session等のID、ライフサイクルと固定境界

### 事実：現在の識別子

`src/domain/ids.rs` は `TaskId` / `GoalId` / `EventId` / `PlannerId` / `ProposalId` / `RunId` 等を別の型にする。
整数IDはqueueの行を指し、`RunId` は空でない文字列として検査されるので、型の検査自体はUUID限定ではない。
`src/infrastructure/clock.rs` のID generatorが実際の新しいrun IDを発行する。
異なるqueueの同じ整数を、外への書き出しで同じtaskとみなす根拠は無い。

`src/domain/task.rs` の `Task`、`goal.rs` の `Goal`、`run.rs` の `TaskRun` は別の集約。
`TaskRun` はtaskの1回の実行をclaimから着地まで持ち、base・branch・結果commitを結ぶ。
runのstatusと、`src/domain/run_phase.rs` の工程の区間・attemptは別である。
[Persistence](../design/persistence.md)の制約によりtaskの未完了runは高々1つで、同じtaskには失敗後の新しいrunがありうる。

`src/domain/plan_request.rs` の `PlanRequest` / `RequestRef` はtaskが出来る前の依頼を持つ。
`creation_origin` はplannerやproposalからrequestとの結び付きを読む。
requestは無い場合や複数に結び付く場合があり、goalとrequestの1:1は保証されていない（[Plan planners](../design/supervisor-lifecycle/plan-planners.md)）。

`src/domain/sessions.rs` の `SpanRef` / `SpanContext` / `Scope` はsessionの区間を表す。
`src/application/session.rs` の `Session` はwrapperが使うportや起動条件の束であり、全役共通の永続Session集約ではない。
`src/domain/turn.rs` は同じrunの複数turnを表し、provider変更やcontextの上限で会話のsessionを新しくしうる。
Codexはthreadを自分で名付けるので、jobの開始時点のsession IDが無いことも正常に表す（`src/domain/headless_job.rs` の `JobSession::record`）。
`src/domain/stats/executions.rs` の `Execution` は非対話のturn・jobの1回と、対話sessionの区切りの計測の記録であり、永続の汎用 `Invocation` 集約ではない。
`JobOn` / `JobRef` とreviewのattempt・job名が実行を結ぶが、依頼の `Actor Invocation ID` という全役共通の新しいIDは確認できない。

### 事実：現在のstickyは群の保持であり、設定全体の固定ではない

`src/domain/worker_model.rs` の `draw` は試し名とtask IDの `name:task` のSHA-256先頭8byteを整数にして100で割った余りを割合と比べる。
`src/domain/provider_trial.rs` も同じ `draw` を使い、異なる試し名で別々に引く。
二群の決定的割当で、少数のtaskの件数比を厳密に揃える仕組みではない。
明示のseed・実験設定の版・多群のweightの型はこの2試しには無い。

`worker_model::choose` は過去の最初の群と試し名を復元するが、候補は現在の `trial.session(group)` から選ぶ。
`ProviderTrial::choose` も群と名前を保持し、`assigned` が現在のtreatmentのproviderを使う。
どちらも無効化の判定が保持の探索より先にある。
したがって群・名前を保つことを、実験条件のsnapshotを永続化してそのまま復元することと同一視できない。
`src/infrastructure/task_store.rs` はその後 `WorkerSession::claimed` で人のhold解除によるmodelの選択や既存の段上げを適用する。
providerを選んだhold解除はpairの試しの対象外になる。

### 仮説・提案：固定単位の候補

| 候補 | 現行との接続 | 仮説としての利点と限界 |
| --- | --- | --- |
| task | 2試しの群・retry後も同じTaskId | worker×reviewの初期候補。計画を作る前のplannerには使えない |
| run | claimで作成、resume・reviseで保持 | 実行単位の記録に自然。retryで変わり、task全体の効果は混ざる |
| request | PlanRequestとcreation_origin | planner×plan reviewの候補。requestの無いdraft・findingは別途扱いが必要 |
| goal | GoalIdと所属・依存 | 下流もまとめる候補。ただし最初の計画時にはgoal自体が無い場合を解決する必要がある |
| CLI呼び出し | Executionとjob・turn | 起動ごとの再抽選は差し戻し比較を変質させる。主に実態の識別境界に使う候補 |

**提案**：task単位をworker側の初期候補とし、plannerの境界はrequest起点の追加契約として比較する。
ADR-t3957-1決定3のrequest単位という方針と、その未実装の部分を区別して前提にする。
割当を再現できることは、LLMの出力、CLI更新、外部状態を完全に再現することを意味しない。
同じgoalの依存taskは前のcommitと情報を共有するため、task単位の群を独立標本と仮定するのは未検証である。

### 再利用・拡張・作り直しの候補

- **再利用**：型付きID、requestの出どころ、run・turn・jobの関係。別のtask/session台帳を作らず結合できる。
- **拡張**：割当が対象に結び付き、起動で参照できる永続の識別。再起動とretryに同じ説明を使える。
- **作り直し**：群だけを保持して現在の候補で解釈する復元。長期の再分析には当時の候補と版が必要になるため。
- **goal 114との接続**：決定的割当・版・最初の記録からの復元は一般契約の担当。task 1692の方針を前提に、task以前の役割と下流の関係を追加する候補を後続で判断する。

## 4. 永続化・event・log・履歴・計測値と成果物

### 事実：SQLiteだけではない

`src/infrastructure/sqlite.rs` の `SqliteQueue` がローカルの既定で、WALのqueue DBを使う。
`src/infrastructure/postgres/mod.rs` には `PostgresQueue` / `PostgresStore` / `PostgresTx` が実在する。
`src/infrastructure/postgres/client.rs` の接続は `postgres` featureで有効になり、`exec.rs` / `sql_dialect.rs` で共有SQLを写す。
PostgreSQLを新設すべき空白と扱うのは誤りである。
一方、queueのDBがPostgresでもrunのprompt・receipt・logまでDB内に入る構造ではない（[Persistence](../design/persistence.md)）。

`src/infrastructure/runtime_store/` はrunの遷移・event・lease・台帳等の読み書きを持つ。
`coordination.rs` / `transitions.rs` / `run_log.rs` と `src/infrastructure/task_store.rs` が、状態とeventを同一transactionで確定する入口になる。
`src/infrastructure/store_contract/` の共通契約はSQLiteとPostgresに同じ読み書きの性質を確かめる。

### 事実：正本・ビュー・消えてよい材料

[計測](../design/measurement.md)のSSOT表とADR-t1662-2はStateStore・EventStore・SessionStepStore・NodeSampleStore等を正本、LedgerStore・ReportStoreをビューに分ける。
`src/domain/ledger/` の `fold` / `builder` がevent等から行を作り、`window` がstats・KPI・reportの共通の読み取りを担う。
`src/application/kpi.rs` の `kpi` はこの窓を読み、StateStore・Git・run dirを直接読んで数値を補わない。
`src/domain/claim_facts.rs` の `attributes` / `KEYS` がclaimの既知の値を開いた属性の組へ写す。
群の追加ごとに台帳の固定列を増やす必要はない（ADR-t1662-1）。

run dirのprompt・receipt・log・`worktime.jsonl`、Claude transcript、Codex rolloutは消えてよい材料である。
取り込まれた値と、元の全ログが永続に残ることは別である。
`src/infrastructure/location.rs` の `QueueLocation` がqueueとrunファイルの場所を解決する。
receiptはvalidation・integration時のeventに写すので、worktree削除後も取り込まれた内容を辿れる（`runtime_store/transitions.rs`、[Persistence](../design/persistence.md)「Runtime ownership」）。

`src/application/main_log.rs` の `records` と `src/application/supervise/observation/main_log.rs` はmainのfirst-parent履歴・変えたpath・到達時刻をeventへ記録する。
`src/domain/stats/main_log.rs` の `fold_main_log` はそれを読む（[Main history](../design/main-history.md)）。
Gitのobjectとrefの到達可能性を含め、任意のdiffの永久保存を保証する仕組みではない。

### 事実：実行の意図・実態・費用・欠測

`ActorLaunch` とreviewの `planned_provider` は起動意図、`JobSession` / `ExecutionTokens` とturnの終了の記録は実態を持つ。
`src/domain/tokens.rs`、`src/infrastructure/claude_turns.rs` / `codex_turns.rs` が取得元と欠測理由を扱う（[Execution tokens](../design/execution-tokens.md)）。
CLI呼び出しのtokenとsession区間のtokenを両方足してはいけない。
Codexの費用はproviderから請求額が返った値ではなく、`src/domain/agent_eval/round.rs` の `ProviderCost::usd` が設定の単価を掛けた換算。
`src/domain/kpi/pair.rs` は単価が無い場合や値が無い場合を理由付きで返す。
現在の換算は読み取り時の設定に依存するので、過去の価格版を固定した汎用Measurement契約とは言えない。

`src/domain/environment.rs` の環境と資源、`runtime_store/run_resources.rs`、`node_samples.rs` はhostや負荷と時間窓を持つ。
[実行環境と資源](../design/measurement-environment.md)と[欠測の契約](../design/measurement-missing.md)は、単位・取得範囲・観測の無い値を0にしない約束を持つ。
`src/domain/session_step.rs` と `runtime_store/session_steps.rs` はseqの重複を除き、配送の抜けを表せる。
ADR-t1662-3の送信主体はagentでなくexecutorである。

### 事実：監査と機密の境界

[Audit](../design/audit.md)と `src/domain/audit/record.rs` / `chain.rs`、`src/infrastructure/audit.rs` はeventと別の監査を持つ。
SQLiteで状態変更と同時に追記する監査を、実験の生ログや計測値の保存先と同一視しない。
同文書は段0のPostgresでは監査を書かないこと、同一UIDに対する保証の限界も明示する。
`src/domain/command_shape.rs` のコマンドの形とtelemetry設定は、未知の値をdigestにする収集量の制御を持つ（ADR-t1673-1・ADR-t3201-1）。
これはprompt・成果物・全ログを漏れなくマスキングする保証ではない。
保持は `src/domain/retention.rs` と設定の `[retention]`、権限はqueue serviceの認可の境界で扱う。
依頼のEvidence / Measurementという全種類共通のversion付きデータ模型・artifact保持契約・結合済みJSONLのexportは、この経路には無い。

### 仮説・提案

**仮説**：材料のpathだけを書き出すと、後日の分析で参照先を失い、当時の判定を追えない。
**提案**：正本に残す最小の証拠と、保持期限のある本文を分け、消えた理由・hash・版を残す候補を検討する。
一般の設定版と測定方法・価格版は別に扱い、秘密の除去・アクセス・保存量を収集時点から契約に含める。

### 再利用・拡張・作り直しの候補

- **再利用**：transaction、EventStore、開いた属性、台帳、共通の窓、TelemetrySink。記録と集計を二重に実装しないため。
- **拡張**：起動・割当・成果物を結ぶ参照、Measurementの版と推計の印、欠測・保持の出力。再分析で意味を失わないため。
- **作り直し**：材料を永久のEvidenceと見なす読み方、現在価格だけによる過去費用の説明。保存契約と測定の版を独立させる必要がある。
- **goal 114との接続**：数値の置き場と実験の版は共通部分。2試しが既に持つclaimとjobの記録を移す際、Evidence本文の保持・exportまで一般A/Bが実装済みとは仮定しない。

## 5. worktree・Gitの着地・並列と衝突

### 事実：runの隔離と着地の直列化

`src/application/supervise/execution/session.rs` のprovision経路がrunごとの作業場所とwrapperを用意する。
`src/domain/views.rs` の `worktree_name` がrunの名前を他のscratchと重ならないようにする。
`src/infrastructure/adapters.rs` の `worktree_command` は同時addのGit管理記録の競合も扱う（[Run worktrees](../design/supervisor-lifecycle/run-worktrees.md)）。
workerは自分のbranchへcommitし、着地とpushは `src/application/integrate.rs` の `Integrator` だけが行う。
runごとにworktreeがあることは、共有するhostの負荷・cache・provider利用上限が独立という意味ではない。

`src/application/integrate.rs` の `land` / `land_integrating` はrebase・receiptの再照合・verify・squash・mainの更新を進める。
`begin_integration` とDBの制約でqueueの着地slotは1つ。
`src/application/integrate/binding.rs` は着地を許した差分のfileとblob hashの組を照合し、承認後に内容が変われば同じ判断のまま着地しない（[Integrate](../design/supervisor-lifecycle/integrate.md)）。
`Repository::advance_main` がmainを進め、元のrun履歴は `refs/dagq/runs/<run-id>` に残る。

### 事実：衝突と環境の差は観測対象

`src/application/supervise/execution/landing.rs` の衝突の事前判定と `recheck.rs` はGitの衝突と、合わせた木の検証を扱う。
衝突でworkerに戻す経路と、着地の検証の失敗で戻す経路がある。
`worker_model::raises` / `for_resume` はtask由来の失敗で段上げし、rebase conflictだけでは上げない。
衝突をagentの品質に直結させず、手直しの理由と段上げを別に読める。

`src/application/supervise/execution/claim.rs` は並列枠・優先度・provider控え・負荷・空き容量等の制約でclaimする。
`dagq.toml` の `[supervisor]` と、衝突の多いpathを控える仕組みは群の実行時刻にも影響する。
同じ割合で割り当てても同じ時間帯・並列度・base commitで実行される保証は無い。

containerのrunでは作業cloneと制御側の着地worktreeの境界がある。
`src/application/integrate.rs` と[Run worktrees](../design/supervisor-lifecycle/run-worktrees.md)の「containerのrunのclone」が参照するADR-t2179-1は、制御側がworkerのcloneでGitを動かさない境界を決める。
providerごとの実行環境の差も含めて比較する必要がある。
掃除は `src/application/supervise/host_ops/cleanup.rs` がlease・processを確認して行い、worktreeやビルド出力を永続のartifactとはしない。

### 仮説・提案

**仮説**：worker×reviewの本番比較には、群が変えた作業時間によって次のtaskのbaseと衝突率が変わる相互作用がある。
**提案**：base・review対象head・着地commit、並列度、待ちと衝突、実行環境を記録したまま提供する。
固定入力のBenchmarkは別のtreeから始め、実験の結果を本番mainへ自動で着地させる通常runと分ける候補を採る。

### 再利用・拡張・作り直しの候補

- **再利用**：run隔離、直列着地、lease、衝突解消、履歴ref。実験がGitの所有境界を増やさずに済む。
- **拡張**：artifact参照と保持、並列・衝突の実験への関連付け。消える作業場所と残す証拠を分離できる。
- **作り直し**：本番worktreeをそのままReplay用の固定入力とする考え方。可変のmain・未追跡の状態を共有すると長期の再現を保てない。
- **goal 114との接続**：指示の上書きやswitchbackだけではGitの初期条件を揃えない。2試しの通常runの着地をBenchmarkの独立反復と呼ばない。

## 6. Agent Eval・test・検証・分析の仕組みと再利用範囲

### 事実：判定の固定入力evalは実在する

[Agent eval](../design/agent-eval.md)と[Eval programs](../design/agent-eval-programs.md)は、dev・hold-out・productionの周とケース・採点・費用上限を持つ（ADR-t1728-1・ADR-t3948-1）。
`src/application/agent_eval.rs` の `AgentEvals` は依頼・結果のuse case。
`src/application/supervise/execution/agent_eval.rs` が依頼を実行し、`src/domain/agent_eval/round.rs` が周と費用を、`record.rs` がeventからの状態を扱う。
supervisorの再起動でもeventから周を読むが、生きたeval processをそのまま引き継ぐ方式ではない。
providerはevalの途中に切り替えず、利用不能時には理由を記録して待つ。

reviewのagentは `.dagq/agents/<name>/evals/` と共有patchを持ち、定義digest・ケース集合digestを記録する。
役割のevalは `src/application/role_eval.rs` の `RoleHarness` で、plan review・goal review・run全体のreviewを扱う。
`identity` はsnapshotとscoreの版を含む識別を返し、`prompt` / `tree` / `score` が本番と同じ役割の入口を共有する。
`src/application/role_snapshot.rs` の `Snapshot` / `PlanInput::capture` は起動時にpromptの入力を固定し、あとからqueueの状態で当時の資料を推測しない。
workerとplannerの生成のEnd-to-End全体を測る汎用Benchmarkはこのharnessの対象ではない。

caseのtreeは本番repositoryとGitの管理情報を共有せず、ケースの期待をtreeに置かない。
queueに届かない起動と読み取り境界は `src/application/actor_executor.rs` とprovider adapterで制約する。
snapshotの欠けや消えたbaseは理由付きでケースにしない。
同じ成果物への独立reviewに再利用できるが、レビューでworkerが直した結果を含む比較とは入力が異なる。

### 事実：evalとKPIは既に判断を持つ

`src/domain/agent_eval/review.rs` 等の採点と `round.rs` はrecall・precision・error・費用を使う。
ADR-t3948-1は閾値を満たす組の中で軽い設定を採用する方針を持つ。
これは「評価の全てが現在もDAGQの外にある」という実装ではない。
依頼の責務境界を新しい実験に適用するとき、既存のevalの合否や採用方針との境界を後続ADRで説明する必要がある。
本調査はそれらの判断を変更しない。

`src/application/kpi.rs` の `kpi` と `src/domain/kpi/pair.rs` の `standing` / `strata` は、台帳から群・fallback・環境原因の層を作る。
ADR-t4005-1の元の「割当を主」と読むのは現行と異なり、ADR-t4118-1、さらに `amended_by` のADR-t4163-1まで辿る必要がある。
現在の主は割当どおり動き環境原因でないrun、別に切り替え・環境の層、副に割当全体を残す。
切り替えと環境は重なるので、層の件数の和が副に一致するとは限らない。
主に残ったrunだけを無作為比較と解釈する根拠は無く、副と欠測件数も外部分析に必要である。

### 事実：testとverifyの境界

`src/domain/worker_model.rs` と `provider_trial.rs` のunit testは、決定的なdraw・群の保持・対象外・割合等を値で確かめる。
`tests/it/worker_model.rs` / `runtime_claim_worker.rs` / `review_subagents.rs` はclaim・起動・記録の境界を通す。
`src/infrastructure/store_contract/` はDB間の契約、`tests/it/agent_eval_rounds.rs` 等はevalの周の配線を確かめる。
testの合否、taskの受け入れverify、reviewのprogram、LLM判定、KPIの目標割れは異なる結果である。
全testの個別結果を汎用実験に自動で関連付ける仕組みまでは確認できない。

### 仮説・提案

**仮説**：既存のevalの起動・snapshotと算術集計を共通化し、採点のpolicyを外へ渡す構成なら通常運用を壊さず再分析しやすい。
**提案**：実験のexportはrawの判定と計測を優先し、現行evalのscoreを唯一の品質定義にしない。
plannerは生成taskの数自体を変えるので、taskの成功率だけで比較せずrequest / goalと下流を結ぶ候補を検討する。

### 再利用・拡張・作り直しの候補

- **再利用**：snapshot、隔離tree、同じjob起動、費用上限、event復元、共通の窓。別のLLM実行基盤を置かずに済む。
- **拡張**：通常観測 / Benchmark / 将来Replayの識別、外部採点へ渡す入力と結果、変更に強い契約test。
- **作り直し**：実験群ごとの固定KPI分岐の追加方式と、既存evalの採用基準を全実験へ強制する構成。役割・測定軸・外部policyを独立して変えられる形が必要。
- **goal 114との接続**：一般A/Bと2試しは通常運用の比較で、固定snapshotのevalとは制約が違う。割当・実行記録の共有と、評価のpolicyの共有は別に判断する。

## 7. CLI・API・設定の慣習、migrationと後方互換性

### 事実：CLIからqueue serviceのuse caseへ

`src/main.rs` はコマンドの入口、`src/compose/` がadapterを組み立て、`src/application/` がuse case、`src/domain/` が判断を持つ。
queueの書き込みはqueue serviceを経由し、workerはDBを直接開くAPIではない（[Persistence](../design/persistence.md)、[Supervise](../design/supervisor-lifecycle/supervise.md)）。
`src/application/supervisor_service/wire.rs` の `Wire` はserviceとの損失のない表現を持つ。
CLIの人向けJSONは必ずしも集約の全情報を運ばず、`role_snapshot.rs` の `Whole<T>` もその理由でWireを使う。
外部exportの契約をCLI表示のJSONと暗黙に同じにする根拠は無い。

`src/infrastructure/runner_assignment.rs` の `SqliteQueue::runner_operation` はrunner認証・claim・leaseの一つのwrite入口。
その `Operation::Poll` は `claim_task_filtered` に `WorkerTrial::default()` を渡す。
`src/domain/runner_assignment.rs` の `Assignment` はrunとrunnerのleaseの世代を返す型である。
この経路はlocal supervisorのtrial設定の読み込みと同じではなく、名前にAssignmentがあっても比較実験のAssignmentの模型ではない。
新しい仕組みはこのpull経路も含めて対象・非対象を説明する必要がある。
`src/` の型定義には汎用の `Experiment` / `Variant` / `Invocation` は確認できず、このrunnerの `Assignment` を実験割当の実装済み根拠にしない。

### 事実：設定はTOMLと厳格な検査、読み直す時点がある

`src/infrastructure/run_env.rs` の `parse_config` / `Config` は既知の表とkeyを検査し、未知の名前は誤りにする。
新しい実験の表を足すと古いbinaryはその設定を読めなくなるので、schemaが互換でも設定は別に導入の順序を要する。
`dagq.toml` のtrial・roles・provider_fallbackのcommentもこの順序を明記する。
[Run environment](../design/supervisor-lifecycle/run-environment.md)は設定の読み元と反映の境界を持つ。
workerの試しは `claim.rs` のfill passで、役割の設定はjobを開く時点で読み直す。
「設定変更が全起動に同時に効く」とする単一snapshotの約束は無い。

基準commitには期限付きの `dagq.local.toml` の重ねる層もある。
`src/infrastructure/run_env.rs` の `inspect_local_config` と `src/domain/local_config.rs`、[Local config](../design/supervisor-lifecycle/local-config.md)が有効範囲と観測の記録を持つ。
新しい実験表がそのまま重ねられるとは仮定せず、許すkeyと固定する条件の境界を検討する必要がある。
projectの値とhostの値をruntimeの固定値へ写さない方針はADR-t2257-1決定1が持つ。

### 事実：migrationの互換と動いている旧runner

`build.rs` が `migrations/` を登録し、`src/infrastructure/schema.rs` の `declaration` / `floor_for` / `compatibility_violations` が互換の宣言と下限を扱う。
queueを開くだけではmigrationせず、明示のmigrate等のserviceの入口が適用する。
claim時のbinaryの写しで動き続けるrunnerがあるため、互換migrationでは古いwriterも成立する必要がある（ADR-0045・ADR-0067）。
[Migrationの規則](../development/migrations.md)はリリース済みSQLを不変とし、新しい変更は追加する。
Postgresは `migrations-postgres/base.sql` と名前で対応するSQLを持ち、SQLite側の版と同じ意味になる。
Postgresのmigrationも必要で、SQLite専用の列やtriggerを足すだけでは共通の契約を満たさない（ADR-t3986-1）。

`src/domain/write_rules.rs` と[Persistence](../design/persistence.md)は、eventの未知kindを読む寛容さと、閉じた値を拒む列を分ける。
kindを足すこと、既存の値の意味を変えること、古いreaderが読めない値を足すことは別の互換問題である。
属性の開いた組で群を運べても、設定の復元・制御に必要な割当の正本をそこだけで満たせるとは限らない。

### 入力としてのgoal 114と現行の位置

goal 114の詳細なqueue記録はこの調査では読んでいない。
ここで使う前提はtaskに添付されたgoalのdescriptionの要約であり、taskの現在の状態や着手履歴をこの文書には写さない。
共通契約の範囲は実験の宣言・決定的割当・版・最初の記録からの復元・同じ仕組みの競合・収集の終了と決着・数値の置き場。
この基準の `docs/adr/INDEX.md` にはtask 1692の決定は確認できず、同taskが既に契約を確定したものとして引用しない。
ADR-t3957-1決定7・ADR-t4005-1決定11は、2試しを一般A/Bの宣言の一つとして移せる形にする方針である。

重なるのは宣言・抽選・保持・記録・集計の入口であり、食い違うのは2試しの専用key・二群のhash・制限された候補・群のみの復元と、一般の版付きの契約である。
goal 114が扱う指示の節の上書き・switchbackは、actorごとの起動先や役割の組、Evidence本文の保持とexportをそのまま与えるものではない。
これらの追加契約を後続で設計し、共通契約を別の名前で作り直さない。

### 仮説・提案

**仮説**：新設定と新DBだけを同時に切り替える移行では、旧runnerのclaimやeventを読み替えて群の意味を失う恐れがある。
**提案**：読み込み対応 → 記録・復元 → 起動への適用 → 旧設定の移行という境界を分ける候補を検討する。
旧群の意味を保持する移行と、新しい実験条件を作る操作を分け、設定を消したときも過去の参照が解釈できる契約を判断する。

### 再利用・拡張・作り直しの候補

- **再利用**：CLI→use case→domain→storeの層、認可、migration互換判定、両DBのstore contract。実験のために別の制御面を持たない。
- **拡張**：実験の設定検査、serviceの読み書きと版付きexport、旧writerを含む適合test。保存だけでなく入口全体の互換を守るため。
- **作り直し**：役割別に設定key・hash・KPIの専用軸を増やす方法。共通契約の上に起動の解決を載せ、追加軸のたびに全層を直す構造を避ける。
- **goal 114との接続**：共通契約の担当を保ち、2試しをその宣言に移す際の追加分だけを設計する。具体のkey・式・export・保持期間はこの調査で決定せず、後続の契約文書とADRに委ねる。
