---
id: design-supervisor-lifecycle-receipt-and-session-exit
type: design
title: "Receipt and session exit"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1433-2
  - design-supervisor-lifecycle
  - adr-0027
  - adr-0022
  - adr-t803-1
  - adr-t947-3
  - adr-t1504-2
  - adr-t1487-1
---

# Receipt and session exit

workerの対話のrunの処理は無い（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
wrapperはbackgroundだけで動き、runはworkspaceを開かない（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。

receipt の受領と wrapper の終了は別の事象である。
worker は receipt を一時ファイルから rename して公開し、wrapper は turn の終わりを `turn_finished` と idle marker に記録する。

1. **idle 判定**: turn の process が終わった後に wrapper が `idle.json` を書く。
   receipt より新しい idle marker を見て、session を開いたまま validation と review に進む。
2. **終了要求**: verdict が終了を求めるとき、`exit_requested` を記録して `turns/exit` に終了依頼を書く。
   wrapper は走っている turn を止めて終了する。
3. **終了確認**: wrapper の `session_exited` または失った wrapper の生死を確認して次へ進む。
   終わった後は wrapper の残りを handle で止め（`stop_run_session`。[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3）、`workspace_closed`を記録する。
   ADR-t1433-3 より前に workspace で開いた session の ID は閉じず、cmux にも聞かずに人に任せる。
   run の画面は読み取らない。
4. **idle より先に見た wrapper の終了**: turn が失敗して wrapper が終わったのを idle marker より先に見たときは、その時点で receipt の有無を読む。
   receipt があれば終了コードによらず validation に進み、receipt が無く非 0 なら `failed` にする。
   1 と同じ結果になり、見た順で変わらない（[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)、判定は`domain::run::session_end_status`）。

### `/exit`の再試行<a id="exitの再試行"></a>

worker に `/exit` を打たず、打鍵の再試行・画面のダイアログの応答・時間切れの `stuck_exit` の ask と復旧 job は無い。
過去の `exit_retried`・`exit_request_timed_out`・`stuck_exit` の記録は履歴として読める。
planner には何も打たない（人の planner の行は cmux を呼ばずに閉じる）。

## 画面からのidleの推定

runtimeはどのsessionの画面も読まず、画面からidleを推定しない（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)が[ADR-t803-1](../../adr/2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)をsupersededにし、inboxへの知らせも画面を読まない。[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)）。
idleはwrapperのidle markerだけで判じる。
過去の`idle_inferred`のeventと`screen-idle.json`は読めるまま残り、新しくは書かれない。

- **最後の入力**: idle markerは最後の入力より新しいときだけturnの終わりを示す。
  最後の入力は`screen_idle::last_input`が読む。
- **workerのsession**<a id="workerのsession"></a>: `Supervisor::session_idle` は wrapper の idle marker だけを読む。

## 黙ったwrapper<a id="黙ったwrapper"></a>

- wrapperのheartbeatが古くても、wrapperが生きていればsessionは生きていて、supervisorはkillしない（判定は`wrapper_pulse`）。
  生きていれば沈黙ごとに1回`wrapper_heartbeat_expired`を記録して終了の依頼のfileを書き、heartbeatが戻れば印を消す。
- 終了を記録した後なら次のpollで終了を扱い、終了の記録の無いまま居なくなっていれば失ったsessionとして復旧する。
- 生きているかは記録したhandleのpidと起動時刻で判じ、heartbeatの古さは生死に使わない（[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)）。
- wrapperは終了の記録を一時的な失敗で失わないよう再試行し、その合計の待ちはsupervisorが黙ったとみなすまでの時間に収める（`application::session`の`record_exit`）。
- 落とし穴: wrapperのheartbeatとrun_eventsの`created_at`は実時計で書かれ、supervisorは注入した`Clock`と比べる。
  wrapperの居る`supervise`のtestで固定時刻の`Clock`を使うと生死を誤るので、固定するのはwrapperの居ない経路だけにする。
  receiptやidle markerのmtimeと比べる時刻も、ファイルの時刻と揃うよう実時計から読む。

## workerへの答えの配送<a id="workerへの答えの配送"></a>

[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定2。
runtimeがworkerのsessionに渡すのは終了の依頼・resumeの解消の依頼・reviseの依頼と`worker_question`の答えだけで、どれも次のturnの依頼としてrun dirの`turns/`に書く。

- 答えはturnの間（最後に始まったturnの終わりをidle markerが示すとき）に送り、そのときはmarkerとaskの時刻を比べない。
  turnの後に開くask（queue serviceより前に起動したCodexのturnのask）があり、markerがaskより古くなりうるため。
- 依頼を書けたら`ask_delivered`とaskのcloseを同じtransactionで記録する。
  書けなければ`ask_delivery_failed`だけを記録してaskを閉じず、再送しない（inboxのattentionになる）。
  書いてからcloseまでの間にsupervisorが死んだときだけ、引き継いだsupervisorが同じ答えをもう一度送りうる。
- runtimeが届けるrunへの答え（`domain::session_takes_answers`）はattentionにせず、届ける者の居ない答えは、答えをworkerに送ってaskを閉じるattentionになる。
- 配送の前に閉じた`worker_question`の答えは送らず、閉じたことを1回だけ伝える（[receiptの無いidleの検知](idle-without-receipt.md)）。

## follow_upsの分類コード<a id="follow_upsの分類コード未実装"></a>

[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)の決定。
runのreview（[Review](review.md#差し戻しの分類コード)）と同じ種類の問題には同じ名前を使う。

- **receiptの形**: `follow_ups`の各要素に`category`（コード1つ）を足す。
  `category`の無い要素と一覧に無い値もreceiptの受理を変えない（validationは拒まない）。
- **記録**: `integrate`がdraftを登録するとき（[integrate](integrate.md)の10）、`follow_up_registered`のpayloadとdraftの出どころの`material`に`category`を載せる（欠けの判定は`domain::follow_up::follow_up_category`）。
  draftから出るeventの出どころの欄にも`category`が載る。
  runtimeのplannerとの突き合わせは[Draft planners](draft-planners.md#follow_upの種類と判断の集計)。
- **workerへの見せ方**: workerのprompt（[prompt](prompt.md)）のreceiptの例は`follow_ups`の要素に`category`を持ち、promptは`FOLLOW_UP_CATEGORIES`（`src/domain/follow_up.rs`。下の表と同じ一覧と短い定義）から一覧と付け方を1行で出す。
  一覧を変えるときは表と`FOLLOW_UP_CATEGORIES`を一緒に変える。
- **付け方**: 迷ったら、follow_upを片付けたときに何が変わるかで選ぶ（runtimeの挙動が直る → `defect`、testが安定する → `flaky_test`、文書が実装に追いつく → `docs_drift`）。
  `flaky_test`と`test_gap`はdescriptionにtestの名前（`<module>::<name>`）を書く。
  重複や採否はコードにしない（ADR-t947-3決定2）。

| コード | 定義 |
|---|---|
| `defect` | runtime（またはscript）が決まった仕様どおりに動かない経路。再現の条件か、コードの場所と誤りを書く |
| `flaky_test` | 既存のtestが負荷や順序で時々落ちる・時間切れになる |
| `test_gap` | 経路にtestが無い、またはtestの作りが弱い（上限の無い待ち、実時計への依存）。今は落ちていない。runのreviewの`test_gap`と同じ種類 |
| `docs_drift` | 文書（design・ADRの索引・pluginのskill・AGENTS.md）が実装かacceptedのADRとずれている。runのreviewの`docs_drift`と同じ種類 |
| `remaining_scope` | acceptedのADRかgoalが決めた事のうち、このtaskで実装しなかった残り |
| `improvement` | 仕様の誤りではないが、よくする案（観測・集計の追加、refactor、速さ、使い勝手） |
| `measurement` | 着地の後か期間の後に数えて確かめる依頼 |
| `decision` | 人かplannerの判断が要る問いで、作業の中身が決まっていない |
| `ops` | repositoryの変更ではなく、人かinboxがhost・本番queue・外部サービスで行う作業 |
| `other` | どれにも当たらない。descriptionで説明する |

## follow_upsの所属の提案

[ADR-t1504-2](../../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定11と[ADR-t1504-1](../../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)決定5。
workerはfollow_upごとに、元goalのacceptanceとの関係を提案として書けるが、確定はしない（所属の判断を記録するのはruntimeのplannerと人。[所属の判断](../follow-up-membership.md)）。

- **receiptの形**: `follow_ups`の各要素に任意の`membership_proposal`を足す。
  形はworkerのpromptのreceiptの例が、3つの欄の意味は`FOLLOW_UP_PROPOSAL`が持つ。
  問題と根拠は`description`に書く。
  欄の無い要素も、未知の分類や形の違う値もreceiptの受理を変えない（`category`と同じ。ADR-t947-3決定3）。
- **記録**: `integrate`の登録（[integrate](integrate.md)の10）が、`follow_up_registered`のpayloadとdraftの出どころの`material`に`membership_proposal`を書いたまま載せる（`domain::follow_up::follow_up_membership_proposal`）。
  登録しなかった項目は`follow_up_registered`に残る。
  runtimeは提案を判断として扱わず、`follow_up_judgements`の行も書かない。
- **workerへの見せ方**: workerのprompt（[prompt](prompt.md)）のreceiptの例は`follow_ups`の要素に`membership_proposal`を持ち、`FOLLOW_UP_PROPOSAL`（`src/application/prompt/`）が、提案に留めて自分で判断も移動もしないことを言う。
  resumeの解消依頼とreviseの依頼は手順5の受け入れ条件の対応づけの後に同じことの短い形を足す。
- **plannerへの見せ方**: runtimeのplannerのpromptはfollow_upのdraftごとの節にworkerの提案（無ければ無いこと）を載せ、提案を起点に所属を判断して記録する手順を持つ（[Draft planners](draft-planners.md#所属の判断)）。

## Spikeの結果

[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定3・4のとおり、実行区分（`domain::execution_class`）が`spike`のtaskのrunは、結果をreceiptの`spike_result`に書く。
欄とその意味は`domain::execution_class`のdoc commentが持つ。

- 検査するのはSpikeのrunだけで、実装の区分のrunの`spike_result`は受理を変えない。
- 見るのは欄がそろっていることだけで、根拠が判定を支えるかは判じない。
  否定的な結果と未解決は欠けではなく、成功のreceiptで返す。
- 上限（時間・回数）はagentへの指示で、runtimeは強制しない（止めるのは既存のturnとrunの上限だけ）。
- 欠けの扱いは要求evidenceと同じで、[Validation](validation.md)の9のとおり`needs_session`にしてresumeが書き足させる。
- receiptは書いたまま`validation_finished`と`integration_receipt`に残るので、結果を読む側はそこから読む。

## baseがすでに満たすとき（already_satisfied）

[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)決定1のとおり、workerはbaseが受け入れ条件をすでに満たし差分が要らないとき、commitせずにreceiptの`result`を`already_satisfied`にする。

- **根拠を必須にする**: 主張はbaseで行った確かめを根拠として`base_checks`に持ち、根拠の無い主張は受け付けずrunを`failed`にする。
  主張はcommitしないことと一体で、commitしたrunの主張は受け付けない。
  欄の形と受理の検査は`Receipt`のdoc commentが持つ。
- **根拠の範囲**: workerが書くのは[local-checks](../../development/local-checks.md)の「workerの手元の検証」で許された確かめだけ。
  receiptの検査はコマンドの中身を判じないので、coverageの関門や全体の`cargo test`が無いことを理由に拒まない。
  taskのverification_commandsの全体はruntimeがsnapshotで流す（[Validation](validation.md)の「already_satisfiedの検証」）。
- **workerへの見せ方**: workerのpromptの`ALREADY_SATISFIED`（`src/application/prompt/`）がreceiptの規則の後に1段落で言う（[prompt](prompt.md)の「受け入れ条件の対応づけ」）。
