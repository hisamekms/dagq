---
id: plan-docs-slim
type: plan
title: docs/design の4指標（総量と伸び・docs/design を変えた着地の割合・docs の衝突と claim の控え・道具の結果に占める docs）と docs だけの衝突の種類（M5）の定義と基準値
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
4指標と M5（docs だけの衝突の件数と種類の内訳、request 46）は `scripts/docs-metrics.py` が数え、`scripts/docs-metrics-test.py` が fixture（`scripts/docs-metrics-fixtures/`）で確かめる。
毎週の見直しで流して基準値と前週と並べる手順は `.claude/skills/throughput-review/reference/weekly.md` の「docs の4指標」が持つ。
KPI（runtime）にしないのは、M4 が runtime の持たない会話記録を読み、M1・M2 は git で足りるため。

## 測り方

```sh
python3 scripts/docs-metrics.py --since 2026-09-29T00:00:00Z --until 2026-10-06T00:00:00Z \
  --stats stats.json --claude-projects ~/.claude/projects --events events-*.json --format json
```

- 期間は `--since` と `--until`（UTC、`[since, until)` の半開区間。日付だけなら 00:00Z）。
- M1・M2 は `--repo`（既定は cwd）の `--ref`（既定は `main`）の履歴を読む。
- M3 は `--stats` に `dagq stats --since <since>` の JSON を保存したファイルを渡す（script は queue を読まない）。
- M4 は `--claude-projects` に Claude Code の会話記録の dir（`~/.claude/projects`）を渡す。
- M5 は `--events` に `dagq events --full --kind ...` の JSON を保存したファイル（複数可。頁送りの各頁）を渡し、`--repo` の git の object を読む。
- `--stats`・`--claude-projects`・`--events` を省くと M3・M4・M5 は `null`。`--metrics m1,m2` で選べる。`--format` は `markdown`（既定）か `json`。

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

- **対象の dir**: 名前が `-runs-<run id>-worktree` で終わる dir（dagq の `runs/<run id>/worktree` を cwd にした会話）だけを読む。
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
同じ定義の前の値は、着地の後の最初の週次の見直しが `[2026-09-29T00:00:00Z, 2026-10-06T00:00:00Z)` を script で流して保存する（weekly.md）。

### M4 の (a)(b)

前の値は task 1957 が同じ script・同じ母集団で `[2026-09-29, 2026-10-06)` について求める。
request 44 の全結果の中央値 1.1K・p90 6.9K は (a) の基準値に流用しない（全 tool_result の分位点から上位文書の grep の分位点は導けない）。

### M5（参考の値: request 46、人の session の値）

request 46（2026-10-06、人の session）が数えた docs の衝突 135件の内訳: 1 frontmatter の日付が 72件、2 同じ位置への追記が 18件、3 箇条書きが 29件、4 段落が 12件、5 表の行とファイルの追加・削除が 4件。
分類の手順はこの script と別（その場の数え方）なので、上の定義と比べられる基準値ではない。

script の基準の期間 `[2026-09-29, 2026-10-06)` の値は、worker が本番の queue を読まないので task 1957 が同じ script で求める。
request 46 の値との差（件数と種類の違い）と理由（分類の規則・再現の仕方・対象の kind の違い）も task 1957 がここに書く。

## 保存

週次の見直しは script の JSON の出力を `~/.local/share/dagq-hostmetrics/docs-slim/<since>_<until>.json`（UTC の日付）に、M5 の入力の events の JSON を同じ dir の `events-<since>_<until>-<頁>.json` に保存し、前週と基準値はそこから読む。
Claude Code は既定で30日より古い会話記録を消すので、基準の期間の M4 は 2026-10-28 より前に流して保存する。
