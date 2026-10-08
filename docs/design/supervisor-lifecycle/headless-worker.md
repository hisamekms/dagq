---
id: design-supervisor-lifecycle-headless-worker
type: design
title: "非対話のworker"
status: current
created: 2026-09-28
scope: runtime
related:
  - design-supervisor-lifecycle-task-replanning
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
---

# 非対話のworker

## 概念

### 目的

workerのrunを、1 turnを1回の非対話のagentの呼び出しにする経路だけで動かす（[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)）。
人は打ち込まず、supervisorは画面を読まない。
対話の経路は廃止し（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）、session wrapperはworkspaceを持たないbackgroundのprocessとしてだけ動く（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)・[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
runtimeのplannerも同じturnの駆動で動く（下の「非対話のruntimeのplanner」）。

### 全体の流れ

```text
supervisor（provision / start_resume / reopen）
  → prepare_turns: limits.json を書き、前のsessionの exit と取られていない依頼を捨てる
  → launch_background: session wrapper（--background）を切り離して起動、handle を記録
session wrapper（Turns）
  → 最初のturn: prompt.txt で session を始める
  → turnごと: provider の CLI を起動し、出力を TurnReader で読み、止める条件を見張る
  → turn_finished を記録し、idle marker を書き、次の依頼を待つ（heartbeat は続く）
supervisor
  → idle marker・receipt・ask を読み、次の文（answer・revise・resume・催促）を turns/ の依頼に書く
  → 終わりは turns/exit、wrapper が残れば handle で止める（wrapper_stopped を記録）
```

### 責務と境界

- providerの違いは`AgentProvider`の`turn_command`・`turn_reader`・`turn_permission_mode`に閉じ、supervisorとwrapperの流れはproviderを知らない。
  動くのはClaudeとCodex（Codexの固有の点は[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。
- supervisorは依頼と終了の依頼を書くだけで、turnを起動しない。
  turnを起動し、記録するのはwrapperだけ。
  wrapperが死んで残したturnと、turnの外に切り離されたprocessは、supervisorの停止と復旧jobが止める。
- wrapperは誰のturnかを`TurnOwner`（runかplanner）で持ち、違いはそこに閉じる。
- e2eはworkerのturnで流さず、要るrunにはreviewのpassの後にruntimeがhostで流す（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)、[着地の前のe2e](landing-e2e.md)）。
  落ちれば同じsessionの次のturnとしてresumeを依頼する。
- 人への質問は`dagq ask`だけで、turnの設定はagentの質問の道具を拒む。
- Claudeのturn（workerとruntimeのplanner）の設定は、後のpromptを予約する道具も拒む。
  予約したwakeupは`claude -p`をturnの終わりの後も生かし、turnを終わらせないため（一覧と理由は`adapters.rs`の`PRINT_MODE_DENIED_TOOLS`のdoc comment）。
- runtimeはrunのためにcmuxのworkspaceもgroupも作らない（inboxは対象外、[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。

### 不変条件

- 1つのsessionの依頼は1つのwrapperだけが取り、1つずつturnにする（順は下の「run dirの`turns/`」）。
- 依頼は一時fileからrenameで置き、その後で`turn_requested`を記録する。
  記録の失敗は依頼を取り消さない。
- turnの終わりの印はidle markerで、Claudeの`Stop` hookは使わない。
  wrapperは`turn_finished`の記録の後に書き、wrapperを失ったturnのmarkerだけはsupervisorが書く（下の「待ちの最中に失ったsessionの開き直し」）。
- wrapperは識別の組（pidと起動時刻）で名指し、起動時刻が合わないpidにsignalを送らない。
- run dirはworkerが書けるので、runtimeはrun dirの中のlinkを辿らず、通常のfileだけを上限付きで読む。
- 黙っただけのwrapperはkillしない（[wrapperが黙ったsession](silent-wrapper.md)）。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| wrapperのturnの駆動・止める判断・記録 | `src/application/headless_session.rs`の`Turns` |
| supervisorから見たsession・依頼の書き込み | `src/application/supervise/headless.rs`（`request_turn`・`prepare_turns`・`send_to_planner`） |
| `turns/`のfileの名前・依頼・idle marker・turnのcostとトークン数 | `src/domain/turn.rs`（`TurnRequest`・`idle_marker`・`turn_own_cost`・`turn_own_models`・`request_to_take`・`request_read`） |
| Claudeの呼び出しと出力の読み | `ClaudeCode::turn_command`、`src/infrastructure/claude_turns.rs` |
| Codexの呼び出しと出力の読み | `src/infrastructure/codex_turns.rs` |
| turnの設定（拒否の規則） | `src/infrastructure/adapters.rs`の`headless_worker_settings`・`headless_required_settings`・`HEADLESS_DENIED_TOOLS`・`PRINT_MODE_DENIED_TOOLS` |
| run dirのfileの境界 | `src/infrastructure/agent_dir.rs`（`in_run_dir`）、`LocalRunFiles` |
| receiptの無いturnの扱い | `src/application/supervise/stall.rs`の`StallWatch::observe_turn` |
| sessionの終わりとrunの状態 | `domain::run::session_end_status` |
| 待ちの最中の開き直し | `src/application/supervise/reopen.rs` |
| backgroundの起動・停止・生死 | `src/infrastructure/background.rs`の`BackgroundWrappers`、`src/domain/background_wrapper.rs`、`src/application/supervise/background.rs` |
| 停止の記録・残ったturnの停止 | `application::recording`の`RecordingSessions::stop_background`・`stopping_left_turns` |
| logを読むCLI | `src/application/session_log.rs` |
| wrapperの入口と登録 | `compose::wrapper_entry`、`application::session`の`register`・`WrapperStart` |
| runtimeのplannerのturn | `src/application/supervise/planner_turns.rs`、`application::planner::launch_planner` |

## 経路の全体

- 最初のturnは`prompt.txt`をpromptにしてsessionを始め、後のturnはsupervisorの依頼を1つずつ取って同じsessionをresumeする。
  Claudeはsessionの名前をrunのidにし、Codexは出力で名乗ったthreadのidを`turn_session_identified`として記録して、それをresumeする。
- 依頼を待つ間、agentのprocessは無く、wrapperはheartbeatを続ける。
- 答え・revise・resume・催促のどれも`deliver::submit`が`request_turn`で`turns/`に書く。
  画面の読み取り、入力欄の確認、Enterの送り直し、ダイアログの応答は無い。
- idle markerは`Stop`の形で書くので、最初のsession・revise・resume・[待ち](waiting.md)の見張りはidle markerとreceiptとaskをそのまま読む。
- 終了はどれも終了の依頼になる（reviewのpassの後、resumeの終わり、黙ったwrapper、`stalled`のaskへの`stop`）。
  wrapperは走っているturnを止めてexit code 0で終わり、残っていれば下の「停止」で止める。

## run dirの`turns/`

- 名前は`src/domain/turn.rs`が決める。
  supervisorが書くのは依頼・終了の依頼・`limits.json`、wrapperが書くのは取った印・turnの出力・Codexのturnのコマンドのfile。
  runtimeのplannerには`planner request`のCLIも依頼を書く。
- wrapperは取られていない依頼を`seq`の順に1つずつ取り、renameで取った印を付ける。
- 依頼の番号は取られたものと捨てたものを含めて数え、使い回さない。
- 前のsessionが取らずに終わった依頼は、新しいsessionの前に`prepare_turns`が捨てる。
  新しいsessionはsupervisorが今書く依頼から始まる。
- `limits.json`は[Stall thresholds](stall-thresholds.md)の値で、wrapperがturnのたびに読む。
- runtimeのplannerのwrapperは、壁（ログイン切れ・利用上限・起動の失敗）で終わったturnの後は`provider retry`を先に取り、待っていた依頼をその後に取る（`request_to_take`）。
  workerのwrapperは順番を変えない。
- turnの設定（`claude-headless-settings.json`）は`turn_command`がturnのたびに書く。
  plannerのturnも同じ設定を使い、拒否はactorのroleのもの。

run dirのI/Oの約束と落とし穴:

- run dirの中は記述子に対する操作で扱い、linkとFIFOを辿らず、FIFOのopenで待たない。
  書き込みは新しい一時fileからrenameし、turnの出力は新しいinodeの記述子へ渡す。
- run dirかどうかはpathだけで決める（`agent_dir::in_run_dir`）。
  hostのpath（queueのdirとDB、installしたバイナリ、`/tmp`、linkにしたdata dir）は`std::fs`と同じくlinkを辿る。
  任意の深さのpathを安全にするAPIではない。
- 読めない・書けないrun dirのfileは既存の失敗の経路に乗る（不正な依頼ならwrapperはerrorで終わり、復旧へ渡る）。
  診断のlogとturnのコマンドのfileが書けないときはwarnだけで、turnは続く。
- Codexのturnのコマンドのfileは区間の作業の内訳の元で、読めないときは区間の`work`をnullにして理由を書く（[provider-lifecycle](../provider-lifecycle.md#非対話のworkerの区間)）。

## turnの記録

- eventは`turn_requested`（supervisor）・`turn_started`・`turn_session_identified`・`turn_finished`（wrapper）。
  欄の意味は`EventKind`と書き手（`headless_session.rs`）のそばにある。
- `turn_requested`は送った文として数えるので、引き継ぎやadoptの後の見張りは走っているturnを促さない。
- wrapperはsessionの最初のturnのprocessをrunの`agent`として登録し、後のturnは同じ行のpidを差し替える（`register_turn_agent`）。
  runの状態遷移は最初のturnだけで、turnの間は行が終わったturnのpidのまま残る。
- runのsessionの区間の稼働時間とトークン数は`turn_finished`から取る（[provider-lifecycle](../provider-lifecycle.md#非対話のworkerの区間)）。
- Claudeの`total_cost_usd`と`modelUsage`はsessionの累計なので、wrapperは前の累計を引いてturnの分にする（`domain::turn::turn_own_cost`・`turn_own_models`）。
  turnのトークン数は`modelUsage`から取り、subagentの分を含む。
  `turn_finished`のトークン数の欄と、`modelUsage`の無い出力の扱いは[Executionのトークン数](../execution-tokens.md)。
  `result`の前に止められたturnの分は次のresumeのturnに入り、過去の記録は補正しない。
- 次のturnをresumeするか始め直すかは、sessionが作られたか、providerがsessionを持っているか（`turn_session_exists`）で決める。
  Claude Codeは使われている名前のsessionを拒むので、答える前に失敗したturnが残したsessionもresumeする。
  Codexは名乗ったthreadがあるかだけで決める。
- wrapperはturnの要約を`[dagq]`の行でrunのlogに書き、人は`dagq run log`で読む（下の「出力」）。

## Claudeの呼び出しと出力（`ClaudeCode`・`ClaudeTurnReader`）

- 呼び出しの組み立ては`ClaudeCode::turn_command`、出力の形と読みは`src/infrastructure/claude_turns.rs`のmodule docが持つ。
- turnのprocessは端末を持たず、自分のprocess groupで走る（`CommandSpec::new_session`）。
- 失敗の分類（`authentication`・`usage_limit`・`model`・`other`）は読み手が出力から決める。
  overageで賄っている利用上限は止まりとみなさない（`usage_limit_hit`）。
- Claudeはtoolの実行中も出力にheartbeatがあるので、出力の途絶えを止まりとみなせる。
  Codexは無いので途絶えでは止めない。

## wrapperがturnを止めるとき

- 止める条件は、出力の途絶え（heartbeatのあるproviderだけ）、時間の上限、頼んだpermission modeで始まらない、providerが使えない、終了の依頼の5つ。
  出力から読む条件は`observed_stop`、時間と終了の依頼は`stop_decision`が決める。
  閾値は[Stall thresholds](stall-thresholds.md)。
- 頼んだmodeで始まらないのを止めるのは、modelによっては黙って別のmodeになるため（ADR-t813-1決定8）。
- 止め方は`headless_session.rs`の`stop_turn`が持つ。
  ClaudeもCodexもtoolのコマンドを別のprocess groupで走らせるので、groupへのsignalだけでは届かず、子孫をgroupを止める前にpidで集めて止める。
- turnが自分で終わったときは、turnのgroupに残ったものだけを止め、groupの外に切り離されたものは止めない。
  理由はその判断のそばのコメントにあり、残ったものは復旧jobの`stop_processes`がworktreeで判じて止める。
- wrapperがsignal（停止のSIGTERMなど）で終わるときは`stop_groups_on_exit_signals`がturnのgroupだけを止める。
  signal handlerは子孫を集められないので、groupの外のコマンドは`stop_processes`に任せる。

## turnの後の扱い

- 成功（receiptかaskが残った）: wrapperは次の依頼を待つ。
  supervisorはreceiptがあれば[validating](receipt-and-session-exit.md)へ、`worker_question`が開いていれば答えを待つ（slotを空け、ADR-0071の読み替え）。
  答えは次のturnの依頼になる（[workerの質問への回答の送信](worker-question-answer.md)）。
- Codexのworkerの`dagq ask`はsandboxの中からqueue serviceに送られ、その場で開く（[Queue service](../queue-service.md#クライアントモード)）。
- receiptもaskも無いturnの終わり: 待たずに決まった文で促し、上限を使い切れば復旧jobの`stalled`にする（`StallWatch::observe_turn`、ADR-0047決定30の読み替え）。
  拒否が続いたturnは促さずに`stalled`にする。
  落とし穴: receiptが無いことはidle markerを読んだ後にもう一度確かめる。
  turnの終わり際に書かれたreceiptを促すと、同じ作業がやり直されてvalidationの最中にcommitが進み、runが失敗するため。
- providerが使えない（`authentication`・`usage_limit`・`launch`）: wrapperはsessionを終えず、supervisorは促さずにもう一方のproviderへ切り替える（[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)、[provider-lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。
  切り替えられないときはrunを失敗にせず待たせ、控えが解ければ同じsessionへ`provider retry`を送る。
  ログイン切れと利用上限はqueueの控えに加わり、人の`done`で`continue`を送る（[queue hold](queue-hold.md)）。
  reviseとresumeの段でも同じ。
- turnの失敗とwrapperが止めたturn: wrapperはexit code 1で終わり、supervisorは促さずに終わりを待つ。
  runの状態はreceiptの有無で決まり、receiptとwrapperの終わりをどの順で見ても同じになる（[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)、`session_end_status`）。
  receiptを書いた後に失敗したturnのrunはvalidatingへ進み、無ければ`failed`で[triage](triage.md)の復旧jobにかかる。
- 答えずに閉じた`worker_question`: 促しの代わりに閉じたことを伝える文を1回だけ送る（[receiptの無いidleの検知](idle-without-receipt.md)）。

## 待ちの最中に失ったsessionの開き直し

非対話のrunは待ちのあいだagentのprocessを持たず、session・worktree・開いたaskはwrapperが死んでも残る。
そこで[待ち](waiting.md)の最中にwrapperを失ったrunは、runtimeが開き直して待ちを続ける（ADR-0047の第1層、`auto_repaired`に記録）。
use caseは`src/application/supervise/reopen.rs`で、条件・上限・数え方はそのmodule docと定数が持つ。

- 対象は最初のsessionの待ちで、receiptが無く、閉じていない`worker_question`か`stalled`のaskがあるrunだけ。
  revise・resumeの待ちは対象外。
- 最後のturnのprocessが生きていれば、その横に2つ目のsessionを開かず、turnの上限まで終わるのを待つ。
- 失ったwrapperが動いていれば先に止め、止められなければ新しいwrapperを起動しない（同じ依頼を2つのwrapperが取らない）。
- wrapperがturnの終わりを見ずに失われたら、supervisorがそのturnのidle markerを書き、開き直したsessionが答えを次のturnで受けられるようにする。
- 試みの状態はsupervisorのprocessの中だけにある。
  引き継ぎの後に状態の無いrunを見たprocessは、前の試みのwrapperが起動中かもしれないので、登録の待ちの時間を待ってから次へ進む。
- 時間の判断は注入した`Clock`の単調時計で測り、時刻を値で受ける関数が決める。
- 上限を超えたら今の経路に渡し、終了したsessionと同じ判定（ADR-t1594-1）でrunを進める。

## 復旧jobのalertと操作（決定9）

- workerの生きているrunの復旧が扱うのは`stalled`と`idle_process`。
  操作は`send_instruction`・`stop_processes`・`resume`（最初のsessionだけ）・`wait`で、適用の時に次の依頼が無くturnが終わっていること、runのprocessであることを確かめる。
- 画面に由来するalertと、画面に打つ操作は無い。
  過去のalert・verdict・eventは履歴として読める。
- 人が打鍵できないので、過去の`intervene`の答えはaskを開き直して答えを求める（`stall.rs`の`INTERVENE_OPTION`）。

## workerへの文面

workerへのprompt・依頼・答え・復旧jobの指示は、どのrunでも非対話の文面で、`/exit`・画面への打ち込み・backgroundの処理に頼らない。
Codexはsubagent reviewをしない。
文面は[Prompt](prompt.md#経路とproviderごとの文面)が持つ。

## 対話と記録されたtaskのclaimとresume

- 新しく対話を選ぶ経路は無く、`add` / `edit`の`--interactive`は代わり（`dagq run log RUN --follow`と`answer`）を示して拒む（[Provider lifecycle](../provider-lifecycle.md#workerのproviderと経路)）。
- `interactive`と記録されたtaskとrunは読めるが、claimとresumeが実際のrunを`headless`にし、変えたときだけ`worker_mode_converted`を記録する。
- `run screen`はどのrunにも拒否を返す（下の「画面と片付けのCLIの拒否」）。

## workspaceなしのbackgroundのwrapper

workerのsession wrapperは、supervisorから切り離したprocessとしてだけ動く（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)・[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
inboxと、廃止の前に人が開いたplannerは対象外。

- **設定**（ADR-t1433-3決定2）: `[headless] wrapper`は読んで検査するが、workerにもruntimeのplannerにも効かない。
  `"workspace"`ならsupervisorはprocessごとに1回だけ警告する（`warn_ignored_wrapper_setting`）。
  拒まないのは、本番のsupervisorと固定バイナリの入れ替えの順で`dagq.toml`が読めなくならないため。
- **起動**: `ActorProgram::RunSession`のwrapperのcommandを`SessionWrappers::launch_background`に渡す（`background::wrapper_command`、logのpathは`Supervisor::session_log`）。
  logはworkerが書けるrun dirの中なので、shellにpathを開かせず、supervisorが新しいinodeで作った記述子を渡す。
  起動の直後に起動時刻を読めなければ起動の失敗で、`backend_call_failed`に残る（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)決定9の読み替え）。
- **handleと記録**: 識別は`BackgroundHandle`（pidと起動時刻）で、runはこれをworkspaceのIDの代わりに記録し、続けて`wrapper_launched`を記録する。
  記録できなければwrapperを止めてエラーにする。
  workspaceのIDを読む処理はhandleをそのまま使い、cmuxのadapterがhandleの呼び出しをprocessの操作に振り分ける（画面と入力は拒む）。
  sessionが開いているかは`run_session_open`がprocessで判じ、cmuxに聞かない。
  ADR-t1433-3より前にworkspaceで開いたsessionのIDは開いていないとみなし、閉じない（人が自分のterminalで閉じる）。
- **wrapperの入口と登録**: `--background`の入口は端末を確かめず、自分で新しいsessionとprocess groupを持つ（`compose::wrapper_entry`）。
  wrapperは自分の起動が記録されるのを待ってから登録する（`launched_as`）。
  同じpidで別の起動時刻の記録は、死んだwrapperのpidを継いだ別のprocessのものなので自分とみなさない。
- **env**: workerのactorの変数と`[run.env]`（[Run environment](run-environment.md)）はwrapperのprocessのenvで渡す。
  supervisorのenvの`CMUX_*`と`DAGQ_*`は外してから足す。
  agentのenvの組み立ては[session wrapper](session-wrapper.md)が持つ。
- **識別と生死**: 生きているとは、handleのpidが記録した起動時刻のまま居ること（`Supervisor::wrapper_lives`）。
  adopt・引き継ぎ（[Handoff](handoff.md)）・開き直しは記録したhandleで行うので、supervisorが止まってもwrapperとturnは動き続ける。
  heartbeatの古さは生死に使わない（沈黙は[Silent wrapper](silent-wrapper.md)の判定）。
  `stats`の`workspace_mismatch`もcmuxを呼ばずこの生死だけで判じ、終わりを記録せずに居なくなったwrapperを`run_without_wrapper`として出す（[Stats](stats.md)）。
- **停止**: runtimeはまず終了の依頼で終わらせ、残っていればhandleの`stop_background`で止める（ADR-t1404-1決定3）。
  wrapperにSIGTERMを送り、猶予の後にgroupと控えた子孫にSIGKILLを送る（`BackgroundWrappers::stop`）。
  wrapperが先に死んで残したturnは子孫として見つからないので、最後の`turn_started`の記録から止める（`stopping_left_turns`）。
  そのturnが生きている間、sessionは開いているとみなされ、後始末と掃除が停止に進む。
  ADR-t813-1決定5の「最後のturnの終わりにworkspaceをcloseする」は、wrapperの終わりを確かめて残りを止めることと読む。
- **停止の記録**: 止めるたびに、止めた経路（`StopRoute`）とsignalを`wrapper_stopped`に記録する（`RecordingSessions::stop_background`）。
  SIGTERMで終わったかSIGKILLまで要ったかを[評価](../../plans/headless-background-evaluation.md)の「processの残り」が数えるため。
  runの無いwrapper（planner）の停止はqueueのeventになる。
  記録できなくても停止の結果は変えない。
  復旧jobの`stop_processes`はwrapperでなくpidを止めるので`wrapper_stopped`を記録しない。
- **出力**（ADR-t1404-1決定6）: wrapperの`[dagq]`の要約とlogはrun dir（plannerはplannerのdir）の`session.log`に追記される（resume・開き直しは試みごとのlog）。
  書けない行は捨て、sessionは止めない。
  turnの生の出力は`turns/`、時間の流れは`dagq timeline RUN`。
- **logを読むCLI**（`application::session_log`）: `dagq run log RUN`はrunの最後の`wrapper_launched`のlogを、`dagq planner log ID`はplannerの`session.log`を出し、人がpathを組み立てない。
  大きなlogも1回の読みの上限に関わらず分けて全体を出し、`--follow`はwrapperが終わるまで追う。
  resumeで次のwrapperが起動したら、コマンドを打ち直して読む。
  run dirのlogはworkerが書けるので、linkを辿らずに読む。
  権限は`screen.read`（[Authorization](../authorization.md)）。
- **画面と片付けのCLIの拒否**（ADR-t1433-3決定3・4）: `dagq run screen`はどのrunにも拒否を返し、代わりに`run log`と`turns/`の場所を示す。
  `dagq run close-workspaces`は権限の検査の後に拒む。
  どちらもcmuxを呼ばない。
- **pidとlogの場所**: `status`の`runs`と`show`の`runs[0]`は、runの今のsessionがbackgroundなら`background`（handle・pid・起動時刻・log）を持つ（`current_background_session`）。
  `planners`の各要素も、handleを記録したplannerなら同じ形を持つ。
- **評価**: 切り替えの前後の比較は、workspaceに戻すかの判断でなく、backgroundの経路の退行（残ったprocess・startup・起動の失敗・識別の誤り）の確認として読む（ADR-t1433-3決定5、[zero-based-headless-readiness](../../plans/zero-based-headless-readiness.md)）。

## 非対話のruntimeのplanner<a id="非対話のruntimeのplanner"></a>

runtimeのplannerは経路の選択なしに非対話で立ち、この文書のturnの駆動をplannerのディレクトリで使う（[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定2、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3）。
旧keyの`[roles.runtime_planner] route`は値に関わらず無視する（[Actor model](actor-model.md#runtimeのplannerの経路)）。
仕組みの全体は[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)が持つ。

- **持ち主ごとの違い**（`TurnOwner::Planner`）: `turns/`とidle markerはplannerのディレクトリ、作業ディレクトリはrepositoryのcheckout、turnのeventはqueueのeventで`planner_id`で名指す。
  providerはClaudeだけで切り替えない（ADR-t1394-2決定5）。
  sessionの名前は同じIDのplannerを作り直したqueueと共有しない（`planner_session_name`）。
  providerにplanner固有の分岐は無い（`TurnTarget`）。
- **依頼**: revise・`planner_question`の答え・`provider retry`・終了は`Supervisor::send_to_planner`が`turns/`の依頼と終了の依頼にし、plannerには打ち込まない。
  落とし穴: 依頼の前に入力の印を書くので、壁で失敗したturnの後に印だけが新しくなると、plannerは`working`に見えたまま誰も壁を見ない。
  wrapperはそのとき壁のturnのidle markerを書き直す（`renew_wall_marker`）。
- **初期promptが最初のturn**: 人の答えだけを待って終わったplannerの後のanswerは、新しいplannerの初期prompt（`prompt.txt`）に質問・前のplannerのnote・draftと載る（resumeしない）。
- **状態**（`planner_view`）: 画面を読まず、idle markerと待っている依頼から`idle` / `working`を決める。
  壁で終わったturnのplannerは依頼が待っていても`idle`。
  生死はhandleのpidと記録した起動時刻だけで判じ、heartbeatが古くても`lost`にせず、cmuxに聞かない。
  答えを読み終えたかは、その依頼を取ったturnが壁でなく終わるまで追う（`request_read`）。
  壁で失敗した直後に終わらせると答えが読まれずに失われるため。
- **時間切れ**: wrapperがturnを`[stall]`の上限で止めたとき、plannerごとに1回`planner_unresponsive`を出す（`tell_of_stopped_planner_turns`、ADR-t1394-2決定3）。
- **使えないとき**（`tend_planner_walls`）: 壁のplannerは終わらせず、控えのaskを開き、控えが解けたら`provider retry`を置く。
  待つあいだはreviseと答えも置かない。
- **起動**: `launch_planner`が`ActorProgram::PlannerSession`のwrapperをbackgroundで立て、handleをplannerの行に記録する。
  cmuxのworkspace・色・pill・groupは作らない。
  `dagq submit`はplannerの記録からhandleを読んでproposalの持ち主にするので、wrapperは最初のturnの前にhandleが記録されるのを待つ。
- **終わり**: 終了の依頼でwrapperはexit code 0で終わる。
  人の答えだけを待つplannerも終了の依頼で終わり、行は`runtime_answer_wait`で閉じる（[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)の「人の答えだけを待つplannerの終わり」）。
  掃除はhandleをprocessの識別で判じ、生きているwrapperの行はheartbeatが遅れても閉じない。
  wrapperが殺されて残ったturnは、plannerの最後の`turn_started`から止める（ADR-t1404-1決定3・8）。
- **区間と集計**: 最初の`turn_started`が区間を開き、plannerの閉じが閉じる（[provider-lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。
  経路ごとの集計は[stats](stats.md)と[kpi](kpi.md)、workerの経路の集計はrunの無い`turn_finished`を数えない。
- **CLI**（[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)）: `planner screen`は画面を読まずに`turns/`の場所を返し、`planner send`は拒む（[Session send](session-send.md#人とinboxの画面の読み取りと送信)）。
  続きの依頼は次のturnの依頼として置く（[`plan` / `planners`](plan-planners.md#続きの依頼と非対話のplannerのcli)）。
- **test**: 人の答え待ちでの終了と、answerで立つplannerの最初のturnは本物のwrapperで`planner_headless::a_headless_draft_planner_ends_while_its_question_waits_and_a_new_one_goes_on_from_the_answer`と`a_headless_request_planner_ends_while_its_question_waits_and_the_answer_opens_one_that_declines`。

## taskの再計画（予定・未実装）

再計画による保留は次turnの配送を封鎖し、background wrapperと書き手の終了を確認してsnapshotを保存する。
遅れたreceiptと旧generationの終了は通常のvalidation/着地へ進めない。
Claude/Codexは同じ停止・復元の契約を使う。
詳細は[taskの再計画](task-replanning.md)が持つ。
現行の挙動は上の各節のとおりで、この追加だけではrunを保留しない。
