---
id: plan-team-version-search-spike
type: plan
title: スパイク：PostgreSQL の pg_trgm・pg_bigm で dagq search と related の検索の手がかりが今の SQLite の FTS5 trigram と同じ結果を返せるかの実測
status: completed
created: 2026-11-04
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - persistence
related:
  - adr-0063
  - adr-t3981-1
  - adr-t3986-1
  - design-persistence
  - plan-team-version-write-leader
  - plan-team-version-sqlite-dialect
---

# スパイク：pg_trgm・pg_bigm は今の FTS5 trigram と同じ検索の結果を返せるか

goal 250（チームの版）の task 3991 の実測。
問いは、RDS for PostgreSQL で使える拡張（pg_trgm・pg_bigm）で、`dagq search` と `dagq related` の検索の手がかりが、今と同じ問い合わせで同じか許せる違いの結果を返せるか。

## 判定

**does_not_hold。**
A1（pg_trgm）・A2（pg_bigm）の両方を実の PostgreSQL 18.6 で動かし、どちらも基準 (c)・(d)・(e) を満たさなかった。

- 合致の集合は同じだった。拒否されなかった 42 件の全部で Jaccard が 1 で、2 文字の日本語の語・path の部分一致・大文字と小文字の畳み方・`%` `_` の escape に違いは無かった。
- 順位は同じにならなかった。長い語を含む問い合わせの上位 10 件の共通の割合の平均は A1 が 0.458、A2 が 0.485 で、基準の 0.8 に届かない。
- related の上位集合の共通の割合の平均は A1 が 0.485、A2 が 0.046 で、基準の 0.7 に届かない。
- FTS5 で上位 10 件に入る b は 26 組のうち 22 組で、そこから落ちる組は A1 が 2 組、A2 が 18 組で、基準の 1 組以下を超える。
- 重複の判定への影響: 見つけたい b が上位 10 件に入る組は FTS5 が 22 組、A1 が 21 組で、数ではほぼ同じだが、見せる候補の顔ぶれは半分ほど入れ替わる。A2 は 5 組まで減る。
- plan review の検索の候補（上位 5 件、参考の測定）に b が入る組は FTS5 が 14 組、A1 が 11 組、A2 が 3 組で、候補の共通の件数の平均は A1 が 1.77 件、A2 が 0.08 件。

根拠の制約は 2 つある。

- container は使えず、PostgreSQL は run の一時の directory に build して macOS の上で動かした（下の「環境」）。
  判定の基準は holds にだけ container を求めるので、does_not_hold の判定はできる。
  結論が container で変わらないと見る理由は「macOS で動かしたことの影響」に書く。
- 資料は git log と ADR で、本番の task の行ではない（acceptance・context の列が空）。

## 環境と使った時間

| 項目 | 値 |
|---|---|
| 対象の commit | `ea279b4118737cef3385be4a1441e4ef8c663994`（この run の base の main） |
| 資料の件数 | commit 2,226 件（`git log --no-merges`）、ADR 330 件（その commit の `docs/adr/*.md` の全件、`docs/adr/README.md` を含む）、合わせて 2,556 行 |
| PostgreSQL | 18.6（source から build、aarch64-apple-darwin27.2.0、Apple clang） |
| pg_trgm | 1.6（18.6 の contrib） |
| pg_bigm | tag `v1.2-20250903`（`pg_extension` の版は 1.2） |
| LC_CTYPE・LC_COLLATE | `en_US.UTF-8`（provider は libc、encoding は UTF8） |
| SQLite | 3.53.1（python の `sqlite3`）。dagq 本体は `libsqlite3-sys` 0.38.2 の bundled で、同じ版かは確かめていない |
| container | 使えなかった（下） |
| 使った時間 | 約 1 時間 25 分（上限 3 時間） |
| 試行の回数 | 2 回（上限 3 回）: container の確認 1 回、source の build 1 回（1 回で成功）。比べる周回は数えない |

container を使えなかった理由: この run の経路で `podman info` は接続できず、`podman machine list` と `podman system connection list` は空だった。
host に machine も接続も無いので、動かすには `podman machine init` が要る。
machine を作るのは host の操作で worker はしない（[operations](../development/operations.md#workerがhostと実queueでできないこと)）。
接続が無いまま init すると新しい接続が既定になり（[container-runtime-spike](container-runtime-spike.md)）、machine が動く間は dagq の machine も起動できない（[Container runtime](../design/container-runtime.md#dagq専用のmachine)）。
そこで machine は作らず、build した PostgreSQL を `$TMPDIR` の下だけに置いて 127.0.0.1 で動かした。host にツールは入れていない。

## RDS for PostgreSQL で使えるか

両方とも使える。公式の一覧（[Extension versions for Amazon RDS for PostgreSQL](https://docs.aws.amazon.com/AmazonRDS/latest/PostgreSQLReleaseNotes/postgresql-extensions.html)、2026-11-04 に確認）の値は次のとおり。

| RDS の版 | pg_bigm | pg_trgm |
|---|---|---|
| 18.6 | 1.2_20250903 | 1.6 |
| 17.11 | 1.2_20250903 | 1.6 |
| 16.15 | 1.2-20250903 | 1.6 |
| 15.19 | 1.2_20250903 | 1.6 |

この実測の版（18.6・pg_trgm 1.6・pg_bigm 1.2-20250903）は RDS の 18.6 の行と同じ。

文書と実測で確かめたこと（[pg_trgm](https://www.postgresql.org/docs/current/pgtrgm.html)、[pg_bigm](https://github.com/pgbigm/pg_bigm/blob/master/docs/pg_bigm_en.md)）。

- pg_trgm は語の文字（alphanumeric）で語に分ける。文書は locale に触れないので実測した。
  LC_CTYPE が `en_US.UTF-8` なら `show_trgm('着地の検証abc')` は日本語の trigram を返し、`C` なら日本語を捨てて `word_similarity('着地', '着地の検証')` が 0 になる。
  provider が builtin の `C.UTF-8` でも LC_CTYPE を `C` にして作った DB は同じく 0 だった。
  絞り込みの `ILIKE` は語の分け方に依らないが、順位の関数は DB を作るときの LC_CTYPE に依る。
- pg_trgm の GIN は `LIKE`・`ILIKE` に効くが、trigram を取り出せない語は索引の全走査に落ちる。pg_bigm の文書も 1〜2 文字の語で pg_trgm を Slow、pg_bigm を Fast とする。
- `bigm_similarity` は大文字と小文字を区別する（pg_trgm の similarity は区別しない）。
  絞り込みは `lower()` で畳むが、順位は description のとおり畳まずに測った。

## 測り方

script は [spike.py](team-version-search-spike/spike.py)、問い合わせは [queries.tsv](team-version-search-spike/queries.tsv)、組は [pairs.tsv](team-version-search-spike/pairs.tsv)、PostgreSQL の build は [build-pg.sh](team-version-search-spike/build-pg.sh)。
python は標準ライブラリだけで、接続先は `PG*` の環境変数で渡すので、container の PostgreSQL にもそのまま向けられる。

```sh
sh docs/plans/team-version-search-spike/build-pg.sh "$TMPDIR/w"
"$TMPDIR/w/pg/bin/psql" -h 127.0.0.1 -p 55433 -U spike -d postgres -c 'CREATE DATABASE libc_en TEMPLATE template0'
PGHOST=127.0.0.1 PGPORT=55433 PGUSER=spike PGDATABASE=libc_en \
  python3 -I docs/plans/team-version-search-spike/spike.py --repo . \
  --commit ea279b4118737cef3385be4a1441e4ef8c663994 --psql "$TMPDIR/w/pg/bin/psql" --out OUT
```

### 資料

両方の索引に同じ行を入れた。

- commit 1 件は task 相当の行で、title は件名、description は本文、`updated_at` は committer の日時（UTC に揃えた文字列）、rowid は 4×n（n は日時の古い順、同じ秒は sha の順、1 始まり）。
- ADR 1 件は note 相当の行で、title は最初の `# ` の見出し、text は frontmatter を除いた本文、`updated_at` はその file を最後に変えた commit の日時、rowid は 4×n+2（n は file 名の順）。
- SQLite は migration 0026 の `search_index` と同じ `fts5(..., tokenize = 'trigram')` を一時の DB に作った。
- PostgreSQL は `search_docs` の doc と td の 2 つの式に GIN（`gin_trgm_ops`、`lower()` の式に `gin_bigm_ops`）を作った。

### search の経路

解釈は `parse_terms`・`tokens` を写して両方に通した。1〜2 文字の語を OR・NOT・括弧と組み合わせると拒否する。

| adapter | 絞り込み | 順位 |
|---|---|---|
| FTS5（今） | `search_index MATCH 式`（3 文字以上の語を `"語"` に括る）と、短い語ごとの `LIKE '%語%' ESCAPE '\'` | 長い語があれば `bm25(search_index), rowid DESC`、短い語だけなら `updated_at DESC, rowid DESC` |
| A1 pg_trgm | 語 → `doc ILIKE '%語%'`、並び・AND → AND、OR → OR、`a NOT b` → `a AND NOT b` | 長い語ごとの `word_similarity(語, doc)` の和の降順、同点は rowid DESC。短い語だけなら `updated_at DESC, rowid DESC` |
| A2 pg_bigm | A1 と同じ式で語 → `lower(doc) LIKE likequery(lower(語))` | 長い語ごとの `bigm_similarity(語, doc)` の和の降順、同点は rowid DESC。短い語だけなら A1 と同じ |
| A3（参考） | A1 と同じ | A1 の絞り込みの行に script の中で FTS5 と同じ bm25 をかける（列の重み 1、k1 1.2、b 0.75、D と avgdl は trigram の数、N と n は索引全体） |

description に無く、測るために決めたことは 4 つ。

- `doc ILIKE` の語の `%` `_` `\` は短い語と同じく escape した。しないと `plan_review` の `_` が任意の 1 文字になる。
- FTS5 の優先順位（並び > NOT > AND > OR）に合わせて式を組み立てた。`a NOT b c` は `a AND NOT (b AND c)` になる。
- NOT の後ろの語も「長い語」として順位の和に入れた（`parse_terms` の `long` と同じ）。
- `SET enable_seqscan = off` で流し、表の走査でなく索引の経路の結果を測った。`updated_at` の順は `COLLATE "C"` にした。

### related の経路

- 組は git log を読んで選んだ。同じ節を直した docs（same_section）、同じ test を直した commit（same_test）、同じ task を原因に別の文書を直した docs（same_cause）、同じ向きの変更を分けた commit（same_change）。
  候補は件名の中の節の名前・test の名前と、読んで同じ変更と見たもので、FTS5 の順位では選んでいない。
- FTS5 は `trigram_query` と同じ規則で `{title description} : ("g1" OR "g2" …)` を作り、`kind='task'` の行の bm25 を a 自身の bm25 で割って 0〜1 に収めた。a 自身の値が負でなければ手がかり無し。
- A1 の強さは `word_similarity(a.title, x.td) ÷ word_similarity(a.title, a.td)`、A2 は同じ式を `bigm_similarity` で、どちらも 0〜1 に収めた。
- 点と順位は `score_pair`・`rank` のとおり。強さは 1 を上限に切り、0.1 未満なら手がかり無し、重みは round(4.0×強さ)、候補の点は round(重みの和)（round は小数 3 桁で、0.5 は 0 から遠い側へ）。点が 0 の候補と a 自身を除き、点の降順、同点は rowid の降順に並べる。
- 検索以外の手がかり（paths・file 名・test 名・ADR・task 番号・follow-up・goal）は 0 に固定した。これらは索引を読まず、どの adapter でも同じ値になるため。
  他の手がかりを足したときの候補の点の差は丸める前の強さの差ではなく、閾値と丸めの後の検索の重みの差に等しいので、|強さの差| と |重みの差| を別の列にした。
- 上位集合は点が 0 より大きい候補の先頭 10 件、共通の割合は共通の件数 ÷ 2 つの上位集合の件数の大きい方（両方とも空なら 1）。

### 2 回の一致

同じ資料で script を 2 回流し、出力の 6 つの CSV（`search`・`related`・`criteria`・`meta`・`search_top`・`plan_review`）は 2 回とも完全に一致した（`cmp` で確かめた。時間の `timings.txt` は比べない）。

## 基準の値

| 基準 | A1 pg_trgm | A2 pg_bigm | A3（参考、判定に使わない） |
|---|---|---|---|
| (a) 合致の集合の Jaccard が 1 でない問い合わせ（2 件以下で合格） | 0 件: 合格 | 0 件: 合格 | 0 件 |
| (b) 短い語だけの問い合わせ 9 件で同順位が揃わないもの（0 件で合格） | 0 件: 合格 | 0 件: 合格 | 0 件 |
| (c) 長い語を含む 33 件の共通の割合の平均（0.8 以上） | 0.458 | 0.485 | 1.000 |
| (c) 共通の割合が 0.6 未満の問い合わせ（0 件で合格） | 20 件: 不合格 | 19 件: 不合格 | 0 件 |
| (d) FTS5 の上位 10 件の b が落ちる組（1 組以下で合格） | 2 組: 不合格 | 18 組: 不合格 | 0 組 |
| (e) 上位集合の共通の割合の平均（0.7 以上で合格） | 0.485: 不合格 | 0.046: 不合格 | 1.000 |
| 問い合わせと組の件数 | 46 件・26 組: 届いた | 46 件・26 組: 届いた | 同じ |
| 2 回の出力の一致 | 一致 | 一致 | 同じ |

related の |強さの差| と |重みの差| の集計（A3 は 3 つとも 0）。

| | A1 | A2 |
|---|---|---|
| \|強さの差\|（丸める前）の最大・平均 | 0.212・0.101 | 0.930・0.358 |
| \|重みの差\|（閾値と丸めの後）の最大・平均 | 1.039・0.427 | 4.000・1.457 |
| 閾値の 0.1 をまたいだ組 | 2 組 | 2 組 |
| 上位 10 件に b が入る組（FTS5 は 22 組） | 21 組 | 5 組 |
| 上位 5 件に b が入る組（FTS5 は 22 組） | 20 組 | 3 組 |

(d) で落ちた組: A1 は `e3fe20660/6d0bf8a2b`（FTS5 の 3 位が A1 の 12 位）と `c1acda878/300d61bcb`（5 位が 37 位）。
A2 は [criteria.csv](team-version-search-spike/criteria.csv) の `d_pairs` の 18 組。

## なぜ順位が揃わないか

順位の関数の意味が bm25 と違うので、調整で埋まる差ではない。

- `word_similarity` は語が文書のどこかにあれば同じ値に近づき、語の回数・語の珍しさ・文書の長さを見ない。
  `plan_review` では合致する 168 行のうち 160 行が 1.000 で並び（`why.txt`）、同点は rowid の降順なので新しい commit の順になる（共通 0 件）。
- `bigm_similarity` は文書全体との類似度なので、短い文書が勝つ。
  `plan_review` の上位 5 件は 636〜950 文字の行で、合致する行の平均は 5,661 文字（[why.txt](team-version-search-spike/why.txt)）。
- related の A2 は、a 自身の説明が長いと分母の `bigm_similarity(a.title, a.td)` が小さくなり、他の行の強さが 1 に近づく。b の強さの中央値は FTS5 の 0.397 に対して A2 は 0.705 で、順位を分けにくい。
- A3 は全部の問い合わせと組で FTS5 と順位まで一致した。絞り込みが同じで順位の式を bm25 にすれば同じ結果になるので、違いは順位の式だけから来る。

## macOS で動かしたことの影響

RDS は Linux で、今回は macOS の libc の `en_US.UTF-8` で動かした。結論が変わらないと見る理由は次のとおり。

- 絞り込み（(a)・(b)）は `ILIKE`・`LIKE` の結果で、日本語は畳み方の影響を受けない。
- A2 の不足（(d) 18 組、(e) 0.046）は `bigm_similarity` の式から来る。`LC_CTYPE=C` の DB でも A2 の値は変わらなかった。
- A1 は LC_CTYPE に依るが、`LC_CTYPE=C` の DB（日本語が語の文字でなくなる最も遠い場合）でも en_US.UTF-8 でも不合格で、値は (c) 0.448・(d) 4 組・(e) 0.342（[criteria-lc-c.csv](team-version-search-spike/criteria-lc-c.csv)。plan review の表を除いて 1 回だけ流した）。
  (c) の 0.6 未満には `plan_review`・`docs/adr` のような ASCII だけの語も多く、LC_CTYPE が違っても語の分け方は変わらない。

確かめていないのは glibc の `en_US.UTF-8` での実測で、A1 の (d) は 2 組と基準に近い。
ただ (c) と (e) は基準から大きく離れているので、(d) が変わっても判定は変わらない。

## 他に測ったこと

### 索引の経路の時間

同じ資料の `EXPLAIN (ANALYZE)` の結果は [explain.txt](team-version-search-spike/explain.txt)。

- `着地`（2 文字）: A1 は索引から全行（2,556 行）を返して 1,933 行を recheck で捨てる（約 142 ms）。A2 は 623 行だけ返す（約 0.4 ms）。
- `全文検索`: どちらも 5 行で約 0.8 ms。
- `docs/adr`: A1 は 1,219 行から、A2 は 717 行から 265 行に絞る（約 60 ms と約 39 ms）。

順位の関数は文書の全文を読むので重い。

- title で全 2,226 行を順位づける 1 回は、手で流した値で `word_similarity` が約 1.3 秒、`bigm_similarity` が約 0.7 秒（`why.txt` に取り直した値がある）。
- plan review の検索（語 20 個の OR）を A1 と A2 で 1 回ずつ流すと、1 組で約 45 秒かかった（[timings.txt](team-version-search-spike/timings.txt)）。
  負荷のある host の 1 回の値で、A3 の時間は分けて測っていない。

### plan review の検索の候補（参考）

`any_word_query` を写し、task 相当の行だけを探して a 自身を除いた上位 5 件（`DUPLICATE_CANDIDATES`）に b が入るかを測った。基準の外の参考の値で、列は `b が入るか（1 か 0）` と FTS5 の候補との共通の件数。

| 組 | 語の数 | FTS5 | A1 | A2 | A3 | A1の共通 | A2の共通 | A3の共通 |
|---|---|---|---|---|---|---|---|---|
| 1 | 20 | 1 | 1 | 0 | 1 | 2 | 0 | 5 |
| 2 | 20 | 1 | 0 | 0 | 1 | 0 | 0 | 5 |
| 3 | 20 | 0 | 0 | 0 | 0 | 1 | 0 | 5 |
| 4 | 20 | 1 | 0 | 1 | 1 | 2 | 1 | 5 |
| 5 | 20 | 0 | 1 | 0 | 0 | 4 | 0 | 5 |
| 6 | 20 | 0 | 0 | 0 | 0 | 1 | 0 | 5 |
| 7 | 20 | 1 | 1 | 0 | 1 | 2 | 0 | 5 |
| 8 | 20 | 1 | 0 | 0 | 1 | 0 | 0 | 5 |
| 9 | 20 | 1 | 1 | 0 | 1 | 3 | 0 | 5 |
| 10 | 20 | 1 | 0 | 0 | 1 | 2 | 0 | 5 |
| 11 | 20 | 0 | 0 | 0 | 0 | 1 | 0 | 5 |
| 12 | 20 | 1 | 1 | 0 | 1 | 2 | 0 | 5 |
| 13 | 20 | 0 | 0 | 1 | 0 | 3 | 0 | 5 |
| 14 | 15 | 0 | 0 | 0 | 0 | 2 | 0 | 5 |
| 15 | 20 | 1 | 1 | 0 | 1 | 1 | 0 | 5 |
| 16 | 20 | 1 | 1 | 0 | 1 | 1 | 0 | 5 |
| 17 | 20 | 0 | 0 | 0 | 0 | 2 | 0 | 5 |
| 18 | 20 | 0 | 0 | 0 | 0 | 2 | 0 | 5 |
| 19 | 20 | 1 | 1 | 0 | 1 | 3 | 0 | 5 |
| 20 | 20 | 1 | 0 | 0 | 1 | 1 | 0 | 5 |
| 21 | 20 | 1 | 1 | 0 | 1 | 3 | 0 | 5 |
| 22 | 20 | 0 | 0 | 0 | 0 | 1 | 0 | 5 |
| 23 | 18 | 0 | 0 | 0 | 0 | 1 | 0 | 5 |
| 24 | 20 | 0 | 1 | 0 | 0 | 3 | 0 | 5 |
| 25 | 10 | 1 | 1 | 1 | 1 | 3 | 1 | 5 |
| 26 | 20 | 0 | 0 | 0 | 0 | 0 | 0 | 5 |

## 推奨（後続の計画へ）

今回の判定は does_not_hold なので、検索の adapter を A1・A2 の順位の関数のまま書かない。

| 判定 | 検索の adapter の形 |
|---|---|
| does_not_hold（今回） | **絞り込みは pg_bigm の GIN（`lower(doc) LIKE likequery(lower(語))`）、順位はアプリの中で bm25 を計算する別の索引の形**にする（A3 の順位の式）。pg_bigm は 2 文字の語が速く、LC_CTYPE に依らない。統計（N・語ごとの n・行の trigram の数・avgdl）の持ち方と大きさの限界を adapter の task で決める。これで足りなければ外の検索 service を別の索引の候補にする |
| 順位の入れ替わりを人が受け入れるとき（基準を緩める判断） | pg_trgm の `word_similarity`（A1）が最も近い。b が上位 10 件に入る組は 21 組で FTS5 の 22 組とほぼ同じだが、候補の顔ぶれは半分入れ替わる。DB を LC_CTYPE が `C` でない形で作る条件が付く |
| holds だったとき（今回は当たらない） | 基準を満たした拡張（pg_trgm か pg_bigm）の順位の関数をそのまま使う |
| unresolved だったとき（今回は当たらない） | 人か inbox が RDS か container で同じ script を流す |

pg_bigm の順位（A2）は基準を緩めても採らない。b が上位 10 件に入る組が 5 組まで減り、重複の判定の手がかりとして弱い。

後続の task が決めることは次の 3 つ。

- A3 の統計を更新と検索のどちらで作るか。本番の行数での時間は測っていない。
- related の検索の手がかりは 200 個までの trigram の OR なので、絞り込みの行が多くなる。アプリで計算する量を上限つきで決める。
- 非 ASCII の大文字と小文字（FTS5 の畳み方と `lower()` の違い）はこの資料に出ないので、adapter の test で確かめる。

container や RDS での確かめは判定には要らない。glibc の値を取りたくなったら、上のコマンドの接続先を変えるだけで流せる。

## 限界

- 資料は commit の件名・本文と ADR の本文で、本番の task の acceptance・context・goal・着地の commit の行を含まない。worker は queue を読まない。
- 組は 26 組で、選んだのはこの run の worker 1 人の目。割合の精度は高くない。
- 問い合わせのうち合致 0 件のものは `重複の判定`・`SEARCH_WEIGHT`・`src/infrastructure/search.rs`・`migrations/0026_search.sql` の 4 件で、(a)・(c) では一致に数えた。これを除いても A1・A2 は (c) を満たさない。
- A1・A2 の順位は description の式のとおりで、順位の式を組み合わせて調整する案（`ts_rank` や語の回数を足すなど）は測っていない。A3 が一致した以上、bm25 に揃えるならその式を直接計算する方が近い。

## search の表

全部の問い合わせ（46 件）。分類の件数は 1〜2 文字の日本語 10、3 文字以上の日本語 7、英字の識別子 9、path 8、長い語だけの OR・NOT 6、拒否の確認 4、記号 2。
拒否の 4 件は両方とも拒否で、一致とした。A3 の列と上位の rowid は [search.csv](team-version-search-spike/search.csv) と [search_top.csv](team-version-search-spike/search_top.csv) にある。

| 問い合わせ | 分類 | FTS5の件数 | A1の件数 | A2の件数 | A1のJaccard | A2のJaccard | A1の上位10の共通 | A2の上位10の共通 | A1の共通の割合 | A2の共通の割合 | A1の同順位 | A2の同順位 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `着地` | 1〜2文字の日本語 | 623 | 623 | 623 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `検証` | 1〜2文字の日本語 | 313 | 313 | 313 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `競合` | 1〜2文字の日本語 | 46 | 46 | 46 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `重複` | 1〜2文字の日本語 | 111 | 111 | 111 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `依存` | 1〜2文字の日本語 | 163 | 163 | 163 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `人` | 1〜2文字の日本語 | 552 | 552 | 552 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `着地 検証` | 1〜2文字の日本語 | 188 | 188 | 188 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `着地 integrate` | 1〜2文字の日本語 | 288 | 288 | 288 | 1.000 | 1.000 | 2 | 1 | 0.200 | 0.100 | 1 | 0 |
| `検証 worktree` | 1〜2文字の日本語 | 93 | 93 | 93 | 1.000 | 1.000 | 2 | 1 | 0.200 | 0.100 | 1 | 0 |
| `重複 related` | 1〜2文字の日本語 | 22 | 22 | 22 | 1.000 | 1.000 | 5 | 6 | 0.500 | 0.600 | 2 | 2 |
| `全文検索` | 3文字以上の日本語 | 5 | 5 | 5 | 1.000 | 1.000 | 5 | 5 | 1.000 | 1.000 | 0 | 1 |
| `受け入れ条件` | 3文字以上の日本語 | 307 | 307 | 307 | 1.000 | 1.000 | 0 | 1 | 0.000 | 0.100 | 0 | 0 |
| `差し戻し` | 3文字以上の日本語 | 144 | 144 | 144 | 1.000 | 1.000 | 0 | 0 | 0.000 | 0.000 | 0 | 0 |
| `書き込み` | 3文字以上の日本語 | 120 | 120 | 120 | 1.000 | 1.000 | 3 | 0 | 0.300 | 0.000 | 0 | 0 |
| `自動更新` | 3文字以上の日本語 | 82 | 82 | 82 | 1.000 | 1.000 | 1 | 1 | 0.100 | 0.100 | 0 | 0 |
| `重複の判定` | 3文字以上の日本語 | 0 | 0 | 0 | 1.000 | 1.000 | 0 | 0 | 1.000 | 1.000 | 0 | 0 |
| `人の判断` | 3文字以上の日本語 | 101 | 101 | 101 | 1.000 | 1.000 | 1 | 1 | 0.100 | 0.100 | 1 | 0 |
| `related_docs` | 英字の識別子 | 2 | 2 | 2 | 1.000 | 1.000 | 2 | 2 | 1.000 | 1.000 | 0 | 0 |
| `bm25` | 英字の識別子 | 4 | 4 | 4 | 1.000 | 1.000 | 4 | 4 | 1.000 | 1.000 | 0 | 2 |
| `plan_review` | 英字の識別子 | 168 | 168 | 168 | 1.000 | 1.000 | 0 | 3 | 0.000 | 0.300 | 0 | 0 |
| `approve_landing` | 英字の識別子 | 153 | 153 | 153 | 1.000 | 1.000 | 1 | 2 | 0.100 | 0.200 | 0 | 0 |
| `worker_question` | 英字の識別子 | 100 | 100 | 100 | 1.000 | 1.000 | 0 | 3 | 0.000 | 0.300 | 0 | 0 |
| `SEARCH_WEIGHT` | 英字の識別子 | 0 | 0 | 0 | 1.000 | 1.000 | 0 | 0 | 1.000 | 1.000 | 0 | 0 |
| `trigram` | 英字の識別子 | 8 | 8 | 8 | 1.000 | 1.000 | 8 | 8 | 1.000 | 1.000 | 0 | 2 |
| `BM25` | 英字の識別子 | 4 | 4 | 4 | 1.000 | 1.000 | 4 | 4 | 1.000 | 1.000 | 0 | 2 |
| `"queue service"` | 英字の識別子 | 175 | 175 | 175 | 1.000 | 1.000 | 1 | 2 | 0.100 | 0.200 | 0 | 0 |
| `src/infrastructure/search.rs` | path | 0 | 0 | 0 | 1.000 | 1.000 | 0 | 0 | 1.000 | 1.000 | 0 | 0 |
| `docs/adr` | path | 265 | 265 | 265 | 1.000 | 1.000 | 0 | 1 | 0.000 | 0.100 | 0 | 0 |
| `src/domain/related.rs` | path | 3 | 3 | 3 | 1.000 | 1.000 | 3 | 3 | 1.000 | 1.000 | 3 | 3 |
| `migrations/0026_search.sql` | path | 0 | 0 | 0 | 1.000 | 1.000 | 0 | 0 | 1.000 | 1.000 | 0 | 0 |
| `docs/design/persistence.md` | path | 73 | 73 | 73 | 1.000 | 1.000 | 0 | 4 | 0.000 | 0.400 | 0 | 0 |
| `tests/it/runtime_claim.rs` | path | 8 | 8 | 8 | 1.000 | 1.000 | 8 | 8 | 1.000 | 1.000 | 1 | 2 |
| `.dagq/agents` | path | 56 | 56 | 56 | 1.000 | 1.000 | 2 | 2 | 0.200 | 0.200 | 0 | 0 |
| `DOCS/ADR` | path | 265 | 265 | 265 | 1.000 | 1.000 | 0 | 1 | 0.000 | 0.100 | 0 | 0 |
| `全文検索 OR trigram` | OR・NOT | 9 | 9 | 9 | 1.000 | 1.000 | 9 | 9 | 1.000 | 1.000 | 0 | 2 |
| `自動更新 NOT 着地の検証` | OR・NOT | 72 | 72 | 72 | 1.000 | 1.000 | 1 | 1 | 0.100 | 0.100 | 0 | 1 |
| `supervisor NOT worker` | OR・NOT | 943 | 943 | 943 | 1.000 | 1.000 | 0 | 0 | 0.000 | 0.000 | 0 | 0 |
| `(integrate OR 着地の) AND 検証の` | OR・NOT | 110 | 110 | 110 | 1.000 | 1.000 | 2 | 0 | 0.200 | 0.000 | 0 | 0 |
| `related_docs OR title_matches` | OR・NOT | 2 | 2 | 2 | 1.000 | 1.000 | 2 | 2 | 1.000 | 1.000 | 2 | 0 |
| `docs/adr NOT src/domain` | OR・NOT | 232 | 232 | 232 | 1.000 | 1.000 | 0 | 0 | 0.000 | 0.000 | 0 | 0 |
| `着地 OR 検証` | 短い語とOR・NOTなどの拒否 | parse_termsの規則で拒否 |  |  |  |  |  |  |  |  |  |  |
| `着地 NOT worktree` | 短い語とOR・NOTなどの拒否 | parse_termsの規則で拒否 |  |  |  |  |  |  |  |  |  |  |
| `(着地)` | 短い語とOR・NOTなどの拒否 | parse_termsの規則で拒否 |  |  |  |  |  |  |  |  |  |  |
| `trigram OR` | 短い語とOR・NOTなどの拒否 | FTS5の構文で拒否 |  |  |  |  |  |  |  |  |  |  |
| `_` | 記号（escapeの確認） | 2374 | 2374 | 2374 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |
| `%` | 記号（escapeの確認） | 196 | 196 | 196 | 1.000 | 1.000 | 10 | 10 | 1.000 | 1.000 | 10 | 10 |

## related の表

組の一覧（件名は 70 文字で切った。全文と A3 の列は [related.csv](team-version-search-spike/related.csv)）。

| 組 | a | aの件名 | b | bの件名 |
|---|---|---|---|---|
| 1 | `8d4cd0bc4` | docs: authorization.md の「ほかのコマンド（CLIの入口）」の残りの role の行を、task 3110 で CLI… | `54cd59984` | docs: authorization.md の「ほかのコマンド（CLIの入口）」の「残り…ほかのrole…queueを開く」に、super… |
| 2 | `2684ce88d` | docs: authorization.md の「ほかのコマンド（CLIの入口）」の report・graph --out の認可の説明を、… | `afd0a00a4` | docs: authorization.md の「ほかのコマンド（CLIの入口）」を、task 3105 で queue service 経… |
| 3 | `c33d92f66` | docs: queue-service.md「まだ無いもの」の段(5)の行から、既に control のユースケースで送る人・inbox の… | `e3fe20660` | docs: queue-service.md の冒頭の概要と「まだ無いもの」の段(5)の行を、task 3590 で supervisor … |
| 4 | `e3fe20660` | docs: queue-service.md の冒頭の概要と「まだ無いもの」の段(5)の行を、task 3590 で supervisor … | `6d0bf8a2b` | docs: docs/design/queue-service.md の「まだ無いもの」から実装済みの broker の宛てと integr… |
| 5 | `beb22f6e5` | docs(plans): landing-it-selection.md の「決めたこと」の 3（古い表と上限）と要点の IT を全部流す条… | `c49a4c303` | docs(plans): landing-it-selection.md の「決めたこと」の 2 の「表の tree に無い src・cra… |
| 6 | `57afcd073` | docs: provider-lifecycle.md の「workerのproviderと経路」に、provider の組の試しの B 群… | `884fde391` | docs: provider-lifecycle.md の「workerのproviderと経路」の約束で、model だけなら resum… |
| 7 | `e41e775a3` | docs: commands.md の「表示とreviewのコマンドをまたぐ約束」と doctor の入口の行、operations.md … | `21abdac71` | docs: commands.md の「表示とreviewのコマンドをまたぐ約束」の review が queue service を通らず… |
| 8 | `00fe28661` | docs: needs-session.md の「再計画で保留したrun」に、継続・撤回の新しい session の前に worktree … | `96db17bd0` | docs: needs-session.md の「再計画で保留したrun」に、task 3654 の継続・撤回の後の新しい session … |
| 9 | `225021a90` | docs: operations.md の「本番queueと固定バイナリ」の grant を置く条件を、task 3585 で instal… | `2ab1f9e89` | docs: operations.md の「本番queueと固定バイナリ」の migrate の主体と grant の期間を、task 35… |
| 10 | `aadb12fac` | docs: authorization.md の「queue serviceのユースケース（service側）」に、task 3581 で … | `9f5728239` | docs: authorization.md の「queue serviceのユースケース（service側）」と roles.md の「C… |
| 11 | `1f480931e` | docs: roles.md の「headlessのjobのverdictと遷移」の表に、task 1527 の再計画の診断 job の v… | `07cc0ae2c` | docs: roles.md の「headlessのjobのverdictと遷移」に already_satisfied の判定の revi… |
| 12 | `2475e6a22` | runtime: resume の next turn の「Tasks landed on <branch> since your base… | `58d619e13` | runtime: resume の next turn の「Tasks landed on <branch> since your base… |
| 13 | `3c4f15dea` | test: runtime_adopt::a_supervisor_that_lost_its_lease_stops_touching_t… | `616080565` | runtime_adopt::a_supervisor_that_lost_its_lease_stops_touching_the_run… |
| 14 | `5553649c5` | runtime_claim::resident_supervisor_without_runs_is_listed_until_it_sto… | `efa7f8cd2` | test: runtime_claim::resident_supervisor_without_runs_is_listed_until_… |
| 15 | `85f0c4e9c` | test: cli_version::auto_update_installs_each_runtime_landing_and_puts_… | `2c20ef6d0` | fix: 自動更新の job が update_installed を記録して終わった直後に supervisor が古い読みの updat… |
| 16 | `b6ebdad6a` | docs: authorization.md の「制御側の起動としての supervise と auto-update」を、task 358… | `ca3a91a0b` | docs: run-environment.md の「起動し直すときの引き継ぎ」を、task 3581 で自動更新の job が up を打… |
| 17 | `e41e775a3` | docs: commands.md の「表示とreviewのコマンドをまたぐ約束」と doctor の入口の行、operations.md … | `74b6ea2eb` | test: it 登録簿の 2 つの reason（it-slow-allow.d/t3107.toml の queue_binary::o… |
| 18 | `8d4cd0bc4` | docs: authorization.md の「ほかのコマンド（CLIの入口）」の残りの role の行を、task 3110 で CLI… | `d1d478da9` | docs: check_access と QueueCommand::reads_only の doc comment、persistenc… |
| 19 | `d5486af00` | feat: already_satisfied の主張の判定の verdict を、pass でも confidence を保つ専用の入口で… | `daff39bd8` | feat: already_satisfied の主張の判定が high の pass なら、閉じる時点で main の head を sn… |
| 20 | `fd8c1c48e` | feat: 全体の review の prompt に、差し戻す欠陥を探す観点（並行と競合・停止と再開・旧版の記録・記録の原子性と欠け・権限… | `e4d3baad7` | feat: 全体の review の prompt に、今の回に差し戻す指摘を 1 回で出し切り後の回に残さないことを求める指示を足す |
| 21 | `4a3b1c648` | fix: 自動更新の handoff の drain が着地 slot を待つ run の着地を始めず、保留の有無に依らず lease を持… | `0d15e5ef5` | fix: 自動更新の handoff では、着地の slot を待つ run を時間で解ける保留（ディスクの空き不足・CI の返事待ち）で … |
| 22 | `da960f7d6` | test: run_container::a_run_env_naming_no_sccache_starts_no_server_and_… | `c17c6ee17` | test: run_container::the_supervisor_starts_the_sccache_server_of_a_tur… |
| 23 | `ae9d797c4` | landing recheck: run の command が落ちた pass では main だけで command を流し、main … | `1e7eaa475` | docs: integrate.md と needs-session.md の landing recheck の例外の説明に、contai… |
| 24 | `c1acda878` | feat: 全 event を読む stats・kpi・report・stats_inputs・stats --ledger・forecas… | `300d61bcb` | feat: marks・mark・mark --retract を全 event でなく印に要る種類と id の event だけを読む形で… |
| 25 | `b8878ee21` | chore: この repository の dagq.toml から [eval] watch = false を外し、agents' w… | `41d73e077` | chore: この repository の dagq.toml から [throughput_review] kpi_stats = fa… |
| 26 | `553d20102` | config: 予算の超過の直し方（細かい事実は定義のそばの doc comment へ移すか文書を分け、他に記述の無い地図の行は消さない）… | `6ab8b3064` | docs: docs と runtime の task の verify に sh scripts/check-design-docs.sh… |

各組の b の順位と上位集合。圏外は点が 0 で候補に無いこと。

| 組 | FTS5のbの順位 | A1のbの順位 | A2のbの順位 | FTS5の上位集合 | A1の上位集合 | A2の上位集合 | A1の共通 | A2の共通 | A1の割合 | A2の割合 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | 2 | 16 | 10 | 10 | 10 | 7 | 0 | 0.700 | 0.000 |
| 2 | 1 | 1 | 23 | 10 | 10 | 10 | 5 | 0 | 0.500 | 0.000 |
| 3 | 1 | 2 | 29 | 10 | 10 | 10 | 5 | 1 | 0.500 | 0.100 |
| 4 | 3 | 12 | 3 | 10 | 10 | 10 | 2 | 2 | 0.200 | 0.200 |
| 5 | 3 | 1 | 18 | 10 | 10 | 10 | 8 | 1 | 0.800 | 0.100 |
| 6 | 1 | 1 | 12 | 10 | 10 | 10 | 4 | 0 | 0.400 | 0.000 |
| 7 | 1 | 1 | 11 | 10 | 10 | 10 | 5 | 1 | 0.500 | 0.100 |
| 8 | 1 | 1 | 8 | 10 | 10 | 10 | 6 | 3 | 0.600 | 0.300 |
| 9 | 1 | 1 | 14 | 10 | 10 | 10 | 7 | 0 | 0.700 | 0.000 |
| 10 | 1 | 1 | 133 | 10 | 10 | 10 | 10 | 0 | 1.000 | 0.000 |
| 11 | 2 | 4 | 138 | 10 | 10 | 10 | 8 | 0 | 0.800 | 0.000 |
| 12 | 1 | 1 | 231 | 8 | 10 | 10 | 3 | 0 | 0.300 | 0.000 |
| 13 | 3 | 1 | 51 | 10 | 10 | 10 | 2 | 0 | 0.200 | 0.000 |
| 14 | 1 | 1 | 641 | 10 | 10 | 10 | 4 | 0 | 0.400 | 0.000 |
| 15 | 1 | 6 | 214 | 10 | 10 | 10 | 4 | 0 | 0.400 | 0.000 |
| 16 | 1 | 1 | 8 | 10 | 10 | 10 | 3 | 1 | 0.300 | 0.100 |
| 17 | 18 | 17 | 21 | 10 | 10 | 10 | 5 | 1 | 0.500 | 0.100 |
| 18 | 27 | 18 | 59 | 10 | 10 | 10 | 7 | 0 | 0.700 | 0.000 |
| 19 | 2 | 1 | 571 | 10 | 10 | 10 | 7 | 1 | 0.700 | 0.100 |
| 20 | 圏外 | 1 | 3 | 0 | 10 | 10 | 0 | 0 | 0.000 | 0.000 |
| 21 | 1 | 2 | 430 | 10 | 10 | 10 | 8 | 0 | 0.800 | 0.000 |
| 22 | 2 | 2 | 356 | 10 | 10 | 10 | 4 | 0 | 0.400 | 0.000 |
| 23 | 圏外 | 13 | 288 | 10 | 10 | 10 | 2 | 0 | 0.200 | 0.000 |
| 24 | 5 | 37 | 161 | 10 | 10 | 10 | 3 | 0 | 0.300 | 0.000 |
| 25 | 1 | 2 | 1 | 10 | 10 | 10 | 5 | 1 | 0.500 | 0.100 |
| 26 | 3 | 2 | 15 | 10 | 10 | 10 | 2 | 0 | 0.200 | 0.000 |

各組の b の強さと差。

| 組 | FTS5のbの強さ | A1のbの強さ | A2のbの強さ | A1の\|強さの差\| | A2の\|強さの差\| | A1の\|重みの差\| | A2の\|重みの差\| | A1の0.1またぎ | A2の0.1またぎ |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 0.459 | 0.354 | 0.687 | 0.105 | 0.228 | 0.418 | 0.913 | 0 | 0 |
| 2 | 0.506 | 0.553 | 0.737 | 0.047 | 0.231 | 0.189 | 0.925 | 0 | 0 |
| 3 | 0.402 | 0.407 | 0.601 | 0.005 | 0.199 | 0.022 | 0.797 | 0 | 0 |
| 4 | 0.242 | 0.291 | 0.700 | 0.049 | 0.458 | 0.197 | 1.833 | 0 | 0 |
| 5 | 0.392 | 0.466 | 0.725 | 0.074 | 0.333 | 0.296 | 1.331 | 0 | 0 |
| 6 | 0.310 | 0.423 | 0.627 | 0.114 | 0.317 | 0.455 | 1.270 | 0 | 0 |
| 7 | 0.402 | 0.355 | 0.648 | 0.047 | 0.247 | 0.188 | 0.987 | 0 | 0 |
| 8 | 0.519 | 0.333 | 0.709 | 0.186 | 0.190 | 0.743 | 0.762 | 0 | 0 |
| 9 | 0.637 | 0.531 | 0.686 | 0.107 | 0.049 | 0.427 | 0.196 | 0 | 0 |
| 10 | 0.597 | 0.476 | 0.629 | 0.121 | 0.033 | 0.484 | 0.130 | 0 | 0 |
| 11 | 0.289 | 0.350 | 0.575 | 0.062 | 0.287 | 0.247 | 1.147 | 0 | 0 |
| 12 | 0.273 | 0.484 | 0.636 | 0.212 | 0.363 | 0.847 | 1.452 | 0 | 0 |
| 13 | 0.561 | 0.529 | 1.000 | 0.032 | 0.439 | 0.128 | 1.754 | 0 | 0 |
| 14 | 0.632 | 0.756 | 1.000 | 0.124 | 0.368 | 0.494 | 1.471 | 0 | 0 |
| 15 | 0.603 | 0.638 | 0.713 | 0.035 | 0.110 | 0.138 | 0.440 | 0 | 0 |
| 16 | 0.321 | 0.331 | 0.766 | 0.010 | 0.445 | 0.041 | 1.780 | 0 | 0 |
| 17 | 0.132 | 0.224 | 0.604 | 0.091 | 0.472 | 0.366 | 1.889 | 0 | 0 |
| 18 | 0.142 | 0.257 | 0.600 | 0.115 | 0.458 | 0.459 | 1.831 | 0 | 0 |
| 19 | 0.292 | 0.346 | 0.677 | 0.054 | 0.385 | 0.218 | 1.541 | 0 | 0 |
| 20 | 0.089 | 0.227 | 0.745 | 0.137 | 0.656 | 0.906 | 2.979 | 1 | 1 |
| 21 | 0.498 | 0.345 | 0.806 | 0.153 | 0.308 | 0.612 | 1.232 | 0 | 0 |
| 22 | 0.268 | 0.342 | 0.771 | 0.074 | 0.503 | 0.296 | 2.013 | 0 | 0 |
| 23 | 0.070 | 0.260 | 1.000 | 0.189 | 0.930 | 1.039 | 4.000 | 1 | 1 |
| 24 | 0.435 | 0.233 | 1.000 | 0.203 | 0.565 | 0.810 | 2.259 | 0 | 0 |
| 25 | 0.705 | 0.577 | 0.983 | 0.128 | 0.278 | 0.512 | 1.113 | 0 | 0 |
| 26 | 0.139 | 0.282 | 0.601 | 0.143 | 0.462 | 0.570 | 1.846 | 0 | 0 |
