---
id: design-supervisor-lifecycle-headless-worker
type: design
title: "非対話のworker"
status: current
created: 2026-09-28
updated: 2026-10-03
last_verified: 2026-10-03
scope: runtime
related:
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

[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)の実装（task 815）。taskの`worker_mode`が`headless`（経路を指定しないClaudeのtask（既定、[ADR-t1340-1](../../adr/2026-10-02-t1340-1-claude-worker-defaults-to-headless.md)）と`add --headless`、Codexのtask。[provider-lifecycle](../provider-lifecycle.md#workerのproviderと経路)）のrunは、workerの1 turnを1回の非対話の呼び出しにする。動くのはClaude（`claude -p --output-format stream-json --verbose`）とCodex（`codex exec --json`と`codex exec resume --json`、task 816。[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。providerの違いは`AgentProvider`の`turn_command`（呼び出しのargv）と`turn_reader`（出力を読む`TurnReader`）と`turn_permission_mode`に閉じ込め、supervisorとsession wrapperの流れはproviderを知らない。

## 経路の全体

- **e2eはworkerのturnで流さない**（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。ClaudeでもCodexでも、e2eが要るrunにはreviewのpassの後にruntimeがhostで流し（[Review](review.md#着地の前のe2e)）、落ちれば同じsessionの次のturnとしてresumeの依頼（`ResumeKind::E2e`）を送る。Codexのworkspace-writeのsandboxで走らないe2eを名前で除外する規則と、その除外を書くreceiptの`e2e`の書式（task 1206）は無い。
- **session wrapperがturnを動かす**（決定3）。runのworkspaceのwrapper（`session`、[session wrapper](session-wrapper.md)）は、runの`worker_mode`が`headless`なら`src/application/headless_session.rs`の`Turns`でturnを1つずつ起動する。最初のturnは`prompt.txt`をpromptにし、sessionを始める（Claudeはsessionのidをrunのidにする`--session-id <run id>`、Codexは出力の`thread.started`でthreadのidを名乗り、wrapperがそれを`turn_session_identified`としてrunに記録する）。後のturnはsupervisorの依頼を1つずつ取り、同じsessionをresumeする（Claudeは`--resume <run id>`、Codexは`codex exec resume <記録したthreadのid>`）。依頼を待つ間もheartbeatを続け、agentのprocessは無い。
- **supervisorは打ち込まずに依頼を書く**（決定2）。supervisorがsessionに送るものはすべて`deliver.rs`の`submit`を通るので、`headless`のrunでは`headless.rs`の`request_turn`が、文をrunの`turns/`への依頼に、`/exit`を終了の依頼に替える（`Submission::Queued`）。送った文の確認（`StartCheck`）、Enterの送り直し、入力欄の確認、画面の読み取り（ダイアログ・作業中・認証の画面・idleの推定・既知のダイアログへの応答）は`headless`のrunでは行わない（`watch_prompt`・`session_idle`・`answer_known_dialog`・`answer_exit_dialog`・`known_dialog_ready`が早く戻る）。
- **turnの終わりがidleの印**。wrapperはturnのprocessが終わり`turn_finished`を記録した後に、runのidle marker（`idle.json`）を`Stop`の形で書く（`hook_event_name: Stop`、background taskは無し、`dagq_turn`に`turn`・`outcome`・`failure`・`permission_denials`）。Claudeの`Stop` hookは使わない（turnのsettingsにhookは無い）。これで最初のsession・reviewのrevise・`needs_session`のresume・待ち（[waiting](waiting.md)）の既存の見張りが、idle markerとreceiptとaskをそのまま読む。
- **終了**: 終了の依頼を見たwrapperは、turnを走らせていれば止め（`outcome: stopped`）、exit code 0で終わる。reviewのpassの後の`/exit`、resumeの試行の終わり、wrapperが黙ったときの`/exit`、`stalled`のaskへの`stop`の答え（下の「turnの後の扱い」）は、どれもこの依頼になる。workspaceのcloseの時点は対話と同じ（決定5）。

## run dirの`turns/`

`src/domain/turn.rs`が名前を決める。supervisorとwrapperはrun dirと`turns/`をlinkを辿らずに開いた記述子でfileを扱う（task 1184）。通常のfileだけを上限付きで読み、書き込みは新規の一時fileからrenameし、turnの出力は新しいinodeの記述子へ渡す。link・FIFOの指す先を読み書きせず、FIFOのopenで待たない。idle markerとreceiptも同じ境界で扱う（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)の「run dirのfile」）。

| file | 書く側 | 中身 |
| --- | --- | --- |
| `request-NNNNNN.json` | supervisor | `TurnRequest`（`seq`・`what`・`prompt`）。一時fileからrenameで置く。`seq`は`turns/`の依頼（取られたものを含む）の最大の次 |
| `request-NNNNNN.taken.json` | wrapper | 取った依頼（renameで印を付ける）。wrapperは取られていない依頼を`seq`の順に1つずつ取る |
| `request-NNNNNN.dropped` | supervisor | 前のsessionが取らずに終わった依頼。新しいsessionの前に捨てる（下の`prepare_turns`）。番号は取られた依頼と同じく数え、使い回さない |
| `exit` | supervisor | 終了の依頼 |
| `limits.json` | supervisor | turnの上限（`silence_secs`・`limit_secs`。testが秒未満で入れたときは`silence_ms`・`limit_ms`も持ち、秒の代わりに使う。[Stall thresholds](stall-thresholds.md)）。`[stall]`から |
| `turn-NNNNNN.jsonl` / `.err` | agent | turnのstdout（providerのJSONL）とstderr |

supervisorは最初のsessionのworkspaceを開く前（`provision`）とresumeのworkspaceを開く前（`start_resume`）に`prepare_turns`を行い、`limits.json`を書き、前のsessionの終了の依頼と取られていない依頼を捨てる。turnの設定は`claude-headless-settings.json`（`permissions.deny`だけ。`SIGNAL_BY_NAME_DENIED`とworkerのroleの拒否、`autoMode`）で、`turn_command`がturnのたびに書く。

task 1184でrun dirのI/Oを洗い出し、次の呼び出しを`agent_dir`の記述子に対する操作へ寄せた。通常のfileが読めない・書けない場合のrunの失敗の扱いは既存の経路を使う（不正な依頼ならwrapperはerrorで終了し、復旧へ渡る）。

| 呼び出し | 対象と扱い |
| --- | --- |
| `supervise/headless.rs`・`headless_session.rs` → `LocalRunFiles` | 依頼・終了依頼・limits・prompt・idle markerと一時file。終了依頼は通常のfileだけを認める |
| supervisorのsession・resume・revise・validation・review → `LocalRunFiles` | receipt、idle marker、設定・review material。全文は64MiBまで。非通常のidle markerはwarnして印なしとする |
| `LocalSpawner`・`adapters::run_shell_to_log`・`diff_to_file` | turnのstdout/stderr・検証log・差分。新しいfileの記述子を子processに渡す |
| `adapters::write_settings`・`turn_command`・`create` | Claudeの設定（Codexからの切り替え後も）とworkspaceの作成結果。安全な一時fileからrename |
| `runtime_store::Refusals`・`sessions::work_breakdown` | 診断logへの追加。通常のfileを上限付きで読み、新しいinodeで置き換える。読めなければlogだけを欠く |
| `broker_token`・`LocalRunFiles`のtree操作 | runのbroker設定と後始末。dirの列挙・子dirのopen・削除・大きさの集計も記述子に対して行い、linkを辿らない |

queueのdirなどworkerが書けない場所と、workerが書くrun dir（直下の`turns/`・`broker/`など）を区別する。pathにqueueの`runs/`の下の部分（run dirとその中）があるときだけ（`agent_dir::in_run_dir`）、`LocalRunFiles`と`agent_dir`の`create_file`・`append`は記述子の操作を使う: ディレクトリの初回openが指定したdirとその親の2段をlinkとして拒み、最後の要素のfileもlinkを辿らず（`O_NOFOLLOW`と`AT_SYMLINK_NOFOLLOW`）、通常のfileだけを64MiBまで読み、書きはlinkを置き換える。それ以外のpath（queueのdirとDB、installしたバイナリやbrokerのclient、macOSの`/tmp`、linkにしたdata dir、scratchpad）はhostのもので、`std::fs`と同じくlinkを辿り、上限も当てない。`in_run_dir`はpathだけで決め、最初の`runs`という名前の要素の下を run dir とみなすので、queueより上に`runs`というdirがあるhostのpath（`/Users/x/runs/project/...`）もrun dirの扱い（linkを拒むだけで、辿る範囲は広がらない）になる。任意の深さのpathを安全にするAPIではない。`LocalRunFiles::copy`の元（runtimeのバイナリ）はruntimeのもので、linkを辿って読み、64MiBの上限を当てない。Claudeのdebug logのhookの失敗は`RunFiles::read_tail`で末尾だけを読むので、64MiBを超えるlogでも見つかる。treeの走査は開いたdirから`openat`で子へ進む。

## turnの記録

- `turn_requested`（supervisor）: 送った文として数える（引き継ぎ・adoptの後の`StallWatch`もこのeventを最後の入力に数えるので、走っているturnを促さない）。`seq`・`what`（`answer of ask N`・`revise request`・`resolution request`・`nudge`・`recovery instruction`・`continue`など、対話の`submit`の`what`と同じ）・`workspace_id`。
- `turn_started`（wrapper）: `turn`（runの通し番号）・`resume`（sessionを続けるか）・`request`（依頼の`seq`、最初のturnは`null`）・`what`・`pid`・`session_id`・`silence_secs`・`limit_secs`。wrapperはそのsessionの最初のturnのprocessを`agent`として登録する（`agent_started`、runが`running`になる）。後のturn（answer・revise・resume・催促・providerの切り替えの後）は1つずつ別のprocessなので、wrapperは起動のたびに`run_processes`の同じ`agent`の行の`pid`をそのturnのprocessに差し替え、`heartbeat_at`を今にする（`RunCoordination::register_turn_agent`。task 862）。`run_processes`の主キーは`(run_id, role)`でagentの行は1つなので、行を足さずに差し替え、migrationは要らない。`agent_started`とrunの状態遷移は最初のturnだけで、`turn_started`の`pid`は今までどおりturnごとに載る。agentのpidを読む箇所（`idle_process`の見張りの`own_of`と`without_session_helpers`、`stop_processes`の対象など）はその1行を読むので、走っているturnのprocessとその子のhelper（MCP serverなど）をsessionのものとして扱い、終わったturnのpidを見ない。turnの間（次の依頼を待つ間）は、行は終わったturnのpidのまま残る。
- `turn_session_identified`（wrapper）: 出力でsessionを名乗るprovider（Codex）のturnが名乗ったとき、`turn`・`session_id`・`provider`。後のturnは最後に記録したものをresumeする。
- `turn_finished`（wrapper）: `turn`・`outcome`・`failure`・`stopped`（wrapperが止めた理由の文）・`exit_code`・`message`・`session_id`・`session_created`・`num_turns`・`duration_ms`・`cost_usd`（そのturnの分。Claudeの`total_cost_usd`はsessionの累計なので、下の読み手の項のとおりwrapperがturnの分にする）・`session_cost_usd`（costがsessionの累計のprovider（Claude）だけ、そのturnの終わりの累計。task 1199より前の記録には無い）・`usage`（providerの出力のまま）・`provider`・`tokens`（そのturnのruntimeの種類のトークン数`{input, output, cache_read, cache_creation, messages}`と、あれば`cost_usd`。読めなければnull）・`tokens_total`（usageがsessionの累計のprovider（Codex）だけ、その累計。他はnull。ADR-t813-2の決定7、[provider-lifecycle](../provider-lifecycle.md#非対話のworkerの区間)）・`permission_denials`（件数）・`denied_tools`。runのsessionの区間（`route: headless`）の稼働時間とトークン数はこのturnから取る。

`outcome`は`succeeded`・`failed`・`silent`・`timed_out`・`launch_mismatch`・`stopped`、`failure`は`authentication`・`usage_limit`・`model`・`sandbox`（Codexのsandboxの拒否で失敗したturn）・`launch`（agentを起動できない、または出力に1行も出さずに非0で終わった。起動できなかったturnも`turn_started`（`pid: null`）と`turn_finished`を持つ）・`other`。`turn_started`には`provider`も載る。`session_created`は、providerのmodelが一度でも答えたturnで`true`になる。Codexはsessionを名乗ったか（`turn_session_identified`があるか）だけで決め、あればそのthreadをresumeし、無ければtaskのpromptから始め直す。以下はClaudeの決め方。次のturnは、それまでにsessionが作られたか、providerがそのsessionを持っている（`AgentProvider::turn_session_exists`。Claudeは`$CLAUDE_CONFIG_DIR`か`~/.claude`の`projects/`にtranscriptがある。答える前に失敗したturnが残しうる。Claude Codeは使われている`--session-id`を拒む）ならresumeし、どちらでもなければ`--session-id`でtaskのpromptから始め直す（依頼の文はtaskのpromptの後に付ける）。

workspaceのterminalにはwrapperがturnの要約（turnの開始、agentの文、tool、turnの結果）を`[dagq]`の行で出す。人は打ち込まない（決定4）。

## Claudeの呼び出しと出力（`ClaudeCode`・`ClaudeTurnReader`）

- 呼び出し: `claude -p --output-format stream-json --verbose (--session-id|--resume) <run id> --permission-mode auto --debug-file <runのlog> --add-dir <run dir> --settings <run dir>/claude-headless-settings.json [--model M --effort E] -- <prompt>`、cwdはworktree、stdinは閉じ、process groupを分ける（`CommandSpec::new_session`）。
- 読み手（`src/infrastructure/claude_turns.rs`）: `system/init`から`session_id`・model・`permissionMode`、`assistant`の文とtool、`result`の`is_error`・`num_turns`・`duration_ms`・`total_cost_usd`・`usage`・`permission_denials`を読む。`usage`・`num_turns`・`duration_ms`はそのturnの分だが、`total_cost_usd`と`modelUsage`はsessionの累計（resumeした前のturnを含む。run 2d1f5abdの2 turn目の`total_cost_usd`は1 turn目の4.7212を含む6.0195で、`modelUsage`の`outputTokens`も55270から73069へ足されていた。task 1199）。読み手は`TurnResult::cost_cumulative`を立て、wrapperはturnの`cost_usd`と`tokens`の`cost_usd`を、今の累計から同じ`session_id`の前の`turn_finished`の累計（`session_cost_usd`。それが無いtask 1199より前の記録では当時累計だった`cost_usd`。値がnullのturnは飛ばしてさらに前を見る）を引いた分（小数6桁に丸める）にし、累計を`session_cost_usd`に書く（`domain::turn::turn_own_cost`）。引くのは`turn_started`の`resume`が真のturnだけで、sessionを始めたturn・`session_id`の無いturn・前の累計が見つからないturnは累計をそのままturnの分にする。前の累計より今の累計が小さい（差が負。同じidで別のsessionが始まったなど前提が崩れた）ときも、記録を捨てずに今の累計をそのままturnの分にする。providerの切り替えの後にClaudeへ戻ったturnは、切り替えの回数から決まる新しい名前のsessionを始める（`resume`が偽）ので、累計をそのままturnの分にする。`result`の前に止められたturnの分は記録されず、次のresumeしたturnの分に入る（runのturnの合計は最後の累計に等しいまま。Codexのトークン数と同じ）。Codexのturnはcostを持たないので変えない。過去の記録は補正しない（[provider-lifecycle](../provider-lifecycle.md#非対話のworkerの区間)）。失敗の分類は、`system/api_retry`の401か`authentication_failed`、`assistant.error`の`authentication_failed`が`authentication`、`rate_limit_event`の`status: rejected`（`isUsingOverage`か`overageInUse`が真なら overage で賄っているので止まりとみなさない。Claude Code 2.1.285 の CLI 自身の条件に合わせた）・`assistant.error`の`rate_limit` / `billing_error`・`api_error_status` 429が`usage_limit`、404かmodelが見つからない旨の文が`model`、ほかは`other`。resultが無いか非0の終了は失敗。Claudeの出力はtoolの実行中も30秒ごとにheartbeatがあるので、出力の途絶えを止まりとみなせる（`heartbeats`）。

## wrapperがturnを止めるとき

wrapperはturnの出力を`turn_*.jsonl`から読みながら（wait interval、既定1秒ごと）、次のどれかでturnを止める。止めるときは、先にturnのprocessの子孫をpidで集め（`ProcessControl::descendants`）、turnのprocess groupにSIGKILLを送り（`Spawned::kill_group`）、集めた子孫にも1つずつpidでSIGKILLを送る（`headless_session.rs`の`stop_turn`、task 1085）。Codexは実行するコマンドをcodexと別のprocess group（pgidがコマンド自身のpid）で走らせるので、groupへのsignalだけではコマンド（`cargo test`など）が親1のまま残る（task 1061の測定、[codex-headless-jobs-spike](../../plans/codex-headless-jobs-spike.md)の4.）。Claude Code（2.1.285）もBash toolの1回ごとにshellを別のprocess group（pgidがそのshellのpid）で起動し、turnのgroupには`claude`自身しか居ないので、groupへのSIGKILLだけではtoolのshellとその子が親1のまま残る（task 864の測定、[manual-smoke](../manual-smoke.md#非対話の-claude-の前提の確認)の1.）。子孫はgroupを止める前に集める（止めた後は親が1になって辿れない）。signalはSIGINTでなくSIGKILLにする: 子孫をpidで直接止めるのでproviderに片付けを任せる必要がなく、応答しないproviderを待たない。Claudeの非対話のturnにも同じ経路を使う。

| 理由 | `outcome` | 条件 |
| --- | --- | --- |
| 出力の途絶え | `silent` | heartbeatのあるprovider（Claude）で、`[stall].turn_silence_secs`（既定900秒）のあいだ出力の行が無い |
| 時間の上限 | `timed_out` | turnが`[stall].turn_limit_secs`（既定14400秒）を超えた |
| 頼んだ設定で始まらない | `launch_mismatch` | 最初の`system/init`の`permissionMode`が`auto`でない（haikuは黙って`default`になる。決定8） |
| providerが使えない | `failed`（`authentication` / `usage_limit`） | 読み手が認証切れか利用上限を言った（401の再試行を待たない） |
| 終了の依頼 | `stopped` | `turns/exit`がある |

閾値は[Stall thresholds](stall-thresholds.md)の`turn_silence_secs`と`turn_limit_secs`で、supervisorが`limits.json`に書いてwrapperがturnのたびに読む。

turnが自分で終わったときも、wrapperはそのprocess groupに残ったものを止める。ただしClaudeもCodexもtoolのコマンドをturnと別のgroupで走らせるので、`nohup … &`のように切り離したものはturnのgroupに居らず、これでは止まらない（task 864の測定）。groupの外に残った子孫（turnのprocessが別のgroupで起動し、turnの終わりより長く生きるもの）は止めない。理由: (a) turnのprocessが終わった時点でその子は親が1になっていて、親子関係ではturnのものと見分けられない。(b) turnの途中で集めたpidを後で止めると、その間に終わったpidを別のprocessが使っていれば無関係のprocessを止めうる（見張りのたびに`ps`を打つ負荷もかかる）。(c) Codexはturnを終える前に実行したコマンドの終わりを待つので、自分で終わったturnがgroupの外に残すのは、agentがわざと切り離したものに限られる（Claudeも終わる前に`run_in_background`のtaskを止める。task 864の測定）。残ったものはrunのworktreeで動くprocessとして、復旧jobの`stop_processes`がpidで止められる（`run_processes`はworktreeで動くprocessを含む）。この振る舞いはtest（`runtime_headless::a_turn_that_ends_by_itself_leaves_what_runs_outside_its_group`）が確かめる。

wrapper自身がturnの途中で終わるとき（エラー）は、上と同じく子孫とturnのgroupを止めてから終わる。hangup・terminate・interruptのsignal（workspaceのclose、`stop_processes`）で終わるときは、`stop_groups_on_exit_signals`（`src/infrastructure/process.rs`。実のwrapperの入口`compose::session`だけが入れる）が、このprocessが`new_session`で起動してまだ止めていないgroupだけを止める。signal handlerの中では子孫を集める`ps`を呼べず（async-signal-safeでない）、wrapperは子孫のpidを見張りのたびに記録してもいない（上の(b)）ので、groupの外のコマンドはhandlerでは止めない。`stop_processes`は自分でrunのprocess（wrapperの子孫とworktreeで動くもの）をpidで止めるのでそのコマンドも止まり、workspaceのcloseで残ったものは上と同じくworktreeで動くprocessとして`stop_processes`が止められる。turnは端末を持たないので、止めなければwrapperより長く生きる。

## turnの後の扱い

- **Codexのworkerのask**: turnのagentは（Claudeのturnと同じく）queue serviceのsocketとtokenのfileを持ち、queue DBのpathは持たないので、sandboxの中の`dagq ask`はクライアントモードでserviceに送られ、その場で開く（ADR-t1233-5決定4・5、[Queue service](../queue-service.md#クライアントモード)）。開いたaskは下のaskと同じに扱う。
- **成功**（receiptかaskが残ったturnを含む）: wrapperは次の依頼を待つ。supervisorはidle markerを読み、receiptがあれば今のvalidatingへ（[receipt and session exit](receipt-and-session-exit.md)）、`worker_question`が開いていれば答えを待つ（runはslotを空けて待ちになる。ADR-0071の読み替え、決定6）。答えは`answer to ask N: ...`を依頼にして送る（[workerの質問への回答の送信](worker-question-answer.md)）。
- **receiptもaskも無いturnの終わり**（`StallWatch::observe_turn`）: 対話の`idle_without_receipt_secs`は待たず（閾値0）、決まった文の促し（`stall_nudged`）を依頼で送る。促しは1 phaseに`HEADLESS_NUDGES`（2）回まで（対話は1回。ADR-0047決定30の読み替え）。前の促しの`stall_resolved`は`nudged_again`になる。使い切った後のturnも同じなら復旧jobの`stalled`（理由`turn_without_receipt`）にする。
- **permissionの拒否が続いて進まない**: receiptもaskも無く終わったturnの`permission_denials`が`PERMISSION_DENIAL_LIMIT`（3）件以上なら、促さずにすぐ復旧jobの`stalled`（理由`permission_denied`）にする。
- **providerが使えない**（`failure`が`authentication` / `usage_limit` / `launch`）: wrapperはsessionを終えずに次の依頼を待ち、supervisorは促しも復旧jobもせず、失敗したturnの呼び出しをもう一方のproviderの新しいsessionへの依頼にする（ADR-t813-2。[provider-lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。切り替えられない（もう一方も使えない、切り替えの上限）ときはrunを失敗にせず待たせ（`provider_waiting`）、自分のproviderの控えが解ければ同じsessionへもう一度送る（`provider retry`）。Claudeの認証と利用上限、および両方使えないときは、`authentication`ならqueueの認証のholdに、`usage_limit`なら利用上限のhold（`reason_category: cost`、subject `usage_limit`）に、対話のsessionの画面と同じ`raise_wall`で加わる（`auth_required` / `usage_limited`を記録する。task 438）。人が`done`と答えると、対話と同じくsupervisorが「続けて」（`continue`）を依頼で送る（[queue hold](queue-hold.md)）。reviseとresumeの段でも同じ（`SessionWatch::provider_wall`）。
- **turnの失敗と、wrapperが止めたturn**（`other`・`model`の失敗、`silent`・`timed_out`・`launch_mismatch`）: wrapperはsessionを終える（exit code 1）。supervisorはそのturnを促さず、wrapperの終了を待つ。receiptの無いまま終わったrunは`failed`になり、今の[triage](triage.md)の復旧job（alert `failed`）にかかる（runが`worker_question`か`stalled`のaskの答えを待つ最中にwrapperが終わったときは、下の「待ちの最中に失ったsessionの開き直し」が先に開き直す）。復旧jobの材料（`ended_run_material`）には、最後の5つの`turn_finished`（`outcome`と`stopped`）が載る。
- **答えずに閉じた`worker_question`**（task 1372）: 答えがworkerに届かないまま閉じた`worker_question`（`ask_delivered`の無い`ask_closed`か、runtimeが答えを書いて閉じた`ask_answered`の`runtime_closed`）があり、sessionがそのcloseより後にturnを終えていない（idle markerがcloseのミリ秒より後でない）なら、`stall_nudged`の促しか復旧jobの代わりに、閉じたことを伝える文（`prompt::closed_question_notice`）を次のturnの依頼として1回だけ送る。中身と記録は[receiptの無いidleの検知](idle-without-receipt.md)の「答えずに閉じた`worker_question`」。

## 待ちの最中に失ったsessionの開き直し

task 1372（goal 86の暫定の対応）。2026-09-30T08:42にtask 681・695の非対話のCodexのwrapperのheartbeatが同時に切れた（orphaned）。非対話のrunは待ちのあいだagentのprocessを持たず、sessionのid・transcript・worktree・開いたaskはwrapperが死んでも残るので、[待ち](waiting.md)の最中にwrapperを失ったrunはruntimeが開き直して待ちを続ける（ADR-0047の第1層、`auto_repaired`に記録）。use caseは`src/application/supervise/reopen.rs`。

- **待ちの最中にsessionを失ったときの今までの扱い**（変更前の実装を確かめた結果）: 待ちの見張り（[waiting](waiting.md)の見張りの2）はwrapperが終了を記録したか、heartbeatが切れてpidも死んでいれば、`answer_prompt`・`stuck_exit`・（`Session`なら）`stalled`のaskを閉じて`session_exited`で待ちを終える。`worker_question`のaskは閉じない（開いたまま残る）。slotに戻った`SessionWatch`は、終了を記録したwrapperならreceiptの無いまま`finish_supervision`でvalidatingに渡して`failed`（`receipt_missing`）にし、復旧job（alert `failed`）にかける。終了を記録せずに死んだwrapperなら`wrapper_pulse`がerrorを返し、runは`runtime_error`で手放される（`recover`の後に`interrupted`として復旧jobへ）。どちらも開いた`worker_question`は残り、後で答えても`runtime_delivers`は`false`でinboxの手の配送になり、復旧jobの`resume`で開いた解消依頼のsession（`ResumeWatch`の`live`）が答えを配送しうる。
- **条件**: 待ちの見張りがwrapperを失ったと見たとき（終了を記録した、heartbeatが切れてpidも死んだ、または前の試みでprocessの行を消した後に新しいwrapperがまだ居ない）、runが`headless`で、待ちが最初のsession（`Session`のphase。revise・resumeの待ちは対象外）にあり、receiptが無く、閉じていない`worker_question`か`stalled`のaskがある。wrapperの終わり方（exit code）は問わない。最後のturnのprocess（`agent`の行のpid）が生きていれば（turnの途中でwrapperを失った。2026-09-30の2本は最初のturnの途中だった）、そのturnの横に2つ目のsessionを開かず、待ちを続けてturnが終わるのを待ち、終わってから開き直す。待つのはturnの上限（`[stall]`の`turn_limit_secs`。wrapperが生きていれば止めていた時間）までで、それを過ぎても生きていれば開き直さずに今の経路（復旧jobが`stop_processes`で止められる）に渡す。
- **開き直し**: 失ったsessionのworkspaceがcmuxにあれば閉じて`workspace_closed`（`by: supervisor`、`reopen`）を記録する。閉じられない（cmuxが答えない・closeが失敗する）ときは、生きているかもしれないwrapperの横に新しいworkspaceを開かず、`session_reopen_failed`（`cause: close_failed`）を記録して次の試みを待つ。閉じたら`clear_lost_session`でrunのprocessの行を消す（runは`running`のまま）。最後の`turn_started`に`turn_finished`が無ければ（wrapperがturnの終わりを見ずに失われた）、supervisorがそのturnのidle marker（`outcome: succeeded`）を書き、開き直したsessionが次のturnの間にある（`between_turns`）とみなされて答えを次のturnで受けられるようにし、`auto_repaired`の`conditions`の`lost_turn`にそのturnの番号を残す。消すのは見て失ったwrapperの行（かwrapperの行が無いとき）だけで、終了を記録していない別のpidのwrapperがその間に登録していれば拒み、その試みは`open_failed`になる。続けてruntimeの写しを取り直して`turns/`の残った依頼と終了の依頼を片付け（`prepare_turns`）、resumeと同じ`session ... --resume`のwrapperでworkerのenvと`[run.env]`のworkspaceを開く。wrapperは依頼を待ち、最初の依頼で同じsessionをresumeする（`register_resume_wrapper`は`needs_session`に加えて`running`のrunも受ける）。開いたworkspaceは`session_reopened`がrunのworkspaceにし（`workspace_closed_at`を空に戻す）、`workspace_created`（`reopened`: 試みの番号）と`auto_repaired`（`layer: runtime`、`repair: headless_session_reopened`、`conditions`: `worker_mode`・`waiting`・`receipt`・`open_asks`・`lost_turn`・`attempt`・`attempts`、`detail`: `workspace_id`・`previous_workspace`・`cause`（`exited` / `died` / `not_registered`）・`exit_code`）を1つのtransactionで記録する。待ちは終えない（`run_waiting_ended`を書かない）ので、答えが来ればslotに戻って今のとおり次のturnで配送する。
- **上限と間隔**: 試みはsessionの最後の`turn_started`から数え（`reopen::attempts_in_a_row`。`auto_repaired`の`headless_session_reopened`と、`cause`が`registration_timeout`でない`session_reopen_failed`（`open_failed` / `close_failed`）が1回ずつ）、続けて`REOPEN_ATTEMPTS`（3）回まで。開いたworkspaceでwrapperが`registration_timeout`のうちに登録しなければ`session_reopen_failed`（`cause: registration_timeout`）を記録し、次の試みはそのworkspaceを先に閉じる（同じrunの依頼を2つのwrapperが取らない）。cmuxそのものが止まったときは開き直しも失敗しうるので、次の試みは前の試みから`WorkspaceBackend::reopen_interval`（60秒）を空ける（最初の試みはすぐ）。試みの状態（開いたworkspaceの登録待ち、前の試みの時刻、失ったwrapperのexit code）はsupervisorのprocessの中（`Supervisor::reopens`、runごと）だけにあり、slotに戻ったrunのwrapperが生きているのを見たら消す。引き継ぎやadoptの後に、wrapperの行が無く自分の状態も無いrunを見たprocessは、前のprocessの試みのwrapperがまだ起動中かもしれないので、`registration_timeout`を待ってから次の試みに進む。
- **上限を超えたとき**: 今の経路に渡す。wrapperの行が残っていれば（開き直したwrapperが登録した後にまた失われた）今のとおり`failed`か`runtime_error`になる。上限を超えたとき、登録しなかった試みのworkspaceが残っていれば閉じる（後から起動したwrapperが登録しないように）。試みが行を消した後なら、待ちを`session_exited`で終え、slotの`SessionWatch`が失ったwrapperのexit code（死んでいたら1）で`finish_lost_session`を呼び、終了したsessionと同じ後始末（画面の保存だけは無い）の後にrunを`failed`にして復旧job（alert `failed`）にかける。
- **test**: `tests/it/runtime_headless_reopen.rs`の`a_session_lost_during_its_wait_is_opened_again_and_takes_the_answer`（wrapperが待ちの最中に終わり、開き直したsessionが答えをresumeで受けて着地する。`failed`にも復旧jobにもならない）と`a_session_that_cannot_be_opened_again_goes_to_its_recovery_job`（wrapperが登録しない試みが3回続くと`failed`になり復旧jobにかかる）、`a_session_lost_in_the_middle_of_a_turn_is_opened_again_once_the_turn_ended`（turnが生きているあいだは開かずに待ち、終わってから開き直して`lost_turn`のidle markerを書き、答えを配送して着地する）、`a_turn_outliving_its_lost_wrapper_past_its_limit_goes_to_recovery`（turnが上限を過ぎても生きていれば開かずに復旧jobへ）、`the_queue_forgets_only_the_lost_wrapper_and_takes_the_reopened_one`（queueの`clear_lost_session`・`register_resume_wrapper`・`session_reopened`・`finish_lost_session`）、`reopen`のunit test（試みの数え方）、`domain::run`の`only_a_running_run_reopens_its_session_in_a_new_workspace`。

## 復旧jobのalertと操作（決定9）

| alert | どこで | 選べる操作 |
| --- | --- | --- |
| `stalled`、理由`turn_without_receipt` | 生きているrun（最初のsession） | `send_instruction`・`stop_processes`・`resume`・`wait`（`HEADLESS_STALLED_ACTIONS`） |
| `stalled`、理由`permission_denied` | 同上 | 同上 |
| `failed`（`turn_finished`の`outcome`が`silent`・`timed_out`・`launch_mismatch`・`failed`） | 終わったrun | `retry`・`retry_inherit`・`resume`・`wait`（`ENDED_ACTIONS`） |

- `send_instruction`はinstructionを依頼にした同じsessionのresume（`dagq: the supervisor's recovery job for run ... asks: ...`）、`resume`は生きているrunなら`needs_session`に退けて新しいworkspaceのwrapperでresume、終わったrunなら今のtriageのresume（どちらも同じsessionのresumeで、依頼が解消依頼になる）。`retry` / `retry_inherit`は終わったrunだけで選べる（turnを止めてsessionを終えるのはruntimeが先に行う）。`stop_processes`はrunのprocess（wrapperの子を含む）をpidで止める。`answer_known_dialog`・`close_and_proceed`と、`prompt_waiting`・`stuck_exit`・`long_background`・`idle_process`の画面由来のalertの操作は出さない（ダイアログが無い）。
- runtimeは今と同じく`confidence: high`の`repair`で前提が成り立つときだけ適用する（`send_instruction`はturnが終わってから次の依頼がまだ無いこと）。`escalate`・`low`・上限（alertごとに3回）超えのときだけ`stalled`のask（`reason_category`付き）が開く。復旧jobは画面の代わりに最後のturnの要約（`turns_excerpt`）を読み、`recovery_requested`の`facts`には`turn`（`turn`・`outcome`・`failure`・`permission_denials`）と`nudges`が載る。
- `headless`の`stalled`のaskは、画面もキーも無いことと、答える前にturnの記録（questionの末尾のturns、runのdirの`turns/`、`dagq timeline RUN`）を読むことを書き、選択肢は`wait` / `stop` / `propose`とjobの足したもの（`stall::headless_options`。`intervene`は無く、`stop`は`headless`のaskだけ。対話のrunのaskは`wait` / `intervene` / `propose`のままで、人がworkspaceに`/exit`を打つ）。`wait`は次のturnが終わるまで次のaskを開かない。記録を読んで人に持っていくのは答える前に行うことで、答えの選択肢にしない（task 1179）。`intervene`は画面とキーに手を入れる印で、非対話のsessionには手を入れる経路が無いため。それでも`intervene`（`intervene: ...`の文を含む）が届いたとき（変更前に開いたaskへの答えか、文として打たれたもの）は、supervisorはheldにせず、そのaskの`stall_resolved`の`answered_intervene`を`reopened: true`つきで1回記録してからaskを閉じ、すぐに同じrunの新しい`stalled`のaskを、前のaskのoptionsから`intervene`を除いたもの（`stop`が無ければ`wait`の後に足す）と同じ`reason_category`で開き直す（促しも復旧jobも通さず、通知は新しいaskとして1回）。questionは前のaskのquestionのうちsessionの様子を書いた部分（答えの説明より前。元のalert・秒数・jobの見立て。`stall::situation_of`）をそのまま残し、前のaskが`intervene`と答えられたことを1文足し、答えの説明と末尾のturnsを非対話のもの（`stall::headless_ask_text`）にする。前のaskが見つからないか答えの説明が読めなければ、receiptの無いturnのquestionにする。`intervene`を次のturnとして送らない。slotの外で待つrunでも開き直す（何も送らないので）。記録してから閉じるので、その間でsupervisorが止まれば、引き継いだsupervisor（adopt）は閉じていない回答済みの`intervene`のaskを適用済みとして読まずにwatchの`asked`にし、次のpollで記録を重ねずに閉じて開き直す。閉じた後・開く前に止まれば、最後の`stalled`のaskが`reopened: true`の`answered_intervene`で閉じられていてそれより新しいaskが無いことから開き直しを読み、次のpollで1回だけ開く。supervisorが適用する前に誰かが`intervene`のaskを閉じたとき（supervisorの居ない間を含む）も、heldにせず、結末が無ければ`answered_intervene`（`reopened: true`）を記録して開き直す。開き直しはaskが開くまでwatchに残り、開けなければ次のpollでやり直す。`reopened`の結末の後にsessionへ何か送られていれば（`turn_requested`・`ask_delivered`・`stall_nudged`）、adoptはそれを開き直しとして読まない。adoptは非対話のrunでは`answered_intervene`を人が手を入れた時刻（held）として読まない。復旧jobがescalateした`long_background`・`idle_process`の`stalled`のaskも、非対話のrunでは同じoptions（`intervene`無し、`stop`あり）にし、questionはworkspaceと画面・キーの手順を書かずに、alertの様子とjobの見立ての後に非対話の答えの説明と末尾のturnsを置く（`headless_ask_text`）。`stop`は、supervisorがそのsessionに終了の依頼（`submit`の`Input::Exit`。上の「終了」と同じ`turns/exit`）を1回だけ書き、askを閉じて`stall_resolved`の`answered_stop`を記録する（task 1104）。`stop`は指示の文として次のturnに送らない。wrapperは走っているturnを止めて（`outcome: stopped`）exit code 0で終わり、runはreceiptの無いまま終わったsessionと同じく、validatingがreceiptの欠けで`failed`にして復旧job（alert `failed`）にかかる。`turns/exit`を書いてからaskを閉じるので、その間でsupervisorが止まっても、引き継いだsupervisorは`turns/exit`を見て（`exit_requested`）書き直さず、askを閉じて`answered_stop`を記録するだけにする。adoptは`answered_stop`の時刻を人が手を入れた時刻として読み、促しもaskも出さない。それ以外の答えは`answer to ask N: <答え>`を依頼にして次のturnにし（`stall_resolved`の`answered_instruction`）、askを閉じる。runが待ちでslotの外にいる間は送らず（`stop`の終了の依頼も同じ）、slotに戻ってから送る（決定6）。非対話のsessionは待ちのあいだ動かないので、この2つの答えは待ちを`answered`で終え、runはslotが空けば戻る（[待ち](waiting.md)、task 1104）。この答えはちょうど1回だけ送る（task 863）: 依頼の`what`はaskを名指す`answer of the stalled ask N`（`stalled_answer_what`）で、supervisorは依頼を書いてから`close_ask`と`stall_resolved`を記録する。書く前に`turns/`に同じ`what`の依頼（取られていないものか取られたもの。前のsessionとともに捨てられた`.dropped`は数えない）があれば（`requested`）書かず、askを閉じて`answered_instruction`を記録するだけにする。依頼のfileが送ったことの記録なので、依頼を書いた後・askを閉じる前にsupervisorが止まれば、引き継いだsupervisor（adoptもexecの引き継ぎも、閉じていない回答済みのaskを`StallWatch`がもう一度読む）は依頼を見つけて2回目を書かず、依頼を書く前に止まれば1回書く。`turn_requested`のeventは依頼の後に書き、その記録の失敗は注記だけなので、eventではなくfileで照合する。

## workerへの文面

非対話のrunのworkerに送るprompt・依頼・答え・復旧jobの指示は、`/exit`・画面への打ち込み・backgroundの処理に頼る指示を持たず、「このturnで終え、receiptかaskでturnを終える」「答えは次のturnのpromptで届く」「AGENTS.mdを読む」「`pkill` / `killall`を使わない」を書く。Codexはsubagent reviewをせず、taskの要る`subagent_review`はCodexのrunには要らない。どれも[Prompt](prompt.md#経路とproviderごとの文面)にまとめる（task 817）。

## 対話の経路との違い

対話のrunの経路（画面の判定・idleの印・`/exit`・打ち込み・Enterの送り直し・既知のダイアログ・`prompt_waiting`・`stuck_exit`）はそのまま（決定7）。Claudeの既定は非対話で、対話の経路は`add` / `edit`の`--interactive`で選んだtaskだけが使う（ADR-t1340-1。保存の形と、既定の`interactive`をNULLに戻したmigration 0057は[provider-lifecycle](../provider-lifecycle.md#workerのproviderと経路)）。`headless`のrunだけが上の経路を通り、分岐はsupervisorの`headless(run)`とwrapperの`worker_mode`で行う。

## 予定: workspaceなしのbackgroundのwrapper

[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)（goal 89）の予定で、**まだ実装していない**。今は上のとおりwrapperはrunのcmuxのworkspaceの中で動く。下の欄名・event・file名・CLIの綴りは実装のtaskが決める仮のもので、実装したらこの節を今の姿に書き直す。

- **設定**: `dagq.toml`の`[headless] wrapper = "workspace" | "background"`（仮）。既定は`workspace`。supervisorはsessionのwrapperを起動する時点（最初のsession・`needs_session`のresume・下の開き直し）で読み、動いているwrapperは動かさない。reviseは生きているsessionへの依頼なので関係しない。古い固定バイナリは知らない欄で起動できないので、この repositoryの`dagq.toml`に足すのは固定バイナリが対応してから（[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)）。非対話のruntimeのplanner（goal 87、ADR-t1394-2）も同じ設定に従う。対話のworker・inbox・人が開くplannerは対象外。
- **起動**: `background`ではsupervisorはworkspaceを作らず、wrapper（`session`。`--resume`を含む）を自分の子でない切り離したprocessとして起動する（`setsid`で新しいsessionとprocess groupを持ち、supervisorはwaitせず、親は1になる）。stdinは`/dev/null`、stdout・stderrはrun dirのlog。wrapperはTTYを確かめず（今はTTYを要る）、`workspace_id`の保存を待つ代わりに、supervisorが起動を記録する（`wrapper_launched`（仮）: `pid`・`started_at`（OSの起動時刻）・`pgid`・`log`）のを待ってから登録する。supervisorは起動の直後に、その記録から`run_processes`のwrapperの行（`pid`と起動時刻）を作り、wrapperの登録より前でも下の識別でadoptと停止ができるようにする。launchd modeではLaunchAgentの停止がwrapperを巻き込まないこと（plistの`AbandonProcessGroup`）、in-cmux modeではsupervisorのworkspaceのcloseのhangupが届かないことを実装が確かめる。起動できなければ（processを作れない）provisioningの失敗として扱う（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)決定9の読み替え）。
- **env**: 今workspaceの`--env`で渡す`DAGQ_ROLE`・`DAGQ_QUEUE`と`[run.env]`（[Run environment](run-environment.md)）を、wrapperのprocessのenvで渡す。agentのenvの組み立て（queue serviceのsocketとtoken、`DAGQ_QUEUE`を外す。[session wrapper](session-wrapper.md)）は変えない。
- **識別と生死**: `run_processes`のwrapperの行に`pid`に加えて起動時刻を持ち、生きているとは「そのpidのprocessが記録した起動時刻のまま居る」こと。heartbeatは今の[wrapperが黙ったsession](silent-wrapper.md)の判定に使う。adopt・引き継ぎ（[Handoff](handoff.md)）・開き直し・掃除はworkspaceのUUIDでなくこの組で行い、起動時刻の合わないpidには何も送らない。
- **停止**: 今workspaceのclose（hangup）で止める経路（reviewの後、`stalled`の`stop`、cancel、復旧jobの`stop_processes`、後始末と掃除、開き直しの前）は、まず`turns/exit`の終了の依頼で終わらせ、`exit_timeout`のうちに終わらなければ、識別したwrapperのpidにSIGTERMを送り、猶予の後も残っていればwrapperのprocess groupと、wrapperが`turn_started`の`pid`で記録したturnのprocess group（turnはwrapperと別のgroupで走る）にSIGKILLを送る（`wrapper_stopped`（仮）: `pid`・`signal`・`reason`）。wrapperはSIGTERMで今の`stop_groups_on_exit_signals`のとおり自分が起動したturnのgroupを止める。ADR-t813-1決定5の「最後のturnの終わりにworkspaceをcloseする」は「wrapperが終了の依頼で終わったことを確かめ、残っていれば止める」になる。
- **出力**: wrapperが今terminalに出す`[dagq]`の要約（turnの開始、agentの文、tool、turnの結果）を、run dirの`session.log`（仮。resumeは試行ごと）に書く。人は`dagq run log RUN [--follow]`（仮）で読み・追う。turnの生の出力は今の`turns/`、時間の流れは`dagq timeline RUN`。runtimeは見るためのworkspaceを開かない。画面で見たい人は自分のterminalでこのCLIを打つか、taskに`--interactive`を選ぶ。
- **workspaceを前提にした記録と判定**（ADR-t1404-1決定10）: backgroundのsessionでは、workspaceのUUIDで行っていた識別・生死の判定をwrapperの識別の組に置き換える。実装が直す箇所の目安: proposalの持ち主のplannerの結び付けと生死（[Plan planners](plan-planners.md)）、`stats`の`running_alerts`の`workspace_mismatch`（backgroundのrunはcmuxの一覧でなくwrapperの生死で見る。[Stats](stats.md)）、`runtime_planner`の区間を推定で閉じる判定（cmuxの一覧でなくwrapperの生死）、`DAGQ_SESSION_KIND`などworkspaceの`--env`で渡していた変数（processのenv）。answerはどれも次のturnの依頼として送る。
- **評価**: 切り替えの前後で、cmuxの呼び出しの失敗（`backend_call_failed`・captureの時間切れ）、閉じた記録の無いworkspace（[zero-based-headless-readiness](../../plans/zero-based-headless-readiness.md)のE1・E3）、startupを比べ、既定を変えるかは別のADRで決める。
