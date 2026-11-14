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

判定は全体で **unresolved**（(i) は holds（4678 が unresolved とした session 行の records・improvement_proposals の goal を通る所属・proposal の task の読む量・run の session の開いたままの判定は下の「追試 (i) earlier（Spike 4755）」で holds）、(iv) は holds、(ii) は 4678 で kind ごとに holds と unresolved に分かれ、unresolved の kind は追試（Spike 4756）でも、古い snapshot と範囲の間の mark が does_not_hold、ほかは 3000 単位の答えの比べが上限で済まず unresolved（30・300 単位の答えと 30・3000 単位の行の数では一致の見込み）、(iii) は 4678 で試した部分と追試（Spike 4754）の (iii-a)〜(iii-c) で holds、(iii-d) U_E は unresolved。下の「判定」と「追試 (iii) 可視性（Spike 4754）」「追試 (i) earlier（Spike 4755）」「追試 (ii) queue 行（Spike 4756）」。4756 の unresolved の部分は「追試 (ii) の残り（Spike 4811）」で holds）。

- (i) earlier: **holds**（builder が新しく書く 2 つの小さな表が要る。4678 が unresolved とした session 行の records、improvement_proposals の goal を通る所属、名指す proposal の task の読む量、run の session が開いたままかの判定は「追試 (i) earlier（Spike 4755）」で holds）。task・session・run 行の earlier を「範囲が名指すものの seek（A）」と「S に開いているもの（B）」だけにすると、kpi の 4 問と `stats --since --full` の答えが全件と一致し、返る earlier の record は 30 単位でも 3000 単位でも 20 件（今の読みの in-memory の形は 456 件と 42,036 件）。ただし確定した単位は範囲と何も名指し合わず、開いたまま残るものも持たない（下の「試作」の限り）。範囲と結ばれる古い履歴は手で作った約 10 の場合だけで確かめた。同じ proposal の task の片方が範囲の前に終わる場合は、proposal が名指す task の記録まで読まないと proposal を動いていると数え違える（`a_proposal_the_range_names_is_read_with_the_ends_of_its_tasks_before_the_range`）ので、名指す proposal から task へ 2 段で seek する。ただし試作はその task の records を全部返すので、範囲の前に終わった task が範囲の前の record を多く持つと読む量はその履歴に比例する（30/3000 の比べはこの経路を増やしていない）。その task の終わりだけを読む形は追試で holds。goal の task が proposal に入る所属（`goal_submitted` が名指す proposal の goal の task）は 4678 の試作に無く、追試で holds。B を確定した履歴を走査せずに見つけるには、run 行が終わったときに開いたまま残したもの（slot・ask・session・landing の待ち）を builder が別の表に書く要がある。最初の Execution（C）は返る行は 1 件のままだが DB の走査は外した run 行の全部に比例するので、actor × provider の最初の時刻を builder が別の表に書く。
- (ii) queue 行: **部分ごとに分かれる**。控え（`claim_deferred`）・watcher の待ち・alert・finding・landing の slot・mark（`mark_added`）は「S に終わっていないか S 以降の record を持つ行、kind ごとの S 前の最後の行、`queue_before` の行」で全件と一致し、3000 単位でも返る行の数が変わらない（holds）。`slots_full`・`claim_hold`・`landing_hold`、supervisor の生存と heartbeat、forecast の snapshot と baseline（試作で行の数が単位で増えない）、mark の撤回、main の記録（D の見込み）、post_landing、goal の属性、update・backend の失敗・run の無い ask と repair、`stall_config`、`candidates_sampled`（kpi の候補の数の標本）は 4678 では **unresolved** で、「追試 (ii) queue 行（Spike 4756）」でも unresolved（snapshot と範囲の間の mark は、snapshot が確定した履歴より古いとき does_not_hold）。`live_alert_stream` の records は goal 273 と Spike 4663 が持つ。4756 で unresolved に残った kind は「追試 (ii) の残り（Spike 4811）」で全部 holds になった。
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

### (ii) queue 行: 部分ごとに分かれる（4678 で unresolved の行は追試でも unresolved）

下の表で unresolved の行は「追試 (ii) queue 行（Spike 4756）」の「判定」が見込みの読みの形と足りない根拠を持つ（`live_alert_stream` を除く）。表の `mark_added` は今の kind に無い名前（mark は `mark_recorded`）で、その行の holds は読む fold の無い 1 件ごとの行の読みとしてのもの。

根拠の test は `each_queue_kind_read_by_its_range_keeps_the_answers_of_every_row_or_names_what_it_misses` と `the_reads_of_the_whole_path_add_up_to_as_many_whatever_the_settled_history`（kpi）、`stats_since_a_time_answers_as_the_whole_read_with_the_narrowed_earlier`（stats）。読みの形は kind ごとに「S に終わっていない（`ended_at IS NULL OR ended_at >= S`）か S 以降の record を持つ行（`inputs_through.events > queue_before`）、kind ごとの S 前の最後の行、`queue_before` を持つ行」（試作の `queue_narrowed`）。`ledger_rows(kind, subject_kind, ended_at)` の索引と、`json_extract(inputs_through, '$.events')` の式の索引が要る。

| kind と読む fold | 判定 | 根拠と残る量 |
| --- | --- | --- |
| 控え（`claim_deferred`）と watcher の待ち（`inbox_watcher_absent`・`inbox_watcher_returned`） | holds | 30/3000 から 1/1・2/2 に減り答えは一致 |
| `slots_full`・`claim_hold`・`landing_hold` | unresolved（追試でも unresolved） | 試作の履歴に無い（同じ区間の行の形なので控えと同じ形の見込み） |
| `live_alert`（1 件ごと） | holds | 30/3000 → 1/1 |
| `finding`（findings_open は B、finding_resolve_time は A） | holds | finding の行は settled になるまで `ended_at` が無いので、S に開いている finding は終わっていない行の読みで足りる。範囲で解決したものは S 以降の record を持つ。32/3002 → 3/3 |
| `landing_slot` | holds | 45/3015 → 8/8 |
| `mark_added` | holds | 30/3000 → 1/1 で kpi の 4 問は一致。mark の読みの約束は 4135・4670 が持つ |
| supervisor の生存と heartbeat（`supervisor_started`・`supervisor_stopped`・`supervisor_alive`） | unresolved（追試でも unresolved） | 狭めても答えは一致した（191 → 40）が、行の数は確定した単位で増えない（日ごと）ので 30/3000 の比べになっていない。S 前の最後の heartbeat は supervisor ごとに 1 行要る（試作は kind ごとの最後） |
| mark の撤回、forecast の snapshot、baseline（`forecast_recorded`・`attributes_baseline`・`finding_baseline`） | unresolved（追試でも unresolved、古い snapshot と範囲の間の mark は does_not_hold） | 撤回は履歴に無い。snapshot と baseline は 1 行ずつで単位で増えない |
| main の記録（`main_log`・`main_history`・`main_covers`、`MAIN_LOG_KINDS`） | unresolved（追試でも unresolved） | 試作の履歴に無い。`fold_main_log(self.events())` が記録の全部を畳む（main が持つ path は全期間の記録から）ので、範囲の形では D の見込み |
| `post_landing_book`（revert と CI の失敗） | unresolved（追試でも unresolved） | 試作の履歴に無い |
| goal の属性 | unresolved（追試でも unresolved） | 試作の履歴に goal の event が無い |
| update・backend の失敗・run の無い ask と repair | unresolved（追試でも unresolved） | 試作の履歴に無い |
| `stall_config`（E 以前の最新） | unresolved（追試でも unresolved） | 試作の履歴に無い。`events_by_kind` を E 以前で降順に 1 件の形の見込み |
| `live_alert_stream`（judgments） | unresolved（goal 273 と Spike 4663） | 試作の履歴に無い。行は baseline ごとに 1 つだが、開いている stream の records が 60 秒ごとに伸びる |
| `candidates_sampled`（kpi の候補の数、`Context::candidates`） | unresolved（追試でも unresolved） | 試作の履歴に無い。stream とは別の queue の event（1 件ごとの行）で、各標本を次の標本か、それを書いた supervisor の生存の終わりまで重みにする。見込みの形は、範囲の標本と、S 以前の最後の標本（S をまたいで重みを持つ。追試で、S をまたぐのは supervisor によらず S 前の最後の 1 件と分かった）と、その標本の supervisor の生存の行。時計が戻ると「次の標本」が id の順と時刻の順で違うので、`candidates` の並べ方（`self.events` の id の順）に合わせる |

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
- (ii) の kind: 追試（Spike 4756）で、範囲の task を名指す snapshot が確定した履歴より古い forecast の問いは範囲が決まらない（does_not_hold）。ほかの (ii) の kind は追試でも unresolved で、それを読む問いの扱いは追試の「後続の draft への推奨」が持つ。

## 読みの約束

時刻から読む読み（範囲 (S, E]、`queue_before` は S 以前の queue の最後の record）は、全件の答えと同じまま、次だけを store から読む。

1. 終わっていない行と、S の後の record を持つ行（今の `rows_from`）。
2. 1 の行が名指す task と run の行（今の `rows_wanted`、key で）。
3. 開いたまま残した行の表にある run の行（key で）。builder が run 行を書くときに書き、閉じる event で消す。約束に入れるのは slot・ask・landing の待ちと、close の `opened_event_id` で対にならない run の session（追試で更新）。
4. 外した行の record のうち、範囲が名指す task・plan review・proposal と、範囲が名指す proposal の task（`task_submitted`・`task_reopened` の `proposal_id` から 2 段目）と名指す proposal の goal の task（`goal_submitted` から 3 段目）はその task の終わりだけ、範囲の task を名指す snapshot の task の `TASK_MARK_KINDS`（`run_events` の `events_by_task` と payload の式の索引）、範囲が名指す planner（actor を含む）と plan review の session 行（key の接頭辞で）。`run_events` から読むときは 6 の可視性で絞る（追試で更新。形は「追試 (i) earlier（Spike 4755）」の「読みの約束の更新」）。
5. S 以前の時刻を持つ `integration_started` のうち id が最大のものの run（試作の `last_start` と同じ順）。読みの形は `events_by_kind(kind, id)` を id の降順に歩き、時刻が S 以前の最初の 1 件で止める。歩く数は S の後の時刻を持つ `integration_started`（範囲とその後の着地、時計が進んだもの）に比例し、範囲の前の確定した履歴には比例しない。最初の Execution は builder が書く actor × provider の表から。
6. queue 行は kind ごとに、S に終わっていないか S 以降の record を持つ行、kind ごとの S 前の最後の行、`queue_before` の行。kind ごとに足すもの（supervisor・snapshot と mark・baseline・main の要約と sha・goal・run の無い ask）は「追試 (ii) queue 行（Spike 4756）」の「読みの約束の更新」が持つ。
7. `run_events` の小さな読みは、(iii) の条件の event だけを返す。条件は追試（Spike 4754）の「読みの約束の更新」の 7 が置き換える（走行中の run、W と R の後の session と queue の event、R による検査の省略を含む。U_E は追試でも unresolved で約束に入れない）。

## 未解決

(iv) に未解決の部分は無い。(i) の下の 4 項目（session 行の records、improvement_proposals の goal を通る所属、名指す proposal の task の読む量、run の session が開いたままかの判定）は「追試 (i) earlier（Spike 4755）」で holds になった（項目は 4678 の時点の記録として残す）。(iii) の 4678 で上限に達して止めた部分は、追試（Spike 4754）で走行中の run・W と R の後の event と食い違い・R による省略に判定が出て、U_E だけが追試でも未解決（追試の節の「未解決」）。(ii) の次の kind は試作の履歴に無い、または行の数が単位で増えないので 30/3000 の比べになっていない。(ii) も作り直しは 3 回に達した。下の (ii) の項目は「追試 (ii) queue 行（Spike 4756）」で判定が出た（`live_alert_stream` を除く。項目は 4678 の時点の記録として残す）。

- (i) session 行の records（追試で holds）: 試したことは、`Named::names`（task・proposal・plan review・planner の id を名指す record を持つ session 行を丸ごと残す）で答えが一致したことだけ。提案する key の接頭辞の seek（planner と plan review の id だけ）そのものと、actor の `planner:<id>` から planner を特定する planner_routes の場合は試していない。止まった理由: review の指摘で分かった抜けで、作り直しの回数は残っていた（2 回）が、試作を接頭辞の seek に替えて planner_question の ask の場合を足すと (i) の比べを全部流し直すので、revise の時間の中で他の指摘を先にした。次に要る試作: `Named` に actor の `planner:<id>` を足し、session 行を key の接頭辞だけで残す形にして、範囲の planner_question の ask と範囲の前に閉じたその planner の session（route を持つ）を置いて全件と比べる。約 120 行。
- (ii)（追試でも unresolved） `slots_full`・`claim_hold`・`landing_hold`、supervisor の生存と heartbeat（supervisor ごとの最後の heartbeat）、mark の撤回、forecast の snapshot と baseline: 確定した単位にそれぞれの行を足し（supervisor は単位ごとに別の supervisor の生存）、30/3000 で比べる。約 150 行。

- (ii)（追試でも unresolved） main の記録（`main_log`・`main_history`・`main_covers`）: 試したことは無い。`fold_main_log` が記録の全部を畳むので、範囲の形では main が S に持つ path を作れない見込み（D）。次に要る試作: `main_commits_recorded`・`main_paths_recorded` を確定した履歴に足し、S での main の状態を builder が書く要約（S 以前の最新の状態の行）から読む形と全件を比べる。約 200 行の見込み。
- (ii)（追試でも unresolved） `post_landing_book`: 次に要る試作: revert と CI の失敗を範囲の前の着地に付け、範囲の run だけの seek と全件を比べる。約 100 行。
- (ii)（追試でも unresolved） goal の属性: goal の作成・変更を範囲の前に置き、範囲の task の goal の E 以前の最新の seek と比べる。約 100 行。
- (ii)（追試でも unresolved） update・backend の失敗・run の無い ask と repair、`stall_config`: 1 件ごとの行で、S 以降の行と E 以前の最新の seek の形。開いた run の無い ask は B。約 150 行。
- (i) 名指す proposal の task の読む量（追試で holds）: 試したことは、2 段目の task の records を `Named::names` で全部返す形で答えが一致したことだけ。止まった理由: 作り直しの上限の 3 回に達した（人の答え ask 1656 で unresolved にしてよいとされた）。次に要る試作: 範囲の前に終わった proposal の task（task 66）に範囲の前の `task_priority_changed` を 30 件と 3000 件足し、その task は E 以前の最後の `task_status_changed` と後の `task_reopened` だけを読む形にして、`improvement_proposals` を含む答えが全件と同じで返る earlier が 30 と 3000 で同じことを比べる。約 60 行。
- (i) run の session が開いたままかの判定（追試で holds）: 試したことは無い（試作の `open_at_end` は件数の比べ）。止まった理由: 上限の 3 回（ask 1656）。次に要る試作: `open_at_end` の session を close の `opened_event_id` で対にし（`take_earlier` と同じ規則）、着地した古い run に open と `opened_event_id` の無い close、重複した close を置いて（`numbered` で補わない）、全件の読みと比べる。約 60 行。
- (i) improvement_proposals の goal を通る所属（追試で holds）: 試していない。止まった理由: 作り直しの上限の 3 回に達した（3 回目が task が直に名指す所属の 2 段目）。次に要る試作: goal を範囲の前に作り、その goal を proposal に `goal_submitted` で入れ、goal の task の片方を範囲の前に、もう片方を範囲で完了させ、finding で proposal を結んで、`goal_submitted` からの 3 段の seek と全件を比べる。約 80 行。
- (ii)（追試でも unresolved） `candidates_sampled`: 試していない。次に要る試作: 確定した単位ごとに supervisor の生存と `candidates_sampled` を置き、S をまたぐ標本と、範囲の前に生存が終わった supervisor の標本を足して、`kpi` の candidates の値を全件と比べ、30/3000 で読んだ行の数を比べる。約 100 行。
- (ii) `live_alert_stream`: records が開いている stream の中で伸びるので、行の数でなく records の読みの問い。goal 273（ADR-t4644-1 の写さない kind と task 4660・4646）と Spike 4663（queue 行を要る列に絞る測定）が持つので、ここでは試作しない。

## 後続の draft への推奨

goal 273 と Spike 4663 と重ねない: (C, S] の写さない kind の queue の event と `live_alert_stream` の records は goal 273（task 4660 の共通の読み取り関数、4646）に、queue 行の読みを要る列に絞ることは Spike 4663 に任せる。この Spike の約束は「どの queue 行を読むか」だけを決め、行の中の何を読むかは決めない。

- 4667（索引の migration）: 4670 の索引に、`run_events` の `follow_up_registered` の `task_id` の部分の式の索引、`plan_review_id`・`proposal_id` の式の索引、`ledger_rows(kind, subject_kind, ended_at)`、`ledger_rows` の `json_extract(inputs_through, '$.events')` の式の索引を足す。
- 新しい task（builder の 2 つの表）: 開いたまま残した行の表と、最初の Execution の表を足す migration と builder の書き込み。run の session は close の `opened_event_id` で対にして表に入れる（全件の読みの `take_earlier` と同じ規則。追試で holds）。今の行から埋める段と、無い store では全件の読みに戻す形を含める。約 400〜600 行。4122 の前提。
- 4122（store の範囲の読みと小さな読み）: `earlier_in` の task・run・session 行の部分を「読みの約束」の 3〜5 に置き換え（session 行と名指す proposal・goal の task は追試の推奨、U_E は追試の (iii-d) が unresolved なので付けない）、queue 行を 6 の形で読む。`LedgerWindowRead` に R・W（`ledger_marks`）を足し、小さな読みは (iii) の条件で絞る。SQLite の adapter の test で `EXPLAIN QUERY PLAN` が索引を使うことを確かめる。
- 4132（domain の範囲の関数）: `take_earlier` は今のまま使える（渡す record が減るだけ）。名指すものの集め方（試作の `Named`）を domain に置く。
- 4133（kpi の配線）・4134（report の配線）: 受け入れの比較に、試作の 30 と 3000 単位の履歴と `Reads` の和の数え方を使う。範囲が決まらない問いに入れる問いは「追試 (ii) queue 行（Spike 4756）」の「後続の draft への推奨」が持つ。
- 4677（run_events の小さな読み）: (c)(d) の claim と (g) の `TASK_MARK_KINDS` を (iii) の条件で返す。(f) の cursor の時刻は見える event だけで引く。(iii) の条件と U_E は追試の「後続の draft への推奨」が持つ。
- supervisor の report の経路の draft: `SupervisorQueue` を通る読みも R・W を同じ snapshot で読む（task 4660 の `ledger_rows_covered` と同じく 1 回の要求で返す）。
- 新しい Spike か試作の task（(ii) の unresolved の kind は追試（Spike 4756）が判定を出し、推奨はその節の「後続の draft への推奨」が持つ。(i) の 4 つは追試（Spike 4755）で holds になり、(iii) は追試（Spike 4754）が済ませて U_E だけが追試の「未解決」に残る）: 上の「未解決」の試作。(ii) は追試でも unresolved の部分が残り、範囲が決まらない問いに入れる問いはその節の推奨が持つ。

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

7. `run_events` の小さな読みは、event を持ち主ごとに判じて返す。run の event は run 行の `inputs_through` が id 以上か、run 行が無く、終わっていない task 行のどれかの `detail.runs` が run を名指すとき（event の `task_id` によらない）。task の event は id が run・task・node の R 以下か、task 行の `inputs_through` が id 以上のとき。session の event は id が W 以下か W の後に書かれた session 行の records にあるとき、queue の event は id が queue の R 以下か R の後に書かれた queue 行の records にあるとき。W と 2 つの R は窓の行と同じ snapshot で別々に読み、印の無い store では session と queue の event を全件の読みで判じる。行に記録されていない run に属する入力は補わない（ask 1607）。cursor の event の時刻は見える event で引く。台帳の行の上限 U_E は (iii-d) の判定まで約束に入れず、窓の終わりより後の行も今のとおり読む（上限を付けない）。終わっていない task 行は部分索引 `ledger_rows_open_tasks`（`ledger_rows(kind) WHERE kind = 'task' AND ended_at IS NULL`）で引き、確定した task 行を走査しない（`ledger_rows_by_subject_end` は `subject_kind` が先に来て、task 行の `subject_kind` は空なので `ended_at` を引けない）。W と 2 つの R は、legacy でない run・task・session・queue の行に今と違う ledger_v の行が 1 行も無いときだけ使う。その有無は部分索引 `ledger_rows_by_version`（`ledger_rows(ledger_v) WHERE legacy = FALSE AND kind IN ('run', 'task', 'session', 'queue')`）を今の ledger_v の前と後の 2 回引いて確かめ、今の ledger_v の行は読まない。違う ledger_v の係は印が自分の版でないので cursor 0 から畳み直し、それまでに畳んだ records で各 kind の行を書き換えるので、前の版でも後の版でも、その行は W や R 以下の event を欠きうる。そのような行がある間は判定は判じられず、全件の読みに戻す。これは台帳を作る係の heads（後の ledger_v の session 行だけを見る）より厳しい。legacy の行は版を持ったまま残り（ADR-t1662-2 決定 8）、書いたときの records を持つので印を無効にしない。一つの版が追いついた store には違う版の行が無い。

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

比べなかった場合: SQLite の adapter の SQL（下の SQL は試作の in-memory の条件と同じ式だが流していない）。範囲の前の revert が範囲の run の commit を target にする場合（試作は sha でだけ引く）。snapshot と S の間の派生の mark（supervisor の start など。単位の後の snapshot の場合はその間に無い）。要約の `main_observed` の `reverts` は fixture に合わせて true に固定した。`goal_review` など planner と plan review 以外の session 行（試作の履歴に無い）。`--compare`・`--goal`・report の経路。

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

## 追試 (ii) queue 行（Spike 4756）

上の「未解決」の (ii) の部分（live_alert_stream を除く）を、上の試作を広げて全件の読みと比べ直した。判定は **unresolved**（(ii-a) の「古い snapshot と範囲の間の mark」は **does_not_hold**）。どの部分も 30・300 単位の答えと 30・3000 単位の行の数では全件と一致したが、最終の形で 3000 単位の答えを比べる前に調査の上限（3 時間）に達し、baseline・main の読む量・post_landing の反例・goal の最新の seek・finding の proposal の結びは要る形で比べていない（下の「判定」と「未解決」）。(i) は「追試 (i) earlier（Spike 4755）」、(iii) は「追試 (iii) 可視性（Spike 4754）」が持つので、ここでは扱わない。`live_alert_stream` は goal 273 と Spike 4663 が持つので試作していない。

### 環境

- 対象の commit: `deb4d35a408b08a27f93740949c1c1555b58183d`（この run の base。(i)・(iii) の追試の試作と文書を含む）。
- host: Apple M1、macOS 27.2.1（Darwin、arm64）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`cargo-nextest`。
- provider と経路: 本番 queue の Spike の run（task 4756）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。試作は上と同じ in-memory の `#[cfg(test)]` の test で、SQLite の adapter は使っていない。本番の queue と DB は読んでいない（合成の event だけ）。
- 使った時間と回数: 上限 3 時間に対して約 3 時間（17:30〜20:30 UTC。review の指摘で S をまたぐ場合の時刻を直して流し直した約 25 分を含む）に、2 回目の review の手直しの約 30 分（文書と receipt だけ。上限に達しているので試作は広げず、判定を unresolved に揃えた）。内訳は（コードの読みの約 30 分と fixture・比べ方の土台の約 25 分は部分に共通、debug build の test の待ちを含む）。部分ごとの時間と試作の作り直し（形を変えて全件と比べ直すこと、上限は部分ごとに 3 回）は、(ii-a) 約 35 分・1 回（1 回目の形は S 前に最後に始まった supervisor だけを読み、main の cover の判定が全件と食い違った。2 回目に範囲の records が名指す supervisor と、範囲の task を名指す snapshot を足して一致）、(ii-f) 約 5 分・0 回、(ii-b) 約 20 分・0 回、(ii-e) 約 10 分・0 回、(ii-d) 約 5 分・0 回、(ii-c) 約 10 分・0 回（1 回目の fixture に単位ごとの CI の失敗と revert が無かったので足した。読みの形は変えていないので作り直しに数えない）。文書と review は約 35 分。
- 差分の大きさ: 試作 約 1,220 行、文書 約 150 行。試作の途中（main の要約と goal の比べを足した所）で目安の約 1,200 行に達したので、それ以降は試作を広げず（累計の mark の表などを試さず）、test の時間を抑える形の直しだけをした。
- debug build の時間: 単位ごとに claim の待ち（`slots_full`・`claim_hold`）と task を持つ 3000 単位の履歴は、台帳を作る係の fold が待ちと開いた task の組に比例し（1000 単位で約 22 秒、task を外すと約 1.6 秒）、全件の `kpi` は supervisor の生存を始まりごとに全 event から引く（3000 単位の生存で 1 部分に約 6 分）。そのため test は、答えを 30 単位（単位の task と run を含む）と既定 300 単位（単位の task と run を外した `queue_units_only`）で比べ、行の数を 30 単位と 3000 単位（同じく task を外す）で比べる。答えを 3000 単位で比べた実行は `DAGQ_SPIKE_4756_LARGE=3000` で 1 回流した（下の「打ったコマンド」）。

### 読んだコードと打ったコマンド

読んだコード（上の節に足したもの）:

- `src/domain/ledger/queue.rs` の module doc と `of_kind`（待ちは区間の行、ほかの kind は 1 件ごとの行）、`src/domain/ledger/owner.rs::queue_wide`。
- `src/domain/kpi/window.rs`: `supervisor_lives`（生存は次の start・自分の stop・古い heartbeat・最後の event で終わる）、`slots_held`、`candidates`（標本は次の標本かその supervisor の生存の終わりまで）、`forecast_kpis`。`src/domain/supervisor_life.rs`（`Life`・`heartbeats`。次の start で終わらない）。
- `src/domain/stats/main_log.rs`（`fold_main_log`・`covers`・`supervisor_alive_at`・`history_for`）、`src/domain/stats/conflicts.rs`（窓の commit と今の path）、`src/domain/areas.rs::landed_commits`、`src/domain/ledger/post_landing.rs::fold`（revert の記録と CI の記録の始まりと届き、`recorded` の集合）。
- `src/domain/forecast/score.rs`（snapshot と終わりの間の mark を数える。撤回された mark を除く）、`src/domain/marks.rs`（`mark_recorded`・`mark_retracted`、supervisor の start などの派生の mark）。
- `src/domain/ledger/attributes.rs`（goal の `goal_created`・`goal_updated` は event の `goal_id`、baseline の印）、`src/domain/stall.rs::StallConfig::loaded`（最後の 1 件）、`src/application/stats.rs::of_window`・`src/application/kpi.rs::with_input`。

打ったコマンド:

```sh
git log -1 --format=%H
rustc --version; cargo --version; uname -sm
sh scripts/cargo-brief.sh cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::(the_waits|a_snapshot_after|the_candidates|main_s_record|what_befell|the_goals_of|the_updates|every_queue_part)/)' --no-capture
DAGQ_SPIKE_4756_LARGE=3000 sh scripts/cargo-brief.sh --log $TMPDIR/large3000.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::(the_waits|a_snapshot_after|the_candidates|main_s_record|what_befell|the_goals_of|the_updates)/)' --no-capture
sh scripts/cargo-brief.sh cargo fmt --all --check
sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings
sh scripts/check-local-gates.sh --base deb4d35a408b08a27f93740949c1c1555b58183d
```

### 試作

同じ `src/application/ports/observation/earlier_visibility_spike.rs` に足した。どれも未配線の `#[cfg(test)]` で、runtime の読みは変えていない。

- 履歴（`history_ii(settled)`）: 上の `history` の確定した単位のそれぞれに、その単位だけの supervisor（`u<k>`）の start・alive・stop と、その supervisor の `slots_full`・`claim_hold`・`landing_hold` の区間、`candidates_sampled`、撤回される `mark_recorded`、単位の task の snapshot、単位の goal の作成と変更、`update_failed`・`backend_call_failed`・run の無い ask と答え・`auto_repaired`・`stall_config_loaded`、main の commit（file を足し、2 単位ごとに前の file を消して前の commit を revert し、10 単位ごとに名を変える）と `main_observed`・`ci_checked`・`ci_turned_red` を足す（`add_queue_units`）。範囲をまたぐ場合（`add_queue_cases`）は、単位の後に始まり範囲の中まで生きる supervisor `s3` と S の前の標本、S をまたぐ待ちと開いたままの claim の hold、範囲で撤回される mark と撤回されない mark、範囲で答える run の無い ask と答えの無い ask、範囲の前に作って変えた goal 700 と範囲で作って着地する task 67、範囲の前に着地した run 60 の commit `c60` と範囲の run 65 の `r65`、範囲で同じ commit をもう一度記録し `c60` を revert し、`r65` と `c60` で CI が赤くなる場合。
- S は kpi の週の問い（`--period week --last 2`、比べの週を含めて 6 週）の読みの始まりで、day 161 の月曜（約 161.3 日目）。S をまたぐ場合は day 155〜160 に置いた（review の指摘で、1 回目の fixture の待ち・mark・ask が S の後にあったのを直した。読みの形は変えていない）。
- 狭めた読み（`queue_ii(rows, S, kinds, QueueShape)`）: 4678 の `queue_narrowed`（S に終わっていないか S 以降の record を持つ行、kind ごとの S 前の最後の行、`queue_before` の行）に、部分ごとの読みを足す。main の記録は `main_summary` が S 前の行を builder の要約の 1 行（S での path・記録の始まりと届き・Git の読みの状態・CI の記録の最初と最後の check）に置き換え、範囲が名指す sha の commit の行を足す。
- 答えは kpi の 4 問と `--by area`（`[areas]` つき）、`stats --since`（2 日前）`--full`（`[areas]` つき）の JSON 全体と、日の問いの窓の `main_covers` と run 60・65 の `post_landing_book` を全件の読みと比べる（`queue_misses`）。読む量は 4678 の `Reads` の queue 行のうち部分の kind の行（main は要約の行を含む）を数える。

| test | 部分 | 比べたもの |
| --- | --- | --- |
| `the_waits_lives_marks_and_snapshots_read_by_their_range_answer_as_every_row` | (ii-a)、snapshot は day 3（単位より前） | 4678 の形は 2 件食い違う（`main_covers` と `conflict_hotspots` の理由が `SupervisorStopped` と `Stale`、snapshot の比べの層）。狭めた形の食い違いは `forecast.marks_between` だけ（30 単位で 6 と 66、300 単位で 6 と 606。食い違いの記録は 64 件で打ち切る）で、読む行は 159 と 6,099（30/3000、全行は 526 と 27,256） |
| `a_snapshot_after_the_settled_history_reads_its_marks_by_the_range` | (ii-a)、同じ snapshot を単位の後（day 158）に置き、snapshot と S の間に撤回されない mark を置く | 30 と 300 単位で答えが一致。読む行 96 と 96（全行 527 と 27,257） |
| `the_candidates_sampled_read_by_their_range_answer_as_every_row` | (ii-f) | 4678 の形も一致。読む行 3 と 3（全行 32 と 3,002） |
| `main_s_record_read_from_a_summary_answers_as_every_row` | (ii-b) | 4678 の形は run 60 の CI の記録が `before_record`（全件は `recorded`）。要約の形は一致し、読む行 5 と 5（全行 99 と 9,009） |
| `what_befell_a_landing_read_by_the_range_answers_as_every_row` | (ii-c) | 一致。`ci_turned_red` の読む行 4 と 4（全行 32 と 3,002）。revert は main の記録の行で (ii-b) の読みに入る |
| `the_goals_of_the_range_s_tasks_read_by_the_goal_answer_as_every_row` | (ii-d) | 4678 の形は goal 700 の属性（report が読む `goal_summaries`）を落とす（kpi・stats の答えは一致）。goal の seek の形は一致し、読む行 5 と 5（全行 62 と 6,002） |
| `the_updates_failures_and_asks_of_no_run_read_by_their_range_answer_as_every_row` | (ii-e) | 4678 の形は S をまたぐ run の無い ask を 1 つ落とし `ask_wait` が食い違う（`n` 1 と 2）。ask の seek を足した形は一致し、読む行 11 と 11（全行 186 と 18,006） |
| `every_queue_part_of_ii_read_by_its_range_together_answers_as_every_row` | 全部の queue の kind（4678 の kind を含む）を同時に狭める（snapshot は day 158） | 30 と 300 単位で答えが一致。queue 行は 136（30 単位、task を外すと 136）、136（300）、136（3000）で、全行は 1,168・1,138・8,698・84,298 |

`DAGQ_SPIKE_4756_LARGE=3000` の実行（goal の比べと S をまたぐ場合の直しを足す前の木。goal 700 の属性と S をまたぐ待ち・mark・ask はこの実行では比べていない）では、答えの比べを 3000 単位で流した 5 つ（`a_snapshot_after_…`・`main_s_record_…`・`the_candidates_…`・`the_goals_of_…`・`the_updates_…`）が 30 単位と同じ行の数で全件と一致した（1 本 4〜6.5 分）。`the_waits_…`（day 3 の snapshot）と `what_befell_…` は background の 30 分の上限で止まり、3000 単位の答えは出ていない（300 単位では上の表のとおり）。

比べなかった場合: SQLite の adapter の SQL（下の SQL は試作の in-memory の条件と同じ式だが流していない）。`main_rewritten`・`main_reverts_imported` の `unread`（試作の履歴に無い）。複数の branch の CI の記録。`--compare`・`--goal`・report の経路（report は同じ `LedgerWindow` の関数を読むが流していない）。

### 判定

全体で **unresolved**。4678 の「判定」の (ii) の表の該当の行はこの判定に従う。表の行のうち unresolved だったものは「追試 (ii) の残り（Spike 4811）」の「判定」が判定を出した（各行の末尾の 1 文。この表の判定は 4756 の時点の記録として残す）。test の名前は上の表のもの。答えの比べ（`queue_misses`）は kpi の 4 問と `--by area` の JSON 全体と `stats --since --full` の JSON 全体なので、表の「読む fold」に書いた読み手の出力（stats と kpi の欄の名前）はどれもその比べに入る。答えは全件の読みと JSON 全体で 30 と 300 単位（最終の木）で比べ、読む量は部分の kind の queue 行を 30 と 3000 単位で数えた。受け入れの求める 3000 単位の答えの比べは最終の木で流しておらず（上の 3000 単位の実行は直す前の木で、2 本は済んでいない）、上限に達したので、どの部分も holds とせず unresolved とし、30・300 単位で見えたことを「見込み」として書く。

| 部分・kind | 読む fold | 判定 | 見込みの読みの形 | 30・300 単位で見えたこと | 足りない根拠 |
| --- | --- | --- | --- | --- | --- |
| (ii-a) `slots_full`・`claim_hold`・`landing_hold` | `pre_claim` の区間、`slots_held` | unresolved（見込み holds） | 4678 の約束 6 のまま | S をまたぐ区間（day 158・159 に開始）で答えが一致、各 2 / 2 | 3000 単位の答え 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| (ii-a) supervisor の生存と heartbeat | `kpi::supervisor_lives`・`supervisor_life::Lives`・`main_log::supervisor_alive_at` | unresolved（見込み holds） | S 前に最後に始まった supervisor と範囲の records が名指す supervisor の `supervisor_started` と kind ごとの S 前の最後の行（`json_extract(payload, '$.supervisor')` の部分の式の索引） | 4678 の形は `main_covers` が `SupervisorStopped`（全件は `Stale`）。足した形は一致、started 2 / 2・alive 80 / 80・stopped 1 / 1 | 3000 単位の答え 追試の残り（Spike 4811）で holds（3000 単位の答え。単位ごとの supervisor の生存でも 3000 単位で比べた）。 |
| (ii-a) mark の撤回 | `forecast::score`・`marks` | unresolved（見込み holds） | 読んだ mark を名指す `mark_retracted`（`json_extract(payload, '$.mark')`）と範囲の撤回 | S 前の mark が範囲で撤回・S 前で撤回・撤回されない場合で一致、2 / 2 | 3000 単位の答え 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| (ii-a) forecast の snapshot | `forecast::score` | unresolved（見込み holds） | 範囲の task を名指す snapshot を builder の target の表で引く | 4678 の形は比べの層が食い違う。足した形は一致、1 / 1 | 3000 単位の答え 追試の残り（Spike 4811）で holds（snapshot が day 158 のとき、3000 単位の答え）。 |
| (ii-a) snapshot と範囲の間の mark | `forecast::score` の `marks` | **does_not_hold**（snapshot が確定した履歴より古いとき） | 最も古い名指す snapshot から S までの mark を読むほかない | 読む行 159 → 6,099、派生の mark を読まないと `marks_between` が 6（全件は 66、300 単位で 606） | — |
| (ii-a) baseline の印 | `attributes::fold_within`・`findings::fold_within` | unresolved | 試作は印の行を全部読む | fixture の印は testkit の各 1 件だけで単位ごとに増えない。docs/design/attribute-records.md の「基準と照合」は繰り返す照合と組の印を認めるので、組を単位ごとに増やすと試作の読みは増える | 組を単位ごとに増やした履歴での比べ 追試の残り（Spike 4811）で holds（照合の組を単位ごとに増やし、S 前の最新の印と名指す goal の基準だけを読む形）。 |
| (ii-f) `candidates_sampled` | `kpi::Context::candidates` | unresolved（見込み holds） | 4678 の約束 6 のままと supervisor の形 | 一致、3 / 3 | 3000 単位の答え（直す前の木では一致） 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| (ii-b) main の記録 | `fold_main_log` | unresolved | builder の要約と sha の表（試作の `main_summary`） | 答えは一致。ただし 5 / 5 は 4678 の `Reads` の単位でない: 要約の行を 2 回数え、sha で引いた `main_commits_recorded` と CI の check は要約の行の records に詰めて数えていない。`main_paths_recorded` は単位の外の 1 件だけ | builder が S ごとに保存する要約と、問いごとの sha の検索で返す行・event を分けて数えた比べ 追試の残り（Spike 4811）で holds（保存する要約の行 5 と sha の検索の event 3 を別々に数え、30・3000 単位で同じ）。 |
| (ii-c) `post_landing_book` | `post_landing::fold` | unresolved | (ii-b) の要約と範囲の `ci_turned_red` | fixture の場合では一致したが、反例を通っていない: 範囲に届く run 60 の `c60` で S 前に CI が赤くなり後で別の sha で赤くなると、`queue_narrowed` は後者だけを残し要約も失敗を補わないので `count`・`first_at` が違う。古い revert は seek が `commit.sha` しか照らさず target を照らさないので落ちる。CI の check は branch ごとでなく全体の最初と最後だけを残す | 上の反例を置いた比べと、範囲の run の S 前の失敗と revert の seek 追試の残り（Spike 4811）で holds（3 つの反例を置き、着地の commit で失敗と revert の target を引く seek と branch ごとの CI の要約で一致）。 |
| (ii-d) goal の属性 | `attributes::fold_within` の goal | unresolved | 範囲の task が名指す goal の E 以前の最新の seek | 4678 の形は report の `goal_summaries` で goal 700 を落とす。試作は名指す goal の行を全部読み、goal 700 の履歴は 2 件に固定 | goal 700 自身の確定した変更を単位ごとに増やし、最新だけを読む形の比べ 追試の残り（Spike 4811）で holds（goal 700 の `goal_updated` を単位ごとに増やし、最新の置き換える record からだけ読む形で report の `goal_summaries` まで一致）。 |
| (ii-e) `update_failed` | `stats::updates::updates`（`UPDATE_EVENT_KINDS` の id が窓の中の event だけ）。stats の `updates` | unresolved（見込み holds） | 4678 の約束 6 のまま（範囲の行。読み手が窓の外の event を読まない） | `the_updates_failures_and_asks_of_no_run_read_by_their_range_answer_as_every_row` と `every_queue_part_of_ii_read_by_its_range_together_answers_as_every_row` が stats の JSON の `updates` を比べ一致。fixture の `update_failed` は e2e の段の `failed_tests` を持つ形だけ | 3000 単位の答え。`update_started` から `update_installed` までの組のように範囲の前に始まる段は fixture に無い 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| (ii-e) `backend_call_failed` | `stats::backend_failures`（`src/domain/stats.rs`、id が窓の中の event だけ）。stats の `backend_failures` と、kpi がその数から出す `backend_failures_per_run` | unresolved（見込み holds） | 4678 の約束 6 のまま | 同じ 2 つの test が stats の `backend_failures` と kpi の `backend_failures_per_run` を比べ一致 | 3000 単位の答え 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| (ii-e) `auto_repaired`（run の無いもの） | `stats::auto_repairs::auto_repairs`（id が窓の中の `auto_repaired` と `ask_opened` を日ごとに数える）。stats の `auto_repairs` | unresolved（見込み holds） | 4678 の約束 6 のまま | 同じ 2 つの test が stats の `auto_repairs`（日ごとの `asks_opened` を含む）を比べ一致。fixture の payload に `repair`・`layer` が無く、どれも `unknown` に数えた | 3000 単位の答え。`repair`・`layer` を持つ payload 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| (ii-e) `stall_config_loaded` | `StallConfig::loaded`（窓の event のうち最後の 1 件）を `application::stats::stall_config` が読む。stats の `stall_config` | unresolved（見込み holds） | E 以前の最後の 1 件（`events_by_kind` を id の降順に 1 件）。4678 の約束 6 の kind ごとの S 前の最後の行と範囲の行で同じになる | 同じ 2 つの test が stats の `stall_config` を比べ一致（範囲に無く、S 前の最後の day 152 の 1 件が読まれる場合） | 3000 単位の答え。範囲の中に `stall_config_loaded` がある場合 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| (ii-e) run の無い ask と答え（`ask_opened`・`ask_answered`） | `stats::asks::asks`（`tracks` は E までの全 event で ask を対にする）と `stats::asks::human_waits`、`stats::ask_seen::seen_waits`。stats の `asks`、kpi の `ask_wait`・`ask_apply_wait`・`ask_seen_wait`、kpi の `asks_per_landing`（窓の中の `ask_opened` を数える） | unresolved（見込み holds） | 範囲の答えが名指す ask の `ask_opened`（`json_extract(payload, '$.ask_id')` の部分の式の索引）と、開いたまま残した行の表の答えの無い ask | 同じ 2 つの test。4678 の形は S をまたぐ ask を落として kpi の `ask_wait` が食い違う（`n` 1 と 2）。足した形は stats の `asks` と kpi の 4 つが一致、(ii-e) の kind の行は 11 / 11 | 3000 単位の答え。答えを適用する event（`ask_apply_wait`）が run の無い ask に付く場合は fixture に無い 追試の残り（Spike 4811）で holds（3000 単位の答え）。 |
| finding が結ぶ proposal（追試 (i) の「未解決」の留保） | `kpi::improvement_proposals` | unresolved | S 前に proposed から settled になった finding の結びを読む形 | この追試の fixture に無く、`queue_narrowed` の finding の読みも変えていない | 追試 (i) の「未解決」の試作（約 50 行） 追試の残り（Spike 4811）で holds（範囲が名指す proposal を `proposed` で結ぶ finding を引く形）。 |

- 全部を同時に狭めた読み（`every_queue_part_of_ii_read_by_its_range_together_answers_as_every_row`）は 30 と 300 単位で答えが一致し、queue 行の和は 30・300・3000 単位で 136。ただし main の数は上の (ii-b) と同じく 4678 の単位でない。
- ADR-t1662-2 決定 1・7・8 との関係（見込み）: 足す表（snapshot の target、main の commit の sha、main の要約、開いたまま残した行の表の queue の ask）は event だけから作り直せる台帳の一部（ビュー）で、行の形と `ledger_v` を変えない。今ある台帳には無いので、埋めるまで全件の読みに戻す。
- does_not_hold の部分の意味: kpi の既定の問いの `forecast`（`details.forecast` の `marks_between` と `forecast.*` の層）は、範囲の task を名指す snapshot が確定した履歴より古いと、その間の mark を読まずに答えが出ない。4678 の (iv) の「snapshot の古い問いは範囲が決まらない」を mark について裏づけた。

### 読みの約束の更新

- 約束 6 は 4678 のまま（(ii) で unresolved の kind は全ページで読む）。この追試の判定がどれも holds でないので約束に入れない。見込みの形（上の「判定」の表）は、次の試作で holds になったものから約束 6 に足す。
- 約束 6 の finding の読み（S に開いているか範囲の record を持つ行）は、追試 (i) の「未解決」のとおり、S 前に proposed から settled になった finding が結ぶ proposal を落としうる。その判定まで `improvement_proposals` を読む問いは finding の行を全部読む留保を残す。
- 約束 1〜5・7 は変えない。

### 未解決

調査の上限（3 時間）と差分の目安（約 1,200 行）に達したので、次の部分を unresolved として止めた。作り直しの回数は (ii-a) 1 回、ほかは 0 回で上限の 3 回には達していない。

- 3000 単位の答え（(ii-a) の待ち・supervisor・撤回・snapshot、(ii-f)、(ii-e)）: 試したことは、30 と 300 単位（task を外した履歴）の答えと 30・3000 単位の行の数の比べ。止まった所: 3000 単位の答えは直す前の木で 5 本だけ済み、最終の木（S をまたぐ場合と goal の比べを足したもの）では流していない。理由: debug build で全件の `kpi` が 3000 の supervisor の生存を始まりごとに全 event から引き、1 本約 4〜6.5 分、待ちの fold も単位の task に比例して遅く、2 本は 30 分で済まなかった。次に要るもの: release build か、fixture の supervisor を単位ごとに作らず生存の行だけを増やす形で、最終の木の 3000 単位の答えを流すこと（試作の変更は約 30 行、流す時間は release で数分の見込み）。
- baseline の印: 試したことは、testkit の各 1 件の印で全部の印を読む形。止まった所: 照合の組を単位ごとに増やす履歴を作っていない。次に要る試作: 単位ごとに照合の組（task の baseline と queue の印）を足し、S 以前の最新の組の印だけを読む形と全件を比べる。約 80 行。
- (ii-b) main の読む量: 試したことは、要約と sha の検索を 1 つの行に詰めた形で答えが一致したこと。止まった所: 返す量を 4678 の `Reads` の単位で数えていない（要約の重複、sha の検索の event を数えない）。単位ごとの `main_paths_recorded` も無い。次に要る試作: builder が保存する要約の行と、問いごとの sha の検索が返す event を別々に数え、単位ごとに paths の記録も足して 30・3000 単位で比べる。約 80 行。
- (ii-c) post_landing: 止まった所: 範囲に届く run の S 前の CI の失敗、範囲の run の commit を target にする S 前の revert、複数の branch の CI の記録を fixture に置いていない。次に要る試作: その 3 つを置き、範囲の run の着地の commit で失敗と revert の target を引く seek と branch ごとの記録の要約と全件を比べる。約 100 行。
- (ii-d) goal の最新の seek: 止まった所: 試作は名指す goal の行を全部読み、goal 700 の変更は 2 件に固定。次に要る試作: goal 700 の確定した `goal_updated` を単位ごとに増やし、E 以前の最新の `goal_created` か `goal_updated` とその後の状態の変化だけを読む形と比べる。約 60 行。
- finding が結ぶ proposal: 追試 (i) の「未解決」の試作（約 50 行）がこの追試にも入っていない。
- does_not_hold の (ii-a) mark の代わり: mark の数を builder が累計の表に書けば行を読まずに `marks_between` を数えられる見込みだが、`forecast::score` が mark の event を受け取る形を変えることになり試していない。約 150 行。

### 後続の draft への推奨

goal 273（task 4660・4646）と Spike 4663 と重ねない: `live_alert_stream` の records と (C, S] の写さない kind は goal 273 に、queue 行を要る列に絞ることは Spike 4663 に任せる。この追試は「どの queue 行を読むか」だけを扱う。

判定が unresolved なので、(ii) の kind は判定が出るまで今の全ページの読みのまま（4678 の約束 6）。同じ部分の Spike を重ねるより、planner が次のどちらかを決める: (a) (ii) の kind を読む問いを「範囲が決まらない」に入れる小さい範囲、(b) 上の「未解決」の試作（合わせて約 400 行と 3000 単位の実行）を実装の task の受け入れの比較に入れて、その中で確かめる。

- 「範囲が決まらない」に入れる kpi・report の問い（(a) のとき）: (ii) の kind を読む問いの全部。kpi の既定の問い（`slot_usage` と `slot_check` の supervisor の生存、`candidates`、`forecast`、`ask_wait` の run の無い ask、`improvement_proposals` の finding、post_landing の `details.post_landing`）、`--by area`、stats の `conflict_hotspots`・areas・`main_covers`・updates・backend の失敗、report の `goal_summaries` と observer の kpi。forecast は snapshot が古いとき does_not_hold なので (b) でも残る。これだと goal 271 の受け入れ 1 の範囲の読みが kpi・report の既定の問いに残らないので、受け入れを弱めずに人へ scope の planner_question を上げる。
- builder の表の新しい task: (b) のとき、snapshot の target の表、main の commit の sha の表（revert の target と CI の失敗の sha を列に）、main の要約の行（S 以前の最新と branch ごとの CI の記録）、開いたまま残した行の表の queue の ask を、4678 と (i) の追試の 2 つの表に足す見込み。約 300 行の追加（4678 の約 400〜600 行と合わせて）。判定までは draft のまま。
- 4667（索引の migration）: (b) のとき `json_extract(payload, '$.supervisor')`・`'$.mark'`（`mark_retracted`）・`'$.ask_id'`（`ask_opened`）の部分の式の索引。約 30 行。(a) なら足さない。
- 4122（store の範囲の読み）: (a) なら queue 行は今の全ページのまま（変更なし）。(b) なら約束 6 に判定の出た kind の形を足し、adapter の test で `EXPLAIN QUERY PLAN` を確かめる。約 200 行。
- 4132（domain の範囲の関数）: (b) のとき名指すものの集め方（試作の `queue_ii` と `main_summary`）に supervisor・範囲の task と goal・sha・ask を足す。約 150 行。(a) なら追加なし。
- 4133（kpi の配線）: (a) なら範囲が決まらない問いの判定（上の kpi の問いを全件の読みに戻す分岐）と、範囲で読む残りの問いの 30・3000 単位の受け入れの test。約 120 行。(b) なら上の「未解決」の場合を受け入れの比較に足し、forecast の snapshot が古いときだけ全件に戻す分岐。約 250 行。
- 4134（report の配線）: (a) なら report の作成・observer の kpi・push の判定・report_inputs のうち (ii) の kind を読むものを全件の読みに残す分岐と test。約 100 行。(b) なら `goal_summaries` の goal の seek と post_landing の反例を受け入れの比較に足す。約 200 行。

## 追試 (ii) の残り（Spike 4811）

上の「追試 (ii) queue 行（Spike 4756）」が上限で unresolved に残した部分を、同じ試作を広げて全件の読みと比べ直した。部分は (A) 最終の形の 3000 単位の答え、(B) baseline の印、(C) main の記録の読む量、(D) `post_landing_book` の反例、(E) goal の最新の seek、(F) finding が結ぶ proposal。判定はどの部分も **holds**（下の「判定」）。(ii) の kind で範囲の読みに比例する履歴が残るのは、4756 の does_not_hold（snapshot が確定した履歴より古いときの、snapshot と範囲の間の mark）だけになる。`live_alert_stream` と (C, S] の写さない kind は goal 273、queue 行を要る列に絞ることは Spike 4663、待ちの fold の O(waits × open tasks) は task 4805 が持つので、ここでは扱わない。

### 環境

- 対象の commit: `bfd6eb9f6a1c1d623eac8e39265e4f63063590d9`（この run の base）。Spike 4756 の着地（`f915a5421`）から base までに、試作（`earlier_visibility_spike.rs`）と読む fold（`src/domain/ledger/queue.rs`・`post_landing.rs`・`attributes.rs`、`src/domain/stats/main_log.rs`、`src/domain/kpi/window.rs`、`src/domain/forecast/score.rs`）を変える commit は無い（`git log f915a5421..HEAD -- <それらの path>` が空。`a44d684bb` の `score.rs` の変更は 4756 の着地より前）。
- host: Apple M1、macOS 27.2（Darwin 27.2.0、arm64）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`cargo-nextest`。debug build（`target/debug`、release build と別の target は使っていない）。
- provider と経路: 本番 queue の Spike の run（task 4811）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。試作は in-memory の `#[cfg(test)]` の test で、SQLite の adapter は使っていない。本番の queue と DB は読んでいない（合成の event だけ）。
- 使った時間と回数: 上限 3 時間に対して約 2 時間 30 分（2026-11-11 22:19〜2026-11-12 00:50 UTC 頃。debug build の待ちと 3000 単位の実行の約 1 時間 20 分を含み、部分の時間と重なる）。部分ごとの時間（コードの読みと試作の土台の約 30 分は共通）と作り直し（形を変えて全件と比べ直すこと、上限は部分ごとに 3 回）は次のとおり。
  - (A): 約 15 分、0 回（数え方の直し 1 回は、30 単位の数に 4756 の `part_report` が main の要約の行を足していたのをそろえただけで形は変えていない）。
  - (B): 約 25 分、1 回（review の指摘で、task の基準と queue の印の組を足した。1 回目の fixture は goal と finding の基準だけで、task の基準を (i) の読みに任せて比べていなかった。読みの形は変えていない）。
  - (C): 約 15 分、2 回（1 回目は sha を名指す record に、全部読む kind の行の範囲の前の record まで入れ、sha の検索の event が 32 / 3002 と単位で増えた。2 回目に範囲の record だけが sha を名指す形にして 3 / 3。review の指摘で、着地の commit も範囲に届く run 行のものだけにした（試作の単位の run は着地の commit を持たなかったので数が出なかった。単位の run に commit を持たせ、単位の task と run を持つ 30 単位の履歴でも読む量が同じことを比べた））。
  - (D): 約 10 分、0 回。
  - (E): 約 10 分、0 回。
  - (F): 約 20 分、1 回（review の指摘で、goal の task だけで走る proposal を足した。1 回目の形は範囲が直に名指す proposal だけを読み、goal を通る proposal を結ぶ S 前の finding を落としうる。2 回目に範囲が名指す goal の `goal_submitted` が名指す proposal も足して一致）。
  - review の指摘の直し（(C)(D) の着地の commit を範囲の run だけから取る、(B) の組の印を読む行に足す、単位の run に着地の commit を持たせる）: 約 20 分。形の直しは (C) の 2 回目に数える。
  - 文書と review: 約 30 分。
- 差分の大きさ: 試作 約 950 行（新しい `earlier_visibility_spike/rest.rs`。目安の約 400〜500 行を超えたのは、rustfmt の縦の形の fixture と 6 部分の比べの土台のため。目安に達した後は部分を広げず、最後の 3000 単位の実行の直しだけをした）、文書 約 150 行。

### 読んだコードと打ったコマンド

読んだコード（上の節に足したもの）:

- `src/domain/ledger/baseline.rs::taken`（基準の record は組を閉じる印が窓にあるときだけ取る）、`src/domain/ledger/attributes.rs::fold_within`・`apply_goal`（`goal_created`・`goal_updated` の `new`・`attributes_baseline` は goal を丸ごと置き換え、`goal_status_changed`・`goal_closed`・`goal_reopened` はその上に足す）、`goal_summaries`。`src/domain/ledger/findings.rs::fold`。docs/design/attribute-records.md の「基準と照合」。
- `src/domain/ledger/post_landing.rs::fold`・`of_run`（revert は commit の sha ごとに 1 回、失敗は event ごと、CI の記録は branch ごとの始まりと届き、失敗は run の push の branch のものだけ）。`src/domain/stats/main_log.rs::fold_main_log`・`covers`・`supervisor_alive_at`（始まった supervisor ごとに全 event から生存の終わりを引く）。
- `src/domain/kpi/window.rs::improvement_proposals`（`proposed` の結びは finding が `open` に戻るまで残る）と `src/domain/finding.rs::improvement_running`。`src/application/report.rs::diagram_draft`。

打ったコマンド:

```sh
git log -1 --format=%H
git log --oneline f915a5421..HEAD -- src/application/ports/observation/earlier_visibility_spike.rs src/domain/ledger/queue.rs src/domain/ledger/post_landing.rs src/domain/ledger/attributes.rs src/domain/stats/main_log.rs src/domain/kpi/window.rs src/domain/forecast/score.rs
rustc -V; cargo -V; uname -sm
sh scripts/cargo-brief.sh --log $TMPDIR/rest2.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::/)' --no-capture
DAGQ_SPIKE_4811_LARGE=3000 sh scripts/cargo-brief.sh --log $TMPDIR/large3000.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::/)' --no-capture
DAGQ_SPIKE_4811_LARGE=3000 DAGQ_SPIKE_4756_LARGE=3000 sh scripts/cargo-brief.sh --log $TMPDIR/large3000b.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::(rest::(a_finding|a_goal|main_s_saved|the_baselines|what_befell)|a_snapshot_after|the_candidates|the_updates)/)' --no-capture
sh scripts/cargo-brief.sh --log $TMPDIR/rest3.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::(a_finding|a_goal|main_s_saved|the_baselines|what_befell)/)' --no-capture
DAGQ_SPIKE_4811_LARGE=3000 sh scripts/cargo-brief.sh --log $TMPDIR/large3000c.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::(a_finding|a_goal|main_s_saved|the_baselines|what_befell)/)' --no-capture
DAGQ_SPIKE_4811_LARGE=3000 sh scripts/cargo-brief.sh --log $TMPDIR/large3000d.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::(the_baselines|what_befell)/)' --no-capture
DAGQ_SPIKE_4811_LARGE=3000 sh scripts/cargo-brief.sh --log $TMPDIR/large3000e.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::every_queue_kind/)' --no-capture
sh scripts/cargo-brief.sh --log $TMPDIR/b300.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::(the_baselines|a_goal_is)/)' --no-capture
DAGQ_SPIKE_4811_LARGE=3000 sh scripts/cargo-brief.sh --log $TMPDIR/b3000.log cargo nextest run --locked --lib -E 'test(/earlier_visibility_spike::rest::(the_baselines|a_goal_is|every_queue_kind)/)' --no-capture
sh scripts/cargo-brief.sh cargo nextest run --locked --lib --stress-duration 60s -E 'test(/earlier_visibility_spike::rest::/)'
sh scripts/check-local-gates.sh --base bfd6eb9f6a1c1d623eac8e39265e4f63063590d9
sh scripts/check-design-docs.sh; sh scripts/check-design-sentences.sh; sh scripts/check-frontmatter-dates.sh; sh scripts/check-doc-links.sh; sh scripts/check-doc-frontmatter.sh
sh scripts/cargo-brief.sh cargo fmt --all --check
sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings
```

1 本目は既定（答えを 30 と 300 単位）、2 本目と 3 本目が 3000 単位の答えの実行（2 本目の 5 本は test の大きさの loop の誤り（3000 を 2 回飛ばした）で索引の外になり、直して 3 本目で流し直した）。3 本目は 4756 の単位ごとの supervisor の生存の test も最終の木の 3000 単位で流した（3 本とも通った。1 本 2.5〜6 分）。4 本目以降は review の指摘を直した後の (B)〜(F) の 300 単位と 3000 単位の実行（5 本目は (C)(E)(F) が通った所で shell の時間の上限で止まり、(B)(D) を 6 本目で、全部の kind を同時に狭めた test を 7 本目で流し直した。どれも通った）。review の 1 回目の手直しで task の基準を足した後に、(B)(E) を 300 単位、(B)(E) と全部の kind を同時に狭めた test を 3000 単位で流し直し、追加した 7 本を stress（1 周が 15 秒を超えるので `--stress-duration 60s`）に通した（周回と結果はこの run の手元の検証の記録（`scripts/cargo-brief.sh` が書く記録）と receipt が持つ）。(A) の test は直しの前後で読みの形と履歴が変わらないので 2 本目の実行を記録とする。

### 試作

`src/application/ports/observation/earlier_visibility_spike/rest.rs`（`earlier_visibility_spike` の子の `#[cfg(test)]` の module、未配線。runtime の読みは変えていない）。

- 履歴: (A) は 4756 の履歴（`near_snapshot`・`history_ii`・`queue_units_only`）の単位の supervisor（`u<k>`）を 1 つ（`u`）にした `shared_lives`（最初の start と最後の stop だけを残し、単位ごとの heartbeat・待ち・標本・失敗は残す）。全件の `kpi` は始まった supervisor ごとに全 event から生存を引くので、これで 3000 単位の答えを debug build で流せる。単位ごとの start・stop が増える形は、4756 の test を `DAGQ_SPIKE_4756_LARGE=3000` で最終の木で流して比べた。(B)〜(E) は `rest_log`（4756 の `near_snapshot` に `add_rest` を足す）: 単位ごとに goal の `attributes_baseline`、task 64（範囲で着地する task。行は範囲が名指すので丸ごと読む）の `attributes_baseline` と組の印、finding の `finding_baseline`（finding の解決後の状態）と組の印、goal 700 の `goal_updated`、`main_paths_recorded`（単位の commit は前の単位の file を消すので main の path の数は単位で増えない）、2 単位ごとに branch `release` の `ci_checked` と `ci_turned_red`。範囲をまたぐ場合として、単位の前の `release` の check、run 60 の `c60` が S 前に赤くなりその後に別の sha `x1` で赤くなる場合（範囲でも `c60` が赤くなる）、S 前の `c60` の revert `rv60b`（範囲の `rv60` と合わせて 2 回）、goal 700 の最後の変更の後の `goal_status_changed`、S 前の goal 700 と task 64 の基準でその組の印が範囲（`--at` の窓の後）にあるもの。単位の run はそれぞれ単位の commit を着地させる。(F) は 4678 の `history`（単位の task を持つ）に、単位ごとに finding を単位の proposal へ `proposed` にしてから解決する形と、S 前に proposal 9 へ `proposed` にして解決した finding 51（proposal 9 の task 68 は開いたまま）、goal 701 だけで走る proposal 10（`goal_submitted` が名指し、goal 701 の task 69 は開いたまま、task は proposal を名指さない）を S 前に結んで解決した finding 53、その後に解決した finding 52 を足す（`links_log`）。
- 狭めた読み（`queue_rest`）: 4756 の `queue_ii` の supervisor・snapshot・ask の形に、`Rest` の部分を足す。(B) 基準の kind ごとの S 前の最新の印。(E) 範囲の行が名指す goal ごとに、問いの窓の最も早い終わり（`--at` の窓）以前で goal を置き換える最新の record（`goal_created`、`new` を持つ `goal_updated`、組の印がその終わり以前にある `attributes_baseline`）と、その goal のその後の record。(D) 範囲の run の着地の commit を sha に持つ `ci_turned_red`。(F) 範囲の record が名指す proposal と、範囲が名指す goal の `goal_submitted` が名指す proposal を、`proposed` で名指す finding の行。(C)(D) の main は `main_rows` が S 前の記録を、builder が保存する行（path・記録の届き・Git の読み・branch ごとの CI の最初と最後の check）と、範囲の record が名指す sha（範囲に届く run 行の着地の commit、範囲の記録の sha・`after`・revert の target、(D) では着地の commit を target にする revert）の commit の event に置き換え、行の key で別々に数える（`rest_reads` の [queue 行, 保存する行, sha の event]）。(B)(E) の基準は組の印の行も読む行に足す。各部分は読む量を、単位の task と run を外した 30・3000 単位の履歴と、持つ 30 単位の履歴の 3 つで同じことを確かめる（`rest_part`）。
- 答えの比べ（`rest_misses`）: 4756 の `queue_misses`（kpi の 4 問と `--by area`、`stats --since`（2 日前）`--full`（`[areas]` つき）の JSON 全体、日の問いの窓の `main_covers`、run 60・65 の `post_landing_book`）に、report の `goal_summaries`（goal 700）と `diagram_draft`、窓の終わりの task 64 の属性（`Attributes::tasks`、task の基準と queue の印の組の答え）を足す。fixture に近く着地する task が無いので `diagram_draft` はどちらも `not_drawn` で、比べたのは実質 `goal_summaries`（report の中で goal の題を図に渡す値）。`improvement_proposals` は kpi の JSON に入る。

| test | 部分 | 比べたもの |
| --- | --- | --- |
| `the_final_shapes_of_the_waits_lives_samples_and_failures_answer_as_every_row_with_many_units` | (A) (ii-a)（snapshot は day 158）・(ii-f)・(ii-e) | 4756 の最終の形（`QUEUE_SHAPED`）で、1 つの生存の履歴の 30 と 3000 単位の答えが全件と一致。部分の kind の行は (ii-a) 95 / 95、(ii-f) 2 / 2、(ii-e) 10 / 10（30/3000、単位ごとの生存の履歴でも同じ）。3000 単位の全行は 21,259・3,002・18,006 |
| 4756 の `a_snapshot_after_the_settled_history_reads_its_marks_by_the_range`・`the_candidates_sampled_read_by_their_range_answer_as_every_row`・`the_updates_failures_and_asks_of_no_run_read_by_their_range_answer_as_every_row`（`DAGQ_SPIKE_4756_LARGE=3000`） | (A) 単位ごとの supervisor の start・stop が増える履歴 | 最終の木で 3000 単位の答えが一致（下の「判定」の注） |
| `the_baselines_read_from_their_latest_marks_answer_as_every_row` | (B)（task の基準と queue の印の組を含む） | 一致（task 64 の属性は範囲の印で閉じる組の基準 `t64 baseline`、goal 700 の `goal_summaries` を含む）。基準の kind の queue 行は 4 / 4（全行 94 と 9,004）。task の基準は task 64 の行の record（範囲が名指す行で丸ごと読み、単位ごとに 1 件増える） |
| `main_s_saved_rows_and_its_commits_by_sha_are_counted_apart_and_stay_as_many` | (C) | 一致。[範囲の main の行, 保存する行, sha の event] は [3, 5, 3] / [3, 5, 3]（全行 146 と 13,511）。4756 の形は run 60 の CI の記録が `before_record`（全件は `recorded`）と revert の数 1（全件は 2） |
| `what_befell_a_landing_reads_its_failures_and_reverts_by_its_commit` | (D) | 一致。[7, 5, 3] / [7, 5, 3]（全行 195 と 18,015）。4756 の形は run 60 の `ci_failures` の `count` 1・`first_at`・`coverage` と `reverts` の `count` 1・`first_at` の 5 つが食い違う |
| `a_goal_is_read_from_its_latest_record_that_replaces_it` | (E)（基準の kind も狭める） | 一致（`goal_summaries` の goal 700 と `diagram_draft` を含む）。goal と基準の kind の行は 7 / 7（全行 187 と 18,007） |
| `a_finding_settled_before_the_range_links_the_proposal_the_range_names` | (F) | 一致。finding の行は 5 / 5（全行 35 と 3,005）。4756 の形は `improvement_proposals` を 0（全件は 2: proposal 9 と、goal 701 を通る proposal 10）と数え、その比べの層を含め 45 か所食い違う |
| `every_queue_kind_read_by_the_final_shapes_together_answers_as_every_row` | 全部の queue の kind を同時に（`REST`） | 30・3000 単位で答えが一致。[137, 5, 3] は 30・300・3000 単位で同じ（queue 行の全部は 1,267・9,907・96,307） |

比べなかった場合: SQLite の adapter の SQL（下の SQL は試作の in-memory の条件と同じ式だが流していない）。`main_rewritten`・`main_reverts_imported`（`unread`）。窓の終わりが組の印より前の窓（`--at` の窓）での task 64 と goal 700 の属性そのもの（`LedgerWindow::attributes` は窓の終わりで畳み、試作の比べは日の問いの窓だけ。`--at` の窓は kpi の JSON の比べだけに入る）。goal の task が範囲の外にある goal の `goal_summaries` の task の数（範囲の task だけの goal で比べた）。finding の基準の組が多くの record を持つ場合（fixture の組は 1 件ずつ）。

### 判定

どの部分も **holds**。根拠の test は上の表の名前。答えは全件の読みと JSON 全体で 30 と 3000 単位で比べ、読む量は部分の kind の queue 行（main は保存する行と sha の event を別に）を 30 と 3000 単位で数えた。どの部分もその kind の行が単位ごとに増える履歴で比べた。

| 部分 | 判定 | 読みの形（SQL と索引、または builder の表） | 30/3000 の読む量 |
| --- | --- | --- | --- |
| (A) (ii-a) `slots_full`・`claim_hold`・`landing_hold` | holds | 4678 の約束 6 のまま（`ledger_rows(kind, subject_kind, ended_at)` と `inputs_through` の式の索引） | (ii-a) の kind の和 95 / 95 |
| (A) (ii-a) supervisor の生存と heartbeat | holds | S 前に最後に始まった supervisor（`events_by_kind(kind, id)` を id の降順に歩き、時刻が S より前の最初の 1 件）と範囲の record が名指す supervisor の、kind ごとの S 前の最後の event: 同じ索引を `kind = ? AND json_extract(payload, '$.supervisor') = ?` で id の降順に歩き、`created_at < S` の最初の 1 件で止める（時計の戻りは約束 5 と同じ id の順の規則。`CREATE INDEX ... ON run_events(kind, json_extract(payload, '$.supervisor'), id) WHERE kind IN ('supervisor_started','supervisor_alive','supervisor_stopped', ...)`） | 同上 |
| (A) (ii-a) mark の撤回 | holds | 読んだ mark を名指す `mark_retracted`（`json_extract(payload, '$.mark')` の部分の式の索引、`WHERE kind = 'mark_retracted'`）と範囲の撤回 | 同上 |
| (A) (ii-a) forecast の snapshot | holds（snapshot が確定した履歴の後のとき） | 範囲の task を名指す snapshot を builder の snapshot の target の表（`ledger_snapshot_targets(task_id, event_id)`）で引く | 同上 |
| (A) (ii-f) `candidates_sampled` | holds | S 前の最後の標本（`events_by_kind` を id の降順）と範囲の標本、その supervisor の生存の行 | 2 / 2 |
| (A) (ii-e) `update_failed`・`backend_call_failed`・`auto_repaired`・`stall_config_loaded` | holds | 4678 の約束 6 のまま（範囲の行。`stall_config_loaded` は E 以前の最後の 1 件で、kind ごとの S 前の最後の行と範囲の行が同じものを返す） | (ii-e) の kind の和 10 / 10 |
| (A) (ii-e) run の無い ask と答え | holds | 範囲の答えと答えを適用する event が名指す ask の opening（`asks_by_id`）と answer（`ledger_answered_asks` の key から行）、開いたまま残した行の表の答えの無い ask。ask 1735 の回答による更新 | 同上（適用する event の反例は task 4807 で比較） |
| (B) baseline の印 | holds | 基準の kind ごとに S 前の最新の印（`events_by_kind` を id の降順に歩き、印（task・goal の無い `attributes_baseline`、`finding` の値の無い `finding_baseline`）で時刻が S より前の最初の 1 件）。歩く数が S で閉じていない組の基準の record に比例しないよう、印だけの部分の索引（`WHERE kind = 'attributes_baseline' AND task_id IS NULL AND goal_id IS NULL`、`WHERE kind = 'finding_baseline' AND json_extract(payload, '$.finding') IS NULL`）を足す。名指す goal の基準は (E) の seek が読み、その組の印（基準の id の後の最初の印、同じ索引で `id > ? ORDER BY id LIMIT 1`）も読む。範囲の印は約束 6 の範囲の行。finding の基準の値の record は finding の行に入り、finding の行の読み（約束 6 と (F)）が持つ。task の基準は task 行の record で、範囲が名指す task の行は (i) の読みが丸ごと読み、その基準は読んだ印のうち後の最初のもので組を判じても全件と同じになる（`baseline::taken` は record の後の最初の印で決め、S 前の組の印はどれも窓の中なので、S 前の最新の印で閉じても取ることは変わらない） | 基準の kind の行（印と goal の基準）4 / 4。finding の行は数えていない（全部の kind を同時に狭めた test が数える） |
| (C) main の記録 | holds（(D) の revert の target の seek と組で） | builder が S 以前の main の要約を保存する行（path・記録の届き・Git の読み・branch ごとの CI の最初と最後の check）を読み、範囲の record が名指す sha の commit を builder の commit の表（`ledger_main_commits(sha, reverts, event_id)`、`sha` と `reverts` の索引）で引く。revert の数は、範囲の run の着地の commit を target にする S 前の revert を (D) の seek で引かないと全件と違うので、(C) の test は (D) と同じ形（`landing`）で流した。試作は要約を S ちょうどで作った。実装は builder の pass ごとの要約のうち S 以前の最新と、その位置から S までの main の event を読む（pass の間隔に比例し、確定した履歴には比例しない） | 保存する行 5 / 5、sha の event 3 / 3、範囲の main の行 3 / 3。要約の path の量は S に main が持つ path の数（repository の大きさ）に比例する |
| (D) `post_landing_book` | holds | (C) の形に、範囲の run の着地の commit を target にする revert（commit の表の `reverts` の索引）と、その commit を sha に持つ `ci_turned_red`（`json_extract(payload, '$.sha')` の部分の式の索引、`WHERE kind = 'ci_turned_red'`）を足す。CI の要約は branch ごと | [7, 5, 3] / [7, 5, 3] |
| (E) goal の最新の seek | holds | 範囲の task が名指す goal ごとに `events_by_goal(goal_id, id)` を問いの E 以前から id の降順に歩き、goal を置き換える最初の record（`goal_created`、`new` を持つ `goal_updated`、組の印が E 以前の `attributes_baseline`）で止め、それより後の record（状態の変化・閉じ・開き直し）を読む | 7 / 7。歩く数はその goal の最後の置き換えの後の record に比例し、範囲の外の確定した履歴には比例しない |
| (F) finding が結ぶ proposal | holds | 範囲の record が名指す proposal（`Named` の proposal と同じ）と、範囲の行が名指す goal の `goal_submitted`（`events_by_goal`）が名指す proposal を、`proposed` で名指す `finding_status_changed` を `json_extract(payload, '$.proposal_id')` の部分の式の索引（`WHERE kind = 'finding_status_changed'`）で引き、その finding の行を key で読む。走っている proposal は終わっていない task（proposal の task か、`goal_submitted` が名指す goal の task）を持ち、その task 行は外れないので、この 2 つに限ってよい（`improvement_running` は終わっていない task が 1 つ以上要る） | 5 / 5 |

- (A) の注: 1 つの生存の履歴では `supervisor_started`・`supervisor_stopped` の行は単位で増えない（増えるのは heartbeat・待ち・標本・失敗）。単位ごとに start・stop が増える履歴の 3000 単位の答えは、4756 の 3 本の test（上の表）で最終の木で比べた。day 3 の古い snapshot の (ii-a) は 4756 の does_not_hold のままで、この追試では試していない。
- (E) の止まり方のうち、組の印が E 以前にある goal の `attributes_baseline` で止まる場合は試作で起きていない（goal 700 の基準の印は `--at` の窓の後にあり、止まったのは `goal_updated`）。`taken` の条件は `baseline::taken` と同じ。
- (C) の潜む違い: sha で引いた最後の単位の commit は前の単位の commit を revert するが、狭めた読みではその target が記録に無く理由が `before_record`、全件では `not_a_landing` になる。比べた答えは理由を出さないので食い違いに現れないが、revert の一覧を出す読み手を足すときは target の commit も sha の表から引く。
- 4756 の形との違い: (C)(D) の 4756 の `main_summary` は CI の check を branch を問わず最初と最後だけ残し、revert を sha だけで引くので、上の反例で run 60 の答えが食い違う。(F) の 4756 の finding の読み（S に開いているか範囲の record を持つ行）は finding 51 の結びを落とす。(B)(E) の 4756 の形（基準と名指す goal の行を全部読む）は答えが一致するが、読む行が単位で増える。
- ADR-t1662-2 決定 1・7・8 との関係: 足す表（snapshot の target、main の commit の sha と revert の target、main の要約の行、開いたまま残した行の表の queue の ask）は event だけから作り直せる台帳の一部（ビュー）で、event が SSOT のまま。行の形と `ledger_v` を変えない。今ある台帳には無いので、表を作る migration で今の行から埋めるか、次の pass が埋めるまで全件の読みに戻す。索引は `run_events` の部分の式の索引で、行の形を変えない。

### 読みの約束の更新

- 約束 6 に足す kind: (ii-a) の待ち・supervisor の生存と heartbeat・mark の撤回・forecast の snapshot（snapshot が確定した履歴の後にあるとき）、(ii-f) `candidates_sampled`、(ii-e) の全部、baseline の印、main の記録（要約の行と sha の表）、`ci_turned_red`、goal の record、finding の結び。形は上の「判定」の表。
- 足さない kind: (ii-a) の snapshot と範囲の間の mark（snapshot が確定した履歴より古いとき。4756 の does_not_hold で、planner の決定 A が受け入れた増え方）、`live_alert_stream`（goal 273）、`main_rewritten`・`main_reverts_imported` の `unread`（試作の履歴に無い。全ページの読みに残す）。
- 4756 の「読みの約束の更新」の finding の留保（`improvement_proposals` を読む問いは finding の行を全部読む）は、上の (F) の形で task 4808 が解いた（finding の行は範囲で読み、S 前に settled になった finding は結ぶ proposal から読む。test は `ledger_queue_named_answers_as_every_row` の finding 51〜53）。
- 約束 6 に足した kind（task 4808。ビューを信じてよい store だけ。戻りの条件と読む量の境は `LedgerWindowRead::queue_views_usable`・`queue_main_before`・`queue_sought` の doc comment）。どれも SQLite の `queue_named_tests::the_reads_by_the_views_return_as_many_with_300_settled_units`（30 と 300 単位）、store の約束の `ledger_queue_named_answers_as_every_row`・`ledger_queue_named_goes_back_whole`・`ledger_queue_named_past_an_old_writer`、`queue_named_tests::each_seek_of_the_views_searches_its_index` で確かめた。
  - main の記録（`main_commits_recorded`・`main_paths_recorded`・`main_observed`・`main_read_failed`・`main_read_recovered`・`ci_checked`）: `queue_before` 以前の最新の要約とその後の event、S 前は名指す sha の commit（`queue_views::tests::the_folds_from_each_summary_and_the_events_after_it_are_those_of_every_event`）。
  - `ci_turned_red` と revert の target: 範囲の run の着地の commit を sha と target に持つもの（run 60 の反例）。保持した `main_reverts_imported`（S 前も）と sha の seek が返した複合 event の revert が名指す target も参照先が増えなくなるまで補う（task 4808、`ledger_queue_named_answers_as_every_row` の `imported-revert`・`import-before`・`sought-revert`・`chain1`〜`chain5` の Fact 全体の比べ）。
  - goal の record（`goal_created`・`goal_updated`・`goal_status_changed`・`goal_closed`・`goal_reopened`・goal の `attributes_baseline`）: 名指す goal ごとに S 前の最後の置き換えまで（`queue_named::tests::a_goal_s_walk_stops_at_what_replaces_it_before_the_time`）。
  - baseline の印（`attributes_baseline`・`finding_baseline` の印）: kind ごとに S 前の最新。
  - finding の行（`finding`）: 範囲で読み、名指す proposal を結ぶもの。
- main と CI の時計が進んで戻った場合も確定履歴に沿って要約を後退させない。main は書き手が時刻の境界で作って進める状態から S 以後の各窓の E に合うものを同じ要約行で選び、CI はその branch の S 時点の最初・最後の記録を索引で補う（task 4808、`ledger_queue_named_past_a_clock_set_back` と `an_early_clock_set_back_keeps_the_reads_bounded_with_300_settled_units`。時計の反例を全確定単位より前に置き、30・300 単位の答えと実際の返却量を比べた。main の各 kind の状態は `a_main_clock_regression_keeps_the_eligible_state_of_every_main_kind` でも比べた）。
- 未来時刻の main の観測の後に S 前の CI の記録だけが続く場合も、最初の CI で時刻別の状態を作って進め、要約の後退と全 CI の再読を避ける（task 4808、共通の `ledger_queue_named_with_only_ci_after_a_future_main_event` は 30・300 件、SQLite の `only_ci_after_a_future_main_event_keeps_the_reads_bounded_with_300_checks` も 30・300 件で答えと返却量を比べる。間に別の main event を挟まない）。
- task 4807: 待ち（`slots_full`・`claim_hold`・`landing_hold`）は約束 6 の範囲と S 前の最後の行で読む。根拠は `the_named_queue_answers_and_counts_with_300_settled_units`。
- task 4807: supervisor は最後の start と範囲の record・最後の sample が名指す token の kind ごとの S 前の最後の event を seek する。根拠は `the_named_queue_answers_and_counts_with_300_settled_units` と `the_named_queue_seeks_use_their_indexes_and_the_whole_read_skips_excluded_kinds`。
- task 4807: 読んだ mark の撤回を `events_by_retracted_mark` で補う。根拠は `the_named_queue_answers_and_counts_with_300_settled_units`。
- task 4807: task が名指す snapshot を表から seek し、その snapshot に必要な mark を 2 段目で補う。古い snapshot の mark の増え方の境は変えない。根拠は `the_named_queue_answers_and_counts_with_300_settled_units` と `the_named_queue_keeps_the_marks_of_an_old_snapshot`。
- task 4807: `candidates_sampled` と (ii-e) の失敗・修復・stall config は範囲と S 前の最後の行で読む。根拠は `the_named_queue_answers_and_counts_with_300_settled_units`。
- task 4807・ask 1735: run の無い ask は範囲の答えと答えを適用する event が名指す ask の opening と answer を seek し、未回答の ask を表から補う形に更新する。根拠は `names_include_answer_applications_and_the_last_sample_s_supervisor` と `the_named_queue_answers_and_counts_with_300_settled_units`。表と印の有効性を判じられないときは全ページに戻る（`the_named_queue_shared_contract_answers_and_falls_back` と `the_named_queue_falls_back_past_an_old_writer_then_seeks_after_refilling`）。
- 約束 1〜5・7 は変えない。

### 未解決

この追試の 6 部分に unresolved は無い。限りは「比べなかった場合」と「判定」の注（(A) の 1 つの生存の履歴と 4756 の 3 本、(E) の基準で止まる場合、(C) の revert の理由、`diagram_draft` が `not_drawn` のまま）。

実装の追試（task 4807）で、(A) の run の無い ask に反例が出た（`a_run_less_ask_answered_before_s_and_applied_in_range_is_a_counterexample`）。S 前に開いて答えた ask の答えを範囲内で適用し、別の ask の opening と answer が S 前の最後の行になっている場合、元の形は先の ask の opening と answer を読まず、全件の `ask_apply_wait` が 1 件あるのに 0 件になる。ask 1735 の回答で、適用する event が名指す ask も集めて opening と answer を補う形に広げ、operation 58 に `replan propose` した。task 4807 の共通の store の組はこの反例も全件と比べる。元の形の限界を示す試作の test は残す。

### 後続の draft への推奨

goal 273（task 4660・4646）・Spike 4663・task 4805 と重ねない: `live_alert_stream` の records と (C, S] の写さない kind は goal 273 に、queue 行を要る列に絞ることは Spike 4663 に、待ちの fold の O(waits × open tasks) は task 4805 に任せる。この追試は「どの queue 行を読むか」だけを決めた。どの部分も holds なので、「範囲が決まらない」に足す kpi・report の問いは無い（forecast の snapshot が確定した履歴より古いときも全件に戻さず、snapshot から S までの mark を 2 段目で読む。増え方の境は 4670 の (vii) と planner の決定 A のとおり）。

- 4806（表と索引）: snapshot の target の表、main の commit の表（`sha`・`reverts`・event の id）、main の要約の行（pass ごと、branch ごとの CI の最初と最後の check を含む）、開いたまま残した行の表に queue の答えの無い ask を足す。`run_events` に `supervisor`・`mark`（`mark_retracted`）・`ask_id`（`ask_opened`）・`sha`（`ci_turned_red`）・`proposal_id`（`finding_status_changed`）の部分の式の索引と、基準の印だけの部分の索引を足す。約 300〜400 行。
- 4807（(ii-a)(ii-e)(ii-f) の読み）: 上の表の (A) の形。受け入れの比較は、この追試の `shared_lives` の 30 と 3000 単位の履歴と 4756 の単位ごとの生存の履歴。約 200 行。
- 4808（(ii-b)(ii-c)(ii-d)・baseline・finding の読み）: (B)〜(F) の形。受け入れの比較に (D) の 3 つの反例、(E) の `goal_summaries` と `diagram_draft`、(F) の finding 51 を入れる。要約は pass ごとの S 以前の最新とその後の event の読みで、試作の S ちょうどの要約と違う所（要約と S の間の event）を test に足す。約 300 行。
- 4122（store の範囲の読み）: 約束 6 の kind ごとの形を足し、adapter の test で `EXPLAIN QUERY PLAN` が上の索引を使うことを確かめる。約 200 行。
- 4132（domain の範囲の関数）: 名指すものの集め方（試作の `queue_rest`・`main_rows`）に supervisor・goal・sha（範囲の record だけが名指す）・ask・proposal を置く。約 150 行。
- 4133（kpi の配線）: (ii) の kind で全件に戻すのは forecast の snapshot が古いときだけ。受け入れの比較にこの追試の 30・3000 単位の履歴を使う。約 150 行。
- 4134（report の配線）: `goal_summaries`・`diagram_draft` と post_landing を受け入れの比較に足す。約 150 行。
