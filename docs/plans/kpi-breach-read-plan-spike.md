---
id: plan-kpi-breach-read-plan-spike
type: plan
title: スパイク：record_kpi_breach の lock の下の読み（breach_events）は、run の無い別の kind の event が増えると走査が増える plan を選ぶか、候補の直しで breach の event の数に抑えられるか
status: completed
created: 2026-10-30
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - plan-codegen-units-spike
---

# スパイク：breach_events の読みの plan

問いは「`src/infrastructure/kpi_push.rs` の `breach_events` の文（`kpi_breaches_open`・`kpi_breach_events_open`・`record_kpi_breach` が使う）と `record_kpi_push_abandoned` の同じ形の文は、migration を最後まで当てた今の schema で sqlite_stat1 の無い DB について、run の無い別の kind の event が増えると走査する行が増える plan を選ぶか。選ぶなら、候補の直しのどれが走査を breach の event の数に抑えるか」。

判定は **holds**（下の「判定」）。
今の文は `events_by_run` ではなく `events_by_goal`（`goal_id IS NULL`）に沿って goal の無い event を全部たどり、VmStep の比は 3,637 だった。
`+run_id` だけの直しは plan を変えず、`INDEXED BY events_by_kind` と `+run_id`・`+task_id`・`+goal_id` の全部は、N によらず VmStep 251（比 1.0）に抑えた。

## 環境

- 対象の commit: `c389edc92125d843f1d9a343c98a69dedb564b47`（この run の base）。測るコードはこの上の run の commit に入れた。
- sqlite: rusqlite 0.40.2 の `bundled`（libsqlite3-sys 0.38.2、SQLite 3.53.2）。
- host: Apple M1（8 コア）、メモリ 16 GiB、macOS 14.7.4。
- provider と経路: 本番 queue の Spike の run（task 3690）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。本番 queue の DB は開いていない。
- 時刻: 2026-10-30 03:48:51〜03:50:10（JST）。

## 条件

- DB: test の `$TMPDIR` の下に `SqliteQueue::init` で作った queue（migration を最後まで当てたもの）。ANALYZE も PRAGMA optimize も打たず、sqlite_stat1 は無い。
- event の入れ方（N ごとに 1 度作り、閉じたものをひな形にする）:
  - run・task・goal の無い breach の event 11 件（`kpi_breach_started` と `kpi_breach_resolved` を交互に 5 組と、開いたままの start 1 件）と、`kpi_push_sent` 1 件。id の範囲に等間隔に散らした。
  - run・task・goal の無い別の kind の event 約 N 件（`supervisor_heartbeat`・`queue_write_held_long`・`kpi_daily`）。
  - run・task・goal の付いた event（`turn_requested`）約 N 件。
  - payload は breach が本番と同じ鍵（period・kpi・stratum）に `pushed`・`day` を足した小さな JSON（resolved にも付けた。本番は start だけ）、ほかは 160 byte ほどの文字列の JSON。
  - 入れるときだけ foreign key を切った（run・task・goal の行は作っていない）。
- 文の候補（`kind IN (?1, ?2)` の 2 つの kind は今の文と同じ定数を bind した）:
  - (i) 今の文。
  - (ii) `+run_id IS NULL AND task_id IS NULL AND goal_id IS NULL`。
  - (iii) `+run_id IS NULL AND +task_id IS NULL AND +goal_id IS NULL`。
  - (iv) `FROM run_events INDEXED BY events_by_kind`（WHERE は今の文と同じ）。
  - `record_kpi_push_abandoned` の文（`... ORDER BY id DESC LIMIT 1`）は (i)・(ii)・(iv)。
- 文の読みは、`SqliteQueue::open` で schema を確かめて閉じた後、同じファイルを開いた素の rusqlite の接続で流した（`SqliteQueue` の接続が足すのは busy timeout・foreign key・commit の hook で、plan には関わらない）。`record_kpi_breach` は `SqliteQueue` で呼んだ。
- 1 回の読み: `EXPLAIN QUERY PLAN` の detail の列を取り、続けて `prepare`（cache しない）から `Rows::next` で全行を最後まで step し各行の列 0 を読み終えるまでの時間を測り（行ごとの誤りは test を落とす）、その statement の `get_status`（`VmStep`・`FullscanStep`・`Sort`）を取った。
- `record_kpi_breach` は呼び出しの全体（`BEGIN IMMEDIATE` から commit まで）の時間を測った。`push_day` を渡したので、新しい start は `breach_events` を 2 回読む（`open_in` と当日の数え）。
- 使った時間: 実装と測定と文書で約 40 分（上限 90 分）。候補は description の 4 つ（足した候補は無し）。表は測るコードを `tests/it` に移し、各行を最後まで step して列 0 を読み行ごとの誤りで落ちるようにした後の測定で、外れた周も流し直しも無い（その前の測定は abandoned の文で無い列を読んで誤りを行に数えていたので捨てた。plan と VmStep は同じだった）。

## 打ったもの

```sh
# 常に流す test（N = 0・50 で全部の文と 2 経路を流し、読んだ行数を確かめる）
sh scripts/cargo-brief.sh cargo test --locked --test it queue_kpi_breach_read_plan::
# Spike の測定（N = 0・10,000・100,000）
cargo test --locked --test it queue_kpi_breach_read_plan::the_breach_reads_are_measured_at_the_spike_sizes -- --ignored --nocapture
```

test は `tests/it/queue_kpi_breach_read_plan.rs` の `the_breach_reads_are_measured_at_a_small_size`（常に流す）。
Spike の測定は、同じ file の `#[ignore]` の test `the_breach_reads_are_measured_at_the_spike_sizes`（`run(&[0, 10_000, 100_000])`）を上の 2 つ目のコマンドで流した。
その test は 1 本が数十秒かかり、`tests/it` の時間の関門に許可の項目を足す理由（[testの制約](../development/testing.md)の「判断と境界のtest」）に当たらないので、着地する tree からは外した。
測り直すには、常に流す test の `run(&[0, 50])` を `run(&[0, 10_000, 100_000])` に変えて `-- --nocapture` で流す（行数の確かめはどの N でも当たる）。

- 周と順: 各 N で 3 周。文の候補は周ごとに順を 1 つずつずらした（breach は 1 周目 (i)→(ii)→(iii)→(iv)、2 周目 (ii)→(iii)→(iv)→(i)、3 周目 (iii)→(iv)→(i)→(ii)。abandoned は 1 周目 (i)→(ii)→(iv)、2 周目 (ii)→(iv)→(i)、3 周目 (iv)→(i)→(ii)）。各周の最初に、その周の先頭の候補で結果を捨てる読みを 1 回流した。test は周ごとの順を出力に書き、3 つの N のどれも上の順だった。
- 3 周の plan と VmStep が同じことを test が確かめた（違えば落ちる）。FullscanStep・Sort・行は 1 周目の値。表の時間は 3 周の中央値。
- `record_kpi_breach` の 2 経路は各 N の各周で交互に流し、1 周目と 3 周目は読みだけの経路が先、2 周目は新しい start が先にした。
- 新しい start は、各周でひな形のファイルの複製（`-wal` の無い閉じたひな形を `std::fs::copy`）を開いて測り、測った後に閉じて消した。
- 読みだけの経路は、ひな形とは別に、測る metric の breach を 1 つ開いた複製で測った。
- 周の前後の確かめ（どれかが外れれば test が落ちる）: 新しい start は前に対象の metric の breach が開いていないこと、後に返り値が `Some`（rows 1）でその breach が開いたこと。読みだけの経路は前に対象の breach が開いていること、返り値が `None`（rows 0）。3 つの N の 9 周のどれも全部が当たり、測り直した周は無い。

## 表

plan は `EXPLAIN QUERY PLAN` の detail（`;` で行を区切った）。「行」は読んだ行の数。

| 文 | N | plan | VmStep | FullscanStep | Sort | 行 | 時間 ms の中央値 |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| breach (i) | 0 | SEARCH USING INDEX events_by_goal (goal_id=?) | 165 | 0 | 0 | 11 | 0.028 |
| breach (ii) | 0 | SEARCH USING INDEX events_by_goal (goal_id=?) | 165 | 0 | 0 | 11 | 0.027 |
| breach (iii) | 0 | SEARCH USING INDEX events_by_opened_event (kind=?); USE TEMP B-TREE FOR ORDER BY | 251 | 0 | 1 | 11 | 0.041 |
| breach (iv) | 0 | SEARCH USING INDEX events_by_kind (kind=?); USE TEMP B-TREE FOR ORDER BY | 251 | 0 | 1 | 11 | 0.035 |
| breach (i) | 10,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 60,166 | 0 | 0 | 11 | 8.952 |
| breach (ii) | 10,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 60,166 | 0 | 0 | 11 | 8.890 |
| breach (iii) | 10,000 | SEARCH USING INDEX events_by_opened_event (kind=?); USE TEMP B-TREE FOR ORDER BY | 251 | 0 | 1 | 11 | 0.094 |
| breach (iv) | 10,000 | SEARCH USING INDEX events_by_kind (kind=?); USE TEMP B-TREE FOR ORDER BY | 251 | 0 | 1 | 11 | 0.053 |
| breach (i) | 100,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 600,166 | 0 | 0 | 11 | 93.261 |
| breach (ii) | 100,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 600,166 | 0 | 0 | 11 | 97.637 |
| breach (iii) | 100,000 | SEARCH USING INDEX events_by_opened_event (kind=?); USE TEMP B-TREE FOR ORDER BY | 251 | 0 | 1 | 11 | 0.129 |
| breach (iv) | 100,000 | SEARCH USING INDEX events_by_kind (kind=?); USE TEMP B-TREE FOR ORDER BY | 251 | 0 | 1 | 11 | 0.057 |
| abandoned (i) | 0 | SEARCH USING INDEX events_by_goal (goal_id=?) | 23 | 0 | 0 | 1 | 0.020 |
| abandoned (ii) | 0 | SEARCH USING INDEX events_by_goal (goal_id=?) | 23 | 0 | 0 | 1 | 0.020 |
| abandoned (iv) | 0 | SEARCH USING INDEX events_by_kind (kind=?); USE TEMP B-TREE FOR ORDER BY | 49 | 0 | 1 | 1 | 0.028 |
| abandoned (i) | 10,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 5,021 | 0 | 0 | 1 | 0.647 |
| abandoned (ii) | 10,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 5,021 | 0 | 0 | 1 | 0.679 |
| abandoned (iv) | 10,000 | SEARCH USING INDEX events_by_kind (kind=?); USE TEMP B-TREE FOR ORDER BY | 49 | 0 | 1 | 1 | 0.045 |
| abandoned (i) | 100,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 50,021 | 0 | 0 | 1 | 7.975 |
| abandoned (ii) | 100,000 | SEARCH USING INDEX events_by_goal (goal_id=?) | 50,021 | 0 | 0 | 1 | 8.488 |
| abandoned (iv) | 100,000 | SEARCH USING INDEX events_by_kind (kind=?); USE TEMP B-TREE FOR ORDER BY | 49 | 0 | 1 | 1 | 0.064 |

`record_kpi_breach` の transaction（今の文のまま）:

| 経路 | N | 行 | 時間 ms の中央値 |
| --- | ---: | ---: | ---: |
| 読みだけ（すでに開いた breach の start） | 0 | 0 | 0.158 |
| 新しい start | 0 | 1 | 0.979 |
| 読みだけ | 10,000 | 0 | 8.788 |
| 新しい start | 10,000 | 1 | 20.249 |
| 読みだけ | 100,000 | 0 | 116.020 |
| 新しい start | 100,000 | 1 | 231.416 |

### N=100,000 ÷ N=0 の VmStep の比

| 文 | 比 | 抑えられたか（比 2 未満） |
| --- | ---: | --- |
| breach (i) | 3,637 | いいえ |
| breach (ii) | 3,637 | いいえ |
| breach (iii) | 1.0 | はい |
| breach (iv) | 1.0 | はい |
| abandoned (i) | 2,175 | いいえ |
| abandoned (ii) | 2,175 | いいえ |
| abandoned (iv) | 1.0 | はい |

## 判定

**holds**。根拠は表の行:

- 今の文（breach (i)）は N によらず `events_by_goal (goal_id=?)` を選び、`goal_id IS NULL` の event を全部たどってから table の行で kind・run_id・task_id を確かめる。run の無い別の kind の event が N 件増えると VmStep が約 6×N 増え（165 → 60,166 → 600,166）、時間も 0.028 → 9.0 → 93 ms と N とともに増えた（VmStep はほぼ N に比例し、時間は 10,000 から 100,000 で約 10.4 倍）。`record_kpi_push_abandoned` の文（abandoned (i)）も同じ index を選び、`ORDER BY id DESC LIMIT 1` でも最後の attention より後の goal の無い event を全部たどる（23 → 50,021）。この比の大きさは、ひな形が最後の `kpi_push_sent` を id の範囲の 11/12 の位置に置いたことによる（その後ろの約 1/12 だけをたどった）ので本番に読み替えられず、言えるのは「抑えられない」ことだけ。
- `record_kpi_breach` の読みだけの経路の時間（116 ms）は breach (i) の 1 回（93 ms）に近く、新しい start（231 ms）はその約 2 倍で 2 回の読みとそろう。transaction の時間はこの読みが占めた。
- 走査は `events_by_run` ではなく `events_by_goal` に沿うので、(ii) の `+run_id` は plan も step も変えない（breach (ii)・abandoned (ii) は (i) と同じ行）。planners.rs・sessions.rs の `+run_id` の先例は、この文には効かない。
- (iii)（3 列とも `+`）と (iv)（`INDEXED BY events_by_kind`）は、どの N でも VmStep 251・Sort 1（breach）と 49・Sort 1（abandoned）で、比は 1.0。走査は breach（か attention）の kind の event の数に抑えられた。(iii) は stat の無いときに `events_by_opened_event`（先頭の列が kind）を選び、(iv) は `events_by_kind` を名指しで使う。

本番の規模への読み替え（測っていない）: 本番では task の付いた event も goal_id が NULL のことが多いと考えられ、そうなら今の文は約 107 万件の多くをたどる。この test の DB では run・task の付いた event に goal_id を入れたので、たどったのは run の無い event だけだった。

## 後続の計画への推奨

task 3689 は (a)（breach_events を kind の index に乗せる）を、次の形で書き直して submit するのがよい。

- 文の書き方: `SELECT id, kind, payload FROM run_events INDEXED BY events_by_kind WHERE kind IN (?1, ?2) AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL ORDER BY id`（(iv)）。`record_kpi_push_abandoned` の文も同じに `INDEXED BY events_by_kind` を付ける。
  - (iv) を勧める理由: stat の有無にも別の index の追加にも plan が左右されず、index が消えれば prepare が誤りで落ちて気づける。(iii) も step は同じだが、選ぶ index は planner 任せ（この測定では `events_by_opened_event`）で、stat や index が変わると選び直しうる。
  - `+run_id` だけ（(ii)）では直らない。
- lock の下の読みを 1 回にまとめる (b) は、新しい start の 2 回の読みを 1 回にする（表の新しい start は読みの 2 倍）。(a) の後では読みが 11 行なので効きは小さいが、害は無い。
- 3689 の acceptance (1)・(2) に使える検査の形:
  - plan: `EXPLAIN QUERY PLAN <文>` の detail に `events_by_kind` があり、`events_by_goal`・`events_by_run`・`events_by_task` と `SCAN run_events` が無い。
  - step: この test の helper と同じく、breach の event 11 件と run の無い別の kind の event を小さい N（例: 0 と 2,000）で入れた DB で `Statement::get_status(StatementStatus::VmStep)` を取り、比が 2 未満（N によらず一定）。常に流す test は小さい N にする。
  - 測るコードは `tests/it/queue_kpi_breach_read_plan.rs` にある。3689 が直すと、この Spike の (i) の行は直した後の文を測らなくなるが、候補の文は helper の中の文字列なので残してよい。
- 本番での効きの確かめ: 着地の後、`queue_write_held_long` の `record_kpi_breach` の held_ms が 1 秒を越えないことを数日見る。越えれば、読みの外の原因（WAL の checkpoint・commit の fsync・同じ process の別の transaction との重なり。同じ process の `record_event_once` 6,019 ms・`record_queue_event` 4,169 ms はこの Spike で測っていない）を別に測る。
- 同じ形（`run_id IS NULL AND task_id IS NULL AND goal_id IS NULL` と kind で絞る文）が他の lock の下の読みにもあれば、同じく `events_by_goal` をたどるおそれがある。この Spike では探していない。

本番との条件の差（この Spike で揃えていないもの）: event 約 107 万件と DB 約 2 GB（page cache に乗らない page の読み込みの待ち）、goal_id の分布、WAL と checkpoint の状態、同じ process の別の transaction の重なり、payload の大きさ。表の時間は page cache に乗った小さな DB のもので、本番の 2〜32 秒をこの比で読み替えてはいない。
