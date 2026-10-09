---
id: design-measurement
type: design
title: 計測（SSOTとビュー・区間とタグ・台帳の形・畳む関数・台帳を作る係・送る口・コマンドの形と分類）
status: draft
created: 2026-10-04
scope: runtime
related:
  - design-supervisor-lifecycle-task-hold
  - adr-t1662-1
  - adr-t1662-2
  - adr-t1662-3
  - adr-0049
  - adr-0048
  - adr-0051
  - adr-t1486-1
  - adr-t614-1
  - adr-t1233-4
  - design-architecture
  - design-persistence
  - design-queue-service
  - design-supervisor-lifecycle-stats
  - design-main-history
  - design-supervisor-lifecycle-first-commit
  - design-supervisor-lifecycle-timeline
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-report
  - design-supervisor-lifecycle-host-metrics
---

# 計測

> **一部だけ実装済み（2026-10-09）**: 「区間とタグ」のrunの工程の記録（`run_phase_changed`）・「終端」・「claimの前」・「supervisorの一生」・「今の判定の記録」と「runの比較の軸」の記録は実装済みで、今の姿を書く。
> ほかの節は計測の作り直しの今の予定で、まだ`src/`に無い。
> 今動いている計測は[`stats`](supervisor-lifecycle/stats.md)（着地待ちの内訳・作業の内訳）・[最初のcommitの観測](supervisor-lifecycle/first-commit.md)・[`timeline`](supervisor-lifecycle/timeline.md)・[`kpi`](supervisor-lifecycle/kpi.md)・[レポート](supervisor-lifecycle/report.md)・[hostの負荷](supervisor-lifecycle/host-metrics.md)が持ち、この文書はそれらを変えない。
> 後続のtaskが実装したら、この注記と各節を今の姿に直す。
> 「SSOTとビュー」の表に足すものの範囲は[ADR-t2065-1](../adr/2026-10-08-t2065-1-measurement-ssot-table-holds-stores-and-shared-records-only.md)が持つ。

決めた理由はADR-t1662-1（計測のモデル）・ADR-t1662-2（ストアの抽象化）・ADR-t1662-3（実行する側と送る口とコマンドの分類）が持つ。ここはeventの欄・列とJSONの形・数値・関数の名前の予定を持つ。

## SSOTとビュー

計測が扱う記録の論理ストアと材料の区分（ADR-t1662-2決定1・2）。SSOTは消えてはいけない正本、ビューはSSOTから作り直せるもの、材料は消えてよく台帳と統計が読まないもの。計測の層（台帳と統計）はEventStore・SessionStepStore・NodeSampleStoreだけを読み、StateStoreを読まない（D1）。

| 論理ストア | 中身 | 区分 | 今のアダプタ |
| --- | --- | --- | --- |
| StateStore | 今の状態（task・goal・run・ask・proposal・planner・lease・supervisorなど） | SSOT | queue.dbの状態の表（`tasks`・`goals`・`task_runs`・`asks`・`proposals`・`planners`・`run_leases`・`supervisors`ほか） |
| EventStore | 起きたことのappend-onlyの記録（runの遷移・`run_phase_changed`・claimの前の区間・今の判定の記録・queueの出来事・mainの履歴・Executionのtoken） | SSOT | queue.dbの`run_events`（runの無いqueueのeventを含む）。EventStoreとStateStoreは同じtransactionで確定する |
| SessionStepStore | sessionのstep（turn・tool・コマンドのshape）。`(session_id, seq)`で重複を除き、抜けを残す。90日 | SSOT | 未実装（queue.dbの新しい表の予定） |
| NodeSampleStore | nodeの資源の連続の値（load average・CPU・メモリ・swap・pageout・ファイルシステムの空き） | SSOT | 未実装（queue.dbの新しい表の予定）。今はsupervisorが`host/metrics-YYYYMMDD.csv`に書くだけ |
| LedgerStore | 台帳（run・task・session・queue・nodeの行）。旧方式の行のlegacyのJSONは凍結して捨てない | ビュー | 未実装（queue.dbの新しい表の予定） |
| ReportStore | 統計の出力（日次・週次のレポート） | ビュー | queueのdirの`reports/`のファイルと`report_written`のevent |
| （材料） | run dir（receipt・prompt・`worktime.jsonl`ほか）・log・hostのCSV・Claude Codeのtranscript・Codexのrollout | 材料 | queueのdirとrun dirのファイル、`~/.claude`。台帳と統計は読まない（取り込んだ値はEventStoreかSessionStepStoreに入る） |
| 統合テストのcoverageの対応表 | 「repositoryのファイル → 当たった統合テスト」の対応表（ファイルの粒度。統合テストのbinaryのテストだけで、unit testとe2eは除く）・テストごとの所要時間と結果（nextestのJUnit）・作ったcommitと時刻 | ビュー | CIの夜間のjob（`.github/workflows/it-coverage-map.yml`）が統合テストをテストごとのcoverageつきで流して作り直せる。GitHub Actionsのartifact `it-coverage-map`（`it-coverage-map.json`の1ファイル）、保持30日で、過ぎて消えてもよい。作るのは`scripts/it-coverage-map.sh`。着地の検証は下の「着地の検証の対応表のcache」から読む |
| agentのevalの記録 | evalの依頼・周・実行と費用・成績（[Agent eval](agent-eval.md)） | SSOT | `run_events`の`agent_eval_*`で、周の状態と成績はこれだけから作り直す（`domain::agent_eval::record`）。queueのdirの`agent-evals/`は材料 |
| 着地の検証の対応表のcache | 着地の検証が取った対応表の最新の1つと、取ったCIのrun・そのcommit・runの時刻・取った時刻 | 材料 | `scripts/landing-it.sh`がGitHub Actionsのartifact `it-coverage-map`から取り、新しいrunが無ければ取り直さない。置き場はqueueの外の`${XDG_CACHE_HOME:-$HOME/.cache}/dagq/landing-it/<run id>/`で、消えてもartifactから取り直せる。台帳と統計は読まない |

計測の層の論理ストアか、runtime・CI・`scripts/`が続けて書き他の仕組みが読む共有の記録を足すか変えるtaskは、同じ変更でこの表に行（中身・区分・今のアダプタ）を足すか直す（[ADR-t2065-1](../adr/2026-10-08-t2065-1-measurement-ssot-table-holds-stores-and-shared-records-only.md)、[文書の規則](../development/documents.md)の「design」）。
`docs/plans/`の1回きりや週次の見直しの測定の出力（scriptとCSV）はこの表に足さず、そのplansの文書が区分（SSOT・ビュー・材料）と作り直せる範囲を持つ。

## 区間とタグ

runの一生を、重ならず隙間なく全体を覆う区間の列で表す（ADR-t1662-1決定1〜5）。

- **境目の記録**（実装済み）: supervisorが工程を移るたびに`run_phase_changed`を書き、次の記録が前の工程を閉じる。
  payloadは`phase`（工程の名前で、自由）・`blocker`・`holds`・`attempt`・`cause`（移ったきっかけのeventのkindか理由のコード）・`v`（記録の規則の版で、1から）。
  `attempt`は`{kind, n}`で、`kind`は`first`・`revise`・`conflict`・`resume`、`n`はその種類の何回目か（1から）。
  後の工程は、次のrevise・resumeまで前のattemptのまま。
- **書く所**: supervisorの中の移りは、実行と着地の工程ごとの状態のmoduleの移す操作だけを通る。
  slotのrunはslotの工程・slotの外の待ち・着地の順番待ちから、slotを離れたrunはstatusから工程を読む。
  前と同じ工程とattemptは書かず、プロセスが初めて見るrunは記録を読み直してattemptを引き継ぐ。
  記録の失敗はwarnだけで、runの工程の判断は待たない。
  人の`integrate`の着地は`Integrator`が書く: integration slotを取った所で`landing`を、mainが動く前にslotを返した所（検査の拒否・エラー・`needs_session`・hold・`failed`）でrunが戻ったstatusの工程を書く。
  supervisorの着地はslotが書く。
  storeは`run_integrated`と同じtransactionで`push`を、`recover`と放置のrunの回収は`run_recovered`と同じtransactionでstatusの工程（`interrupted`は`ended`、やめた着地は`landing_queue`）を書く。
- **タグ**: `blocker`は`queue`（slotや着地の順番の空き待ち）・`ai`（agentが動いている）・`compute`（build・test・検証の計算）・`human`（人の答え）・`runtime`（dagq自身の処理と見張りの間隔）・`external`（GitHub・remote・providerのAPIなど外のサービス）・`infra`（実行環境の用意・故障・資源の不足。ADR-t1662-3決定5）。`holds`は`worker_slot`・`landing_slot`・`none`で、slotの数え方（着地の順番だけを待つrunと人の答えを待つrunはslotの外）に従う。
- **`Phase::tags()`**: 記録の工程の定義（`domain::run_phase`）の隣で網羅的な`match`がタグを返す。
  supervisorの工程も網羅的な`match`で記録の工程に対応し、どちらも工程を足して書き忘れるとコンパイルが止まる。
- **unattributed**（予定）: 区間の列に当たらない時間（記録の抜け）はunattributedの区間として出す。KPIは所要時間に対する割合を見張り、2%以上を目標割れにする。
- **物差し**: 所要時間（区間の和）と、slotを握った時間（`holds`が`none`でない区間の和）。

工程とタグ:

| 工程 | 中身 | blocker | holds |
| --- | --- | --- | --- |
| `provisioning` | claimの後、worktreeとsessionの用意 | infra | worker_slot |
| `worker` | workerのsession | ai | worker_slot |
| `validating` | receiptの検証 | runtime | worker_slot |
| `review` | reviewのjob | ai | worker_slot |
| `review_held` | reviewがproviderの控えの終わりを待つ | external | worker_slot |
| `revise` | 生きているsessionの直し（`revise`・衝突） | ai | worker_slot |
| `exiting` | sessionの終わりとworkspaceの片付け | runtime | worker_slot |
| `resume` | resumeしたsession | ai | worker_slot |
| `recovery` | `failed`・`interrupted`のrunの復旧job | ai | worker_slot |
| `waiting` | slotの外で人の答えを待つ（`worker_question`など） | human | none |
| `returning` | 待ちが終わり、slotに戻るのを待つ | queue | none |
| `awaiting_slot` | slotの中で着地の判断を待つ（e2e・着地の保留・着地slot） | queue | worker_slot |
| `awaiting_e2e` | 他のrunのe2eかやり直しを待つ | queue | worker_slot |
| `e2e` | 着地の前のe2e | compute | worker_slot |
| `landing_queue` | 着地の順番だけを待つ、または`land`の答えで承認されて着地を待つ | queue | none |
| `landing` | 着地（rebase・検証・mainの移動） | compute | landing_slot |
| `landing_answer` | `approve_landing`の答えか人の着地を待つ | human | none |
| `needs_session` | resumeを待つ | queue | none |
| `push` | `run_integrated`の後のpush | external | supervisorのslotが持つ間は`worker_slot`、人の`integrate`は`none` |
| `push_pending` | `push_failed`の後 | human | none |
| `ended` | 終わり（`cause`が`pushed`・`push_skipped`・`failed`・`canceled`・`run_recovered`など） | runtime | none |

予定・未実装: taskのholdで走っていた工程が終わり待ちに入った後の工程`held`（human・none）。

### 終端

`run_integrated`の後は工程`push`（`run_integrated`と同じtransactionで記録）で、pushの結果の記録と同じ所で、`push_finished`（`already_delivered`を含む）か`push_skipped`なら`ended`（`cause`は`pushed`・`push_skipped`）を、`push_failed`なら`push_pending`（`blocker: human`）を記録する。
人の`integrate`も同じ`Integrator`を通るので同じ記録になる。
`push_pending`を同じremote・branchへの後の`push_finished`で閉じるのは畳む関数（予定）で、人が手で打った`git push`はeventが無いので次の`push_finished`まで`push_pending`のまま。
着地しないrunは`ended`（`cause`は失敗のstatus・`canceled`・`run_recovered`など）で閉じ、`failed`のrunを復旧jobが動かせば次の工程に移る。
taskは`completed`（そのrunのpushの終わり）か`canceled`で閉じる（ADR-t1662-1決定16）。

### claimの前

taskの台帳は`task_created`から始まる。
claimの前の待ちの理由はsupervisor単位の区間をtaskの区間に重ねて読む（ADR-t1662-1決定6）。

- **区間の行**: `domain::pre_claim::pre_claim_intervals`がeventの列と今の時刻だけから`PreClaimInterval`の行を作り、StateStoreを読まない。
  どの区間も始まりを書いた1つのsupervisorに属し、終わりは下の規則で1通りに決まる。
- **`slots_full`**: supervisorのfill passは、始めと終わりに自分のslotを見て、classの枠が全部埋まれば`slots_full_started`を、空けば`slots_full_ended`を書く（`domain::pre_claim::slots_full_payload`）。
  classは`heavy`と`light`で（`SlotClass`）、classごとに別の区間になる。
  slotはsupervisorごとなので他のsupervisorは書きも閉じもせず、別のsupervisorの`supervisor_started`でも閉じない。
  execの引き継ぎは同じtokenの続きで、開いた区間を自分の記録から読み直して続ける。
  終わりを書かずに止まったsupervisorの区間は、下の「supervisorの一生」の終わりで閉じる。
- **`claim_hold`**: 既存の`claim_held` / `claim_resumed`（queue共通の控え）が区間の始まりと終わりを兼ねる。
  終わりは`stats`の`claim_holds`と同じ規則（`domain::claim_hold::hold_spans`）で、どのsupervisorが書いた終わりでも閉じ、書いたsupervisorの停止でも閉じる。
  控えはqueueの全てのsupervisorのclaimを止めるので、taskに重ねるときは区間のsupervisorに依らずqueueの待ちとして読む。
- **`claim_deferral`**: 既存の`claim_deferred` / `claim_deferral_ended`（taskの控え）が区間の始まりと終わりを兼ねる。
  終わりは`stats`の`claim_deferrals`の`by_end`と同じ規則（`domain::claim_defer::deferral_spans`）。
- **`run_claimed`の欄**: claimはtaskがreadyになった時刻と、claimの判断で並べた候補の中の位置と優先度を書く（`domain::claim_facts`）。
  supervisorは候補ごとの値をclaimに渡し、claimは取ったtaskの値だけを残す。
- **`blocked_by`**: 着地の順番だけを待つ間の`run_phase_changed`（工程`landing_queue`）は、そのとき着地slotを握っていたrunを`blocked_by`に持ち、握るrunが変われば記録し直す。
- 予定・未実装: readyのtaskのhold（[taskのhold](supervisor-lifecycle/task-hold.md)）は、`task_held`から`task_released`までをtaskの区間に重ねて読む。
  runを持つtaskでは、holdをかけた時刻ではなく、走っていた工程が終わって待ちに入った時刻（工程`held`）から解除までを分析から除く。

### supervisorの一生

supervisorの生存と停止の証拠はEventStoreに残し、一生の終わりはeventだけから決める（`domain::supervisor_life::supervisor_life_end`）。

- **停止**: 登録の行が消える全ての経路（自分の停止と`up`・`down`の掃除）は、同じtransactionで同じtokenの`supervisor_stopped`を書く。
  記録に失敗すれば行は残り、次の掃除に任せる（[persistence](persistence.md#runtime-ownership)）。
- **生存**: heartbeatを書くsupervisorは、`SUPERVISOR_ALIVE_INTERVAL_SECS`ごとに`supervisor_alive`を1つ書く。
  毎回のheartbeatをeventにしないのはeventの量を抑えるためで、死んだ時刻の誤差は後の掃除の`last_heartbeat_at`で直る。
  記録の失敗はwarnにしてheartbeatを止めない。
- **終わりの規則**: そのtokenの`supervisor_stopped`があればその時刻（`last_heartbeat_at`があればそれとの早い方）で終わる（`stopped`）。
  無ければそのtokenを`supervisor`に持つ最後のeventを最後の証拠とし、今がそれより`SUPERVISOR_ALIVE_INTERVAL_SECS` + `HEARTBEAT_TIMEOUT_SECS`より後ならその時刻で終わる（`silent`）。
  後で掃除の`supervisor_stopped`が届けば`last_heartbeat_at`で畳み直す。
  どちらでもなければ生きている。
- 停止のeventの無いstaleな登録は`silent`で閉じて掃除の後に直り、`supervisor_alive`の無い前のbuildのtokenも同じ規則で最後のeventで閉じる。
- kpiの`supervisor_lives`は今もsupervisorsの表を読み、この規則へ移すのは段2。

### 今の判定の記録

statsのslotの`alerts`・`running_alerts`・`workspace_check`は状態表・run dirのmarker・processを入力に今を判定するので、supervisorの周回がその入力を観測してEventStoreに記録し、統計はeventだけから同じ値を作れる（`domain::live_alerts`、`application::supervise::live_alerts`）。

- **観測**: どのsupervisorも`OBSERVE_INTERVAL_SECS`（60秒）ごとに、自分のslotやleaseに限らずqueue全体をstatsと同じ読み方で読み、statsと同じ判定の関数（`live_alerts::judge`）で判定する。
  警告の鍵は判定・種類・run・理由で、leaseの持ち主を含めないので、leaseが移っても何も書かない。
- **判定の入力と警告の開閉**: 入力はslot（空き・候補・ready）・閾値とその出どころ・未完了のrunごとの離散の時刻と値（警告でないrunも）で、経過で伸びる長さは持たない。
  開閉のeventは警告がいつ立ち消えたかの記録で、値の再現には使わない。
- **記録**（流れはsupervisorのtokenごと）: 起動の後の最初の成功した観測が`live_alert_baseline`（全ての入力と開いた警告で、入力の版1）。
  入力の値が変わった対象ごとに`live_alert_input_changed`（その対象の今の値の全体で、版を1つ進める）。
  警告の鍵の集合が変わったときだけ`live_alert_started`・`live_alert_ended`。
  `REACH_INTERVAL_SECS`（300秒）ごとと、観測の失敗の始まり・成功への戻りに`live_alert_reached`（成否・失敗の理由・その時の入力の版）。
  書けなかった記録の後は基準からやり直す。
- **畳み**: `live_alerts::fold`はeventと窓の終わりだけから、窓の終わりまでの最後に成功した観測が`GRACE_SECS`（900秒、到達点の3回分）以内の流れのうち最も新しい1つ（同じ時刻はevent idの大きい方）を選ぶ。
  その流れの基準と入力の更新を版の順に当てた入力を、窓の終わりで`judge`に渡す。
  流れどうしは足さない。
- **観測が無い**: 選べる流れが無いときの理由は、基準が無い・記録の欠け（到達点が確かめた版に入力の記録が届かない）・supervisorの停止（全ての流れの`supervisor_life_end`）・観測の失敗・到達点が古い。
  `supervisor_alive`は理由の区別にだけ使う。
- runtimeの制御はこの記録を読まず、statsはまだ今の観測を直接読む。

## 台帳の形

台帳は対象ごとに1行（ADR-t1662-1決定10）。区切りの値は列、フローで形が変わるものはJSON。全ての行が共通に持つ列:

| 列 | 中身 |
| --- | --- |
| `kind` | `run`・`task`・`session`・`queue`・`node` |
| `key` | 対象のid（runはrun id、taskはtask id、sessionは対象の種類とid、queueは時間の区切り、nodeはnodeのidと時間の区切り） |
| `ledger_v` | 畳み方の版。上がったら作り直す（旧方式の行を除く） |
| `inputs_through` | 畳んだ入力の到達点（EventStoreのevent idと、stepとnodeのsampleの受けた順のid） |
| `final` | 確定の印（下の「台帳を作る係」） |
| `legacy` | 旧方式の行だけ。旧方式の印と、今のstatsのrunごとの値（`work`・`validate`・`wait_to_land`・`land_phases`・`startup`）、属性の組とclaimの後の事実のJSON。一度だけ書いて凍結する |

種類ごとの列とJSON（予定）:

- **run**: 列は`task_id`・`claimed_at`・`first_commit_at`・`receipt_at`・`integrated_at`・`ended_at`（pushの終わり）・`outcome`・`duration_secs`・`slot_secs`・`unattributed_secs`、タグごとの合計（`blocker_<値>_secs`・`holds_<値>_secs`）、Executionのtokenの列（入力・出力・cacheの読み書き。集約はtask 1494）。JSONは`intervals`（区間の列: `phase`・`blocker`・`holds`・`attempt`・`start`・`end`・`cause`・`blocked_by`・`resources`）・`steps`（stepの要約）・`sessions`（runの上のsessionとjobの項目）・`attributes`（claimの属性）・`after_claim`（claimの後の事実）・`change`・`area`・`nature`。
- **task**: 列は`created_at`・`ready_at`・`first_claimed_at`・`ended_at`・`outcome`（`completed`・`canceled`）・`runs`・`duration_secs`。JSONは`intervals`（claimの前の待ちとrunごとの区間）と重ねたqueueの区間の要約。
- **session**: runを持たない対象（runtimeのplanner・plan review・goal review・throughput review・observerのjob・inboxと人の対話の区切り）のsessionとjobの1回。列は`target_kind`・`target_id`・`started_at`・`ended_at`・`duration_secs`と、runの行と同じExecutionのtokenの列（対話のsessionは区切りごとの行）。JSONは`steps`の要約と`sessions`の項目。
- **queue**: run・task・sessionに属さないqueue全体の区間と出来事（`slots_full`・`claim_held`・`claim_deferred`、着地slotの使用、`update_installed`・providerの切り替えなど）を時間の区切りごとに。
- **node**: nodeの資源の時系列を時間の区切りで畳んだもの（load averageの平均と最大・CPU・メモリ・swap・ファイルシステムの空き）。今のstatsの`host`、kpiの`host`・`cpu_per_landing`・`load_per_core`・`health.disk`、reportのhostの値の置き場（task 1685・1686）。

## 畳む関数

区間の列を作る関数は1つで（ADR-t1662-1決定2）、台帳を作る係と、走っているrunを見る読み手（`status`・`timeline`・`forecast`）が共有する。入力はEventStoreのeventの列（とstep・nodeのsample）で、出力は区間の列・タグごとの合計・unattributed・claimの後の事実。同じ入力からは同じ出力を返す。claimの後の事実（最後のprovider・Codexで作業したか・最初のCodexの`turn_finished`のmodel）は、`run_claimed`の`provider`・`provider_switched`の`to`・`turn_finished`の`provider`と`model`から、今の`domain::stats::measures::MeasureTrack`と同じ規則で作る（task 1666。既存の出力との一致をunit testの条件にする）。

置き換える今の組み立て（段3で撤去）: `domain::stats::landing::LandClock`、`application::supervise`の`Phase`、`domain::run_progress::Phase`、`domain::forecast::Phase`、timelineの空白の理由。

## 台帳を作る係

- supervisorの周回が終わった後に動き（D7）、行が無い・`ledger_v`が古い・`inputs_through`より後に入力が届いた（dirty）対象を畳み直す。冪等。
- 置き場は観測と分析のcontext（[Architecture](architecture.md)の「観測と分析」、goal 100のtask 1554・1552・1556のmodule）。`Supervisor`の本体に状態を足さない。
- `final`は、runが終わり（`push_pending`でない）、そのrunのtelemetry専用のtokenが失効し、supervisorの資源の最後の記録が済み、猶予が過ぎてから付ける。`final`の後もdirtyなら畳み直す。
- 切り替え前のrun（`run_phase_changed`が無い）は区間に畳まず、`legacy`に今の`RunMeasures`から値を一度だけ書く。
- 台帳はruntimeの制御（claim・slot・resume・着地）に使わない。

## 共通の読み取り関数

stats・kpi・reportの全ての欄（hostの欄と、run・taskを持たない`jobs`・`sessions`などの集計を含む）は、5種類の行を今のstatsの窓と絞り込み（`--since`・`--until`・`--full`・`--goal`）で選んで返す1つの関数だけから作る。新しい行は区間から、旧方式の行は`legacy`から、同じ名前と形の値を返す。

- 今の`work`・`wait_to_land`・`land_phases`・`startup`は、段3の後の1期間は台帳から同じ名前で作り、その後廃止する。`startup`は`first_commit`に改名する。1期間の長さは段3のtaskが決める。
- kpiの`phase.<工程>`と`land_phase.<工程>`も同じ1期間は同じ名前で作り、その後はタグごとの値と`phase.first_commit`に置き換える。hostの設定の`[kpi.targets."phase.startup"]`は`phase.first_commit`に読み替える（ADR-t1662-1が[ADR-0051](../adr/0051-kpi-time-series-report-and-push.md)決定1をamends）。
- 新しい出力は所要時間とslotを握った時間の2本で、`blocker`・`holds`・`attempt`ごとの内訳を出し、`change`と`area`で層に分ける。

## runの比較の軸

claimの属性は`run_claimed`の`attributes`（キーと値の開いた組）に持ち、記録・台帳・`kpi --by <キー>`と`--compare`はキーを知らずに運ぶ（ADR-t1662-1決定21〜26）。

- 記録（実装済み）: claimは`run_claimed`の他の欄を書いた後に、その欄から`domain::claim_facts::attributes`で組を作る。
  キーと元の欄の対応は同じmoduleの表`KEYS`だけが持ち、新しいキーはそこに足す。
- キーは`^[a-z][a-z0-9_.]{0,63}$`、値は文字列で256 byteまで、1つのrunに64キーまで（予定の値）。
- 予約名（派生の軸。属性のキーにしない）: `provider`・`claude`・`model`・`group`・`slot`・`load`・`toolchain`・`change`・`area`・`nature`。kpiが今の`axis_value`と同じ規則で作る。
  組を作る関数は予約名のキーを入れない（`claim_facts::RESERVED`）。
- 読み取りの優先: 層のキーが予約名なら派生の値、それ以外は属性の値、無ければ`unknown`。

## 送る口

（ADR-t1662-3決定1〜13）

- **送る側**: executor（session wrapper・runner、非対話のjobを起動するプロセス、`integrate`とe2eの検証）がstepを30秒ごとにまとめてTelemetrySinkに送る。agentは送らない。supervisorは工程・claim・slot・ask・`environment_attached`・nodeの時系列・外から測れる資源を自分で書く。
- **TelemetrySink**: 今のアダプタはqueue serviceのユースケース`telemetry_report`（APIの版つき。[Queue service](queue-service.md)）。stepは`(session_id, seq)`と対象を持つ。送れなくても結果を変えない。
- **principal**: role `executor_telemetry`（予定の名前）。呼べるのは`telemetry_report`だけ。対象は1つで、runの対象は`run_id`、runを持たない対象は種類とid（`planner_id`・`plan_review_id`・jobのid）。supervisorがclaim・resume・jobの起動・plannerを開くときに発行し、runの終わり・leaseの喪失・jobの終わり・`planner_closed`で失効させる。tokenのファイルはagentのworktree・run dir・plannerのdirの外に置き、agentのenvとargvに渡さない。
- **所属**: runの対象のstepはrunの行に畳み、runを持たない対象のstepはSessionStepStoreに90日だけ持ってsessionの行で読む。
- **差し替える口**: 実行環境の口・資源の口・送る口。資源の共通の単位はCPU時間（秒）・メモリの最大（byte）・読み書きの量（byte）・壁時計の時間（ミリ秒）とnodeのid。3つの口のアダプタは同じ適合test一式を通す。
- **OpenTelemetry**: 区間はspan、stepは子のspan、タグと属性はattributeに写す。記録の経路にはしない。

## コマンドの形と分類

（ADR-t1662-3決定14〜19）

- `[telemetry] command_detail = "program" | "shape" | "redacted"`（既定`shape`）。
- shapeは部分（パイプ・`&&`・`||`・`;`で分けたもの）ごとに`lead`（先頭の予約語）・`assigns`（代入の名前）・`program`・`args`（`{flag: 名前, value?: digest}`・`{sub: 語}`・`{word: digest}`・`{plus: digest}`・`{after_dashdash: 数}`）。digestはSHA-256(`"dagq-cmd-v1\0"` + 語)の先頭16桁のhex。
- 分類は台帳を作るときに`[[telemetry.commands]]`を上から当てる。今のworktimeの分類（`domain::worktime`）をこのrepositoryの`dagq.toml`で表す例:

```toml
[[telemetry.commands]]
class = "e2e"
program = "cargo"
flag_values = { "--test" = ["e2e"] }
takes_value = ["--test"]

[[telemetry.commands]]
class = "llvm_cov"
program = "cargo"
sub = ["llvm-cov"]

# cargo test --locked --test it（filterなし）
[[telemetry.commands]]
class = "full_test"
group = "test"
program = "cargo"
sub = ["test"]
flag_values = { "--test" = ["it"] }
none_of = ["--lib", "--bin", "--bins", "--doc", "--example", "--package", "-p", "-E"]
takes_value = ["--test"]
positional = "none"

# cargo test --locked（targetもfilterもなし）
[[telemetry.commands]]
class = "full_test"
group = "test"
program = "cargo"
sub = ["test"]
none_of = ["--test", "--lib", "--bin", "--bins", "--doc", "--example", "--package", "-p", "-E"]
positional = "none"

# cargo test --locked --test plugin などは上に一致せずここで test
[[telemetry.commands]]
class = "test"
program = "cargo"
sub = ["test"]

# cargo nextest run（targetもfilterもなし）
[[telemetry.commands]]
class = "full_test"
group = "test"
program = "cargo"
sub = ["nextest", "run"]
none_of = ["--test", "--lib", "--bin", "--bins", "--doc", "--example", "--package", "-p", "-E"]
positional = "none"

[[telemetry.commands]]
class = "test"
program = "cargo"
sub = ["nextest"]
```

`cargo test --locked --test it`は3つ目の規則で`full_test`、`cargo test --locked --test plugin`は`plugin`が`["it"]`に無いので3つ目と4つ目（`--test`が在る）に一致せず5つ目で`test`になる。
`positional`が除くのは規則の`sub`が一致した語だけなので、`cargo test runtime`（`runtime`はサブコマンドの位置に平文で残る）は3つ目と4つ目に一致せず`test`になる。
台帳は部分ごとの分類も持ち、`integrate`と重なる検証の数（`full_tests`・`llvm_cov_runs`）は部分の分類から数える（`cargo llvm-cov … && cargo test --locked`は両方に数える）。
今の`domain::worktime`と分け方が違う形: `--test`に別々の値を並べたもの（`--test e2e --test it`は今はe2e、この規則では`flag_values`の「全ての値」に当たらず`test`）と、`nextest run --test it`の`full_test`（同じ形の規則を足せば表せる）。
差は段2の並走比較で説明する。
落ちたtestの名前とpathは設定で有効にし、test runnerの読み方のアダプタを選ぶ。
雛形を配り、`dagq init`が提案する。

## 段と後続のtask

段0（ADR 3本とこの文書、task 1662）→段1（`run_phase_changed`・claimの前の区間・`run_claimed`の欄・`blocked_by`の記録）→段2（ストアのport・台帳と畳む関数・1週間の並走比較。差は`docs/plans/`に説明する）→段3（読み手を台帳と畳む関数へ移し`LandClock`を撤去）。sessionのstep・TelemetrySink・資源・NodeSampleStore・`environment_attached`は別のgoal（goal B）が持つ。
