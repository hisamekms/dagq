---
id: plan-follow-up-membership
type: plan
title: follow-up の所属判断の評価指標と導入前の基準値
status: active
created: 2026-10-05
updated: 2026-10-05
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

[ADR-t1504-1](../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)の評価の準備。所属・採用・priorityは別の判断として数える。導入後の評価は本書の同じ定義と基準期間を使い、tokenの記録が使える期間で本評価を追記する。ここでは既存のgoalを閉じたり、acceptance・所属を変更したりしない。

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

## 導入後の本評価

固定バイナリで所属判断・submit関門・review資料が有効になった時刻をeventsのbuild/handoffと実装の着地から確認し、ADR承認時刻やworkerのcommit時刻だけを切替点にしない。切替後の最初のUTC日から12日間の登録cohortを同じ式で数え、未承認数・欠測を併記する。30日後に(8)を確定する。分類の判断が必要ない元goal無し・abandonedの例外は(1)から除き、件数を別掲する。

provider・model・effort、depth、人のadoptの有無、単独/束、元goalの状態で層を分ける。期間内の他の変更は`dagq marks`と`kpi --compare A..B,C..D`で列挙する。主表はUTCの同じ窓で、暫定tokenのJST窓を混ぜない。sampleが少ない層は率と分母のみ示し、仕組みの因果効果だとは断定しない。総tokenと時間・差し戻し・誤分類を一緒に見て、継続か改善かの理由を本書に追記する。基準値と本評価を分けたまま、未計測を解消した範囲も明記する。
