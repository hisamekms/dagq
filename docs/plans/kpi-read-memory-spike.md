---
id: plan-kpi-read-memory-spike
type: plan
title: スパイク：kpi と ObserverKpi の読み 1 件のメモリの内訳を層ごとに測り、今の台帳の形のまま本番の規模で 2 GB 以下にできるか（process の中で allocator を数える測定と外挿）
status: completed
created: 2026-11-06
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - design-measurement
  - plan-supervisor-startup-memory-spike
---

# スパイク：kpi と ObserverKpi の読み 1 件のメモリの内訳

問いは「`LedgerWindow::events()` の複製をやめた後、`kpi` と `ObserverKpi` の読み 1 件のピークのうち、行の text を読む層・行を `LedgerRow` にする層・`LedgerWindow`・畳んだ結果のどれが何割を占めるか。今の台帳の行の形のまま（`ledger_v` を上げず）、1 件のピークを本番の規模（event 約 120 万件）で 2 GB 以下にできるか」。
本番は読み 1 件で 0.04→10.4 GB に増えた（goal 262）。本番の DB は開かず、合成の台帳を event 2 万・10 万・40 万件で測って 120 万件へ外挿した。

## 判定

**does_not_hold**。候補の直しの全体のピークの外挿は、いちばん小さい試作 `lean` で kpi 2,160,242,940 bytes・ObserverKpi 2,146,373,017 bytes で、どちらも 2 GB（2,000,000,000 bytes）を超える（2 GiB = 2,147,483,648 bytes で数えても kpi は超える）。答えは 3 つの規模の全てで今の形と一致した。

根拠:

- 今の形の外挿は kpi 7.29 GB・ObserverKpi 7.27 GB。`moved`（`records` を行から窓へ移して 2 重に持たない）は両方 5.88 GB。`lean`（`moved` に加えて読む間に queue 行と run 行の `detail.steps` を捨てる）は 2.16 GB と 2.15 GB で、今の形の 3 割まで下がるが 2 GB に 7〜8% 届かない。全体の外挿はどれも線形（2 つの傾きの差 3.7% 以下）で、上限の値ではない。
- `lean` に残る量（40 万件で event 1 件あたり 1,806 bytes）の大半は `records` の `RunEvent` で、event 1 件あたり 1,180 bytes（本体 216 bytes と `Value` の木と `created_at`・`run_id` の text）。payload の JSON の text は平均 154 bytes で、木にすると 6 倍以上になる。`Value` の object は key が 1 つでも `BTreeMap` の葉（約 630 bytes）を持つので、行の持ち方の直しではこれ以上減らない。
- 判定は基準どおり合成の測定の外挿によるもので、超えた幅は 7〜8% と小さい。本番の `lean` がどちらに振れるかは合成からは決まらない。合成の `queue.db` は event 1 件あたり 1,757 bytes で本番の約 2.2 KB の 0.81 倍、今の形のピークは event 1 件あたり 6,077 bytes で本番の約 8.7 KB の 0.70 倍だが、これは今の形の比で、本番の多い分が `lean` の捨てるもの（`detail.steps` など）にあれば `lean` の差は縮み、`records` にあれば広がる。本番の kind の比は worker には読めない。数えた値に入らないものは本番の値を上に押す: 40 万件の `lean` の最大の resident は 1.67〜1.88 GB で、数えた 0.72 GB より大きい（resident は参考で、判定には使わない）。
- 決定を変える必要は見つかっていない。ADR-t1662-2 決定 1・7・8（event が SSOT、台帳はビュー）は、下の推奨のどれも保つ。

判定の範囲: 答えの一致は合成の台帳の `kpi`（`KpiQuery::default()`）と `ObserverKpi` の範囲で確かめた。`stats` と `report` の読みは `lean` では確かめていない（`lean` が捨てるものを読まないことはコードで確かめたが、配線の task は読みごとに確かめ直す）。

## 環境

- 対象の commit: `4e96dc40abe27ddccc117a7feda16d2cc1df2aeb`（この run の base。task 4245 の着地の後）に、未配線の試作（`select_moving_records`・`select_of_records`）と example を足したもの。
- host: Apple M1（MacBookAir10,1）、メモリ 16 GiB、macOS 27.2（26B123）。worker と共有した host で流した。値は確保量の和なので host の負荷に依らない。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`rusqlite 0.40`（bundled SQLite）、`--release` の build。
- provider と経路: Claude Code（`claude-opus-5-5`）の headless の worker が、この run の worktree で流した。
- 経路: `SqliteQueue` を process の中で直接読む。queue service の socket は通さない（読み 1 件の中身は同じで、転送の JSON の分は数えていない）。`ObserverKpi` は `observer_kpi_read` と同じ関数（`report::window`・`kpi::of_window` を日 7・週 4 で 2 回・`observer_input`）を呼ぶ。`KpiConfig` は既定、`AreaMap` は 3 つの area、`Host` は固定の値にした（`dagq.toml` と host の file を読む部分は除く）。

### 打ったもの

example は `examples/kpi_read_memory.rs`。

この Spike の `kpi` は台帳の全行の読み（`KpiQuery::default()` が全行を読んだ形）を測った。今の `kpi::kpi` は期間の問いなら `KpiQuery::default()` でも読む範囲を決め、範囲より前に終わった行を読まないので、下の手順を今の example で流し直しても `kpi` は全行の読みにならず、表の数値にならない。今の example で全行の読みに当たるのは `whole <dir> kpi-whole` で、問いは毎時の review のもの（`--period day --last 2`）なので答えの digest は表と違う。`measure` と候補の切り替えは今の example に無い（「配線の後」）。

```sh
cargo build --release --example kpi_read_memory
E=target/release/examples/kpi_read_memory
$E prepare "$TMPDIR/k/q20k" 20000
$E prepare "$TMPDIR/k/q100k" 100000
$E prepare "$TMPDIR/k/q400k" 400000
# 規模ごとに。1 process に 1 読み、同時に 1 process だけ
for q in q20k q100k q400k; do
  for read in kpi observer; do
    $E measure "$TMPDIR/k/$q" $read
    $E whole "$TMPDIR/k/$q" $read
    $E measure "$TMPDIR/k/$q" $read moved
    $E measure "$TMPDIR/k/$q" $read lean
  done
  $E parts "$TMPDIR/k/$q"
done
# 確かめ: 2 万件の 2 回目
$E measure "$TMPDIR/k/q20k" kpi
$E measure "$TMPDIR/k/q20k" observer
```

合成の queue は `$TMPDIR` に置き（40 万件で 703 MB）、終わって消した。

## 測り方

- 数え方: example だけに `#[global_allocator]` を置き、`System` へそのまま渡しながら使用中の bytes（`Layout::size` の和）とピークを数える。値はどれも queue を開いた直後を 0 とした差。allocator の端数（size class の切り上げ）と bundled SQLite が自分の malloc で取るもの（page cache など）は数えた値に入らない。`realloc` は差だけをピークに足すので、写す間に古い塊と新しい塊が並ぶ分はピークに入らない。`getrusage` の最大の resident は参考に添え、判定には使わない。
- メモリの上限: 1 process の数えた使用中の bytes が 3 GiB（3,221,225,472 bytes）を超えたら allocator の中で process を止める。`prepare` も `measure`・`whole`・`parts` も同じ。**止まった規模と読みは無い**。40 万件の最大は今の形の読みの 2,430,767,216 bytes で、`parts` の複製も上限の内だった。上限は数えた bytes に当てるので、resident はそれを超えることがある（40 万件の ObserverKpi の今の形で 3.77 GB）。20 万件への代替は要らなかった。`prepare` は event 1 万件ごとに台帳を作るので、40 万件でも使用中のピークは 29,944,375 bytes だった。
- 周回: 規模と読みの組ごとに 1 回。値は確保量の和で時間と host の負荷に依らず、合成の列も固定の値で決まるので、周回を重ねても変わらない。交互に流す必要も無い（時間を比べない）。確かめとして 2 万件を kpi・ObserverKpi で 2 回ずつ流した全体のピークの差は **0 bytes（0.00%）**（kpi 129,612,217 と 129,612,217、ObserverKpi 130,041,192 と 130,041,192）で、1% を超えないので 3 回の周回はしていない。

### 層の区切り

読みを `ledger_window` と `of_window` の順に段に切った。段は重ならないので、同じ確保は 1 つの層にだけ数える。層のピークは「その段の間の使用中の最大 − その段の始めの使用中」、層の持ち越しは「その段の終わりの使用中 − その段の始めの使用中」。

| 段 | 中身 | 切った形 |
| --- | --- | --- |
| (a) 行の text | `SELECT * FROM ledger_rows` の text の列 | 本物の読みは 1 行ずつ text を読んで `LedgerRow` にして捨てるので、(a) と (b) は 1 行の中で混ざり段に切れない。そこで (a) は全行の text を一度に持つ別の読みとして流し、段の終わりに捨てる（持ち越しは 0）。ピークは「全行の text を持ったとしたら」の量で、本物の読みが text を持つのは 1 行ずつ（最大の行は 40 万件で 253,985 bytes、node 行）。 |
| (b) `LedgerRow` | `LedgerKind::ALL` の `ledger_rows` と on its way の run の fold | 本物と同じ呼び方。1 行の text の一時の分はここに入る。 |
| (c) `LedgerWindow` | `select` | 本物と同じ。 |
| (d) 畳む | `of_window`（`ObserverKpi` は 2 回と `observer_input`、`kpi_breach_events_open`）| 本物と同じ。窓は持ったまま。 |

(b) の内訳（`records`・`totals`・`intervals`・`attributes`・`detail`）は 1 つの段に切れないので、`parts` が別の process で `(b)` の後に各部分を複製し、その複製が割り当てた量で求めた（内訳の行と分かるように書いた。複製は `Vec` の容量でなく長さの分で、各部分の list の本体は引き、各部分の inline の分は数える。`rows_held` との差の大半は `Vec<LedgerRow>` の容量の余り）。

全体のピークは段のピークに段の始めの使用中を足したものの最大で、切った読み（`measure`）と切らない読み（`whole`。本物の `kpi::kpi` と `report::window`＋`of_window`）の差は全ての組で 500 bytes 以下、答えは同じだった。段に切ったことで値は変わっていない。

式:

- 全体に占める割合 = 層のピークの bytes ÷ その読み 1 件の process 全体のピークの bytes（同じ読み・同じ event 数の値どうし）。層は同時に持たれたり先に解放されたりするので、和は 100% にならない。
- 外挿: いちばん大きい 2 つの規模 `(x2, y2)`・`(x3, y3)` の傾き `b = (y3 − y2) ÷ (x3 − x2)` から、120 万件の値 = `y3 + b × (1,200,000 − x3)`。
- 線形性: 小さい 2 つの規模の傾きと大きい 2 つの規模の傾きの差が大きい方の 15% 以内なら線形とみなす。超えるものは大きい方の傾きで外挿した値を上限として書く。

本番の規模の台帳は作っていない。

### 合成の event の列

`prepare` が固定の値で決まる列を書く（乱数は使わない）。測った規模は 20,064・100,006・400,026 件（run 192・957・3,828、`ledger_rows` 12,174・60,665・242,650 行で、うち 2 行は書いた位置の印 `rows_written`・`sessions_written`。読みの行はそれを除く 12,172・60,663・242,648 行）。

- 比: 持ち主の比は [先例](supervisor-startup-memory-spike.md) が本番の `KeptSize` に合わせたものを使う。queue 74 件と run と task 30 件を 1 組にし、40 run ごとに supervisor の一生、5 run ごとに planner の session。
- payload の形はコードの書き手に合わせた（`supervisor_alive` は `{"supervisor": token}`、`live_alert_input_changed` は `input_payload` と `RunInput`、`main_observed` は `observed_payload`、`main_commits_recorded` は `commit_json`）。`receipt_observed` の要約と `review_finished` の本文はこの repository の receipt の長さから 2.4 KB・3 KB にした。
- event の外の入力: run ごとに session の step 240 個（model の turn と tool の call を交互に）。host の sample は全ての列を持つものを 42 秒ごと（event 14 件に 1 個。本番の 30 秒ごと・保持 30 日を 120 万件にしたときの比）。event は 3 秒ごと（本番の 120 万件が約 6 週間）。
- 合わないところ: `supervisor_alive` の 43% は本番の間隔（300 秒）とは合わない。行の数の比を先例に合わせた結果で、本番の queue の event の kind の比は worker には読めない。効くのは「小さい object の payload の event が多い」ことだけなので、kind の内訳に依る削り方（例: alive を間引く）は判定の根拠にしていない。

| `kind`（40 万件） | 件数 | 比 | payload の平均の bytes |
| --- | --- | --- | --- |
| `supervisor_alive` | 172,260 | 43.1% | 49 |
| `live_alert_input_changed` | 45,936 | 11.5% | 384 |
| `main_observed` | 45,936 | 11.5% | 101 |
| `run_phase_changed` | 30,624 | 7.7% | 114 |
| `session_turns` | 30,624 | 7.7% | 54 |
| `task_status_changed` | 7,656 | 1.9% | 34 |
| `agent_started` | 3,828 | 1.0% | 32 |
| `claim_deferral_ended` | 3,828 | 1.0% | 62 |
| `claim_deferred` | 3,828 | 1.0% | 98 |
| `first_commit_observed` | 3,828 | 1.0% | 53 |
| `integration_rebased` | 3,828 | 1.0% | 15 |
| `integration_started` | 3,828 | 1.0% | 49 |
| `landing_queued` | 3,828 | 1.0% | 14 |
| `main_commits_recorded` | 3,828 | 1.0% | 263 |
| `push_finished` | 3,828 | 1.0% | 35 |
| `receipt_observed` | 3,828 | 1.0% | 2,435 |
| `review_finished` | 3,828 | 1.0% | 3,072 |
| `run_claimed` | 3,828 | 1.0% | 103 |
| `run_integrated` | 3,828 | 1.0% | 113 |
| `slots_full_ended` | 3,828 | 1.0% | 65 |
| `slots_full_started` | 3,828 | 1.0% | 65 |
| `task_created` | 3,828 | 1.0% | 157 |
| `validation_finished` | 3,828 | 1.0% | 33 |
| `session_closed` | 765 | 0.2% | 68 |
| `session_opened` | 765 | 0.2% | 91 |
| `live_alert_baseline` | 96 | 0.0% | 164 |
| `stall_config_loaded` | 96 | 0.0% | 78 |
| `supervisor_started` | 96 | 0.0% | 60 |
| `supervisor_stopped` | 95 | 0.0% | 80 |
| `attributes_baseline` | 1 | 0.0% | 2 |

全体の payload の平均は 154 bytes。

| 規模 | `queue.db` の bytes（checkpoint の後） | event 1 件あたり | 本番の約 2.2 KB（2.6 GB ÷ 120 万件 = 2,167 bytes）との比 |
| --- | --- | --- | --- |
| 20,064 件 | 35,323,904 | 1,760 | 0.81 |
| 100,006 件 | 175,517,696 | 1,755 | 0.81 |
| 400,026 件 | 703,209,472 | 1,757 | 0.81 |

大きさの大半は `ledger_rows`（2 万件で 17.0 MB）と `session_steps` とその索引（8.6 MB）、`run_events`（4.7 MB）。値は `sqlite3 queue.db "select name, sum(pgsize) from dbstat group by 1"` で読んだ。(a) の表のピーク（2 万件で 20.1 MB）は text の和 15,080,936 bytes に `String` と `Vec` の本体を足したもの。

## 候補の直しの試作

試作は 2 つ。どちらも kpi・stats・report から呼ばない。

| 名 | 置き場 | 中身 |
| --- | --- | --- |
| `moved` | `select_moving_records`（`src/domain/ledger/window.rs`、`#[doc(hidden)]`） | `select` が `records` を行から複製する代わりに移す。行の `records` は空になり、窓の `records` だけが持つ。 |
| `lean` | `read_lean`（`examples/kpi_read_memory.rs`）と `select_of_records`（`src/domain/ledger/window.rs`、`#[doc(hidden)]`） | `moved` に加えて、`ledger_rows_after` で 50 行ずつ読みながら、`records` を移し、queue 行を捨て、run 行の `detail.steps` を消す。`records` の `Vec` は最新の event の id の大きさで確保する。 |

`lean` が捨てるものを読まないこと: 窓の読みが行を引くのは `LedgerKind` の `Run`・`Task`・`Node`・`Session` だけで、queue 行の値は読まない（`Filters::goals_of` は読むので、`moved` も `lean` も filter の無い窓だけを扱う。配線では読む前に goal を拾う）。`steps::DETAIL_KEY` を読むのは行を作り直す係（`rules_of`）と行の store で、窓の読みには無い。

| 読み | 候補 | event 数 | 全体のピークの bytes | 今の形に対して | 答えの digest（`answer_digest`。今の形と同じか） | 最大の resident（参考） |
| --- | --- | --- | --- | --- | --- | --- |
| kpi | none | 20,064 | 129,612,217 | 100% | `17454358167767668323`（同じ） | 181,043,200 |
| kpi | none | 100,006 | 608,332,148 | 100% | `7300146135464483438`（同じ） | 559,759,360 |
| kpi | none | 400,026 | 2,430,766,723 | 100% | `7448202597071937673`（同じ） | 2,238,726,144 |
| kpi | moved | 20,064 | 98,213,571 | 76% | `17454358167767668323`（同じ） | 164,790,272 |
| kpi | moved | 100,006 | 489,548,368 | 80% | `7300146135464483438`（同じ） | 589,889,536 |
| kpi | moved | 400,026 | 1,959,189,761 | 81% | `7448202597071937673`（同じ） | 2,903,261,184 |
| kpi | lean | 20,064 | 43,157,820 | 33% | `17454358167767668323`（同じ） | 95,092,736 |
| kpi | lean | 100,006 | 183,258,265 | 30% | `7300146135464483438`（同じ） | 255,115,264 |
| kpi | lean | 400,026 | 722,474,790 | 30% | `7448202597071937673`（同じ） | 1,673,363,456 |
| ObserverKpi | none | 20,064 | 130,041,192 | 100% | `13876524636493898585`（同じ） | 183,877,632 |
| ObserverKpi | none | 100,006 | 616,202,923 | 100% | `1932554367057400730`（同じ） | 809,975,808 |
| ObserverKpi | none | 400,026 | 2,430,766,723 | 100% | `17040305594125197112`（同じ） | 3,774,005,248 |
| ObserverKpi | moved | 20,064 | 98,213,571 | 76% | `13876524636493898585`（同じ） | 149,110,784 |
| ObserverKpi | moved | 100,006 | 489,548,368 | 79% | `1932554367057400730`（同じ） | 432,439,296 |
| ObserverKpi | moved | 400,026 | 1,959,189,761 | 81% | `17040305594125197112`（同じ） | 3,145,908,224 |
| ObserverKpi | lean | 20,064 | 43,586,795 | 34% | `13876524636493898585`（同じ） | 92,487,680 |
| ObserverKpi | lean | 100,006 | 191,129,040 | 31% | `1932554367057400730`（同じ） | 369,311,744 |
| ObserverKpi | lean | 400,026 | 724,415,856 | 30% | `17040305594125197112`（同じ） | 1,884,241,920 |

答えの digest は答えの JSON（object の key は整列）の `DefaultHasher` の値。答えの大きさは kpi が 324,932・559,361・762,440 bytes、ObserverKpi が 3,003・13,185・34,953 bytes で、中身のある答えどうしを比べている。

試作の外で一度だけ確かめたこと（判定には入れない）: `lean` で kind ごとに payload を `Null` にして答えが変わらない 20 の kind を全部消すと、2 万件で 43.2 MB→28.4 MB（event 1 件あたり約 1,420 bytes、線形なら 120 万件で約 1.7 GB）になった。ただし `supervisor_alive` や `live_alert_input_changed` のように読み手がコードにあるのに合成の列では答えに出ない kind も含むので、答えが一致したことはその kind を読まないことの証拠にならない。

## 結果

### 読みと層ごとのピーク

候補 `none` が今の形。(a) は「全行の text を一度に持ったとしたら」の量で本物の読みでは同時に持たない（上の「層の区切り」）。線形でない層の外挿は上限の値。

| 読み | 候補 | 層 | event 数 | 値の区分 | ピークの bytes | 持ち越しの bytes | 全体に占める割合 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| kpi | none | 全体 | 20,064 | 測定 | 129,612,217 | — | 100.0% |
| kpi | none | (a) 行の text | 20,064 | 測定 | 20,148,200 | 0 | 15.5% |
| kpi | none | (b) LedgerRow | 20,064 | 測定 | 93,901,379 | 93,879,619 | 72.4% |
| kpi | none | (c) LedgerWindow | 20,064 | 測定 | 33,024,104 | 25,024,584 | 25.5% |
| kpi | none | (d) 畳む | 20,064 | 測定 | 10,707,886 | 4,074,251 | 8.3% |
| kpi | none | 全体 | 100,006 | 測定 | 608,332,148 | — | 100.0% |
| kpi | none | (a) 行の text | 100,006 | 測定 | 100,274,257 | 0 | 16.5% |
| kpi | none | (b) LedgerRow | 100,006 | 測定 | 468,033,984 | 467,946,944 | 76.9% |
| kpi | none | (c) LedgerWindow | 100,006 | 測定 | 139,485,206 | 118,859,446 | 22.9% |
| kpi | none | (d) 畳む | 100,006 | 測定 | 21,525,630 | 5,989,559 | 3.5% |
| kpi | none | 全体 | 400,026 | 測定 | 2,430,766,723 | — | 100.0% |
| kpi | none | (a) 行の text | 400,026 | 測定 | 402,847,584 | 0 | 16.6% |
| kpi | none | (b) LedgerRow | 400,026 | 測定 | 1,873,132,177 | 1,872,784,017 | 77.1% |
| kpi | none | (c) LedgerWindow | 400,026 | 測定 | 557,982,578 | 475,482,258 | 23.0% |
| kpi | none | (d) 畳む | 400,026 | 測定 | 75,507,742 | 7,612,255 | 3.1% |
| kpi | none | 全体 | 1,200,000 | 外挿 | 7,290,110,356 | — | 100.0% |
| kpi | none | (a) 行の text | 1,200,000 | 外挿 | 1,209,629,781 | 0 | 16.6% |
| kpi | none | (b) LedgerRow | 1,200,000 | 外挿 | 5,619,689,146 | 5,618,644,735 | 77.1% |
| kpi | none | (c) LedgerWindow | 1,200,000 | 外挿 | 1,673,864,908 | 1,426,382,123 | 23.0% |
| kpi | none | (d) 畳む | 1,200,000 | 外挿 | 219,445,766 | 11,939,015 | 3.0% |
| kpi | moved | 全体 | 20,064 | 測定 | 98,213,571 | — | 100.0% |
| kpi | moved | (a) 行の text | 20,064 | 測定 | 20,148,200 | 0 | 20.5% |
| kpi | moved | (b) LedgerRow | 20,064 | 測定 | 93,901,379 | 93,879,619 | 95.6% |
| kpi | moved | (c) LedgerWindow | 20,064 | 測定 | 4,333,824 | -7,711,200 | 4.4% |
| kpi | moved | (d) 畳む | 20,064 | 測定 | 10,707,886 | 4,074,251 | 10.9% |
| kpi | moved | 全体 | 100,006 | 測定 | 489,548,368 | — | 100.0% |
| kpi | moved | (a) 行の text | 100,006 | 測定 | 100,274,257 | 0 | 20.5% |
| kpi | moved | (b) LedgerRow | 100,006 | 測定 | 468,033,984 | 467,946,944 | 95.6% |
| kpi | moved | (c) LedgerWindow | 100,006 | 測定 | 21,601,296 | -38,332,656 | 4.4% |
| kpi | moved | (d) 畳む | 100,006 | 測定 | 21,525,630 | 5,989,559 | 4.4% |
| kpi | moved | 全体 | 400,026 | 測定 | 1,959,189,761 | — | 100.0% |
| kpi | moved | (a) 行の text | 400,026 | 測定 | 402,847,584 | 0 | 20.6% |
| kpi | moved | (b) LedgerRow | 400,026 | 測定 | 1,873,132,177 | 1,872,784,017 | 95.6% |
| kpi | moved | (c) LedgerWindow | 400,026 | 測定 | 86,405,616 | -153,331,056 | 4.4% |
| kpi | moved | (d) 畳む | 400,026 | 測定 | 75,507,742 | 7,612,255 | 3.9% |
| kpi | moved | 全体 | 1,200,000 | 外挿 | 5,877,844,863 | — | 100.0% |
| kpi | moved | (a) 行の text | 1,200,000 | 外挿 | 1,209,629,781 | 0 | 20.6% |
| kpi | moved | (b) LedgerRow | 1,200,000 | 外挿 | 5,619,689,146 | 5,618,644,735 | 95.6% |
| kpi | moved | (c) LedgerWindow | 1,200,000 | 外挿 | 259,200,000 | -459,963,047 | 4.4% |
| kpi | moved | (d) 畳む | 1,200,000 | 外挿 | 219,445,766 | 11,939,015 | 3.7% |
| kpi | lean | 全体 | 20,064 | 測定 | 43,157,820 | — | 100.0% |
| kpi | lean | (a) 行の text | 20,064 | 測定 | 20,148,200 | 0 | 46.7% |
| kpi | lean | (b) LedgerRow | 20,064 | 測定 | 32,549,425 | 32,449,806 | 75.4% |
| kpi | lean | (c) LedgerWindow | 20,064 | 測定 | 4,333,824 | 0 | 10.0% |
| kpi | lean | (d) 畳む | 20,064 | 測定 | 10,707,886 | 4,074,251 | 24.8% |
| kpi | lean | 全体 | 100,006 | 測定 | 183,258,265 | — | 100.0% |
| kpi | lean | (a) 行の text | 100,006 | 測定 | 100,274,257 | 0 | 54.7% |
| kpi | lean | (b) LedgerRow | 100,006 | 測定 | 163,084,971 | 161,732,507 | 89.0% |
| kpi | lean | (c) LedgerWindow | 100,006 | 測定 | 10,800,648 | 0 | 5.9% |
| kpi | lean | (d) 畳む | 100,006 | 測定 | 21,525,630 | 5,989,559 | 11.7% |
| kpi | lean | 全体 | 400,026 | 測定 | 722,474,790 | — | 100.0% |
| kpi | lean | (a) 行の text | 400,026 | 測定 | 402,847,584 | 0 | 55.8% |
| kpi | lean | (b) LedgerRow | 400,026 | 測定 | 652,289,864 | 646,966,920 | 90.3% |
| kpi | lean | (c) LedgerWindow | 400,026 | 測定 | 43,202,808 | 0 | 6.0% |
| kpi | lean | (d) 畳む | 400,026 | 測定 | 75,507,742 | 7,612,255 | 10.5% |
| kpi | lean | 全体 | 1,200,000 | 外挿 | 2,160,242,940 | — | 100.0% |
| kpi | lean | (a) 行の text | 1,200,000 | 外挿 | 1,209,629,781 | 0 | 56.0% |
| kpi | lean | (b) LedgerRow | 1,200,000 | 外挿 | 1,956,706,886 | 1,940,797,046 | 90.6% |
| kpi | lean | (c) LedgerWindow | 1,200,000 | 外挿 | 129,600,000 | 0 | 6.0% |
| kpi | lean | (d) 畳む | 1,200,000 | 外挿 | 219,445,766 | 11,939,015 | 10.2% |
| ObserverKpi | none | 全体 | 20,064 | 測定 | 130,041,192 | — | 100.0% |
| ObserverKpi | none | (a) 行の text | 20,064 | 測定 | 20,148,200 | 0 | 15.5% |
| ObserverKpi | none | (b) LedgerRow | 20,064 | 測定 | 93,901,379 | 93,879,619 | 72.2% |
| ObserverKpi | none | (c) LedgerWindow | 20,064 | 測定 | 33,024,104 | 25,024,584 | 25.4% |
| ObserverKpi | none | (d) 畳む | 20,064 | 測定 | 11,136,861 | 23,988 | 8.6% |
| ObserverKpi | none | 全体 | 100,006 | 測定 | 616,202,923 | — | 100.0% |
| ObserverKpi | none | (a) 行の text | 100,006 | 測定 | 100,274,257 | 0 | 16.3% |
| ObserverKpi | none | (b) LedgerRow | 100,006 | 測定 | 468,033,984 | 467,946,944 | 76.0% |
| ObserverKpi | none | (c) LedgerWindow | 100,006 | 測定 | 139,485,206 | 118,859,446 | 22.6% |
| ObserverKpi | none | (d) 畳む | 100,006 | 測定 | 29,396,405 | 93,687 | 4.8% |
| ObserverKpi | none | 全体 | 400,026 | 測定 | 2,430,766,723 | — | 100.0% |
| ObserverKpi | none | (a) 行の text | 400,026 | 測定 | 402,847,584 | 0 | 16.6% |
| ObserverKpi | none | (b) LedgerRow | 400,026 | 測定 | 1,873,132,177 | 1,872,784,017 | 77.1% |
| ObserverKpi | none | (c) LedgerWindow | 400,026 | 測定 | 557,982,578 | 475,482,258 | 23.0% |
| ObserverKpi | none | (d) 畳む | 400,026 | 測定 | 77,448,808 | 240,688 | 3.2% |
| ObserverKpi | none | 全体 | 1,200,000 | 外挿 | 7,269,123,704 | — | 100.0% |
| ObserverKpi | none | (a) 行の text | 1,200,000 | 外挿 | 1,209,629,781 | 0 | 16.6% |
| ObserverKpi | none | (b) LedgerRow | 1,200,000 | 外挿 | 5,619,689,146 | 5,618,644,735 | 77.3% |
| ObserverKpi | none | (c) LedgerWindow | 1,200,000 | 外挿 | 1,673,864,908 | 1,426,382,123 | 23.0% |
| ObserverKpi | none | (d) 畳む | 1,200,000 | 外挿 | 260,170,787 | 632,652 | 3.6% |
| ObserverKpi | moved | 全体 | 20,064 | 測定 | 98,213,571 | — | 100.0% |
| ObserverKpi | moved | (a) 行の text | 20,064 | 測定 | 20,148,200 | 0 | 20.5% |
| ObserverKpi | moved | (b) LedgerRow | 20,064 | 測定 | 93,901,379 | 93,879,619 | 95.6% |
| ObserverKpi | moved | (c) LedgerWindow | 20,064 | 測定 | 4,333,824 | -7,711,200 | 4.4% |
| ObserverKpi | moved | (d) 畳む | 20,064 | 測定 | 11,136,861 | 23,988 | 11.3% |
| ObserverKpi | moved | 全体 | 100,006 | 測定 | 489,548,368 | — | 100.0% |
| ObserverKpi | moved | (a) 行の text | 100,006 | 測定 | 100,274,257 | 0 | 20.5% |
| ObserverKpi | moved | (b) LedgerRow | 100,006 | 測定 | 468,033,984 | 467,946,944 | 95.6% |
| ObserverKpi | moved | (c) LedgerWindow | 100,006 | 測定 | 21,601,296 | -38,332,656 | 4.4% |
| ObserverKpi | moved | (d) 畳む | 100,006 | 測定 | 29,396,405 | 93,687 | 6.0% |
| ObserverKpi | moved | 全体 | 400,026 | 測定 | 1,959,189,761 | — | 100.0% |
| ObserverKpi | moved | (a) 行の text | 400,026 | 測定 | 402,847,584 | 0 | 20.6% |
| ObserverKpi | moved | (b) LedgerRow | 400,026 | 測定 | 1,873,132,177 | 1,872,784,017 | 95.6% |
| ObserverKpi | moved | (c) LedgerWindow | 400,026 | 測定 | 86,405,616 | -153,331,056 | 4.4% |
| ObserverKpi | moved | (d) 畳む | 400,026 | 測定 | 77,448,808 | 240,688 | 4.0% |
| ObserverKpi | moved | 全体 | 1,200,000 | 外挿 | 5,877,844,863 | — | 100.0% |
| ObserverKpi | moved | (a) 行の text | 1,200,000 | 外挿 | 1,209,629,781 | 0 | 20.6% |
| ObserverKpi | moved | (b) LedgerRow | 1,200,000 | 外挿 | 5,619,689,146 | 5,618,644,735 | 95.6% |
| ObserverKpi | moved | (c) LedgerWindow | 1,200,000 | 外挿 | 259,200,000 | -459,963,047 | 4.4% |
| ObserverKpi | moved | (d) 畳む | 1,200,000 | 外挿 | 260,170,787 | 632,652 | 4.4% |
| ObserverKpi | lean | 全体 | 20,064 | 測定 | 43,586,795 | — | 100.0% |
| ObserverKpi | lean | (a) 行の text | 20,064 | 測定 | 20,148,200 | 0 | 46.2% |
| ObserverKpi | lean | (b) LedgerRow | 20,064 | 測定 | 32,549,425 | 32,449,806 | 74.7% |
| ObserverKpi | lean | (c) LedgerWindow | 20,064 | 測定 | 4,333,824 | 0 | 9.9% |
| ObserverKpi | lean | (d) 畳む | 20,064 | 測定 | 11,136,861 | 23,988 | 25.6% |
| ObserverKpi | lean | 全体 | 100,006 | 測定 | 191,129,040 | — | 100.0% |
| ObserverKpi | lean | (a) 行の text | 100,006 | 測定 | 100,274,257 | 0 | 52.5% |
| ObserverKpi | lean | (b) LedgerRow | 100,006 | 測定 | 163,084,971 | 161,732,507 | 85.3% |
| ObserverKpi | lean | (c) LedgerWindow | 100,006 | 測定 | 10,800,648 | 0 | 5.7% |
| ObserverKpi | lean | (d) 畳む | 100,006 | 測定 | 29,396,405 | 93,687 | 15.4% |
| ObserverKpi | lean | 全体 | 400,026 | 測定 | 724,415,856 | — | 100.0% |
| ObserverKpi | lean | (a) 行の text | 400,026 | 測定 | 402,847,584 | 0 | 55.6% |
| ObserverKpi | lean | (b) LedgerRow | 400,026 | 測定 | 652,289,864 | 646,966,920 | 90.0% |
| ObserverKpi | lean | (c) LedgerWindow | 400,026 | 測定 | 43,202,808 | 0 | 6.0% |
| ObserverKpi | lean | (d) 畳む | 400,026 | 測定 | 77,448,808 | 240,688 | 10.7% |
| ObserverKpi | lean | 全体 | 1,200,000 | 外挿 | 2,146,373,017 | — | 100.0% |
| ObserverKpi | lean | (a) 行の text | 1,200,000 | 外挿 | 1,209,629,781 | 0 | 56.4% |
| ObserverKpi | lean | (b) LedgerRow | 1,200,000 | 外挿 | 1,956,706,886 | 1,940,797,046 | 91.2% |
| ObserverKpi | lean | (c) LedgerWindow | 1,200,000 | 外挿 | 129,600,000 | 0 | 6.0% |
| ObserverKpi | lean | (d) 畳む | 1,200,000 | 外挿 | 260,170,787 | 632,652 | 12.1% |

今の形（kpi、40 万件）を読むと:

- (b) の `LedgerRow` が全体の 77%（持ち越し 1.87 GB）。うち `detail` が 36%（run 行の `detail.steps` と node 行の `detail.samples`）、`records` が 19%。
- (c) の `LedgerWindow` が 23%（持ち越し 475 MB）。`records` を行から複製する分で、(b) の `records` と同じ event を 2 重に持つ。
- (d) の畳む層は 3%（ピーク 76 MB）で、問題の主因ではない。
- (a) の text は全部持ったとしても 17%（403 MB）で、本物の読みは同時に持たない。本番の 8.7 KB/event の説明は text ではなく、text を `Value` の木にした大きさ。

全体のピークの場所は (c) か (d) の最中で、(b) の持ち越しの上に (c) の複製が乗るところ。

### (b) の内訳

各部分を複製してその複製が割り当てた量（内訳の行）。割合は kpi の今の形の全体のピークに対するもの。

| 部分（複製の量） | event 数 | 値の区分 | bytes | 全体に占める割合（kpi の今の形） | 持ち主の内訳 |
| --- | --- | --- | --- | --- | --- |
| records の RunEvent（内訳の行） | 20,064 | 測定 | 23,678,568 | 18.3% | queue 16.0M run 7.1M task 0.5M session 0.1M node 0.0M |
| totals（内訳の行） | 20,064 | 測定 | 1,801,568 | 1.4% | queue 1.2M run 0.3M task 0.3M session 0.0M node 0.0M |
| intervals（内訳の行） | 20,064 | 測定 | 867,744 | 0.7% | queue 0.3M run 0.2M task 0.3M session 0.0M node 0.0M |
| attributes（内訳の行） | 20,064 | 測定 | 292,128 | 0.2% | queue 0.3M run 0.0M task 0.0M session 0.0M node 0.0M |
| detail（内訳の行） | 20,064 | 測定 | 44,292,687 | 34.2% | queue 0.9M run 36.8M task 0.1M session 0.1M node 6.3M |
| key・built_at・outcome・subject（内訳の行） | 20,064 | 測定 | 3,368,108 | 2.6% | queue 3.3M run 0.1M task 0.0M session 0.0M node 0.0M |
| 行の本体（`size_of::<LedgerRow>()` 680 bytes × 12,172 行。計算値で、部分の複製が数えた各欄の inline の分と一部重なる） | 20,064 | 測定 | 8,276,960 | 6.4% | — |
| (b) の段の終わりの使用中（`rows_held`） | 20,064 | 測定 | 93,879,619 | 72.4% | — |
| records の RunEvent（内訳の行） | 100,006 | 測定 | 118,028,782 | 19.4% | queue 79.7M run 35.3M task 2.7M session 0.4M node 0.0M |
| totals（内訳の行） | 100,006 | 測定 | 8,978,982 | 1.5% | queue 6.1M run 1.4M task 1.4M session 0.0M node 0.0M |
| intervals（内訳の行） | 100,006 | 測定 | 4,324,998 | 0.7% | queue 1.4M run 1.2M task 1.7M session 0.0M node 0.0M |
| attributes（内訳の行） | 100,006 | 測定 | 1,455,912 | 0.2% | queue 1.4M run 0.0M task 0.0M session 0.0M node 0.0M |
| detail（内訳の行） | 100,006 | 測定 | 220,860,631 | 36.3% | queue 4.7M run 183.6M task 0.7M session 0.5M node 31.3M |
| key・built_at・outcome・subject（内訳の行） | 100,006 | 測定 | 16,812,697 | 2.8% | queue 16.2M run 0.3M task 0.2M session 0.1M node 0.0M |
| 行の本体（`size_of::<LedgerRow>()` 680 bytes × 60,663 行。計算値で、部分の複製が数えた各欄の inline の分と一部重なる） | 100,006 | 測定 | 41,250,840 | 6.8% | — |
| (b) の段の終わりの使用中（`rows_held`） | 100,006 | 測定 | 467,946,944 | 76.9% | — |
| records の RunEvent（内訳の行） | 400,026 | 測定 | 472,159,938 | 19.4% | queue 318.9M run 141.0M task 10.8M session 1.4M node 0.0M |
| totals（内訳の行） | 400,026 | 測定 | 35,915,512 | 1.5% | queue 24.3M run 5.7M task 5.8M session 0.1M node 0.0M |
| intervals（内訳の行） | 400,026 | 測定 | 17,299,896 | 0.7% | queue 5.6M run 4.7M task 6.9M session 0.0M node 0.0M |
| attributes（内訳の行） | 400,026 | 測定 | 5,823,552 | 0.2% | queue 5.6M run 0.1M task 0.1M session 0.0M node 0.0M |
| detail（内訳の行） | 400,026 | 測定 | 884,193,159 | 36.4% | queue 18.7M run 735.1M task 3.0M session 2.1M node 125.3M |
| key・built_at・outcome・subject（内訳の行） | 400,026 | 測定 | 67,449,184 | 2.8% | queue 65.1M run 1.0M task 0.9M session 0.2M node 0.1M |
| 行の本体（`size_of::<LedgerRow>()` 680 bytes × 242,648 行。計算値で、部分の複製が数えた各欄の inline の分と一部重なる） | 400,026 | 測定 | 165,000,640 | 6.8% | — |
| (b) の段の終わりの使用中（`rows_held`） | 400,026 | 測定 | 1,872,784,017 | 77.0% | — |

40 万件で run 行の `detail` は 735 MB（1 行あたり 192 KB、ほぼ `detail.steps`）、node 行の `detail` は 125 MB（28,574 sample、1 個あたり 4.4 KB）。queue 行は 233,892 行で、`records` を除いて本体 159 MB と `detail`・文字列などに 119 MB を持つが、窓の読みは使わない。

### 外挿と線形性の確かめ

傾きと event 1 件あたりは bytes。全体の行はどれも線形で、線形でないのは (d) と `lean` の (c)（kpi の (d) と `lean` の (c) は小さい規模で傾きが緩く、ObserverKpi の (d) は大きい規模で緩い）。(d) は全体の 3〜10% の層で、全体の傾きの差は 3.7% 以下に収まる。

| 読み | 候補 | 層 | 小さい 2 つの傾き（bytes/event） | 大きい 2 つの傾き（bytes/event） | 差（大きい方に対して） | 線形か | 40 万件の event 1 件あたりの bytes | 120 万件の外挿 bytes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| kpi | none | 全体 | 5,988 | 6,074 | 1.4% | 線形 | 6,077 | 7,290,110,356 |
| kpi | none | (a) 行の text | 1,002 | 1,009 | 0.6% | 線形 | 1,007 | 1,209,629,781 |
| kpi | none | (b) LedgerRow | 4,680 | 4,683 | 0.1% | 線形 | 4,683 | 5,619,689,146 |
| kpi | none | (c) LedgerWindow | 1,332 | 1,395 | 4.5% | 線形 | 1,395 | 1,673,864,908 |
| kpi | none | (d) 畳む | 135 | 180 | 24.8% | 線形でない（外挿は上限） | 189 | 219,445,766 |
| kpi | moved | 全体 | 4,895 | 4,898 | 0.1% | 線形 | 4,898 | 5,877,844,863 |
| kpi | moved | (a) 行の text | 1,002 | 1,009 | 0.6% | 線形 | 1,007 | 1,209,629,781 |
| kpi | moved | (b) LedgerRow | 4,680 | 4,683 | 0.1% | 線形 | 4,683 | 5,619,689,146 |
| kpi | moved | (c) LedgerWindow | 216 | 216 | 0.0% | 線形 | 216 | 259,200,000 |
| kpi | moved | (d) 畳む | 135 | 180 | 24.8% | 線形でない（外挿は上限） | 189 | 219,445,766 |
| kpi | lean | 全体 | 1,753 | 1,797 | 2.5% | 線形 | 1,806 | 2,160,242,940 |
| kpi | lean | (a) 行の text | 1,002 | 1,009 | 0.6% | 線形 | 1,007 | 1,209,629,781 |
| kpi | lean | (b) LedgerRow | 1,633 | 1,631 | 0.1% | 線形 | 1,631 | 1,956,706,886 |
| kpi | lean | (c) LedgerWindow | 81 | 108 | 25.1% | 線形でない（外挿は上限） | 108 | 129,600,000 |
| kpi | lean | (d) 畳む | 135 | 180 | 24.8% | 線形でない（外挿は上限） | 189 | 219,445,766 |
| ObserverKpi | none | 全体 | 6,081 | 6,048 | 0.5% | 線形 | 6,077 | 7,269,123,704 |
| ObserverKpi | none | (a) 行の text | 1,002 | 1,009 | 0.6% | 線形 | 1,007 | 1,209,629,781 |
| ObserverKpi | none | (b) LedgerRow | 4,680 | 4,683 | 0.1% | 線形 | 4,683 | 5,619,689,146 |
| ObserverKpi | none | (c) LedgerWindow | 1,332 | 1,395 | 4.5% | 線形 | 1,395 | 1,673,864,908 |
| ObserverKpi | none | (d) 畳む | 228 | 160 | 29.9% | 線形でない（外挿は上限） | 194 | 260,170,787 |
| ObserverKpi | moved | 全体 | 4,895 | 4,898 | 0.1% | 線形 | 4,898 | 5,877,844,863 |
| ObserverKpi | moved | (a) 行の text | 1,002 | 1,009 | 0.6% | 線形 | 1,007 | 1,209,629,781 |
| ObserverKpi | moved | (b) LedgerRow | 4,680 | 4,683 | 0.1% | 線形 | 4,683 | 5,619,689,146 |
| ObserverKpi | moved | (c) LedgerWindow | 216 | 216 | 0.0% | 線形 | 216 | 259,200,000 |
| ObserverKpi | moved | (d) 畳む | 228 | 160 | 29.9% | 線形でない（外挿は上限） | 194 | 260,170,787 |
| ObserverKpi | lean | 全体 | 1,846 | 1,778 | 3.7% | 線形 | 1,811 | 2,146,373,017 |
| ObserverKpi | lean | (a) 行の text | 1,002 | 1,009 | 0.6% | 線形 | 1,007 | 1,209,629,781 |
| ObserverKpi | lean | (b) LedgerRow | 1,633 | 1,631 | 0.1% | 線形 | 1,631 | 1,956,706,886 |
| ObserverKpi | lean | (c) LedgerWindow | 81 | 108 | 25.1% | 線形でない（外挿は上限） | 108 | 129,600,000 |
| ObserverKpi | lean | (d) 畳む | 228 | 160 | 29.9% | 線形でない（外挿は上限） | 194 | 260,170,787 |

本番との突き合わせ: 今の形の外挿 7.29 GB は本番の 10.4 GB の 0.70 倍で、`queue.db` の比 0.81 より少し離れる。本番は allocator の端数と SQLite の分も footprint に入るので、数えた値より大きく出る方向にずれる。

## 後続の計画への推奨

1. `lean` の 2 つの直しは判定に関わらず効く（ピークが今の 3 割、kpi と ObserverKpi の答えは合成の台帳で変わらない）ので、まず実装の task にする: `select` が `records` を移す、window の読みが queue 行と run 行の `detail.steps` を持たない（SQL で要る列と `json_remove` に絞れば parse も減る）。配線は `kpi`・`stats`・`report` と filter のある窓（`Filters::goals_of`）の答えを、読みごとに既存の test で確かめる。本番の読み 1 件は 10.4 GB から約 3 GB になる見込み（今の形の合成の比 0.70 で割り戻した値で、本番の多い分が `lean` の捨てるものにあればもっと小さい。本番の前後の比較は goal の受け入れ 5 が扱う）。
2. 2 GB を切るには `RunEvent` の持ち方を変える。これは台帳の行の形も `ledger_v` も変えない（読みの中のメモリの形だけ）: payload を `Value` でなく text で持ち、kind で読むときに解く形なら、event 1 件あたり本体を除く約 964 bytes（`Value` の木と `created_at`・`run_id` の text）のうち木の分が text の 154 bytes になり、差は多くて約 810 bytes × 120 万件で約 1 GB 下がる（上限の試算で、未測定。`RunEvent.payload` の読み手は広いので、試作と別の Spike が要る）。判定の「台帳の形か結果の持ち越し」の前に、こちらを先に試すことを勧める。台帳の形の変更と KPI の結果の持ち越しは ADR-t1662-2 の範囲を動かすので、それでも足りなければに回す。
3. node 行の `detail.samples` は 120 万件で約 0.38 GB（sample の数は保持 30 日で頭打ちなので、event の増え方とは別）。読む期間で絞る仕事（goal 255）と合わせて見る。
4. (d) の畳む層は 3% なので、計算の側の直しは後でよい。

試作の `select_moving_records`・`select_of_records` は配線のときに置き換えて消す。

## 配線の後

`lean` を runtime に配線した。`LedgerStore::ledger_window` が `WindowRows`（`src/domain/ledger/window.rs`）で `ledger_rows_after` の頁を 50 行ずつ読みながら畳む。`select_moving_records`・`select_of_records`・`read_lean` と example の `measure` と candidate の切り替えは消した（Spike の測定の example は commit `70b44f049` にある）。下の表は、配線の commit の example の `whole` が runtime の読み（`kpi::kpi` と、`report::window` と `of_window` と `observer_input`）をそのまま測ったもので、当時の `whole <dir> kpi` は Spike と同じ全行の読みだった。今の `kpi::kpi` は期間の問いで読む範囲を決めるので、今の `whole <dir> kpi`（`KpiQuery::default()`）は範囲を読み、下の手順を流し直しても表の数値にならない。今の example で全行の読みに当たるのは `whole <dir> kpi-whole`（問いは `--period day --last 2`）で、範囲を読む同じ問いは `whole <dir> kpi-range`。

比はどの組も 1.05 以下で、答えの digest は Spike の今の形と `lean` と同じだった。`stats` と `report` と filter のある窓は example で測らず、答えの一致を unit test（`application::window_read_tests`）で確かめた。

| 読み | event 数 | 全体のピークの bytes | 試作 lean の全体のピークの bytes | 比 |
| --- | ---: | ---: | ---: | ---: |
| kpi | 20,064 | 43,161,532 | 43,157,820 | 1.0001 |
| kpi | 100,006 | 183,277,277 | 183,258,265 | 1.0001 |
| ObserverKpi | 20,064 | 43,591,128 | 43,586,795 | 1.0001 |
| ObserverKpi | 100,006 | 191,148,673 | 191,129,040 | 1.0001 |

値は確保量の和で周回で変わらないので、組ごとに 1 回流した。上限は同じ 1 process の数えた使用中 3 GiB で、止まった組は無い。40 万件は流していない。本番の DB は開いていない。

- 対象の commit: `9e3a227b1ad0117a9aa4380b6b4677f31c0cc311`（base `ca332d68b` の上の、この配線の commit）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`--release` の build、macOS arm64。

```sh
cargo build --release --locked --example kpi_read_memory
E=target/release/examples/kpi_read_memory
$E prepare "$TMPDIR/k/q20k" 20000
$E prepare "$TMPDIR/k/q100k" 100000
# 1 process に 1 読み、同時に 1 process だけ
for q in q20k q100k; do
  for read in kpi observer; do
    $E whole "$TMPDIR/k/$q" $read
  done
done
rm -rf "$TMPDIR/k"
```
