---
id: plan-docs-slim
type: plan
title: docs/design の4指標（総量と伸び・docs/design を変えた着地の割合・docs の衝突と claim の控え・道具の結果に占める docs）と docs だけの衝突の種類（M5）・docs_drift の差し戻し（M6）の定義と基準値
status: active
created: 2026-10-07
owners:
  - hisamekms
tags:
  - documentation
  - measurement
related:
  - design-measurement
  - development-documents
---

# docs/design の4指標の定義と基準値

goal 159（request 44）の再発防止の4（計測）と、段4の前後比較の基準。
4指標と M5（docs だけの衝突の件数と種類の内訳、request 46）と M6（run の review の docs_drift の差し戻し、request 102）は `scripts/docs-metrics.py` が数え、`scripts/docs-metrics-test.py` が fixture（`scripts/docs-metrics-fixtures/`）で確かめる。
毎週の見直しで流して基準値と前週と並べる手順は `.claude/skills/throughput-review/reference/weekly.md` の「docs の4指標」が持つ。
KPI（runtime）にしないのは、M4 が runtime の持たない会話記録を読み、M1・M2 は git で足りるため。

## 測り方

```sh
python3 scripts/docs-metrics.py --since 2026-09-29T00:00:00Z --until 2026-10-06T00:00:00Z \
  --stats stats.json --claude-projects ~/.claude/projects --events events-*.json \
  --review-events events-review-*.json --format json
```

- 期間は `--since` と `--until`（UTC、`[since, until)` の半開区間。日付だけなら 00:00Z）。
- M1・M2 は `--repo`（既定は cwd）の `--ref`（既定は `main`）の履歴を読む。
- M3 は `--stats` に `dagq stats --since <since>` の JSON を保存したファイルを渡す（script は queue を読まない）。
- M4 は `--claude-projects` に Claude Code の会話記録の dir（`~/.claude/projects`）を渡す。
- M5 は `--events` に `dagq events --full --kind ...` の JSON を保存したファイル（複数可。頁送りの各頁）を渡し、`--repo` の git の object を読む。
- M6 は `--review-events` に `dagq events --full --kind review_finished` の JSON を保存したファイル（複数可。頁送りの各頁）を渡す。
- `--stats`・`--claude-projects`・`--events`・`--review-events` を省くと M3・M4・M5・M6 は `null`。`--metrics m1,m2` で選べる。`--format` は `markdown`（既定）か `json`。

## 定義

### M1 docs/design の総量と伸び

- 期間の各 UTC 日 D（D の 00:00Z が期間に入る日）について、`git rev-list -1 --first-parent --before=<D 00:00Z> <ref>` の commit を1回読む（周回しない）。
- その commit の `git ls-tree -r -l` で、`docs/design/` 配下の `.md` の byte の合計・文書数・30,720 byte（30 KiB、[文書の規則](../development/documents.md)の「design」の地図の予算）を超える文書数を数える。
- 列は日付・総 byte・前日差・文書数・30 KiB 超の数。伸びは最後の日と最初の日の総 byte の差と、その1日あたり。

### M2 docs/design を変えた着地の割合

- 着地は `<ref>` の first-parent の commit のうち committer date が期間に入るもの。merge は first parent との差分で読み、rename は検出しない。
- `docs/design` を変えた着地の数 ÷ 着地の数。
- 上位4文書（`docs/design/persistence.md`・`domain-model.md`・`supervisor-lifecycle/stats.md`・`provider-lifecycle.md`）のどれかを変えた着地の数 ÷ 着地の数。
- `docs/design` の追加行と削除行の合計（`git log --numstat`。binary は数えない）。

### M3 docs の衝突と claim の控え

`dagq stats --since` の JSON の欄（2026-10-07 の `src/domain/stats/conflicts.rs` と `src/domain/claim_defer.rs` で確かめた）から:

- `conflict_hotspots.files[]` のうち `path` が `docs/` で始まるものの `conflicts` の合計と、全ての `files[]` の `conflicts` の合計に占める割合。
- `claim_deferrals.count`（控えの件数）・`claim_deferrals.secs`（待ちの合計）・`claim_deferrals.by_end.expired.count`（上限で切れた件数）と `.secs`。

### M4 道具の結果に占める docs

- **対象の dir**: 名前が `-runs-<run id>-worktree-<run id>` か `-runs-<run id>-worktree` で終わる dir（dagq の run の worktree `runs/<run id>/worktree-<run id>`、古い配置の `runs/<run id>/worktree` を cwd にした会話）だけを読む。
  それ以外（inbox の repository・planners・plan-reviews・observer・reports・run dir そのもの）と、使い捨ての queue の一時の dir（名前に `-private-var-folders-`・`-var-folders-`・`-tmp-`・`-private-tmp-` を含む）は数えない。
- **worker と review**: 各会話（dir 直下の jsonl）の最初の user の message が、worker の prompt の冒頭の句 `You are executing dagq task ` で始まれば worker、run の review の prompt の冒頭の句 `You review run ` で始まれば review（句は 2026-10-07 の `src/application/prompt.rs` の文面）。
  どちらにも当たらない会話（recovery・triage の job、新しい会話記録で始まる resume など）は数えない。
  記録の model（`<synthetic>` を除く）に Claude でないものがある会話も数えない。Codex の会話は `~/.claude/projects` に入らない。
  会話の `<会話>/subagents/*.jsonl` は親の会話の区分に従う。
- **期間**: tool の呼び出しは、その tool_result の記録の timestamp（UTC）が `[since, until)` に入るものだけを数える。会話の数は、期間に数えた結果があるか最初の記録が期間に入る会話。
- **呼び出しごとの値**: tool_result の文字数（文字列なら長さ、text の並びなら text の長さの和）。
  tool_use の入力の path か pattern（Read・Edit・Write の `file_path`、Grep の `path` と `glob`、Glob の `path` と `pattern`、Bash の command の語）を worktree からの相対の path にし、`docs/` 配下を指せば docs、`src/` 配下を指せば src の結果とする（両方を指せば両方に数える）。
- **出す値**: docs と src の結果の文字数 ÷ 全 tool_result の文字数、上位4文書の丸ごとの Read（`offset`・`limit` なし）の回数、grep/rg の回数（Grep と、Bash の command を改行と `;`・`|`・`&` で区切ったコマンドのうち pipe から読まない grep・rg を走らせた呼び出し）、範囲を絞った Read（`offset` か `limit` あり）の回数、1回の結果の文字数の中央値と p90。worker と review を合わせた値と、それぞれの値。
- **分位点**: 文字数を昇順に並べ、中央値は `statistics.median`（偶数なら中の2つの平均）、p90 は最近順位法（n×0.9 の切り上げ番目）。
- **(a) 上位文書の grep の結果**: Grep の `path` が上位4文書か、それを含む `docs/design` の dir（`docs/design`・`docs/design/supervisor-lifecycle`）を指すもの、`glob` が上位4文書を名指す（basename・`**/<basename>`・それに合う `docs/design/` の pattern）もの、Bash の grep/rg の path の引数がそれらを指すものの tool_result の件数・文字数の中央値・p90（worker と review、上の期間と分位点）。
- **(b) worker が src/ を読む量の run ごとの集計**: Read の `file_path` が `src/` 配下、Grep の `path` が `src/` を指す、Bash の grep/rg の path の引数が `src/` を指す呼び出しの tool_result の文字数を、worker の会話（その subagents を含む）だけで run ごとに合計し、run の数・0 の run の数・中央値・p90 を出す。
  - run への束ね方: 同じ dir（`-runs-<run id>-worktree`）の会話を1つの run とし、dir 名の run id を key にする。
  - 分母の run: その run の worker の会話の最初の記録の timestamp（会話が複数なら最も早いもの）が `[since, until)` に入る run。その run の記録は区間の外にかかるものも全て数え、`src/` を読まなかった run は 0 として数える。

### M5 docs だけの衝突の件数と種類の内訳

task 1957 の前後比較に足す、request 46 の衝突の種類の内訳（task 1966）。

- **kind と欄**: `conflict_precheck`（`main`・`head`・`merge_base`・`conflicts`）、`landing_recheck_failed` と `integration_deferred`（`main`・`head`・`conflicts`、`merge_base` なし）。
  2026-10-07 の `src/application/supervise/landing.rs`・`src/domain/recheck.rs`・`src/application/integrate.rs` で確かめ、script の `CONFLICT_KINDS` が持つ。
  `conflicts` が空か無い event（送れずに取り下げた request、衝突でない `integration_deferred` など）は数えない。
- **重複の除き方**: 入力の全てのファイルを通して event ID が同じものは1件（最初に読んだもの）。同じ run の別の event は別に数える。
- **期間**: event の `created_at`（UTC）が `[since, until)` に入るもの。1日あたりは期間の日数（`until − since`）で割る。
- **docs だけ**: `conflicts` の path が全て `docs/` 配下の event。1つでも外が混ざる event は数えず、`mixed_events` にだけ数える。
- **merge_base の補い方**: payload の `merge_base` を使い、無ければ `git merge-base <main> <head>`（2つの commit が決まれば時刻に依らない）。`main` か `head` が `--repo` の object に無いか、merge-base が出なければ event は再現不可。
- **再現**: path ごとに merge_base・main・head の版を取り、`git merge-file -p --diff3` の衝突の塊（`<<<<<<<` から `>>>>>>>`）を取り出す。
  path が3つのうち1つか2つに無い（追加と削除）ときは merge-file にかけず種類 5 の塊1つ。3つ全てに無い path と塊の出ない path（rebase は commit ごとに当てるので端の3つの版では衝突しないことがある）は path 再現不可。
- **event の扱い**: 1つ以上の path で塊が出れば分類済み、そのうち塊の出ない path があれば一部再現にも数える。どの path にも塊が出なければ（object の欠けを含む）再現不可。
- **塊の種類**: 3つの側の空でない行で、1 → 2 → 5 → 3 → 4 の順に最初に当たるもの（狭いものから）。
  - 1 frontmatter の日付の行: 全ての行が `updated:`・`last_verified:`・`created:` で始まる。
  - 2 同じ位置への追記: merge_base の側が空で、main と head の側が空でない（`docs/adr/README.md` の表の末尾など。足した行が表の行や箇条書きでも 2）。
  - 5 表の行とファイルの追加・削除: 全ての行が `|` で始まる。追加・削除の path は上の再現の規則で 5。
  - 3 箇条書きの項目: 全ての行が字下げの後に `- `・`* `・`数字. ` で始まる。
  - 4 段落: それ以外（種類の混ざった塊を含む）。
- **event の種類**: 塊の出た path の塊の種類のうち、規則で合わせにくい重いものの順（4 段落 → 5 表とファイル → 3 箇条書き → 2 追記 → 1 日付）で最初のもの。
- **割合の分母**: 種類ごとの event の割合は分類済みの event の数で割る。再現不可は分母に入れず件数を別に出す。塊の数は種類ごとと合計を出す。
- 規則は request 46 の値に合わせて調整しない（proposal 739 の revise 1）。

### M6 run の review の docs_drift の差し戻し

task 1957 の前後比較に足す、docs の差し戻しで run の時間を使うことそのものの指標（request 102）。

- **kind と欄**: `review_finished` の payload の `primary_code`（主な理由の code。最初の理由の最初の code）・`reason_codes`（`reasons` と同じ順の、理由ごとの code の配列の配列）・`reasons`（理由の文の配列）と、event の `run_id`。
  2026-10-12 の `src/application/supervise/landing.rs` の `judge_review` と `src/domain/review_reason.rs` で確かめ、script の `REVIEW_KIND` と `DOCS_DRIFT` が持つ。
  `reason_category` は実記録の多くが null なので使わない。
- **重複の除き方**: 入力の全てのファイルを通して event ID が同じものは1件（最初に読んだもの）。
- **期間**: event の `created_at`（UTC）が `[since, until)` に入るもの。
- **分母**: 期間の `review_finished` の event の `run_id` の異なる数（run の数）。同じ run の複数の review は event では別に数え、run では1つ。
- **出す値**: `review_finished` の event の数と run の数、主な理由が docs_drift（`primary_code` が `docs_drift`）の event の数と run あたりの値（÷ run の数）、`reason_codes` のどれかの理由に `docs_drift` を含む event の数と run あたりの値。
- **種類**: 主な理由が docs_drift の event について、`reason_codes` で `docs_drift` を含む最初の理由の文（その添字の `reasons`。`reason_codes` が無いか、その添字の文が無ければ `reasons` の最初の文）を小文字にし、次の順で最初にキーワードを含む種類にする（script の `DRIFT_KINDS` が同じ一覧と順を持つ）。
  1. 予算（`budget`。予算に収めるための削除）: `budget`・`予算`・`check-design-docs`・`allow list`・`byte`
  2. 1文1行（`one_sentence_per_line`）: `one sentence`・`1文1行`・`one line per sentence`・`per line`・`line length`
  3. 書き写し（`copy`）: `copied`・`copies`・`copy of`・`書き写`・`transcri`・`field list`・`duplicat`
  4. architecture.md の境界の記録（`architecture_boundary`）: `architecture.md`
  5. 古い記述（`stale`）: `stale`・`still says`・`still describes`・`no longer`・`outdated`・`out of date`・`古い`・`still names`・`still lists`・`does not record`・`doesn't record`・`missing`・`not updated`
  6. その他（`other`）: どれにも当たらないもの（件数を必ず出す）
- **割合の分母**: 種類ごとの割合は主な理由が docs_drift の event の数で割る。
- 規則は request 102 で planner が決めたもので、下の参考の値に合わせて調整しない。

## 基準値

### M1（script、base `f56a5580`）

`python3 scripts/docs-metrics.py --since 2026-09-22 --until 2026-10-08 --metrics m1`（期間 2026-09-22〜2026-10-07、claim の日まで）。

| 日（00:00Z） | 総 byte | 前日差 | 文書数 | 30 KiB 超 |
| --- | --- | --- | --- | --- |
| 2026-09-22 | 17,503 | - | 7 | 0 |
| 2026-09-23 | 127,027 | 109,524 | 7 | 1 |
| 2026-09-24 | 255,347 | 128,320 | 7 | 2 |
| 2026-09-25 | 402,724 | 147,377 | 8 | 3 |
| 2026-09-26 | 550,533 | 147,809 | 8 | 3 |
| 2026-09-27 | 985,805 | 435,272 | 72 | 6 |
| 2026-09-28 | 1,456,145 | 470,340 | 84 | 9 |
| 2026-09-29 | 1,735,635 | 279,490 | 85 | 14 |
| 2026-09-30 | 1,953,544 | 217,909 | 87 | 16 |
| 2026-10-01 | 2,012,700 | 59,156 | 87 | 17 |
| 2026-10-02 | 2,127,931 | 115,231 | 89 | 19 |
| 2026-10-03 | 2,348,524 | 220,593 | 90 | 24 |
| 2026-10-04 | 2,622,418 | 273,894 | 91 | 28 |
| 2026-10-05 | 2,698,424 | 76,006 | 93 | 26 |
| 2026-10-06 | 2,853,515 | 155,091 | 93 | 28 |
| 2026-10-07 | 3,005,727 | 152,212 | 97 | 29 |

伸びは15日で 2,988,224 byte（1日あたり 199,215 byte）。2026-09-29 以降の7日（09-29→10-06）は 1,117,880 byte（1日あたり約 160KB）。

### M2（script、base `f56a5580`）

`python3 scripts/docs-metrics.py --since 2026-09-29 --until 2026-10-08 --metrics m2`（2026-09-29 以降、claim の日まで）。

| 着地 | docs/design を変えた | 割合 | 上位4文書を変えた | 割合 | 追加行 | 削除行 | 合計 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 478 | 337 | 70.5% | 126 | 26.4% | 8,754 | 4,640 | 13,394 |

### M3（request 44、人の session が 2026-10-06 に測った値）

`dagq stats --since 2026-09-29T00:00:00Z` の出力から。

- `conflict_hotspots` の file は全て `docs/design` と `docs/adr/README.md`（docs の割合 100%）。
- claim の控えは1週間で 756件、待ちの合計は約 909時間、うち 243件が expired。

### M4（request 44、人の session が 2026-10-06 に測った値）

直近14日の worker・review の会話 2,482件で、上位4文書の丸ごとの Read 0回、grep/rg 1,569回、範囲を絞った Read 454回、結果の文字数の中央値 1.1K・p90 6.9K、docs 約10%、src 43%。

request 44 の値は人の session がその場で数えたもので、対象の会話と grep/rg の数え方が上の定義と同じとは限らない。
同じ定義の前の値は、下の「前後比較」の「前」（`[2026-09-29T00:00:00Z, 2026-10-06T00:00:00Z)` を script で流した値）。

### M4 の (a)(b)

前の値は下の「前後比較」の「前」（同じ script・同じ母集団で `[2026-09-29, 2026-10-06)` を流した値）。
request 44 の全結果の中央値 1.1K・p90 6.9K は (a) の基準値に流用しない（全 tool_result の分位点から上位文書の grep の分位点は導けない）。

### M5（参考の値: request 46、人の session の値）

request 46（2026-10-06、人の session）が数えた docs の衝突 135件の内訳: 1 frontmatter の日付が 72件、2 同じ位置への追記が 18件、3 箇条書きが 29件、4 段落が 12件、5 表の行とファイルの追加・削除が 4件。
分類の手順はこの script と別（その場の数え方）なので、上の定義と比べられる基準値ではない。

script の基準の期間 `[2026-09-29, 2026-10-06)` の値と request 46 の値との差は、下の「前後比較」の M5 が持つ。

### M6（参考の値: inbox の手元の概算）

request 102（2026-10-11、inbox がその場で数えた値）: goal 159 の段の着地 2026-10-07T07:15Z の前と後の `review_finished` で、主な理由が docs_drift の event は 110件・0.51/run から 103件・0.55/run。
最初の理由の分類で、古い記述 84→45、予算 4→16、architecture.md の境界 10→16、1文1行 1→6、書き写し 6→8。
期間も分類の手順もこの script と別なので、上の定義と比べられる基準値ではない。
script の前後の期間の値は、下の「前後比較」の M6 が持つ。

## 前後比較（段4）

goal 159 の段4（task 1957）の結果。
定義は上の「定義」のまま（task 1946 の定義と、M5 は task 1966、M6 は task 2289 が足した定義）で、script は変えていない。

### 期間と入力

- **前**: `[2026-09-29T00:00:00Z, 2026-10-06T00:00:00Z)`（7日）。
  段の task（1942〜1955）のどれも着地していない7日で、M1〜M6 の全てをこの期間で比べる。
- **後**: `[2026-10-08T00:00:00Z, 2026-10-15T00:00:00Z)`（7日）。
  段の task（1946 を除く 1942〜1955）の最後の着地は task 1954 の 2026-10-07T07:10Z で、その翌 UTC 日の 00:00Z を S とした `[S, S+7d)`。
- **script**: `scripts/docs-metrics.py` の commit `56ceb9c8` の版を、前後とも同じ引数の形で流した。
  前は `--since 2026-09-29 --until 2026-10-06`、後は `--since 2026-10-08 --until 2026-10-15`。
- **M1・M2**: `--repo` は repository の root、`--ref` は main（runtime の planner が流した時点の main は `1a7f887b`）。
  worker が main `a181216a` で `--metrics m1,m2` を流し直し、同じ値が出ることを確かめた（期間の commit は両方の履歴で同じ）。
- **M3**: `dagq stats --since <since>T00:00:00Z --until <until>T00:00:00Z` の JSON（`stats-2026-09-29_2026-10-06.json`・`stats-2026-10-08_2026-10-15.json`）。
- **M4**: `--claude-projects ~/.claude/projects`（2026-10-15T01:00Z 頃に読んだ会話記録）。
- **M5**: `dagq events --full --kind conflict_precheck --kind landing_recheck_failed --kind integration_deferred` を前後の期間の `--since`・`--until` で読んだ JSON（各1頁: 前 193件・後 97件）。
- **M6**: `dagq events --full --kind review_finished` を前後の期間で読んだ JSON（各1頁: 前 810件・後 561件）。
- 入力と script の JSON の出力（`2026-09-29_2026-10-06.json`・`2026-10-08_2026-10-15.json`）は、submit した runtime の planner が 2026-10-15T01:00Z 頃に throughput-review skill の weekly.md の「docs の4指標」の手順で `~/.local/share/dagq-hostmetrics/docs-slim/` に保存した。
  worker は本番の queue と会話記録を読まないので、M3〜M6 の値は task の context に planner が写した値を使った。
- 欠けの指標は無い。

### 表

比は 後 ÷ 前（どちらかが負か0のときは「-」）。
1日あたりは期間の日数（7）で割り、着地あたりは M2 の着地の数（前 434・後 197）で割った値。
後の期間の着地は前の半分に近いので、件数の比は着地あたりの行と合わせて読む。

| 指標 | 前 | 後 | 差 | 比 |
| --- | --- | --- | --- | --- |
| M1 最後の日の docs/design の総 byte | 2,698,424（10-05、93文書） | 1,890,190（10-14、85文書） | -808,234 | 0.70 |
| M1 30 KiB 超の文書の数（最後の日） | 26 | 27 | +1 | 1.04 |
| M1 1日あたりの伸び（日次の差の平均） | +160,465 | -54,267 | -214,732 | - |
| M1 1日あたりの伸び（design の文書を消すか移した着地を除く） | - | +21,528 | - | - |
| M2 着地の数 | 434 | 197 | -237 | 0.45 |
| M2 docs/design を変えた着地 ÷ 着地 | 306 / 434 = 0.7051 | 148 / 197 = 0.7513 | +0.0462 | 1.07 |
| M2 上位4文書を変えた着地 ÷ 着地 | 114 / 434 = 0.2627 | 42 / 197 = 0.2132 | -0.0495 | 0.81 |
| M2 docs/design の追加行 / 削除行 | +6,907 / -3,693 | +5,085 / -6,130 | - | - |
| M3 conflict_hotspots の docs/ の件数（全体に占める割合） | 102（136 の 0.75） | 56（87 の 0.6437） | -46 | 0.55 |
| M3 docs/ の衝突の1日あたり | 14.57 | 8.00 | -6.57 | 0.55 |
| M3 docs/ の衝突の着地あたり | 0.235 | 0.284 | +0.049 | 1.21 |
| M3 claim の控えの件数（1日あたり） | 736（105.1） | 1,255（179.3） | +519 | 1.71 |
| M3 claim の控えの待ちの合計（1日あたり） | 877.6h（125.4h） | 4,063h（580.4h） | +3,185.4h | 4.63 |
| M3 claim の控えの着地あたり | 1.696 | 6.371 | +4.675 | 3.76 |
| M3 expired の件数（1日あたり） | 231（33.0） | 153（21.9） | -78 | 0.66 |
| M3 expired の待ちの合計（1日あたり） | 312h（44.6h） | 197.8h（28.3h） | -114.2h | 0.63 |
| M3 expired の着地あたり | 0.532 | 0.777 | +0.244 | 1.46 |
| M4 all の会話 / 結果の数 | 1,061 / 68,144 | 573 / 45,298 | - | 0.54 / 0.66 |
| M4 all の docs/ の割合 | 0.1398 | 0.1491 | +0.0093 | 1.07 |
| M4 all の src/ の割合 | 0.1841 | 0.2347 | +0.0506 | 1.27 |
| M4 all の結果の中央値 / p90（文字） | 1,428 / 14,986 | 1,324 / 13,744 | -104 / -1,242 | 0.93 / 0.92 |
| M4 worker の会話 / 結果の数 | 398 / 33,654 | 215 / 26,011 | - | 0.54 / 0.77 |
| M4 worker の docs/ / src/ の割合 | 0.2334 / 0.4968 | 0.2393 / 0.5522 | +0.0059 / +0.0554 | 1.03 / 1.11 |
| M4 worker の中央値 / p90 | 1,142 / 7,876 | 976 / 7,376 | -166 / -500 | 0.85 / 0.94 |
| M4 review の会話 / 結果の数 | 663 / 34,490 | 358 / 19,287 | - | 0.54 / 0.56 |
| M4 review の docs/ / src/ の割合 | 0.1038 / 0.0635 | 0.1075 / 0.0881 | +0.0037 / +0.0246 | 1.04 / 1.39 |
| M4 review の中央値 / p90 | 1,822 / 25,691 | 2,116 / 27,185 | +294 / +1,494 | 1.16 / 1.06 |
| M4 (a) 上位文書の grep の結果の件数 | 2,091 | 1,125 | -966 | 0.54 |
| M4 (a) 上位文書の grep の結果の中央値 / p90 | 852 / 5,268 | 992 / 6,161 | +140 / +893 | 1.16 / 1.17 |
| M4 (b) worker の run の数（src/ を読まなかった run） | 397（55） | 201（24） | -196 | 0.51 |
| M4 (b) run ごとの src/ の読みの中央値 / p90（文字） | 40,140 / 137,076 | 61,660 / 197,661 | +21,520 / +60,585 | 1.54 / 1.44 |
| M5 衝突の event / docs と外の混在 | 155 / 50 | 80 / 54 | - | 0.52 / 1.08 |
| M5 docs だけの衝突の event（1日あたり） | 105（15.0） | 26（3.71） | -79 | 0.25 |
| M5 docs だけの衝突の着地あたり | 0.242 | 0.132 | -0.110 | 0.55 |
| M5 分類済み / 再現不可 | 103 / 2 | 25 / 1（path 再現不可 1） | -78 / -1 | 0.24 / 0.5 |
| M5 1 frontmatter の日付（割合） | 60（0.5825） | 0（0） | -60 | 0 |
| M5 2 同じ位置への追記 | 17（0.165） | 0（0） | -17 | 0 |
| M5 3 箇条書き | 14（0.1359） | 6（0.24） | -8 | 0.43 |
| M5 4 段落 | 6（0.0583） | 17（0.68） | +11 | 2.83 |
| M5 5 表の行とファイルの追加・削除 | 6（0.0583） | 2（0.08） | -4 | 0.33 |
| M6 review_finished の event / run | 810 / 424 | 561 / 206 | - | 0.69 / 0.49 |
| M6 run あたりの review の数 | 1.91 | 2.72 | +0.81 | 1.43 |
| M6 主な理由が docs_drift（run あたり） | 135（0.3184） | 110（0.534） | -25（+0.2156） | 0.81（1.68） |
| M6 reason_codes に docs_drift を含む（run あたり） | 204（0.4811） | 241（1.17） | +37（+0.6889） | 1.18（2.43） |
| M6 予算（割合） | 6（0.0444） | 16（0.1455） | +10 | 2.67 |
| M6 1文1行 | 2（0.0148） | 2（0.0182） | 0 | 1.0 |
| M6 書き写し | 8（0.0593） | 11（0.1） | +3 | 1.38 |
| M6 architecture.md の境界 | 6（0.0444） | 14（0.1273） | +8 | 2.33 |
| M6 古い記述 | 93（0.6889） | 37（0.3364） | -56 | 0.40 |
| M6 その他 | 20（0.1481） | 30（0.2727） | +10 | 1.5 |

- M1 の後の 10-14 の日次の差 -388,746 の内訳は、git の first-parent の着地ごとの docs/design の byte の差で確かめた。
  design の文書を消すか `docs/development/` へ移した着地 5件（task 2325・2326・2328・2331・2332、計 -392,176）がほぼ全てで、goal 159 の段3 の書き直しではない。
  M1 の日次の差が数える 10-08〜10-13 の着地では、design の文書を消すか移した着地 7件（上の5件と task 2125・2161）が計 -454,767、それ以外の着地 133件が計 +129,166（増やした着地の和 +194,966、減らした着地の和 -65,800）で、上の表の「除く」の行はその 6日分の平均。
- 前の最後の日 10-05 と後の最初の日 10-08 の間には段3 の書き直しの着地があり、10-07 の 3,005,727 byte から 10-08 の 2,215,791 byte に減った。
  前後の期間はこの書き直しを挟むので、M1 の最後の日の差 -808,234 は、10-05〜10-07 の伸び（+307,303）、段3 の書き直し、後の期間の他の goal の削除・移管とそれ以外の着地の伸びを合わせた正味の値である。

### 参考の値（「前」に入れない）

期間か定義か手順が上の表と違うので、「前」の列に入れず出典を添えて並べる。

| 指標 | 参考の値 | 出典 | 表の「前」と違う点 |
| --- | --- | --- | --- |
| M1 総 byte と伸び | 2026-09-22〜10-07 で +2,988,224（199,215/日）、10-07 は 3,005,727 | 上の「基準値」の M1（base `f56a5580`） | 期間が 15日で claim の日まで |
| M2 割合 | 478件のうち 70.5%・26.4% | 上の「基準値」の M2 | 期間が 2026-09-29〜claim の日（10-07）まで |
| M3 claim の控え | 1週間で 756件・243件が expired・約 909時間 | request 44（人の session、2026-10-06） | `dagq stats --since` の until が測った時点で、期間がずれる |
| M4 全結果 | 中央値 1.1K・p90 6.9K、docs 約10%・src 43% | request 44（直近14日の会話 2,482件） | 対象の会話と数え方が違う。(a) の前の値には使わない |
| M5 docs の衝突 | 135件: 日付 72・追記 18・箇条書き 29・段落 12・表とファイル 4 | request 46（人の session、2026-10-06） | 分類の手順が違う |
| M6 主な理由が docs_drift | 110件・0.51/run → 103件・0.55/run、古い記述 84→45・予算 4→16・境界 10→16・1文1行 1→6・書き写し 6→8 | request 102（inbox の概算、2026-10-11） | goal 159 の着地 2026-10-07T07:15Z で前後に分けた期間で、分類の手順が違う |

- M5 の script の前（105件）と request 46 の 135件の差は、request 46 の数え方の記録が無いので全ては説明できない。
  script は docs の外が混ざる event（前の期間で 50件）を数えず、event を最も重い塊の種類1つに数え（日付と箇条書きを含む event は箇条書き）、3つの版で塊が再現した event だけを分類するので、件数が少なく軽い種類（日付）の割合が近い値に、箇条書きと段落が半分ほどになった。

### 判断

**予算と規則の見直し**: 予算の値も規則も変えない。

- docs/design の伸びは、消すか移した着地を除いても 160KB/日から 22KB/日（比 0.13）に下がり、規則と検査が太ることを止めている。
- 上位4文書を変えた着地の割合は 0.26 から 0.21 に下がったが、docs/design を変えた着地の割合は 0.71 から 0.75 で下がらない。
  後の期間は design の書き直しと削除・移管の着地が続き（削除行 6,130 が追加行 5,085 を上回る）、減らす変更も docs/design を変えた着地に数えるためで、規則（コードと地図に分ける）の作りの問題ではない。
- 予算の値を変えるのは書き直しの後も概念と地図の形で収まらない文書が続くときだけ（[文書の規則](../development/documents.md)の「形と予算」）で、その状態は M1 にも M6 にも出ていない。

**残りの 30 KiB 超の文書**: 次の goal（書き直し）が要る。

- 後の期間の最後の日は 30 KiB 超が 27文書で、前（26）から減っていない。
  2026-10-17 の main（`a181216a`）では 30 KiB 超は 9文書（frontmatter を除いた大きさで予算を超えるのは許可の一覧 `.config/design-docs-allow.toml` の8文書: 地図の7文書 plan-planners・plan-review・authorization・run-environment・observer・draft-planners・supervise と概念の overview）に減った。
- 許可の一覧は値を増やさないので、この8文書は太らないが、予算に戻す task が無ければ一覧に残り続ける。
  goal 159 の受け入れ（4）の9文書は一覧から外れており、この8文書は goal 159 の外なので、別の goal で書き直す（follow_up）。
- M4 (a) の上位文書の grep の結果の中央値と p90 は 1.16〜1.17 倍に増えた。
  1文1行は変えた行だけに当たるので、残る長い行が grep で返る分が残ったと読める。
  書き直しの goal は残りの文書の長い行（1,024 byte 超）も1文1行に直す。

**worker が src/ を読む量**: 増えたことは想定どおりで、打ち手は要らない。

- worker の run ごとの src/ の読みは中央値 1.54 倍・p90 1.44 倍、worker の結果に占める src/ の割合は 0.50 から 0.55 に増え、docs/ の割合（0.23→0.24）は変わらない。
- 細かい事実を doc comment（src/）へ移した（4層の「詳細」）ので、詳細を src/ で読むのは目指した形である。
  1回の結果の大きさ（worker の中央値 1,142→976・p90 7,876→7,376）は下がった。
- 段3 と同じ時期に他の goal の着地も読む量を変えうるので、この値は段3 だけの効果ではない。

**claim の控え**: docs だけの衝突は減ったが、控えは増え、その原因はこの入力からは分からない。

- docs だけの衝突（M5）は着地あたり 0.24 から 0.13 に、expired の待ちの合計は 312h から 198h に下がった。
- 一方で控えの件数は着地あたり 3.8倍、待ちの合計は 4.6倍、expired の件数は着地あたり 1.46倍、M3 の docs/ の衝突（混在の event の docs の file を含む）も着地あたり 1.21倍になった。
  M3 の入力（`claim_deferrals` の件数・待ち・終わり方）からは控えの理由と path が分からず、docs の衝突が原因かも決められないので、原因を path と理由ごとに分けて測る（follow_up）。

**M5 残る衝突への軽い解消の job と task 1968**: どちらも要らない。

- frontmatter の日付（60→0）と同じ位置への追記（17→0）は無くなった。
  ADR 以外の文書の `updated`・`last_verified` を外したこと（task 1964）と ADR の索引の生成（task 1967）の効果で、規則で合わせられる軽い種類がもう残っていない。
- 残る 25件のうち 17件（0.68）は段落で、6件は箇条書き、2件は表とファイルである。
  段落の衝突は同じ段落を2つの task が書き換えたもので、意味を見ずに合わせる軽い job では正しく解けない。
  段落が 6 から 17 に増えたのは、後の期間に doc comment へ移す書き直しと文書の削除・移管が同じ文書に重なったためで、書き直しが落ち着けば減る見込み。
- task 1968（dagq.toml の path ごとのマージの仕方の一般化）が効くのは union で合わせられる追記と日付の行で、その種類は 0 件になった。
  表の行の 2件のために一般化を足す価値は無い。

**M6 docs の差し戻し**: 減っていない。増えた種類は規則と検査の作りから来ており、打ち手が要る。

- 主な理由が docs_drift の run あたりは 0.32 から 0.53（1.68倍）、reason_codes に docs_drift を含む run あたりは 0.48 から 1.17（2.43倍）に増えた。
  run あたりの review の数も 1.91 から 2.72 に増え、差し戻しの往復が増えている。
- 古い記述（93→37）は減った。
  design に細かい事実を書かなくなり、古くなる行が減ったためと読める。
- 増えたのは予算（6→16）・architecture.md の境界（6→14）・書き写し（8→11）・その他（20→30）で、その他を除く3つは段の規則と検査が新しく作った指摘である。
  その他は理由の文を読まないと出どころが分からない。
  予算は予算の値と許可の一覧（task 1943〜1945）が作った指摘で、予算の超過の指摘と予算に収めるための削除（規則が消さないとする行の削除）の指摘を含み、script の値からはどちらが多いか分からない。
  architecture.md の境界は境界を文書に記録する規則から来ており、その review の subagent（architecture-boundaries）は後の期間の 2026-10-13 に退役した（task 2392）ので、次の期間で減る見込み。
  書き写しは run の review が書き写しを指摘するようにした規則（[ADR-t1942-2](../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)）の作りから来る。
- 打ち手は2つ。
  1. 差分が触っていない文書の古い記述を差し戻さず follow_up にする規則（[ADR-t2352-1](../adr/2026-10-16-t2352-1-review-sends-back-only-documents-the-diff-touched-and-follows-up-the-rest.md)、後の期間の後に着地）の効果を、次の7日で同じ script で測る（follow_up）。
  2. 予算の種類の理由の文を読み、超過の指摘（`scripts/check-design-docs.sh` のプログラムの review と重なる）が多ければ全体の review の差し戻しの基準から外し、削除の指摘が多ければ規則の「消さない行」の書き方を見直す（follow_up）。

## 保存

週次の見直しは script の JSON の出力を `~/.local/share/dagq-hostmetrics/docs-slim/<since>_<until>.json`（UTC の日付）に、M5 の入力の events の JSON を同じ dir の `events-<since>_<until>-<頁>.json` に、M6 の入力の `review_finished` の events の JSON を `events-review-<since>_<until>-<頁>.json` に保存し、前週と基準値はそこから読む。
基準の期間 `[2026-09-29, 2026-10-06)` の入力と出力（`2026-09-29_2026-10-06.json`・`events-2026-09-29_2026-10-06-*.json`・`events-review-2026-09-29_2026-10-06-*.json`）は保存済みで、流し直さない（Claude Code は既定で30日より古い会話記録を消すので、消えた後に流すと M4 の欠けた値で上書きする）。
保存の記録はこの文書の「前後比較（段4）」の「期間と入力」が持つ。

区分: 保存する JSON は入力から `scripts/docs-metrics.py` で作り直せるビュー。
入力の main の git の履歴は SSOT、`dagq stats --since` と衝突の event の `dagq events --full` の JSON は EventStore から作り直せるビュー、Claude Code の会話記録は材料で、会話記録が消えた期間は作り直せない。
M6 の入力の `dagq events --full --kind review_finished` の JSON は EventStore から作り直せるビュー、M6 の出力（script の JSON の `m6`）は入力から作り直せるビューで、入力の JSON か EventStore の該当の期間の event が残る限り同じ表を作り直せる。
