---
id: design-measurement
type: design
title: 計測（SSOTとビュー・区間とタグ・台帳の形・畳む関数・台帳を作る係・送る口・コマンドの形と分類）
status: draft
created: 2026-10-04
scope: runtime
related:
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
  - design-supervisor-lifecycle-commands
  - design-main-history
  - design-supervisor-lifecycle-host-metrics
  - design-measurement-commands
  - design-measurement-environment
  - design-measurement-otel
---

# 計測

> **一部だけ実装済み**: 「SSOTとビュー」のport・「区間とタグ」のrunの工程の記録（`run_phase_changed`）・「終端」・「claimの前」・「supervisorの一生」・「今の判定の記録」と「runの比較の軸」の記録、台帳と「畳む関数」・「台帳を作る係」・「共通の読み取り関数」・「並走の比較と照合」と「送る口」（中から測った資源は予定）・「コマンドの形と分類」は実装済みで今の姿を書く。
> ほかの節は計測の作り直しの今の予定で、まだ`src/`に無い。
> 計測のコマンドをまたぐ約束は[計測のコマンド](supervisor-lifecycle/commands.md)が持つ。

決めた理由はADR-t1662-1〜3が持つ。
ここは計測の地図で、計測の流れ（記録→台帳→読み手と送る口）、SSOTとビューの区分、区間とタグの考え方、知りたいことからコードへの入口、コードから読めない約束を持つ。
eventの欄・既定値・場面ごとに誰が何を書くかは、入口に挙げた型・関数・moduleのdoc commentが持つ。

## SSOTとビュー

計測が扱う記録の論理ストアと材料の区分（ADR-t1662-2決定1・2）。
SSOTは消えてはいけない正本、ビューはSSOTから作り直せるもの、材料は消えてよく台帳と統計が読まないもの。
計測の層（台帳と統計）はEventStore・SessionStepStore・NodeSampleStore・RunResourceStoreだけを読み、StateStoreを読まない（D1）。
論理ストアのportは`application::ports`の`shared`の`EventStore`と`StateStore`で、ほかは`observation`に置く。

| 論理ストア | 中身 | 区分 | 今のアダプタ |
| --- | --- | --- | --- |
| StateStore | 今の状態（task・goal・run・ask・proposal・planner・lease・supervisorなど） | SSOT | queue.dbの状態の表 |
| EventStore | 起きたことのappend-onlyの記録（runの遷移・`run_phase_changed`・claimの前の区間・今の判定の記録・queueの出来事・mainの履歴・[task/goal/findingの属性](attribute-records.md)・Executionのtoken） | SSOT | queue.dbの`run_events`（runの無いqueueのeventを含む）。EventStoreとStateStoreは同じtransactionで確定する |
| SessionStepStore | sessionのstep・中から測った資源と、対象と持ち主。`(session_id, seq)`で重複を除き、抜けを残す | SSOT | queue.dbの`session_steps`ほか |
| NodeSampleStore | nodeの資源の連続の値（load average・CPU・メモリ・swap・pageout・ディスクの空き・待ちと着地待ちのrunのビルド成果物の合計と件数・共通の単位） | SSOT | queue.dbの`node_samples`（hostのCSVは写し。[hostの負荷](supervisor-lifecycle/host-metrics.md)） |
| RunResourceStore | runの資源を外から測った窓 | SSOT | queue.dbの`run_resources` |
| LedgerStore | 台帳（run・task・session・queue・nodeの行）。旧方式の行のlegacyのJSONは凍結して捨てない | ビュー | queue.dbの`ledger_rows` |
| ReportStore | 統計の出力（日次・週次のレポート） | ビュー | queueのdirの`reports/`のファイルと`report_written`のevent |
| （材料） | run dir（receipt・prompt・`worktime.jsonl`ほか）・log・hostのCSV・Claude Codeのtranscript・Codexのrollout | 材料 | queueのdirとrun dirのファイル。Claude Codeのtranscriptはhostの`$CLAUDE_CONFIG_DIR`か`~/.claude`にあり、containerのrunのものは[Container runtime](container-runtime.md)。sessionの有無の判断と区間の計測が読み、非対話のworkerの区間のトークン数はtranscriptでなくturnのstreamから取る（取り込んだ値はEventStore・SessionStepStore・NodeSampleStoreに入る） |
| 統合テストのcoverageの対応表 | 「repositoryのファイル → 当たった統合テスト」の対応表（ファイルの粒度。統合テストのbinaryのテストだけで、unit testとe2eは除く）・テストごとの所要時間と結果（nextestのJUnit）・作ったcommitと時刻 | ビュー | CIの夜間のjob（`.github/workflows/it-coverage-map.yml`）が統合テストをテストごとのcoverageつきで流して作り直せる。GitHub Actionsのartifact `it-coverage-map`で、保持の期間を過ぎて消えてもよい。着地の検証は下の「着地の検証の対応表のcache」から読む |
| agentのevalの記録 | evalの依頼・周・実行と費用・成績、productionのケースの集合の依頼・準備・ラベル・組み立てとdigest・disputedの人の決定（[Agent eval](agent-eval.md)） | SSOT | `run_events`の`agent_eval_*`と`agent_cases_*`で、周と集合の状態はこれだけから作り直す。queueのdirの`agent-evals/`とラベルのjobの出力（`agent-cases/<set id>/labels/`）は材料 |
| productionの集合のファイル | 集合のケースの一覧とpatch | ビュー | queueのdirの`agent-cases/<set id>/`の`cases.json`と`patches/`。`agent_cases_*`とgitの`<base>...<head>`の差分から作り直し、commitが辿れないケースは理由つきで飛ばす |
| queueのbrokerの記録 | 動いているbrokerが起動ごとに書き、probeとrelayの見張りが読む自分の記録 | 材料 | `<queue dir>/broker/state.json`（寿命はbrokerの1回の起動で、起動し直せば作り直せる） |
| runのrelayの記録 | 開いたrunのrelay（containerを起動し直す材料）で、relayの見張りが読む | 材料 | `<queue dir>/relay/<run>.json`（寿命はrunのrelayを開いてから閉じるまで） |
| run branchのbundle | containerのrunのrun branch 1本 | 材料 | run dir。turnの終わりにcontainerが書き制御側が取り込む |
| runの状態のファイル | containerのrunのHEADとstatus・turnの終わりの印と世代（workerの申告） | 材料 | run dir。turnの始めと終わりにcontainerが書き、検証・復旧・再計画の保留が読む |
| cloneの作業と復元 | containerのrunの未commitの差分とuntracked（workerの申告）、復元するsnapshotの写しとその答え | 材料 | run dir。作業はturnの終わりにcontainerが書き再計画の保留が読む。写しは制御側が書きcontainerが次のturnの始めに当て、その答えをresumeの間に制御側が読む |
| mainのbundle | 今のmain 1本 | 材料 | run dir。制御側が書き、次のturnの始めにcontainerが取り込む |
| 手元の検証の記録 | workerの手元の検証の1回の実行ごとの記録とlog | 材料 | run dirの`local-checks/`（turnのenvが名指す）。pjのscriptが書き、runtimeがreceiptのcommitのtreeと照合してreviewの資料に載せる |
| machineのメモリの予約と待ち | dagq専用のmachineの予約（持ち主・種類・上限・取った時刻）と待ちの列（同じ欄で時刻は待ち始め）。全てのqueueのclaim・wrapper・`integrate`が読む | 材料 | data homeの`dagq/machine/`のlockの下の`reservations/`と`waiting/`。寿命は持ち主のturnかverificationの間で、失っても記録の無いcontainerを上限で数えて安全側で、計測の層は読まない。待った区間の正本はEventStoreのevent |
| 着地の検証の対応表のcache | 着地の検証が取った対応表の最新の1つと、取ったCIのrun・そのcommit・runの時刻・取った時刻 | 材料 | hostで`scripts/landing-it.sh`がartifact `it-coverage-map`から取ってqueueの外に置き、containerのverificationはrun dirの材料への写しだけを読む（置き場・取り直し・写し方はscriptの冒頭）。cacheも写しも消えてよい |

計測の層の論理ストアか、runtime・CI・`scripts/`が続けて書き他の仕組みが読む共有の記録を足すか変えるtaskは、同じ変更でこの表に行（中身・区分・今のアダプタ）を足すか直す（[ADR-t2065-1](../adr/2026-10-08-t2065-1-measurement-ssot-table-holds-stores-and-shared-records-only.md)、[文書の規則](../development/documents.md)の「design」）。

## 区間とタグ

runの一生を、重ならず隙間なく全体を覆う区間の列で表す（ADR-t1662-1決定1〜5）。
工程の境目は`run_phase_changed`で、次の記録が前の工程を閉じる。
統計は工程の名前でなくタグで数え、`blocker`は何がrunを次へ移すか、`holds`はその間に握るslotを表す。
物差しは所要時間（区間の和）と、slotを握った時間（`holds`が`none`でない区間の和）の2本。
区間の列に当たらない時間（記録の抜け）はunattributedの区間として出し、KPIが所要時間に対する割合を見張って`[kpi.targets]`の目標で判じる。

| 知りたいこと | コードの入口 |
| --- | --- |
| 工程・タグ・attemptと、誰がどの場面でどの工程を書くか | `domain::run_phase`のmodule doc |
| supervisorの中の工程の移りと記録 | `supervise::execution::stages`の`Slot::transition` |
| slotを離れたrunの工程 | `Phase::resting` |
| 人の`integrate`の着地が書く工程 | `application::integrate`の`record_phase` |
| holdが効いたrunの`held`と解除 | `infrastructure::task_holds` |

- storeは`run_integrated`と同じtransactionで工程`push`を、`run_recovered`と同じtransactionでstatusの工程を書く。
  台帳は、着地と回収の工程がそのeventと食い違わないことを前提に畳む。
- 工程を足すときは、記録の工程のタグとsupervisorの工程の対応の網羅的な`match`に足す（書き忘れるとコンパイルが止まる）。

### 終端

`run_integrated`の後は工程`push`で、pushの結果の記録で`ended`か`push_pending`に移る（`Phase::after_push`）。
人が手で打った`git push`はeventが無いので、次の`push_finished`まで`push_pending`のまま残る。
taskは`completed`（着地したrunがあればそのpushの終わり）か`canceled`で閉じる（ADR-t1662-1決定16）。

### claimの前

taskの台帳は`task_created`から始まる。
claimの前の待ちの理由はsupervisor単位の区間をtaskの区間に重ねて読む（ADR-t1662-1決定6）。
区間の行は`domain::pre_claim::pre_claim_intervals`が作る。
どの区間も始まりを書いた1つのsupervisorに属し、終わりは1通りに決まる（区間の種類ごとの始まりと終わりは`domain::pre_claim`のmodule doc）。
`run_claimed`の欄と意味は`domain::claim_facts`のmodule docが持つ。

### supervisorの一生

supervisorの生存と停止の証拠はEventStoreに残し、一生の終わりはeventだけから決める（`domain::supervisor_life`）。
登録の行が消える全ての経路は、同じtransactionで同じtokenの`supervisor_stopped`を書く（[persistence](persistence.md#runtime-ownership)）。

### 今の判定の記録

statsのslotの今の判定は状態表・run dirのmarker・processを入力に今を判定するので、supervisorの周回がその入力を観測してEventStoreに記録し、統計はeventだけから同じ値を作れる。
記録の流れと畳み方は`domain::live_alerts`、観測の周回は`supervise::observation::live_alerts`のmodule docが持つ。

- 警告の開閉のeventはいつ立ち消えたかの記録で、値の再現には使わない。
- 畳みはsupervisorの流れを足さず、選べる流れが無ければ理由つきの欠測にする（下の「欠測と集約の契約」）。
- runtimeの制御はこの記録を読まず、statsは今の観測を直接読む。

## 台帳の形

台帳は対象（run・task・session・queue・node）ごとに1行（ADR-t1662-1決定10）。
区切りの値は列、フローで形が変わるものはJSON。
行の項目と種類ごとの中身は`LedgerRow`と`domain::ledger`の各moduleのdoc comment、作り直しの規則は`ledger::rebuilds`が持つ。

- どのeventも、どれか1つの行の記録になり（`domain::ledger::owner`）、読み手は行だけから今の統計の関数の入力を戻せる。
- runの行には予定の欄があり、欄の一覧は`LedgerRow`のdoc commentが持つ。

## 畳む関数

区間の列を作る関数は1つで（ADR-t1662-1決定2）、台帳を作る係と、走っているrunを見る読み手（`status`・`timeline`・`forecast`）が共有する（`domain::ledger::fold`）。
読み手は`run_phase_changed`の名前とタグを出し、記録の無いrunは`legacy`にする（各moduleのdoc comment）。
入力はEventStoreのeventと、それから作るclaimの前の区間の行・今の時刻だけで、同じ入力からは同じ出力を返す。

- runとtaskの畳み方は`domain::ledger::fold`、runのstepの内側の層は`domain::ledger::steps`のdoc commentが持つ。
- claimの後の事実は`domain::after_claim`が作り、statsの`MeasureTrack`も同じものを使う。

## 台帳を作る係

何を書くかは`domain::ledger::builder`が決め、`supervise::observation::ledger`が書く（印・書く行・確定・旧方式・1周の上限と、周回・保持・失敗は両moduleのdoc）。

- 着地の処理の中では作らず、runtimeの制御（claim・slot・resume・着地）は台帳を読まない。
- runは終わりから猶予の後で、telemetryのtokenの失効と資源の最後の窓の記録の後にだけ確定する（[実行環境と資源](measurement-environment.md#台帳との順序)）。
- storeの印は、その位置までの全入力を行が記録する約束で、Wはsession行、Rはほかの行のカーソルごとの位置。
- 増分の走査はRより後だけを読み、対象別・id指定の読みはRより前に及ぶが確定済みの履歴に比例しない（`builder`のdoc）。
- 読めない・書けない周回は印を残して次に任せ、runとtaskの結果を変えない。

## 共通の読み取り関数

stats・kpi・reportの全ての欄は、行を窓と絞り込みで選んで返す1つの関数（`domain::ledger::window`）だけから作る。
統計のmoduleはこの関数が返す値だけを取り、StateStore・run dir・process・Gitを読まない。
窓の当て方・絞り込み・旧方式と走っているrunの扱い・欄ごとの元の行は`domain::ledger::window`、今の名前の意味の違いは`ledger::read`のdoc commentが持つ。

- kpiの工程の名前と、hostの設定の目標の読み替えは`domain::kpi::window`のmodule docが持つ（[ADR-t1662-1](../adr/2026-10-04-t1662-1-runs-are-covering-phase-intervals-folded-into-a-ledger.md)が[ADR-0051](../adr/0051-kpi-time-series-report-and-push.md)決定1をamends）。
- 新しい出力は所要時間とslotを握った時間の2本でタグごとの内訳とunattributedを出し、`change`と`area`で層に分ける。

### 欠測と集約の契約

値の無い窓は0や空でなく、理由つきの欠測として返す。

- 今の判定: 窓の終わりで猶予内の最も新しい成功した観測を持つ流れ1つの入力を窓の終わりで判定し、流れを足さず、開閉の記録から値を作らない。
  選べなければ「観測が無い」で、理由は`domain::live_alerts::Missing`のdoc commentが持つ。
- mainの履歴: 到達点（観測の到達時刻で、commitの時刻ではない）から猶予より後の窓は「記録が無い」（Gitの読み取りの失敗・supervisorの停止・到達点が古い・取り込みより前）。
- 着地の後の品質: 記録の前の着地は「記録より前」、読めない線の取り込みの前は「不明」で、窓の終わりまでの経過と到達を添える。
- 属性とfinding: 基準のeventより前の窓は「基準より前」。
- providerの控えと切り替え: 記録より前は「記録なし」、旧方式も畳む。
- Executionのcontext: 記録が無い（欄の無い前の記録）と未計測（nullと理由）を分ける。
- token: Executionの終わった時刻（対話は区切りの時刻）で日に振り分け、actor × provider × modelで足す（[Execution tokens](execution-tokens.md)）。

## 並走の比較と照合

`stats`・`kpi`・`report`は全eventも読まない（[ADR-t3745-1](../adr/2026-10-31-t3745-1-stop-readers-of-every-event-until-the-ledger-readers.md)）。
`stats --ledger`は止めたままで、段2の並走の比較はせず、段3の後に消す。
比べ方と照合は`domain::ledger::compare`のmodule docが持つ。

## runの比較の軸

claimの属性は`run_claimed`の`attributes`（キーと値の開いた組）に持ち、記録・台帳・kpiはキーを知らずに運ぶ（ADR-t1662-1）。

- 記録: claimは`run_claimed`の他の欄を書いた後に、その欄から`claim_facts::attributes`で組を作る。
  キーと元の欄の対応は同じmoduleの表`KEYS`だけが持ち、新しいキーはそこに足す。
- キーは`^[a-z][a-z0-9_.]{0,63}$`、値は256 byte、1つのrunに64キーまで（値と数は予定）。
- 予約名（派生の軸。属性のキーにしない）は`claim_facts::RESERVED`が持ち、kpiが派生の規則で作る。
- [指示の版](supervisor-lifecycle/prompt.md#workerが読んだ指示の版)も属性のキーで、`kpi --by`で層になる。
- 群の比べは`--by group`・`--by pair_group`で読み、組は`kpi::trial`・`kpi::pair`のdoc comment。
  providerの組は割り当てどおりに動いた組を主に、切り替えたrunを割り当てた群ごとの別の層に、割り当てた群を副に出す。
  判定できないrunは、環境が原因のrunの層を除いて副だけに含め、群ごとに件数を見せ、切り替えたrunの数には入れない。
  復旧jobが環境が原因と判定してrunの記録に残したrunは、主から外して割り当てた群ごとの別の層に同じ項目で並べ、副には残す（ADR-t4163-1）。
  判定の無いrunは不明として今の層のままにする。

## 送る口

（ADR-t1662-3決定1〜13）

- **送る側**: executor（session wrapper、`integrate`とe2eの検証、非対話のjobを起動するプロセス）がstepと中から測った資源をTelemetrySinkに送る。
  agentは送らない。
  supervisorは工程・claim・slot・ask・`environment_attached`・nodeの時系列・外から測れる資源を自分で書く。
- **集める口**: executorはagentの出力を`StepReader`で、検証は流したコマンドからstepを作り、30秒ごとと終わりに送り、送れなくても結果を変えない（送り直しと置き場は`step_report`）。
- **TelemetrySink**: portで、今のアダプタはqueue serviceの`telemetry_report`（[Queue service](queue-service.md#telemetry_report)）。
- **principal**: role `executor-telemetry`。
  制御側の依頼でserviceが対象ごとに発行し、制御側が失効させ、agentに渡さない（詳細は[Queue service](queue-service.md#principalとtoken)）。
- **所属**: runの対象のstepはrunの行に畳み、runを持たない対象のstepはSessionStepStoreに持ってsessionの行で読む。
- **sessionの登録**: 制御側がsessionを起動の前に、agentが名前を決めるsession（Codexのthread）は出力で読んだ時点で対象に登録する（経路は`application::telemetry_tokens`のmodule doc）。
- **差し替える口**: 送る口・環境と資源の口（[実行環境と資源](measurement-environment.md)、適合testも）。
- **OpenTelemetry**: [対応の表](measurement-otel.md)で写し、記録の経路にしない。

## コマンドの形と分類

stepのコマンドの形と台帳を作るときの分類は[計測のコマンドの形と分類](measurement-commands.md)が持つ。

## 段と後続のtask

段0（ADR 3本とこの文書）→段1（記録）→段2（ストアのport・台帳と畳む関数）→段3（読み手を台帳と畳む関数へ移す）。
