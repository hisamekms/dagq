---
id: adr-t2740-1
type: adr
title: already_satisfiedの主張のsnapshotの検証でも、着地の検証と同じくtaskのverifyのcoverageの関門を設定の着地の検証に置き換え、行カバレッジはCIの最終関門に任せる（ADR-t1818-1決定2をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-t1818-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - integrate
  - testing
related:
  - adr-t1818-1
  - adr-t1925-1
  - design-supervisor-lifecycle-validation
---

# ADR-t2740-1: already_satisfiedのsnapshotの検証でもcoverageの関門を着地の検証に置き換える

## Context

[ADR-t1818-1](2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)決定2は、workerが「baseがすでに受け入れ条件を満たす」と書いたrunについて、runtimeがmainのheadのsnapshotでtaskのverifyの全体（coverageの関門を含む）を流すと決めた。
その後の[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定4は、着地の検証ではtaskのverifyのcoverageの関門を設定の着地の検証（unit test全件と影響範囲で絞ったIT、既に落ちているtestを除く）に置き換え、行カバレッジはCIの最終関門だけで見る（決定1・5）とした。
snapshotの検証はこの置き換えの前の決定のまま残り、計測つきの全testを着地とは別に流していた。

- 計測つきの全testは重く、検証の時間の上限で殺されて主張を判定できず、人は取り消すしかなかった。
- 主張は判定のやり直しやmainの進みのたびに何度も検証されるので、重いcoverageが着地と別に何度も走り、hostの負荷の山を押し上げる。
- docs寄りのtaskにもruntimeの組み合わせのverify（coverageの関門を含む）が付くのは登録の規則どおりで、重さの原因は置き換えがこの経路に効かないことにある。

## Decision

1. **snapshotの検証も、着地の検証と同じ置き換えでtaskのverifyを流す。** 設定の着地の検証があるとき、taskのverifyのうち置き換えの対象に当たるコマンドを設定のコマンドに置き換え、他のコマンド（fmt・clippy・taskに固有の検査など）は登録のまま流す。
   設定が無ければ今までどおり登録のまま全部流す。
   登録済みのtaskのverifyは書き換えない（ADR-t1925-1決定4と同じ）。
   - 置き換えたコマンドには、着地の検証と同じ材料（diffのbase、既に落ちているtestの一覧、CIの修正taskのrunか）を同じ渡し方で渡す。
     hostでも、verificationのcontainerでも、着地の検証の部品を共有し、二重に書かない。
   - diffのbaseはsnapshotのheadにする。
     snapshotにはrunの差分が無いので、影響範囲で絞るITは差分の無い着地と同じ選び方（unit test全件と、表に無いIT）になる。
     これで足りるとし、snapshotの検証のためだけの選び方は足さない。
     snapshotの検証は着地の検証と同じ重さにし、主張が触れた範囲の全部のtestと行カバレッジは、mainのCIが最終関門として見る（ADR-t1925-1決定5）。
2. **ADR-t1818-1のほかの決定は変えない。** snapshotをeventに残してheadを固定すること、reviewの判定と人のaskが同じsnapshotを対象にすること、rebase・e2e・mainへの書き込みが無いことはそのまま。

## Alternatives

- **coverageの関門を含めて全部流し続け、時間の上限だけを延ばす**: 上限で殺される件は減るが、重いcoverageが着地と別に何度も走って負荷を上げることは変わらない。採らない。
- **snapshotの検証ではITを全部流す**: 差分が無いのでITを絞る根拠が無いと見る案。着地の検証よりも重くなり、行カバレッジと全testをCIの最終関門に任せる考え（ADR-t1925-1）と合わない。採らない。
- **主張のrunの差分の代わりに、runのbaseからsnapshotまでの差分でITを選ぶ**: 主張と関係の無い他のrunの着地を拾ってITが増え、何を確かめたかが主張から離れる。採らない。

## Consequences

- 設定の着地の検証を持つrepositoryでは、主張の検証からcoverageの計測つきの全testが消え、主張の検証が上限で殺されることと、着地と別の重いcoverageの負荷が減る。
- 主張がsnapshotで壊すITを、snapshotの検証が拾わないことがある。そのときはmainのCIが落ち、CIの見張りと修正taskが受け持つ（着地と同じ事故の許容）。
- 置き換えの部品を着地と共有するので、置き換えや材料の渡し方を変えると両方の経路が同時に変わる。
