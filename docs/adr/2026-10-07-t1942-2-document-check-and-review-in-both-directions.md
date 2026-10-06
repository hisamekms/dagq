---
id: adr-t1942-2
type: adr
title: workerの文書の照合とrunのreviewを両方向にする。workerは変えた名前で候補を探し探した名前をsummaryに書くが、designに書くのは流れ・境界・不変条件・コードから読めない約束が変わったときだけで、名前がdesignに無いことはずれではない。reviewは欠けに加えてコードの書き写しと経緯の混入を指摘する（ADR-t1428-1決定4・5をamends、ADR-t1688-1を置き換え）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
supersedes:
  - adr-t1688-1
amends:
  - adr-t1428-1 decision 4
  - adr-t1428-1 decision 5
owners:
  - hisamekms
tags:
  - documentation
  - worker
  - review
related:
  - adr-t1428-1
  - adr-t1688-1
  - adr-t1942-1
  - adr-t1420-1
  - adr-t1453-2
  - design-supervisor-lifecycle-prompt
  - development-documents
---

# ADR-t1942-2: workerの文書の照合とrunのreviewを両方向にする（ADR-t1428-1決定4・5をamends、ADR-t1688-1を置き換え）

## Context

2026-10-06の人の依頼（request 44、goal 159）。
[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)は、designに概念と地図だけを書き、細かい事実はコードのdoc comment、経緯はADRとgitに置くと決めた。

今の照合とreviewは、designを増やす方向にだけ働く。

- [ADR-t1428-1](2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)決定4は、workerが関連文書を差分と照合し古いものを直すと決め、決定5はreviewが見つけたずれを指摘すると決めた。
- [ADR-t1688-1](2026-10-04-t1688-1-worker-searches-documents-by-changed-names.md)は、workerが変えた名前でrepositoryの文書を探し、探した名前をsummaryに書くと決め、reviewのsubagentが探した名前を確かめる。
- この組み合わせは「変えた名前がdesignに無ければずれ」と読まれ、workerは新しいeventの欄・flag・既定値をdesignに書き足し、reviewは書き写しも経緯の混入も指摘しなかった。
  9/29以降の着地の70%が`docs/design/`を変えた。

ADR-t1688-1は決定2個の両方が変わるので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)により丸ごと置き換える。
ADR-t1428-1は決定6個のうち2個を変えるのでamendsにする。

## Decision

1. **workerは文書の候補を、taskが名指すもの・作業中に見つけたものに加え、変えた名前（コマンド・flag・設定のkey・役割・fileのpathなど、差分が変える挙動の名前）でrepositoryの文書を探して拾う。** 探す手段（検索の道具）は名指さず、検査の工程（決まったコマンドの実行や結果の提出）にしない。runtimeのpromptの句は汎用にし、repository固有の探す範囲と見落としやすい型は、このrepositoryでは[documents.md](../development/documents.md)の「workerの文書の照合」に書く（ADR-t1453-2）。（ADR-t1688-1決定1を引き継ぐ。）
2. **designを書くのは、流れ・境界・不変条件・コードから読めない約束が変わったときと、記述が今のコードかacceptedのADRと食い違うときだけにする（ADR-t1428-1決定4をamends）。** 候補の文書が差分と食い違う（古い記述が今のコードやacceptedのADRと合わない）ときは、taskのpathsの中なら直し、外なら`docs_drift`のfollow_upにpathと節を書く。変えた名前がdesignに無いことはずれではなく、名前・欄・既定値を足すためにdesignを書き足さない。その意味は定義のそばのdoc commentに書く。designを直すときは経緯（task番号・「以前は」・文字数の変遷）を書き込まない。文書の変更が要らないtaskに日付だけの差分を強いないこと、決定4のそれ以外（照合の時点・summaryに更新したpath・節か不要の理由を書くこと）は変えない。
3. **summaryには、照合したpath・節か不要の理由に加えて、探した名前を短く書く。** 読んだことの申告ではなく、reviewが同じ名前で探し直して見落としを確かめられる根拠である。（ADR-t1688-1決定2を引き継ぐ。）
4. **runのreviewは両方向に照合する（ADR-t1428-1決定5をamends）。** taskの記述・実装の差分・関連文書・summaryの根拠を照合し、文書の差分があることだけで正しいとしないことは変えない。指摘するのは、欠け（差分と食い違う古い記述、変わった流れ・境界・不変条件・約束の書き漏れ）に加えて、差分が文書に入れたコードの書き写し（eventの欄・flag・既定値・関数名・test名の列挙）と経緯の混入である。名前がdesignに無いことだけでは欠けとして指摘しない。このrepositoryでは、探した名前の確かめと両方向の指摘をrunのreviewのdesign-consistencyのsubagentが受け持ち、当たるpathはrepositoryの設定が持つ（[documents.md](../development/documents.md)の「workerの文書の照合」）。

Task・receipt・queue DBの構造とschema、reviewのverdictの形と分類コードは変えない。
ADR-t1428-1が退けた「workerにgrepなどの検査のコマンドを課す」は引き続き退ける。
文書の形と大きさの機械の検査はADR-t1942-1決定7が持ち、この決定はAIの照合とreviewだけを決める。
workerのpromptの文面とrunのreviewの定義は後続のtaskが変え、文面は[Prompt](../design/supervisor-lifecycle/prompt.md#文書の照合)が持つ。

## Alternatives

- **ADR-t1688-1をamendsで直す**: 決定2個の両方を書き直すので、ADR-t1091-1により置き換える。
- **名前による候補の探し方をやめる**: 役割の説明が散らばる文書などの見落とし（task 1400・1462の差し戻し）は名前で探せば出る。探し方は残し、見つけた後に書く条件を絞る。
- **reviewは欠けだけを見続け、書き写しは大きさの検査に任せる**: 予算内でも書き写しと経緯は溜まり、機械では意味を見分けられない。
- **書き写しと経緯の指摘に新しい分類コードを足す**: reviewのverdictの形を変えずに今のコードで指摘でき、足すかは計測の後に決める。

## Consequences

- workerがdesignに書き足す量と、designを変える着地の割合が減ることを期待する。goal 159の前後比較で、着地のうちdesignを変えた割合とdocsの衝突の件数を読む。
- reviewの差し戻しに、書き写しと経緯の混入の指摘が加わる。書き直しの段の前は既存の文書に書き写しが多いが、指摘するのは差分が入れたものだけにする。
- [documents.md](../development/documents.md)の「workerの文書の照合」を同じ変更で合わせる。workerとrunのreviewのpromptの文面と、design-consistencyの定義は後続のtaskで直すまで、ADR-t1688-1の文面のまま動く。
