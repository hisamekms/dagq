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

> **一部だけ実装済み**: 「SSOTとビュー」のport・「区間とタグ」のrunの工程の記録（`run_phase_changed`）・「終端」・「claimの前」・「supervisorの一生」・「今の判定の記録」と「runの比較の軸」の記録、run・task・session・queue・nodeの台帳と「畳む関数」・「台帳を作る係」・「共通の読み取り関数」・「並走の比較と照合」と「送る口」（中から測った資源は予定）・「コマンドの形と分類」は実装済みで今の姿を書く。
> ほかの節は計測の作り直しの今の予定で、まだ`src/`に無い。
> 今動いている計測は[計測のコマンド](supervisor-lifecycle/commands.md)と最初のcommitの観測（`first_commit_observed`）が持ち、この文書はそれらを変えない。

決めた理由はADR-t1662-1〜3が持つ。
ここはeventの欄・列とJSONの形・数値・関数の名前の予定を持つ。

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
`docs/plans/`の1回きりや週次の見直しの測定の出力（scriptとCSV）はこの表に足さず、そのplansの文書が区分（SSOT・ビュー・材料）と作り直せる範囲を持つ。

## 区間とタグ

runの一生を、重ならず隙間なく全体を覆う区間の列で表す（ADR-t1662-1決定1〜5）。

- **境目の記録**（実装済み）: supervisorが工程を移るたびに`run_phase_changed`を書き、次の記録が前の工程を閉じる。
  payloadの欄と`attempt`の数え方は`PhaseChange::payload`と`Attempt`のdoc commentが持つ。
  後の工程は、次のrevise・resumeまで前のattemptのまま。
- **書く所**: supervisorの中の移りは工程ごとの状態のmoduleの移す操作を通り、外では下の`Integrator`とstoreが書く。
  slotのrunはslotの工程・slotの外の待ち・着地の順番待ちから、slotを離れたrunはstatusから工程を読み、`validating`に休む主張と着地の列か拾い直しか人待ちかで分かれる`awaiting_integration`はeventでも読む（`Phase::resting`）。
  前と同じ工程とattemptは書かず、プロセスが初めて見るrunは記録を読み直してattemptを引き継ぐ。
  人の`integrate`の着地は`Integrator`が書く: integration slotを取った所で`landing`を、mainが動く前にslotを返した所でrunが戻ったstatusの工程を書く。
  supervisorの着地はslotが書く。
  storeは`run_integrated`と同じtransactionで`push`を、`recover`と放置のrunの回収は`run_recovered`と同じtransactionでstatusの工程（`interrupted`は`ended`、やめた着地は`landing_queue`、reviewし直すなら`returning`）を書く。
  storeはholdが効いたunfinishedのrunに`held`を書く: すぐ効いた`task_held`、leaseの返却・回収・保存の中のsettle、`hold_parked`、休むrunのholdをsettleするsupervisorのpass。
  `held`に入り解除の前のrunがunfinishedのまま休むなら、回収・leaseの返却・`Integrator`のslotの返却はstatusの工程を書かず（終端のstatusなら書く）、解除の`task_released`が休む工程で閉じる。
- **タグ**: `blocker`は何がrunを次へ移すか、`holds`はその間に握るslotで、値と意味は`Blocker`と`Holds`のdoc commentが持つ。
  `holds`はslotの数え方（着地の順番だけを待つrunと人の答えを待つrunはslotの外）に従い、slotに残る答え待ちの`waiting`はworker slotを握る。
- **taskのhold**: `task_held`から待ちに入るまでは走る工程のまま、待ちに入った記録からholdが閉じるまでを`held`にする。
- **containerのメモリの待ち**: runの`container_memory_waiting`から`container_memory_wait_ended`までで（欄は定数のdoc comment）、turnでは`turn_started`の前、verificationでは最初の`verification_command`の前にあり、工程を変えない。
- **`already_satisfied`の主張**: mainのsnapshotの検証は`validating`と別の工程でworker slotを持ち、検証を待つ間（slotやcontainerの空き）と答えを待つ間はslotの外の工程にする（[ADR-t1818-1](../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)）。
- **`Phase::tags()`**: 記録の工程の定義（`domain::run_phase`）の隣で網羅的な`match`がタグを返す。
  supervisorの工程も網羅的な`match`で記録の工程に対応し、どちらも工程を足して書き忘れるとコンパイルが止まる。
- **unattributed**: 区間の列に当たらない時間（記録の抜け）はunattributedの区間として出す。
  KPIは所要時間に対する割合を見張り、閾値以上を目標割れにする（予定）。
- **物差し**: 所要時間（区間の和）と、slotを握った時間（`holds`が`none`でない区間の和）。

### 終端

`run_integrated`の後は工程`push`（`run_integrated`と同じtransactionで記録）で、pushの結果の記録と同じ所で、`push_finished`（`already_delivered`を含む）か`push_skipped`なら`ended`を、`push_failed`なら`push_pending`を記録する。
人の`integrate`も同じ`Integrator`を通るので同じ記録になる。
`push_pending`を同じremote・branchへの後の`push_finished`で閉じるのは畳む関数で、人が手で打った`git push`はeventが無いので次の`push_finished`まで`push_pending`のまま。
着地しないrunは`ended`で閉じ、`failed`のrunを復旧jobが動かせば次の工程に移る。
taskは`completed`（着地したrunがあればそのpushの終わり）か`canceled`で閉じる（ADR-t1662-1決定16）。

### claimの前

taskの台帳は`task_created`から始まる。
claimの前の待ちの理由はsupervisor単位の区間をtaskの区間に重ねて読む（ADR-t1662-1決定6）。

- **区間の行**: `domain::pre_claim::pre_claim_intervals`がeventの列と今の時刻だけから`PreClaimInterval`の行を作り、StateStoreを読まない。
  どの区間も始まりを書いた1つのsupervisorに属し、終わりは下の規則で1通りに決まる。
- **`slots_full`**: supervisorのfill passは、始めと終わりに自分のslotを見て、classの枠が全部埋まれば`slots_full_started`を、空けば`slots_full_ended`を書く（`domain::pre_claim::slots_full_payload`）。
  classは`SlotClass`で、classごとに別の区間になる。
  slotはsupervisorごとなので他のsupervisorは書きも閉じもせず、別のsupervisorの`supervisor_started`でも閉じない。
  execの引き継ぎは同じtokenの続きで、開いた区間を自分の記録から読み直して続ける。
  終わりを書かずに止まったsupervisorの区間は、下の「supervisorの一生」の終わりで閉じる。
- **`claim_hold`**: 既存の`claim_held` / `claim_resumed`（queue共通の控え）が区間の始まりと終わりを兼ねる。
  終わりは`stats`の`claim_holds`と同じ規則（`domain::claim_hold::hold_spans`）で、どのsupervisorが書いた終わりでも閉じ、書いたsupervisorの停止でも閉じる。
  控えはqueueの全てのsupervisorのclaimを止めるので、taskに重ねるときは区間のsupervisorに依らずqueueの待ちとして読む。
- **`claim_deferral`**: 既存の`claim_deferred` / `claim_deferral_ended`（taskの控え）が区間の始まりと終わりを兼ねる。
  終わりは`stats`の`claim_deferrals`の`by_end`と同じ規則（`domain::claim_defer::deferral_spans`）。
- **taskのhold**: readyの間の`task_held`から同じholdが閉じるまでは、readyに重ねず工程`held`（runの`held`と同じタグ）にする（`domain::task_hold::task_hold_spans`）。
- **`run_claimed`の欄**: claimが書く欄と意味は`domain::claim_facts`のmodule docが持つ。
  supervisorは候補ごとの値をclaimに渡し、claimは取ったtaskの値だけを残す。
- **`blocked_by`**: 着地の順番だけを待つ間の`run_phase_changed`（工程`landing_queue`）は、そのとき着地slotを握っていたrunを`blocked_by`に持ち、握るrunが変われば記録し直す。

### supervisorの一生

supervisorの生存と停止の証拠はEventStoreに残し、一生の終わりはeventだけから決める（`domain::supervisor_life::supervisor_life_end`）。

- **停止**: 登録の行が消える全ての経路（自分の停止と、`up`・`down`・生きているsupervisorの掃除）は、同じtransactionで同じtokenの`supervisor_stopped`を書く。
  記録に失敗すれば行は残り、次の掃除に任せる（[persistence](persistence.md#runtime-ownership)）。
- **生存**: heartbeatを書くsupervisorは、`SUPERVISOR_ALIVE_INTERVAL_SECS`ごとに`supervisor_alive`を1つ書く。
  記録の失敗はwarnにしてheartbeatを止めない。
- **終わりの規則**: そのtokenの`supervisor_stopped`があればその時刻（`last_heartbeat_at`があればそれとの早い方）で終わる（`stopped`）。
  無ければそのtokenを`supervisor`に持つ最後のeventを最後の証拠とし、今がそれより`SUPERVISOR_ALIVE_INTERVAL_SECS` + `HEARTBEAT_TIMEOUT_SECS`より後ならその時刻で終わる（`silent`）。
  後で掃除の`supervisor_stopped`が届けば`last_heartbeat_at`で畳み直す。
  どちらでもなければ生きている。
- 停止のeventの無いstaleな登録は`silent`で閉じて掃除の後に直り、`supervisor_alive`の無い前のbuildのtokenも同じ規則で最後のeventで閉じる。
- kpiの`supervisor_lives`は今もsupervisorsの表を読み、この規則へ移すのは段2。

### 今の判定の記録

statsのslotの今の判定は状態表・run dirのmarker・processを入力に今を判定するので、supervisorの周回がその入力を観測してEventStoreに記録し、統計はeventだけから同じ値を作れる（`supervise::observation::live_alerts`）。

- **観測**: どのsupervisorも`OBSERVE_INTERVAL_SECS`ごとにqueue全体の入力を読み（eventは前回の後の分を`LiveHistory`に畳む）、statsと同じ判定の関数（`live_alerts::judge`）で判定する。
  警告の鍵（`AlertKey`）はleaseの持ち主を含めないので、leaseが移っても何も書かない。
- **判定の入力と警告の開閉**: 入力は離散の時刻と値で、経過で伸びる長さは持たない（入力の中身は`domain::live_alerts`のmodule doc）。
  開閉のeventは警告がいつ立ち消えたかの記録で、値の再現には使わない。
- **走っているrun**: receiptの無いidleは、markerの後の`provider_waiting`・closeまでの`worker_question`・`answer_prompt`・終わったresumeとreviseのsession・containerのメモリの待ちを数えず、次のturnのmarkerから数える（stallの見張りと同じ）。
  入力と警告のworkspaceはrunが最後に開いたsession（`LiveRun`のdoc comment）。
- **記録**（流れはsupervisorのtokenごと）: 起動の後の最初の成功した観測が`live_alert_baseline`。
  入力の値が変わった対象ごとに`live_alert_input_changed`。
  警告の鍵の集合が変わったときだけ`live_alert_started`・`live_alert_ended`。
  `REACH_INTERVAL_SECS`ごとと、観測の失敗の始まり・成功への戻りに`live_alert_reached`。
  書けなかった記録の後は基準からやり直す。
- **畳み**: `live_alerts::fold`はeventと窓の終わりだけから、窓の終わりまでの最後に成功した観測が`GRACE_SECS`以内の流れのうち最も新しい1つ（同じ時刻はevent idの大きい方）を選ぶ。
  その流れの基準と入力の更新を版の順に当てた入力を、窓の終わりで`judge`に渡す。
  流れどうしは足さない。
- **観測が無い**: 選べる流れが無いときの理由は、基準が無い・記録の欠け（到達点が確かめた版に入力の記録が届かない）・supervisorの停止（全ての流れの`supervisor_life_end`）・観測の失敗・到達点が古い。
  `supervisor_alive`は理由の区別にだけ使う。
- runtimeの制御はこの記録を読まず、statsは今の観測を直接読む。

## 台帳の形

台帳は対象ごとに1行（ADR-t1662-1決定10）。
区切りの値は列、フローで形が変わるものはJSON。
行の項目は`LedgerRow`のdoc commentとmigrationのコメント。

- **作り直し**: 版が上がった行と、到達点より後に入力が届いた行は畳み直す（`ledger::rebuilds`、確定の印の後と旧方式の行も同じ）。
- **記録**: どのeventも、どれか1つの行の記録になる（`domain::ledger::owner`）。
  行は自分の記録とその入力のevent idの範囲を持ち、行だけから今の統計の関数の入力が戻る。
- **run**: 区間の列・タグごとの合計・claimの属性の組（キーを知らずにそのまま）・claimの後の事実と、そのrunの上のsessionとjob・Executionの列・stepの畳み（`steps`）。
  予定: `change`・`area`・`nature`。
- **task**: 区間の列と重ねたsupervisor単位の区間で、終わる前も確定しない行を持つ（draftの移りとrunの無いaskも記録）。
- **session**: runを持たない対象のsessionとjobの1回（`domain::ledger::session`）。
- **queue**: run・task・sessionに属さないqueue全体の区間か出来事の1つ（`domain::ledger::queue`）。
- **node**: hostの欄の置き場で、NodeSampleStoreだけを読みnodeと時間の区切りごとにsampleのまま持ち、係が同じ保持で消す（`domain::ledger::node`）。

## 畳む関数

区間の列を作る関数は1つで（ADR-t1662-1決定2）、台帳を作る係と、走っているrunを見る読み手（`status`・`timeline`・`forecast`）が共有する（`domain::ledger::fold`）。
入力はEventStoreのeventと、それから作るclaimの前の区間の行・今の時刻だけで、同じ入力からは同じ出力を返す。

- **run**: 境目は`run_phase_changed`だけ。
  最後の記録が`ended`なら閉じ、`push_pending`なら後の`push_finished`で閉じ、ほかは今の時刻まで畳んで終わりを持たない（確定していない）。
- **task**: statusの移りを工程にし、runの間はそのrunの区間を差し込み、readyの間は`ready_at`で依存の待ちを分けてsupervisor単位の区間を重ねる。
- **claimの後の事実**は`domain::after_claim`が作り、statsの`MeasureTrack`も同じものを使う。
- **step**: runのstepは`domain::ledger::steps`が親の区間ごとにkindの内訳と被覆率に畳み（内側の層は重なってよい）、区間の外のstepとseqの抜けを数える。

## 台帳を作る係

何を書くかは`domain::ledger::builder`が決め（数値は定数）、`supervise::observation::ledger`が書く。

- **周回**: supervisorの周回が間隔ごとに、drainとhandoffの間も、jobのthreadで動く（D7）。
  着地の処理の中では作らず、runtimeの制御（claim・slot・resume・着地）は台帳を読まない。
- **置き場**: 係の状態は観測と分析の状態、組み立ては`compose::observation`にあり、`Supervisor`の本体に状態を足さない。
- **印**: EventStore・NodeSampleStore・SessionStepStore・runの資源のカーソルの後に届いた入力だけを読み、そのrun・task・それが変えたsession・queue・readyだったtaskの行（記録でない終わりも）・nodeの時間に印を付ける（全行を走査しない）。
  畳むrun・taskのeventはその都度読み、queue全体の畳みは届いたeventを一度だけ読んで持ち越す（`builder::Kept`）。
  持ち越すのは本と、確定していないsession・queue・run・taskの行が読むeventだけ（`Kept`のdoc comment）。
  カーソルの始めから読む起動の追いつきでも、最初に行を畳む周がstoreの確定した行のうち畳み直さない行のeventを落とす。
  確定した行に後のeventが届けば、記録はstoreの行から、pushはidで読み戻す。
  周回は新しいeventが変えうる行と印の付いた行だけを畳み、周回で作る領域は前からのeventの数に比例しない。
- **書く行**: 終わったrun（`push_pending`と旧方式を含み途中は書かない）と、全てのtask・session・queue・nodeの行。
- **確定**（`final`）: run・taskは終わりから猶予の後で、`push_pending`には付けない。
  runは発行したtelemetryのtokenの失効の記録の後にだけ確定する。
  session・queue・nodeの行は`domain::ledger::settle`が決める（閉じた時刻か区間の終わりから猶予の後で、閉じていない行は確定しない）。
  runは資源の最後の窓の記録の後にだけ確定する（[実行環境と資源](measurement-environment.md#台帳との順序)）。
- **旧方式**: 切り替え前のrun（`run_phase_changed`が無い）とそのtaskは区間に畳まず、`legacy`に今の`RunMeasures`から値を一度だけ書く。
  storeは値と版を上書きせず、後の入力では記録だけを追う。
- **保持**: nodeの行はhostの負荷と同じ日数で消す。
  消す・変える行を上限の行数ごとにcommitし、無ければlockを取らず、止まれば次の周回が続ける。
- **失敗**: 読めない・書けない周回は印を残して次に任せ、続けば`ledger_build_failed`を1度記録する。
  runとtaskの結果は変えない。

## 共通の読み取り関数

stats・kpi・reportの全ての欄は、行を今のstatsの期間の窓（時刻かevent idの境界で、窓の終わりまで数える）とgoal・change・areaの絞り込みで選んで返す1つの関数（`domain::ledger::window`、`LedgerStore::ledger_window`）だけから作る。
行は入力のevent idの範囲と時刻の範囲で窓に当て（event idの境界は時刻で越えない）、旧方式の行は`legacy`から同じ名前と形の値を返す（`domain::ledger::read::read`）。
絞り込みはtask・goal・runの行を選び（nodeの行は時刻だけで当てる）、窓に状態を持ち込む畳み（属性・finding・mainの履歴・今の判定）は全ての行の記録を窓の終わりで畳む。
段3の統計のmoduleはこの関数が返す値だけを取り、StateStore・run dir・process・Gitを読まない（D1）。
欄・指標・節と今のuse caseの入力ごとの元の行は、同じmoduleのdoc commentの表が持つ。

- 今の値を残す1期間（ADR-t1662-1決定19）の長さは段3のtaskが決める。
- kpiの`phase.<工程>`と`land_phase.<工程>`も同じ1期間は同じ名前で作り、その後はタグごとの値と`phase.first_commit`に置き換える。hostの設定の`[kpi.targets."phase.startup"]`は`phase.first_commit`に読み替える（ADR-t1662-1が[ADR-0051](../adr/0051-kpi-time-series-report-and-push.md)決定1をamends）。
- 新しい出力は所要時間とslotを握った時間の2本でタグごとの内訳を出し、`change`と`area`で層に分ける。

### 欠測と集約の契約

値の無い窓は0や空でなく、理由つきの欠測として返す。

- 今の判定: 窓の終わりで猶予内の最も新しい成功した観測を持つ流れ1つの入力を窓の終わりで判定し、流れを足さず、開閉の記録から値を作らない。
  選べなければ「観測が無い」で、理由は「今の判定の記録」の5つ。
- mainの履歴: 到達点（観測の到達時刻で、commitの時刻ではない）から猶予より後の窓は「記録が無い」（Gitの読み取りの失敗・supervisorの停止・到達点が古い・取り込みより前）。
- 属性とfinding: 基準のeventより前の窓は「基準より前」。
- Executionのcontext: 記録が無い（欄の無い前の記録）と未計測（nullと理由）を分ける。
- token: Executionの終わった時刻（対話は区切りの時刻）で日に振り分け、actor × provider × modelで足す（[Execution tokens](execution-tokens.md)）。

## 並走の比較と照合

段2の並走で使い、段3で今の値を台帳から作った後に消す読み取り（`stats --ledger`）で、runの値・run・taskを持たない欄・hostの欄の比べ方と台帳の照合は`domain::ledger::compare`のmodule docが持つ。

## runの比較の軸

claimの属性は`run_claimed`の`attributes`（キーと値の開いた組）に持ち、記録・台帳・kpiはキーを知らずに運ぶ（ADR-t1662-1決定21〜26）。

- 記録（実装済み）: claimは`run_claimed`の他の欄を書いた後に、その欄から`domain::claim_facts::attributes`で組を作る。
  キーと元の欄の対応は同じmoduleの表`KEYS`だけが持ち、新しいキーはそこに足す。
- キーは`^[a-z][a-z0-9_.]{0,63}$`、値は文字列で256 byteまで、1つのrunに64キーまで（予定の値）。
- 予約名（派生の軸。属性のキーにしない）は`claim_facts::RESERVED`が持ち、kpiが今の`axis_value`と同じ規則で作る。
  組を作る関数は予約名のキーを入れない。
- 読み取りの優先: 層のキーが予約名なら派生の値、それ以外は属性の値、無ければ`unknown`。

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

段0（ADR 3本とこの文書）→段1（`run_phase_changed`・claimの前の区間・`run_claimed`の欄・`blocked_by`の記録）→段2（ストアのport・台帳と畳む関数・並走比較。差は`docs/plans/`に説明する）→段3（読み手を台帳と畳む関数へ移し`LandClock`を撤去）。
