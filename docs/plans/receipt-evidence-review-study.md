---
id: plan-receipt-evidence-review-study
type: plan
title: runのreviewのsubagent receipt-evidenceの選ばれ方・トークン・行き先と、親のreviewとの検査の重なりの測定と、選択肢と推奨
status: active
created: 2026-10-08
owners:
  - hisamekms
tags:
  - measurement
  - review
related:
  - adr-t1453-1
  - adr-t1895-1
  - adr-t1895-2
  - development-local-checks
  - plan-review-subagents-spike
---

# runのreviewのsubagent receipt-evidenceの選ばれ方・トークン・行き先と、親のreviewとの検査の重なりの測定と、選択肢と推奨

`receipt-evidence`（`.dagq/agents/receipt-evidence/AGENT.md`、`dagq.toml`の`[review.subagents.receipt-evidence] paths = ["**"]`）は、どのrunのreviewにも必須のsubagentとして付く。
この文書は、固定の7日の記録からその選ばれ方・費用・行き先を数え、親のreviewのpromptとの検査の重なりを項目ごとに対応づけ、選択肢を比べて推奨を出す。
変更はしない（人が選んだ後に別の依頼で行う）。

## 1. 窓・母集団・読んだコマンド

- **窓（固定）**: 2026-10-01T00:00:00Z〜2026-10-08T00:00:00Z（UTC）。
- **母集団**: `review_started`の`created_at`が窓の中にあるrunのreviewの試み`(run_id, attempt)`。`run_id`の無いevent（plan review・goal review・plannerのsession）は対象外。窓の前に始まり窓の中で終わった試みは数えない。母集団は**1073試み**（`(run_id, attempt)`の重複は0）。
- **読んだコマンド**（本番queueを固定バイナリ`~/.local/bin/dagq`の読み取りだけのコマンドで読んだ）。どちらも前の出力の`cursor`を`--after`に渡し、`events`が空になるまで読んだ。

  ```sh
  # (i) 開始: 2ページ（1000件と73件）、3ページ目が空
  dagq events --full --kind review_started --since 2026-10-01 --until 2026-10-08 --limit 1000 --after 0
  dagq events --full --kind review_started --since 2026-10-01 --until 2026-10-08 --limit 1000 --after 127089
  dagq events --full --kind review_started --since 2026-10-01 --until 2026-10-08 --limit 1000 --after 136484   # 空
  # (ii) 終了とsession: 窓の終わりから1日後（2026-10-09）まで。11ページ（1000件×10と249件）、12ページ目が空
  dagq events --full --kind review_finished --kind review_retried --kind review_failed --kind session_opened --kind session_closed --since 2026-10-01 --until 2026-10-09 --limit 1000 --after 0
  #   … --after 61117, 67827, 75217, 82147, 97625, 105421, 113092, 120081, 127628, 134584, 136484（空）
  ```

- **読んだ時点の限り**: 読んだのは2026-10-08で、(ii)の最後のeventは2026-10-08T09:37:03Z。終了を探す範囲は窓の終わりから1日後までの予定だったが、実際はこの時点までになった。終了の無い試み（下のm1）の75件は全て、同じrunの次のattemptの`review_started`が終了より先に来ていたので、読んだ時点が早いことで終了なしに数えた試みは無い。終了のある998試みの最も遅い終了は2026-10-07T23:54:08Z、全1073試みに対応する`session_closed`があり（m5は0）、読んだ時点より後に終了やsessionの閉じが来る試みも無い。
- **周回数が当たらない理由**: 過去の記録を1回読む測定で、同じコマンドを繰り返しても値は変わらない。周回数と交互の実行は当たらない。
- **差分のpathの出所**: `review_started.subagents`の`receipt-evidence`の`paths`（`**`に当たったpath、つまりそのreviewの範囲の変えたpathの全て）を第一にした。`subagents`の無い試み（subagentの仕組みの前と、Codexのreview）は、着地のcommit（`main`の`Dagq-Run`のtrailer）の差分、無ければ`refs/dagq/runs/<run_id>`とmainのmerge baseからの差分で分類した。taskの`--paths`と`change`は、workerのpromptが`dagq show`を打たないと決めているので使っていない。
- **集計の道具**: `receipt-evidence-review-study/`の`analyze.py`（試みごとの表`attempts.csv`を作る）・`stats.py`・`stats2.py`・`stats3.py`（集計と代理値）・`sample.py`（4の標本）。手順は次のとおり（`$D`は作業のdirectory）。
  1. 上の各ページの出力を`$D/start-<n>.json`・`$D/end-<n>.json`に置き、`jq -s '[.[].events[]]'`でそれぞれ`$D/start.json`・`$D/end.json`にまとめる。
  2. 着地のcommitとrunの対応を`git log main --since=2026-09-29 --format='%H %(trailers:key=Dagq-Run,valueonly,separator=)' | awk 'NF==2' > $D/landed.txt`で作る。
  3. `python3 -I analyze.py $D <worktree> <出力のcsv>`（`$D/rows.json`も書く）、`python3 -I stats.py $D`（`$D/only.json`も書く）、`python3 -I stats2.py $D`、`python3 -I stats3.py $D stats2.py`、`python3 -I sample.py $D`の順に流す。

### 記録に無かった値

- **subagentごとのトークンと時間**: 無い。`session_closed.tokens`と`active_secs`はreviewのsession全体（親と全てのsubagent）の値で、`review_finished`の`tokens`（`model_usage`）もsession全体の値である。agentごとに残るのは`agents[]`の`verdict`・`reasons`・`summary`と`route.agents[].destination`だけ。
- **親が独自に見つけた指摘か**: 親のverdictはsubagentの結果を取り込む（`SUBAGENTS_INSTRUCTION`）ので、親の`reasons`にある指摘を親が自分で見つけたのかは記録から分からない。
- **Codexのreviewのトークンとactive_secs**: Codexのreviewの`session_closed`は`active=unavailable`・`active_unavailable=transcript_not_claude`で`tokens`と`active_secs`を持たない（例: run 732f927d-1fd4-488e-9d19-52e4e23b79d0 attempt 1のevent 60191に対応するevent 60200）。
- **`retried`・`failed`の試みのagentと行き先**: `review_retried`・`review_failed`は`agents`と`route`を持たない。

## 2. 試みごとの表と欠測

試みごとの表は[receipt-evidence-review-study/attempts.csv](receipt-evidence-review-study/attempts.csv)（1073行）に置いた。

- 列: `run_id`・`task_id`・`attempt`・`started_at`・`result`（下の6種）・`cause`・`error`・`code`・`path_class`（差分のpathの種類）・`path_source`（分類の出所）・`n_paths`・`selected`（`review_started.subagents.agents`の名前）・`re_verdict`・`re_dest`（receipt-evidenceのverdictと`route.agents[].destination`）・`others_dest`（他のagentの行き先）・`parent_dest`（`route.parent`）・`final_dest`（`route.destination`）・`provider`・`switched_from`・`switch_reason`・`wall_secs`・`duration_secs`・`active_secs`・`input`・`output`・`cache_creation`・`cache_read`・`missing`（欠測の種類）・`m6_why`・`m7_why`。
- **終了の対応づけ**: 同じ`run_id`と`attempt`の`review_finished`・`review_retried`・`review_failed`のうち、開始より後で最初のもの。`review_retried`はそのattemptを終えた試みとして`retried`にし、次のattemptは別の試みにした。同じrunの次の`review_started`が終了より先に来た試みは終了なしにした。
- **sessionの対応づけ**: `review_started.session_id`と同じ`session_id`の`session_closed`。
- **差分のpathの種類**: `docsだけ`（全てが`docs/**`かrootの`*.md`）、`configだけ`（全てが`dagq.toml`・`.config/**`・`.dagq/**`）、`docsとconfigだけ`、`runtimeを含む`（`src/**`・`crates/**`・`migrations/**`・`build.rs`のどれかを含む）、`その他`（`tests/**`・`plugins/**`・`scripts/**`・`.github/**`・`.claude/**`などを含みruntimeを含まない）、`差分なし`。

### 結果ごとの件数

| 結果 | 全体 | receipt-evidenceが選ばれた | 選ばれなかった |
| --- | ---: | ---: | ---: |
| pass | 508 | 365 | 143 |
| revise | 364 | 254 | 110 |
| concern | 108 | 78 | 30 |
| retried | 4 | 4 | 0 |
| failed | 14 | 14 | 0 |
| 終了なし | 75 | 58 | 17 |
| 計 | 1073 | 773 | 300 |

- `failed`の14件は全て`code=job_failed`（600秒の時間切れ11件、`exit status: 1` 3件）で、`error`がreceipt-evidenceを名指すものは0件。
- `retried`の4件の`cause`は`unreadable` 1件と無し3件。`error`は必須のsubagentの結果の欠け3件（adr-rules・architecture-boundaries・receipt-evidenceを1件ずつ名指す）と、verdictのJSON無し1件。receipt-evidenceを名指すのは1件（run f68cc82a-1bc6-4f40-a5a5-ea5300ca726c attempt 4、`receipt-evidence did not complete (revise)`）。

### 欠測の種類と件数

| 種類 | 件数 | 内訳 |
| --- | ---: | --- |
| m1 終了なし | 75 | 選ばれた58・選ばれなかった17。75件とも次のattemptの`review_started`が先に来た |
| m2 retried・failedで`agents`・`route`が無い | 18 | retried 4・failed 14（全て選ばれた試み） |
| m3 `review_finished`に`agents`が無い | 283 | `review_started`の`subagents`にreceipt-evidenceがあった0・無かった283（Codex 200・subagentの仕組みの前のClaude 83） |
| m4 `route`が無い | 283 | m3と同じ283件 |
| m5 対応する`session_closed`が無い | 0 | — |
| m6 トークンが無効 | 215 | `tokens`無し（`transcript_not_claude`）208、`tokens`無し（`transcript_missing`）1、`reason=inferred`で4欄が全て0 6（例: run f68cc82a attempt 2）。欄の一部だけ有るものは0 |
| m7 `active_secs`が無効 | 209 | `active=unavailable`の`transcript_not_claude` 208、`transcript_missing` 1 |

Codexのreviewの208試みは全てm6とm7に当たる（m6とm7は重なってよい）。

### 指標ごとの有効件数と除外件数

| 指標 | 母集団 | 有効 | 除外（種類ごと） | 選ばれた試みの有効（除外） | 選ばれなかった試みの有効（除外） |
| --- | ---: | ---: | --- | --- | --- |
| トークン（母集団−(m5∪m6)） | 1073 | 858 | m5 0・m6 215 | 766（m6 7） | 92（m6 208） |
| active_secs（母集団−(m5∪m7)） | 1073 | 864 | m5 0・m7 209 | 772（m7 1） | 92（m7 208） |
| 壁時計の時間（母集団−m1） | 1073 | 998 | m1 75（m5〜m7では除外しない） | 715（m1 58） | 283（m1 17） |

## 3. 集計

### (a) receipt-evidenceが選ばれた回数

- 母集団1073試みのうち**773回（72.0%）**。
- 選ばれなかった300試みは、subagentの仕組みが動く前（最初の`subagents`付きの`review_started`は2026-10-03T04:15:16Z）の298試み（Claude 92・Codex 206）と、変えたpathが無くagentが選ばれなかったCodexの2試み。
- 仕組みが動いた後の775試みでは、差分のある773試みの全てで選ばれた（`paths = ["**"]`のため）。
- 選ばれた773試みは全て`switched_from=codex`・`switch_reason=subagents_unsupported`でClaudeに切り替わっていた。

### (b) トークン

`total = input + output + cache_creation + cache_read`は4種を同じ重みで足した値で、料金の重みではない。`cache_read`を除いた値（`input + output + cache_creation`）も並べる。p90は最近順位法（昇順の`ceil(0.9n)`番目）。母集団はトークンが有効な試み（m5・m6を除く）。

| 種類 | 全体（有効858） 合計 / 中央値 / p90 | 選ばれた（有効766） 合計 / 中央値 / p90 | 選ばれなかった（有効92） 合計 / 中央値 / p90 |
| --- | --- | --- | --- |
| input | 27,458 / 32 / 48 | 25,568 / 32 / 48 | 1,890 / 16 / 38 |
| output | 6,995,484 / 8,066.5 / 12,499 | 6,545,221 / 8,325 / 12,499 | 450,263 / 3,369 / 12,222 |
| cache_creation | 63,646,917 / 72,614 / 112,395 | 55,354,536 / 70,487.5 / 109,135 | 8,292,381 / 84,894 / 125,518 |
| cache_read | 785,896,313 / 794,345.5 / 1,622,233 | 696,315,380 / 807,813 / 1,614,848 | 89,580,933 / 551,086 / 1,967,873 |
| total | 856,566,172 / 869,078.5 / 1,748,138 | 758,240,705 / 886,123 / 1,732,529 | 98,325,467 / 632,436.5 / 2,099,377 |
| cache_readを除く | 70,669,859 / 80,485 / 124,064 | 61,925,325 / 78,972.5 / 120,136 | 8,744,534 / 86,849.5 / 137,125 |

- 選ばれなかった試みでトークンが有効な92件は全て2026-10-01〜10-02のClaudeのreviewで、選ばれた試み（10-03以後）とは日付・promptの版・subagentの有無が違う。
- 選ばれた試みのcache_readの中央値は選ばれなかった試みより大きい（807,813対551,086）が、cache_readを除いた値の中央値は小さい（78,972.5対86,849.5）。

### (c) 時間

壁時計の時間は終了のeventの`created_at`−`review_started.created_at`。母集団は終了のeventがある試み（m1を除き、トークンやactive_secsの欠測は含める）。`active_secs`の母集団はactive_secsが有効な試み（m5・m7を除く）。

| 指標 | 全体 合計 / 中央値 / p90（有効） | 選ばれた（有効） | 選ばれなかった（有効） |
| --- | --- | --- | --- |
| 壁時計（秒） | 130,089 / 112 / 205（998） | 100,915 / 118 / 221（715） | 29,174 / 89 / 166（283） |
| `duration_secs`（秒） | 127,147 / 111 / 202（994） | 98,885 / 117 / 217（711） | 28,262 / 88 / 163（283） |
| active_secs（秒） | 75,292 / 81 / 137（864） | 68,790 / 84 / 135（772） | 6,502 / 43 / 149（92） |

- `duration_secs`は`review_finished`・`review_failed`にだけあり、`retried`の4件には無い（994件）。
- 壁時計との食い違い（5秒を超える）は4件で、全て選ばれなかった試み: run 8dcd7775 attempt 1（壁時計1060秒・`duration_secs` 425秒）、aa91a7cb attempt 3（174・155）、aa9fa5aa attempt 1（149・130）、e94f55e2 attempt 4（22・16）。選ばれた試みの差は最大5秒。

### (d) 行き先・差分のpathの種類・結果

receipt-evidenceの行き先（選ばれた773試み）は次のとおり。

| receipt-evidenceの行き先 | 件数 |
| --- | ---: |
| land | 645 |
| send_back | 48 |
| ask | 4 |
| 無し（m1 58・m2 18） | 76 |

receipt-evidenceのverdictはpass 634・revise 48・concern 15。concern 15のうち11はlandの行き先になった。

review全体の行き先（`route.destination`）はland 410・send_back 263・ask 24・無し376（`route`の無いm1〜m4）で、親の行き先（`route.parent`）はland 410・send_back 266・ask 21・無し376。`route`のある697試みは全てreceipt-evidenceが選ばれた試みである。

差分のpathの種類ごとの件数は次のとおり。

| 差分のpathの種類 | 全体 | 選ばれた | 選ばれなかった | receipt-evidenceがlandより重い行き先 |
| --- | ---: | ---: | ---: | ---: |
| runtimeを含む | 725 | 546 | 179 | 47 |
| docsだけ | 157 | 95 | 62 | 2 |
| その他 | 152 | 108 | 44 | 3 |
| docsとconfigだけ | 25 | 14 | 11 | 0 |
| configだけ | 12 | 10 | 2 | 0 |
| 差分なし | 2 | 0 | 2 | 0 |

分類の出所は`review_started.subagents` 773、着地のcommit 274、runのref 24、`subagents`のagent無し2。

結果ごとの件数は上の「結果ごとの件数」の表のとおり。retriedの`cause`は`unreadable` 1・無し3で、`error`がreceipt-evidenceを名指すのは1件。

## 4. receipt-evidenceだけがlandより重い行き先を出した試み

### (a) 記録から分かる上限

- 分母は、receipt-evidenceが選ばれ、m2〜m4（とm1）に当たらない**697試み**。
- receipt-evidenceの行き先がlandより重い試みは52件。
- そのうち他の全agentがlandの試みは**25件（3.6%）**。行き先はsend_back 23・ask 2で、差分の種類はruntimeを含む22・その他2・docsだけ1。
- 25件とも親の行き先（`route.parent`）はreceipt-evidenceと同じで、親がlandだった試みは0件。親のverdictはsubagentの結果を取り込むので、記録だけでは親が独自に見つけたかは分からない。

### (b) 標本の判定

25件は20件を超えるので、`run_id#attempt`の昇順に並べ、`i = 0..19`について`floor(i×25/20)`番目の20件を取った（`sample.py`）。
外れた5件は11752111#1・65029e8f#2・96309350#2・ccaed544#1・f4b8ea86#2。
receipt-evidenceと親の`reasons`を読み、行き先を重くした指摘が親のpromptの明示の検査（受け入れ条件の照合・`REVIEW_DOCS_CHECK`の文書の照合・コードの正しさ）でも見つかるものか、receipt-evidenceの項目（A-xxx）でしか見つからないものかを判定した。
親のpromptは`REVIEW_RULES`でAGENTS.mdと名指す文書の規則も読ませるので、「receipt-evidenceだけ」は「親のpromptが明示しない`local-checks.md`の手順の規則でしか見つからない」の意味である。
判定は読み手（このtaskのworker）の判断である。

| # | run#attempt（task） | 指摘の要約 | 当たるA-xxx | 判定 | 根拠 |
| --- | --- | --- | --- | --- | --- |
| 1 | 01881c1c#1（1861） | 文書とdoc commentの「1回だけ数える」がコードの3節と食い違う | 無し | 親でも見つかる | 文書とコードの食い違いで`REVIEW_DOCS_CHECK`の範囲 |
| 2 | 029cc29d#1（775） | 受け入れ条件の「warnは1回」をtestが示せず、満たさない項目をfollow_upに回してsucceeded | A-257 | 親でも見つかる | 受け入れ条件の未達で、親の受け入れ条件の照合が見る |
| 3 | 068c08e3#2（1481） | 変えたunit testがstressに無い | A-071（軽微にA-257） | receipt-evidenceだけ | stressの対象の規則で、受け入れ条件と文書の照合は見ない |
| 4 | 0f387041#1（2057） | stressとitの時間の関門をしなかった理由、fmt・clippyの記録が無い | A-077・A-058・A-219 | receipt-evidenceだけ | 手元の検証の記録の規則（ci.ymlだけの差分） |
| 5 | 14f3c0be#4（1440） | 受け入れ条件が消すとしたtestが残り、測定の行が欠け、件数の誤り | A-257 | 親でも見つかる | 受け入れ条件の未達とreceiptの誤り |
| 6 | 1dc0d5cf#3（1632） | 変えたitのtest 12本に時間の関門を当てず、変えたunit testを流さずstressもしない | A-219・A-059・A-071 | receipt-evidenceだけ | testの範囲とstressと時間の関門の規則 |
| 7 | 4383f305#2（1439） | `--test it`をfilterなしで3回流した（人の判断） | A-060・A-057 | receipt-evidenceだけ | 手元で流してよい範囲の規則（合計の誤りは軽微） |
| 8 | 49be069c#1（1981） | stressと時間の関門が無い、受け入れ条件2のtestをfollow_upに回した | A-071・A-076・A-219・A-257 | 両方 | 受け入れ条件2の未達は親も見つかり、stressはreceipt-evidenceの規則 |
| 9 | 7c88cb42#3（1641） | 変えたunit test 4本を流さずstressもせず、helperの扱いの記録が無い | A-059・A-071・A-076・A-061 | receipt-evidenceだけ | testの範囲とstressの規則 |
| 10 | 8db1a6cd#3（838） | 新しい・変えたunit test 3本をstressしていない | A-071 | receipt-evidenceだけ | stressの規則 |
| 11 | 8eeacc00#1（1247） | 変えたtest 6本をstressせず、module全体を流していない | A-071・A-059 | receipt-evidenceだけ | stressとtestの範囲の規則 |
| 12 | 94987414#2（1434） | 受け入れ条件が求めるmoduleごとの実測の時間が無く推定、rebase後に流し直していない | A-257 | 親でも見つかる | 受け入れ条件がevidenceの値を項目ごとに求めている |
| 13 | 98a2d261#3（1518） | stressが無い、pluginの文書の変更の後に`--test plugin`を流していない | A-071・A-063・A-060 | receipt-evidenceだけ | stressと全体を比べるtestの規則 |
| 14 | a1b7032b#1（1570） | 変えたtest 2本のstressが無い | A-071 | receipt-evidenceだけ | stressの規則 |
| 15 | a943da00#3（1574） | 新しいunit testがfilterに当たらず流れていない、stressが無い、対応づけのtest名の誤り | A-059・A-071・A-257・A-219 | receipt-evidenceだけ | 決め手はtestの範囲とstress（名前の誤りは親も挙げた） |
| 16 | cb8fbdad#2（2035） | 足した・変えたunit testのstressが無い（人の判断） | A-071・A-077 | receipt-evidenceだけ | stressの規則 |
| 17 | d8b9c5e3#1（1438） | 10本のうち一部しかstressせず名前も無い、helperの残りの扱いの記録が無い | A-071・A-076・A-061 | receipt-evidenceだけ | stressとtestの範囲の規則 |
| 18 | dcf01067#1（2085） | psの書式を変えたがitのstubが古く着地で落ちる、関係するitのmoduleを流していない | A-059 | 両方 | 落ちるtestはコードの正しさとして親も見つかり、流す範囲はreceipt-evidenceの規則 |
| 19 | dcf01067#2（2085） | 時間の関門の結果が最後の差分と合わない、変えたunit testをstressしていない | A-219・A-071 | receipt-evidenceだけ | 時間の関門とstressの規則 |
| 20 | ebe22e74#4（1704） | ADRの決定がADR-0047決定12を変えるのに`amends`・索引・receiptに無い | 無し | 親でも見つかる | 文書（ADR）の照合とtaskの指示の照合（adr-rulesはlandにした） |

判定ごとの件数: **receipt-evidenceだけ13**（うちstressのA-071・A-077を含むもの12。含まないのは#7だけ）、**親でも見つかる5**、**両方2**。
「両方」の2件は、親でも見つかる指摘だけで同じ行き先（send_back）になるので、receipt-evidenceが無くても行き先は変わらなかったと読める。
標本の比を25件に当てると、receipt-evidenceの項目でしか見つからない指摘で行き先が重くなった試みは約16件（697試みの約2.3%）と見積もれるが、標本の外挿で確度は低い。

## 5. 検査項目の対応表

親のreviewのpromptの検査（`src/application/prompt.rs`の`review_prompt`）は次の5つに分ける。

- P1: 受け入れ条件の照合（acceptanceを本文に載せ、passを「受け入れ条件とtaskの指示を満たす」と定める）
- P2: 文書の照合（`REVIEW_DOCS_CHECK`）
- P3: repositoryの規則（`REVIEW_RULES`。AGENTS.mdと名指す文書の規則で判定する）
- P4: reviseの定義の「missing tests or evidence、formatter・linterの指摘、diffと食い違うreceipt」
- P5: concernの「受け入れ条件との食い違い・頼まれていない変更」

機械の列は、goal 153のprogramのreview（[ADR-t1895-2](../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)）のscriptにできるかを書く。
programのreviewはtestを流さず、検査の対象はrun worktreeなので、receiptの欄を読む検査は、receiptをprogramに渡す決定が別に要る（表では「receiptが要る」と書く）。

| 項目 | 中身 | 重なる親の検査 | receipt-evidenceだけ | 親だけ | 機械で検査できるか |
| --- | --- | --- | --- | --- | --- |
| A-057・A-093 | workerは全体の`cargo test`とllvm-covを流さない | P3（一般） | 明示はreceipt-evidenceだけ | — | 不可（何を流したかはreceiptの自由文。文字列の検出は弱い） |
| A-058 | fmtとclippyを流す | P4（formatter・linter） | 記録の有無 | — | 一部（`cargo fmt --check`はprogramのreviewで流せる。clippyはbuildが要りADR-t1895-2決定1の外） |
| A-066 | verifyのうちcoverageの関門と全体のtest以外を流す | P3（一般） | ○ | — | 一部（verifyの各行がevidenceに出るかの文字列の照合、receiptが要る） |
| A-219 | 手元の検証の繰り返し（layer-depsと時間の関門を含む） | P3（一般） | ○ | — | 一部（`check-layer-deps.sh`は形式の検査でprogramにできる。時間の関門はnextestの出力が要り不可） |
| A-059・A-065 | 関係するtestを`<module>::`で流し、範囲を書く | P4（missing evidence） | ○ | — | 一部（diffの変えたmoduleの名前がevidenceにあるかの照合、receiptが要る） |
| A-060・A-220 | filterなし・接頭辞だけ・全体を流さない | P3（一般） | ○ | — | 一部（evidenceの`--test it`の後のfilterの有無、receiptが要る） |
| A-061 | helperを変えたら代表のmoduleと残りの任せ先を書く | P3（一般） | ○ | — | 一部（`tests/common`・`runtime_support`を変えた差分でevidenceの記載の有無、receiptが要る） |
| A-062 | brokerのcrateは`-p <crate>` | P3（一般） | ○ | — | 可（`crates/<crate>`を変えた差分で`-p <crate>`の有無、receiptが要る） |
| A-063 | 全体を比べるtest（migration・`doctor`/`status`・pluginの文書） | P3（一般） | ○ | — | 可（pathの規則から求めるmodule名・`--test plugin`の有無、receiptが要る） |
| A-067・A-218・A-225 | e2eを流さず`not_applicable`と理由 | P3（一般）。worker promptにも同じ指示 | ○ | — | 可（receiptの`e2e.status`と理由の有無、receiptが要る） |
| A-068 | `e2e_failed`のresumeの例外 | P3（一般） | ○ | — | 不可（resumeの理由はqueueの記録で、programのreviewはqueueを読まない） |
| A-071・A-223 | 足した・変えた`#[test]`をstressする | P4（missing evidence） | ○ | — | 可（diffから変えた`#[test]`を列挙し、名前がevidenceにあるか。列挙は`check-it-test-time.sh`と同じ考え方。receiptが要る） |
| A-073 | 15秒を超えるtestは`--stress-duration` | P3（一般） | ○ | — | 不可（1周の時間は流さないと分からない） |
| A-075 | 落ちたら原因を直してから通す | P3（一般） | ○ | — | 不可 |
| A-076 | evidenceに名前・周回・結果 | P4 | ○ | — | 一部（周回と結果の書式の検出、receiptが要る） |
| A-077 | 変えたtestが無ければしないことと理由 | P4 | ○ | — | 可（diffに変えたtestが無いときevidenceにstressの記載があるか、receiptが要る） |
| A-078 | cargo-nextestが無ければしない理由 | P3（一般） | ○ | — | 不可 |
| A-080・A-081・A-221 | 全体のtestを流してよいのは検証が落ちたresumeの再現だけ | P3（一般） | ○ | — | 不可（resumeの理由はqueueの記録） |
| A-257 | 受け入れ条件の対応づけ、満たせない項目をfollow_upに回してsucceededにしない、根拠の書き方、測定の周回数 | **P1・P5**（受け入れ条件の照合と未達）、P4（receiptの食い違い） | 根拠の書式（`<module>::<test>`・`--since`/`--until`）と周回数の比較 | — | 一部（summaryに受け入れ条件の番号ごとの対応があるかの検出だけ。満たすかの判断は不可） |
| 親だけ | 文書の照合（docs_drift、コードの書き写しと経緯の混入） | P2 | — | ○ | 一部（ADR-t1895-2の形式の検査の範囲） |
| 親だけ | コードの正しさ・頼まれていない変更・concernの判断 | P1・P5 | — | ○ | 不可 |

名指しで重なるのはA-257（P1・P5と）とA-058（P4のformatter・linterと）である。
A-059・A-065・A-071・A-076・A-077はP4の「missing tests or evidence」の一般の言い方にだけ重なり、どのtestを流しどれをstressするかの規則は親のpromptに無い。残りの項目は親のpromptがP3で一般に指すだけで、規則に沿った明示の検査はreceipt-evidenceだけが持つ。
4の標本でもA-257が決め手の指摘（#2・#5・#12）は親も受け入れ条件の未達として挙げていた。

## 6. 選択肢の比較

### 費用の列の前提

- 記録はreviewのsession全体の値（親と全てのsubagent）で、receipt-evidence単体の値ではない。単体の削減量は記録からは算出できない。
- 親と他のagentは選択肢の後も動くので、その費用を削減量に数えない。
- 代わりに次の代理値を出す。
  - (i) 同じ差分の種類の中で、選ばれた試みと選ばれなかった試みのsessionの中央値の差。
  - (ii) 選択肢でreceipt-evidenceが外れる試みの数×(i)。
- (i)は因果ではない。選ばれなかった試みは2026-10-01〜10-03のsubagentの仕組みの前の試みで、他のsubagentも動いていない（選ばれた試みの多くは他に2〜6本のagentを並列に動かした）。promptの版と差分の大きさも違う。このため(i)はreceipt-evidence単体より、subagent全体と時期の違いを含んだ大きめの値になりうる。
- (i)が負か、その指標の有効件数でどちらかの組が10件未満なら「算出不能」とする。
- トークンの(ii)からは、選択肢でCodexに戻れる試みを外す（ClaudeのsessionのトークンがCodexの費用に置き換わるため。下の「Codexに戻れる割合」の列に別に書く）。
- 時間は、subagentが並列に走るので、1本外しても壁時計は最も遅いagentの分しか縮まらない。

差分の種類ごとの(i)は次のとおり（中央値、括弧は有効件数で選ばれた組・選ばれなかった組の順）。

| 差分の種類 | total（トークン） | cache_readを除く | 壁時計（秒） | active_secs（秒） |
| --- | --- | --- | --- | --- |
| docsだけ | 486,251−323,931（92・9）→ 算出不能（9件） | 負 → 算出不能 | 84.5−105＝−20.5（88・59）→ 算出不能（負） | 55−21（94・9）→ 算出不能（9件） |
| configだけ | 算出不能（10・0） | 算出不能（10・0） | 53.5−39.5（10・2）→ 算出不能（2件） | 算出不能（10・0） |
| docsとconfigだけ | 489,038.5−919,035.5（14・4）→ 算出不能（4件・負） | 負 → 算出不能 | 77−48.5＝28.5（13・10） | 59−39.5（14・4）→ 算出不能（4件） |
| runtimeを含む | 1,108,120−908,996＝199,124（543・67） | 負 → 算出不能 | 141−98＝43（501・170） | 97−53＝44（546・67） |
| その他 | 578,190−290,155＝288,035（107・12） | 負 → 算出不能 | 91−50.5＝40.5（103・40） | 67−26.5＝40.5（108・12） |

壁時計の選ばれなかった組には、トークンとactive_secsが欠測のCodexのreviewも含む（Claudeのreviewだけで比べると選ばれなかった組の中央値はさらに短く、同じ向きである）。

### 選択肢でreceipt-evidenceが外れる試み

- **B（pathsを絞る）**: globの案は`paths = ["src/**", "crates/**", "migrations/**", "build.rs", "tests/**", "Cargo.toml", "Cargo.lock", "scripts/**", "plugins/**", ".github/**", ".config/**"]`。`docs/**`・rootの`*.md`・`dagq.toml`・`.dagq/**`・`.claude/**`・`.githooks/**`だけの差分から外す。窓の選ばれた773試みに当てると120試みで外れる（docsだけ95・configだけ10・docsとconfigだけ12・その他3。docsとconfigだけの2試みは`.config/**`を含むので残る）。
- **D（廃止）**: 選ばれた773試みの全て。
- **A・C・E**: 外れる試みは0（receipt-evidenceは残り、Cは項目を減らし、Eは項目の一部を機械に移す）。

### Codexに戻れる試み

分母は`switch_reason=subagents_unsupported`の773試み。今の`dagq.toml`の他のagentのglobを、各試みの変えたpathに当てて数えた。括弧は記録の`selected`から数えた値で、窓の間に他のagentのglobが広かった時期を含む。

- B: 30試み（3.9%。docsだけ24・configだけ2・docsとconfigだけ1・その他3）（記録の選択では4）。
- D: 42試み（5.4%。docsだけ24・その他15・configだけ2・docsとconfigだけ1）（記録の選択では10）。
- A・C・E: 0。

### 比較の表

| 選択肢 | トークンへの効き（7日） | 時間への効き（7日） | 見逃しの危険（4の判定から） | Codexに戻れる割合 | goal 153・154との関係と順番 |
| --- | --- | --- | --- | --- | --- |
| A 今のまま | 0（変えない） | 0。receipt-evidenceを名指すやり直しで足した時間は記録から1件で、run f68cc82a attempt 4（retried、279秒）の次の試みattempt 5が600秒の時間切れ（壁時計603秒）。時間切れの11件はagentを名指さないので割り当てられない | 無し | 0/773 | 独立。goal 154の後は`subagents_unsupported`の切り替えが無くなり、receipt-evidenceも独立のjobとしてCodexで動かせる |
| B pathsを絞る | 算出不能（理由: 外れる120試みのうちCodexに戻れる30を除いた90試み（トークンが有効なのはdocsだけ69・configだけ8・docsとconfigだけ11）は、どの組も(i)の選ばれなかった組が10件未満（9・0・4）。その他の3試みは全てCodexに戻れる） | 壁時計: 代理値（仮定: 差分の種類の中の中央値の差がreceipt-evidenceの有無による。外れる試み×(i)）＝docsとconfigだけ11×28.5秒＝313.5秒（有効件数: 選ばれた13・選ばれなかった10）＋その他3×40.5秒＝121.5秒（103・40）、計435秒。docsだけ（負）とconfigだけ（2件）は算出不能。active_secs: その他3×40.5秒＝121.5秒（108・12）、docsだけ（9件）・configだけ（0件）・docsとconfigだけ（4件）は算出不能 | 低い。外れる120試みでreceipt-evidenceがlandより重かったのは2件で、1件（5446ca44#1）は他のagentもsend_back、1件（ebe22e74#4）は4の判定で親でも見つかる。ただしdocsだけの測定のtaskで、A-257の測定の根拠の書き方（周回数・`--since`/`--until`）の検査がreceipt-evidenceから外れる | 30/773（3.9%） | 独立して先にできる。同じ`dagq.toml`の`[review.subagents.*] paths`を変えるrequest 80のtask（2106・2107）の着地の後に回し、衝突を避ける。goal 154の後もpathsの設定は残るので無駄にならない |
| C 重なる規則（A-257）を親に寄せる | 算出不能（理由: 外れる試みは0。項目を減らしたreceipt-evidenceの1回あたりの減りは、subagentごとのトークンが記録に無く出せない） | 算出不能（理由: 同じ） | 低い。標本でA-257が決め手の3件（#2・#5・#12）は全て親も受け入れ条件の未達として挙げた。根拠の書式（`<module>::<test>`）の指摘は軽微の扱いが多い（#3・#15）。測定のtaskの周回数の比較は親のP1が受け入れ条件として読む | 0/773 | 独立。`.dagq/agents/receipt-evidence/AGENT.md`の1行だけ。goal 154の後もagentの定義は残るので無駄にならない |
| D 廃止して親に吸収 | 代理値（仮定: 差分の種類の中のtotalの中央値の差がreceipt-evidenceの有無による。Codexに戻れる42試みを除いた外れる試み×(i)）＝runtimeを含む543×199,124＝108,124,332（有効件数: 選ばれた543・選ばれなかった67）＋その他92×288,035＝26,499,220（107・12）、計約1.35億。docsだけ（9件）・configだけ（0件）・docsとconfigだけ（4件・負）は算出不能。cache_readを除く値はどの組も負で算出不能。(i)は他のsubagentと時期の違いを含むので、この値は単体の削減量を大きく見積もる。親のpromptが規則を読む分の増えは数えていない | 壁時計: 代理値（同じ仮定）＝runtimeを含む501×43秒＝21,543秒（501・170）＋その他103×40.5秒＝4,171.5秒（103・40）＋docsとconfigだけ13×28.5秒＝370.5秒（13・10）、計26,085秒。docsだけ（負）・configだけ（2件）は算出不能。active_secs: runtimeを含む546×44秒＝24,024秒（546・67）＋その他108×40.5秒＝4,374秒（108・12）、計28,398秒、残りは算出不能。並列のため実際の壁時計の短縮はreceipt-evidenceが最も遅いagentのときだけ | 高い。標本20件のうち13件はreceipt-evidenceの項目でしか見つからない指摘が行き先を決めた（うち12件がstress）。25件に当てると約16件（分母697の約2.3%）。親はP3でlocal-checks.mdを読めるが、明示の検査が無くなる | 42/773（5.4%） | goal 154（agentごとのjob）の後なら、agentごとのトークンと時間が記録に残り、単体の費用を測ってから決められる。その前に決める根拠は弱い |
| E 機械で検査できる項目をgoal 153のprogramのreviewに移す | 算出不能（理由: 外れる試みは0で、programのreviewのトークンは0だが、receipt-evidenceから項目を減らした分の減りは記録に無く出せない） | 算出不能（理由: 同じ。programの時間は加わる） | 中。移せる候補はA-071・A-077（stressの対象の列挙と記載の有無）・A-063・A-062・A-067・A-058のfmt・A-219のlayer-deps。標本の13件のうち12件のstressの指摘は文字列の照合で拾える見込みだが、言い換えや「理由つきで省いた」を誤って落とす危険がある。receiptを読む検査はprogramにreceiptを渡す決定が要る（ADR-t1895-2決定2は検査の対象をrun worktreeとする） | 0/773（項目を全て移してreceipt-evidenceを廃すならDと同じ42） | goal 153（1900・1901・1919でprogramのreviewの段がつながる）の着地の後。receiptを渡す決定（ADR）が先に要る |

## 7. 推奨

- **推奨: BとCを組み合わせて先に行い、Eはgoal 153の後、Dはgoal 154の後に測ってから決める。確信度はlow。**
- 理由:
  - 記録から分かる見逃しは、Bで外れる120試みのうちreceipt-evidenceだけが行き先を重くし、かつその指摘がreceipt-evidenceの項目でしか見つからない試みが0件（該当の候補はebe22e74#4の1件で、4の判定は親でも見つかる）、Cで親に寄せるA-257の決め手の指摘は標本の3件とも親も挙げていた。
  - Bは30試み（3.9%）をCodexに戻し、docsとconfigだけの差分のreviewのsubagentを減らす。
  - Dは標本の13/20がreceipt-evidenceの項目でしか見つからない指摘で、見逃しの危険が高い。単体の費用もgoal 154の前は測れない。
- 確信度がlowの理由:
  - Bのトークンの代理値は算出不能で、費用の効きの大きさを示せていない。
  - 4の判定は読み手の判断で、標本は20件である。
  - Bの外れる試みは窓の中で120件と少ない。
- **Bを選ばずCだけにする場合**: docsだけの測定のtaskの根拠の書き方の検査はreceipt-evidenceに残る。Bだけにする場合は、docsだけの差分でA-257の検査がreceipt-evidenceから外れても、親のP1が受け入れ条件を見る。

### 人が選んだ後に要る変更のtaskの概要

- **B**: `dagq.toml`の`[review.subagents.receipt-evidence] paths`を上のglobの列に変え、同じfileの`[review.subagents.design-consistency]`の前のcommentの「docs/development と残りは receipt-evidence と worker の文書の照合が見る」を直す。request 80のpathsの修正（2106・2107）の着地の後にする。
- **C**: `.dagq/agents/receipt-evidence/AGENT.md`のA-257の行を外す。親は`REVIEW_RULES`とP1で`local-checks.md`の「受け入れ条件の対応づけ」を読むので、promptとruntimeは変えない。
- **E**: goal 153の着地の後に、(1) programのreviewにreceiptを渡すかを決めるADR（ADR-t1895-2決定2の範囲の追加）、(2) 変えた`#[test]`の名前がreceiptの`tests`のevidenceにあるかを見るscript（`scripts/`）と`dagq.toml`のprogramの一覧への追加、(3) `.dagq/agents/receipt-evidence/AGENT.md`から移した項目（A-071・A-077など）を外す。
- **D**: goal 154の着地の後、agentごとのjobの記録でreceipt-evidence単体のトークンと時間を1週測り、その値でこの比較をやり直してから、`dagq.toml`の`[review.subagents.receipt-evidence]`と`.dagq/agents/receipt-evidence/`を消すかを決める。
