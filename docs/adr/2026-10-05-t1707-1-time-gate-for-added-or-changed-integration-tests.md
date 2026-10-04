---
id: adr-t1707-1
type: adr
title: 差分で足した・変えたtests/itのtestの1本の時間を閾値と比べ、超えたものは許可の一覧に理由が無ければ落とす関門を、CIとworkerの手元とreviewに置く
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
owners:
  - hisamekms
tags:
  - testing
  - performance
  - ci
related:
  - adr-t1410-1
  - adr-t963-1
  - adr-t598-1
  - adr-0076
  - design-slow-tests
  - development-testing
---

# ADR-t1707-1: 差分で足した・変えたtests/itのtestの1本の時間を閾値と比べ、超えたものは許可の一覧に理由が無ければ落とす関門を、CIとworkerの手元とreviewに置く

## Context

本番の`integrate`の関門の直近10本（2026-10-04）で、testの時間の合計の95%はtests/itのintegration test（`dagq::it`）だった。goal 68の移し替えは変えたmoduleで時間を縮めたが、同じ時期に足されたintegration testがそれを上回り、全体は伸びた。新しい機能のtestがcaseをloopしてcaseごとにsupervisorを起動し直す形で足され続け、移し替えの後のmoduleも新しい機能のtestで増え直した（例: あるmoduleが200秒を超えた）。

[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)は、判断はunit test、tests/itは境界の代表に絞る規則を決めたが、その規則とreviewの判断だけでは増え直しが止まっていない。規則に反した形を、人や読み手の注意に頼らず機械的に見つける場所が無い。この決定はADR-t1410-1の決定を変えず、それを守らせる関門を足す。

## Decision

1. **関門の方針**: tests/itのtestのうち、base（着地の前ならrunのbase、着地の後ならpushの前のcommit、pull requestならそのbase）からの差分で足したものと本文の行が変わったものについて、その1本の時間を閾値と比べる。閾値は本番の関門の1本の平均を基準に、その数倍の帯に置く。超えたtestは、許可の一覧に理由つきで載っていなければ関門を落とす。既存の遅いtestは導入のときに許可の一覧に載せ、関門は新しい増え直しだけを捕まえる。

2. **許可の一覧に載せてよい理由**: 次のどちらかに当たるものだけ。(a) そのtestが守る境界を書けるもの: SQLite・Git・プロセス・supervisorの配線・復旧とadoptのどれか。ADR-t1410-1がtests/itで確かめる境界のうちcmuxはここに挙げない（cmuxはinboxだけに残り、inboxへのcmuxの送り出しはsupervisorの配線として書く）。(b) 移し替えの予定があり、その行き先のtask（かgoal）を書けるもの。判断のcaseを並べただけのtestは(a)に当たらない。項目は登録したtaskを持ち、testを直すか移すtaskは同じ変更で自分の項目を外すか直す。

3. **関門の配置**: 3か所に置く。(1) CIのpushとpull_requestで、coverageの関門と同じnextestの出力を読む。(2) workerの手元で、変えたtestを流したnextestの出力に当ててreceiptの前に確かめる。(3) runのreviewのtestの規則を見るsubagentの検査項目で、許可の一覧に足した項目の理由が決定2に当たるかを見る。

4. **落ちたときの扱い**: CIのmainへのpushで落ちれば、既存の仕組みでci-failureのissueが開き、plannerが直すtaskを作る。workerは、関門に当たったtestを直すか（判断をunit testへ移す・caseごとの起動をやめる）、決定2に当たる理由で許可の一覧に項目を足す。どちらもできなければ`failed`のreceiptに理由を書く。

5. **integrateに段を足さない**: `integrate`の検証に関門の段を足さない。着地は直列で、全部のtestを流す検証にもう1段を足すと1本ずつの着地の上限が下がる（[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定4がe2eをintegrateで流さないのと同じ考え）。着地の前はworkerの手元とreview、着地の後はCIで捕まえる。

6. **限界**: (1) helperやfixtureだけの変更（`#[test]`の付かない関数、tests/common、stubのscript）は、それを使うtestを遅くしても対象にしない。差分から対象を機械的に決められるのはtestの本文だけだから。(2) CIのrunnerと本番の関門では並列度とmachineが違い、同じtestの秒が違う。CIの秒で落ちた・通ったことは本番の秒と一致しない。(3) workerの手元の秒もhostの負荷で揺れる。これらは日次の見直しで`dagq::it`の本数と合計を追うこと（goal 118）で補う。

7. **変える手順**: 閾値と許可の理由の条件を変えるのは、日次の見直しや本番の関門のlogで、関門が増え直しを捕まえていない・正当なtestを止めすぎていると分かったとき。plannerかinboxが本番の関門の直近のlogの1本の平均と分布（遅いtestの集計）を見て提案し、閾値の帯の基準（本番の1本の平均の数倍）か許可の理由の条件を変えるなら、人が決めて新しいADRで変える（置き換えかamendsかは`docs/development/documents.md`の「ADR」）。基準の中で値だけを直すなら、`docs/design/slow-tests.md`の関門の節の値と決め方を直すtaskにする。

## Alternatives

- **`integrate`に関門の段を足す**: 本番の着地を最も確実に守るが、直列の着地の上限を下げる（決定5）。採らない。
- **kpiや`stats`に記録して見るだけ**: 増え直しが見えるのは着地の後で、誰かが読むまで止まらない。追跡はgoal 118の日次の見直しが行い、関門は止める役に分ける。
- **moduleやtest binary全体の時間の合計で比べる**: 既存の遅いtestと新しいtestを分けられず、移し替えで縮んだ分が増え直しを隠す。1本ずつにする。
- **reviewの判断だけに任せる**: 今の形で止まっていない（Context）。
- **既存の遅いtestも全部落とす**: 移し替えの前に全部の変更が止まる。既存のものは許可の一覧に載せ、移し替えのgoalが外す。

## Consequences

- 新しく足したtestと本文を変えたtestが閾値を超えると、着地の前にworkerとreviewが、着地の後にCIが気づく。既存の遅いtestを直すだけの変更は、その項目があるので落ちない。
- 許可の一覧は、移し替えが終わるまでの遅いtestの台帳を兼ね、項目の理由がその行き先を示す。
- helperだけの変更による遅れは捕まえず、CIと本番の秒の違いは残る（決定6）。日次の見直しが合計で補う。
- 閾値・許可の一覧の書式と置き場所・scriptの名前と引数・CIのstepは`docs/design/slow-tests.md`が持ち、規則の本文は`docs/development/testing.md`の「判断と境界のtest」が持つ。
