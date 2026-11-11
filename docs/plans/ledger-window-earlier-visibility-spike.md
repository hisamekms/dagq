---
id: plan-ledger-window-earlier-visibility-spike
type: plan
title: スパイク：台帳の窓を範囲で読むとき、外した行の earlier と queue 行の全件の読みを範囲の外の確定した履歴に比例しない読みに置き換え、run_events の小さな読みを全件の台帳の読みと同じ可視性に保てるか
status: completed
created: 2026-11-11
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - plan-ledger-window-read-preconditions-spike
  - plan-ledger-window-read-contract-spike
  - plan-ledger-queue-rows-readers-spike
  - plan-kpi-read-memory-spike
  - design-measurement
---

# スパイク：台帳の窓の earlier・queue 行・小さな読みの可視性

問いは「台帳の窓を範囲 (S, E] で読むとき（goal 271 の受け入れ 1）、時刻から読む読み（`WindowQuery::read_from_ms`）が範囲の外の確定した履歴に比例して store から返す (i) 外した行の earlier と (ii) queue 行の全ページの読みを、全件の読みと同じ答えのまま比例しない読みに置き換えられるか。(iii) `run_events` を直接読む小さな読みが全件の台帳の読みと同じ可視性を保つ条件とその判定の読む量は何か。(iv) 受け入れ 1 の『読む行の数』をどう数えるか」。

判定は全体で **unresolved**（(i) は holds（4678 が unresolved とした session 行の records・improvement_proposals の goal を通る所属・proposal の task の読む量・run の session の開いたままの判定は下の「追試 (i) earlier（Spike 4755）」で holds）、(iv) は holds、(ii) は kind ごとに holds と unresolved に分かれ、(iii) は 4678 で試した部分と追試（Spike 4754）の (iii-a)〜(iii-c) で holds、(iii-d) U_E は unresolved。下の「判定」と「追試 (iii) 可視性（Spike 4754）」「追試 (i) earlier（Spike 4755）」）。

- (i) earlier: **holds**（builder が新しく書く 2 つの小さな表が要る。4678 が unresolved とした session 行の records、improvement_proposals の goal を通る所属、名指す proposal の task の読む量、run の session が開いたままかの判定は「追試 (i) earlier（Spike 4755）」で holds）。task・session・run 行の earlier を「範囲が名指すものの seek（A）」と「S に開いているもの（B）」だけにすると、kpi の 4 問と `stats --since --full` の答えが全件と一致し、返る earlier の record は 30 単位でも 3000 単位でも 20 件（今の読みの in-memory の形は 456 件と 42,036 件）。ただし確定した単位は範囲と何も名指し合わず、開いたまま残るものも持たない（下の「試作」の限り）。範囲と結ばれる古い履歴は手で作った約 10 の場合だけで確かめた。同じ proposal の task の片方が範囲の前に終わる場合は、proposal が名指す task の記録まで読まないと proposal を動いていると数え違える（`a_proposal_the_range_names_is_read_with_the_ends_of_its_tasks_before_the_range`）ので、名指す proposal から task へ 2 段で seek する。ただし試作はその task の records を全部返すので、範囲の前に終わった task が範囲の前の record を多く持つと読む量はその履歴に比例する（30/3000 の比べはこの経路を増やしていない）。その task の終わりだけを読む形は追試で holds。goal の task が proposal に入る所属（`goal_submitted` が名指す proposal の goal の task）は 4678 の試作に無く、追試で holds。B を確定した履歴を走査せずに見つけるには、run 行が終わったときに開いたまま残したもの（slot・ask・session・landing の待ち）を builder が別の表に書く要がある。最初の Execution（C）は返る行は 1 件のままだが DB の走査は外した run 行の全部に比例するので、actor × provider の最初の時刻を builder が別の表に書く。
- (ii) queue 行: **部分ごとに分かれる**。控え（`claim_deferred`）・watcher の待ち・alert・finding・landing の slot・mark（`mark_added`）は「S に終わっていないか S 以降の record を持つ行、kind ごとの S 前の最後の行、`queue_before` の行」で全件と一致し、3000 単位でも返る行の数が変わらない（holds）。`slots_full`・`claim_hold`・`landing_hold`、supervisor の生存と heartbeat、forecast の snapshot と baseline（試作で行の数が単位で増えない）、mark の撤回、main の記録（D の見込み）、post_landing、goal の属性、update・backend の失敗・run の無い ask と repair、`stall_config`、`candidates_sampled`（kpi の候補の数の標本）、`live_alert_stream` の records は **unresolved**（下の「未解決」）。
- (iii) 可視性: **U_E を除いて holds**（4678 で試した部分と追試の (iii-a)〜(iii-c)。(iii-d) U_E は unresolved）。run と task の event を持ち主の行の `inputs_through` で判定すると、係の遅れ・resume した run の行の後の event・task が終わり run 行の無い run・legacy の行だけが名指す run を含む store で全件の `LedgerWindow::events` と一致し、引いた持ち主の数は 30 単位でも 3000 単位でも 17。4678 が上限で残した走行中の run、W・R の後で書かれていない session・queue の event と W・R の食い違い、R による検査の省略は追試で holds、U_E は追試でも unresolved（下の「追試 (iii) 可視性（Spike 4754）」の「判定」）。event の id の cursor の時刻は見える event だけで引く要がある（見えない event の id では全体の時刻が後になる）。
- (iv) 数え方: **holds**（数え方として）。経路全体で store から返る run・task・session 行、earlier の record、最初の Execution、queue 行、範囲の event の和は、上の形で 30 単位でも 3000 単位でも 245（今の読みの in-memory の形は 1,045 と 63,415）。

## 環境

- 対象の commit: `48026f59cc912a5b0a5fd9cf53198acb524ddeb3`（この run の base。task 4660 の `578b379f6` を含む）。
- host: Apple M1、macOS 27.2（Darwin 27.2.0、arm64）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`cargo-nextest`。
- provider と経路: 本番 queue の Spike の run（task 4678）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。試作は in-memory（`domain::ledger::build` が書く行と、`domain::ledger::window` の `WindowRows` をそのまま使う `#[cfg(test)]` の test）で、SQLite の adapter は使っていない。本番の queue と DB は読んでいない（合成の event だけ）。
- 使った時間と回数: 上限 4 時間に対して、1 回目の turn が約 1 時間 30 分（00:49〜02:20 JST）、review の手直しの turn が約 45 分（02:22〜03:10 JST、stress を含む）、合わせて約 2 時間 15 分。その後の review の手直し 2 回（candidates_sampled と goal を通る所属の切り分け、人の差し戻し ask 1656 による proposal の task の読む量と run の session の判定の切り分け）は文書だけで、合わせて約 35 分（stress を含む）。部分ごとの時間（コードの読みと試作の土台の約 20 分は (i) に入れた）と試作の作り直し（形を変えて全件と比べ直すこと、上限は部分ごとに 3 回）は次のとおり。
  - (i): 約 55 分（1 回目の turn 約 40 分、手直し約 15 分）、3 回。1 回目は task 行の records を範囲が名指す task・plan review・proposal・planner の seek だけにして、forecast の `marks_between` が 4 から 3 に変わった（snapshot が名指す task の plan の変化を落とした）。2 回目に snapshot が名指す task の `TASK_MARK_KINDS` を足して一致した。3 回目（review の指摘）に同じ proposal の task の片方が範囲の前に終わる場合を足し、proposal の task を 2 段目で読む形にして一致した。session 行の記録の場合は上限の前だが時間の都合で試作していない（「未解決」）。
  - (ii): 約 30 分、3 回（上限に達した）。1 回目は kind が少なかった。2 回目に mark・控え・alert・finding を足すと `findings_open` が合わなかった。3 回目に finding の状態の変化を実際の形（`finding` の値を持つ）にして一致した（S に開いている finding を読む条件も足したが、finding の行は settled まで `ended_at` が無いので「S に終わっていない行」の条件と同じで、答えを変えたのは fixture の直し）。
  - (iii): 約 25 分、3 回（上限に達した）。1 回目は session と queue の event を「どれかの行の records にあるか」で判定した（全件の読みと同じことをしていた）。2 回目に W・R とその後に書かれた行で判定する形にした。3 回目に、mark の後の record を持つ session・queue の行を作る部分書き込みを足した。subagent の review が、走行中の run と書かれていない session・queue の event の反例が store に無いことを指摘したが、上限の 3 回に達したので作り直さず unresolved にした。U_E も同じ理由で試作していない。
  - (iv): 約 10 分、1 回。
  - 文書と review: 約 35 分（部分に分けない）。
- 差分の大きさ: 試作 約 1,250 行、文書 約 210 行、testkit の 2 関数の公開と mod の宣言。

## 読んだコードと打ったコマンド

読んだコード:

- `src/domain/ledger/window.rs`: 「A read from a time」、`RowsFrom::leaves_out`、`EARLIER_RUN_KINDS`・`RUN_END_STATUSES`・`ends_run_occupancy`、`Earlier`・`EarlierRecords`、`WindowRows::take_earlier`（run_id の無い record を events に、claim・ask・session・終わりを行を選ぶのにだけ使い、ほかの EARLIER の kind は run_id を外して events に入れる）、`rows_wanted`、`runs_named`（`detail.runs` だけ）、`runs_on_their_way`・`run_on_its_way`、`select_from_time_with`、`LedgerWindow::judgments`・`main_log`・`main_history`・`main_covers`・`post_landing_book`。
- `src/infrastructure/runtime_store/ledger.rs`: `earlier_in`（外した task・session 行の records の全部、run 行の EARLIER の kind と状態の record、`detail.executions` の actor × provider の `MIN(at)`）、`rows_left_out_sql`・`record_not_before_sql`。
- `src/application/ports/observation.rs`: `window_rows`（queue 行は全ページ、`take_earlier` の後に `rows_wanted` の loop、`with_runs_on_their_way`）、`ledger_runs_past_their_rows`（窓は読まない）。
- `src/domain/ledger.rs::build`（event はちょうど 1 つの行の records、`owner`）、`src/domain/ledger/owner.rs`、`src/domain/ledger/session.rs`（session 行の key は `{kind}/{planner か plan review の id}/{what}/{round}`、`subject_kind`・`subject_id` を持つ）、`src/domain/ledger/session/written.rs`（W は session 行が session の event を全部記録した位置、R は queue 行と、run・task・session・node 行のそれぞれの位置で、W と同じ transaction で書き、W より後でない）。
- `src/domain/ledger/queue.rs` の module doc（kind ごとの行の増え方）と `finding`（行の `outcome` は finding の状態、`ended_at` は settled のとき）。
- `src/domain/kpi/window.rs`: `findings`（`finding_recorded` で open、`finding_status_changed` で状態）、`improvement_proposals`、`slot_usage`。`src/domain/stats/landing_utilization.rs`（`KINDS`・`LEAVING_STATUSES`・`reads`）。`src/domain/marks.rs::cursor_millis`。
- `migrations/`: `ledger_rows` の索引は主キーだけ。`run_events` は `events_by_run(run_id, id)`・`events_by_task(task_id, id)`・`events_by_goal`・`events_by_kind(kind, id)`。

打ったコマンド:

```sh
git log -1 --format=%H
rustc --version; cargo --version; uname -sm
grep -rhn "CREATE INDEX" migrations | grep -i "ledger_rows\|run_events"
cargo nextest run --lib earlier_visibility_spike --no-capture
cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike/)' --no-capture
cargo fmt --all
cargo clippy --locked --all-targets -- -D warnings
sh scripts/cargo-brief.sh cargo nextest run --locked --lib --stress-duration 60s -E 'test(/earlier_visibility_spike::/)'
sh scripts/check-local-gates.sh --base 48026f59cc912a5b0a5fd9cf53198acb524ddeb3
```

## 試作

`src/application/ports/observation/earlier_visibility_spike.rs`（`#[cfg(test)]` の未配線の module。`observation.rs` が `mod earlier_visibility_spike;` で持つ）。runtime の読みは変えていない。

- 履歴（`history(settled)`）は `testkit::log_of_a_time`（slot を持ったままの run、答えの無い ask、閉じない session、範囲の前の follow-up と範囲で着地する draft、範囲の前の planner の session と plan review の予想、legacy の run、forecast の snapshot）に、範囲の前に開いて範囲で閉じる ask と session、範囲の前の proposal の task が範囲で着地するもの、範囲の前からの landing の待ち、範囲の前の属性の記録だけを持つ task を足し、範囲の外（11〜150 日目）に `settled` 単位の確定した履歴を置く。1 単位は task の着地と run、答えた ask、follow-up の draft の cancel、proposal、planner の session、queue の待ち・mark・控え・alert・解決した finding。範囲は 198 日目からの 2 日（読みの時刻は 200 日目）。確定した単位の id（task・proposal・planner・finding）は範囲の何も名指さず名指されず、単位は全部閉じている。だから 30 と 3000 の比べは「範囲と結ばれない確定した履歴が増えても読む量が増えない」ことを確かめ、範囲と結ばれる古い履歴（依存、共有の proposal や goal、古い run の follow-up、古い snapshot）と開いたまま残る run の数（B）の増え方は、手で作った約 10 の場合だけで確かめた。
- 狭めた読み（`narrowed`）は `select_from_time_with` と同じ順で行を取り、earlier の record だけを部分（`Part`）ごとの読みに置き換えて `WindowRows::take_earlier` に渡す。queue 行の狭めた読みは `queue_narrowed`。全件の答えは `select`（全行）に `read_whole_ledger` の問いで出す。返した数は `Reads`（行・earlier・最初の Execution・queue 行）で数える。「今の読み」の数は `narrowed` に狭める部分を渡さない形で、run 行の record の選び（`parts_of_run_record` の和集合）は `select_from_time_with` の条件（`EARLIER_RUN_KINDS`・`ends_run_occupancy`・`landing_utilization::reads`）と同じ。SQLite の `earlier_in` の SQL そのものではない。
- 可視性の試作（`store_cases`・`visible`・`small_read`）は、最後の 40 件を書いていない store（R・W はその前）、resume した run 3 の行の後の event、task が終わり run 行が書かれていない run 12、legacy の task 行だけが名指す run 16、mark の後まで書いた session・queue の行を作り（run 7 の行も落としたが、task 7 は cancel されて行が終わっているので走行中の run ではない。この store に走行中の run は無い）、範囲の event のうち全件の読みの `events()` に入るものと小さな読みの答えを比べる。

| test | 場合 | 比べたもの |
| --- | --- | --- |
| `the_read_from_a_time_today_returns_earlier_records_in_proportion_to_the_settled_history` | 今の読み（全部の部分を今の形） | 答えは全件と一致。earlier は 456 → 42,036、queue 行は 426 → 21,216（30 → 3000 単位）、行は 30 のまま |
| `each_part_narrowed_alone_keeps_the_answers_of_every_row_or_names_what_it_misses` | 部分を 1 つずつ狭める（30 単位） | kpi の 4 問（日 2・週 2・`--by nature,provider`・`--at`）の答えの JSON 全体が全件と一致 |
| `a_proposal_the_range_names_is_read_with_the_ends_of_its_tasks_before_the_range` | proposal 8 の task 66 が範囲の前に完了し task 65 が範囲で完了する（planner が提案した finding で結ぶ） | 2 段目（proposal の task）を読まないと `improvement_proposals` が 1、全件と 2 段目つきは 0 で、答えの JSON 全体が一致 |
| `every_part_narrowed_returns_as_many_records_whatever_the_settled_history` | 全部の部分を狭める（30 と 3000 単位） | 3000 単位で答えが一致。行 30・earlier 20・Execution 1 は 30 単位と同じ |
| `stats_since_a_time_answers_as_the_whole_read_with_the_narrowed_earlier` | `stats --since`（2 日前）`--full` を、全部の部分と全部の queue の kind を狭めて | 答えの JSON 全体が全件（`since` つきの全行）と一致。30 と 3000 単位で読んだ数は同じ |
| `each_queue_kind_read_by_its_range_keeps_the_answers_of_every_row_or_names_what_it_misses` | queue 行の kind を 1 つずつ狭める（30 と 3000 単位の行の数を数え、30 単位で kpi を比べる） | どの kind も一致。行の数: 控え・alert・mark・待ちは 30/3000 → 1/1、landing_slot 45/3015 → 8/8、finding 32/3002 → 3/3 |
| `the_reads_of_the_whole_path_add_up_to_as_many_whatever_the_settled_history` | earlier と queue 行の全部を狭め、範囲の event を足す | 3000 単位で kpi の日と週の問いの答えが一致。合計は 245 と 245（今の読みは 1,045 と 63,415） |
| `the_small_read_of_the_saved_owners_and_the_runs_on_their_way_sees_what_the_whole_read_sees` | 上の store（30 と 3000 単位） | 範囲の event 133 件のうち見える 59 件と小さな読みの答えが一致（余分も欠けも無い）。mark の後の行の record 1 件を含む。持ち主の行を引いた数は 17 で同じ |
| `a_cursors_time_of_every_event_runs_past_that_of_the_visible_ones` | 見えない event（run 行の無い run の）の id を cursor にする | 見えない event の id では全 event の `cursor_millis` が見える event のものより後になる（1917193705000 > 1917193700000）。`--since` の範囲が欠けることそのものは流していない |

比べなかった場合: SQLite の adapter の SQL（下の「読みの約束」の SQL は試作の in-memory の条件と同じ式だが、流していない）。時計の戻り（4670 の (vi) が扱う）。`--compare`・`--goal`・report の経路。

## 判定

### (i) earlier: holds（4678 で unresolved の 4 行は追試で holds）

部分ごとに、置き換える読みの形・全件と同じになる根拠・読む量を書く。下の表で unresolved の 4 行（読む量・goal を通る所属・session 行・run の session）は「追試 (i) earlier（Spike 4755）」の「判定」で holds になり、読みの形はそちらが持つ。根拠の test は `each_part_narrowed_alone_keeps_the_answers_of_every_row_or_names_what_it_misses`（部分ごと）、`every_part_narrowed_returns_as_many_records_whatever_the_settled_history`（全部、3000 単位）、`stats_since_a_time_answers_as_the_whole_read_with_the_narrowed_earlier`。

| 部分 | 判定 | 読みの形 | 読む量 |
| --- | --- | --- | --- |
| task 行の records（属性・最初の ready・plan_quality・draft_flow・follow_up_categories・cancel_reasons） | holds | 範囲が名指す task の行は `rows_wanted` が key で丸ごと読む（今と同じ）。外した task 行の records は、範囲の record が名指す task・plan review・proposal の record と、範囲の task を名指す forecast の snapshot の task の `TASK_MARK_KINDS`（4670 の (vii) の 2 段目）だけ。`run_events` の `events_by_task(task_id, id)` と、payload の `plan_review_id`・`proposal_id` の式の索引で seek する | 範囲が名指す数に比例（A） |
| improvement_proposals の task が直に名指す所属（`task_submitted`・`task_reopened` の `proposal_id`） | 答えは holds、読む量は追試で holds（task の終わりだけを読む） | 範囲の record が名指す proposal の `task_submitted`・`task_reopened` を `proposal_id` の式の索引で seek し、その task を名指すものに足して、その task の records を読む（試作の `Named::note_members`）。task の片方が範囲の前に終わった proposal を、その終わりを落として動いていると数えないため（`a_proposal_the_range_names_is_read_with_the_ends_of_its_tasks_before_the_range`: 2 段目が無いと `improvement_proposals` が 1、全件と 2 段目つきは 0）。ただし試作の `narrowed_with` は、足した task（fixture の task 66 のような範囲の前に終わった task）の records のうち `Named::names` に当たるものを全部 earlier に返す。その task に範囲の前の `task_priority_changed` を 30 件・3000 件足すと、名指す proposal と task の数も全件の答えも同じまま、返る earlier はその数に比例して増える。30/3000 の比べは範囲と結ばれない task だけを増やすので、この経路を検べていない。読む量を名指す proposal の task の数に抑える見込みの形は、その task ごとに E 以前の最後の `task_status_changed`（と後の `task_reopened`）だけを `events_by_task` を id の降順に歩いて読むこと。試していない | 試作の形はその task の履歴に比例（見込みの形は名指す proposal の task の数） |
| improvement_proposals の goal を通る所属（goal の最後の `goal_submitted` が名指す proposal に、その goal の今の task が入る） | 追試で holds | 試作の履歴に goal の event が無く、`Named::note_members` は task が直に名指す所属だけを集める。見込みの形は、範囲が名指す proposal を名指す `goal_submitted` を `proposal_id` の式の索引で seek し、その goal の task（attributes の `task_goals`、goal の今の task）の records を `events_by_task` で読む 3 段の seek | 名指す proposal の goal の task の数（A の見込み） |
| draft_backlog | holds | 終わっていない task 行は外れない（`leaves_out` は終わった行だけ）ので今の行の読みで足りる | 開いている数（B） |
| session 行の records（planner_routes・plan review の session_closed の model・effort・sessions・jobs） | 追試で holds | 見込みの形は、範囲の record が名指す planner・plan review の session 行を主キー `(kind, key)` の key の接頭辞（`runtime_planner/<id>/`）の範囲で読むこと。試作の条件（`Named::names`）は task・proposal を名指す session 行も残すのでこの seek より広く、試作が答えの一致を示したのはその広い条件。また `Named::note` は actor の `planner:<id>` を集めないが、planner_routes は planner_question の actor から planner を特定するので、範囲の ask と範囲の前に閉じたその planner の route の場合が試作に無い。どちらも試していない | A の見込み |
| run 行の claim と slot の終わり（`run_claimed`・`run_integrated`・`run_satisfied_closed`・`RUN_END_STATUSES`） | holds | 行が終わったとき slot を持ったまま（claim があり終わりが無い）の run だけを読む。下の「開いたまま残した行の表」から引き、その run の行を key で丸ごと読む（今の `take_earlier` が wanted にするのと同じ） | B |
| ask（`ask_opened`・`ask_answered`） | holds | 同じ表で、答えの無い ask を残した run。試作の `open_at_end` は ask を `take_earlier` と同じ `AskRef::key` で対にする | B |
| run の session（`session_opened`・`session_closed`） | 追試で holds | 同じ表で閉じない session を残した run を引く見込みだが、試作の `open_at_end` は open と close の件数を比べるだけで、`WindowRows::take_earlier` と `stats::sessions::spans` のように close の `opened_event_id` で対にしない。対にならない close（`opened_event_id` の無い古い形の close など）や重複した close を全件の読みは終わりと数えないので、古い run に open とその形の close があると、全件では session が開いたままなのに試作は閉じたと判じてその run を落としうる。fixture は `numbered` が close に `opened_event_id` を補うのでこの差が出ない | B の見込み |
| `follow_up_registered` | holds | payload の `task_id` が範囲の task の record。`run_events` に `CREATE INDEX ... ON run_events(json_extract(payload, '$.task_id')) WHERE kind = 'follow_up_registered'` を足して seek し、(iii) の可視性で絞る | A |
| ほかの EARLIER の kind（`task_priority_changed`・依存・`task_submitted`・`task_reopened`・`task_status_changed`・`goal_submitted`・`proposal_withdrawn`・`plan_decided`・`proposal_settled`） | holds | 範囲の task か proposal の record（`events_by_task` と `proposal_id` の索引）。forecast の終わりは 4670 の (vii) の読み | A |
| landing（`landing_utilization::KINDS` と `LEAVING_STATUSES`） | holds | S 以前の時刻の `integration_started` のうち id の最大（試作の `last_start`。時計が戻ると時刻の最大と違うので、索引は 4670 の (vi) の id の順の規則に従う）の run と、開いたまま残した表の landing の待ちの run | A + B |
| 最初の Execution（`Earlier::executions`） | holds（表を足す形で） | 返るのは actor × provider ごとに 1 行で履歴に依らない（試作で 1 件のまま）が、今の `GROUP BY` は外した run 行の `detail.executions` を全部走査する（C）。builder が actor × provider ごとの最初の時刻の表を書き、それを読む | 組の数 |

B を確定した履歴を走査せずに見つける形: 終わっていない行（`ended_at IS NULL`）では足りない。外した run 行は終わった行で、試作の run 7（claim の後に task が cancel）・run 8（着地の後に ask）・run 10（着地の後の session）のように、行が終わっても slot・ask・session が開いたままの run がある。行の列はこれを表さないので、builder が run 行を書くとき、開いたまま残したもの（slot・ask・session・landing の待ち）を別の表（例: `ledger_open(kind, key, what)`、主キーで引く）に書き、閉じる event でその行を消す（試作の `open_at_end` が slot・ask・landing の待ちについて同じ判定。run の session は全件の読みと同じく close の `opened_event_id` で対にする要があり、試作の件数の比べはそれと違った。対にする形は追試で holds）。

ADR-t1662-2 決定 1・7・8 との関係: 足す 2 つの表（開いたまま残した行と最初の Execution）は、event だけから作り直せる台帳の一部（ビュー）で、event が SSOT のまま。`ledger_omitted_points`（0104）と同じく行の形と `ledger_v` を変えない。ただし今ある台帳にはこの表が無いので、表を作る migration で今の行から埋めるか、次の pass が埋めるまで全件の読みに戻す。

### (ii) queue 行: 部分ごとに分かれる

根拠の test は `each_queue_kind_read_by_its_range_keeps_the_answers_of_every_row_or_names_what_it_misses` と `the_reads_of_the_whole_path_add_up_to_as_many_whatever_the_settled_history`（kpi）、`stats_since_a_time_answers_as_the_whole_read_with_the_narrowed_earlier`（stats）。読みの形は kind ごとに「S に終わっていない（`ended_at IS NULL OR ended_at >= S`）か S 以降の record を持つ行（`inputs_through.events > queue_before`）、kind ごとの S 前の最後の行、`queue_before` を持つ行」（試作の `queue_narrowed`）。`ledger_rows(kind, subject_kind, ended_at)` の索引と、`json_extract(inputs_through, '$.events')` の式の索引が要る。

| kind と読む fold | 判定 | 根拠と残る量 |
| --- | --- | --- |
| 控え（`claim_deferred`）と watcher の待ち（`inbox_watcher_absent`・`inbox_watcher_returned`） | holds | 30/3000 から 1/1・2/2 に減り答えは一致 |
| `slots_full`・`claim_hold`・`landing_hold` | unresolved | 試作の履歴に無い（同じ区間の行の形なので控えと同じ形の見込み） |
| `live_alert`（1 件ごと） | holds | 30/3000 → 1/1 |
| `finding`（findings_open は B、finding_resolve_time は A） | holds | finding の行は settled になるまで `ended_at` が無いので、S に開いている finding は終わっていない行の読みで足りる。範囲で解決したものは S 以降の record を持つ。32/3002 → 3/3 |
| `landing_slot` | holds | 45/3015 → 8/8 |
| `mark_added` | holds | 30/3000 → 1/1 で kpi の 4 問は一致。mark の読みの約束は 4135・4670 が持つ |
| supervisor の生存と heartbeat（`supervisor_started`・`supervisor_stopped`・`supervisor_alive`） | unresolved | 狭めても答えは一致した（191 → 40）が、行の数は確定した単位で増えない（日ごと）ので 30/3000 の比べになっていない。S 前の最後の heartbeat は supervisor ごとに 1 行要る（試作は kind ごとの最後） |
| mark の撤回、forecast の snapshot、baseline（`forecast_recorded`・`attributes_baseline`・`finding_baseline`） | unresolved | 撤回は履歴に無い。snapshot と baseline は 1 行ずつで単位で増えない |
| main の記録（`main_log`・`main_history`・`main_covers`、`MAIN_LOG_KINDS`） | unresolved | 試作の履歴に無い。`fold_main_log(self.events())` が記録の全部を畳む（main が持つ path は全期間の記録から）ので、範囲の形では D の見込み |
| `post_landing_book`（revert と CI の失敗） | unresolved | 試作の履歴に無い |
| goal の属性 | unresolved | 試作の履歴に goal の event が無い |
| update・backend の失敗・run の無い ask と repair | unresolved | 試作の履歴に無い |
| `stall_config`（E 以前の最新） | unresolved | 試作の履歴に無い。`events_by_kind` を E 以前で降順に 1 件の形の見込み |
| `live_alert_stream`（judgments） | unresolved | 試作の履歴に無い。行は baseline ごとに 1 つだが、開いている stream の records が 60 秒ごとに伸びる |
| `candidates_sampled`（kpi の候補の数、`Context::candidates`） | unresolved | 試作の履歴に無い。stream とは別の queue の event（1 件ごとの行）で、各標本を次の標本か、それを書いた supervisor の生存の終わりまで重みにする。見込みの形は、範囲の標本と、S 以前の最後の標本（S をまたいで重みを持つ。supervisor ごとに 1 件）と、その標本の supervisor の生存の行。時計が戻ると「次の標本」が id の順と時刻の順で違うので、`candidates` の並べ方（`self.events` の id の順）に合わせる |

### (iii) 可視性: U_E を除いて holds（4678 で試した部分と、追試の (iii-a)〜(iii-c)。U_E は追試でも unresolved）

根拠の test は `the_small_read_of_the_saved_owners_and_the_runs_on_their_way_sees_what_the_whole_read_sees` と `a_cursors_time_of_every_event_runs_past_that_of_the_visible_ones`。

- 小さな読みが返す event を次に限ると全件の `events()` と一致した（試作の `small_read`）。試作の store で反例があったのは run と task の event の場合だけで、下の印（4678 で unresolved）の条件は追試（Spike 4754）が反例のある store で判定した。
  - run の event: その run の行が保存され `inputs_through` が id 以上か、保存された終わっていない task 行の `detail.runs` が名指す run 行の無い run（走行中、id が R の後でも見える。4678 では試作の store に無く unresolved、追試の (iii-a) で holds）。resume した run の行の後の event、task が終わり run 行の無い run、legacy の行だけが名指す run（`runs_named` は `detail.runs` だけを読む）は落ちる（holds）。task 行がまだ無い run は試作に無い。
  - task の event: task 行が保存され `inputs_through` が id 以上。
  - session と queue の event: event から行の key が決まらない（session 行の key は book が決める）。id が W（session）か R（queue）以下なら行にある（marks の定義）。それより後は、`inputs_through` が mark より後の session・queue 行の records にあるときだけ（4678 では落ちる event が無く W と R も同じ位置で unresolved、追試の (iii-b) で W と queue の R を別々に読む形で holds）。
- R・W は `ledger_read_snapshot` の中で読む要がある。今の `LedgerWindowRead` は `ledger_marks` を持たないので足す。id ≤ R の task と queue の event、id ≤ W の session の event は持ち主の行の検査を省ける見込み（4678 では試していない。追試の (iii-c) で task の event について holds）。run の event は id ≤ R でも省けない（builder は終わるまで run 行を書かず、書き込みの上限で切れた pass の run もある）。
- 判定の読む量は、候補の event の持ち主の run と task の数（試作で 17、30 と 3000 単位で同じ）と、mark の後に書かれた session・queue 行の数（係の遅れに比例）で、確定した履歴には比例しない。後者は `json_extract(inputs_through, '$.events')` の索引で引く。
- 行に記録されていない run に属する入力は補わない（ask 1607）。上の形は、全件の読みが持つ行と走行中の run だけを見る。task 4660 の共通の読み取り関数が窓の端を補う (C, S] の写さない kind の queue の event はこの判定の外で、その関数のまま扱う。時間で来る変化（time_pending）は event でないので、この判定に入らない。
- 台帳の行の上限 U_E（draft 4122）: 4678 では試していない。追試の (iii-d) でも unresolved（E の後に始まった走行中の run の入力を保てない反例が review で示され、作り直しの上限に達した。追試の節の「判定」と「未解決」）。
- event の id の cursor の時刻（draft 4677 の (f)）は見える event だけで引く。全体で引くと後の時刻にずれ（試作で 1917193705000 と 1917193700000）、`--since` の範囲が欠けうる。

### (iv) 数え方: holds

受け入れ 1 の「読む行の数」は、経路全体で store から返る run・task・session 行、earlier の record、最初の Execution の行、queue 行、小さな読みの event の和として数える（試作の `Reads` と `the_reads_of_the_whole_path_add_up_to_as_many_whatever_the_settled_history`）。範囲の外の確定した履歴を 30 単位と 3000 単位にして和が等しいことを unit test で確かめる。DB が走査する行は返る数と別で、最初の Execution の `GROUP BY` のように返る数が一定でも走査が比例する読みは、表を足すか、SQLite の `EXPLAIN QUERY PLAN` が索引を使うことを adapter の test で確かめる。 この和が 30 と 3000 で等しいのは試作の履歴が増やした経路（範囲と結ばれない確定した単位）の限りで、名指す proposal の task の履歴を増やす経路は追試の `a_member_task_s_ends_alone_answer_as_its_whole_history_whatever_its_own_history` が比べた。

残る比例する読みと受け入れ 1 の但し書きへの当てはめ:

- `--compare` の群の読み（4135 の決定 C）: 範囲が決まらない問い。
- forecast の 2 段目（4670 の (vii)）: snapshot と範囲の間の履歴と範囲の後の mark に比例する。範囲の外の確定した履歴のうち snapshot より前には比例しないので、受け入れ 1 の比較では snapshot を範囲の近くに置いて数え、snapshot の古い問いは範囲が決まらない問いに入れる。
- (ii) の unresolved の kind（特に main の記録）: 判定が出るまで、それを読む問い（`--by area`・areas・`conflict_hotspots`・`main_covers`）は範囲が決まらない問いのまま。

## 読みの約束

時刻から読む読み（範囲 (S, E]、`queue_before` は S 以前の queue の最後の record）は、全件の答えと同じまま、次だけを store から読む。

1. 終わっていない行と、S の後の record を持つ行（今の `rows_from`）。
2. 1 の行が名指す task と run の行（今の `rows_wanted`、key で）。
3. 開いたまま残した行の表にある run の行（key で）。builder が run 行を書くときに書き、閉じる event で消す。約束に入れるのは slot・ask・landing の待ちと、close の `opened_event_id` で対にならない run の session（追試で更新）。
4. 外した行の record のうち、範囲が名指す task・plan review・proposal と、範囲が名指す proposal の task（`task_submitted`・`task_reopened` の `proposal_id` から 2 段目）と名指す proposal の goal の task（`goal_submitted` から 3 段目）はその task の終わりだけ、範囲の task を名指す snapshot の task の `TASK_MARK_KINDS`（`run_events` の `events_by_task` と payload の式の索引）、範囲が名指す planner（actor を含む）と plan review の session 行（key の接頭辞で）。`run_events` から読むときは 6 の可視性で絞る（追試で更新。形は「追試 (i) earlier（Spike 4755）」の「読みの約束の更新」）。
5. S 以前の時刻を持つ `integration_started` のうち id が最大のものの run（試作の `last_start` と同じ順）。読みの形は `events_by_kind(kind, id)` を id の降順に歩き、時刻が S 以前の最初の 1 件で止める。歩く数は S の後の時刻を持つ `integration_started`（範囲とその後の着地、時計が進んだもの）に比例し、範囲の前の確定した履歴には比例しない。最初の Execution は builder が書く actor × provider の表から。
6. queue 行は kind ごとに、S に終わっていないか S 以降の record を持つ行、kind ごと（supervisor の生存は supervisor ごと）の S 前の最後の行、`queue_before` の行。(ii) で unresolved の kind は全ページのまま。
7. `run_events` の小さな読みは、(iii) の条件の event だけを返す。条件は追試（Spike 4754）の「読みの約束の更新」の 7 が置き換える（走行中の run、W と R の後の session と queue の event、R による検査の省略を含む。U_E は追試でも unresolved で約束に入れない）。

## 未解決

(iv) に未解決の部分は無い。(i) の下の 4 項目（session 行の records、improvement_proposals の goal を通る所属、名指す proposal の task の読む量、run の session が開いたままかの判定）は「追試 (i) earlier（Spike 4755）」で holds になった（項目は 4678 の時点の記録として残す）。(iii) の 4678 で上限に達して止めた部分は、追試（Spike 4754）で走行中の run・W と R の後の event と食い違い・R による省略に判定が出て、U_E だけが追試でも未解決（追試の節の「未解決」）。(ii) の次の kind は試作の履歴に無い、または行の数が単位で増えないので 30/3000 の比べになっていない。(ii) も作り直しは 3 回に達した。

- (i) session 行の records（追試で holds）: 試したことは、`Named::names`（task・proposal・plan review・planner の id を名指す record を持つ session 行を丸ごと残す）で答えが一致したことだけ。提案する key の接頭辞の seek（planner と plan review の id だけ）そのものと、actor の `planner:<id>` から planner を特定する planner_routes の場合は試していない。止まった理由: review の指摘で分かった抜けで、作り直しの回数は残っていた（2 回）が、試作を接頭辞の seek に替えて planner_question の ask の場合を足すと (i) の比べを全部流し直すので、revise の時間の中で他の指摘を先にした。次に要る試作: `Named` に actor の `planner:<id>` を足し、session 行を key の接頭辞だけで残す形にして、範囲の planner_question の ask と範囲の前に閉じたその planner の session（route を持つ）を置いて全件と比べる。約 120 行。
- (ii) `slots_full`・`claim_hold`・`landing_hold`、supervisor の生存と heartbeat（supervisor ごとの最後の heartbeat）、mark の撤回、forecast の snapshot と baseline: 確定した単位にそれぞれの行を足し（supervisor は単位ごとに別の supervisor の生存）、30/3000 で比べる。約 150 行。

- (ii) main の記録（`main_log`・`main_history`・`main_covers`）: 試したことは無い。`fold_main_log` が記録の全部を畳むので、範囲の形では main が S に持つ path を作れない見込み（D）。次に要る試作: `main_commits_recorded`・`main_paths_recorded` を確定した履歴に足し、S での main の状態を builder が書く要約（S 以前の最新の状態の行）から読む形と全件を比べる。約 200 行の見込み。
- (ii) `post_landing_book`: 次に要る試作: revert と CI の失敗を範囲の前の着地に付け、範囲の run だけの seek と全件を比べる。約 100 行。
- (ii) goal の属性: goal の作成・変更を範囲の前に置き、範囲の task の goal の E 以前の最新の seek と比べる。約 100 行。
- (ii) update・backend の失敗・run の無い ask と repair、`stall_config`: 1 件ごとの行で、S 以降の行と E 以前の最新の seek の形。開いた run の無い ask は B。約 150 行。
- (i) 名指す proposal の task の読む量（追試で holds）: 試したことは、2 段目の task の records を `Named::names` で全部返す形で答えが一致したことだけ。止まった理由: 作り直しの上限の 3 回に達した（人の答え ask 1656 で unresolved にしてよいとされた）。次に要る試作: 範囲の前に終わった proposal の task（task 66）に範囲の前の `task_priority_changed` を 30 件と 3000 件足し、その task は E 以前の最後の `task_status_changed` と後の `task_reopened` だけを読む形にして、`improvement_proposals` を含む答えが全件と同じで返る earlier が 30 と 3000 で同じことを比べる。約 60 行。
- (i) run の session が開いたままかの判定（追試で holds）: 試したことは無い（試作の `open_at_end` は件数の比べ）。止まった理由: 上限の 3 回（ask 1656）。次に要る試作: `open_at_end` の session を close の `opened_event_id` で対にし（`take_earlier` と同じ規則）、着地した古い run に open と `opened_event_id` の無い close、重複した close を置いて（`numbered` で補わない）、全件の読みと比べる。約 60 行。
- (i) improvement_proposals の goal を通る所属（追試で holds）: 試していない。止まった理由: 作り直しの上限の 3 回に達した（3 回目が task が直に名指す所属の 2 段目）。次に要る試作: goal を範囲の前に作り、その goal を proposal に `goal_submitted` で入れ、goal の task の片方を範囲の前に、もう片方を範囲で完了させ、finding で proposal を結んで、`goal_submitted` からの 3 段の seek と全件を比べる。約 80 行。
- (ii) `candidates_sampled`: 試していない。次に要る試作: 確定した単位ごとに supervisor の生存と `candidates_sampled` を置き、S をまたぐ標本と、範囲の前に生存が終わった supervisor の標本を足して、`kpi` の candidates の値を全件と比べ、30/3000 で読んだ行の数を比べる。約 100 行。
- (ii) `live_alert_stream`: records が開いている stream の中で伸びるので、行の数でなく records の読みの問い。goal 273（ADR-t4644-1 の写さない kind と task 4660・4646）と Spike 4663（queue 行を要る列に絞る測定）が持つので、ここでは試作しない。

## 後続の draft への推奨

goal 273 と Spike 4663 と重ねない: (C, S] の写さない kind の queue の event と `live_alert_stream` の records は goal 273（task 4660 の共通の読み取り関数、4646）に、queue 行の読みを要る列に絞ることは Spike 4663 に任せる。この Spike の約束は「どの queue 行を読むか」だけを決め、行の中の何を読むかは決めない。

- 4667（索引の migration）: 4670 の索引に、`run_events` の `follow_up_registered` の `task_id` の部分の式の索引、`plan_review_id`・`proposal_id` の式の索引、`ledger_rows(kind, subject_kind, ended_at)`、`ledger_rows` の `json_extract(inputs_through, '$.events')` の式の索引を足す。
- 新しい task（builder の 2 つの表）: 開いたまま残した行の表と、最初の Execution の表を足す migration と builder の書き込み。run の session は close の `opened_event_id` で対にして表に入れる（全件の読みの `take_earlier` と同じ規則。追試で holds）。今の行から埋める段と、無い store では全件の読みに戻す形を含める。約 400〜600 行。4122 の前提。
- 4122（store の範囲の読みと小さな読み）: `earlier_in` の task・run・session 行の部分を「読みの約束」の 3〜5 に置き換え（session 行と名指す proposal・goal の task は追試の推奨、U_E は追試の (iii-d) が unresolved なので付けない）、queue 行を 6 の形で読む。`LedgerWindowRead` に R・W（`ledger_marks`）を足し、小さな読みは (iii) の条件で絞る。SQLite の adapter の test で `EXPLAIN QUERY PLAN` が索引を使うことを確かめる。
- 4132（domain の範囲の関数）: `take_earlier` は今のまま使える（渡す record が減るだけ）。名指すものの集め方（試作の `Named`）を domain に置く。
- 4133（kpi の配線）・4134（report の配線）: 受け入れの比較に、試作の 30 と 3000 単位の履歴と `Reads` の和の数え方を使う。(ii) の unresolved の kind を読む問いは範囲が決まらない問いに入れる。
- 4677（run_events の小さな読み）: (c)(d) の claim と (g) の `TASK_MARK_KINDS` を (iii) の条件で返す。(f) の cursor の時刻は見える event だけで引く。(iii) の条件と U_E は追試の「後続の draft への推奨」が持つ。
- supervisor の report の経路の draft: `SupervisorQueue` を通る読みも R・W を同じ snapshot で読む（task 4660 の `ledger_rows_covered` と同じく 1 回の要求で返す）。
- 新しい Spike か試作の task（(ii) の unresolved の kind（`candidates_sampled` を含む）。(i) の 4 つは追試（Spike 4755）で holds になり、(iii) は追試（Spike 4754）が済ませて U_E だけが追試の「未解決」に残る）: 上の「未解決」の試作。kpi の candidates を読む問いは、その判定まで範囲が決まらない問いに入れるか、4133・4134 のその部分を draft のまま残す。main の記録の判定が does_not_hold なら、`--by area` などは範囲が決まらない問いに入れる。

## 追試 (iii) 可視性（Spike 4754）

問いは、上の (iii) で unresolved だった 4 つの部分、(iii-a) 走行中の run、(iii-b) W・R の後で行に書かれていない session・queue の event と W・R の食い違い、(iii-c) R による持ち主の行の検査の省略、(iii-d) 台帳の行の上限 U_E について、`run_events` の小さな読みと範囲 (S, E] の窓の読みを、全件の読みと同じ答えのまま範囲の外の確定した履歴に比例しない形にできるか。判定は (iii-a)〜(iii-c) が **holds**、(iii-d) が **unresolved**（下の「判定」）。(i) earlier と (ii) queue 行の追試は別の Spike が持つ。

### 環境

- 対象の commit: `5af9d606b9b3d671167d2ebca4f1f6fbb07556a1`（この run の base。4678 の試作の commit `db9ed1c0e` を含む）。
- host: Apple M1、macOS 27.2（Darwin 27.2.0、arm64）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`cargo-nextest`。
- provider と経路: 本番 queue の Spike の run（task 4754）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。試作は 4678 と同じ in-memory（`domain::ledger::build` の行と `select` の窓）で、SQLite の adapter は使っていない。本番の queue と DB は読んでいない（合成の event だけ）。
- 使った時間と回数: 上限 2 時間に対して約 2 時間 20 分（1 回目の turn 16:54〜17:50 JST、supervisor の review の手直しの turn 17:51〜18:10 JST、18:16〜18:50 JST、18:55〜19:05 JST、19:07〜19:10 JST、19:10〜19:25 JST。2 時間を超えた分は review の手直しで、最後の手直しは全件の読みの量の数えを足しただけ。host の負荷で test の build と実行が 1 回 6〜10 分かかった時間を含む）。試作の作り直し（形を変えて全件と比べ直すこと）は上限の部分ごと 3 回に対して、(iii-a) 1 回、(iii-b) 1 回、(iii-c) 0 回、(iii-d) 3 回（上限に達した。supervisor の review の 3 回目の指摘は上限の後なので試作を直さず、(iii-d) を unresolved にして文書だけを直した）。(iii-a) の 1 回と (iii-d) の 2・3 回目は supervisor の review の指摘による: (iii-a) は event の `task_id` で task 行を引く形が、`task_id` の無い event や別の task の id を持つ event で全件と食い違うので、終わっていない task 行の `detail.runs` を引く形（全件の読みの `runs_on_their_way` と同じ所属）に替え、run 70 の R の後の event を `task_id` の無いものと別の task の id のものにした。(iii-d) は打ち切らない探索が E の後の履歴に比例するので、4670 の (vi) の K 件の打ち切りと上限なしへの戻りにし、E の後の履歴を 30 件と 3000 件にした場合と時計の戻りを足して読む行を比べた。(iii-d) の 3 回目は、`inputs_from` だけで行を除くと、全部の record が E より後に書かれた行の、payload の `at` が窓にある token の cut と、E の後に初めて現れた actor × provider の Execution（`LedgerWindow::execution_tokens` が窓の選びによらず全部の行から読む）を落とすので、4670 の (vi) の records の時刻の条件と Execution を持つ行を残す条件を足し、全部の event と見える event の 2 通りの上限をそれぞれ窓に渡して比べた（最初に試した、除いた行の最初の Execution を `earlier` に渡す形は、`stats` が `earlier` を窓の前の Execution として読むので答えを変え、採らなかった）。(iii-b) と (iii-d) の 1 回は subagent の review の指摘による: (iii-b) は (R, W] に session の event が無く W を R で代える場合を通らなかったので planner 990004 の開始を足し、印の後に書かれた行の数と中身を assert にした。(iii-d) は `select` が `inputs_from` が窓の終わりより後の行をもともと除くので上限の比べが落ちえなかったので、低すぎる上限で答えが変わる対照と歩いた数の assert を足した。部分ごとの時間は、コードの読みと store の組み立て約 25 分を (iii-a)(iii-b) に、(iii-c) 約 5 分、(iii-d) 約 10 分、文書と review と手直し約 20 分、supervisor の review の手直し 1 回目約 20 分（(iii-a) 約 5 分、(iii-d) 約 10 分、文書約 5 分）、2 回目約 35 分（(iii-d) 約 25 分、文書約 10 分）、3 回目約 10 分（文書だけ）、4 回目約 5 分（文書だけ）、5 回目約 15 分（(iii-a)〜(iii-c) の test に全件の読みの量の数え（`whole_reads`）を足し、文書を直した。試作の形は変えていないので作り直しに数えない）。
- 差分の大きさ: 試作 約 780 行、文書 約 120 行（上限の約 600 行を超えたが、review の指摘の直しに限って足した）。

### 読んだコードと打ったコマンド

読んだコード: 4678 の試作（`history`・`visible`・`small_read`・`store_cases`）、`src/domain/ledger/window.rs` の `runs_named`・`runs_on_their_way`・`run_on_its_way`・`Positions::of`（`Cursor::Time` の id は records の時刻が E 以下の id の最大）、`src/application/ports/observation.rs` の `ledger_read_snapshot`・`window_rows`・`with_runs_on_their_way`、`src/domain/ledger/owner.rs` の `owner`・`of_session`、`src/domain/ledger/session/written.rs`（`written_through`・`rows_to_resume`・`RowsWritten::position` の queue と core の別々の位置）、`src/domain/ledger.rs` の `LedgerRow::inputs_from`・`inputs_through`（行ごとの値）。

打ったコマンド:

```sh
git log -1 --format=%H
rustc --version; cargo --version; uname -sm
cargo nextest run --lib -E 'test(/earlier_visibility_spike::/)' --no-capture
sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings
sh scripts/cargo-brief.sh cargo nextest run --locked --lib --stress-duration 60s -E 'test(=application::ports::observation::earlier_visibility_spike::<足した test の名前>) | …'
sh scripts/check-local-gates.sh --base 5af9d606b9b3d671167d2ebca4f1f6fbb07556a1
```

### 試作

同じ file（`src/application/ports/observation/earlier_visibility_spike.rs`、`#[cfg(test)]` の未配線）に足した。runtime の読みは変えていない。

- store（`store_apart(settled)`）: 4678 の `history(settled)`（範囲の外に 30 か 3000 の確定した単位）の後に尾を足す。task 70 の作成と run 70 の claim と開始、planner 990001・990003 の session の開始までを R（run・task・node 行、`Marks::core`）まで書き、その後に run 70 の `first_commit_observed`（`task_id` の無い event）、task 70 の `task_priority_changed`（task の event）、resume した run 3 の event を置いて queue の R（`Marks::queue`）、その後に planner 990004 の session の開始、`mark_added` m-b と `claim_deferred` を置いて W（`Marks::w`）にする。W の後に planner 990001 と 990003 の close、planner 990002 の開始と close、`mark_added` m-a、run 70 の `receipt_observed`（別の task 999 の `task_id` を持つ event）、`inbox_watcher_absent` を置く。行は kind ごとに自分の印までの event から `build` で作り（R ≤ W、queue の R と core の R は別の位置）、run 70（走行中）と run 12（task が終わり run 行が書かれていない）の run 行を落とし、印の後まで進んだ pass が planner 990001 の session 行と m-a の queue 行の 2 行だけを書き直して止まった形にする。だから task 70 の行は終わっておらず `detail.runs` が run 70 を名指し、run 70 は走行中の run（`runs_on_their_way` がちょうど run 70）で、W・R の後の session・queue の event のうち 6 件はどの行にも無い。
- 小さな読み（`small_read_apart`）: run の event は run 行が保存され `inputs_through` が id 以上か、run 行が無く、保存された終わっていない task 行のどれかの `detail.runs` が run を名指すとき（終わっていない task 行を 1 回読み、event の `task_id` によらない）。task の event は task 行の `inputs_through` が id 以上か（`spare` のとき）id が core の R 以下なら行を引かない。session の event は id が W 以下か W の後に書かれた session 行の records にあるとき、queue の event は id が queue の R 以下か R の後に書かれた queue 行の records にあるとき。`one_mark` は queue の event も W で判じる。引いた数は `Looked`（持ち主の行・終わっていない task 行・印の後に書かれた行）。
- U_E（`u_e_capped`・`bounded`・`left_out`・`store_after`）: U_E は `run_events` を id の降順に最大 K = 64 件歩き、時刻が E 以下の最初の event の id。見つからなければ上限なし。全部の event の最大 id（`of_all`）と見える event だけの最大 id（`of_seen`）もそれぞれ上限にして比べる。行を除くのは、最初の入力（`inputs_from`）が U_E より後で、どの record も `created_at` と payload の `at` が E より後で、Execution（`tokens_source` を持つ record）を持たない行だけ（`left_out`。比べのために最初の入力だけで除く形も持つ）。保存された残りの行と、走行中の run を event から畳んだ行で窓を作る。`store_after` は E の後に、payload の `at` が窓の中（E の 10 分前）で E の 5 分後に書かれた inbox の token の cut、codex の planner の turn（runtime_planner × codex の最初の Execution）、`mark_added` 30 件か 3000 件（それぞれ queue 行）を書き、時計の戻りの場合は最後に時刻が E より前の event を 1 件足す。答えは `stats --since --until --full` と `LedgerWindow::execution_tokens` の JSON 全体。読む量は、窓の終わりより後に始まる行のうち読んだ数（`read_past_the_end`）と、除いた行の数と歩いた event の数。

| test | 場合 | 比べたもの |
| --- | --- | --- |
| `a_run_on_its_way_and_the_rows_past_w_and_r_are_seen_as_the_whole_read_sees_them` | 上の store（30 と 3000 単位）の範囲の event 150 件 | 全件の `visible` の 125 件と小さな読みが一致。run 70 の R の後の 2 件（`task_id` の無いものと task 999 のもの）が両方に見え、W・R の後で行に無い session・queue の event 6 件が両方から落ち、印の後に書き直した 2 行の event（planner 990001 の close と m-a）は両方に見える。`one_mark` は queue の R と W の間の 2 件を余分に取る。session を queue の R で判じると答えは同じで、印の後の行として読む session 行が増える。`Looked` は 30 と 3000 で同じ（持ち主の行 19、終わっていない task 行 2、印の後の行 2）。4678 の `Reads` の数え方で、小さな読みが読むのは引いた行 23 と候補の event 150 の 173 で 30 と 3000 単位で同じ、全件の読み（`whole_reads`: run・task・session 行、queue 行、走行中の run を畳む event）は 158 + 427 + 4 = 589 から 12,038 + 21,217 + 4 = 33,259 に増える |
| `r_spares_the_task_rows_of_the_task_events_up_to_it_and_not_the_run_rows` | 同じ store で、id ≤ core の R の task の event の行を引かない | 答えは全件と一致し、持ち主の行の引きは 19 → 12（30 と 3000 で同じ。終わっていない task 行 2 の読みは省けない）。読む量の和は 173 → 166 で 30 と 3000 単位で同じ（全件の読みは 589 と 33,259）。run 12 の event は id ≤ R でも全件の読みに見えないので、run の event は R で省けない |
| `u_e_of_every_event_and_of_the_visible_ones_and_searched_by_k_keep_what_the_whole_read_counts` | `stats --since`（2 日前）`--until`（E は読みの 85 分前）`--full` と `execution_tokens`、E の直前に見えない event、E の後に書かれた窓の中の cut と codex の最初の Execution。範囲の前の確定した履歴（30・3000 単位）と E の後の履歴（30・3000 件）、時計の戻り | 全部の event の上限（30 単位で 1580）と見える event の上限（1577）のどちらでも、最初の入力だけで除くと cut の token（`stats` の inbox）と codex の `recorded_from`（`execution_tokens`）を落とし、`left_out` で除くと答えの JSON 全体が全件と一致（この store の限り。E の後に始まった走行中の run が無く、反例は「判定」の (iii-d)）。2 通りとも 30 と 3000 単位で同じで、除いた行 31、窓の終わりより後に始まる行は 2 行（cut と codex の行）を読む（全件は 33）。K 件の探索は（30 単位, E の後 30 件）と（3000 単位, 30 件）で同じで、36 件歩いて見つかり、読む量は上の 2 通りと同じ。E の後 3000 件では K 件で打ち切って上限なしに戻り、その行を全件と同じ 3003 行読む。時計の戻りでは 1 件目で見つかるが U_E が全部の行より後で 3003 行読む。対照に task 70 の行の最初の入力より低い上限にすると、最初の入力だけで除く形は答えが変わり、`left_out` は records の時刻でその行を残して答えが変わらない |

比べなかった場合: SQLite の adapter の SQL、範囲の前の時計の戻り（4670 の (vi)）、R を保留する builder（store は R の定義どおりに作り、builder が R を守ることは試していない）、`--since` が `--until` より後の窓、W・R の無い store（印が無いか古い版の印）、(C, S] の写さない kind の queue の event（goal 273）。

### 判定

(iii-a)〜(iii-c) は holds、(iii-d) は unresolved。holds の 3 つは、4678 の `Reads` の数え方（引いた行と読んだ event の和）で、小さな読みの量が範囲の前の確定した履歴を 30 単位と 3000 単位にして同じ（173、R で省くと 166）で、同じ store の全件の読み（589 と 33,259）のように比例しない（上の表、`whole_reads` と `small_reads`）。答えは全件の読みが見る event と同じ。どれも event（`run_events`）を SSOT のまま読み、台帳の行（ビュー）とその印で見えるかを判じるだけで、新しい状態を持たない（ADR-t1662-2 決定 1・7・8）。

- (iii-a) 走行中の run: **holds**。根拠は `a_run_on_its_way_and_the_rows_past_w_and_r_are_seen_as_the_whole_read_sees_them`。run 行の無い run の event は、保存された終わっていない task 行（`ledger_rows WHERE kind = 'task' AND ended_at IS NULL`、約束 1 の窓の読みがもともと読む行）のどれかの `detail.runs`（`runs_named`）が run を名指すなら id によらず見える。event の `task_id` は使わないので、`task_id` の無い event と別の task の id を持つ event（試作の run 70 の R の後の 2 件）も全件の読みと同じに見える。これは全件の読みの `with_runs_on_their_way` が `runs_on_their_way` の run を `run_events(run)` の全部から畳むことと同じ所属の判定で、run 12 のように task が終わり run 行の無い run の event は落ちる（ask 1607 の補わない形のまま）。読む量は終わっていない task 行の数（B、試作で 2、30 と 3000 単位で同じ）で、確定した履歴に比例しない。
- (iii-b) W・R の後の session・queue の event と W・R の食い違い: **holds**。根拠は同じ test。session の event は id ≤ W か、`inputs_through.events > W` の session 行の records にあるとき、queue の event は id ≤ queue の R か、`inputs_through.events > R` の queue 行の records にあるとき見える。queue の event を W で判じると queue の R と W の間の event を余分に取る（逆に session を低い queue の R で判じると答えは同じで、印の後の行として読む session 行が増えるだけ）ので、W は `written_through`、queue の R は `rows_to_resume(.., true)`、run・task・node の R は `rows_to_resume(.., false)` の別々の位置を同じ snapshot で読む。印の後に書かれた行は `ledger_rows(kind, json_extract(inputs_through, '$.events'))` の式の索引で引き、数は係の遅れに比例する（試作で 2）。印の無い store では session・queue の event は判じられないので全件の読みに戻す。
- (iii-c) R による検査の省略: **holds**（task の event について）。根拠は `r_spares_the_task_rows_of_the_task_events_up_to_it_and_not_the_run_rows`。id ≤ core の R の task の event は行を引かずに見える（R の定義で task 行が記録している。持ち主の行の引きは 19 → 12）。queue と session の event は (iii-b) のとおりもともと行を引かない。run の event は R 以下でも省けない（builder は終わるまで run 行を書かず、書き込みの上限で残った run もある）。
- (iii-d) U_E: **unresolved**。試作（`u_e_of_every_event_and_of_the_visible_ones_and_searched_by_k_keep_what_the_whole_read_counts`）が示したのは次の 3 つ。(1) 最初の入力（`inputs_from`）だけで行を除く上限は、全部の event の最大 id（30 単位で 1580）と見える event の最大 id（1577）のどちらでも答えを変える。`select` は最初の入力が窓の終わりより後の行を選ばないが、`stats` は payload の `at` が窓にある token の cut を数え（`executions::executions`）、`LedgerWindow::execution_tokens` は窓の選びによらず全部の行の Execution を読んで actor × provider の最初の時刻（`recorded_from`）を出すので、全部の record が E より後に書かれた行もその入力になる。(2) 最初の入力が U_E より後で、どの record の `created_at` も payload の `at` も E より後で、Execution（`tokens_source`）を持たない行だけを除く形（`left_out`）は、試作の store では 2 通りの上限とも答えが全件と一致し、窓の終わりより後に始まる行は 33 行のうち 2 行だけ読む（30 と 3000 単位で同じ）。(3) U_E を `run_events` の id の降順に最大 K = 64 件で打ち切って探すと、試作の store では 36 件で見つかり、E の後に K 件より多く書かれた範囲では K 件で打ち切って上限なしに戻り、全件と同じだけ読む（4670 の (vi)）。時計の戻りでは、最後に書かれた時刻の戻った event で探索は 1 件で見つかるが、その U_E は全部の行の最初の入力より後なので、上限を付けても読む量は全件と同じで減らない。ただし (2) の形は全件と同じにならない場合があり、判定にできない。supervisor の review が示した反例では、E の後に `task_created` と `run_claimed` が書かれて task 行だけが保存され、その run に最初の worker × codex の `turn_finished`（`tokens_source` つき）がある。その task 行は Execution の record を持たないので `left_out` で除かれ、残りの行から `runs_on_their_way` を引くので run は畳まれず、全件の `execution_tokens` の `by_provider.codex.recorded_from` が消える（試作の `bounded` は除いた後の行から走行中の run を引く）。試作の store は走行中の task 70 を E より前に始めているので、この場合を通らない。作り直しの上限の 3 回に達したので、この場合の試作は組んでいない。今に近い範囲で上限が除けるのは K 行以内（U_E が K 件の中で見つかれば U_E より後の event は K 件より少ない）で、上限を付けない今の読みとの差はその分だけ。cursor の event の id から時刻を引くとき（4678 の「event の id の cursor の時刻」）は逆向きで、見える event だけで引く規則のまま。

### 読みの約束の更新

4678 の「読みの約束」1〜6 は変えない。7 を次に置き換える（上の 7 はこの節を指す）。

7. `run_events` の小さな読みは、event を持ち主ごとに判じて返す。run の event は run 行の `inputs_through` が id 以上か、run 行が無く、終わっていない task 行のどれかの `detail.runs` が run を名指すとき（event の `task_id` によらない）。task の event は id が run・task・node の R 以下か、task 行の `inputs_through` が id 以上のとき。session の event は id が W 以下か W の後に書かれた session 行の records にあるとき、queue の event は id が queue の R 以下か R の後に書かれた queue 行の records にあるとき。W と 2 つの R は窓の行と同じ snapshot で別々に読み、印の無い store では session と queue の event を全件の読みで判じる。行に記録されていない run に属する入力は補わない（ask 1607）。cursor の event の時刻は見える event で引く。台帳の行の上限 U_E は (iii-d) の判定まで約束に入れず、窓の終わりより後の行も今のとおり読む（上限を付けない）。

### 未解決

(iii-a)〜(iii-c) に unresolved の部分は無い。比べなかった場合（上の「試作」）は約束の条件として書いた: 印の無い store（全件の読みに戻す）、範囲の前の時計の戻り（4670 の (vi)）、SQLite の adapter の SQL（4122 の adapter の test が持つ）。

- (iii-d) U_E: 試したことは、最初の入力だけで除く上限と、records の時刻と Execution を持つ行を残す上限（`left_out`）を、全部の event と見える event の 2 通りの U_E と K 件の探索で、範囲の前の 30・3000 単位、E の後の 30・3000 件、時計の戻りの store で全件と比べたこと（上の「判定」の (1)〜(3)）。止まった所: `left_out` は E の後に始まった走行中の run の Execution を保てない（task 行に Execution の record が無く除かれ、除いた後の行から走行中の run を引くので畳まれない）。止まった理由: 作り直しの上限の 3 回に達した（3 回目までの指摘はどれも全件の読みが窓の選びによらず読む入力で、残る入力が他にもあるかを試作で尽くしていない）。次に要る試作（試していない修正案）: 走行中の run を除く前の行（終わっていない task 行の全部、約束 1 の窓の読みがもともと読む行）から引いて畳み、終わっていない行は上限で除かない形にして、E の後に `task_created`・`run_claimed`・worker × codex の `turn_finished` だけを持つ task と run を置き、`stats` と `execution_tokens` を全件と比べる。あわせて窓の選びによらず全部の records を読む `LedgerWindow` の読み（今は `execution_tokens`）を列挙し、それぞれの入力を上限が保つかを確かめる。約 60〜100 行の見込み。上限が除けるのは今に近い範囲で K 行以内なので、planner はこの試作をせず U_E を約束から外す（上限を付けない）ことも選べる。

### 後続の draft への推奨

goal 273 と Spike 4663 と重ねない: (C, S] の写さない kind の queue の event は goal 273（task 4660 の共通の読み取り関数、4646）が扱い、この約束の queue の R の規則はその外の queue の event に当てる。queue 行を要る列に絞ることは Spike 4663 に任せる。

- 4677（run_events の小さな読み）: 約束 7 の判定を足す。`LedgerWindowRead` から W と 2 つの R（`ledger_marks`）を同じ snapshot で読み、run 行の無い run は終わっていない task 行の `detail.runs` で判じ（event の `task_id` を使わない）、印の後に書かれた session・queue 行を式の索引で引く。unit test は試作の 3 つの test の場合（走行中の run、印の後で行に無い event、R と W の食い違い、R の省略、run 12）を 30 と 3000 単位で。約 250〜350 行。
- 4122（store の範囲の読みと小さな読み）: U_E は (iii-d) が unresolved なので足さず、窓の終わりより後の行も今のとおり読む（上限を付けない。答えは全件と同じで、上限で減らせるのは今に近い範囲で K 行以内）。U_E を足すなら上の「未解決」の試作の判定の後に、その形（K 件で打ち切る探索、records の時刻と Execution と終わっていない行を残す条件、除く前の行から走行中の run を引くこと）で足し、adapter の test は `stats` の答えと `LedgerWindow::execution_tokens` の両方を全件と比べる。小さな読みの部分は 4677 の判定を使う。大きさ: この追試が 4122 に足すのは、小さな読みを 4677 の判定に通す配線（`ledger_read_snapshot` の中で W と 2 つの R を窓の行と同じ snapshot で読んで渡すこと）の約 30〜50 行だけで、U_E の部分は 0 行（足さない）。U_E を後で足すなら、未解決の試作の判定の後に約 100〜150 行の見込み。
- 4667（索引の migration）: `ledger_rows(kind, json_extract(inputs_through, '$.events'))` の式の索引（4678 の推奨と同じもの）で足り、追試で新しい索引は増えない。
- 4133（kpi の配線）・4134（report の配線）: (iii) は配線を止めなくなった（U_E を付けない上の側は今の読みのままで、答えは変わらない）。4677 と 4122 の着地を前提に、(i)・(ii) の追試の判定に合わせて書き直す。この追試が足すものは無い。
- 4679（supervisor の経路）: `SupervisorQueue` を通る読みも W と 2 つの R を窓と同じ 1 回の要求で返す（task 4660 の `ledger_rows_covered` と同じ形）。約 50〜100 行の見込み。

## 追試 (i) earlier（Spike 4755）

上の「未解決」の (i) の 4 つの部分を、上の試作を広げて全件の読みと比べ直した。判定は 4 つとも **holds**（下の「判定」）。(ii) は同じ proposal の別の Spike が持ち、(iii) は上の「追試 (iii) 可視性（Spike 4754）」が持つので、ここでは扱わない。

### 環境

- 対象の commit: `e80bf6d961971db0e670de2c7f7a613ee964102b`（この run の base。4678 の結果の commit から試作と文書の変更は無い）。
- host: Apple M1、macOS 27.2（Darwin 27.2.0、arm64）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`cargo-nextest`。
- provider と経路: 本番 queue の Spike の run（task 4755）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。試作は上と同じ in-memory の `#[cfg(test)]` の test で、SQLite の adapter は使っていない。本番の queue と DB は読んでいない（合成の event だけ）。
- 使った時間と回数: 上限 2 時間に対して、調査と試作で約 50 分（10:20〜11:10 UTC。コードの読みの約 25 分は 4 部分に共通、test の実行の待ちを含む）、文書と review で約 40 分、review の手直し（全件の側の読む量の数え）で約 25 分。試作の作り直し（形を変えて全件と比べ直すこと、上限は部分ごとに 3 回）は 4 部分とも 0 回（最初の形で全件と一致した）。部分ごとの時間は (i-c) 約 10 分、(i-a) 約 10 分、(i-b) 約 5 分、(i-d) 約 10 分（共通の読みを除く）。(i-a) の test は 1 回目に読む量の比べに queue 行を含めて落ちた。queue 行は (ii) の範囲でこの試作では全部読むので、比べを run・task・session 行、earlier、最初の Execution に直した（読みの形は変えていないので作り直しに数えない）。
- 差分の大きさ: 試作 約 640 行（review の手直しで全件の側の読む量の数えを足した。見込みの約 320 行より大きく、文書と合わせて上限の目安の約 600 行を超えたので、試作をこれ以上広げなかった）、文書 約 110 行。

### 読んだコードと打ったコマンド

読んだコード（上の節に足したもの）:

- `src/domain/ledger/window.rs`: `WindowRows::take_earlier`（session は close の `opened_event_id` の集合で対にし、対にならない open の run を wanted にする）、`RowsFrom::leaves_out`。
- `src/domain/stats/sessions.rs::spans`（close は `opened_event_id` が名指す open の最初の 1 つだけを閉じ、名指さない close と 2 つ目の close は何も閉じない）、`src/domain/sessions.rs::SpanRef`。
- `src/domain/ledger/session.rs`（session 行の key は `{kind}/{planner か plan review の id}/{what}/{round}`、`span_target`・`outside_of`）、`src/domain/ledger/owner.rs`（`ask_opened` は run の無いとき task の行か queue の行）。
- `src/domain/stats/planner_routes.rs`（`planner_question` の ask の route は actor の `planner:<id>` から、route は `runtime_planner` の `session_opened` か turn の event から）。
- `src/domain/kpi/window.rs::improvement_proposals`（`goal_submitted` の goal の task は window の attributes の `task_goals` から）、`src/domain/finding.rs::improvement_running`（終わりは completed と canceled）、`src/domain/ledger/fold.rs` の task の終わり（task 行が終わるのは最後の状態が completed か canceled のときだけ）、`src/domain/ledger/attributes.rs::fold_within`。

打ったコマンド:

```sh
git log -1 --format=%H
rustc --version; cargo --version; uname -sm
cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::(a_run_s|a_planner_s|a_proposal_s_goal|every_shape)/)' --no-capture
sh scripts/cargo-brief.sh cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::/)' --no-capture
sh scripts/cargo-brief.sh cargo fmt --all --check
sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings
cargo nextest run --locked --lib --success-output final -E 'test(/earlier_visibility_spike::(a_run_s|a_planner_s|a_proposal_s_goal|every_shape|a_member_task)/)'
sh scripts/cargo-brief.sh sh scripts/check-design-docs.sh
sh scripts/cargo-brief.sh sh scripts/check-design-sentences.sh
sh scripts/check-local-gates.sh --base e80bf6d961971db0e670de2c7f7a613ee964102b
```

### 試作

同じ `src/application/ports/observation/earlier_visibility_spike.rs` に、部分ごとの読みの形を `Shape` で切り替える `narrowed_shaped` と、4678 の形（`OF_4678`）・全部の形（`SHAPED`）を足した。どれも未配線の `#[cfg(test)]` で、runtime の読みは変えていない。答えは kpi の 4 問と `stats --since`（2 日前）`--full` の JSON 全体を全件の読みと比べ（`shape_misses`）、読む量は 4678 の `Reads` の run・task・session 行、earlier、最初の Execution で比べる（queue 行は (ii) の範囲で、この試作では全部読む）。fixture は上の `history` に部分ごとの場合を足す `history_with`。

| test | 場合 | 比べたもの |
| --- | --- | --- |
| `a_run_s_session_closed_by_an_old_close_or_twice_stays_open_as_the_whole_read_pairs_it` | (i-c) 範囲の前に着地した run 70 に open と `opened_event_id` の無い close、run 71 に open 2 つと 2 つ目の open を名指す close 2 つ（`numbered` の補いを上書きして消す） | 対にした `open_at_end` は両方を開いたままとし、kpi と stats の答えが全件と一致（30 と 3000 単位で行 34・earlier 34・Execution 1）。件数の比べ（4678 の形）は両方を閉じたとし、`session_open.review` が全件と食い違う |
| `a_planner_s_session_row_is_read_by_the_prefix_of_its_key_and_the_planner_an_ask_s_actor_names` | (i-a) 範囲の前に開いて閉じた planner 70（route `headless`）の session と、範囲に actor `planner:70` の `planner_question` の ask 2 件（task の無い queue の行と、task 62 の行） | key の接頭辞（`runtime_planner/<id>/`・`plan_review/<id>/`）の seek と actor の planner で答えが一致（30 と 3000 単位で行 30・earlier 22・Execution 1）。4678 の形と actor を足さない接頭辞の seek は `health.planner_routes` の route が `unknown` になり食い違う |
| `a_member_task_s_ends_alone_answer_as_its_whole_history_whatever_its_own_history` | (i-b) proposal 8 の task 66（範囲の前に完了）に範囲の前の `task_priority_changed` を 30 件と 3000 件 | 終わりだけを読む形（`member_end`）も records を全部読む形も答えが一致。earlier は終わりだけなら 20 と 20、records を全部なら 50 と 3,020 |
| `a_proposal_s_goal_tasks_are_read_by_the_goal_submitted_naming_it` | (i-d) goal 80 を proposal 9 に `goal_submitted` で入れ、finding 51 で結ぶ。task 81 は範囲で完了、82 は範囲の前に完了、83 は範囲の前に cancel で、その `task_created` が範囲の名指す plan review 9 を名指す | 3 段の seek（`Named::note_goal_members`）で `improvement_proposals` を含む答えが一致（30 と 3000 単位で行 32・earlier 25・Execution 1）。4678 の形と goal の seek の無い形は task 83 の `task_created` だけを読んで cancel を読まず、`improvement_proposals` が 1（全件は 0） |
| `every_shape_returns_as_many_records_whatever_the_settled_history` | 上の 4 つの場合を合わせ（task 66 の変更は 30 件）、全部の形 | 30 と 3000 単位で答えが一致し、行 36・earlier 41・Execution 1 で同じ |

各 test は同じ追加の fixture で、全件の側の読む量も 4678 の `Reads` の単位で数える（`full_reads`・`note_full`）。「今の読み」は狭める部分を渡さない時刻からの読み（4678 の `earlier_today` と同じ）、「全行の読み」は全件の答えを出す全部の行の読みで、行を丸ごと返すので earlier は 0 になり、行が持つ records の数を添える。値は kpi の日の問い（`--period day --last 2`）のもので、`行 / earlier / Execution / queue 行` の順に並べる。queue 行はこの追試で狭めていない（(ii) の範囲）。

| 場合 | 単位 | 追試の形 | 今の読み | 全行の読み（records） |
| --- | --- | --- | --- | --- |
| (i-c) | 30 | 34 / 34 / 1 / 428 | 34 / 474 / 1 / 428 | 159 / 0 / 0 / 428（1,167） |
| (i-c) | 3000 | 34 / 34 / 1 / 21,218 | 34 / 42,054 / 1 / 21,218 | 12,039 / 0 / 0 / 21,218（81,357） |
| (i-a) | 30 | 30 / 22 / 1 / 427 | 30 / 458 / 1 / 427 | 156 / 0 / 0 / 427（1,126） |
| (i-a) | 3000 | 30 / 22 / 1 / 21,217 | 30 / 42,038 / 1 / 21,217 | 12,036 / 0 / 0 / 21,217（81,316） |
| (i-b)（task 66 の変更） | 30 件 | 30 / 20 / 1 / 426 | 30 / 486 / 1 / 426 | 155 / 0 / 0 / 426（1,153） |
| (i-b)（task 66 の変更） | 3000 件 | 30 / 20 / 1 / 426 | 30 / 3,456 / 1 / 426 | 155 / 0 / 0 / 426（4,123） |
| (i-d) | 30 | 32 / 25 / 1 / 430 | 32 / 465 / 1 / 430 | 160 / 0 / 0 / 430（1,165） |
| (i-d) | 3000 | 32 / 25 / 1 / 21,220 | 32 / 42,045 / 1 / 21,220 | 12,040 / 0 / 0 / 21,220（81,355） |
| 4 つを合わせて | 30 | 36 / 41 / 1 / 433 | 36 / 515 / 1 / 433 | 165 / 0 / 0 / 433（1,242） |
| 4 つを合わせて | 3000 | 36 / 41 / 1 / 21,223 | 36 / 42,095 / 1 / 21,223 | 12,045 / 0 / 0 / 21,223（81,432） |

追試の形の行・earlier・Execution は 30 と 3000 で同じで、今の読みの earlier と全行の読みの行と records は単位（(i-b) は task 66 の変更の数）に比例して増える。test はこの増え方も確かめる（今の読みの earlier と全行の読みの行が 3000 で 30 の 25 倍を超える。(i-b) は今の読みの earlier と全行の records が 2,970 以上増える）。

比べなかった場合: SQLite の adapter の SQL（下の SQL は試作の in-memory の条件と同じ式だが流していない）。`goal_review` など planner と plan review 以外の session 行（試作の履歴に無い）。`--compare`・`--goal`・report の経路。

### 判定

4 つとも **holds**。4678 の「判定」の (i) の表の該当の行はこの判定に従う。読む量の全件の読み（今の読みと全行の読み）との比べは上の「試作」の読む量の表で、答えの比べは下の表の test。

| 部分 | 判定 | 読みの形 | 全件と同じになる根拠 | 読む量 |
| --- | --- | --- | --- | --- |
| (i-c) run の session が行の終わりに開いたままか | holds | builder が run 行を書くとき、その行の `session_opened` のうち、同じ行の `session_closed` の `opened_event_id` の集合に無いものを開いたまま残した行の表に書き、名指す close が来たら消す（試作の `open_at_end(row, true)`。`take_earlier` と `stats::sessions::spans` と同じ規則。名指さない close と 2 つ目の close は何も消さない） | `a_run_s_session_closed_by_an_old_close_or_twice_stays_open_as_the_whole_read_pairs_it` | B（開いたまま残した run の数） |
| (i-a) session 行の records | holds | 範囲の record が名指す planner（payload の `planner_id` と actor の `planner:<id>`）と plan review（`plan_review_id` を text で）の session 行だけを、主キー `(kind, key)` の key の範囲で読む: `kind = 'session' AND key >= 'runtime_planner/<id>/' AND key < 'runtime_planner/<id>0'`（`'0'` は `'/'` の次の文字）、plan review も同じ。索引は今の主キーで足りる。actor は読んだ run・task・session 行の record（`Named::note`）と、範囲の queue の record（S 以降）から集める | `a_planner_s_session_row_is_read_by_the_prefix_of_its_key_and_the_planner_an_ask_s_actor_names`。4678 の形が残した task・proposal を名指す session 行を落としても答えは変わらない（試作の履歴の限り） | 名指す planner と plan review の session 行の数（A） |
| (i-b) 名指す proposal の task の読む量 | holds | 2 段目で名指した task（範囲がほかに名指さないもの）は records を全部でなく、`task_created`、最後の `task_goal_changed`、最後の `task_status_changed`、その後の `task_reopened` だけを読む（`member_end`）。外した task 行の records は全部 S より前なので「E 以前の最後」は「最後」と同じ。SQL は task ごとに `WHERE task_id = ? AND kind = ? ORDER BY id DESC LIMIT 1`（`task_reopened` は `id >` その status の id）で、索引 `run_events(task_id, kind, id)` を足す（今の `events_by_task(task_id, id)` だけでは task の終わりの後の event を歩く） | `a_member_task_s_ends_alone_answer_as_its_whole_history_whatever_its_own_history`（task 自身の範囲の前の履歴を 30 件と 3000 件にして答えと earlier が同じ）。増やした履歴は `task_priority_changed` だけで、ほかの kind（依存・`task_edited`）は `member_end` が読まない kind なので同じ経路だが流していない | 名指す proposal の task の数 × 定数 |
| (i-d) improvement_proposals の goal を通る所属 | holds | 範囲が名指す proposal と、読んだ finding の行が `proposed` で結ぶ proposal を名指す `goal_submitted` を、`proposal_id` の式の索引（4678 の推奨と同じ、`WHERE kind = 'goal_submitted'`）で seek し、その goal の task を `task_created` の `goal_id` と `task_goal_changed` の `to` の部分の式の索引で seek し、その task は (i-b) と同じ終わりだけを読む | `a_proposal_s_goal_tasks_are_read_by_the_goal_submitted_naming_it` | 名指す proposal の goal の task の数（A）。試作は queue の行（finding と `goal_submitted`）を全部読んだので、結ぶ proposal を集める読みの量は数えていない（下の「未解決」） |

- (i-d) の根拠の補足: 外した task 行は最後の状態が completed か canceled のときだけ終わる（`fold.rs` の task の終わり）ので、goal の task が丸ごと window に無ければ `improvement_running` の答えは変わらない。食い違うのは task 83 のように外した task の record が一部だけ（別の名指しで）window に入り、その終わりが入らないときで、3 段の seek はその task の終わりを足して答えを全件に揃える。
- 終わりの読み（`member_end`）は `task_created` だけを作成の record とする。attributes は `attributes_baseline` も作成と同じに読む（`attributes.rs` の `apply_task`）が、baseline だけを持つ task は試作に無い。4132 で `member_end` と goal の task の seek に `attributes_baseline` を含める。
- ADR-t1662-2 決定 1・7・8 との関係: (i-c) は 4678 の「開いたまま残した行の表」に run の session を入れる規則で、表は event だけから作り直せる台帳の一部（ビュー）のまま。(i-a)・(i-b)・(i-d) は `ledger_rows` の主キーと `run_events` の索引の seek で、行の形と `ledger_v` を変えない。足す索引は `run_events(task_id, kind, id)` と、`task_created` の `goal_id`・`task_goal_changed` の `to` の部分の式の索引。

### 読みの約束の更新

- 約束 3: run の session を入れる。開いたまま残した行の表は slot・ask・landing の待ちと、close の `opened_event_id` で対にならない `session_opened` を持つ。外した run 行の session の record を今の読みで読む但し書きは消す。
- 約束 4: session 行は「範囲の record が名指す planner（actor の `planner:<id>` を含む）と plan review の session 行を key の接頭辞で」に替え、今の読みの但し書きを消す。範囲が直に名指す task・plan review・proposal の record は今の約束のまま読み、2 段目と 3 段目でだけ名指す task、つまり名指す proposal の task と、名指す proposal を名指す `goal_submitted` の goal の task（読んだ finding が結ぶ proposal を含む）は、その task の終わり（`task_created`・最後の `task_goal_changed`・最後の `task_status_changed`・その後の `task_reopened`）だけを読む。読む量と goal を通る所属の但し書きを消す。
- 約束 1・2・5・6 は変えない（7 は「追試 (iii) 可視性（Spike 4754）」の「読みの約束の更新」が置き換える）。

### 未解決

この追試の 4 部分に unresolved は無い。限りは「比べなかった場合」（SQL を流していない、planner と plan review 以外の session 行と `attributes_baseline` だけの task が試作の履歴に無い）と次の 1 つ。

- (i-d) と (ii) の finding の読みの組み合わせ: (i-d) の test は queue の行を全部読み、結ぶ proposal を全部の finding の行から集めた。`improvement_proposals` は `proposed` になった finding の結びを、その finding が `open` に戻るまで保つ（`kpi/window.rs` の `FindingStatusChanged`）。だから S より前に `proposed` になって settled した finding の行を、(ii) の finding の読み（S に開いているか範囲の record を持つ行）は読まず、その proposal を数え落としうる。これは goal の seek の形でなく、`improvement_proposals` の入力の proposal を集める queue の行の読みの問いなので、(ii) の追試の finding の場合に入れる。次に要る試作: S より前に proposed から settled になった finding が結ぶ proposal（task が範囲で動いている）を置き、(ii) の finding の読みで全件と比べる。約 50 行。

### 後続の draft への推奨

- builder の表の新しい task: 開いたまま残した行の表に run の session を入れ、close の `opened_event_id` で対にする（名指さない close と重複した close は消さない）。上の test の 2 つの run の場合を builder の unit test に写す。4678 の見込みの約 400〜600 行に約 50 行足す。
- 4667（索引の migration）: `run_events(task_id, kind, id)` と、`task_created` の `json_extract(payload, '$.goal_id')`・`task_goal_changed` の `json_extract(payload, '$.to')` の部分の式の索引を足す（`goal_submitted` の `proposal_id` は 4678 の推奨の式の索引に含める）。約 30 行。
- 4122（store の範囲の読み）: `earlier_in` の session 行の部分を key の接頭辞の範囲の読みに替え、名指す proposal の task と goal の task の終わりの seek を足す。SQLite の adapter の test で、上の 4 つの場合を全件と比べ、`EXPLAIN QUERY PLAN` が主キーと足した索引を使うことを確かめる。約 150 行。
- 4132（domain の範囲の関数）: 名指すものの集め方（試作の `Named`）に actor の `planner:<id>`、`plan_review_id` の text、読んだ finding が結ぶ proposal、goal の task の集め方と `member_end` を置く。約 100 行。
- 4133・4134（kpi・report の配線）: `planner_routes` を読む問いは (i) では「範囲が決まらない」に入れなくてよい。`improvement_proposals` も (i) では入れなくてよいが、上の「未解決」の (ii) の finding の読みの判定までは finding の行を全部読む。受け入れの比較に上の 4 つの場合を足す。「範囲が決まらない」に入れる問いは (i) からは無い（(ii)・(iii) の判定に従う）。
