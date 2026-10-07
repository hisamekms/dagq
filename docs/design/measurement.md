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
  - design-supervisor-lifecycle-first-commit
  - design-supervisor-lifecycle-timeline
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-report
  - design-supervisor-lifecycle-host-metrics
---

# 計測

> **まだ実装が無い（2026-10-04）**: この文書は計測の作り直し（goal 71、request 13）の今の予定で、`src/`にはまだ何も入っていない。今動いている計測は[`stats`](supervisor-lifecycle/stats.md)（着地待ちの内訳・作業の内訳）・[最初のcommitの観測](supervisor-lifecycle/first-commit.md)・[`timeline`](supervisor-lifecycle/timeline.md)・[`kpi`](supervisor-lifecycle/kpi.md)・[レポート](supervisor-lifecycle/report.md)・[hostの負荷](supervisor-lifecycle/host-metrics.md)が持ち、この文書はそれらを変えない。後続のtaskが実装したら、この注記と各節を今の姿に直す。

決めた理由はADR-t1662-1（計測のモデル）・ADR-t1662-2（ストアの抽象化）・ADR-t1662-3（実行する側と送る口とコマンドの分類）が持つ。ここはeventの欄・列とJSONの形・数値・関数の名前の予定を持つ。

## SSOTとビュー

計測が扱う記録の論理ストアと材料の区分（ADR-t1662-2決定1・2）。SSOTは消えてはいけない正本、ビューはSSOTから作り直せるもの、材料は消えてよく台帳と統計が読まないもの。計測の層（台帳と統計）はEventStore・SessionStepStore・NodeSampleStoreだけを読み、StateStoreを読まない（D1）。

| 論理ストア | 中身 | 区分 | 今のアダプタ |
| --- | --- | --- | --- |
| StateStore | 今の状態（task・goal・run・ask・proposal・planner・lease・supervisorなど） | SSOT | queue.dbの状態の表（`tasks`・`goals`・`task_runs`・`asks`・`proposals`・`planners`・`run_leases`・`supervisors`ほか） |
| EventStore | 起きたことのappend-onlyの記録（runの遷移・`run_phase_changed`・claimの前の区間・queueの出来事・Executionのtoken） | SSOT | queue.dbの`run_events`（runの無いqueueのeventを含む）。EventStoreとStateStoreは同じtransactionで確定する |
| SessionStepStore | sessionのstep（turn・tool・コマンドのshape）。`(session_id, seq)`で重複を除き、抜けを残す。90日 | SSOT | 未実装（queue.dbの新しい表の予定） |
| NodeSampleStore | nodeの資源の連続の値（load average・CPU・メモリ・swap・pageout・ファイルシステムの空き） | SSOT | 未実装（queue.dbの新しい表の予定）。今はsupervisorが`host/metrics-YYYYMMDD.csv`に書くだけ |
| LedgerStore | 台帳（run・task・session・queue・nodeの行）。旧方式の行のlegacyのJSONは凍結して捨てない | ビュー | 未実装（queue.dbの新しい表の予定） |
| ReportStore | 統計の出力（日次・週次のレポート） | ビュー | queueのdirの`reports/`のファイルと`report_written`のevent |
| follow-up所属判断の基準値 | 登録cohortの時間・verdict・reviseとplanner sessionの集計、event ID付きCSV | ビュー | `scripts/follow-up-membership.py`がCLIのeventsとgoalのsnapshotから作る`docs/plans/follow-up-membership-evidence/`。現状の旧データのgoal残件はStateStoreのCLI snapshotをeventで巻き戻す暫定の分析（台帳の読み取り経路には入れない）。入力snapshotと`stats`・`kpi`・暫定tokenの出力はrun dirの`membership-evidence/`に置く採取材料。定義と再計算は[評価](../plans/follow-up-membership.md) |
| （材料） | run dir（receipt・prompt・`worktime.jsonl`ほか）・log・hostのCSV・Claude Codeのtranscript・Codexのrollout | 材料 | queueのdirとrun dirのファイル、`~/.claude`。台帳と統計は読まない（取り込んだ値はEventStoreかSessionStepStoreに入る） |
| 文書候補探索の比較出力 | `docs/plans/docs-candidate-search/out/` の run・層の CSV、選択 event の ID、ページ取得記録・定義/並行変更の一覧 | ビュー | `docs/plans/docs-candidate-search/` の Python script。既存 acceptance-check fetch を介して queue service の読み取り CLI（events/show/stats 等）と git を読み、初回 review 前の validation receipt を既存 compute と共有して集計。原文 snapshot は一時的な材料として TMPDIR のみ（台帳は読まない） |
| 統合テストのcoverageの対応表 | 「repositoryのファイル → 当たった統合テスト」の対応表（ファイルの粒度。統合テストのbinaryのテストだけで、unit testとe2eは除く）・テストごとの所要時間と結果（nextestのJUnit）・作ったcommitと時刻 | ビュー | CIの夜間のjob（`.github/workflows/it-coverage-map.yml`。毎日・`workflow_dispatch`・jobかscriptを変えるmainへのpush）が統合テストをテストごとのcoverageつきで流して作り直せる。GitHub Actionsのartifact `it-coverage-map`（`it-coverage-map.json`の1ファイル）、保持30日で、過ぎて消えてもよい。作るのは`scripts/it-coverage-map.sh`（JSONの形・テストの名前・置き場はscriptの冒頭のcomment） |
| 着地のITの絞り込みの測定 | 着地ごとの絞ったIT（本数・直列の和・見込みの壁時計・全部流したかと理由）・CIで新たに赤くなったテストと範囲の着地と見逃し・前N日の足されたテストと変わったsrcのファイル（古さの材料）のCSV | ビュー | queueのevent・git・CIの履歴・artifactから`docs/plans/landing-it-selection/`のscript（`collect.sh`が読むCLIは`dagq events`・`git`・`gh`、`analyze.py`が集計）で作り直せる。CSVは`docs/plans/landing-it-selection/`に置く。対応表のartifactの保持（30日）が切れると同じ表では作り直せない。定義は[測定](../plans/landing-it-selection.md) |
| goal 119の前後の関門のlogの比較 | 本番のintegrateのcoverageの関門のlogの区間ごとの選んだlog（相対path・mtime・並列数・Summaryとtestの時間の秒・load1）と除いたlogと理由のCSV | ビュー | `docs/plans/it-reduction/measure_goal119.py`がrun dirの関門のlog（材料）・hostのload1のCSV（材料）・gitの履歴から作り直せる（queueは開かない）。CSVは`docs/plans/it-reduction/goal-119/`。関門のlogとhostのCSVが消えた期間は作り直せない。定義は[測定](../plans/it-reduction.md)の「goal 119の前後」 |
| docsの4指標とM5 | docs/designの総量と伸び・docs/designを変えた着地の割合・docsの衝突とclaimの控え・道具の結果に占めるdocs・docsだけの衝突の種類の週ごとの値と基準値 | ビュー | `scripts/docs-metrics.py`が入力から作り直せる。入力はmainのgitの履歴（SSOT。git）・`dagq stats --since`のJSONと衝突のeventの`dagq events --full`のJSON（ビュー。EventStoreから作り直せる）・Claude Codeの会話記録（材料。`~/.claude/projects`、30日で消える）。出力は[基準値](../plans/docs-slim.md)と週次の見直しが`~/.local/share/dagq-hostmetrics/docs-slim/`に置くJSON。会話記録が消えた期間は作り直せない |

**新しいストアやビューを足すときはこの節に区分を書く。** 新しい表・ファイル・外の記録を計測が読む・書くようにするtaskは、同じ変更でこの表に行（中身・区分・今のアダプタ）を足すか直す（ADR-t1662-2決定9、[文書の規則](../development/documents.md)の「design」）。

## 区間とタグ

runの一生を、重ならず隙間なく全体を覆う区間の列で表す（ADR-t1662-1決定1〜5）。

- **境目の記録**: supervisorが工程を移るたびに`run_phase_changed`を書く。payloadは`phase`（工程の名前。自由）・`blocker`・`holds`・`attempt`（1から）・`cause`（移ったきっかけのevent idか理由のコード）・`v`（payloadの版。1から）。
- **タグ**: `blocker`は`queue`（slotや着地の順番の空き待ち）・`ai`（agentが動いている）・`compute`（build・test・検証の計算）・`human`（人の答え）・`runtime`（dagq自身の処理と見張りの間隔）・`external`（GitHub・remote・providerのAPIなど外のサービス）・`infra`（実行環境の用意・故障・資源の不足。ADR-t1662-3決定5）。`holds`は`worker_slot`・`landing_slot`・`none`。
- **`Phase::tags()`**: 工程の定義の隣で網羅的な`match`がタグを返し、工程を足してタグを書き忘れるとコンパイルが止まる。
- **unattributed**: 区間の列に当たらない時間（記録の抜け）はunattributedの区間として出す。KPIは所要時間に対する割合を見張り、2%以上を目標割れにする。
- **物差し**: 所要時間（区間の和）と、slotを握った時間（`holds`が`none`でない区間の和）。

工程とタグの組の例（段1のtaskが決める予定。名前は変わりうる）:

| 工程 | blocker | holds |
| --- | --- | --- |
| worker（sessionが作業している） | ai | worker_slot |
| validating | runtime | worker_slot |
| review | ai | worker_slot |
| 人の答え待ち（`approve_landing`・`worker_question`） | human | none |
| landing_queue（着地slotの順番待ち） | queue | none |
| verify（`integrate`の検証） | compute | landing_slot |
| push | external | none |
| push_pending（`push_failed`の後） | human | none |
| held（taskのholdで走っていた工程が終わり待ちに入った後。予定・未実装） | human | none |

### 終端

`run_integrated`の後は工程`push`（`run_integrated`と同じtransactionで記録）で、`push_finished`（`already_delivered`を含む）か`push_skipped`で閉じる。`push_failed`は`push_pending`（`blocker: human`）にし、同じremote・branchへの後の`push_finished`で閉じる。着地しないrunは失敗・取り消し・interruptedで閉じる。taskは`completed`（そのrunのpushの終わり）か`canceled`で閉じる（ADR-t1662-1決定16）。

### claimの前

taskの台帳は`task_created`から始まる。claimの前の待ちの理由はsupervisor単位の区間をtaskの区間に重ねて読む（ADR-t1662-1決定6）。

- 区間のevent: `slots_full_started` / `slots_full_ended`、`claim_held` / `claim_resumed`、`claim_deferred` / `deferral_ended`（queueのevent。台帳のqueueの行に畳む）。
- `run_claimed`に足す欄: `ready_at`（taskがclaimできるようになった時刻）・`candidate_rank`（claimの時の候補の中の順位、1から）・`candidates`（候補数）。
- 着地slotの待ちの区間には`blocked_by`（その間に着地slotを握っていたrunのid）を持つ。
- 予定・未実装: readyのtaskのhold（[taskのhold](supervisor-lifecycle/task-hold.md)）は、`task_held`から`task_released`までをtaskの区間に重ねて読む。
  runを持つtaskでは、holdをかけた時刻ではなく、走っていた工程が終わって待ちに入った時刻（工程`held`）から解除までを分析から除く。

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

- キーは`^[a-z][a-z0-9_.]{0,63}$`、値は文字列で256 byteまで、1つのrunに64キーまで（予定の値）。
- claimの属性のキー: `build`・`claude_version`・`codex`・`rustc_release`・`rustc_host`・`parallel`・`slots`・`load_avg`・`requested_provider`・`claim_provider`・`route`・`claim_model`・`effort`・`trial_group`。
- 予約名（派生の軸。属性のキーにしない）: `provider`・`claude`・`model`・`group`・`slot`・`load`・`toolchain`・`change`・`area`・`nature`。kpiが今の`axis_value`と同じ規則で作る。
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

`cargo test --locked --test it`は3つ目の規則で`full_test`、`cargo test --locked --test plugin`は`plugin`が`["it"]`に無いので3つ目と4つ目（`--test`が在る）に一致せず5つ目で`test`になる。`positional`が除くのは規則の`sub`が一致した語だけなので、`cargo test runtime`（`runtime`はサブコマンドの位置に平文で残る）は3つ目と4つ目に一致せず`test`になる。台帳は部分ごとの分類も持ち、`integrate`と重なる検証の数（`full_tests`・`llvm_cov_runs`）は部分の分類から数える（`cargo llvm-cov … && cargo test --locked`は両方に数える）。今の`domain::worktime`と分け方が違う形: `--test`に別々の値を並べたもの（`--test e2e --test it`は今はe2e、この規則では`flag_values`の「全ての値」に当たらず`test`）と、`nextest run --test it`の`full_test`（同じ形の規則を足せば表せる）。差は段2の並走比較で説明する。落ちたtestの名前とpathは設定で有効にし、test runnerの読み方のアダプタを選ぶ。雛形を配り、`dagq init`が提案する。

## 段と後続のtask

段0（ADR 3本とこの文書、task 1662）→段1（`run_phase_changed`・claimの前の区間・`run_claimed`の欄・`blocked_by`の記録）→段2（ストアのport・台帳と畳む関数・1週間の並走比較。差は`docs/plans/`に説明する）→段3（読み手を台帳と畳む関数へ移し`LandClock`を撤去）。sessionのstep・TelemetrySink・資源・NodeSampleStore・`environment_attached`は別のgoal（goal B）が持つ。
