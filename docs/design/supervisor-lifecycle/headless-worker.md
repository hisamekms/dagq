---
id: design-supervisor-lifecycle-headless-worker
type: design
title: "非対話のworker"
status: current
created: 2026-09-28
updated: 2026-10-02
last_verified: 2026-10-02
scope: runtime
related:
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

[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)の実装（task 815）。taskの`worker_mode`が`headless`（`add --headless`、[provider-lifecycle](../provider-lifecycle.md#workerのproviderと経路)）のrunは、workerの1 turnを1回の非対話の呼び出しにする。動くのはClaude（`claude -p --output-format stream-json --verbose`）とCodex（`codex exec --json`と`codex exec resume --json`、task 816。[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。providerの違いは`AgentProvider`の`turn_command`（呼び出しのargv）と`turn_reader`（出力を読む`TurnReader`）と`turn_permission_mode`に閉じ込め、supervisorとsession wrapperの流れはproviderを知らない。

## 経路の全体

- **e2eはworkerのturnで流さない**（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。ClaudeでもCodexでも、e2eが要るrunにはreviewのpassの後にruntimeがhostで流し（[Review](review.md#着地の前のe2e)）、落ちれば同じsessionの次のturnとしてresumeの依頼（`ResumeKind::E2e`）を送る。Codexのworkspace-writeのsandboxで走らないe2eを名前で除外する規則と、その除外を書くreceiptの`e2e`の書式（task 1206）は無い。
- **session wrapperがturnを動かす**（決定3）。runのworkspaceのwrapper（`session`、[session wrapper](session-wrapper.md)）は、runの`worker_mode`が`headless`なら`src/application/headless_session.rs`の`Turns`でturnを1つずつ起動する。最初のturnは`prompt.txt`をpromptにし、sessionを始める（Claudeはsessionのidをrunのidにする`--session-id <run id>`、Codexは出力の`thread.started`でthreadのidを名乗り、wrapperがそれを`turn_session_identified`としてrunに記録する）。後のturnはsupervisorの依頼を1つずつ取り、同じsessionをresumeする（Claudeは`--resume <run id>`、Codexは`codex exec resume <記録したthreadのid>`）。依頼を待つ間もheartbeatを続け、agentのprocessは無い。
- **supervisorは打ち込まずに依頼を書く**（決定2）。supervisorがsessionに送るものはすべて`deliver.rs`の`submit`を通るので、`headless`のrunでは`headless.rs`の`request_turn`が、文をrunの`turns/`への依頼に、`/exit`を終了の依頼に替える（`Submission::Queued`）。送った文の確認（`StartCheck`）、Enterの送り直し、入力欄の確認、画面の読み取り（ダイアログ・作業中・認証の画面・idleの推定・既知のダイアログへの応答）は`headless`のrunでは行わない（`watch_prompt`・`session_idle`・`answer_known_dialog`・`answer_exit_dialog`・`known_dialog_ready`が早く戻る）。
- **turnの終わりがidleの印**。wrapperはturnのprocessが終わり`turn_finished`を記録した後に、runのidle marker（`idle.json`）を`Stop`の形で書く（`hook_event_name: Stop`、background taskは無し、`dagq_turn`に`turn`・`outcome`・`failure`・`permission_denials`）。Claudeの`Stop` hookは使わない（turnのsettingsにhookは無い）。これで最初のsession・reviewのrevise・`needs_session`のresume・待ち（[waiting](waiting.md)）の既存の見張りが、idle markerとreceiptとaskをそのまま読む。
- **終了**: 終了の依頼を見たwrapperは、turnを走らせていれば止め（`outcome: stopped`）、exit code 0で終わる。reviewのpassの後の`/exit`、resumeの試行の終わり、wrapperが黙ったときの`/exit`、`stalled`のaskへの`stop`の答え（下の「turnの後の扱い」）は、どれもこの依頼になる。workspaceのcloseの時点は対話と同じ（決定5）。

## run dirの`turns/`

`src/domain/turn.rs`が名前を決める。supervisorとwrapperはrun dirと`turns/`をlinkを辿らずに開いた記述子でfileを扱う（task 1184）。通常のfileだけを上限付きで読み、書き込みは新規の一時fileからrenameし、turnの出力は新しいinodeの記述子へ渡す。link・FIFOの指す先を読み書きせず、FIFOのopenで待たない。idle markerとreceiptも同じ境界で扱う（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)の「run dirの他のfile」）。

| file | 書く側 | 中身 |
| --- | --- | --- |
| `request-NNNNNN.json` | supervisor | `TurnRequest`（`seq`・`what`・`prompt`）。一時fileからrenameで置く。`seq`は`turns/`の依頼（取られたものを含む）の最大の次 |
| `request-NNNNNN.taken.json` | wrapper | 取った依頼（renameで印を付ける）。wrapperは取られていない依頼を`seq`の順に1つずつ取る |
| `request-NNNNNN.dropped` | supervisor | 前のsessionが取らずに終わった依頼。新しいsessionの前に捨てる（下の`prepare_turns`）。番号は取られた依頼と同じく数え、使い回さない |
| `exit` | supervisor | 終了の依頼 |
| `limits.json` | supervisor | turnの上限（`silence_secs`・`limit_secs`。testが秒未満で入れたときは`silence_ms`・`limit_ms`も持ち、秒の代わりに使う。[Stall thresholds](stall-thresholds.md)）。`[stall]`から |
| `turn-NNNNNN.jsonl` / `.err` | agent | turnのstdout（providerのJSONL）とstderr |
| `../ask-requests/<id>.json` / `.taken` | Codexのworkerの`dagq ask` / supervisor | askの要求と、取り込んだ印（run dirの直下の`ask-requests/`。[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)） |

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

queueのdirなどworkerが書けない場所と、workerが書くrun dir（直下の`turns/`・`broker/`など）を区別する。pathにqueueの`runs/`の下の部分（run dirとその中）があるときだけ（`agent_dir::in_run_dir`）、`LocalRunFiles`と`agent_dir`の`create_file`・`append`は記述子の操作を使う: ディレクトリの初回openが指定したdirとその親の2段をlinkとして拒み、最後の要素のfileもlinkを辿らず（`O_NOFOLLOW`と`AT_SYMLINK_NOFOLLOW`）、通常のfileだけを64MiBまで読み、書きはlinkを置き換える。それ以外のpath（queueのdirとDB、installしたバイナリやbrokerのclient、macOSの`/tmp`、linkにしたdata dir、scratchpad）はhostのもので、`std::fs`と同じくlinkを辿り、上限も当てない。`in_run_dir`はpathだけで決め、最初の`runs`という名前の要素の下を run dir とみなすので、queueより上に`runs`というdirがあるhostのpath（`/Users/x/runs/project/...`）もrun dirの扱い（linkを拒むだけで、辿る範囲は広がらない）になる。任意の深さのpathを安全にするAPIではない。`LocalRunFiles::copy`の元（runtimeのバイナリ）はruntimeのもので、linkを辿って読み、64MiBの上限を当てない。Claudeのdebug logのhookの失敗は`RunFiles::read_tail`で末尾だけを読むので、64MiBを超えるlogでも見つかる。treeの走査は開いたdirから`openat`で子へ進む。`ask-requests/`は既存の`open_agent_dir`と専用の上限・拒否のeventを使う。

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

- **Codexのworkerのask**: sandboxの中の`dagq ask`はqueueに書けないので、run dirの`ask-requests/`への要求になり、supervisorが見張りのpassの最初に検査して開く（dirはlinkを辿らずに開いた記述子に対してだけ扱う。取り込まれていない通常のfileの要求があるうちはidle markerでturnの終わりを判じない）（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)、ADR-t813-3の決定3）。開いたaskは下のaskと同じに扱う。
- **成功**（receiptかaskが残ったturnを含む）: wrapperは次の依頼を待つ。supervisorはidle markerを読み、receiptがあれば今のvalidatingへ（[receipt and session exit](receipt-and-session-exit.md)）、`worker_question`が開いていれば答えを待つ（runはslotを空けて待ちになる。ADR-0071の読み替え、決定6）。答えは`answer to ask N: ...`を依頼にして送る（[workerの質問への回答の送信](worker-question-answer.md)）。
- **receiptもaskも無いturnの終わり**（`StallWatch::observe_turn`）: 対話の`idle_without_receipt_secs`は待たず（閾値0）、決まった文の促し（`stall_nudged`）を依頼で送る。促しは1 phaseに`HEADLESS_NUDGES`（2）回まで（対話は1回。ADR-0047決定30の読み替え）。前の促しの`stall_resolved`は`nudged_again`になる。使い切った後のturnも同じなら復旧jobの`stalled`（理由`turn_without_receipt`）にする。
- **permissionの拒否が続いて進まない**: receiptもaskも無く終わったturnの`permission_denials`が`PERMISSION_DENIAL_LIMIT`（3）件以上なら、促さずにすぐ復旧jobの`stalled`（理由`permission_denied`）にする。
- **providerが使えない**（`failure`が`authentication` / `usage_limit` / `launch`）: wrapperはsessionを終えずに次の依頼を待ち、supervisorは促しも復旧jobもせず、失敗したturnの呼び出しをもう一方のproviderの新しいsessionへの依頼にする（ADR-t813-2。[provider-lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。切り替えられない（もう一方も使えない、切り替えの上限）ときはrunを失敗にせず待たせ（`provider_waiting`）、自分のproviderの控えが解ければ同じsessionへもう一度送る（`provider retry`）。Claudeの認証と利用上限、および両方使えないときは、`authentication`ならqueueの認証のholdに、`usage_limit`なら利用上限のhold（`reason_category: cost`、subject `usage_limit`）に、対話のsessionの画面と同じ`raise_wall`で加わる（`auth_required` / `usage_limited`を記録する。task 438）。人が`done`と答えると、対話と同じくsupervisorが「続けて」（`continue`）を依頼で送る（[queue hold](queue-hold.md)）。reviseとresumeの段でも同じ（`SessionWatch::provider_wall`）。
- **turnの失敗と、wrapperが止めたturn**（`other`・`model`の失敗、`silent`・`timed_out`・`launch_mismatch`）: wrapperはsessionを終える（exit code 1）。supervisorはそのturnを促さず、wrapperの終了を待つ。receiptの無いまま終わったrunは`failed`になり、今の[triage](triage.md)の復旧job（alert `failed`）にかかる。復旧jobの材料（`ended_run_material`）には、最後の5つの`turn_finished`（`outcome`と`stopped`）が載る。

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

対話のrunの経路（画面の判定・idleの印・`/exit`・打ち込み・Enterの送り直し・既知のダイアログ・`prompt_waiting`・`stuck_exit`）はそのまま（決定7）。`headless`のrunだけが上の経路を通り、分岐はsupervisorの`headless(run)`とwrapperの`worker_mode`で行う。
