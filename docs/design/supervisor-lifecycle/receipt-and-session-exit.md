---
id: design-supervisor-lifecycle-receipt-and-session-exit
type: design
title: "Receipt and session exit"
status: current
created: 2026-09-26
updated: 2026-10-06
last_verified: 2026-10-06
scope: runtime
related:
  - adr-t1433-2
  - design-supervisor-lifecycle
  - adr-0027
  - adr-0022
  - adr-t803-1
  - adr-t947-3
  - adr-t1504-2
---

# Receipt and session exit

worker の対話の run の処理は task 1437 で撤去した（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。task 1440 から wrapper は background だけで動き、run は workspace を開かない（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。

receipt の受領と wrapper の終了は別の事象である。worker は receipt を一時ファイルから rename して公開し、wrapper は turn の終わりを `turn_finished` と idle marker に記録する。

1. **idle 判定**: turn の process が終わった後に wrapper が `idle.json` を書く。receipt より新しい idle marker を見て、session を開いたまま validation と review に進む。
2. **終了要求**: verdict が終了を求めるとき、`exit_requested` を記録して `turns/exit` に終了依頼を書く。wrapper は走っている turn を止めて終了する。
3. **終了確認**: wrapper の `session_exited` または失った wrapper の生死を確認して次へ進む。終わった後は wrapper の残りを handle で止め（`stop_run_session`。[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3）、`workspace_closed`（`workspace_id` は handle）を記録する。ADR-t1433-3 より前に workspace で開いた session の ID は閉じず、cmux にも聞かずに人に任せる。run の画面は読み取らない。
4. **idle より先に見た wrapper の終了**: turn が失敗して wrapper が終わった（exit code 1）のを idle marker より先に見たときは、その時点で receipt の有無を読む。receipt があれば終了コードによらず validation に進み（`supervision_finished` は `exit_code` と `receipt: true`、`last_error` は書かない）、receipt が無く非 0 なら `failed` にする。1 と同じ結果になり、見た順で変わらない（[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)、手順は [supervise](supervise.md) の手順 8）。

### `/exit`の再試行<a id="exitの再試行"></a>

worker の `/exit` の打鍵・再試行・画面のダイアログの応答・時間切れの `stuck_exit` の ask と復旧 job は撤去した。過去の `exit_retried`・`exit_request_timed_out`・`stuck_exit` の記録は履歴として読める。人の planner への対話の送信は [session-send](session-send.md) のとおり残る（task 1577 まで）。runtime の planner には task 1441 から何も打たない。

## 画面からのidleの推定

以下は残る人の planner（`planner_view`、task 1577 まで）と inbox の促し（task 1442 まで）の処理だけに当たる。worker の run と runtime の planner の画面は使わない（runtime の planner は task 1441 から非対話の turn だけで、wrapper の idle marker で判断する）。

[ADR-t803-1](../../adr/2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)。idleの印（`idle.json`）は主な信号のまま残し、印が無いか最後の入力より古いときだけ、画面（`cmux capture`）からidleを推定する。判定はproviderにもsessionの種類にも依らない`application::screen_idle`の1つにまとめ、人のplannerの`planner_view`（[Plan planners](plan-planners.md)）と inbox の促しが使う。worker の画面の推定は task 1437 で撤去した。ADR-t803-1 は [ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md) が superseded にし、引き継ぐ決定は無い。runtime の planner の画面の推定は task 1441 で撤去した。人の planner と inbox の画面の推定は、それを消す task 1577・1442 までの今の実装として残るだけで、それを支える ADR は無い。

- **印が判断できないとき**: 印が無い（`missing`）か、印のmtimeがsessionの最後の入力より古い（`stale`）。最後の入力は`screen_idle::last_input`が、agentの入力の印`prompt-submit.json`のmtime、supervisorが打った文の印`supervisor-input.json`（`SUPERVISOR_INPUT_FILE`。supervisorが文をsessionに打つ直前に`record_supervisor_input`が書く。人のplannerではreviseの配送とplanner_questionのanswerの配送。`/exit`は書かない）のmtime、sessionを開いた時刻（plannerは行の`created_at`）の遅い方として読む。印が最後の入力より新しい間は、今までどおり印で判断する（印の`background_tasks`に`running`があれば`working`）。
- **1回のcaptureの見え方**（`screen_idle::look` / `look_of`）: 入力欄が入力を受けられ（`AgentSignals::input_ready`）、作業中の表示が無く（`AgentSignals::working`）、ダイアログも無い（`AgentSignals::detect_prompt`）なら`Idle`で、`AgentSignals::transcript`の指紋と、画面にbackgroundの処理が動いている表示があるか（`AgentSignals::screen_background`。task 823）を持つ。`screen_background`の既定は`None`（画面から読めないprovider。backgroundの処理が無いものとして扱う）で、Claude adapter（`claude::background_on_screen`）は入力欄の下の行（status line）を`·`で区切った項目のうち、0より大きい数とbackgroundの処理の種類（`shell(s)`・`background task(s)`・`monitor(s)`・`agent(s)`、`BACKGROUND_COUNTS`）の組（例 `⏵⏵ auto mode on · 3 shells · ← for agents · ↓ to manage`。2026-09-28のClaude Codeで確認）があれば`Some(true)`、無ければ`Some(false)`を返す。数の無いhint（`← for agents`）と入力欄より上のtranscript（`3 shells still running`）は読まない。入力欄があり、`detect_prompt`の読むダイアログが無く作業中の表示があれば`Working`（task 845。captureの間隔を延ばすためだけに`Busy`と分ける）、それ以外（ダイアログ、入力欄が無い）は`Busy`、captureの失敗は`Unreadable`。
- **区間**（`screen_idle::observe`、`Observation`）: `Idle`のcaptureは、最後の入力より後に始まり同じtranscriptで同じbackgroundの表示の区間を延ばし（backgroundの処理が終わって表示が消えれば新しい区間になり、そこから`screen_idle_secs`を数え直す）（前のcaptureより後のミリ秒のときだけ`captures`を1つ足す。区間の時刻と最後の入力はUnixミリ秒で比べる（task 1045）。`screen-idle.json`の`first_seen_ms` / `last_seen_ms`で、秒で書いた古いfileは読まずに区間をやり直す）、そうでなければ新しい区間を始める。`Working`と`Busy`は区間を終わらせ、`Unreadable`は区間をそのままにする。supervisorは区間をメモリ（`Spans`）に持ち、idleの印の隣の`screen-idle.json`（`SCREEN_IDLE_FILE`。`first_seen_ms`・`last_seen_ms`・`captures`・`transcript`・`background`・`recorded`。`background`の無い古い写しは`null`として読む）には写しを書けるときに書く。ディスク満杯で写しが書けなくても区間は失われない（写しは読むだけのコマンドと、引き継いだ次のプロセスがメモリに無い区間を読むためのもの）。
- **推定**（`ScreenProbe::infer`、`Observation::idle`）: 今のcaptureが読め、区間の`captures`が2以上で、`last_seen_ms - first_seen_ms`が`[stall].screen_idle_secs`（testが秒未満で入れた値はそのミリ秒）（既定120秒、`DEFAULT_SCREEN_IDLE_SECS`。[Stall thresholds](stall-thresholds.md)）以上ならidleと推定し、idleの始まり（`idle_since`）を区間の`first_seen`に、推定の`background_running`を区間の`background`にする。今のcaptureが読めなければ推定しない（`working`）。supervisorは区間を持って写しを書き（`ScreenIdle::Record(&Spans)`）、読むだけのコマンド（`dagq planners`）は書かずに次のcaptureとして判断する（`ScreenIdle::Peek`。閾値はmain checkoutの`dagq.toml`の`[stall]`、読めなければ既定値）。
- **記録**: supervisorは区間ごとに1回、queueのevent `idle_inferred`（`source: "screen"`、`marker`（`missing` / `stale`）、`since`（Unix秒）、`since_ms`（Unixミリ秒）、`observed_secs`、`observed_ms`、`captures`、`background_running`（画面から読んだbackgroundの表示の有無。読めないproviderは`null`）、plannerでは`planner_id`・`origin`・`workspace_id`）を記録し、区間の`recorded`を立てる（`Spans::mark_recorded`）。eventを書けなかったとき（queueのエラー）はlogに残して他のplannerの扱い（reviseやanswerの配送、`/exit`）を止めず、次のpassで記録し直す。agentのdebug log（plannerは`<planner dir>/claude.log`、`PLANNER_DEBUG_LOG`）の末尾64KiBにidleのhookの失敗の行があれば（`screen_idle::hook_failure`、`AgentSignals::idle_hook_failure`。Claude Codeでは`Hook Stop`と`error`を含む最後の行）、その先頭300文字を`hook_error`に入れる。`dagq events --kind idle_inferred`で読める。
- **backgroundの処理**（task 823）: 画面にbackgroundの処理の表示がある推定は、印の`background_tasks`に`running`があるときと同じに扱う（区間を切って`working`にする形は取らなかった。planner の background 表示を working として待つため）。plannerは`working`のままで（`PlannerProbe::screen_idle`の`IdleProbe::background_running`）。表示が消えれば新しい区間が`screen_idle_secs`続いたところでidleになる。backgroundの表示のある区間も`idle_inferred`（`background_running: true`）に記録し（`dagq planners`の`idle_inferred`も`working`のまま出す）、表示が消えた後の区間は別に`background_running: false`で記録する。表示を読めないprovider（`screen_background`が`None`）では今までどおりbackgroundの処理が無いものとして推定する。
- **限界**: 印が最後の入力より古いだけ（turnを始めないslash commandの後など）のplannerは、以前はすぐ`idle`だったが、今は`screen_idle_secs`の間`working`に見える。
- **workerのsession**<a id="workerのsession"></a>: `Supervisor::session_idle` は wrapper の idle marker だけを読む。run の画面からの推定・capture の間隔・`idle_inferred` の記録は撤去した。
- **test**: `screen_idle.rs` と Claude の汎用の画面判定の unit test、人の planner の `person_planner_screen_idle`、inbox の `inbox_nudge` は残る。worker の画面と runtime の planner の画面を確かめる integration test は撤去した（runtime の planner は task 1441）。

## follow_upsの分類コード<a id="follow_upsの分類コード未実装"></a>

[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)の決定（task 954で実装）。一覧はtask 951の分析（[follow-up-kinds](../../plans/follow-up-kinds.md#ラベル)）を元に、runのreview（[Review](review.md#差し戻しの分類コード)）と同じ種類の問題の名前を揃えた。

- **receiptの形**: `follow_ups`の各要素に`category`（コード1つ）を足す: `{"title", "description", "category"}`。欄名は`kind`にしない（task 984で消したtaskの`kind`と同じく、名前の重なるeventやaskのkindと混ざらないようにするため）。`category`の無い要素と一覧に無い値もreceiptの受理を変えない（validationは拒まない）。
- **記録**: `integrate`がdraftを登録するとき（[integrate](integrate.md)の10）、`follow_up_registered`のpayloadとdraftの出どころの`material`に`category`を載せる（欠けは`unlabeled`、一覧に無い値はそのまま）。欠けの判定は`src/domain/follow_up.rs`の`follow_up_category`（`category`が空でない文字列でなければ`unlabeled`、前後の空白は落とす）。draftから出る`draft_planner_opened`・`draft_planner_settled`などの出どころの欄（`origin_fields`）にも`category`が載る。runtimeのplannerとの突き合わせは[Draft planners](draft-planners.md#follow_upの種類と判断の集計)。
- **workerへの見せ方**: workerのprompt（[prompt](prompt.md)）のreceiptの例は`follow_ups`の要素に`category`を持ち、`follow_up_categories_line`（`src/application/prompt.rs`）が`FOLLOW_UP_CATEGORIES`（`src/domain/follow_up.rs`。下の表と同じ一覧と短い定義）から一覧と付け方を1行で出す。一覧を変えるときは表と`FOLLOW_UP_CATEGORIES`を一緒に変える。
- **test**: `src/domain/follow_up.rs`の`a_follow_up_category_is_kept_as_written_or_unlabeled`が欠け・空白・文字列でない値・一覧に無い値の扱いを、`tests/it/runtime_integrate.rs`の`integrate_registers_the_landed_follow_ups_as_draft_tasks_of_the_goal_once`が`follow_up_registered`（登録したものと飛ばしたもの）と`show`の`origin.material`の`category`、`stats`の`follow_up_categories`を、同じファイルのworkerのpromptのtestがreceiptの例と一覧の行を、`tests/it/plan_review.rs`の`drafts_of_the_runtime_get_planners_within_the_limit_and_a_persons_draft_none`がruntimeのplannerのpromptの種類の行（goal_gapのdraftには出ないこと）を確かめる。集計のtestは[Stats](stats.md#follow_upの種類ごとの集計)と[KPI](kpi.md#follow_upの種類ごとの系列)。
- **付け方**: 迷ったら、follow_upを片付けたときに何が変わるかで選ぶ（runtimeの挙動が直る → `defect`、testが安定する → `flaky_test`、文書が実装に追いつく → `docs_drift`）。`flaky_test`と`test_gap`はdescriptionにtestの名前（`<module>::<name>`）を書く。重複や採否はコードにしない（ADR-t947-3決定2）。

| コード | 定義 | task 951の例 |
|---|---|---|
| `defect` | runtime（またはscript）が決まった仕様どおりに動かない経路。再現の条件か、コードの場所と誤りを書く | 723: 開閉中のwindowでworkspaceの一覧が失敗する |
| `flaky_test` | 既存のtestが負荷や順序で時々落ちる・時間切れになる | 682: `runtime_adopt::a_supervisor_that_lost_its_lease_stops_touching_the_run` |
| `test_gap` | 経路にtestが無い、またはtestの作りが弱い（上限の無い待ち、実時計への依存）。今は落ちていない。runのreviewの`test_gap`と同じ種類 | 440: e2eの上限の無い待ち |
| `docs_drift` | 文書（design・ADRの索引・pluginのskill・AGENTS.md）が実装かacceptedのADRとずれている。runのreviewの`docs_drift`と同じ種類 | 535: supervise.mdの手順11が古い |
| `remaining_scope` | acceptedのADRかgoalが決めた事のうち、このtaskで実装しなかった残り | 441: ADR-0047決定39・40の残り |
| `improvement` | 仕様の誤りではないが、よくする案（観測・集計の追加、refactor、速さ、使い勝手） | 509: `land_phases.verify`を検証コマンドごとに分ける |
| `measurement` | 着地の後か期間の後に数えて確かめる依頼 | 934: 918の着地後の夜の答え待ち |
| `decision` | 人かplannerの判断が要る問いで、作業の中身が決まっていない | 269: 循環検出でcanceledのtaskを外すか |
| `ops` | repositoryの変更ではなく、人かinboxがhost・本番queue・外部サービスで行う作業 | 572: `dagq.toml`にKPIの目標を書く |
| `other` | どれにも当たらない。descriptionで説明する | 160: toolchainを上げるときの1行の注意 |

## follow_upsの所属の提案

[ADR-t1504-2](../../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定11と[ADR-t1504-1](../../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)決定5（task 1508）。workerはfollow_upごとに、元goalのacceptanceとの関係を提案として書けるが、確定はしない（所属の判断を記録するのはruntimeのplannerと人。[所属の判断](../follow-up-membership.md)）。

- **receiptの形**: `follow_ups`の各要素に任意の`membership_proposal`を足す: `{"classification": "required" | "out_of_scope" | "undecided", "acceptance_items": ["..."], "reason": "..."}`。`classification`は、そのfollow_upを実施しなくても元goalのacceptanceを満たせないと考えれば`required`、満たせると考えれば`out_of_scope`、分からなければ`undecided`。`acceptance_items`は関わる元goalのacceptanceの項目、`reason`はその根拠。問題と根拠は今までどおり`description`に書く。欄の無い要素も、未知の分類や形の違う値もreceiptの受理を変えない（`category`と同じ。ADR-t947-3決定3）。
- **記録**: `integrate`の登録（[integrate](integrate.md)の10）が、`follow_up_registered`のpayloadとdraftの出どころの`material`に`membership_proposal`を書いたまま載せる（無ければnull。`src/domain/follow_up.rs`の`follow_up_membership_proposal`）。登録しなかった項目は`follow_up_registered`の`follow_up`（項目そのもの）に残る。runtimeは提案を判断として扱わず、`follow_up_judgements`の行も書かない。
- **workerへの見せ方**: workerのprompt（[prompt](prompt.md)）のreceiptの例は`follow_ups`の要素に`membership_proposal`を持ち、`FOLLOW_UP_PROPOSAL`（`src/application/prompt.rs`）が、descriptionに問題と根拠を書くこと、`membership_proposal`の3つの欄の意味、提案に留めて自分で判断も移動もしないことを1段落で言う。resumeの解消依頼とreviseの依頼は手順5の`ACCEPTANCE_REMAP`の後に`FOLLOW_UP_PROPOSAL_AGAIN`（同じことの短い形）を足す。
- **plannerへの見せ方**: runtimeのplannerのpromptはfollow_upのdraftごとの節に`Membership proposal (the worker's; where you start, not a judgement): <JSON>`（無ければ`(none)`）の行を載せ、提案を起点に所属を判断して記録する手順を持つ（[Draft planners](draft-planners.md#所属の判断)）。
- **test**: `src/domain/receipt.rs`の`a_follow_up_with_or_without_a_membership_proposal_is_accepted`が欄つき・欄なし・形の違う値のreceiptの受理を、`src/domain/follow_up.rs`の`a_membership_proposal_is_kept_as_written_or_null`が記録する値を、`tests/it/runtime_integrate.rs`の`integrate_registers_the_landed_follow_ups_as_draft_tasks_of_the_goal_once`が`follow_up_registered`（登録したものと飛ばしたもの）と`show`の`origin.material`の`membership_proposal`を、`src/application/prompt.rs`の`every_worker_text_that_writes_a_receipt_proposes_follow_up_membership`がworker・resume・reviseのpromptを確かめる。
