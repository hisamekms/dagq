---
id: adr-t4948-1
type: adr
title: 差分で足した・変えたsrcの#[test]の1本の時間を着地の検証のunitの段のログで5秒と比べ、許可の一覧に理由が無ければ落とし、--libの予算は毎晩mainで見る（ADR-t1707-1決定1・2・3・5・6を--libについてamends）
status: accepted
created: 2026-11-16
updated: 2026-11-16
accepted_on: 2026-11-16
amends:
  - adr-t1707-1 decision 1
  - adr-t1707-1 decision 2
  - adr-t1707-1 decision 3
  - adr-t1707-1 decision 5
  - adr-t1707-1 decision 6
amended_by:
  - adr-t5020-1
owners:
  - hisamekms
tags:
  - testing
  - performance
  - integrate
  - ci
related:
  - adr-t1707-1
  - adr-t1410-1
  - adr-t2895-1
  - adr-t3841-1
  - adr-t4979-1
  - development-testing
---

# ADR-t4948-1: 差分で足した・変えたsrcの#[test]の1本の時間を着地の検証のログで閾値と比べ、--libの予算を毎晩見る

## Context

[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)の時間の関門とITの予算は`dagq::it`だけを対象にし、`--lib`のtest（unit test・storeの契約・プロセスの中の配線のtest）にはどの関門も無い。
goal 271が足した30・3000単位のtestは本番の着地の検証のunitの段で1本190〜420秒かかったのに素通りで入り、どのrunの着地の検証も毎回延ばし、そのgoalのworkerは手元で流し直してreviseのturnの上限に当たった。
runの着地の検証のログ（run 0149e149のunitの段）では、`--lib`の5,591本の中央値は0.03秒、p90は0.26秒、p99は30秒で、5秒を超えるものは159本だった。
人は、時間の関門を`--lib`に広げること（着地の検証のログで閾値を超えたら落ち、理由つきで許可の一覧に載せたものだけ通す）と、後で`--lib`にも予算を置いて毎晩のfindingで拾うことを決めた（request 315）。

amendsにするのは、ADR-t1707-1の決定のうち決定1・2・3・5・6を`--lib`のtestについて足し・変え、`tests/it`の関門（決定1〜7）はそのまま残るため（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
既存の遅いunit testを減らすこと自体はこの決定の外で、request 312のtaskが持つ。

## Decision

1. **対象（ADR-t1707-1決定1を--libについてamends）**: baseからの差分で足した`#[test]`と本文の行が変わった`#[test]`のうち、`src/`の`#[cfg(test)]`の中のもの（dagqのlibとbinのunit test。nextestのbinaryは`dagq`と`dagq::bin/dagq`）を対象にする。testの名前はnextestの全名（crateのrootからのmoduleのpathとfnの名前）で照合し、fnの名前だけでは照合しない。`dagq::it`のtestはこの関門では読まず、ADR-t1707-1の関門が見る。

2. **閾値**: 本番の着地の検証のunitの段のログの1本の秒で5秒とし、等しいものは超えない。Contextの分布で5秒は中央値の百倍を超え、p90とp99の間にあって、ふつうのunit testを止めずに重いものだけを捕まえる。[testの制約](../development/testing.md)のプロセスの中の配線のtestの目安（1本およそ3秒、5秒を超えたら分ける）と、`tests/it`の関門の5秒に揃える。

3. **許可の一覧と載せてよい理由（ADR-t1707-1決定2を--libについてamends）**: 一覧は`tests/it`とは別に置き、新しい項目は足すtaskの専用のファイルに足す（別々のtaskの追加が同じファイルに触れないため）。書式は`tests/it`の一覧と同じにする。載せてよいのは次のどちらかに当たるものだけ。(a) そのtestが守る境界を書けるもの: SQLiteのstoreの契約・adapterなど、unitの層で実物を使う理由。(b) 縮める・移す予定のtaskかgoalを書けるもの。判断のcaseを並べただけのtestと、件数を大きくしただけのtestは当たらない。項目は登録したtaskを持ち、testを直す・消す・名前を変えるtaskは同じ変更で自分の項目を外すか直す。

4. **既存の遅いtestを一覧に載せない（ADR-t1707-1決定1の導入の扱いを--libについて変える）**: 導入のときに既存の遅いtestを一覧へ載せない。差分で触れたtestだけが対象なので、触れたtaskが直すか理由を載せる。request 312のtaskが縮めるgoal 271のtestもこれに当たる。全部に理由を書く手間を避け、載せた項目が触れても素通りになる穴を作らないため。

5. **関門の場所（ADR-t1707-1決定3・5を--libについてamends）**: 4か所に置く。(1) 着地の検証: unitの段が出したログを読むだけで、testを流し増やさない。ADR-t1707-1決定5が`integrate`に段を足さないとした理由はtestを流し増やすことの時間で、ログを読むだけの検査には当たらないので、`--lib`については着地にも置く。着地で落ちたときは、hostの分類ではなくcodeの失敗としてworkerに返る。(2) CIのmacOSのjob。(3) workerの手元（local_checksのgate）。(4) runのreviewのtestの規則の検査で、足した項目の理由が決定3に当たるかを見る。

6. **--libの予算の方針**: 毎晩mainで`--lib`の本数・秒の合計・秒の上位20本の和を、repositoryの設定のファイルに置いた天井と比べ、超えれば夜のjobを落として既存のCIの失敗の経路でfindingにする。着地は止めない。1本ずつの関門が捕まえない既存のtestの増え方とhelperによる遅れを、合計で拾うため。

7. **限界（ADR-t1707-1決定6を--libについてamends）**: (1) helperやfixtureだけの変更は、それを使うtestを遅くしても対象にしない。(2) macroが展開するtestと、どの`mod`の宣言も辿らないファイルは見えない。(3) CIのrunner・workerのhost・本番の着地の検証では同じtestの秒が違う。着地の検証の秒が関門の正で、CIと手元の秒は早く気づくためのもの。これらは決定6の予算で補う。

## Alternatives

- **既存の遅いtestを導入のときに全部一覧へ載せる（ADR-t1707-1と同じ）**: 159本に理由を書く手間が要り、載せたtestは触れても素通りになる。決定4のとおり採らない。
- **着地の検証に置かずCIと手元とreviewだけにする（ADR-t1707-1決定5のまま）**: 本番の秒で捕まえられず、重いtestが着地してから気づく。ログを読むだけなら着地の時間は増えないので採らない。
- **閾値を1秒や30秒にする**: 1秒はp90の4倍ほどで、ふつうの配線のtestまで止める。30秒はp99で、goal 271の形を止めきれない。
- **予算で着地を止める**: 既存のtestの合計で関係の無いrunの着地が止まる。予算は夜の見直しの役に分ける。

## Consequences

- 重いunit testを足す・変えるrunは、着地の前に手元・review・着地の検証のどこかで名前と秒を返され、直すか理由つきの項目を足す。
- 既存の遅いtestは、触れたtaskが直すか項目を足すまで一覧に現れない。全体の重さは予算が夜に見る。
- scriptの名前と引数・一覧のpath・名前の求め方は`scripts/check-lib-test-time.sh`の冒頭のコメントが、規則の本文は`docs/development/testing.md`の「判断と境界のtest」が持つ。
