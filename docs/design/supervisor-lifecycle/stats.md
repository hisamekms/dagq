---
id: design-supervisor-lifecycle-stats
type: design
title: "`stats`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1486-1
  - adr-t655-1
  - adr-t639-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-kpi
  - adr-0062
  - design-supervisor-lifecycle-waiting
  - adr-0040
  - adr-0049
  - adr-0048
  - design-provider-lifecycle
  - design-supervisor-lifecycle-worker-model
  - adr-0063
  - adr-0044
  - design-domain-model
  - adr-t610-1
  - adr-0079
  - design-supervisor-lifecycle-plan-review
  - adr-0070
  - design-supervisor-lifecycle-host-metrics
  - adr-t947-1
  - adr-t947-2
  - adr-t947-3
  - adr-t947-4
---

# `stats`

`dagq stats`がqueueの記録（run_events）からrunの時間・着地の内訳・閾値を超えたもの・期間の集計を導いて返す仕組みの概念と地図。
出力の欄・既定値・閾値の意味は`src/domain/stats.rs`と`src/domain/stats/`の定義のそばのdoc commentが持ち、決定の理由はリンクしたADRが持つ。
同じ集計を期間ごとの窓で読む指標は[kpi](kpi.md)、記録を書く側は[provider-lifecycle](../provider-lifecycle.md)と各工程の文書が持つ。

## 目的

- runがどこで時間を使い、何に詰まり、どの閾値を超えたかを、新しい表を持たずに記録から毎回同じ規則で導く（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定5）。
- 走っているrunの止まりの疑い（running alert）と閾値ごとの検知の結果を同じ出力で返し、observer・plan review・人・KPIが同じ値を読む（[ADR-0043](../../adr/0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)決定5・6）。
- 版・設定・試しの群などの変更の前後を、runを属性で分けた群で比べられるようにする。

## 全体の流れ

```text
CLI（src/main.rs） / observer
  └─▶ compose::OneShot::stats_of        今の時刻を1回読む
        └─▶ application::stats::stats   queue・run directory・wrapperのprocess・mainの履歴・host/ を読む
              └─▶ domain::stats::stats  純粋関数: events → runs・群・alerts・期間の集計
                    └─▶ with_changes / with_areas / without_cargo_measures → JSON
```

1回の読みは4つの範囲を持つ。

```text
runのpage:  --since < finished_event_id ≤ --until の終わったrun（既定は直近の一定件数、--fullで全件）
            → runs と、runsから求める群（goals・overall・changes・areas・e2e・versions・load_bands・trial_groups・escalations）
期間の窓:   --since（無ければpageのrunの最初のevent、--fullなら最初）の後 〜 --until（無ければ最新のevent）
            → backend_failures・asks・waiting・sessions・jobs など残りの期間の集計
時間の窓:   期間の窓の両端の時刻（今を超えない）→ host・landing_utilization
今:         → running_alerts・slotのalert・控えの今（held / deferred）
```

## 責務と境界

- domain（`src/domain/stats.rs`と、集計ごとのmoduleを置く`src/domain/stats/`）が全ての数え方を持ち、I/Oを持たない。
  この地図は集計のmoduleを名前だけで指す。
  入力はeventの全件、task→goalの対応、今の時刻、supervisorの空きslotの観測（`SlotSnapshot`）、走っているrunの観測（`LiveSnapshot`）。
- application（`src/application/stats.rs`）はqueueと外の情報源（`StatsSources`: run directory、idle marker、`[stall]`・`[conflicts]`、mainの履歴）を読んで渡すだけで、判断を足さない。
- `stats`は何も書かない。
  hostの負荷はqueueのディレクトリの`host/`を、衝突の多いファイルの着地数はmainのgitの履歴を読むだけ。
- 記録を書くのは各工程（supervisor・`integrate`・hook・job）で、記録の欄の意味は書く側の型と文書が持つ。
- [kpi](kpi.md)・[完了見込み](#完了見込み)・plan reviewの衝突の多いファイルはdomainの関数を直接呼び、同じ規則を読む。

## 不変条件

- 新しい表もeventも持たず、出力は記録からいつでも再導出できる。
  集計を足すときも、記録を足してそれを再導出する形にする。
- 既存の欄は変えず、足すだけにする（observer・KPI・レポート・plan reviewのpromptが欄を読むため）。
- runのpageに従うのはrunsとrunsから求める群と`next_cursor`だけで、期間の集計はpageで切らない。
- 同じevent・同じ今の時刻・同じ観測からは同じ出力になる。
- 記録の無い古いeventは書き換えず、null・`unknown`・`unlabeled`として数える。

## 読み方の約束と落とし穴

- cursorは数字ならevent id、`@`付きならunix秒、RFC 3339ならその時刻以前に記録された最後のevent（`Cursor`）。
  数字だけのものはevent idなので、unix秒には`@`を付ける。
- `--since next_cursor`で続きのpageを読むと、期間の集計は同じ窓を重ねて数える。
  pageごとの期間の集計は足し合わせず、1回の読みの値を窓の全体の値として読む（`Stats`のdoc comment）。
- `--goal`で絞ると、taskを持たないevent（observerの観察、taskの無いask、plannerのturn、自動更新など）は0か数えない。
  goalで絞った値と絞らない値は、その種類の欄では比べられない。
- 期間の集計の多くは窓の中で起きたものを数え、その後の結末は窓の外のeventからも読む（どちらで切るかは集計の型のdoc comment）。
- 対話のworkerの記録（入力の待ち・送信の確認・`/exit`まわり）は過去の記録として読む（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。

## runの行と群

| 知りたいこと | コードの入口 |
| --- | --- |
| runの区間（work・validate・wait_to_land・startup）と回数 | `RunStats`、`runs` |
| goal・全体・change・area・e2eの群 | `GoalStats`・`Intervals`、`with_changes`・`with_areas`・`e2e_groups` |
| claimの属性（版・負荷・provider・model・試しの群） | `RunStats`の欄、`measures` |

- changeはtaskが宣言した変更の種類、areaは着地したcommitの差分を`dagq.toml`の`[areas]`に通した変更の対象（[ADR-t980-1](../../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)、[kpi](kpi.md#area)）。
  areaは保存せず読むたびにgitから求め、1つのrunを持つ全てのareaに数える。
- e2eの群は、validatingがe2eを求めたかとその出どころでrunを分け、差分でe2eを狭めた前後を比べる（[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定2）。
- providerはtaskが求めたもの（requested）と最後に作業したもの（actual）を分け、claimの時点の切り替えも数える（[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)、[provider-lifecycle](../provider-lifecycle.md)）。
  経路（`route`）はclaimの値のままで、途中の切り替えで変えない。
- Codexで作業したrunは、claimの`model`でなくCodexのturnが記録した実際のmodelで数え、試しの群に入れない（[Worker model](worker-model.md)）。

## alerts

- 入口: `Alert`と閾値の定数（`src/domain/stats.rs`の先頭）で、まだ終わっていないrunも見る。
- `awaiting_integration`の起点は最初に`awaiting_integration`になった時刻で、着地の失敗で戻っても変えない。
- `ask_unanswered`は、その後にrun（かそのtaskのrun）が着地したaskを数えない。
- `task_failed`はpageに関係なく全runで数え、最後に失敗したrunが対象に入るときに出す。
- slotのalertは`claim_held`・`claim_deferred`・`idle_slots`の順に1つだけ出し、`stats`を読んだ時点の観測で判定して時間帯の履歴を持たない（`SlotSnapshot`のdoc comment）。
  着地の順番を待つrunは埋まったslotに数え、人の答えを待つrunは数えない（[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)決定13、[ADR-t610-1](../../adr/2026-09-27-t610-1-landing-runs-fill-the-slot-in-status-and-stats.md)）。
  着地待ちが空けた軽い枠（[claimを控える](claim-hold.md#着地待ちが空けた軽い枠)）と、draftのgoalのreadyのtaskは数えない。
- `claim_deferred`は控えの理由を問わず数える（[claimを控える（衝突の多いファイル）](claim-defer.md)、[ADR-t1632-1](../../adr/2026-10-05-t1632-1-claim-waits-for-a-build-that-contains-the-dependencies-landings.md)）。

## running alerts

- 入口: `RunningAlert`と`running_alerts`、markerとwrapperの生死を読むのは`application::stats::stats`。
- `--since`に関係なく毎回出す。
- 見ているsessionは、workerのsession・resume中のsession・送ったreviseのsessionのどれかで、それ以外のrunは見ない。
- `idle_without_receipt`で`nudged`も`asked`もfalseなら、supervisorの検知の漏れを示す（[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)）。
- `long_background`は観測だけで、復旧jobを起動しない。
  経過は、その処理が途切れずにmarkerに載り続けた最初の時刻から測る（markerは上書きされ開始時刻を持たないので、hookが追記するlogから読む。`domain::stall::IDLE_LOG`と`background_first_seen`）。
  logが無いときはmarkerのmtimeからの下限になる。
- `workspace_mismatch`はcmuxに聞かず、runの最後のsessionのbackgroundのwrapperをpidと記録した起動時刻で見る（pidを別のprocessが継げば死んでいる。[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定2・10、[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。
  heartbeatの古さは生死に使わない（[wrapperが黙ったsession](silent-wrapper.md)の側）。
  古いバイナリのworkspaceのsessionは判定せず、`workspace_check`が数を出す。
  `stats --cmux`は受けて無視する。

## 閾値ごとの検知

- 入口: `thresholds`と`ThresholdStats`、判定に使った閾値は`stall_config`、閾値の決め方は[Stall thresholds](stall-thresholds.md)。
- `[stall]`の設定名は検知が0でも全て出す。
- 検知は窓の中のものを数え、結末は窓の外のeventからも決める。
  結末の記録がまだ無い検知は`pending`。
- `preempted`（見逃しの疑い）は、idle markerの履歴がeventに無いので確かめずに疑いとして数える。
- CPU時間が伸びないプロセスの記録はsupervisorのメモリにしか無いので、その設定名の`running_alerts`は常に0（[復旧job](background-recovery-job.md)）。

## claimと着地の控え

- claimの控え（load averageと空き容量）は`domain::claim_hold::claim_holds`（[claimを控える](claim-hold.md)）、着地の検証の控えは`domain::claim_hold::holds_of`（[空き容量を確かめる](disk-space.md)）。
- 衝突の多いファイルなどでのclaimの控えは`domain::claim_defer::claim_deferrals`（[claimを控える（衝突の多いファイル）](claim-defer.md)、[ADR-0069](../../adr/0069-do-not-claim-tasks-overlapping-hot-files.md)）。
- loadの保留中のclaimの間隔の待ちは控えに数えない（[ADR-t1479-1](../../adr/2026-10-04-t1479-1-space-new-claims-while-the-load-hold-is-on.md)、[claimの間隔](claim-hold.md#claimの間隔)）。
- `--goal`でも、今控えているtaskの一覧はqueueの今を出す。

## 着地待ちの内訳

`wait_to_land`をrunのeventで着地の工程に切り分ける（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定5）。

- 入口: `LandClock`と`PHASES`（工程を始めるeventと中身は`PHASES`のコメント）。
- 工程の合計は`wait_to_land`に等しい（工程ごとに秒へ切り捨てるので数秒ずれうる）。
  pushは着地の後なので合計の外で別に測る。
- `needs_session`のstatusを持つeventは他の規則より先に`resume`に移す。
- askは全て答えられたら前の工程に戻り、closeはeventを書かないので、間に他の工程のeventが来たら開いているaskを忘れる。
  runtimeが適用する`approve_landing`の答えは着地slotの順番待ちに移り、inboxが読む答えはaskのまま。
- まだ着地していないrunの内訳は`awaiting_integration`のalertの`phase`にだけ使う。
- 長い裾を作った工程は、工程ごとの`tail_total`を比べて読む。

### verifyのコマンド別の内訳

- 入口: `CommandSecs`・`CommandSummary`。
- 延期された試行と失敗したコマンドも数え、1本の秒を工程の間隔で頭打ちにするので、コマンドの合計は`verify`を超えない。

## 着地の直列処理の使用率

着地（`integrate`）はqueueで1本ずつ流れるので、その使用率が1に近づくと着地数が頭打ちになる。

- 入口: `landing_utilization`と試行の終わりの`ENDS`。
  [kpi](kpi.md)は期間の窓で同じ関数を呼ぶ。
- 着地しなかった試行も数え、試行は重ならないので窓の中の合計は窓の長さを超えない。
  終わりの記録の無い試行は、次の（どのrunの）試行の開始で切る。
- `--goal`はそのgoalのtaskの試行と待ちだけを数えるが、試行を切る次の開始はqueue全体から読む。

## 着地の延期とresume

- 入口: `retries`と`BrokenBy`・`RebasedOnto`・`ResumeAttempt`。
- `broken_by`は延期のrebase先のmainのcommitを、そのcommitを`result_commit`に持つ着地に結び付ける。
  自分の着地と、runtimeの外で動いたmainは結び付けない。
  着地は全eventから探すので、窓の外の着地にも結び付く。
- 不安定なtestによる失敗と、rebase先の着地が自分の着地で同じ検証コマンドを通していた失敗は、rebase先の責任にせず`rebased_onto`に残す。
  後者は、その着地のtreeでは落ちたtestが通っていたので、落ちたのはrun自身の変更かmainとの組み合わせであるため。
- `broke_runs`は`rebased_onto`だけに持つrunを数えない。
- resumeが1回で解けた（`resolved`）とは、`resolved`で終わり、次のresumeまで`needs_session`に戻らなかったこと。
- Codexのrunの段上げはmodelを持たないので段の名で名指す。

## 版と負荷と検証コマンド

- 版と負荷の群は`measures`（`RunMeasures`・`Versions`・`LoadBandStats`）、負荷の帯は`domain::measure::load_band`。
- providerの群はactual（最後のprovider）で分け、途中で切り替えたrunは最後のproviderの群に入る。
- 検証の失敗の分類ごとは`measures::verification_failures`、落ちたtestごとは`failed_tests`（分類は[`integrate`](integrate.md)）。
- hostの分類で落ちた検証のやり直しと、不安定なtestの着地のやり直しは数え方が違う（`FailureClassStats`のdoc comment）。
- workerのsessionで落ちたtestは多くが作業中のtestなので、並べるが不安定なtestの候補の判定には入れない。
  候補はobserverが`flaky_test`のfindingにする材料になる（[Observer](observer.md)）。

## Claude session

- 入口: `sessions`と`Sessions`、区間の記録の書き方は[provider-lifecycle](../provider-lifecycle.md)（[ADR-0048](../../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)決定3・11・12）。
- 区間は`session_opened`とそれを指す最初の`session_closed`の対で、transcriptは読まない。
  閉じていない区間は、runの行では読んだ時刻まで、期間の集計では窓の終わりまでの長さにする。
- 稼働時間はtranscriptのturnの長さの合計で、記録したものだけを数える。
  hookで閉じたinbox・plannerの区間は、supervisorの取り込みが後で書く最後のturnの記録から読む（[ADR-t655-1](../../adr/2026-10-04-t655-1-hook-close-defers-transcript-intake-to-the-supervisor.md)）。
  取り込みの前の区間は稼働時間を読めないものとして数える。
- kindは記録が0でも全て出す。
  inbox・planner・runtimeのplannerはrunを持たないので、runの行と群には出ず期間の集計にだけ出る。
- 経路ごとの内訳は、対話と非対話のruntimeのplannerを同じ物差しで並べるため（ADR-t1394-2決定4）。

## 作業の内訳

- 入口: `work`と`RunWork`・`WorkShares`、分類を記録する側は[provider-lifecycle](../provider-lifecycle.md)。
- runのsessionが閉じるときに記録した内訳から再導出し、transcriptは読まない。
- Codexの区間はwrapperが書いたturnのコマンドから同じ形で作り、時刻の精度は約1秒。
  内訳を作れなかった区間は数えないので、それを含む期間の値は下限。
- 検証の重複は、`integrate`がもう一度流す検証と同じ種類のコマンドをworkerが流した数で、文字列でなく種類で見る。

### cargo専用の計測

- 入口: `CargoOnly`と`without_cargo_measures`、判定は`StatsSources::dagq_source`（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)、[Source repository](source-repository.md)）。
- dagqの検証の形とhostの`rustc`を前提にした値は、queueのrepositoryがdagqのソースのときだけ出し、他では欄ごと出さない（0やnullにすると事実として読まれるため）。
- domainの`stats::stats`を直接使うもの（KPIの窓、plan reviewの衝突の多いファイル、完了見込み）は隠さない。
- 作業の内訳の分類（e2e・llvm-cov・test）は、記録の側が付けないので出力では隠さない。

## トークン数

- 入口: `tokens`と`RunTokens`・`TokenSummary`、記録の書き方は[provider-lifecycle](../provider-lifecycle.md)。
- 区間が閉じたときの記録を足すので、長く開いた区間（常駐のinbox）は閉じた日の窓にまとめて入り、日ごと・actorごとには比べられない。
  ほかの数えない分と、実行ごとの記録への置き換えは[ADR-t1486-1](../../adr/2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)と[Executionのトークン数](../execution-tokens.md#今の穴)が持つ。
- runを持たないobserverとplan reviewのトークン数は期間の集計のsessionのkindごとで読む。

## 重さの予測と実績

- 入口: `predictions`と`RunPrediction`・`RunActual`、百分位は`domain::prediction::percentile`（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定2・4）。
- runには、そのrunの前に記録されたそのtaskの最後の予測を当てる（runの後の予測は次のrunのもの）。
- 予測の値は2〜3倍に偏るので、値でなく百分位で読む。
- 予測の精度はruntimeが集計せず、読む側がこの行から計算する。
- taskに由来する手戻りは衝突とkillを数えない（`domain::plan_quality::rework`）。

## workerのmodelと試しの群

- 入口: `trial`（ADR-0079決定4・6、[Worker model](worker-model.md)）。
- runのpageのrunだけを数えるので、試しの判定は`stats --full`か試しを始めた時点からの`--since`で、推移は`kpi --by group`で読む（[kpi](kpi.md)）。

## workerのsessionの段上げ

- 入口: `escalations`（ADR-0079決定5、[Worker model](worker-model.md)）。
- taskに由来する失敗の後に上げたresumeと切り替えたreviseを数え、上げたresumeがその1回で解けたかを並べる。
- retryのclaimで引き継いだ段は数えず、`kpi --by effort`の層に出る。

## draftの流入と流出

着地1件あたりにruntimeやjobが登録するdraftの数と、それが決着する速さを同じ窓で読む。

- 入口: `drafts`と`DraftFlow`。
- 数えるのはruntimeやjobが登録した（出どころのある）draftだけで、人が登録したdraftと、reopenで戻ったtaskは数えない。
- draftから初めて出たときだけ数え、reviseでdraftに戻して出し直したものは数え直さない。
- [kpi](kpi.md)の着地あたりのdraftと滞留のKPIは同じ関数の結果を読み、規則はこの集計だけが持つ（[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)決定1）。

## 完了見込み

open なtaskとgoalの完了の時刻を、今の計画がそのまま流れたときのsimulationのp50 / p90で見込み、記録して後で答え合わせする（[ADR-0070](../../adr/0070-forecast-snapshots-and-scoring.md)）。

```text
history（着地したrunの標本） ─┐
graphの依存とclaimの順 ───────┼─▶ forecast（simulation）─▶ dagq forecast（記録しない）
走っているrunの段と経過 ──────┘                        └─▶ snapshot（forecast_recorded）─▶ score（完了の時点で答え合わせ）─▶ kpi
```

- 入口: `domain::forecast`（`forecast`・`history`・`in_flight`・`snapshot`・`score`）、入力の組み立ては`application::forecast`、snapshotの周回は`application::supervise::forecast`。
  答え合わせは[kpi](kpi.md#完了見込みの答え合わせ)、observerのfindingは[Observer](observer.md#完了見込みの誤差)。
- 1つの標本は着地した1つのrunで、work・validate・wait_to_landを同じrunから一緒に引く。
  resumeと着地の延期はその区間に含まれるので、別の確率として足さない。
- 流入（新しいtask・follow_up・差し戻し）と失敗したrunのretryは含めないので、見込みは早い側に寄る。
- 見込みに入らないtaskやgoal（draft・submitted、abandonedで閉じたgoal）を待つものは終わらず、p50 / p90の代わりに理由を出す。
- 乱数の種は今の時刻とqueueの最新のevent IDから決まり、同じ時点の同じqueueからは同じ見込みになる。
- snapshotはclaimしているsupervisorだけが周回で見て、きっかけ（`domain::forecast::snapshot::trigger`）があれば記録し、着地のきっかけはp50が動いた対象があるときだけ残す。
  最新の`forecast_recorded`がまだ最新のときだけ書くので、2つのsupervisorが同時に見ても1件になる。
  失敗はclaimと着地を止めず、KPIの記帳のeventなのでobserverを起こさない。
- 値の調整（試行の回数、動いたとみなす値）は、ADR-0070を置き換えずに定数を直す。
  答え合わせのKPIの目標（p50の誤差の比とp90の的中率）はruntimeに埋め込まず、plannerが`dagq.toml`の`[kpi.targets]`に書く。
- 答え合わせは`stats`の出力に足さず、[レポート](report.md)が誤差を載せる。

## hostの負荷

- 入口: `HostSummary`（`domain::host_metrics`）、記録は[hostの負荷の連続の記録](host-metrics.md)。
- 時間の窓の要約で、`--goal`では絞らず、累計の値は近い2行の差から率にする（累計が戻った組は数えない）。
- ファイルが読めなければ標本0と理由を出し、`stats`は失敗しない。

## 差し戻しの分類コードごとの集計

- 入口: `review_reasons`（[ADR-t947-1](../../adr/2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)決定5）、コードの一覧と記録の欄は[Review](review.md)と[Plan review](plan-review.md#差し戻しの分類コード)。
- 窓の中で記録されたverdictを数え、verdictの後の時間と答えは窓の後のeventも読む。
- plan reviewはjobのverdictでなく適用した判断で数える。
- 時間は主のコードにだけ付ける。

## 分類コードごとの集計（未実装）

[ADR-t947-4](../../adr/2026-09-28-t947-4-cancel-carries-a-reason-code.md)のcancelの理由の集計は、まだ実装していない（[Domain model](../domain-model.md#cancelの理由の分類コード未実装)）。
ADR-t947-1・t947-2・t947-3の分はこの文書の各節にある。

- 予定の形: 理由ごとの件数・actor・cancelまでの秒と、cancelまでに使ったrun・plan review・plannerの数。
- 分類コードの集計に共通の約束: 窓の中のeventだけから数え、コードの無い過去の記録は書き換えずに`unlabeled`（cancelは`unrecorded`）として数え、一覧に無い値はその値の行として出す。

## worker_questionの分類コードごとの集計

- 入口: `worker_question_topics`（[ADR-t947-2](../../adr/2026-09-28-t947-2-worker-questions-carry-topic-codes.md)決定4）、コードの一覧と記録の欄は[ask](ask.md#worker_questionの分類コード)。
- 率の分母は窓の中でclaimしたrunの数。
- 夜と昼はhostのlocal timeで分ける（`NIGHT_HOURS`）。
- runtimeが自分で閉じた答えは答えまでの時間に数えない。
- 答えの後の`failed`は、runの最後の状態が`failed`で、そこへ移ったのが答えの後のときだけ数える。
  一度`failed`で終わってからresumeされて着地したrunは着地だけに数える。

## follow_upの種類ごとの集計

- 入口: `follow_up_categories`（[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)決定4）。
  コードの一覧と記録の欄は[Receipt and session exit](receipt-and-session-exit.md#follow_upsの分類コード)、runtimeのplannerの判断との突き合わせは[Draft planners](draft-planners.md#follow_upの種類と判断の集計)。
- 窓と`--goal`の絞り込みは[draftの流入と流出](#draftの流入と流出)と同じ。
- 窓の中に何も起きなかった種類は出さない。
- 重複としてのcancelは、採用率と分けて重複率に数える。
- runtimeが自分で閉じた答えは数えない。

## headlessのjob

worker以外のheadlessのjobを種類・provider・実際のmodelごとに数え、ClaudeとCodexで動いたjobを比べる。

- 入口: `jobs`と`JobStats`（providerの記録は[Actor model](actor-model.md)）。
- 窓の中に終わりのeventが記録されたjobを数え、終わりは直前の同じ対応の開始と組にする。
- agentを起動しなかったもの（過去の記録に残る見直しのskippedの終わり、Codexも使えなかったobserver）はjobに数えない。
  開始の無い復旧の終わり（上限まで使い切って起動しなかった段上げ）も数えないので、`auto_repairs`の復旧jobより少なく出うる。
- providerの記録の無い開始は`claude`に数え、goal reviewは`--goal`でもそのgoalに数える。
- Claudeのjobは実際のmodelを終わりのeventに写さないので、開始の`session_id`で区間の閉じた記録に結び付ける。
  Codexのjobは終わりのeventのmodelを読む。
- スループットの見直しだけがmodeごとにも分ける（毎時と日次・週次では所要時間の桁が違い、費用を見たいのは毎時なので）。

## AIの推奨と確信度の集計

- 入口: `recommendations`と`DECIDED_WITHOUT_ASK`（[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定1・2）、askの欄と表示は[ask](ask.md)。
- askの推奨は窓より前に開いたaskからも読み、runtimeが自分で書いた答えは誰の選択でもないので除く。
- 選んだoptionは選択肢との完全一致で見て、`<option>: <理由>`の形で答える種類（plan と着地の承認）は`:`の前で見る。
- AIがaskにせず決めた判断のうち、適用した差し戻しが結局人へのaskになったものは人に届いた判断なので数えない（照合は窓に依らない）。
- `--goal`で絞ると、observerがaskにしなかった件数は0になり、`blocked`のaskとは比べられない。

## そのほかの期間の集計

| 知りたいこと | コードの入口 | 約束と文書 |
| --- | --- | --- |
| eventの分類コードの件数 | `reason_codes` | 同じ失敗を2回数えない。[domain-model](../domain-model.md#理由の分類コードcode) |
| cmuxの呼び出しの失敗 | `backend_failures` | retryした試行と使い切った失敗の和が件数で、alertは件数で判定する |
| 重複としてのcancel | `duplicate_cancels` | ADR-0063決定5 |
| 着地の後の待ちrunの再確認 | `landing_rechecks` | [Landing recheck](landing-recheck.md) |
| reviewの後に人を待った着地の衝突 | `landing_waits` | 着地が窓にあるrunを数え、待ちと衝突は着地より前の全てのeventから読む |
| 人の答えを待ってslotを空けたrun | `domain::waiting::WaitingStats` | 待ちの合計は、塞いでいたら失われたslotの時間。[人の答えを待つrun](waiting.md)、[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)決定13 |
| workerの経路ごとの健全性 | `routes` | 経路はそのeventの時点のrunの経路。[kpi](kpi.md#期間の健全性)と日次・週次のレポートが読む |
| runtimeのplannerの経路ごとの様子 | `planner_routes` | ADR-t1394-2決定4 |
| 衝突の多いファイル | `conflicts` | 着地の数とファイルの今の状態はmainの履歴から読み、読めなければ分からないものとする。alertは消えたファイルを除く（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)決定21、[衝突の閾値](conflict-thresholds.md)） |
| askの件数・答え・待ちの時間 | `asks`と`AskTimes` | 答えの適用はその後にaskを名指す最初のeventで、slotの空き待ちを除いた時間も並べる（ADR-0071決定8） |
| 人を待たずに直したもの | `auto_repairs` | 日はUTCの日で、復旧jobの終わりは直したものと別に数える（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定45） |
| providerの切り替え | `providers` | [provider-lifecycle](../provider-lifecycle.md) |
| 自動更新とe2eの関門 | `updates` | [Auto-update](auto-update.md)。testごとの関門の結果はobserverが`flaky_test`のfindingにする |

## KPIからの読み口

[kpi](kpi.md)（ADR-0051）は期間ごとの窓でこの`stats`を全件で呼び、同じ区間・着地の内訳・延期・sessionを使う。

- 同じ走査を共有する読み口: 人が答えたaskの答えまでと適用までの秒は`stats::asks::human_waits`、検証コマンドごとの秒は`stats::measures::verification_durations`。
- 着地の直列処理の使用率は、`stats`の時間の窓の代わりにKPIの期間で同じ関数を呼ぶ。
