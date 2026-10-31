---
id: adr-t3762-1
type: adr
title: 差分がtests/itに足したtestは守る境界の宣言を要し、宣言の無いものはrunのプログラムのreviewとCIの関門で落とす
status: accepted
created: 2026-10-31
updated: 2026-10-31
accepted_on: 2026-10-31
amended_by:
  - adr-t3802-1
owners:
  - hisamekms
tags:
  - testing
  - ci
  - review
related:
  - adr-t1410-1
  - adr-t1707-1
  - development-testing
---

# ADR-t3762-1: 差分がtests/itに足したtestは守る境界の宣言を要し、宣言の無いものはrunのプログラムのreviewとCIの関門で落とす

## Context

tests/itの`#[test]`は2026-10-04の1,143本から2026-10-31の1,956本に増え、着地の検証が全部のITを流すと30分の上限で切られた（goal 237）。
[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)は判断をunit test、tests/itを境界の代表に絞る規則を決め、[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)は足した・変えたITの1本の時間の関門を置いた。
時間の関門は閾値より速いITを素通りさせ、行だけで見分けられる形の検査とtest-rulesのagentのreviewも、判断のcaseを並べた速いITが足されるのを止めなかった。
ITを減らす移し替えのgoalが縮めても、新しいITが同じ形で増え直す。
この決定はADR-t1410-1とADR-t1707-1の決定を変えず、それらに関門を1つ足す。

## Decision

1. **境界の宣言を要する**: baseからの差分でtests/itに新しく現れたtestは、守る境界の宣言を要する。宣言の無いもの、境界の値が決まった集合の外のものは、runのプログラムのreviewとCIの関門で落ちる。境界の値はADR-t1410-1がtests/itで確かめる境界（SQLite・Git・プロセス・supervisorの配線・復旧とadopt）で、`docs/development/testing.md`の「判断と境界のtest」の一覧と同じ集合にする。既存のtestとその本文の変更は対象にしない。別のmoduleやファイルへ移しただけのtestは、境界が変わらないので新しいものとして数えない。新しいtestと移したtestの見分け方は関門のscriptの冒頭が持つ。

2. **宣言の置き場と見る役**: 宣言はtestを足すtaskのファイル（`.config/it-boundary.d/t<task ID>.toml`）に置く。別々のtaskの追加が同じファイルに触れないため（ADR-t1707-1の許可の一覧と同じ形）。機械は宣言の有無と境界の値が集合の内かを見て、そのtestでしか確かめられない理由が境界を書いているか（判断のcaseの並べ直しでないか）はtest-rulesのagentが見る。

3. **integrateの検証には足さない**: 関門はworkerの手元・runのプログラムのreview・CIのpushとpull_requestに置き、`integrate`の検証には足さない。ADR-t1707-1決定5の時間の関門の場所と同じで、直列の着地の時間を増やさないため。着地の前はプログラムのreviewが、着地の後はCIが捕まえる。

## Alternatives

- **tests/itの本数の上限を1つのファイルで持つratchet**: 本数の増加そのものは止められるが、並行のtaskが同じファイルの数を変えて衝突し、境界の代表を足す正当なtestも数の取り合いになる。採らない。
- **時間の関門の閾値を下げる**: 速い判断のITは閾値をいくら下げても通り、遅い境界のITの項目だけが増える。採らない。
- **agentのreviewだけに任せる**: 2026-10-04以後の増え直しを止められなかった（Context）。agentは理由の中身を見る役に残し、有無と値は機械が見る。

## Consequences

- 判断のcaseを並べたITを足すrunは、agentのreviewの前にプログラムのreviewで差し戻され、判断をunit testへ移すか境界を宣言するかを選ぶことになる。
- 宣言の一覧は、足したITが守る境界の台帳を兼ねる。testを消すか名前を変えるtaskは消えた名前の項目を外す（scriptは無いtestの項目を警告するだけ）。
- 宣言の理由が境界を書いているかは機械では見ないので、言い換えだけの理由はtest-rulesのagentが返す。
- 規則の本文は`docs/development/testing.md`の「判断と境界のtest」、宣言の書式・境界の値・新しいtestの見分け方・baseの求め方は`scripts/check-it-added-tests.sh`の冒頭のコメントが持つ。
