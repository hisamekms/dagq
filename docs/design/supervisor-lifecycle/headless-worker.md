---
id: design-supervisor-lifecycle-headless-worker
type: design
title: "workerのsession（非対話のworker・session wrapper・receiptとsessionの終わり）"
status: current
created: 2026-09-28
scope: runtime
related:
  - adr-t1594-1
  - adr-t1433-2
  - adr-t1433-1
  - adr-t1394-2
  - adr-t1533-1
  - adr-0054
  - adr-t1433-3
  - adr-t1340-1
  - adr-t1404-1
  - design-supervisor-lifecycle
  - design-provider-lifecycle
  - adr-t1233-2
  - adr-t813-1
  - adr-t813-2
  - adr-t813-3
  - adr-0027
  - adr-0047
  - adr-0071
  - plan-headless-worker-spike
  - adr-0022
  - adr-t803-1
  - adr-t947-3
  - adr-t1504-2
  - adr-t1487-1
  - adr-t1818-1
---

# workerのsession（非対話のworker・session wrapper・receiptとsessionの終わり）

## 概念

### 目的

workerのrunを、1 turnを1回の非対話のagentの呼び出しにする経路だけで動かす（[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)）。
人は打ち込まず、supervisorは画面を読まず、対話の経路は無く（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）、session wrapperはworkspaceを持たないbackgroundのprocessとしてだけ動く（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)・[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
この文書は、session wrapper、turnの駆動と記録、receiptの受領とsessionの終わり、receiptの欄の約束の地図で、runtimeのplannerも同じturnの駆動で動く。

### 全体の流れ

```text
supervisor（provision / start_resume / reopen）
  → prepare_turns: limits.json を書き、前のsessionの残りの依頼を捨てる
  → launch_background: session wrapper を切り離して起動、handle を記録
session wrapper（session → Turns）
  → 自分の起動の記録を待って登録し、最初のturnを prompt.txt で始める
  → turnごと: provider の CLI を起動し、出力を読み、止める条件を見張る
  → turnの終わりを記録し、idle marker を書き、次の依頼を待つ（heartbeat は続く）
supervisor
  → idle marker・receipt・ask を読み、次の文（answer・revise・resume・催促）を turns/ の依頼に書く
  → 終わりは turns/exit、wrapper が残れば handle で止める
```

### 責務と境界

- providerの違いは`AgentProvider`に閉じ、supervisorとwrapperの流れはproviderを知らない。
  動くのはClaudeとCodex（Codexの固有の点は[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。
- supervisorは依頼と終了の依頼を書くだけで、turnを起動し記録するのはwrapperだけ。
  cloneのrunのturnはcontainerで起動する（[Container runtime](../container-runtime.md#workerの実行)）。
- wrapperは誰のturnかを`TurnOwner`（runかplanner）で持ち、違いはそこに閉じる。
- receiptの受領とwrapperの終わりは別の事象で、receiptはworkerが一時fileからrenameで公開し、turnの終わりはwrapperがidle markerで示す。
- e2eはworkerのturnで流さず、reviewのpassの後にruntimeがhostで流し、落ちれば同じsessionの次のturnとしてresumeを依頼する（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)、[着地の前のe2e](integrate.md#着地の前のe2e)）。
- workerへの文面は`/exit`・画面への打ち込み・backgroundの処理に頼らず、Codexはsubagent reviewをしない（[Prompt](prompt.md#経路とproviderごとの文面)）。
- 人への質問は`dagq ask`だけで、turnの設定はagentの質問の道具と、Claudeでは後のpromptを予約する道具も拒む。
  予約したwakeupは`claude -p`をturnの終わりの後も生かし、turnを終わらせないため（一覧と理由は`adapters.rs`の`PRINT_MODE_DENIED_TOOLS`のdoc comment）。

### 不変条件

- 1つのsessionの依頼は1つのwrapperだけが取り、1つずつturnにする。
- 依頼は一時fileからrenameで置き、その後で依頼を記録し、記録の失敗は依頼を取り消さない。
- turnの終わりの印はidle markerだけで、Claudeの`Stop` hookも画面からの推定も使わない。
  wrapperはturnの終わりの記録の後に書き、wrapperを失ったturnのmarkerだけはsupervisorが書く（下の「待ちの最中に失ったsessionの開き直し」）。
- wrapperは識別の組（pidと起動時刻）で名指し、起動時刻が合わないpidにsignalを送らない。
- run dirはworkerが書けるので、runtimeはrun dirの中のlinkを辿らず、通常のfileだけを上限付きで読む。
- 黙っただけのwrapperはkillしない（下の「黙ったwrapper」）。
- runの状態はsessionの終わりにreceiptの有無で決まり、receiptとwrapperの終わりをどの順で見ても同じになる（[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)）。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| wrapperの起動の待ち・登録・終わりの記録 | `src/application/session.rs`の`run_session` |
| wrapperのturnの駆動・止める判断・記録 | `src/application/headless_session.rs`の`Turns` |
| supervisorから見たsession・依頼の書き込み | `src/application/supervise/execution/headless.rs` |
| 終了の依頼と黙ったwrapper | `src/application/supervise/execution/exit.rs`の`ExitWatch`・`wrapper_pulse` |
| `turns/`のfileの名前・依頼・idle marker・turnのcostとトークン数 | `src/domain/turn.rs`の`TurnRequest` |
| Claudeのsessionの有無とtranscriptの置き場 | `src/infrastructure/transcripts.rs`の`ClaudeTranscripts` |
| Codexの呼び出しと出力の読み | `src/infrastructure/codex_turns.rs` |
| turnの設定（拒否の規則） | `src/infrastructure/adapters.rs`の`headless_worker_settings` |
| run dirのfileの境界 | `src/infrastructure/agent_dir.rs` |
| idle markerの読み | `Supervisor::session_idle` |
| backgroundの起動・停止・生死 | `src/infrastructure/background.rs`の`BackgroundWrappers`、`src/domain/background_wrapper.rs` |
| 停止の記録・残ったturnの停止 | `application::recording`の`RecordingSessions` |
| runtimeのplannerのturn | `src/application/supervise/planning/planner_turns.rs` |

## 経路の全体

- 最初のturnは`prompt.txt`でsessionを始め、後のturnはsupervisorの依頼を1つずつ取って同じsessionをresumeする。
  Claudeはsessionの名前をrunのidにし、Codexは出力で名乗ったthreadのidを記録してそれをresumeする。
- runtimeがsessionに渡すのは答え・revise・resume・催促・終了の依頼だけで、どれも`deliver::submit`が`turns/`に書く。

終了の依頼とwrapperの終了コードは`headless_session`のmodule docを参照する。

## session wrapper

起動・登録・turnのagent行・heartbeatと終了記録は`src/application/session.rs`、turnの環境とstepの送信は`headless_session::Turns`が持つ。
古いargvの入口と登録を断られたときの約束は`session.rs`のmodule docを参照する。

wrapper自身もqueueを直接開かずserviceを通り、agentの`dagq`はクライアントモードで動く（[Queue service](../queue-service.md#クライアントモード)）。
DB障害中も子processの所有を手放さず、起動後のエラーで子が生きている可能性があるときはwrapperの終わりを記録しない。
手元の検証の記録はworkerのturnだけに渡し、着地の検証とe2eには渡さない（[ADR-t2589-1](../../adr/2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)決定2）。

## run dirの`turns/`

fileの持ち主・依頼の順序と番号は`src/domain/turn.rs`、sessionの準備は`Supervisor::prepare_turns`、turnごとの設定は`Turns`が持つ。
run dirのI/Oの約束は`agent_dir`のmodule docと`agent_dir::in_run_dir`が持つ。

FIFOのopenで待たず、書き込みは新しい一時fileからrenameする。
run dirかどうかはpathだけで決め、hostのpathは`std::fs`と同じくlinkを辿るので、任意の深さのpathを安全にするAPIではない。
読めない・書けないfileは既存の失敗の経路に乗るが、診断のlogとturnのコマンドのfileが書けなくてもturnは続く。

## turnの記録

- turnのeventの欄の意味は`EventKind`と書き手（`headless_session.rs`）のそばにあり、依頼の記録は送った文として数えるので、引き継ぎやadoptの後の見張りは走っているturnを促さない。
- runのsessionの区間の稼働時間とトークン数はturnの終わりの記録から取り（[provider-lifecycle](../provider-lifecycle.md#非対話のworkerの区間)）、数え方は`domain::turn`と[Executionのトークン数](../execution-tokens.md)。
  `result`の前に止められたturnの分は次のresumeのturnに入り、過去の記録は補正しない。
- 次のturnをresumeするか始め直すかは、sessionが作られたか、providerがsessionを持っているか（`turn_session_exists`）で決める。
  Claude Codeは使われている名前のsessionを拒むので、答える前に失敗したturnが残したsessionもresumeする。
  containerで動くrunのsessionは、hostに無ければrun dirのagentのhomeのtranscriptで探す（[Container runtime](../container-runtime.md#workerの実行)）。
- containerのturnのメモリの待ち（[Container runtime](../container-runtime.md#dagq専用のmachine)）は`turn_started`の前で、稼働時間・沈黙・上限に数えない。

## Claudeの呼び出しと出力（`ClaudeCode`・`ClaudeTurnReader`）

- 呼び出しの組み立ては`ClaudeCode::turn_command`、出力の形と読みは`src/infrastructure/claude_turns.rs`のmodule docが持ち、turnのprocessは端末を持たず自分のprocess groupで走る。
- 失敗の分類（`domain::turn::TurnFailure`）は読み手が出力から決め、overageで賄っている利用上限は止まりとみなさない。

## wrapperがturnを止めるとき

止める条件と終わり方ごとの停止範囲は`headless_session.rs`のmodule docと`stop_turn`、閾値は`[stall]`が持つ。
頼んだpermission modeで始まらないのを止めるのは、modelによっては黙って別のmodeになるため（ADR-t813-1決定8）。
ClaudeもCodexもtoolのコマンドを別のprocess groupで走らせるので、groupへのsignalだけでは届かず、子孫をgroupを止める前にpidで集めて止める。

## turnの後の扱い

receiptとaskの有無に応じた促し・待ち・復旧の判断は`StallWatch::observe_turn`、wrapperの終了とreceiptの関係は`domain::run::session_end_status`が持つ。
receiptを受けたsupervisorはsessionを開いたまま[validation](validation.md)とreviewに進める。

落とし穴: receiptが無いことはidle markerを読んだ後にもう一度確かめる。
turnの終わり際に書かれたreceiptを促すと、同じ作業がやり直されてvalidationの最中にcommitが進み、runが失敗するため。
providerが使えないときは促さず、[providerの切り替え](../provider-lifecycle.md#使えないproviderからの切り替え)か[queue hold](queue-hold.md)で待つ（[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)）。
答えずに閉じた質問への通知は[runの復旧の「receiptの無いturn」](background-recovery-job.md#receiptの無いturn)を参照する。

## receiptとsessionの終わり

idle markerの読みは`Supervisor::session_idle`、終了の依頼は`ExitWatch::poll`、残ったwrapperの停止は`stop_run_session`が持つ（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3）。
画面からの推定（[ADR-t803-1](../../adr/2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)、superseded）の記録は読めるまま新しくは書かれない。
backgroundより前にworkspaceで開いたsessionのIDは閉じず、人に任せる。
workerにもplannerにも打ち込まず、互換の`[exit]`の意味は`domain::exit::ExitConfig`が持つ。

### 黙ったwrapper<a id="黙ったwrapper"></a>

沈黙の記録と回復の扱いは`wrapper_pulse`と`note_heartbeat_expired`、終了記録の再試行は`session::EXIT_RECORD_ATTEMPTS`が持つ。
終了を記録せず居なくなれば失ったsessionとして復旧し、生死は記録したhandleで判じる（下の「識別と生死」）。
落とし穴: wrapperのheartbeatと記録の時刻は実時計で書かれるので、wrapperの居る`supervise`のtestで固定時刻の`Clock`を使うと生死を誤る（`wrapper_pulse`のdoc comment）。

### workerへの答えの配送<a id="workerへの答えの配送"></a>

`worker_question`の答えも次のturnの依頼として届く（[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定2）。
配送の時点・記録とclose・閉じたaskの扱いは`SessionWatch::deliver_answers`と`domain::session_takes_answers`が持つ。
依頼を書いてからcloseまでの間にsupervisorが死んだときだけ、引き継いだsupervisorが同じ答えをもう一度送りうる。
turnの後に開くaskもあるため、turnの間の配送ではmarkerとaskの時刻を比べない。

## receiptの欄の約束

形・分類・所属の提案・Spike・baseが満たすときの約束は[workerのreceipt](worker-receipt.md)が持つ。

## 待ちの最中に失ったsessionの開き直し

非対話のrunは待ちのあいだagentのprocessを持たず、session・worktree・開いたaskはwrapperが死んでも残るので、[待ち](waiting.md)の最中にwrapperを失ったrunはruntimeが開き直して待ちを続ける（ADR-0047の第1層、`src/application/supervise/execution/reopen.rs`のmodule docと定数）。

対象と上限・試みの状態の持ち方は`reopen.rs`のmodule docが持つ。

- 最後のturnのprocessが生きていれば2つ目のsessionを開かず、失ったwrapperが動いていれば先に止め、止められなければ新しいwrapperを起動しない（同じ依頼を2つのwrapperが取らない）。
- wrapperがturnの終わりを見ずに失われたら、supervisorがそのturnのidle markerを書き、開き直したsessionが答えを次のturnで受けられるようにする。

## 復旧jobのalertと操作（決定9）

- workerの生きているrunの復旧が扱うのは`stalled`と`idle_process`で、操作（[復旧job](background-recovery-job.md)）は適用の時に次の依頼が無くturnが終わっていること、runのprocessであることを確かめる。
- 画面に由来するalertと画面に打つ操作は無く、過去の`intervene`の答えはaskを開き直して答えを求める（`stall.rs`の`INTERVENE_OPTION`）。

## 対話と記録されたtaskのclaimとresume

経路の拒否の入口は`domain::worker::refuse_interactive`（[Provider lifecycle](../provider-lifecycle.md#workerのproviderと経路)）。
`interactive`と記録されたtaskとrunは読めるが、claimとresumeが実際のrunを非対話にする互換の約束は`TaskRun::worker_mode`のdoc commentが持つ。

## workspaceなしのbackgroundのwrapper

workerのsession wrapperは、supervisorから切り離したprocess（自分のsessionとprocess group、親は1）としてだけ動き、最初のsession・resume・開き直しのどれも同じ（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)・[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。

- **設定**（ADR-t1433-3決定2）: `[headless] wrapper`は読んで検査するが、workerにもruntimeのplannerにも効かず、`"workspace"`ならsupervisorはprocessごとに1回だけ警告する。
  拒まないのは、固定バイナリの入れ替えの順で本番の`dagq.toml`が読めなくならないため。

起動と環境の境界は`BackgroundWrappers::launch`、handleの記録は`Supervisor::record_launch`、wrapperの入口は`compose::wrapper_entry`が持つ。
logはworkerが書けるrun dirにあるのでshellにpathを開かせず、supervisorが新しいinodeで作った記述子を渡す。

- **識別と生死**<a id="識別と生死"></a>: 生きているとは、handleのpidが記録した起動時刻のまま居ること（`Supervisor::wrapper_lives`）。
  adopt・引き継ぎ（[Handoff](handoff.md)）・開き直しは記録したhandleで行うので、supervisorが止まってもwrapperとturnは動き続ける。
  heartbeatの古さは生死に使わず（上の「黙ったwrapper」）、`stats`もこの生死だけで判じる（[Stats](commands.md)）。

停止のsignalと残ったturnは`BackgroundWrappers::stop`と`RecordingSessions`、**停止の記録**は`StopRoute`、**出力**は`application::session_log`のmodule docが持つ。
ADR-t813-1決定5の「最後のturnの終わりにworkspaceをcloseする」は、wrapperの終わりを確かめて残りを止めることと読む。
停止の記録が書けなくても停止の結果は変えず、logが書けなくてもsessionは止めない。
SIGTERMで終わったかSIGKILLまで要ったかを[評価](../../plans/headless-background-evaluation.md)の「processの残り」が数える。

- **画面と片付けのCLIの拒否**（ADR-t1433-3決定3・4）: `run screen`は`run log`と`turns/`の場所を示して拒み、`run close-workspaces`は権限の検査の後に拒む。

## 再計画の保留

保留の柵を越えたrunには次のturnを送らず、wrapperもturnを始める前ごとに柵を見て終わり、柵の後のreceiptは受理せず、上限を過ぎた書き手は記録したhandleとturnだけを止める（`replan_park.rs`、[taskの再計画](../../plans/task-replanning.md#実装の状態)）。

taskのholdも同じ柵の理由の1つで、別の止め方を持たない（[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)決定7）。
holdの待ちとreceiptの検証の範囲は`replan_park.rs`のmodule docを参照する。
解除の後の続け方は[needs_session](needs-session.md#taskのhold)が、書き手の終わりの確かめ方と、leaseの返却を書き手の終わりと数えないことは`replan_park.rs`のmodule docが持つ。

## 非対話のruntimeのplanner<a id="非対話のruntimeのplanner"></a>

runtimeのplannerは経路の選択なしに非対話で立ち、この文書のturnの駆動をplannerのディレクトリで使う（[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定2、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3）。
依頼・状態・時間切れ・使えないとき・区間は[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)が持つ。

持ち主ごとの違いは`TurnOwner::Planner`が持ち、providerはClaudeだけで切り替えない（ADR-t1394-2決定5）。

- 落とし穴: 依頼の前に入力の印を書くので、壁で失敗したturnの後に印だけが新しくなると、plannerは`working`に見えたまま誰も壁を見ない。
  wrapperはそのとき壁のturnのidle markerを書き直す。
- 生死と掃除はworkerと同じくhandleの識別だけで判じ、heartbeatが古くても`lost`にせず行を閉じない。

**起動**とtelemetryのtokenの寿命は`planner::launch_planner`、最初のturnの前のhandle待ちは`planner::run_planner_session`、残ったturnの停止は`RecordingSessions`が持つ（[Queue service](../queue-service.md#principalとtoken)）。

- **CLI**（[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)）: `planner screen`は画面を読まずに`turns/`の場所を返し、`planner send`は拒む（[続きの依頼と非対話のplannerのCLI](plan-planners.md#続きの依頼と非対話のplannerのcli)）。
