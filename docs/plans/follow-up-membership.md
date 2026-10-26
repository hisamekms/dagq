---
id: plan-follow-up-membership
type: plan
title: follow-up の所属判断の評価指標と導入前の基準値・導入後の評価
status: active
created: 2026-10-05
owners:
  - hisamekms
tags:
  - measurement
  - follow-up
related:
  - adr-t1504-1
  - adr-t1504-2
  - design-follow-up-membership
  - design-measurement
---

# follow-up の所属判断の評価

[ADR-t1504-1](../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)の評価。所属・採用・priorityは別の判断として数える。導入後の評価は本書の同じ定義と基準期間を使い、下の「導入後の本評価」に置く。ここでは既存のgoalを閉じたり、acceptance・所属を変更したりしない。

## 期間と母集団

基準期間は **2026-09-22T00:00:00Z 以上、2026-10-04T00:00:00Z 未満**（UTC、12日）。ADR承認日の前に切り、仕組みの前の挙動を対象にする。この期間のeventには`follow_up_judged`は無い。母集団は登録時に`draft_origins.origin = follow_up`となったdraft。`follow_up_registered.payload.task_id`が非nullの920件を使用する。登録eventは元taskの`task_id`を持ち、draftのIDは**payload**にある。nullのskipped、`goal_gap`・`reopened`として生まれたdraft、通常の人の登録は含めない。所属を移したものも登録時の母集団から落とさない。

登録とoriginの対応は`register_follow_ups`と登録のtransaction（`src/infrastructure/draft_planners.rs`）が持つ。古い登録もeventから復元でき、現在のDBの行の作成時刻（migration時刻の場合がある）は開始時刻に使わない。計測の証拠はイベントIDとsource runのIDで保持する。以降のtask状態を一覧として管理するための文書ではない。

集計の打切りは上のuntil。承認・submit・verdictがまだ無いものは右打切りとして件数を別掲し、0秒・passにはしない。期間内登録のうち期間外承認を後から混ぜない。登録前の過去のreviewや、初回ready後の再審査はこの集計に含めない。後のoriginの変化でも初回登録の評価対象は変えない。

## 指標の定義と式

1. **分類の初回pass率**: 所属判断が必要なfollow-upで、初回submit時の判断と元goalのacceptance版を持ち、初回の完了したplan reviewが`pass`だったtask数 / 初回verdictを観測した同じtask数。分類の判断を持たない旧方式では測れない。旧方式の比較用に「計画全体の初回pass率」を別に出す。verdictはproposal単位なので、束の各taskに同じverdictが付くproxyであり、個別分類が正しい確率ではない。後でも分類の指摘によるreviseと、paths・verifyなど他の理由のreviseを理由と証拠を読んで分ける。task加重に加え、proposalを1回だけ数える率を併記して束の大きさの偏りを見る。
2. **採用から承認までの総時間**: `t_adopt`は初回の`follow_up_adopted`（採用の判断を永続化した時刻）、`t_ready`は初回の`task_status_changed.to = ready`。taskごとに`T = t_ready - t_adopt`、観測できたtaskの`ΣT`・中央値・p90を出す。これは壁時計の時間で、人への問い・待ち・修正を含む。思考の中で採用を決めた瞬間は記録されない。古い人のbypass等でadoptが無いtask、初回readyより後のadoptしか無いtaskは欠測とする（負の時間を0に丸めない）。未承認・cancel・withdrawを成功時間の分母から除き、別途件数と経路を示す。
3. **登録からの時間**: `t_reg`は`follow_up_registered.created_at`。初回の`task_submitted`、初回verdict、初回readyについて`Σ(t_end - t_reg)`・中央値・p90を出す。採用以前の放置が短い承認時間に隠れないようにする。
4. **reviseの周回**: 初回submitから初回readyまで、taskが入った全proposalの`plan_review_finished.verdict = revise`をevent IDで重複を除いて数える。同じproposalのresubmit、withdraw後の別proposalも追う。`revise`以外の失敗・concern・未完了をreviseに置き換えない。承認taskの合計・中央値・p90・0/1/2…周の分布を出し、未承認taskの既観測周回も別に保存する。`ready`はverdictの直前に書かれる場合があるので、その時点で開始済みの同じplan reviewの完了も含める（`plan_review_id`で開始と完了を結ぶ）。
5. **plannerの時間**: 対象taskの`draft_planner_opened.planner_id`、対象taskまたはそのproposalに紐づく（revise用を含む）`session_opened.kind = runtime_planner`と、`session_closed.opened_event_id`を結んだ開閉の秒。複数taskを処理する束でもsessionを1回だけ数え、合計・中央値・p90・未閉鎖数を出す。これはsessionが開いていた壁時計で、active時間や採用から承認までの時間と同一ではない。旧方式の長く開いたsessionの待ちも含む。`stats.sessions.by_kind.runtime_planner`は全originの補助値であり、follow-upの母集団への絞り込みはeventsで行う。
6. **採用から承認までの総token**: Execution（起動1回）の重複を除き、採用〜承認の当該plannerとplan review・reviseのExecutionを合計する。入力・cache read・cache creation・出力をprovider別に保持し、`total = input + cache_read + cache_creation + output`とする（cacheを含む処理量で、課金額ではない）。採用境界を跨ぐExecutionは全量を「境界を跨ぐ量」と別掲し、按分しない。bundleで複数taskを扱うExecutionは束に1回だけ加算し、個別taskに全量を複製しない。観測率（tokenを持つExecution数 / 対象Execution数）を必ず出し、未記録を0にしない。
7. **plannerの事前判断のtoken増分と差し戻しの減り**: 初回submitまでの、対象draftを扱うplannerのExecution（採用前の調査・所属判断を含む）の合計 / 初回submitしたfollow-up数を`P`とする。束の全量を1回だけ数える。`ΔP = P_after - P_before`、`ΔR = revise_after / approved_after - revise_before / approved_before`、さらに(6)の総token / 承認数の前後差を併記する。`ΔR < 0`だけでは得とせず、事前調査の増分・総時間・誤分類と合わせて判断する。初回submit後のrevise用plannerは事前判断に混ぜない。
8. **承認後の誤分類**: 初回ready後30日以内に、当時のacceptanceに照らし分類または所属先が誤っていたと証拠付きで訂正されたtaskのdistinct数 / 30日観測できた承認task数。訂正event数も別掲する。同じtaskの訂正を重複加算せず、単なるacceptance版の更新と確かめ直しは含めない。`follow_up_judged`の`corrects`・理由・証拠、achieved後の訂正ask（`correct_goal`）と回答を読む。goalが閉じた後の誤分類は別の内数として出す。未訂正だから誤り無しとは推論しない。30日未満のtaskは観測日数付きの暫定値とし、旧方式は記録が無いため未計測。
9. **残件の由来**: cutoff時点で`status = open`かつ未closedのgoalを数える。残taskは`completed` / `canceled`以外。残件が1件のgoal、残件が1件以上あり**全て登録時follow_up由来**のgoal、その両方の交差を数える。空goalは「全てfollow_up」には含めない。この数はacceptanceとの関係を分類した数でも、閉じてよいgoal数でもない。

## 導入前の基準値

秒のp90は昇順の`ceil(0.9 × n)`番目（補間なし）。時間の合計はtaskごとの待ちが並行して重なるため、期間の長さと比較しない。

| 指標 | 観測数・分母 | 前の値 |
| --- | --- | --- |
| 分類の初回pass率 | 分類の記録なし | 未計測 |
| 計画全体の初回pass率（proxy） | 477 verdict、443未観測 / 920登録 | 400/477 = 83.86% |
| 登録→初回submit | 477（443未観測） | 合計4,629,569.136秒、中央値358.224秒、p90 33,666.781秒 |
| 登録→初回verdict | 477（443未観測） | 合計4,864,252.466秒、中央値703.791秒、p90 35,316.197秒 |
| 登録→初回ready | 486（434未観測） | 合計5,136,550.070秒、中央値933.466秒、p90 36,146.771秒 |
| 採用→初回ready | 474（ready 486件のうち11件はadopt無し、1件は初回ready後） | 合計265,693.865秒、中央値37.697秒、p90 461.366秒 |
| 承認taskのrevise | 486承認 | 計74周、中央値0、p90 1。0周418件、1周62件、2周6件 |
| follow-upのruntime planner session | 514閉鎖、未閉鎖0 | 合計362,896.105秒、中央値58.1055秒、p90 236.644秒 |
| 採用→承認の総token、事前判断token/submit、増分 | 個別Executionへの帰属が未対応 | 未計測（下節） |
| 承認後の誤分類 | 分類・訂正の記録なし、30日追跡前 | 未計測（0件とはしない） |
| open goal | cutoff時点、過去へ復元 | 66件 |
| 残り1 taskのopen goal | 同上 | 20件 |
| 残件が全てfollow_up由来のopen goal | 空goalを除く | 32件 |
| 残り1 taskかつfollow_up由来 | 同上 | 13件 |

2026-10-03の「open 65件、残り1件13件」は時刻が固定されていない既知の観測で、本測定のcutoffとは違う。今回の13件は**残り1件かつfollow_up由来**の交差であり、20件という全由来の残り1件と取り違えない。旧観測の13件と同じ集合だとも言わない。今回の交差にgoal 20/task 792、21/787、33/436は含まれる。

## tokenの準備と限界

採取時点（2026-10-05）に`dagq goal show 95 --full`でExecution計測の実装・集約が未着地であることを確認した。`kpi`と`stats`には旧sessionのtokenはあるが、Codex jobやClaude subagent、日を跨ぐ対話の欠けがあるため(6)(7)の値として使わない。task 1485の`scripts/token-usage.py`は存在し、実行した。出力のactor・日別の量は次の補助基準になる。

このscriptは**JST日**を取り、2026-09-22〜2026-10-03 inclusive（UTCでは09-21 15:00〜10-03 15:00）である。上のUTC基準期間と9時間ずれるため、分母920件や承認486件で割らない。

| actor / provider（全origin） | input | cache read | cache creation | output | 合計 |
| --- | ---: | ---: | ---: | ---: | ---: |
| runtime_planner / Claude | 15,766 | 607,399,494 | 49,515,177 | 3,610,813 | 660,541,250 |
| plan_review / Claude | 3,222 | 229,869,781 | 104,766,658 | 1,062,969 | 335,702,630 |
| plan_review / Codex | 67,458,039 | 255,299,328 | 0 | 941,278 | 323,698,645 |

暫定scriptはactor・日で出し、task/proposal/採用境界へ帰属する出力を持たない。したがって個別follow-upの採用〜承認の総tokenと事前判断のtokenは依然未計測。加えて古いCodex rollout 17件はfallbackで、transcript削除やpromptからのactor推定も制約になる。全actorの合計をfollow-upの総tokenと呼ばない。

本評価ではgoal 95のExecutionの記録を`kpi / stats`で読み、eventsのplanner_id・proposal_id・plan_review_id・起動回と結ぶ。恒久の記録の欄名はその着地時のdesignを使う。旧期間を恒久方式で遡及して0埋めしない。比較可能なExecutionの記録が無ければ、旧transcriptをIDで結ぶ専用の帰属処理とcoverageの検証を先に行うか、増分は未計測のまま扱う。導入後だけ恒久計測が使える場合は、その期間の絶対値と(1)〜(5)(8)(9)の比較を報告し、tokenの前後差を算出できたとは書かない。

## 再計算と証拠

repositoryの任意のworktreeで、固定バイナリが解決することを確認して実行する。全て読み取り専用。workerは`dagq list` / `dagq show`やqueueへのSQLを使わない。

```sh
which dagq
python3 scripts/follow-up-membership.py --collect "$TMPDIR/membership" \
  --since 2026-09-22T00:00:00Z --until 2026-10-04T00:00:00Z \
  > "$TMPDIR/membership-result.json"
python3 scripts/follow-up-membership.py --input "$TMPDIR/membership/inputs.json" \
  --since 2026-09-22T00:00:00Z --until 2026-10-04T00:00:00Z \
  > "$TMPDIR/membership-recomputed.json"
PYTHONDONTWRITEBYTECODE=1 python3 scripts/follow-up-membership-test.py
```

collectorは`~/.local/bin/dagq goal list`と各`goal show ID --full`を保存し、`events --all --full --limit 1000 --after CURSOR --until SNAPSHOT_END`を空pageまで読む。analysisはCLIを呼ばず、保存したJSONのみから再計算する。各goalの読み取り時刻までのeventで現在の所属・状態・goalのopen/closedをcutoffへ巻き戻す（`task_created`・`task_status_changed`・`task_goal_changed`・`goal_created`・`goal_status_changed`・`goal_closed`）。originは登録eventから復元する。snapshotはtransactionではないので、各goalの読み取りの間の状態変更を`snapshot_race_events`で示す。非空ならgoalの基準値は確定させず再採取する。今回は空。untilはsnapshot_started以前に限る。後でgoalの再開等の新しい遷移が加わった期間へ使うときは、巻戻し処理をその遷移に対応させてから数える。

補助のコマンド:

```sh
dagq stats --full --since 2026-09-22T00:00:00Z --until 2026-10-04T00:00:00Z
dagq kpi --since 2026-09-22T00:00:00Z --until 2026-10-04T00:00:00Z
dagq events --full --run SOURCE_RUN --kind follow_up_registered
dagq timeline SOURCE_RUN --full
dagq goal show 95 --full
python3 scripts/token-usage.py --since 2026-09-22 --until 2026-10-03 --format json
```

`stats / kpi`は母集団を確認する補助ビュー、`timeline`はsource runの登録の前後を調べるときに使う（runのないplannerの時間をtimelineから推定しない）。計算の正本はevents。証拠は[summary.json](follow-up-membership-evidence/summary.json)、[tasks.csv](follow-up-membership-evidence/tasks.csv)（登録・adopt・submit・ready・reviewのevent ID、各時間と周回）、[planner-sessions.csv](follow-up-membership-evidence/planner-sessions.csv)（開閉ID）。JSONのrowsとplanner_sessionsがCSVに対応する。

採取は2026-10-05T10:23:09.679Z〜10:23:17.910Z。全event 104,090件、最終cursor 104090。入力SHA-256は`6e00ef92ee71faff839995586664f104d95a339826ef71808588576a93de4386`。保存した材料はqueue dirの`runs/b92c1830-ac22-492c-8f43-1920ccc876c8/membership-evidence/inputs.json`、補助出力は同directoryの`stats.json`・`kpi.json`・`tokens.json`。source task 107のtimelineは`timeline-source-107.json`。これは再生成可能な採取材料で、queueの状態の正本ではない。run資産の掃除で消える場合も、残るeventから再採取し、CSVのevent IDで比較できる。snapshotのハッシュは採取時刻が違えば変わる。

区分: 正本はEventStore（events）、証拠のCSVとJSONはeventsとgoalのsnapshotから作り直せるビュー、run dirの`membership-evidence/`は消えてよい採取材料。旧データのgoal残件をStateStoreのCLI snapshotからeventで巻き戻すのはこの評価だけの暫定の分析で、台帳の読み取り経路には入れない。

## 導入後の本評価

### 切替点と期間

切替点はeventsのbuild/handoffと着地で決めた。runtimeの部分（task 1505〜1509）の最後の着地はevent 82625（2026-10-04T06:20:58Z）で、それを含む固定バイナリでsupervisorが動き出したのはevent 82854（06:54:22Z、自動更新）。pluginの手順（task 1510）の着地はevent 91217（11:33:07Z）で、それを含む固定バイナリはevent 91607（12:18:30Z）。最初の`follow_up_judged`はevent 82789（06:51:33Z）。切替後の最初のUTC日から12日間の登録cohortを主表にする: **2026-10-05T00:00:00Z 以上、2026-10-17T00:00:00Z 未満**。cohortの所属判断の記録は`source_kind = recorded`だけで、移行で復元した`restored`の判定は無い。

goal 95のExecutionの記録（`tokens_source`を持つ`turn_finished`とplan reviewのjobの終わり）は、runtime plannerではevent 131984（2026-10-08T01:43:46Z）から、plan reviewではevent 132355（02:27:39Z）から始まる。tokenは記録が全期間にあるcohort **2026-10-09T00:00:00Z 以上、2026-10-17T00:00:00Z 未満**（8日、以下「token cohort」）で数える。

### 数えたコマンド

```sh
which dagq
python3 scripts/follow-up-membership.py --collect "$TMPDIR/membership" \
  --since 2026-10-05T00:00:00Z --until 2026-10-17T00:00:00Z > "$TMPDIR/result-after.json"
for w in 2026-09-22T00:00:00Z,2026-10-04T00:00:00Z,before \
         2026-10-09T00:00:00Z,2026-10-17T00:00:00Z,after-tokens \
         2026-10-26T00:00:00Z,2026-10-26T12:53:14.812Z,now; do
  IFS=, read since until name <<< "$w"
  python3 scripts/follow-up-membership.py --input "$TMPDIR/membership/inputs.json" \
    --since "$since" --until "$until" > "$TMPDIR/result-$name.json"
done
PYTHONDONTWRITEBYTECODE=1 python3 scripts/follow-up-membership-test.py
```

`before`は同じ入力から基準値を数え直したもので、上の表の値（920件、400/477、74周、goal 66・20・32・13）と一致した。`now`はcohortには使わず、採取の時点のgoalの残件を数えるためだけに使う。script の`evaluation`は(1)(4)(6)(7)(8)の記録を読む部分。

採取は2026-10-26T12:53:14.812Z〜12:53:21.008Z、全event 527,404件、入力SHA-256は`c67b0e3daf801bc1d24443ee1c343d992835b4d907bc76601a2c02677541cce4`、`snapshot_race_events`は空。入力（約540MB）は保存していない。4つの窓の出力はqueue dirの`runs/9ae33980-215a-4d9b-a51e-52731571b6a6/membership-evidence-after/`に置いた（消えてよい採取材料）。証拠は[after/summary.json](follow-up-membership-evidence/after/summary.json)（cohort・token cohort・採取時点のgoal）、[after/tasks.csv](follow-up-membership-evidence/after/tasks.csv)、[after/planner-sessions.csv](follow-up-membership-evidence/after/planner-sessions.csv)。区分は「再計算と証拠」と同じで、eventsから作り直せるビュー。

### 比較の表

cohortは446件（初回verdictを観測239件、207件は未観測、初回ready 236件）。未観測207件は、runtime plannerの`draft_planner_settled`でduplicateのcancel 127件、cancel 75件、keep_draft 15件、answer待ち1件（同じtaskが2度settledになるものを含む）。

| 指標 | 導入前（09-22〜10-04） | 導入後（10-05〜10-17） |
| --- | --- | --- |
| 分類の初回pass率（submit前に所属判断を持つtask） | 未計測 | 84/231 = 36.36% |
| 計画全体の初回pass率（task加重、proxy） | 400/477 = 83.86% | 87/239 = 36.40% |
| 計画全体の初回pass率（proposalを1回） | 347/402 = 86.32% | 71/176 = 40.34% |
| 初回reviewが所属を名指さなかった率（task加重 / proposal） | 未計測 | 229/239 = 95.82% / 168/176 = 95.45% |
| 登録→初回submit | 中央値358.224秒、p90 33,666.781秒（n 477） | 中央値130.002秒、p90 802.387秒、合計560,989.232秒（n 239） |
| 登録→初回verdict | 中央値703.791秒、p90 35,316.197秒（n 477） | 中央値428.073秒、p90 1,905.784秒、合計658,474.373秒（n 239） |
| 登録→初回ready | 中央値933.466秒、p90 36,146.771秒（n 486） | 中央値629.859秒、p90 4,350.511秒、合計844,825.792秒（n 236） |
| 採用→初回ready | 中央値37.697秒、p90 461.366秒（n 474） | 中央値458.662秒、p90 2,541.970秒、合計284,273.988秒（n 236） |
| 承認taskのrevise | 計74周 / 486、中央値0、p90 1。0周418・1周62・2周6 | 計142周 / 236、中央値1、p90 1。0周107・1周116・2周13 |
| 承認1件あたりのrevise（R） | 0.152 | 0.602（ΔR = +0.449） |
| follow-upのruntime planner session | 514閉鎖、中央値58.106秒、p90 236.644秒 | 448閉鎖・未閉鎖0、中央値107.251秒、p90 323.725秒、合計144,215.273秒 |
| 承認後30日以内の誤分類 | 未計測（下の参考を除く） | 0件 / 承認236件。30日を観測できたtaskは0件（暫定、最長21日） |
| achieved後の訂正のask（`correct_goal`） | — | 0件（全期間） |

所属を名指すreviseは、初回reviseの理由の文に`所属`・`membership`・`judge-follow-up`・`follow_up_judged`・`out_of_scope`・`範囲外`・`acceptance_items`があるものを選び（10件）、文を読んで8件を所属の誤りと判じた（event 100371・121943・125990・129118・129980・133541・137424・186303。116559と205532は過去の記録への参照とevent名の誤りで、所属ではない）。語を含まない書き方の指摘は拾えないので下限である。

承認前の訂正（`--corrects`）は12件で、分類か所属先を変えたのは8 task。分類の訂正はtask 1831・2104・2155の3件、所属先だけの訂正はtask 2046・2081・2130・2440・2471の5件で、5件とも受け入れ条件に要らない改善を同じテーマのlowの受け皿でなくnormal以上のgoalに置いたもの。残りの4件は項目や根拠だけの訂正。

参考: 導入前のcohortにも承認後30日以内の訂正のeventが4件あり、分類か所属先を変えたのはtask 787・1359・1464の3件（601は項目だけの訂正）。どれもこれはtask 1512の棚卸しが移行で復元した判定（`restored`）を後から直したもので、導入前の誤分類率とは比べない。

(1)の分母から除く例外（元goalの無い判断）は0件で、cohortの判断は全て元goalを持つ。元goalがabandonedかはgoalのsnapshotが区別しないので分けていない。

### 層

初回pass（task加重、通過/観測）。導入前の`不明`はplan reviewの起動の記録にproviderが無かった時期のもの。

| 層 | 導入前 | 導入後 |
| --- | --- | --- |
| plan reviewのprovider: codex | 36/82 = 43.9% | 67/203 = 33.0% |
| plan reviewのprovider: claude | 84/98 = 85.7% | 20/36 = 55.6% |
| plan reviewのprovider: 不明 | 280/297 = 94.3% | — |
| 単独のproposal / 束 | 315/356 = 88.5% / 85/121 = 70.2% | 59/130 = 45.4% / 28/109 = 25.7% |
| 採用: planner / 人 | 333/397 = 83.9% / 67/80 = 83.8% | 81/220 = 36.8% / 6/19 = 31.6% |
| depth 0 / 1 / 2 | 67/80 / 279/333 / 54/64 | 6/19 / 64/185 / 17/35 |
| submit前の所属判断: 有 / 無 | — / 400/477 | 84/231 / 3/8 |

初回reviseのproposalの主な理由（`primary_code`）は、導入前49件のうち`verification_rule` 6件・`task_overlap` 8件・`incomplete_spec` 6件・無し13件、導入後82件のうち`verification_rule` 39件・`incomplete_spec` 15件・`task_overlap` 8件・`wrong_premise` 5件。

### token

token cohortは登録252件、初回submit 125件、承認124件（全て採用の記録を持つ）、初回pass 44/125 = 35.2%。Executionは1回だけ数え、束のplannerとplan reviewは束に1回だけ足す（plan reviewの量は同じproposalのfollow-up以外のtaskの分も含む）。totalはinput・cache read・cache creation・outputの和。

(6)は、各Executionの始まり（turnは`turn_started`、plan reviewは`plan_review_started`）と終わりを、それが扱ったtaskの採用（初回の`follow_up_adopted`）と初回readyの間と比べて区分する。束で複数のtaskを扱うExecutionは、どれかのtaskで「採用→初回ready」に入れば、そこに1回だけ数える。初回readyの直前に始まり直後にverdictを書く承認のplan reviewは「採用→初回ready」に入れる（(4)の数え方と同じ）。

| 区分 | 元 | 観測率 | input | cache read | cache creation | output | 合計 |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 採用→初回ready | planner（Claude） | 60/60 | 1,264 | 28,566,255 | 2,675,790 | 284,455 | 31,527,764 |
| 採用→初回ready | plan review（Codex） | 144/153（2 providerの合計） | 15,257,886 | 92,904,960 | 0 | 239,946 | 108,402,792 |
| 採用→初回ready | plan review（Claude） | 同上 | 202 | 8,992,612 | 3,190,099 | 52,978 | 12,235,891 |
| 採用の境界を跨ぐ | planner（Claude） | 80/80 | 2,154 | 60,415,804 | 4,717,154 | 538,569 | 65,673,681 |
| 採用の前 | planner（Claude） | 6/6 | 164 | 4,236,354 | 326,057 | 41,113 | 4,603,688 |
| 未承認・未採用のtaskだけ | planner（Claude） | 47/47 | 622 | 13,501,230 | 1,970,995 | 113,481 | 15,586,328 |
| 未承認・未採用のtaskだけ | plan review（Codex） | 1/1 | 91,319 | 403,200 | 0 | 1,577 | 496,096 |

- 初回readyを跨ぐExecutionと初回readyの後に始まったExecutionは0件。区分の合計は238,526,240で、cohortに結んだExecutionの全量と一致する。
- 採用→承認の総token / 採用の記録を持つ承認: 152,166,447 / 124 = 1,227,150。plan reviewの9件が未計測なので下限。採用の境界を跨ぐ65,673,681は按分せず別掲する（採用は事前判断のplannerの最初のturnの中で行われることが多いため）。
- 事前判断とreviseの分け方((7))は、事前判断のplannerのturnのうち、そのplannerの最初のsubmitの前に始まったturnを事前判断、後に始まったturn（生きているplannerに指摘を届けたreviseを含む）とrevise用のplannerのturnをreviseに数える。
- 事前判断のtoken / 初回submit（P_after）: 85,556,317 / 125 = 684,451。分子は定義(7)のとおり対象draftを扱ったplannerの全量で、submitしなかった127件（cancel・duplicate・keep_draft）を扱った分も含む。
- 導入前はExecutionの記録が無いので、P_before・ΔP・総token / 承認の前後差は算出していない（「tokenの準備と限界」）。

### 残件の由来

| 時点 | open goal | 残り1 task | 残件が全てfollow_up由来 | 両方 |
| --- | ---: | ---: | ---: | ---: |
| 導入前のcutoff（2026-10-04T00:00Z） | 66 | 20 | 32 | 13 |
| 導入後のcutoff（2026-10-17T00:00Z） | 63 | 28 | 27 | 18 |
| 採取時点（2026-10-26T12:53:14.812Z） | 36 | 18 | 15 | 10 |

採取時点の「全てfollow_up由来」はgoal 29・30・37・56・74・76・106・109・115・120・122・137・142・143・222。どれも閉じてよいgoalの数ではない。

### 件数と結論の範囲

導入後のcohortは446件・初回verdict 239件・承認236件で、(2)〜(5)(9)は前後を比べるに足り、(1)(6)(7)は導入前の値が無いので導入後の絶対値だけを出す。足りないのは2つ: (8)は承認から30日を観測できたtaskが無く暫定で、cohortの全taskが30日に達するのは2026-11-16。tokenの前後差（ΔP、総token / 承認の差）は導入前の記録が無く算出できない。この2つは再評価のfollow_up（measurement）に回し、ここでは結論にしない。

前後の差は仕組みの因果効果と断定しない。同じ期間にplan reviewのproviderがCodex中心に移り（初回verdictの203/239）、`dagq marks --since 2026-10-04T00:00:00Z --until 2026-10-17T00:00:00Z`は固定バイナリでのsupervisorの起動273回と記録した印4件（10-04 headlessのwrapperのbackground化、10-07 着地の検証をunit test全件と絞ったITに、10-11 `list`、10-13 supervisorの`max_spikes 1`・`max_investigations 2`）を挙げる。`kpi --compare`は全originの区間の値で、follow-upのcohortに絞れないので主表に使っていない。登録の規則（[taskの登録](../development/task-registration.md)）もこの間に39回変わっている（`git log`）。

### 続ける点と直す点の案

決めるのは人かplanner。

- 続ける: 所属の判断の記録とsubmitの関門。根拠: submit前に判断を持つtaskが初回verdictの231/239で、初回reviewの理由が所属の誤りを名指したのは8/176 proposal（10 task）。承認前に`--corrects`で分類か所属先を変えたtaskは8件で、関門の前に誤りが直される経路が動いている（この8 taskは上の名指しの8 proposalと別の集合で、全てがplan reviewの指摘によるとは確かめていない）。登録→初回readyのp90は36,147秒から4,351秒に縮んだが、他の変更と重なる。承認後の訂正が0件なのは観測が30日に満たないためで、根拠にしない。
- 直す（所属先）: 承認前の所属の訂正8件のうち5件は、要らない改善を同じテーマのlowの受け皿でなくnormal以上のgoalに置いた誤り。`judge-follow-up`の`out_of_scope`でlowより高い優先度の移動先に受け入れ条件の項目の名指しを求める拒否がcohortの後（commit eed007b6、2026-10-24T18:23Z）に入っており、その後のcohortでこの種の訂正が減るかを再評価で見る。
- 調べる（初回passの落ち込み）: 初回passは83.86%から36.40%に下がった。初回reviseの理由に所属を名指したのは1割で、初回reviseのproposalの`primary_code`は`verification_rule`が82件のうち39件（導入前は49件のうち6件）。仮説は、verifyの推奨の組み合わせが登録の前に当てられていないこと。ただしplan reviewのCodexへの移り（Codexの初回passは導入後33.0%、Claudeは55.6%）と登録の規則の変更が重なり、所属の判断の手順そのものがverifyの漏れを増やしたかも分けていない。ΔR = +0.449と採用→承認の中央値の伸び（37.697秒→458.662秒）を理由ごとに分けた数も無い。分けるには、全てのreviseの周回の理由の内訳とproviderの層の比較が要る。
- 直す（数え方）: 所属を名指すreviseを語で選んでいるので、plan reviewの`reason_codes`に所属の誤りのコードがあれば分けて数えられる。
