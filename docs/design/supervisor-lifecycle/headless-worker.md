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
- 終了はどれも終了の依頼になり（reviewのpassの後、resumeの終わり、黙ったwrapper、`stalled`のaskへの`stop`）、wrapperは走っているturnを止めてexit code 0で終わる。

## session wrapper

runtimeがbackgroundで起動する隠しコマンド（`session`）で、入口の約束（古いargvの入口、登録を断られたとき）は`src/application/session.rs`のmodule docが持つ。
wrapperは常に`Turns`で動く（下の「対話と記録されたtaskのclaimとresume」）。

- wrapperは自分の起動の記録を待ってからPIDを一度だけ登録し、leaseが無効なら登録できない。
- 最初のturnのprocessをrunの`agent`として登録し、後のturnは同じ行のpidを差し替える（`register_turn_agent`）。
  runの状態遷移は最初のturnだけで、turnの間は行が終わったturnのpidのまま残る。
- turnのprocessをwaitする間はheartbeatを更新し、DB障害中も子processの所有を手放さない。
  turnの起動後のエラーでは子processが生きている可能性があるので、wrapperの終わりを記録しない。
- **turnの環境**: workerのactorの変数に、queue serviceのsocketと、supervisorがclaimかresumeで発行したworkerのtokenのfileを足したもので、agentの`dagq`はクライアントモードで動く（[Queue service](../queue-service.md#クライアントモード)）。
  wrapper自身はqueueを開かず自分のtokenでserviceを通る（[Queue service](../queue-service.md#principalとtoken)）。
- wrapperは各turnの前にrun dirの手元の検証の記録のdirを作り、そのpathもturnの環境で渡す（containerではrun dirを同じpathにmountするので同じ値）。
  `integrate`の検証・landing recheck・e2eには渡さない（[ADR-t2589-1](../../adr/2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)決定2）。
- **stepの送信**: wrapperはturnの出力をproviderの`StepReader`で読み、runかplannerのtelemetryのtokenを自分で読んで送る（`step_report`、[計測](../measurement.md#送る口)）。

## run dirの`turns/`

- 名前は`src/domain/turn.rs`が決める。
  supervisorが書くのは依頼・終了の依頼・`limits.json`（`[stall]`の値）、wrapperが書くのは取った印・turnの出力・Codexのturnのコマンドのfileで、runtimeのplannerには`planner request`のCLIも依頼を書く。
- wrapperは取られていない依頼を番号の順に1つずつ取り、renameで取った印を付け、番号は取られたものと捨てたものを含めて数えて使い回さない。
- 前のsessionが取らずに終わった依頼は、新しいsessionの前に`prepare_turns`が捨てる。
- 壁の後に`provider retry`を先に取るのはruntimeのplannerのwrapperだけ（`request_to_take`）。
- turnの設定（`claude-headless-settings.json`）はturnのたびに書かれ、plannerのturnも同じ設定で、拒否はactorのroleのもの。

run dirのI/Oの約束と落とし穴（`agent_dir`のmodule docと`agent_dir::in_run_dir`）:

- FIFOのopenで待たず、書き込みは新しい一時fileからrenameする。
- run dirかどうかはpathだけで決め、hostのpathは`std::fs`と同じくlinkを辿り、任意の深さのpathを安全にするAPIではない。
- 読めない・書けないrun dirのfileは既存の失敗の経路に乗り、診断のlogとturnのコマンドのfileが書けないときはwarnだけでturnは続く。

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

- 止める条件は、出力の途絶え（heartbeatのあるproviderだけ）、時間の上限、頼んだpermission modeで始まらない、providerが使えない、終了の依頼で、閾値は`[stall]`。
- 頼んだmodeで始まらないのを止めるのは、modelによっては黙って別のmodeになるため（ADR-t813-1決定8）。
- 止め方は`headless_session.rs`の`stop_turn`が持ち、ClaudeもCodexもtoolのコマンドを別のprocess groupで走らせるので、groupへのsignalだけでは届かず、子孫をgroupを止める前にpidで集めて止める。
- turnが自分で終わったときは、turnのgroupに残ったものだけを止め、groupの外に切り離されたもの（理由はその判断のそばのコメント）は復旧jobの`stop_processes`がworktreeで判じて止める。
- wrapperがsignalで終わるときは、signal handlerが子孫を集められないので、turnのgroupだけを止める（`stop_groups_on_exit_signals`）。

## turnの後の扱い

- 成功（receiptかaskが残った）: wrapperは次の依頼を待つ。
  supervisorはreceiptより新しいidle markerを見ると、sessionを開いたまま[validation](validation.md)とreviewに進み、`worker_question`が開いていればslotを空けて答えを待つ（ADR-0071の読み替え）。
- Codexのworkerの`dagq ask`もsandboxの中からqueue serviceでその場で開く（[Queue service](../queue-service.md#クライアントモード)）。
- receiptもaskも無いturnの終わり: 待たずに決まった文で促し、上限を使い切れば復旧jobの`stalled`にする（`StallWatch::observe_turn`、ADR-0047決定30の読み替え）。
  拒否が続いたturnは促さずに`stalled`にする。
  落とし穴: receiptが無いことはidle markerを読んだ後にもう一度確かめる。
  turnの終わり際に書かれたreceiptを促すと、同じ作業がやり直されてvalidationの最中にcommitが進み、runが失敗するため。
- providerが使えない（`TurnFailure::at_provider_wall`）: wrapperはsessionを終えず、supervisorは促さずにもう一方のproviderへ切り替える（[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)、[provider-lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。
  切り替えられないときはrunを失敗にせず待たせ（[queue hold](queue-hold.md)）、控えが解ければ同じsessionへ`provider retry`を送り、reviseとresumeの段でも同じ。
- turnの失敗とwrapperが止めたturn: wrapperはexit code 1で終わり、supervisorは促さずに終わりを待つ。
  wrapperの終わりをidle markerより先に見たときは、その時点でreceiptの有無を読み、あれば終了コードによらずvalidationへ進み、無く非0なら`failed`で[triage](triage.md)の復旧jobにかかる（`session_end_status`）。
- 答えずに閉じた`worker_question`: 促しの代わりに閉じたことを伝える文を1回だけ送る（[runの復旧の「receiptの無いturn」](background-recovery-job.md#receiptの無いturn)）。

## receiptとsessionの終わり

- idle markerは最後の入力より新しいときだけturnの終わりを示し（stallは依頼のturnも待つ）、画面からの推定（[ADR-t803-1](../../adr/2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)、superseded）の記録は読めるまま新しくは書かれない。
- verdictが終了を求めるときは、終了の依頼を記録してから`turns/exit`に書く（`ExitWatch::poll`）。
  終わった後はwrapperの残りをhandleで止める（`stop_run_session`、[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3）。
  backgroundより前にworkspaceで開いたsessionのIDは閉じず、人に任せる。
- workerに`/exit`を打たず、打鍵の再試行・ダイアログの応答・終了待ちの時間切れのaskと復旧jobは無い（`[exit]`の表は読んで検査するだけで、意味は`domain::exit::ExitConfig`）。
  plannerにも何も打たない。

### 黙ったwrapper<a id="黙ったwrapper"></a>

- 沈黙は生死とwatchに依らず1回記録し（`note_heartbeat_expired`）、生きていればkillせず終了の依頼を書き、heartbeatが戻れば印を消す（`wrapper_pulse`）。
- 終了を記録せず居なくなれば失ったsessionとして復旧し、生死は記録したhandleで判じる（下の「識別と生死」）。
- wrapperは終わりの記録を再試行し、その合計の待ちはsupervisorが黙ったとみなすまでの時間に収める（`EXIT_RECORD_ATTEMPTS`）。
- 落とし穴: wrapperのheartbeatと記録の時刻は実時計で書かれるので、wrapperの居る`supervise`のtestで固定時刻の`Clock`を使うと生死を誤る（`wrapper_pulse`のdoc comment）。

### workerへの答えの配送<a id="workerへの答えの配送"></a>

`worker_question`の答えも次のturnの依頼として`turns/`に書く（[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定2）。

- 答えはturnの間（最後に始まったturnの終わりをidle markerが示すとき）に送り、そのときはmarkerとaskの時刻を比べない。
  turnの後に開くask（queue serviceより前に起動したCodexのturnのask）があり、markerがaskより古くなりうるため。
- 依頼を書けたら配送とaskのcloseを同じtransactionで記録し、書けなければ失敗だけを記録してaskを閉じず、再送しない（inboxのattentionになる）。
  書いてからcloseまでの間にsupervisorが死んだときだけ、引き継いだsupervisorが同じ答えをもう一度送りうる。
- runtimeが届けるrunへの答え（`domain::session_takes_answers`）はattentionにせず、届ける者の居ない答えは、答えをworkerに送ってaskを閉じるattentionになる。
- 配送の前に閉じた`worker_question`の答えは送らず、閉じたことを1回だけ伝える。

## receiptの欄の約束

receiptの形と受理の検査は`Receipt`のdoc commentと[Validation](validation.md)、workerへの書き方の文面は[Prompt](prompt.md)が持つ。

### follow_upsの分類コード<a id="follow_upsの分類コード"></a>

一覧と定義は`FOLLOW_UP_CATEGORIES`（`src/domain/follow_up.rs`）だけが持ち、workerのpromptはそこから出す（[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)）。
runのreview（[Review](review.md#差し戻しの分類コード)）と同じ種類の問題には同じ名前を使う。

- 分類の無い要素と一覧に無い値もreceiptの受理を変えず、runtimeは値をそのまま記録する。
- 記録は`integrate`のfollow_upの登録（[integrate](integrate.md#follow_upsの登録)）がdraftの出どころの材料に載せ、runtimeのplannerとの突き合わせは[Draft planners](draft-planners.md#follow_upの種類と判断の集計)。
- 迷ったら、follow_upを片付けたときに何が変わるかで選び、重複や採否はコードにしない（ADR-t947-3決定2）。

### follow_upsの所属の提案<a id="follow_upsの所属の提案"></a>

workerはfollow_upごとに元goalのacceptanceとの関係を提案として書けるが確定はせず、所属の判断を記録するのはruntimeのplannerと人（[ADR-t1504-2](../../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定11、[所属の判断](../follow-up-membership.md)）。

- 欄の意味は`FOLLOW_UP_PROPOSAL`が持ち、欄の無い要素も形の違う値もreceiptの受理を変えない。
- `integrate`の登録は提案を書いたまま材料に載せ、runtimeは提案を判断として扱わず、plannerがそれを起点に判断する（[Draft planners](draft-planners.md#所属の判断)）。

### Spikeの結果<a id="spikeの結果"></a>

実行区分（`domain::execution_class`）が`spike`のtaskのrunは、結果をreceiptの`spike_result`に書き、欄の意味はそのdoc commentが持つ（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定3・4）。

- 検査するのはSpikeのrunの欄がそろっていることだけで、根拠が判定を支えるかは判じず、否定的な結果と未解決は成功のreceiptで返す。
- 上限（時間・回数）はagentへの指示で、runtimeは強制しない。
- 欠けは要求evidenceと同じく`needs_session`にしてresumeが書き足させる（[Validation](validation.md)）。

### baseがすでに満たすとき（already_satisfied）<a id="baseがすでに満たすときalready_satisfied"></a>

workerはbaseが受け入れ条件をすでに満たし差分が要らないとき、commitせずにreceiptの`result`を`already_satisfied`にする（[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)決定1）。

- 根拠の無い主張とcommitしたrunの主張は受け付けずrunを`failed`にする。
- 根拠は[local-checks](../../development/local-checks.md)の「workerの手元の検証」で許された確かめだけで、receiptの検査はコマンドの中身を判じず、verifyの全体はruntimeがsnapshotで流す（[Validation](validation.md)の「already_satisfiedの検証」）。

## 待ちの最中に失ったsessionの開き直し

非対話のrunは待ちのあいだagentのprocessを持たず、session・worktree・開いたaskはwrapperが死んでも残るので、[待ち](waiting.md)の最中にwrapperを失ったrunはruntimeが開き直して待ちを続ける（ADR-0047の第1層、`src/application/supervise/execution/reopen.rs`のmodule docと定数）。

- 対象は最初のsessionの待ちで、receiptが無く、閉じていない`worker_question`か`stalled`のaskがあるrunだけで、上限を超えたら終了したsessionと同じ判定（ADR-t1594-1）で進める。
- 最後のturnのprocessが生きていれば2つ目のsessionを開かず、失ったwrapperが動いていれば先に止め、止められなければ新しいwrapperを起動しない（同じ依頼を2つのwrapperが取らない）。
- wrapperがturnの終わりを見ずに失われたら、supervisorがそのturnのidle markerを書き、開き直したsessionが答えを次のturnで受けられるようにする。
- 試みの状態はsupervisorのprocessの中だけにあり、引き継ぎの後のprocessは前の試みのwrapperが起動中かもしれないので登録の待ちの時間を待つ。

## 復旧jobのalertと操作（決定9）

- workerの生きているrunの復旧が扱うのは`stalled`と`idle_process`で、操作（[復旧job](background-recovery-job.md)）は適用の時に次の依頼が無くturnが終わっていること、runのprocessであることを確かめる。
- 画面に由来するalertと画面に打つ操作は無く、過去の`intervene`の答えはaskを開き直して答えを求める（`stall.rs`の`INTERVENE_OPTION`）。

## 対話と記録されたtaskのclaimとresume

- 新しく対話を選ぶ経路は無く、`add` / `edit`で対話を選ぶと代わり（`run log`と`answer`）を示して拒む（[Provider lifecycle](../provider-lifecycle.md#workerのproviderと経路)）。
- `interactive`と記録されたtaskとrunは読めるが、claimとresumeが実際のrunを`headless`にし、変えたときだけそれを記録する。

## workspaceなしのbackgroundのwrapper

workerのsession wrapperは、supervisorから切り離したprocess（自分のsessionとprocess group、親は1）としてだけ動き、最初のsession・resume・開き直しのどれも同じ（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)・[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。

- **設定**（ADR-t1433-3決定2）: `[headless] wrapper`は読んで検査するが、workerにもruntimeのplannerにも効かず、`"workspace"`ならsupervisorはprocessごとに1回だけ警告する。
  拒まないのは、固定バイナリの入れ替えの順で本番の`dagq.toml`が読めなくならないため。
- **起動**: `SessionWrappers::launch_background`が起動し、logはworkerが書けるrun dirの中なので、shellにpathを開かせず、supervisorが新しいinodeで作った記述子を渡す。
  起動の直後に起動時刻を読めなければ起動の失敗で、backendの呼び出しの失敗に残る（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)決定9の読み替え）。
- **handleと記録**: 識別は`BackgroundHandle`（pidと起動時刻）で、runはこれをworkspaceのIDの代わりに記録し、続けてwrapperの起動を記録し、記録できなければwrapperを止めてエラーにする。
  sessionが開いているかはprocessで判じ、backgroundより前にworkspaceで開いたsessionのIDは開いていないとみなし、閉じない（人が自分のterminalで閉じる）。
- **入口**: backgroundの入口は端末を確かめず、自分で新しいsessionとprocess groupを持つ（`compose::wrapper_entry`）。
  同じpidで別の起動時刻の記録は、死んだwrapperのpidを継いだ別のprocessのものなので自分とみなさない。
- **env**: workerのactorの変数と`[run.env]`（[Run environment](run-environment.md)）はwrapperのprocessのenvで渡し、supervisorのenvの端末のpaneの変数（`background::TERMINAL_PANE_PREFIXES`）と`DAGQ_*`は外してから足す。
- **識別と生死**<a id="識別と生死"></a>: 生きているとは、handleのpidが記録した起動時刻のまま居ること（`Supervisor::wrapper_lives`）。
  adopt・引き継ぎ（[Handoff](handoff.md)）・開き直しは記録したhandleで行うので、supervisorが止まってもwrapperとturnは動き続ける。
  heartbeatの古さは生死に使わず（上の「黙ったwrapper」）、`stats`もこの生死だけで判じる（[Stats](commands.md)）。
- **停止**: runtimeはまず終了の依頼で終わらせ、残っていればhandleの`stop_background`でwrapperにSIGTERMを、猶予の後にgroupと控えた子孫にSIGKILLを送る（ADR-t1404-1決定3）。
  wrapperが先に死んで残したturnは最後のturnの始まりの記録から止め、それが生きている間sessionは開いているとみなされる。
  ADR-t813-1決定5の「最後のturnの終わりにworkspaceをcloseする」は、wrapperの終わりを確かめて残りを止めることと読む。
- **停止の記録**: 止めるたびに止めた経路（`StopRoute`）とsignalを記録し、記録できなくても停止の結果は変えない。
  SIGTERMで終わったかSIGKILLまで要ったかを[評価](../../plans/headless-background-evaluation.md)の「processの残り」が数えるため。
  復旧jobの`stop_processes`はwrapperでなくpidを止めるので、この記録をしない。
- **出力**（ADR-t1404-1決定6）: wrapperの`[dagq]`の要約は`session.log`に追記され、書けない行は捨ててsessionは止めず、人は`run log`と`planner log`で読む（`application::session_log`のmodule doc）。
- **画面と片付けのCLIの拒否**（ADR-t1433-3決定3・4）: `run screen`は`run log`と`turns/`の場所を示して拒み、`run close-workspaces`は権限の検査の後に拒む。

## 再計画の保留

保留の柵を越えたrunには次のturnを送らず、wrapperもturnを始める前ごとに柵を見て終わり、柵の後のreceiptは受理せず、上限を過ぎた書き手は記録したhandleとturnだけを止める（`replan_park.rs`、[taskの再計画](../../plans/task-replanning.md#実装の状態)）。

taskのholdも同じ柵の理由の1つで、別の止め方を持たない（[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)決定7）。
走っていたturnの終わりを待ってsessionを閉じ、snapshotを取らずworktreeをそのままに`needs_session`で待たせ、そのturnのreceiptは検証するが次の工程は始めない。
解除の後の続け方は[needs_session](needs-session.md#taskのhold)が、書き手の終わりの確かめ方と、leaseの返却を書き手の終わりと数えないことは`replan_park.rs`のmodule docが持つ。

## 非対話のruntimeのplanner<a id="非対話のruntimeのplanner"></a>

runtimeのplannerは経路の選択なしに非対話で立ち、この文書のturnの駆動をplannerのディレクトリで使う（[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定2、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3）。
依頼・状態・時間切れ・使えないとき・区間は[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)が持つ。

- **持ち主ごとの違い**（`TurnOwner::Planner`）: `turns/`とidle markerはplannerのディレクトリ、作業ディレクトリはrepositoryのcheckout、turnのeventはqueueのeventで、providerはClaudeだけで切り替えない（ADR-t1394-2決定5）。
- 落とし穴: 依頼の前に入力の印を書くので、壁で失敗したturnの後に印だけが新しくなると、plannerは`working`に見えたまま誰も壁を見ない。
  wrapperはそのとき壁のturnのidle markerを書き直す。
- 生死と掃除はworkerと同じくhandleの識別だけで判じ、heartbeatが古くても`lost`にせず行を閉じない。
- **起動**: `launch_planner`がbackgroundで立ててhandleをplannerの行に記録し、`dagq submit`はplannerの記録からhandleを読んでproposalの持ち主にするので、wrapperは最初のturnの前にhandleが記録されるのを待つ。
- **telemetryのtoken**: `launch_planner`がwrapperの起動の前にplannerを対象として発行し、ファイルはplannerのディレクトリの外で、wrapperとturnのenv・argvに載せない。
  `planner_closed`の後はserviceが断り、見回りが失効させる（[Queue service](../queue-service.md#principalとtoken)）。
- 殺されたwrapperが残したturnはplannerの最後のturnの始まりの記録から止める（ADR-t1404-1決定3・8）。
- **CLI**（[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)）: `planner screen`は画面を読まずに`turns/`の場所を返し、`planner send`は拒む（[続きの依頼と非対話のplannerのCLI](plan-planners.md#続きの依頼と非対話のplannerのcli)）。
