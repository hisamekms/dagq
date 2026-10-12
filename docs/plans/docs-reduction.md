---
id: plan-docs-reduction
type: plan
title: design 文書の削減（A・C）の前後比較に使う run の review の差し戻しの指標（M7）の定義と、比べる区間・参考の値・想定効果
status: active
created: 2026-10-13
owners:
  - hisamekms
tags:
  - documentation
  - measurement
related:
  - plan-docs-slim
  - development-documents
---

# design 文書の削減の前後比較の指標（M7）

goal 194（request 103）の A（書かないものの一括削除）と C（review の差し戻しの範囲）の前後で、run の review の差し戻しの回数・待ち・review の回数・worker の作業時間を同じ取り方で比べるための定義。
M7 は `scripts/docs-metrics.py` が数え、`scripts/docs-metrics-test.py` が fixture で確かめる。
M1〜M6 の定義と基準値は [docs/design の4指標](docs-slim.md) が持ち、この文書は変えない。

## 測り方

```sh
dagq events --full --kind review_finished --kind review_started --kind run_integrated \
  --since <since> --until <until> --limit 1000 --after <cursor> > events-run-<頁>.json
dagq stats --full --since <since> --until <until> > stats.json
python3 scripts/docs-metrics.py --since <since> --until <until> --metrics m7 \
  --run-events events-run-*.json --stats stats.json --format json
```

- `--run-events` は `dagq events` の JSON を保存したファイル（複数可。頁送りの各頁）。全てのファイルを通して event ID が同じものは1件（最初に読んだもの）で、`created_at`（UTC）が `[since, until)` に入るものだけを数える。
- `--stats` は M3 と同じ渡し方で、M7 は worker の作業時間だけをそこから読む。省くと `worker_work` は `null`。
- `--run-events` を省くと M7 は `null`。script は queue を読まない。

## M7 の定義

規則は planner が request 103 の測定の要求から決めたもの（task 1689 と 2289 の定義を元にした）で、下の参考の値に合わせて調整しない。

- **(a) review の回数**: 期間の `review_finished` の event の数（`attempt` を問わない）と、その `run_id` の異なる数。
- **(b) 差し戻し**: `review_finished` のうち、payload に `route` があれば `route.destination` が `send_back` のもの、`route` が無ければ `verdict` が `revise` のもの。
  `route` のある event は `verdict` によらず `route` で決める（`revise` でも `route` が `ask` なら差し戻しではない）。
  `verdict` が `concern` で `route` の無いものは差し戻しに数えず、件数を `concern_without_route` に出す。
  `route` で決まったものと `verdict` で決まったものの件数も別に出す。
- **(c) ドキュメントの理由**: 差し戻しの `reasons`（理由の文の配列）の添字 i の理由は、`reason_codes[i]`（同じ添字の理由の code の配列）が配列で `docs_drift` を含むか、文 `reasons[i]` が次の path を名指せばドキュメントの理由。
  - 名指しの path: `docs/` の下の path、`plugins/` の下の `.md`、root の `AGENTS.md`・`README.md`。語の始まりで始まるもの（前が ASCII の英数字・`_`・`.`・`-`・`/` でない。日本語の文字は前後に接してよい。先頭の `./` は許す）だけで、`src/docs/...` や `foo/README.md` は名指しに数えない。
  - code の欠け: `reason_codes` が無いか配列でない event は、どの理由も code を持たないとして文の名指しだけで決める。`reason_codes` が `reasons` より短く添字 i の code が無い理由と、`reason_codes[i]` が配列でない理由も、文の名指しだけで決める。`reason_codes` が `reasons` より長いときの余りの code は理由に数えない。
  - 形の崩れの件数: 差し戻しのうち `reason_codes` が無い（配列でない）event・`reasons` より短い event・長い event の数と、配列でない `reason_codes[i]` の理由の数を、それぞれ別の欄（`reason_codes`）に出す。
  - ドキュメントを含む差し戻し（`docs_any`）: ドキュメントの理由を1つ以上持つ差し戻し。
  - ドキュメントだけの差し戻し（`docs_only`）: 理由が1つ以上で、全部がドキュメントの理由の差し戻し。`reasons` が無いか空の差し戻しはどちらにも数えず、件数を `without_reasons` に出す。
- **(d) 差し戻しの待ち**: 差し戻しの `review_finished` の `created_at` から、同じ run の次の `review_started`（`created_at`、同じ時刻なら event ID の順で後のもの）の `created_at` まで。
  期間の中に次の `review_started` が無ければ期間の終わりで打ち切り、打ち切りの件数を `cut` に出して合計と中央値に含めない。
  全体・ドキュメントを含む・ドキュメントだけの3つについて、合計（秒と時間）・中央値（秒）・件数。
- **(e) 着地の数**: 期間の `run_integrated` の event の数。
- **(f) worker の作業時間**: `dagq stats --full` の JSON の `runs[].work`（`run_claimed` から最初の `receipt_observed` までの秒。2026-10-13 の `src/domain/stats.rs` の `RunStats` で確かめた）の、`null` を除いた中央値と件数。
  run を選ぶのは `dagq stats` の `--since`・`--until`（run の終わりの時刻で選ぶ。`--since` は含まず `--until` は含む）で、events の `[since, until)` と境界が違う。
- **(g) 比**: review あたり（差し戻し・ドキュメントを含む・ドキュメントだけの回数 ÷ review の回数）、着地あたり（review の回数 ÷ 着地の数、待ちの合計の時間 ÷ 着地の数）、1日あたり（review・差し戻し・ドキュメントを含む・ドキュメントだけ・待ちの時間・着地 ÷ 期間の日数）。

## 比べる区間

期間の長さが違うので、回数と待ちは1日あたり・review あたり・着地あたりの値を並べて比べる。
着地の時刻は、その task の run の `run_integrated` の event の `created_at`。

| 区間 | since | until |
| --- | --- | --- |
| 基準 | 2026-10-05T15:03:00Z | 2026-10-12T00:38:00Z |
| A の前 | A の task（2335〜2351）の最初の着地の 6.4 日前 | その着地 |
| A の後 | A の task の最後の着地 | C の task 2353 の着地 |
| C の後 | 2353 の着地 | 2353 の着地の 6.4 日後 |

基準の区間は request 103 の人の分析と同じ期間で、script で数え直した値を基準にする。

## 参考の値（人の session の分析）

request 103（2026-10-12、人の dagq session の分析）が基準の区間について出した値。

| 値 | 人の分析 |
| --- | --- |
| review の回数 | 692 |
| 差し戻し | 320 |
| ドキュメントの指摘を含む差し戻し | 281 |
| ドキュメントの指摘だけの差し戻し | 134 |
| 差し戻しから次の review までの待ちの合計 | 204 時間 |
| うちドキュメント分 | 約 123〜138 時間 |
| 着地 | 287 |
| worker の作業時間の中央値 | 890 秒 → 1268 秒（task 1688 の前後。交絡あり） |

取り方（差し戻しの判定・ドキュメントの指摘の判定・待ちの端・対象の event）は記録に残っておらず（`~/.local/share/dagq-hostmetrics/human/` に無い）、上の M7 の定義と同じとは限らないので、比べられる基準値ではない。
差は script の値と並べて書き、定義はこの値に合わせない。

## 想定効果

人の分析の見込みで、前後の値で確かめる。

- ドキュメントの指摘だけの差し戻し: 6.4 日で 134 回から 35〜55 回に。
- 差し戻しの待ち: 80〜100 時間減。
- review の回数: 12〜14% 減。
- 着地の数: +2〜7%。

## 前後の値

基準・A の前後・C の後の script の値は、同じ goal の measure の task が上の区間で script を流してここに書く。
worker は本番の queue を読まないので、この文書を足した task は値を書かない。

## 区分

- SSOT: queue の EventStore（run の event）。M7 は git の履歴も会話記録も読まない。
- 入力: `dagq events --full` と `dagq stats --full` の JSON は EventStore から作り直せるビュー。EventStore の該当の期間の event が残る限り同じ入力を作り直せる。
- 出力: script の JSON の `m7` と Markdown の表は、入力から `scripts/docs-metrics.py` で作り直せるビュー。
- 人の分析の参考の値は材料で、取り方が残っていないので作り直せない。
- 保存する JSON の置き場は [docs/design の4指標](docs-slim.md) の「保存」と同じ dir（`~/.local/share/dagq-hostmetrics/docs-slim/`）に、入力を `events-run-<since>_<until>-<頁>.json`、出力を `m7-<since>_<until>.json` として置く。
