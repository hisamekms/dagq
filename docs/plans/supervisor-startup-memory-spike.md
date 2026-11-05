---
id: plan-supervisor-startup-memory-spike
type: plan
title: スパイク：supervisor の起動の追いつきの後に持ち続ける確保量はどの保持か、確定済みの履歴の event の数に比例するか（process の中で allocator を数える測定）
status: completed
created: 2026-11-01
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - design-measurement
  - plan-build-memory
---

# スパイク：supervisor の起動の追いつきの後に持ち続ける確保量

問いは「supervisor の起動の追いつき（判定の観測の最初の pass と、台帳の係の build pass を cursor の始めから最新まで）を終えた後に process が持ち続ける使用中の確保量の大半はどの保持か。それは確定済みの履歴の event の数に比例するか」。
本番（2026-10-31、約 110 万 event）では exec の後 60〜90 秒で malloc の使用中 6.2 GB・小さな割り当て 6,400 万個に達して下がらなかった（goal 241）。
worker は本番の process を測れないので、合成した履歴の queue を同じ port の経路で process の中で読み、allocator で数えた。

判定は **holds**（下の「判定」）。ただし成り立つのは合成した queue を `SqliteQueue` の経路で読んだ範囲で、本番の 6.2 GB の大半は説明しない。
追いつきの後に持ち続ける量は、合成ではほぼ全部が台帳の係の `Kept` の `queue_rows`（`QueueBook.rows`）と `dropped` にあり、確定済みの event 1 件あたり約 246 bytes・2.06 個で比例して増える。
ただしその量は 110 万件へ外挿して約 0.26 GiB で、本番の 6.2 GB の 4% ほどしか説明しない。
本番の量に近いのは、追いつきの最初の pass の一時のピーク（event 1 件あたり約 3.2 KB・17 個、110 万件で約 3.3 GiB・1,900 万個）で、合成ではこれは pass の終わりに解放される。

## 環境

- 対象の commit: `0e918c82759ed3b44ed2a5464c5afb4eecc83397`（この run の base）に、計測の入口（`Kept::clone_parts`・`Builder::clone_parts`。`#[doc(hidden)]` で runtime からは呼ばない）を足したもの。
- host: Apple M1、メモリ 16 GiB、macOS 27.2（26B123）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`、`--release` の build。
- allocator の数え方: example（`examples/supervisor_startup_memory.rs`）だけに `#[global_allocator]` を置き、`System` へそのまま渡しながら使用中の bytes（`Layout::size` の和）・使用中の割り当ての個数・その 2 つのピークを原子的に数える。ピークは段ごとに今の使用中へ戻す。表の値は queue を開いた直後を 0 とした差。allocator の端数（size class の切り上げ）と bundled SQLite が自分の malloc で取るもの（page cache など）は含まない。`realloc` で伸ばした塊は差だけをピークに足すので、写す間に古い塊と新しい塊が並ぶ分だけピークは低めに出る。`getrusage` の最大の resident も添える。
- 経路: `SqliteQueue`（supervisor の port の trait をそのまま実装する store）に対して、判定の観測は `stats::carried_live_inputs`（`LiveAlertWatch::pass` が呼ぶもの。記録の書き込みは除く）と `live_alerts::judge`、台帳の係は `supervise::build_pass`。queue service（client mode の `SocketCaller` や `InProcessCaller`）の経路は通していない（下の「推奨」）。
- 係ごとの内訳は 2 通りで求めた。追いつきの後に、各部分を複製してその複製が割り当てた量（`clone_parts`。Vec は容量でなく長さの分）と、`LiveHistory`・`Builder`・`Kept` を順に drop して減った量。

### 合成した event の列

本番の queue は worker が読めないので、比は goal 241 に書かれた本番の `KeptSize`（110 万件に対し `queue_rows` 約 78 万・`dropped_rows` 約 70 万・残す event 約 9.4 万）に合わせた。
queue の行は queue 全体の event 1 件ごとに 1 行が多いので、queue 全体の event を全体の 71% にした。
再現できなかった比もある: 合成の `queue_rows` は event の 58%（本番 71%）、`dropped` は 57%（本番 64%）、追いつきの後に `Kept` が残す event は約 0.3%（本番 8.5%、約 9.4 万件）。

| 種類（台帳の行の持ち主） | 比 | 中身 |
| --- | --- | --- |
| queue | 71% | 1 run ごとに 74 件: `supervisor_alive` 62%、`live_alert_input_changed` と `main_observed` 各 16%、`slots_full_started`／`ended` と `claim_deferred`／`claim_deferral_ended` の組。40 run ごとに supervisor の一生（`supervisor_started`・`stall_config_loaded`・`live_alert_baseline`・`supervisor_stopped`） |
| run | 26% | 終わった run 1 件に 27 件（claim から worker・review・landing・push・`ended` まで。`session_turns` 8 件、`review_finished` の本文 1.5 KB、`receipt_observed` の要約 600 bytes を含む） |
| task | 3% | 1 run に `task_created` と `task_status_changed` 2 件 |
| session | 0.4% | 5 run ごとに runtime の planner の `session_opened`／`session_closed` |

開いている行は、走っている run 4 件（claim と worker の段だけ）と、最後の supervisor の一生（止まっていない）。
payload の平均は 100 bytes。大きさの感度を見るため、10 万件で全 payload に 400 bytes を足した列（平均 509 bytes）も測った。

大きさは確定済みの event だけを変えた 3 つ: 20,075 件（run 192）・100,017 件（run 957）・400,037 件（run 3,828）。ほかに 100,017 件の payload を大きくしたものを 1 つ。
各 queue は `prepare` が event を書いた後、前の supervisor として台帳を最後まで作り（2 万件で 61 pass・6 秒、40 万件で 1,212 pass・364 秒）、store が行を確定済みで持つ状態にした。`measure` は再起動した supervisor として新しい `LiveHistory` と `Builder` から始める。

### 打ったコマンド

```sh
cargo build --release --example supervisor_startup_memory
E=target/release/examples/supervisor_startup_memory
$E prepare "$TMPDIR/q20k" 20000
$E prepare "$TMPDIR/q100k" 100000
$E prepare "$TMPDIR/q400k" 400000
$E prepare "$TMPDIR/q100kp" 100000 400
for q in q20k q100k q400k q100kp; do $E measure "$TMPDIR/$q"; done
```

## 結果

### 段ごとの使用中とピーク

単位は MiB と個。「ピーク」はその段の間の最大。

| 段 | 2 万件 使用中 | 2 万件 個数 | 2 万件 ピーク | 10 万件 使用中 | 10 万件 個数 | 10 万件 ピーク | 40 万件 使用中 | 40 万件 個数 | 40 万件 ピーク | 40 万件 ピークの個数 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 判定の観測の最初の pass の後（`LiveHistory` を残す） | 0.0 | 207 | 5.9 | 0.2 | 972 | 6.2 | 0.9 | 3,843 | 7.1 | 41,805 |
| 台帳 pass 1: 全 event を印した後（行を組む前） | 19.3 | 139,510 | 25.1 | 106.3 | 695,240 | 112.2 | 425.2 | 2,780,952 | 431.1 | 2,818,663 |
| 台帳 pass 1: `rows_to_build` と `Inputs::of` の行を持つ間（ピークはそれを drop するまで） | 50.9 | 331,434 | 62.0 | 263.9 | 1,651,919 | 308.4 | 1,055.9 | 6,607,783 | 1,233.9 | 6,607,783 |
| 台帳 pass 1: 行を組み sweep した後 | 4.7 | 41,416 | 62.0 | 23.4 | 206,455 | 308.4 | 94.1 | 828,881 | 1,233.9 | 6,853,553 |
| 台帳 pass 2 | 4.7 | 41,416 | 4.7 | 23.3 | 206,321 | 24.1 | 94.1 | 828,875 | 99.2 | 865,337 |
| 追いつきの後（印が無くなるまで、3〜5 pass） | 4.7 | 41,416 | 4.7 | 23.3 | 206,321 | 23.3 | 93.6 | 825,214 | 99.0 | 861,707 |
| 全ての保持を drop した後 | 0.0 | 3 | — | 0.0 | 3 | — | 0.0 | 3 | — | — |

pass 1 は判定の観測と同じく全 event をページ（`EVENTS_PER_READ` = 5,000）ごとに読む。
判定の観測のピークはページの大きさで決まり 6〜7 MiB でほぼ一定だった。
この段は `build_pass` の前半（印と `rows_to_build`・`Inputs::of`）だけを写し、node・step・資源の読みと `due` は流さない。
台帳の pass 1 のピークは「全 event を印した後」（`Kept` が queue と session の event 全部を `Value` の木のまま持つ）に `Inputs::of` が全ての session・queue の行を組んだ写しが重なったところで、行を書いた数は 0（store が確定済みで持つ）なので store に書く途中の写しは出ていない。
40 万件の最大の resident は 1.35 GB。

payload を大きくした 10 万件（平均 509 bytes）: 印した後 133.7 MiB、`Inputs::of` の間 318.7 MiB、ピーク 363.2 MiB、追いつきの後 23.5 MiB・207,087 個。追いつきの後の量はほぼ変わらず、ピークは 18% 増えた。

### 係ごとの内訳（追いつきの後）

| 候補 | 保持 | 2 万件 MiB | 10 万件 MiB | 40 万件 MiB | 40 万件 個数 |
| --- | --- | --- | --- | --- | --- |
| (a) | `LiveHistory`（drop で減った量。`works` の run ごとの `WorkClock` など） | 0.03 | 0.23 | 0.89 | 3,841 |
| (b) | `Kept` の `queue_rows`（`QueueBook.rows` ほか） | 3.60 | 17.95 | 71.79 | 494,936 |
| (b) | `Kept` の `dropped` | 0.75 | 3.76 | 15.21 | 268,406 |
| (b) | `Kept` の `pre_claim`（`PreClaimBook`） | 0.19 | 0.92 | 3.69 | 43,385 |
| (b) | `Kept` の残す event（sessions・queue・pushes） | 0.07 | 0.34 | 1.35 | 9,184 |
| (b) | `Kept` の `session_rows`・`lives`・push の要約 | 0.02 | 0.08 | 0.32 | 5,460 |
| (b) | `Kept` の全体（drop で減った量） | 4.64 | 23.12 | 92.67 | 821,367 |
| (c) | `Builder` の marks・touched・settled ほか（`Kept` を除く。drop で減った量） | 0.00 | 0.00 | 0.00 | 2 |
| (c) | `rows_to_build` の行の鍵と `Inputs::of` の行の写し（pass の中だけ。印した後との差） | 31.6 | 157.6 | 630.7 | 3,826,831 |
| (d) | store に書く途中の写し | 書いた行 0 で出ない | 同 | 同 | — |
| (e) | どれにも属さない取り残し（全て drop した後） | 0.0 | 0.0 | 0.0 | 3 |

`Kept` の自前の見積もり `KeptSize.bytes` は 40 万件で 57.3 MB（54.7 MiB）で、allocator で数えた 92.67 MiB の 0.59 倍だった（テキストの長さで数え、`BTreeMap` の節と allocator の分を含まないため）。
40 万件の `queue_rows` は 233,891 行（1 行あたり 322 bytes・2.12 個）、`dropped` は 230,063 行（1 行あたり 69 bytes・1.17 個）。

### 傾きと 110 万件への外挿

傾きは 10 万件と 40 万件の差（確定済みの event 300,020 件）から求め、外挿は 40 万件の値に 70 万件分を足した。

| 量 | event 1 件あたり bytes | event 1 件あたり個数 | 110 万件の外挿 | 本番（goal 241）との比 |
| --- | --- | --- | --- | --- |
| 追いつきの後の使用中 | 245.7 | 2.06 | 258 MiB・227 万個 | bytes 4%・個数 3.5% |
| うち `queue_rows` | 188.2 | 1.24 | 197 MiB | — |
| うち `dropped` | 40.0 | 0.67 | 42 MiB | — |
| うち `pre_claim` | 9.7 | 0.11 | 10 MiB | — |
| うち `LiveHistory` | 2.3 | 0.01 | 2.5 MiB | — |
| 台帳 pass 1 の全 event を印した後 | 1,114.5 | 6.95 | 1,169 MiB・765 万個 | — |
| 台帳 pass 1 の `rows_to_build` と `Inputs::of` の行の写し | 1,653.6 | 9.57 | 1,735 MiB | — |
| 台帳 pass 1 のピーク | 3,234.6 | 17.12 | 3,393 MiB・1,884 万個 | bytes 57%・個数 29% |
| 判定の観測の最初の pass のピーク | ほぼ一定（6〜7 MiB） | ほぼ一定 | 約 8 MiB | — |

本番の 6.2 GB は 5.77 GiB、6,400 万個として比べた。
ピークの個数は 2 万件 351,166・10 万件 1,718,792・40 万件 6,853,553。
本番の `KeptSize`（`queue_rows` 78 万・`dropped_rows` 70 万）に合成の 1 行あたりを当てると、`queue_rows` 240 MiB と `dropped` 46 MiB で、`KeptSize` 260 MB を 0.59 で割り戻すと約 420 MiB で、どちらの見積もりでも 0.5 GiB に届かない（本番が残す約 9.4 万件の event を 1 件 1.1 KB として足しても約 100 MiB）。

## 判定

**holds**。

- 追いつきの後に持ち続ける量は、合成では 3 つの大きさとも同じ形で、`Kept` が 99%（`queue_rows` 77%・`dropped` 16%・`pre_claim` 4%）、`LiveHistory` が 1%、`Builder` の残りと取り残しは 0 だった。
- その量は確定済みの event の数に比例する（2 万・10 万・40 万件で event 1 件あたり 244〜246 bytes・2.06 個）。payload の大きさには依らない（10 万件で 1 つの大きさを比べた範囲）。確定済みの queue の event 1 件ごとに `QueueBook.rows` の 1 行（`Assigned`。records の id の Vec と `about` の String を持つ）と、行を落とした印の `dropped` の 1 要素（`(LedgerKind, String)`）が残り、刈り込みが無いため。
- 追いつきの間のピークも履歴の総量に比例する（event 1 件あたり 3.2 KB・17 個）。最初の pass が、queue と session の event 全部を `Kept` に `Value` の木のまま持ったうえで（1.1 KB・7 個）、全ての session・queue の行を `Inputs::of` で写す（1.7 KB・9.6 個）ため。sweep の後に解放される。
- 判定の観測（`LiveHistory`）はページの大きさで決まり、持ち続ける量も event 1 件あたり 2 bytes 余りで、候補から外れる。

ただし、この比例する保持は 110 万件で約 0.26 GiB（`KeptSize` から割り戻しても 0.5 GiB 未満）で、本番の 6.2 GB・6,400 万個の大半ではない。
本番の量の桁に届くのはピーク（3.3 GiB・1,900 万個。本番の payload が大きければもっと増える）のほうで、本番でそれが解放されずに残る理由はこの合成では再現しなかった（全てを drop した後の取り残しは 0）。
したがって「追いつきの後に持ち続ける確保量が履歴に比例する特定の保持にある」は、合成の `SqliteQueue` の経路では台帳の係の `Kept` の `queue_rows` と `dropped` について成り立つ。
本番の 6.2 GB の残りの 9 割以上はこの合成では再現しなかった。候補は、pass 1 のピーク（この経路の中にあり本番の 57% の桁に届く）が本番では解放されないか重なること（係の並走、macOS の malloc）と、この Spike の経路の外（queue service の経路・本番の event の種類と payload）で、下の推奨の 3 で測る。

## 推奨

1. draft 3507 で直す係は台帳の係（`src/domain/ledger/builder.rs` の `Kept`、`src/domain/ledger/queue.rs` の `QueueBook`、`src/application/supervise/observation/ledger.rs` の `pass`・`sweep`）で、`LiveHistory`（`src/domain/live_alerts/history.rs`）は、推奨の 3 の queue service の経路でも小さいと分かれば直さなくてよい。
2. 直し方の案（goal 241 の受け入れ条件 1・2 にそれぞれ当たる）:
   - 持ち続ける量（条件 1）: 確定済みの queue の行を `QueueBook.rows` から外す（後の event が届きうる行、つまり開いている supervisor の一生・stream・alert・finding に属する行だけを残し、1 件で閉じる行は sweep で確定を見たら落とす）。`dropped` は確定済みの行の印を全部持たず、後の event が届きうる行の範囲（開いている一生の行など）だけを持つか、届いたときに store の `ledger_row` で確定済みかを確かめる形にする。
   - ピーク（条件 2）: 最初の pass が全 event を読んでから組むのでなく、ページ（`EVENTS_PER_READ`）ごとに印・組み・sweep を進め、そのページの行のうち store が確定済みで持ち畳み直しても書かないものの event を、次のページを読む前に `Kept` から落とす。`Inputs::of` は一度に全ての session・queue の行を写さず、組む対象（`CHECKS_PER_PASS` の分）に限る。`Kept` の event を `Value` の木で持つことの見直しは、ページごとに落とせば履歴に比例しないので後でよい。
   - 回帰の確かめは、この example の `measure` の「追いつきの後」と「ピーク」の行を 2 つの大きさで比べる（goal の受け入れ条件の test は process の中で数える test を別に書く）。
3. 本番の 6.2 GB の残りを切り分けるために、次に測るもの:
   - queue service の経路: 本番の supervisor は queue を queue service 経由で読む（`InProcessCaller`／`SocketCaller`、`src/compose/supervisor.rs`）。応答の JSON の写しと `supervisor_service::resend::Kept`（受け取られなかった記録の保持）を含め、同じ合成の queue をその経路で読んで、追いつきの後とピークを比べる。
   - 係の並走: 判定の観測（主の thread）と台帳の係（job の thread）と他の周期の観測が起動の直後に同時に全件を読むときの、ピークの重なりと macOS の malloc が使用中として残す量。
   - 本番の event の種類の比と payload の大きさ: 本番の `stats`／`events` の種類ごとの件数と payload の長さの分布を人か inbox が読み、合成の比を合わせ直す。
   - 本番での確かめ: 直しの着地と自動更新の後に、inbox か人が本番の supervisor の footprint と `KeptSize` の log を見る（worker は host を測らない）。

## start_builder の経路の測定（task 4263）

本番の印のある store の起動は `start_builder` から始まるので、前の節までのカーソル 0 の経路に加えてこの経路を測った。
前の supervisor が台帳を作り終えて印を書いた store を起動し、判定の観測の最初の pass の後に `start_builder` を呼び、追いつくまで pass を回す。

判定する量は 3 つで、どれも起動の前（判定の観測の最初の pass の前）の使用中からの差。

- (a) `start_builder` の呼び出しの間のピーク
- (b) 呼び出しの後から追いつきの終わりまでのピーク
- (c) 追いつきの後の kept

結論: 直した後の傾きは 3 つとも上限以内で、110 万件の外挿は (a) 74 MiB、(b)(c) 14 MiB だった。
測る前の実装では (b) が上限を超えたので、下の「超えた傾きと直し」の 2 件を直した。

### 環境

- commit: `86d700bf5296d4c0ee0062cb00b3bf1b7b80f445`（この run の例と src の直し。この節を足す前の commit）
- rustc: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`--release` の build
- host: 前の「環境」と同じ
- 測った時刻: 2026-11-05 13:16:07〜13:16:47 UTC（prepare は 13:01:14〜13:10:20 UTC）

```sh
cargo build --locked --release --example supervisor_startup_memory
E=target/release/examples/supervisor_startup_memory
$E prepare "$TMPDIR/q20k" 20000
$E prepare "$TMPDIR/q100k" 100000
$E prepare "$TMPDIR/q400k" 400000
for q in q20k q100k q400k q100k; do $E measure "$TMPDIR/$q"; done
```

合成の列は前の節と同じで、最後の supervisor の life だけを大きさによらず 10 run に揃えた（`OPEN_LIFE_RUNS`）。開いている行の量を揃えるためで、event 数は前の節と 4 件ずつ違う。
`measure` の時刻は `prepare` が書いた最後の event の 2 日後なので、猶予（1 週間）の内の行は確定しておらず、2 万件はほぼ全部がそれに当たる。傾きは両方に同じ量がある 10 万件と 40 万件から求めた。

周回は各大きさ 1 回で、確かめに 10 万件を 2 回流した。(a)(b)(c) の差は 0 bytes（1% 以内）で、3 回にしなかった。

### 表 1: 大きさごとの段

| 大きさ | event 数 | 段 | 使用中 MiB | 使用中の個数 | ピーク MiB | ピークの個数 |
| --- | --- | --- | --- | --- | --- | --- |
| 2 万件 | 20,079 | 判定の観測の最初の pass の後 | 0.0 | 210 | 1.0 | 6,599 |
| 2 万件 | 20,079 | (a) `start_builder` の呼び出しの後 | 0.2 | 1,662 | 1.5 | 13,091 |
| 2 万件 | 20,079 | (b)(c) 追いつきの後 | 0.2 | 1,663 | 0.3 | 1,773 |
| 2 万件 | 20,079 | 全ての保持を drop した後 | 0.0 | 6 | 0.2 | 1,663 |
| 10 万件 | 100,021 | 判定の観測の最初の pass の後 | 0.2 | 975 | 4.8 | 32,609 |
| 10 万件 | 100,021 | (a) `start_builder` の呼び出しの後 | 1.3 | 8,143 | 6.7 | 64,094 |
| 10 万件 | 100,021 | (b)(c) 追いつきの後 | 1.3 | 8,144 | 1.3 | 8,254 |
| 10 万件 | 100,021 | 全ての保持を drop した後 | 0.0 | 6 | 1.3 | 8,144 |
| 40 万件 | 400,041 | 判定の観測の最初の pass の後 | 0.9 | 3,846 | 20.1 | 130,223 |
| 40 万件 | 400,041 | (a) `start_builder` の呼び出しの後 | 5.0 | 32,461 | 26.8 | 255,506 |
| 40 万件 | 400,041 | (b)(c) 追いつきの後 | 5.0 | 32,462 | 5.0 | 32,572 |
| 40 万件 | 400,041 | 全ての保持を drop した後 | 0.0 | 6 | 5.0 | 32,462 |

ピークはその段の間の最大で、段ごとに今の使用中へ戻す。bytes の値は (a) 1,543,308・7,037,726・28,146,824、(b) 271,186・1,331,085・5,278,262、(c) 257,240・1,317,139・5,264,315（2 万・10 万・40 万件の順）。

### 表 2: 読んだ数

数える読みは起動の前に別に流し、確保量の段に混ぜていない。

| 大きさ | event 数 | store の queue 行 | 読んだ queue 行の頭 | 読んだ session 行の頭 | 読んだ node 行の頭 | push の記録 | pass の数 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 2 万件 | 20,079 | 11,735 | 582 | 38 | 0 | 192 | 1 |
| 10 万件 | 100,021 | 58,476 | 2,896 | 191 | 0 | 957 | 1 |
| 40 万件 | 400,041 | 233,895 | 11,581 | 765 | 0 | 3,828 | 1 |

読んだ queue 行の頭は store の行の 5% で、run 1 件あたり 3 行（待ち 2 行と landing slot の attempt）と life ごとの stream の行。

### 表 3: 傾きと外挿

- event の傾き = (40 万件の値 − 10 万件の値) ÷ (40 万件の event 数 − 10 万件の event 数)
- 110 万件の外挿 = 40 万件の値 + event の傾き × (1,100,000 − 40 万件の event 数)
- 行の傾き = (40 万件の値 − 10 万件の値) ÷ (40 万件の store の queue 行 − 10 万件の store の queue 行)
- 78 万行の外挿 = 40 万件の値 + 行の傾き × (780,000 − 40 万件の store の queue 行)

| 量 | event 1 件あたり bytes | queue 行 1 行あたり bytes | 110 万件の外挿 MiB | 78 万行の外挿 MiB | 受け入れ 4 の上限 MiB | 上限以内か |
| --- | --- | --- | --- | --- | --- | --- |
| (a) | 70.4 | 120.3 | 73.8 | 89.5 | 128 | はい |
| (b) | 13.2 | 22.5 | 13.8 | 16.8 | 128 | はい |
| (c) | 13.2 | 22.5 | 13.8 | 16.7 | 64 | はい |

check の判定はこの表とは別に、check の 2 つの queue（6,077 件と 24,363 件、差 18,286 件）の値で `large − small ≤ 上限 × (large の event 数 − small の event 数) ÷ 1,100,000` を計算する（`within_cap`）。

### 超えた傾きと直し

直す前の check は (b) が 6.85 MiB 増え、許容の 2.13 MiB を超えた（393 bytes/件、110 万件で約 430 MiB）。(a) は許容以内だったが、40 万件の測定で 101 bytes/件、78 万行の外挿が 128.9 MiB で上限をわずかに超えた。どちらも判定の意味と台帳の行を変えずに小さく直せた。

- (b): 作り直した `QueueBook` が、確定して終わった stream の行も次の close で読み直す印を持っていた。起動の最初の pass が life ごとの stream の行を全部畳み直し、その records を同時に持った。その行は畳み直しても同じなので、確定して終わった行には印を付けない（`QueueBook::from_heads`）。その supervisor を名指す event が後から来れば今どおり読み直す。
- (a): ピークは読んだ queue 行の頭と、`Cut::heads` が全部を写した分だった。何も位置より先に無い行は写さず借りる（`Cut::heads_held`）。傾きは 101 から 70 bytes/件に下がった。

### 別に見つけたこと

判定の観測の最初の pass のピークは 1.0・4.8・20.1 MiB で履歴に比例して増えていて（約 53 bytes/件、110 万件で約 56 MiB）、前の節の 6〜7 MiB でほぼ一定とは違う。これは (a)(b)(c) の外で、カーソル 0 の check は台帳の pass のピーク（約 12 MiB）に隠れて見えない。原因はここでは調べていない。
