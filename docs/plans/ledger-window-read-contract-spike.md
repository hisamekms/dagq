---
id: plan-ledger-window-read-contract-spike
type: plan
title: スパイク：kpi・report の台帳の窓を範囲で読むとき、mark の有効時刻・撤回・派生 mark の直前の値・routine の直前の起動を全件と同じに保つ読みの約束と、読む量を範囲の外の履歴に依らせない停止の根拠が今のコードで立つか
status: completed
created: 2026-11-09
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - design-measurement
  - plan-kpi-read-memory-spike
  - plan-ledger-queue-rows-readers-spike
---

# スパイク：台帳の窓を範囲で読むときの mark の読みの約束と停止の根拠

問いは「kpi（`--period`・`--at`・`--since`/`--until`・`--compare` の At と Windows）と report の台帳の窓を、範囲の外の確定した履歴に比例しない量の読みだけで、全件を読む形と同じ答えにできるか」。
先行状態 (i)〜(v) ごとに、何をどの索引と範囲で読めば全件と同じになるか（読みの約束）と、どこで読むのを止めてよいか（停止の根拠）を、コードの調査と in-memory の試作で確かめた。

判定は **does_not_hold**（下の「判定」）。
(iv) は今のコードで約束が立つ。
(i) と (ii) は範囲が今に届く読み（既定の `--period`・`--at` の無い report）では今の索引で立つが、過去の範囲（`--at` の過去・`--until`・`--compare`）では範囲の後に記録された event を全部読む。
(iii) は範囲が今に届いても立たない。
(i)〜(iii) は `run_events` に索引を足す互換の migration（event の形・台帳の行の形・`ledger_v` は変えない）で立ち、それを試作の test が確かめた。
(v)（`--compare` だけが使う群）は、終わった run の本数で探索を止められるが、止める所から群の最初の mark までの slice の claim と routine の mark を時刻で読み、その数は範囲の外の履歴に依る。索引を足しても立たない。

## 環境

- 対象の commit: `f360fe0760b25b7e7e5f6f21b0b2af70763114f5`（この run の base）。
- host: Apple M1、macOS 27.2（26B123）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、cargo-nextest。
- provider と経路: 本番 queue の Spike の run（task 4135）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。本番の queue と DB は読んでいない（試作は合成の event だけを使う）。
- 使った時間と回数: 上限 3 時間に対して約 1 時間 40 分。試作は 1 回書いて 7 件の test が初回で通った。subagent の review の指摘（履歴の始まりの判定、announce の読みが要る場合の test）で 1 回直して 8 件にした。run の review の指摘（slice の下端の排他、時刻で広げる探索が群を分ける終わりより先を読むこと）で、群の探索を終わりの本数で止める形に作り直して 11 件にした。作り直しは 2 回（上限 3 回）。

## 読んだコードと打ったコマンド

読んだコード:

- `src/domain/marks.rs`: `READ_KINDS`・`RECORDED_KINDS`・`RETRACTABLE_KINDS`・`marks`/`marks_with_earlier`・`retractions`・`derived_marks`・`DERIVED`・`claim_value`・`cursor_millis`。
- `src/domain/kpi/compare.rs`: `splitting`・`routine`・`classified`・`overlapping_groups`・`named_change`・`compare`。
- `src/domain/kpi/window.rs`（`Context::new` が `marks` と `finishes` を作るところ）、`src/domain/kpi.rs`（`read_from_ms`・期間ごとの `marks_with_earlier`）。
- `src/domain/ledger/window.rs`（「A read from a time」・`Earlier`・`EARLIER_RUN_KINDS`）、`src/application/ports/observation.rs::ledger_window`、`src/infrastructure/runtime_store/ledger.rs::earlier_in`。
- mark を書く経路: `src/application/marks.rs`（`record_mark`・`retract_mark`）、`src/application/lifecycle.rs::prune_supervisor`。
- 索引: `migrations/` の `run_events` の索引（`events_by_task`・`events_by_goal`・`events_by_kind(kind, id)`・`events_by_run`・`events_by_opened_event`・`events_by_supervisor`）。`created_at` の索引は無い。

打ったコマンド:

```sh
git rev-parse HEAD
grep -rn "INDEX.*run_events" migrations
sh scripts/cargo-brief.sh cargo nextest run --locked --lib -E 'test(/read_contract/)'
sh scripts/cargo-brief.sh cargo fmt --all --check
sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings
```

## 記録時刻と有効時刻の差

- `dagq mark --at` は過去の時刻だけを受ける（`record_mark` が今より後を拒む）。下限は無い。
- prune の `supervisor_stopped` の `last_heartbeat_at` は消した登録の最後の heartbeat で、登録が残っていた間の長さに上限は無い（次の `up`・`down` か、生きている supervisor の掃除まで残る）。
- ほかの記録の mark は記録時刻が有効時刻。

よって、差に上限は無いが向きは一つで、有効時刻は記録時刻を超えない。
ただし時計は一つでない: `--at` は CLI の process の時計で確かめ、`created_at` は DB（SQLite の `strftime('now')`、Postgres では DB server）が書き、`last_heartbeat_at` は supervisor の時計が書く。
この向きは、時計が戻らず、これらの時計のずれが無視できるときに限る。
範囲 `(S, E]` に効く mark は、`S` より後に記録されている。

## 試作

`src/domain/kpi/compare/read_contract.rs`（`#[cfg(test)]` の未配線の module。`compare.rs` が `#[cfg(test)] mod read_contract;` で持つ）。

- `Reads` は store の索引のある読みを in-memory で表し、返した event（と終わった run の時刻）の数を数える。`existing_*` は今の索引で読める形、ほかは今の migration に無い索引を要る形で、要る索引をそれぞれの doc comment に書いた。
- `range_events` が範囲の読みの約束で、選んだ event をそのまま `marks::marks` に渡す。答えは domain の関数が全件のときと同じ規則で決める。
- `classified_in` は直前の起動を足して `compare::routine` に渡し、`groups_touching` は群の端から `min_samples` 本目の終わりまで slice を広げながら `compare::overlapping_groups` と同じ規則で群を作る。
- 各 test は範囲の外の確定した履歴（10 分ごとの run、5 本ごとに routine の起動、10 本ごとに mark とその撤回）を 30 単位と 3000 単位で作り、全件の答えとの一致と、読んだ数の一致（または今の索引での増え方）を比べる。

| test | 場合 | 比べたもの |
| --- | --- | --- |
| `i_a_backdated_mark_is_read_by_its_effective_time_whatever_the_history` | 範囲の後に記録して範囲へ遠く backdate した mark、範囲の後に記録した prune の停止、範囲の中に記録して遠い過去へ backdate した mark | 全件と一致、読んだ数は 30 単位と 3000 単位で同じ |
| `i_the_written_time_misses_a_backdated_mark_or_reads_what_follows` | 同じ（今の索引） | 範囲の記録時刻では backdate を落とし、範囲から今までなら読む数が 10 倍を超えて増える |
| `ii_a_later_retraction_is_read_by_the_mark_it_names` | 範囲の mark を範囲の後に撤回、範囲の後にほかの撤回も多数 | 全件と一致、読んだ数は同じ。今の索引（mark の後の撤回を全部）は 10 倍を超えて増える |
| `iii_each_attributes_last_value_is_read_however_old` | `codex_version` を欠いた claim が長く続いた後の新しい値、範囲の手前の起動が名乗る新しい build（その後に別の build の起動があり、名乗った起動は直前の起動でない） | 全件と一致、読んだ数は同じ。今の索引（claim を遡る）は 10 倍を超えて増える |
| `iv_routine_reads_the_last_start_before_the_range` | 範囲の外の直前の起動の `parallel` が同じ・違う | 全件の routine の判定と一致、読んだ数は同じ |
| `v_the_search_back_reaches_a_mark_backdated_before_the_history` | 履歴の最初の event より前へ backdate した mark と範囲の mark の群 | 全件の群と一致、読んだ数は同じ |
| `v_a_mark_at_the_first_time_of_the_history_is_read` | 履歴の最初の時刻の mark と範囲の mark、間に終わりが無い | 全件の群と一致（slice の下端の排他）、読んだ数は同じ |
| `v_claims_before_the_finishes_that_close_the_group_are_not_read` | 範囲 (10 分, 11 分] の 10.5 分の mark、9.3〜9.7 分の `min_samples` 本の終わり、9.1〜9.2 分の終わっていない claim を 30 件と 3000 件 | 全件の群と一致、読んだ数は同じ |
| `v_claims_between_the_finishes_and_the_group_are_read` | 同じで、終わっていない claim が 9.8〜10.4 分（終わりと群の最初の mark の間） | 全件の群と一致、読んだ数は 10 倍を超えて増える |
| `v_a_group_is_found_whole_and_the_search_stops_at_min_samples` | 窓の前後へ連なる 6 個の mark、隣との間の終わった数が `min_samples`−1・`min_samples` | 全件の群と一致（1 群と各自）、読んだ数は同じ |
| `v_a_backdated_mark_and_a_later_retraction_change_the_group_read` | slice の後に記録して群の間へ backdate した mark、群の mark の後日の撤回 | 全件の群と一致、読んだ数は同じ |

比べなかった場合: `--goal` で終わった数を goal の run だけで数える群（下の (v) の条件）と、時計が戻った履歴は試作に入れていない。理由はそれぞれの節に書いた。

## 判定

### (i) 有効時刻による mark の選択: does_not_hold

- 根拠: `marks::marks_with_earlier` は `mark_recorded` の `payload.at` と prune の `last_heartbeat_at` を有効時刻にし、`record_mark` と `prune_supervisor` はどちらにも下限を付けない。今の索引は `events_by_kind(kind, id)` だけで、範囲の記録時刻で読むと backdate した mark を落とす（`i_the_written_time_misses_a_backdated_mark_or_reads_what_follows`）。
- 有効時刻は記録時刻を超えないので、`S` から今までに記録された mark の種類を読めば落とさない。範囲が今に届く読み（既定の `--period`、`--at` の無い report）ではこれで立つ。過去の範囲では範囲の後の履歴に比例する（同じ test が 10 倍を超えて増えるのを見た）。
- 有効時刻で引ける索引を足せば立つ（`i_a_backdated_mark_is_read_by_its_effective_time_whatever_the_history`）。

### (ii) 範囲の mark の撤回: does_not_hold

- 根拠: `marks::retractions` は全件の `mark_retracted` から mark ごとに最初の撤回を当て、撤回は範囲の後に記録されうる。今の索引では mark の後の `mark_retracted` を全部読む（`ii_a_later_retraction_is_read_by_the_mark_it_names` の後半）。
- 範囲が今に届く読みでは、範囲の mark の撤回は範囲の中に記録されているので立つ。
- 撤回が名指す mark の id で引く索引（`events_by_opened_event` と同じ形）を足せば、範囲の撤回できる mark の数だけ読めば立つ（同じ test の前半）。
- 条件: 撤回できる種類（`RETRACTABLE_KINDS`）の mark の撤回だけを引く。`marks::retractions` はどの id を名指す撤回も当てるが、書く経路の `retraction_payload` が撤回できない種類を拒むので同じになる。

### (iii) 派生 mark の属性ごとの直前の値と間の起動: does_not_hold

- 根拠: `derived_marks` は属性ごとに、その属性を記録した直前の claim と比べる。Codex を動かさない supervisor の claim は `codex_version` を持たず、dagq でない repository の claim は `rustc_*` を持たない。直前の値は任意に古く、今の索引では claim を遡って数えるしかない（`iii_each_attributes_last_value_is_read_however_old` の後半）。範囲が今に届いても同じ。
- 起動が名乗った変化（`dagq_version`・`parallel`）は、直前の claim と範囲の最初の claim の間の `supervisor_started` を見る。間の長さも直前の claim の古さに従う。
- 属性ごとに部分索引（その属性を持つ `run_claimed` の id）を足し、直前の claim を 1 件の seek で得て、間の起動を名乗る値で引く索引を足せば立つ（同じ test の前半）。

### (iv) routine の判定の直前の起動: holds

- 根拠: `compare::routine` が要るのは時刻順で直前の `supervisor_started` の `parallel` だけで、起動は撤回されない（`RETRACTABLE_KINDS` に無い）ので `splitting` で抜けない。今の `events_by_kind(kind, id)` を範囲の最初の id から逆に 1 件引けばよい（`iv_routine_reads_the_last_start_before_the_range`）。
- 条件: 記録時刻が id の順に減らない（時計が戻らない）こと。戻った履歴では id で直前の起動と時刻で直前の起動が食い違いうる。台帳の「A read from a time」が時刻と id を互いに追わないとしているのと同じ前提の違いで、試作には入れていない。

### (v) 群を辿る探索の停止: does_not_hold

群は `--compare`（At の名指す変化と Windows・At の `overlapping`）だけが使い、期間の KPI と report は使わない。

- 終わりの本数で止める所は立つ。`overlapping_groups` では、群の最初の mark `f` の前の mark `p` は、`(p, f]` の終わりが `min_samples` 本より少ないときだけ群に入る。だから入りうるのは、`f` から遡って `min_samples` 本目の終わりより後の mark だけになる。終わりを時刻の降順に `min_samples` 本だけ読めばその位置が分かり、そこまで slice を広げる。群が広がったら新しい最初の mark から繰り返す。後ろも同じで、`min_samples` 本目の終わりの手前まで読む。終わりがそれより少なければ、履歴の始まりか今まで読む。
  - 履歴の始まりは、最初の event の時刻と mark の有効時刻の最小のうち早い方にする（`--at` に下限が無いので、最初の event より前に効く mark がありうる）。slice の下端は排他にする（`v_the_search_back_reaches_a_mark_backdated_before_the_history`・`v_a_mark_at_the_first_time_of_the_history_is_read`）。
  - slice の外で記録された backdate の mark と後日の撤回は、(i) と (ii) の約束で読めば slice に入る（`v_a_backdated_mark_and_a_later_retraction_change_the_group_read`）。
  - 隣との間が `min_samples`−1 なら群は窓の前後へ連なり、`min_samples` なら切れる（`v_a_group_is_found_whole_and_the_search_stops_at_min_samples`）。
  - `min_samples` 本目の終わりより前は読まない（`v_claims_before_the_finishes_that_close_the_group_are_not_read`）。
- 立たないところ: その slice の mark を知るには、slice の中の claim（派生 mark はそこから読む）と routine の mark（起動・停止）を時刻で読む。その数は終わりの本数で制限されない。終わっていない run の claim が多い隙間では、読む数が隙間の claim の数に比例する（`v_claims_between_the_finishes_and_the_group_are_read`）。
  - コードは隙間の claim の数を制限しない（同時に走る run は slot の数までだが、終わりに数えない終わり方の run の claim がどれだけ続くかは、コードからは決まらない）。
  - (iii) の索引は属性ごとの直前の値を与えるだけで、slice の中で値が変わった claim は与えない。
- `--goal` の条件: 終わった数は goal の run だけで数える（`Context::counts`）。goal の最初の run より前は終わりが 0 なので、そこまでの routine でない mark は全部一つの群に入り、答えの群そのものが履歴の始まりまで届く。試作には入れていない。
- 代わり: 下の「全件の読みに残す問い」の `--compare` の項。

## 読みの約束

範囲 `(S, E]`（有効時刻）の mark の読み:

| 読むもの | 索引と範囲 | 要る索引と migration |
| --- | --- | --- |
| 範囲に効く記録の mark（`RECORDED_KINDS` の queue の event） | 記録時刻が範囲のもの、`mark_recorded` の `payload.at` が範囲のもの、prune の停止の `last_heartbeat_at` が範囲のもの | `run_events(kind, json_extract(payload, '$.at'))`（`mark_recorded`）と `run_events(kind, json_extract(payload, '$.last_heartbeat_at'))`（`supervisor_stopped`）の部分索引。索引は多めに拾ってよく、範囲は `marks::marks` が今の規則で決める。範囲が今に届く読みだけなら要らない（`S` から今までに記録されたものを `events_by_kind` で読む） |
| その中の撤回できる mark の撤回 | 撤回が名指す mark の id | `run_events(kind, json_extract(payload, '$.mark'))`。範囲が今に届く読みだけなら要らない |
| 範囲の claim | 範囲の run の行（今の `RowsFrom` の読み） | 要らない |
| 属性ごとの直前の claim | 範囲の最初の id より前で、その属性を持つ最後の `run_claimed` を 1 件 | 属性ごとの部分索引 5 本（`dagq_version`・`claude_version`・`codex_version`・`parallel`、toolchain は `rustc_release` と `rustc_host` の両方） |
| 直前の claim と範囲の最初の claim の間で新しい値を名乗る起動 | `supervisor_started` の名乗る値と id の範囲で 1 件 | `run_events(kind, json_extract(payload, '$.dagq_version'), id)` と `parallel` の同じ形 |
| 直前の起動 | `events_by_kind('supervisor_started', id)` を範囲の最初の id から逆に 1 件 | 要らない |

選んだ event は `marks::marks` にそのまま渡し、答えの規則は domain の今の関数が持つ（試作の `range_events`）。

群の読み（`--compare`）は約束にならない（(v)）。
試作の `groups_touching` の終わりの本数で止める探索は、終わりの読み（終わりの時刻の索引で降順・昇順に `min_samples` 本）を有限にする。だが slice の claim と routine の mark の読みは残る。
`Context::new` の `finishes` は run の終わりの event の時刻なので、終わりの読みは run の行の終わりの時刻でなく、その event の時刻で数える（同じになるかは 4122 で確かめる）。

全件の読みに残す問い:

- `--compare`（At と Windows、`--goal` つきも）: 群は今のとおり全件で作る。そのための読みは、mark の種類（`READ_KINDS`）の全件にする。今の queue 行と `Earlier::claims` の読みと同じ量で、run の行の records の全部ではない。
  - 窓の KPI 自体は、群が決めた範囲（At では `(first − window, last + window]`）を (i)〜(iv) の約束で読む。
  - 別案は 2 つあり、どちらも planner の判断が要る。一つは、派生 mark の値が変わった claim を時刻で引ける形（派生 mark のビューの表か、claim ごとの値の変わりの列）を足す案で、goal の制約（台帳の行の形を変えない）に当たるかを見る。もう一つは、隙間の claim は slot の数で実際には少ないとして、終わりの本数で止める探索を受け入れる案。
- 時計が戻った履歴と時計のずれ: 約束は記録時刻が id の順に減らないことと、「記録時刻と有効時刻の差」の節の時計のずれが無視できることを前提にする。前提が崩れたときの扱いは 4132 で決める（全件に戻すか、受け入れるか）。

約束の外で見つけたもの: `forecast::score::score` は forecast の snapshot から run の終わりまでの mark を数え、snapshot は範囲より前にありうる。この Spike は調べていない。範囲の mark だけを渡すと答えが変わりうるので、4132 で読みの範囲に入れるか確かめる。

## 後続の draft への推奨

この判定は ADR-t1381-1 と ADR-t1662-2 決定 1・7・8 を変えない（event が SSOT のまま、足すのは `run_events` の索引だけ）。

- 分け方: 索引を足す互換の migration（SQLite と Postgres の両方、`migrations.md` の規則）を 4122 の前の独立した task にするか、4122 の最初の段にする。範囲が今に届く読み（既定の `--period`、`--at` の無い report の作成・observer の kpi・push の判定）は (iii) の索引だけで足り、過去の範囲（`--at`・`--until`・`--compare`）は全部の索引が要る。今に届く読みを先に着地させ、過去の範囲を後の段にすると、索引を 2 段に分けられる。
- 4122（store の範囲の読みと小さな読み）: port に上の表の小さな読み（試作の `Reads` の索引のある読みの形）を足す。queue 行を全部読む今の読みは、mark の目的では上の約束に置き換える（queue 行のほかの読み手は別の問いで、この Spike は調べていない）。`Earlier::claims`（範囲の外の run の claim を全部）は、派生 mark の目的では属性ごとの直前の claim 5 件に縮められる。
- 4132（domain の範囲の関数）: 試作の `range_events`・`classified_in` の形にし（`groups_touching` は (v) の別案を planner が選んだときだけ）、答えの規則は今の `marks::marks`・`compare::routine`・`compare::overlapping_groups` に任せる（関数を作り直さない）。`forecast::score::score` の要る mark の範囲と、時計が戻った履歴の扱いをここで決める。
- 4133（kpi の配線）: `--period`・`--at`・`--since`/`--until` は (i)〜(iv) の約束で読む。`--compare` は群を mark の種類の全件で作り、窓の KPI だけを群が決めた範囲で読む形に縮める。`--compare` を別の段（または別の task）に分けると、(v) の別案の判断を待たずに残りを進められる。
- 4134（report の配線）: `--at` の無い report の窓は今に届くので、(i)・(ii) は今の索引で足り、(iii) の索引だけが要る。`--at` が過去の report は kpi の `--at` と同じく全部の索引が要る。push の範囲の読み（task 4589）の形にそろえる。
