---
id: plan-ledger-window-read-preconditions-spike
type: plan
title: スパイク：台帳の窓を範囲で読む約束が、時計の戻りとずれ、forecast の採点の snapshot から終わりまでの mark の数、--compare の群に渡す run の終わりの時刻まで全件と同じ答えを保つ読みの形と読む量の根拠を今のコードで持てるか
status: completed
created: 2026-11-09
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - plan-ledger-window-read-contract-spike
  - plan-kpi-read-memory-spike
  - design-measurement
---

# スパイク：台帳の窓の範囲の読みの先行状態（時計・forecast・群の終わり）

問いは「[範囲の読みの約束](ledger-window-read-contract-spike.md)（先行状態 (i)〜(v)）に (vi)〜(viii) を足して、kpi（`--period`・`--at`・`--since`/`--until`・`--by`・`--goal`・`--compare` の At と Windows）と report の答えを全件を読む形と同じに保ったまま、読む量を範囲の外の確定した履歴に比例させない読みの形を決められるか」。
決められない部分は、どの問いを「範囲が決まらない」（今の全件の読みに戻す）に入れるかを決める。

判定は全体で **does_not_hold**（下の「判定」）。

- (vi) 時計の戻りとずれ: 読みごとに判定が分かれる。(a)〜(e) は、時刻の順の規則を時刻の索引で、id の順の規則を範囲の event ごとの id の seek で読めば、時計が戻りずれても全件と同じになる（索引を足す互換の migration が要る）。4135 の約束のうち、範囲の最初の id から id で遡る読み（(iii) の直前の claim、(iv) の直前の起動）と、範囲が今に届けば索引が要らないとした (i)・(ii) の読みは、時計が戻るかずれると答えが変わるので約束から外す。event の id の cursor の時刻（`marks::cursor_millis`）と、過去の範囲の台帳の行の上限（`stats` が期間の run を `Cursor::Time(E).event_id`、`E` 以前に書かれた event の id の最大で選ぶので、その id が要る）は、打ち切らない探索では範囲の外の履歴に比例する。どちらも固定の件数で打ち切る探索で引き、見つからなければ全件の読みに戻す形で立つ。時計がそろって今に近い cursor と今に届く範囲では範囲で読め、時計が戻るかずれたときと、後に多く書かれた過去の cursor と範囲では全件に戻る。
- (vii) forecast の採点: 2 段の読みで全件と同じ答えになるが、2 段目の読む量は範囲の中で終わる target の開いていた期間（前の終わりか作成から範囲の終わりまで）の snapshot・claim・mark と、snapshot の後に記録された mark（範囲の後のものも含む）に比例する。snapshot より前の履歴には比例しない。
- (viii) `--compare` の群の終わりの時刻: 終わりの event を部分索引で読む形で、run の行の本文を読まずに全件と同じ時刻が得られる。`ledger_rows` の `ended_at` は `finished_event_id` の event の時刻と一般に違うので使えない。

## 環境

- 対象の commit: `a3b65153f59b9d33b44a96feee0cceb5c24ecfa9`（この run の base）。4135 の調べた `f360fe07` からの差で読んだ関数（`marks`・`compare`・`forecast::score`・`kpi::window`・`stats::window`・`ledger::window`）に触れた commit は無い。
- host: Apple M1、macOS 27.2（26B123）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`cargo-nextest 0.9.150`。
- provider と経路: 本番 queue の Spike の run（task 4670）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。本番の queue と DB は読んでいない（試作は合成の event だけを使う）。
- 使った時間と回数: 上限 3 時間に対して、1 回目の turn が約 45 分（22:37〜23:20 JST）。試作は 1 回書き、初回の build の前に自分の読み直し（goal の作成を 2 段目の下限にすること、群の比較を全件の終わりで行うこと、mark の数の期待値）で直し、11 件とも初回で通った。subagent の review の指摘（(vii) に時計の戻りと `triggers_through` の無い snapshot と取り消しが無く時刻の読みを消しても通ること、cursor の今に近い場合が履歴を増やしていないこと、終わりの無い範囲に event が無いこと、前の終わりの target の照合）で 1 回作り直した。作り直した後、(vii) の時刻の 2 つの読み（`recorded_effective` と `TASK_MARK_KINDS` の時刻の読み）をそれぞれ消すと (vii) の 3 件が落ちることを確かめた。run の review の指摘（行の上限を時刻だけで切ると `stats` の id の期間の run が変わる反例、`started_at` が records の時刻の最小の代わりにならないこと、`ended_at` の時刻の丸め）で 2 回目を直し、試作に `vi_a_row_after_the_end_in_time_is_in_the_period_by_id` を足して上限の判定を直した。run の 2 回目の review の指摘（今に届く範囲でも、問い合わせの直前に時計が戻るか DB の時計が進めば `U_E` の探索が確定した履歴を全部歩く）で 3 回目を直し、探索を固定の件数で打ち切って上限なしに戻す形にして `vi_a_clock_turned_back_before_the_read_stops_the_walk_and_reads_every_row` を足した。作り直しは 3 回（上限 3 回）。その後、人が ask 1623 で差し戻した指摘（event の cursor の探索も `K` 件で打ち切れること、(d) の間の起動の根拠の test が起動の seek の分岐に入らないこと）で 4 回目を直した。上限の 3 回を超えたのは人の差し戻しに従ったため。`latest_time_within` と `vi_a_start_between_the_claims_by_id_is_sought_whatever_its_time` を足した。使った時間は合わせて約 2 時間。

## 読んだコードと打ったコマンド

読んだコード:

- `src/domain/marks.rs`: `marks_with_earlier`（有効時刻で選び `(at, id)` で並べる）・`retractions`（mark ごとに id の最初の撤回）・`derived_marks`（claim を id の順に並べ、間の起動も id で見る。mark の時刻は claim の `created_at`）・`cursor_millis`（id 以下の event の時刻の最大）・`cursor_time`。
- `src/domain/kpi/compare.rs`: `splitting`・`routine`（`(at, id)` の順で直前の起動の `parallel` と比べる）・`classified`・`overlapping_groups`。
- `src/domain/forecast/score.rs`: `score`（snapshot の後の最初の終わりを id で選び、mark は `triggers_through` があれば id、無いか派生 mark なら時刻で snapshot の後とし、終わりの時刻以前を数える）、`src/domain/forecast/snapshot.rs`（snapshot は開いている全部の task と goal を 1 件に持つ）。
- `src/domain/kpi/window.rs`: `Context::new`（`marks_with_earlier` と `score::score` を全件で呼ぶ、`finishes` を `everything.runs` の `finished_event_id` の event の時刻で作る）・`counts`・`forecast_kpis`。
- `src/domain/stats/window.rs::ends_run`、`src/domain/stats.rs`（`in_goal` は task の今の goal の map で絞る）、`src/domain/ledger/attributes.rs::task_goals`（task の作成の記録と `task_goal_changed` の最後の値）。
- `src/domain/ledger/window.rs`（「A read from a time」・`RowsFrom::leaves_out`）、`src/domain/ledger.rs`（`fill_run`・`fill_legacy_run`）、`src/domain/ledger/fold.rs::fold_run`。
- `src/application/ports/observation.rs::ledger_read_snapshot`、`src/domain/kpi.rs::read_from_ms`。
- 索引: `migrations/` の `run_events` の索引（4135 の節と同じ。`created_at` の索引は無い）。Postgres の `payload` は text で、読む SQL は `payload::jsonb` で解く（`src/infrastructure/postgres/search_write.rs`）。

打ったコマンド:

```sh
git rev-parse HEAD
grep -rn "CREATE INDEX" migrations | grep run_events
sh scripts/cargo-brief.sh cargo nextest run --locked --lib -E 'test(/read_contract/)'
cargo fmt --all
sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings
```

## 試作

`src/domain/kpi/compare/read_contract/preconditions.rs`（`#[cfg(test)]` の未配線の module。`read_contract.rs` が `mod preconditions;` で持ち、親の `claim_value`・`effective_ms`・`DERIVED`・`Reads`・`range_events` を使う）。

- `Store` は event を書いた順の id で持ち、時刻は各時計が書いた値のままにする（id の順に時刻が戻りうる）。読みはそれぞれ表す索引を doc comment に書き、返した event の数（走査なら歩いた数）を数える。
- 範囲の読みの関数（`range_events`・`forecast_events`・`finishes_in`）が選んだ event を、domain の今の関数（`marks::marks`・`compare::classified`・`score::score`・`stats::run_stats`）にそのまま渡す。答えの規則は全件のときと同じ関数が決める。
- 各 test は範囲の外の確定した履歴（10 分ごとの run、5 本ごとに routine の起動、10 本ごとに撤回した mark と何も持たない snapshot）を 30 単位と 3000 単位で作り、全件の答えとの一致と読んだ数を比べる。

| test | 場合 | 比べたもの |
| --- | --- | --- |
| `vi_routine_takes_the_start_before_it_in_time_not_in_id` | description の例: 起動を id の順に (0 分, parallel 1)・(20 分, 1)・(10 分, 2) と書き、範囲 (15, 25] | 全件では 20 分の起動は routine でない。時刻の索引で直前の起動を引くと一致、4135 の id で引く形は routine にする。読んだ数は同じ |
| `vi_a_claim_is_compared_with_the_claim_before_it_in_id` | claim を v1（範囲の中）・v2（時計が戻り範囲の前）・v2（範囲の中）の順に書く | 全件では範囲に派生 mark が無い。claim ごとに id で前の claim を引くと一致、4135 の範囲の最初の id から遡る形は v1 → v2 を範囲に出す。読んだ数は同じ |
| `vi_a_clock_ahead_of_the_db_is_read_by_the_effective_time` | `--at` が DB の記録時刻より後（CLI の時計が進む）、prune の heartbeat が記録より後、`--at` が DB の now より後 | 有効時刻の索引で一致、読んだ数は同じ。範囲の始まりから今までに記録されたものを読む形（4135 の今に届く範囲の読み）は 3 件のうち 2 件を落とす |
| `vi_a_retraction_by_a_clock_turned_back_is_read_by_the_mark` | 範囲の mark の撤回を、id は後だが時刻は範囲の前に書く | 名指す mark の索引で一致、範囲の時刻で撤回を読む形は落とす。読んだ数は同じ |
| `vi_an_event_cursor_reads_what_is_later_in_time` | id の cursor の event より前の id に、時刻が後の event がある。今に近い cursor（前の履歴を 30 と 3000）と過去の cursor（後の履歴を 30 と 3000） | `cursor_millis` と一致するのは時刻の索引を上から cursor 以下の id まで歩く読み。打ち切らなければ、今に近い cursor では読んだ数は同じ、過去の cursor では後の履歴とともに 10 倍を超えて増える。100 件で打ち切ると、今に近い cursor と後に 30 単位では見つけて全件の時刻と一致、後に 3000 単位では見つけずに全件に戻る。読む数はどれも 100 件以下 |
| `vi_a_start_between_the_claims_by_id_is_sought_whatever_its_time` | 範囲の前の claim（v1）と範囲の claim（v2）の間の id に、時計が進んで範囲の後の時刻で書かれた v2 の起動 | 全件では起動が名乗った変化なので派生 mark は無い。claim ごとの起動の seek で一致し、その起動を除くと `dagq_version v1 → v2` が出る。読んだ数は同じ |
| `vi_a_previous_claim_is_sought_not_taken_from_the_rows_read` | 時計が進んで範囲の終わりより後の時刻だけを持つ run が、範囲の claim の id で前の claim を持つ | 範囲の claim から id で前の claim を引けば一致。終わり以前の時刻の記録を持つ行の claim だけから派生 mark を作る形は範囲に mark を出す。読んだ数は同じ |
| `vi_a_row_after_the_end_in_time_is_in_the_period_by_id` | review の例: claim（30）・着地（31）の後に時計が戻って queue の event（20）、範囲 (15, 25]。今に届く範囲（前の履歴を 30 と 3000）と、過去の範囲（後の履歴を 30 と 3000） | 全件では `Cursor::Time(25).event_id` が queue の event の id で、着地の id はそれ以下なので run は期間に入る。終わりより後の時刻だけの行を外す形は run を落とす。最初の id がその id より後の行だけを外す形は一致する。その id を 100 件で打ち切る探索で引くと、今に届く範囲と後の履歴 30 では見つけて上限を付け、後の履歴 3000 では見つけずに上限なしで全部の行を読む。どれも答えは一致し、探索の読む数は 100 件以下 |
| `vi_a_clock_turned_back_before_the_read_stops_the_walk_and_reads_every_row` | review の 2 回目の例: 時刻 16 に claim・17 に着地、時刻 30 から確定した履歴を 30 と 3000 単位、問い合わせの直前に時計が 25 に戻り、範囲 (15, 25] で今に届く | 打ち切らない探索は確定した履歴を全部歩き 10 倍を超えて増える。100 件で打ち切る探索は 30 単位では見つけ、3000 単位では見つけずに上限なしに戻る。どちらも答えは全件と一致し、探索の読む数は 100 件以下 |
| `vii_the_history_before_the_snapshots_is_not_read` | task 900 が一度終わり reopen して範囲で再び終わる、goal 50 が範囲で閉じる、task 901 は終わらない、task 902 は時計が進んで範囲の後の時刻で一度終わり範囲で再び終わる、task 903 は範囲で取り消す。`triggers_through` を持つ snapshot と持たない snapshot。snapshot より前の id に時計が進んで後の時刻の mark と優先度の変化、snapshot と範囲の間に派生 mark・記録の mark（範囲の後に撤回）・優先度の変化、範囲の後に snapshot より前へ backdate した mark。snapshot より前の履歴を 30 と 3000 | 2 段の読みで全件の samples（task 900 の 2 件・goal 50）・excluded（取り消し）と一致（reopen の前の snapshot と task 902 は数えない）、読んだ数は同じ |
| `vii_the_history_between_the_snapshots_and_the_range_is_read` | 同じで、snapshot と範囲の間の履歴を 30 と 3000 | 一致、読んだ数は 10 倍を超えて増える |
| `vii_the_marks_written_after_the_range_are_read` | 同じで、範囲の後に記録した mark と撤回を 30 と 3000 | 一致、読んだ数は 10 倍を超えて増える |
| `vii_a_snapshot_whose_targets_do_not_finish_in_the_range_is_not_read` | 終わりの直前の settled の run・snapshot・mark を持ち、終わりの無い範囲 | 一致（空）、読んだ数は同じ（0） |
| `viii_the_finishes_are_the_runs_first_ends_read_by_time` | 2 つの mark の間の終わりが `min_samples`−1 本と `min_samples` 本、検証の失敗（最初の終わり）が範囲の前で範囲の中で着地した run、最初の終わりを時計が戻って範囲の前に書いた run、`--goal`（task の goal の map） | 範囲の終わりの時刻が全件の `finished_event_id` の時刻と一致（goal の絞りも）、読んだ数は同じ。全件の終わりで作る群（1 群・各自）も一致 |

比べなかった場合: 台帳の行の上限は、試作では run の行を run の event の全部として表し、`stats` の期間の run の選び（`period_runs`、`stats.rs` の `Cursor::event_id` と `finished_event_id` の絞り）だけで比べた。payload の時刻（Execution の `at`）の条件は試作に入れていない。`ledger_rows` の `ended_at` と終わりの時刻の関係は、行を組む試作を足さずにコードで確かめた（(viii) の節）。SQLite と Postgres の索引の式が実際に引けるかは、試作が in-memory なので確かめていない（4667 で確かめる）。

## 判定

### (vi) 時計の戻りとずれ: holds（一部は前提の崩れを固定の量の読みで検出して全件に戻す形）

時計は 3 つある: `created_at` は DB（SQLite の `strftime('now')`、Postgres では DB server）、`dagq mark --at` は CLI の process（`record_mark` はその時計で今より後を拒むだけ）、prune の `last_heartbeat_at` は supervisor が書く。
id の順に `created_at` が減ることも、有効時刻が記録時刻を超えることも、どの時計も止めない。
全件の読みはどちらの順も混ぜて使う: 時刻の順の規則（有効時刻の選択、`routine`、派生 mark と期間の時刻の絞り）と、id の順の規則（`retractions` の最初の撤回、`derived_marks` の前の claim と間の起動、`cursor_millis`、`score` の snapshot の後の最初の終わり、`stats` の run の最初の終わり）。
だから約束は、時刻の順の規則を時刻の索引で、id の順の規則を範囲の event ごとの id の seek で読む形にした。
前提の崩れを受け入れる形は約束に入れていない。

読みごとの判定:

- (a) 記録の mark の有効時刻での選択: holds（索引を足す）。有効時刻の索引で引けば、記録時刻との差の向きにも時計のずれにも依らない（`vi_a_clock_ahead_of_the_db_is_read_by_the_effective_time`）。4135 が「範囲が今に届く読みでは要らない」とした読み（範囲の始まりから今までに記録されたもの）は、CLI か supervisor の時計が DB より進むと範囲の前に記録された mark を落とすので、約束から外す。
- (b) 撤回: holds（索引を足す）。名指す mark の索引で引き、`retractions` と同じく id の最初の撤回を取る（`vi_a_retraction_by_a_clock_turned_back_is_read_by_the_mark`）。4135 の「今に届けば範囲の中に記録されている」は時計が戻ると崩れるので外す。
- (c) 派生 mark の属性ごとの直前の claim: holds（索引を足す）。`derived_marks` は claim を id で比べ、範囲の claim は時刻で選ばれるので、範囲の claim の間に範囲の外の時刻の claim が id で挟まりうる。範囲の claim ごとに属性ごとの部分索引を id で 1 件ずつ遡る（5 × 範囲の claim の数）。4135 の「範囲の最初の id の前で 1 件」は崩れる（`vi_a_claim_is_compared_with_the_claim_before_it_in_id`）。
- (d) 間の起動: holds（索引を足す）。範囲の claim ごとに、id で前の claim とその claim の間で新しい値を名乗る起動を 1 件引く（4135 の索引の形のまま、引く回数が範囲の claim ごとになる）。起動の時刻は見ず id だけで引くので、時計が進んで範囲の後の時刻で書かれた起動も、範囲の claim の新しい build を名乗れば派生 mark を抑える（`vi_a_start_between_the_claims_by_id_is_sought_whatever_its_time`。その起動は時刻の読み（範囲の記録の mark、範囲の前の最後の起動）のどれにも入らず、この seek を除くと `dagq_version v1 → v2` が出る）。
- (e) routine の直前の起動: holds（索引を足す）。`routine` は `(at, id)` の順で、起動の有効時刻は記録時刻なので、`run_events(kind, created_at, id)` を範囲の始まりの時刻から逆に 1 件引く（`vi_routine_takes_the_start_before_it_in_time_not_in_id`）。4135 の「今の `events_by_kind(kind, id)` を範囲の最初の id から逆に 1 件」は description の例で答えを変えるので外す。
- event の cursor の時刻: holds（固定の件数で打ち切る探索で検出して全件に戻す形）。`cursor_millis(Cursor::Event(id))` は id 以下の全 event の時刻の最大で、その event の時刻ではない（`vi_an_event_cursor_reads_what_is_later_in_time`）。id の索引からは時刻の最大を範囲で引けない。
  - 時刻の索引 `run_events(created_at, id)` を `(created_at, id)` の降順に歩くと、最初に出る id が cursor 以下の event の時刻が、時計がどう動いても id 以下の event の時刻の最大になる。それより前に歩く event は全部 cursor より後の id なので、歩く数は cursor より後の id の event の数 + 1 以下で、cursor が今に近ければ少ない。過去の cursor では後の履歴に比例する（同じ test の打ち切らない探索は 10 倍を超えて増える）。
  - だから探索を固定の件数 `K` で打ち切る（`SELECT MAX(created_at) FROM (SELECT id, created_at FROM run_events ORDER BY created_at DESC, id DESC LIMIT ?K) WHERE id <= ?id`。試作の `latest_time_within`）。`K` 件の中に cursor 以下の id があれば最初のものの時刻が正しい時刻で、それで範囲を決めて読む。無ければその問いを全件の読みに戻す。探索の読む数は履歴に依らず `K` 以下（同じ test: 今に近い cursor と後に 30 単位の cursor では見つけて全件の時刻と一致、後に 3000 単位では見つけずに全件に戻る）。`K` は答えを変えず、全件に戻る頻度だけを決める。時刻の text が `RECORD_TIME_FORM` でない event は時刻の索引の順が時刻の順にならないので、そういう event があれば全件に戻す（4667 でその検出の読みも決める）。
  - これを使う問い: `--at`・`--since`・`--until`・`--compare` に event の id を渡した kpi（`read_from_ms` は今 `--at` の id で全件に戻す）。今に近い id はこの形で範囲を決められ、後に `K` 件より多く書かれた過去の id は全件に戻る。
  - 別案（planner の判断）: 打ち切らずに store が `MAX(created_at) WHERE id <= ?` を 1 つの値として返す集約にすれば、過去の id でも service が受ける行は 1 行で、service のメモリは履歴に比例しない。DB の走査は id 以下の全部になる。
- 台帳の行の範囲の上限（4122 の (1)）: この判定は Spike 4754 の追試で見直された（[earlier-visibility の Spike](ledger-window-earlier-visibility-spike.md)の「追試 (iii) 可視性（Spike 4754）」の (iii-d) は unresolved で、E の後に始まった走行中の run の入力を保てない反例がある）。読みの約束 7 は U_E を入れず、planner の決定（request 294）で台帳の行に上限を付けず窓の終わりより後の行も今のとおり読む。下の判定は 4670 の時点の記録として残す。 holds（固定の件数で打ち切る探索で前提の崩れを検出し、上限なしに戻す形）。上限で読む量が減るのは、時計がそろって今に届く範囲だけ。過去の範囲は、範囲の後に打ち切りの件数より多く書かれていれば上限なしに戻るので、上限では読む量が減らない（範囲が決まらない）。
  - `stats::stats` は期間の端を `Cursor::Time(ms).event_id`（`ms` 以前に書かれた event の id の最大、`src/domain/stats.rs` の `Cursor::event_id`）に直し、run を `finished_event_id` がその id の間にあるもので選ぶ（kpi の期間は `Context::stats(Some(Cursor::Time(start)), Some(Cursor::Time(end)))`）。時計が戻ると、記録が全部 `E` より後の時刻の run の終わりが `U_E = Cursor::Time(E).event_id` 以下の id になり、全件ではその run が期間に入る（`vi_a_row_after_the_end_in_time_is_in_the_period_by_id`）。だから時刻だけで行を外す上限は答えを変え、約束に入れない。前の claim・最初の終わりの seek は run の集計の材料を補わない。
  - 全件と同じになる上限は「最初の id（`inputs_from`）が `U_E` より後で、どの記録の payload の時刻も `E` より後」の行だけを外す形。`U_E` より後の id の event は定義から全部 `E` より後に書かれているので、`created_at` の条件は id の条件に含まれる（同じ test）。
  - `U_E` を store が引く読みは、id の索引を新しい方から `E` 以前に書かれた最初の event まで歩く形で、歩く数は `U_E` より後の id の event の数になる。`E` が now でもこの数は時計に依る: 問い合わせの直前に時計が戻るか、DB の時計が問い合わせ側（`E` を決める CLI か service）より進むと、その差の間に書かれた event を全部歩く（`vi_a_clock_turned_back_before_the_read_stops_the_walk_and_reads_every_row` の打ち切らない探索は確定した履歴とともに 10 倍を超えて増える）。過去の範囲では範囲の後の履歴を歩く。時刻の索引からは、時計が戻ると id の最大を範囲で引けない。
  - だから探索を固定の件数 `K` で打ち切る（`SELECT MAX(id) FROM (SELECT id, created_at FROM run_events ORDER BY id DESC LIMIT ?K) WHERE created_at <= ?E`。試作の `last_id_within`）。`K` 件の中に `E` 以前に書かれた event があればそれが `U_E` で（`U_E` より後の id は定義から全部 `E` より後に書かれているので、新しい方から最初のものが最大）、上の条件で上限を付ける。無ければ上限を付けず、今のとおり `E` より後の行も全部読む（全件に戻す）。探索の読む数は履歴に依らず `K` 以下で、答えはどちらでも全件と同じ（`vi_a_row_after_the_end_in_time_is_in_the_period_by_id`・`vi_a_clock_turned_back_before_the_read_stops_the_walk_and_reads_every_row`）。`K` は答えを変えず、上限なしに戻る頻度だけを決める（試作は 100。本番の値は 4122 が、時計のずれの間と問い合わせの間に書かれる event の数から決める）。下限の今の形（`RowsFrom` の、外す行の `inputs_through` が範囲の時刻以前の queue の記録以下）は、domain が読んだ event から `U_S` を正しく出せる形なので変えない。上限には同じ形が無い: 外す行が全部 `U_E` より後であることを、`U_E` を知らずに示せる記録が無い。
  - 過去の範囲（`--at` の過去・`--until`・`--compare` の窓）は、同じ形で範囲の後に `K` 件より多く書かれていれば上限なしに戻る。上限で読む量は減らないので「範囲が決まらない」に入れる。別案は event の id の cursor と同じで、store が打ち切らずに `U_E` を 1 つの値として返す集約（`SELECT id FROM run_events WHERE created_at <= ? ORDER BY id DESC LIMIT 1`。service が受けるのは 1 行で、DB は範囲の後の id を全部歩く）を planner が採るかどうか。
  - 下限（今の `RowsFrom::leaves_out` の時刻と id の両方）は今のまま変えない。

### (vii) forecast の採点: does_not_hold

`Context::new` は `score::score(events, &context.marks)` を全件で呼び、`forecast_kpis` は窓の中で終わった sample の誤差と、snapshot から終わりまでの mark の数（`marks=0/1+` の層、`with_marks`・`marks_between` の中央値と最大）を出す。
sample は (snapshot, target) の組で、snapshot は開いている全部の target を持つので、target が開いていた間の全部の snapshot がそれぞれ sample になる。

2 段の読み（試作の `forecast_events`）:

1. KpiQuery・now・設定から決まる範囲 `(S, E]` で、終わり（`task_status_changed` の `completed`・`canceled`、`goal_closed`）を記録時刻で読む。終わりごとに、その target の id で前の終わりを 1 件 seek し、無ければ target の最初の event を 1 件 seek する（snapshot は target ができる前に無い）。
2. 1 から決まる 2 段目の範囲で読む:
   - id が「前の終わり（か最初の event）の最小」から「範囲の終わりの最大」までの `forecast_recorded` の全部。SQLite には snapshot の target の索引が作れない（payload の配列の中で、`json_each` は索引にできない）。Postgres は `GIN (((payload::jsonb)->'tasks') jsonb_path_ops)` と `goals` の同じ形で `@>` で引けるが、引いても 1 target の snapshot は開いていた間の全部なので、読む数は同じ規模になる。
   - 記録の mark と task の計画の変化（`TASK_MARK_KINDS`）: 読んだ snapshot の `triggers_through` の最小より後の id の全部（今まで。`score` は id で snapshot の後とし、有効時刻が snapshot より前に backdate した mark も数える）と、snapshot の `at_secs` の最小から `E` までに効いたか記録されたもの。撤回は名指す mark の索引。
   - snapshot の `at_secs` の最小から `E` までの claim と、claim ごとの id で前の claim と間の起動（(vi) の (c)(d)）。
3. 選んだ event を `marks::marks` と `score::score` にそのまま渡し、範囲で終わった sample と excluded に絞る。

確かめたこと:

- 答え: 全件の samples・excluded と一致した。reopen の前の snapshot は前の終わりで切れ、範囲の前の snapshot の target が範囲で終わらなければ sample にならず、範囲の後の撤回と backdate も数えた（`vii_the_history_before_the_snapshots_is_not_read` ほか 3 件）。終わりの前の終わりが範囲の外なら、それより前の snapshot は全件でもその終わりで切れて範囲の sample にならない。
- 読む量: snapshot より前（範囲の中で終わる target が開く前）の履歴には比例しない（`vii_the_history_before_the_snapshots_is_not_read`）。target が開いてから範囲までの snapshot・claim・mark・計画の変化には比例する（`vii_the_history_between_the_snapshots_and_the_range_is_read`）。snapshot の後に記録された mark と task の計画の変化は範囲の後のものも読む（`vii_the_marks_written_after_the_range_are_read`）。範囲に終わりが無ければ 1 段目で止まる（`vii_a_snapshot_whose_targets_do_not_finish_in_the_range_is_not_read`）。
- 2 段目の範囲は、範囲の中で終わる target のうちいちばん長く開いていたものの開いていた期間で決まる。goal は数週間開くので、既定の `--period` でも 2 段目は数週間の snapshot と claim になる。これは範囲の外の履歴で、KpiQuery からは上限が決まらない。
- 時計: 1 段目の終わりは時刻の索引（`run_events(kind, created_at, id)`）、前の終わりと最初の event は id の seek、mark は (vi) の約束で読むので、時計が戻っても答えは変わらない。前の終わりを id で引くので、時刻が戻った終わりが「snapshot の後の最初の終わり」になる場合も全件と同じになる。

forecast の部分だけを kind で絞った全件に残す形: 読むのは `READ_KINDS`・`forecast_recorded`・`task_status_changed`（`completed`・`canceled` に絞れる部分索引）・`goal_closed`・`TASK_MARK_KINDS`・`run_claimed`（派生 mark の元）の event で、台帳の行の records の本文ではない。
`run_claimed` は今の `Earlier::claims` の読み（範囲の外の run の claim を全部）と同じ量で、`READ_KINDS` は 4135 が `--compare` で全件に残すとした読みと同じ。
足すのは `forecast_recorded` で、その量は 1 件が開いている全部の task と goal の行（id と p50・p90）を持つので「snapshot の数 × 開いている target の数」に比例する。
今の queue 行の全ページ（本番で約 200 行、平均約 14 万 byte、約 27 MB）との比較は本番の snapshot の数が要り、この Spike は本番の DB を読まないので数えていない（4133 か inbox が `events_by_kind('forecast_recorded')` の件数と payload の長さの和を数えれば決まる）。

### (viii) `--compare` の群の終わりの時刻: holds

`Context::finishes` は `everything.runs` の各 run の `finished_event_id` の event の時刻で、`finished_event_id` は `stats::window::ends_run` に当たる run の最初の event（id の順）。
`ends_run` は `run_integrated`・`run_satisfied_closed` に加えて、payload の `status` が `failed`・`interrupted` のどの kind の event も含む（`validation_finished` の失敗も）。
`READ_KINDS` に終わりの event は無い。

読みの形（試作の `finishes_in`）:

- 終わりの部分索引 `run_events(created_at, id) WHERE run_id IS NOT NULL AND task_id IS NOT NULL AND (kind IN ('run_integrated', 'run_satisfied_closed') OR json_extract(payload, '$.status') IN ('failed', 'interrupted'))` で範囲の終わりを時刻で読み、run ごとに同じ条件の `run_events(run_id, id)` の部分索引で最初の終わりを 1 件 seek し、その時刻が範囲の中のときだけ数える。
  - 範囲の中で着地した run の最初の終わりが範囲の前の検証の失敗なら数えない、時計が戻って最初の終わりが範囲の前の時刻なら数えない（`viii_the_finishes_are_the_runs_first_ends_read_by_time`）。
  - 群を mark の種類の全件で作る形（4135 の後の planner の決定）では、群に要る終わりも全期間になる。そのときは同じ部分索引で `SELECT run_id, MIN(id)` を run ごとに 1 行、その event の時刻を読む。量は run の数に比例するが、1 行は run の id と時刻だけで、run の行の records を読まない。
- `--goal`: `Context::counts` と `stats` の `in_goal` は、task の今の goal（`ledger::attributes` の `task_goals`、task の作成の記録と最後の `task_goal_changed`）で絞り、run の時点の goal ではない。終わりの event の `task_id` をこの map で絞れば同じになる（同じ test の goal 7 の場合）。map は今の kpi も窓の attributes から作っているので、新しい読みは要らない。
- `ledger_read_snapshot`: 終わりの読みは `run_events` の索引の読みなので、snapshot の callback が受け取る port（開いた snapshot を共有する）に足せば、範囲の行の読みと一つの状態で読める。port の外の入口から読むと別の状態になる。
- 共有の SQL の置き場: 終わりの条件は `ends_run` と同じ述語なので、ADR-t3982-1 の形どおり store の port の 1 つの読み（例: run の最初の終わりの時刻）と、SQLite と Postgres の共有の SQL に置く。Postgres では `json_extract` を `payload::jsonb ->> 'status'` にする。

`ledger_rows` の `ended_at` は `finished_event_id` の event の時刻と同じでない（`src/domain/ledger.rs::fill_run`・`fill_legacy_run`、`src/domain/ledger/fold.rs::fold_run`）:

- 畳んだ run の `ended_at` は終わりの event の生の時刻ではない。`fold_run` は工程の記録の時刻を手前の工程の終わり（cursor）以上に丸め（`from = record.at.max(cursor)`）、最後が `ended` ならその丸めた時刻、`push_pending` で終わる run は後の `push_finished` の時刻をその工程の始まり以上に丸めた時刻（`at(push).max(from)`）を `ended_at` にする。時計が戻ると、工程（時刻 20）の後に `ended` を時刻 10 で書いた run の `ended_at` は 20 になる。どれも `run_integrated` とは別の event で、時刻も違いうる。
- 検証や resume が `failed` を記録した後に着地した run は、`finished_event_id` が最初の失敗、`ended_at` が最後の終わりになる。
- 失敗して次の手を待つ run は終わりがあるが `ended_at` が無い。legacy の行は `ended_at` を持たない。

だから `ended_at` の列は群の終わりに使えず、終わりは event から読む。

## 読みの約束

4135 の「読みの約束」の表を次で置き換える（範囲 `(S, E]`、時刻は有効時刻）。
索引は多めに拾ってよく、範囲と答えは domain の今の関数が決める（選んだ event をそのまま渡す）。

| 読むもの | 索引と範囲 | 要る索引と migration |
| --- | --- | --- |
| 範囲に効く記録の mark（`RECORDED_KINDS` の queue の event） | 有効時刻が範囲のもの。今に届く範囲でも同じ | 3 本の部分索引の和: `mark_recorded` の `json_extract(payload, '$.at')`、prune の `supervisor_stopped` の `json_extract(payload, '$.last_heartbeat_at')`（秒。範囲は秒に丸めて多めに拾う）、`RECORDED_KINDS` の `(kind, created_at, id)` |
| その中の撤回できる mark の撤回 | 撤回が名指す mark の id | `run_events(kind, json_extract(payload, '$.mark'), id) WHERE kind = 'mark_retracted'` |
| 範囲の claim | 記録時刻が範囲のもの（範囲の run の行の今の読みか、`(kind, created_at, id)`） | 行から読むなら要らない |
| 属性ごとの前の claim | 範囲の claim ごとに、その id より前でその属性を持つ最後の `run_claimed` を 1 件 | 属性ごとの部分索引 5 本（4135 と同じ） |
| 前の claim とその claim の間で新しい値を名乗る起動 | 範囲の claim ごとに 1 件 | 4135 と同じ 2 本 |
| routine の直前の起動 | `S` 以前の時刻で `(created_at, id)` の最後の `supervisor_started` を 1 件 | `run_events(kind, created_at, id)` |
| forecast の終わり | 記録時刻が範囲のもの | `run_events(kind, created_at, id)`（`task_status_changed` は `to` で絞る部分索引にしてもよい） |
| forecast の前の終わりと target の最初の event | 終わりごとに id で 1 件 | `events_by_task`・`events_by_goal` に終わりの部分索引（`(task_id, id)`・`(goal_id, id)`）。最初の event は今の索引 |
| forecast の 2 段目 | 上の (vii) の 2 の全部 | 今の `events_by_kind` と上の索引。範囲の外に比例する |
| run の終わり | 終わりの部分索引で時刻が範囲のもの、run ごとに最初の終わりを 1 件 | (viii) の 2 本 |
| 範囲の行（下限） | 今の `RowsFrom::leaves_out` | 要らない |
| 範囲の行（上限） | `U_E` を新しい方から `K` 件で打ち切る探索で引き、見つかれば最初の id が `U_E` より後で、どの記録の payload の時刻も `E` より後の行を外す。見つからなければ外さない（全件に戻す）。過去の範囲は上限で読む量が減らない（範囲が決まらない） | `ledger_rows(kind, inputs_from)` の条件と records の payload の時刻の条件（今の `record_not_before_sql` の逆）。`started_at` は時計が戻ると records の時刻の最小でない（`fold_run` の始まりは claim と最初の工程から決まる）ので、時刻の条件に使わない |
| event の id の cursor の時刻 | 時刻の降順に `K` 件で打ち切り、cursor 以下の最初の id の時刻。無ければ全件に戻す | `run_events(created_at, id)`（kind を問わない。時刻の text が `RECORD_TIME_FORM` でない event の有無の検出も 4667 で決める） |

表の「範囲の行（上限）」の行は 4670 の時点の記録で、今の約束ではない。Spike 4754 の (iii-d) と約束 7（[earlier-visibility の Spike](ledger-window-earlier-visibility-spike.md)の「追試 (iii) 可視性（Spike 4754）」）と request 294 の planner の決定で、台帳の行に上限を付けない。

索引の式は SQLite と Postgres で同じ値を引くこと（Postgres は `payload::jsonb ->> 'key'`、時刻は text の形 `RECORD_TIME_FORM` のまま比べる）を 4667 で確かめる。
`last_heartbeat_at` は秒の整数で、有効時刻にするときに ms の text に直すが、索引はその式でなく秒の値に張り、範囲を秒に丸めて多めに拾えば、SQLite の式の索引で日付の関数を使わずに済む。

範囲が決まらない問い（全件の読みに残す）:

- event の id を cursor に渡した kpi（`--at ID`・`--since ID`・`--until ID`・`--compare ID`）で、`K` 件の探索が cursor の時刻を見つけないとき（後に多く書かれた過去の id）。(vi) の別案（打ち切らない集約 1 行）を planner が採るなら外せる。
- 台帳の行の上限で、`K` 件の探索が `U_E` を見つけないとき（時計が戻るかずれた履歴、範囲の後に多く書かれた過去の範囲）。範囲の後の行も読む今の形に戻す。過去の範囲（`--at` の過去・`--until`・`--compare` の窓）はふつうこちらになる。同じ別案（打ち切らない `U_E` の集約 1 行）を採るなら外せる。
- forecast の KPI の 2 段目は範囲の外の履歴に比例する（(vii)）。範囲の読みを採るなら「範囲の中で終わる target の開いていた期間」に比例すると約束に書いて受け入れるか、forecast の部分を kind で絞った全件に残すかを planner が決める。後者でも forecast を持つ問い（kpi の期間・`--at`・`--since`/`--until`・`--compare`・report の作成と observer の kpi）は全部 forecast の KPI を出すので、forecast の部分だけが全件になり、ほかの KPI は範囲で読める。
- `--compare` の群（4135 の (v)、変えない）。

## 後続の draft への推奨

この判定は ADR-t1381-1 と ADR-t1662-2 決定 1・7・8 を変えない（event が SSOT のまま、足すのは `run_events` の索引だけ）。
goal の制約（台帳の行の形と `ledger_v` を変えない）にも当たらない。

- 4667（索引の migration）: 上の表の索引を足す。4135 の推奨の「今に届く範囲は (iii) の索引だけで足りる」は時計のずれで崩れるので、今に届く範囲にも有効時刻の 3 本・撤回の 1 本・`(kind, created_at, id)` が要る。索引の段の分け方は、範囲の読み（(vi) の全部）を先に、forecast の前の終わりと run の終わりの部分索引を後にできる。各索引が SQLite と Postgres で `EXPLAIN` で引かれることを test にする。
- 4122（store の範囲の読みと小さな読み）: （行の上限の部分は 4670 の時点の推奨。Spike 4754 の (iii-d) と約束 7 と request 294 の決定で、4122 は上限を付けない。）port に上の表の読みを足し、`ledger_read_snapshot` の callback の port から読む。claim ごとの前の claim と間の起動の seek、`S` 以前の時刻の起動の seek、run の最初の終わりの時刻（範囲と全期間の `MIN(id)` の 2 形）、forecast の終わり・前の終わり・最初の event を足す。行の上限は、`U_E` を `K` 件で打ち切る探索の読み（上の SQL）と「最初の id が `U_E` より後で payload の時刻も `E` より後」の条件で足し、探索が見つけなければ上限を付けない（時刻だけ・`started_at` だけの条件にせず、打ち切らない探索も使わない）。`K` の値を決め、どちらの経路を通ったかを読みの結果に残す。過去の範囲の上限を打ち切らない集約で付けるのは、別案を planner が採ったときだけ。`Earlier::claims` を派生 mark の目的で読むのをやめ、seek に置き換える（`Earlier::claims` に残る読み手があれば別の問い）。
- 4132（domain の範囲の関数）: 試作の `range_events`・`forecast_events`・`finishes_in` の形にする。event の id の cursor は `K` 件で打ち切る時刻の探索で時刻を引き、見つからなければ全件に戻す（今の `read_from_ms` の「`--at` の id なら全件」を、この探索の結果で決める形にし、`--since`・`--until`・`--compare` の id にも同じ形を当てる）。範囲が決まらない条件には、この探索と行の上限の `U_E` の探索が見つけなかったときを足す。(5) の「時計の前提の崩れを検出できなければ受け入れる」を外す: 時計の崩れは約束の読みで答えが変わらないか、`K` 件の探索で検出して全件に戻すので、受け入れる場合は残らない。forecast の扱いは上の 2 つの選択肢から planner が決めた方を書く。
- 4133（kpi の配線）: `--compare` の群に渡す終わりを `finishes_in`（全期間なら run ごとの `MIN(id)` の 1 行）で読み、`--goal` は今の窓の attributes の `task_goals` で絞る。受け入れの比較（全件と同じ答え）に forecast の KPI（`forecast.*` の層と `details` の `with_marks`・`marks_between`）と、時計が戻った履歴（試作の 6 つの場合）を入れる。
- 4134（report の配線）: `--at` の無い report も今に届く範囲で索引が要る（4135 の推奨を直す）。forecast の KPI は 4133 と同じ扱いにする。push の範囲の読み（task 4589）にそろえる点は変えない。
