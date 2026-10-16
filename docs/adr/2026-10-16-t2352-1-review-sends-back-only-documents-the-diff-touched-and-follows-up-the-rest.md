---
id: adr-t2352-1
type: adr
title: runのreviewが文書の指摘で差し戻すのは、差分が変えた文書と差分の食い違いと、差分と文書の述べる不変条件・約束・境界の食い違いだけにし、差分が触っていない文書の古い記述はverdictのfollow_upsで返してruntimeがdocs_driftのfollow_upのdraftにする（ADR-t1942-2決定4、ADR-t1428-1決定5をamends）
status: accepted
created: 2026-10-16
updated: 2026-10-16
accepted_on: 2026-10-16
amends:
  - adr-t1942-2 decision 4
  - adr-t1428-1 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - review
  - documentation
related:
  - adr-t1428-1
  - adr-t1942-2
  - adr-t2389-1
  - adr-t2259-1
  - adr-t1453-2
  - adr-t2257-1
  - adr-t1091-1
  - design-supervisor-lifecycle-review
---

# ADR-t2352-1: runのreviewが文書の指摘で差し戻すのは差分が触った文書と不変条件の食い違いだけにし、差分が触っていない文書の古い記述はfollow_upにする

## Context

[ADR-t1428-1](2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)決定5と[ADR-t1942-2](2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)決定4（[ADR-t2389-1](2026-10-12-t2389-1-docs-check-only-in-the-overall-review.md)がamends）は、runのreviewが関連文書を差分と照合し、taskが名指したかどうかに関わらず見つけたずれを指摘すると決めた。
指摘はverdictの理由になり、差分が触っていない文書の古い記述（差分の前から古かったもの、差分が変えた挙動の説明が別の文書に無いか古いこと）でもrunは差し戻される。

人の分析（request 103）では、2026-10-05〜10-12の6.4日のrunのreview 692回のうち差し戻しが320回、文書の指摘を含む差し戻しが281回、文書の指摘だけの差し戻しが134回で、差し戻しから次のreviewまでの待ち204時間のうち文書の分は約123〜138時間だった。
差分が触っていない文書の古さはそのrunの作業の欠陥ではないのに、runごとに往復の待ちを積む。
人の言葉: 「runのreviewは、差分が触った文書の食い違いと不変条件の食い違いだけを差し戻し、差分が触っていない文書の古い記述はfollow_up（docs_drift）にする」。

今のreviewのverdictにはfollow_upを作る口が無く、follow_upのdraftを作るのはworkerのreceiptの`follow_ups`だけである。

## Decision

1. **差し戻す文書の指摘は2つだけにする。** runのreviewが文書の指摘を理由にrunを差し戻す（`revise`、または`concern`の`send_back`）のは、(a) 差分が変えた文書が差分と食い違うことと、(b) 差分（コード・設定）が文書の述べる不変条件・約束・境界と食い違うこと、だけにする。
   何を文書の食い違い・欠陥とするか（コードの書き写し・経緯の混入など）はrepositoryの規則が決め、このADRは差し戻すかfollow_upにするかの振り分けだけを決める。
2. **差分が触っていない文書の古い記述はfollow_upにする。** 差分の前から古かった記述と、差分が変えた挙動の説明が差分の触っていない文書に無いこと・古いことは差し戻さず、reviewがverdictのfollow_upsに文書のpath・節・理由で返す。
   follow_upsは全体のreviewとagentのjobのどちらのverdictも、pass・revise・concernのどれでも持てて、verdictの判定と行き先を変えない。
3. **runtimeがfollow_upのdraftに記録する。** runtimeはreviewの判定のときに、follow_upsの各項目をそのrunのtaskのfollow_upのdraft（category `docs_drift`、元のrunとgoalをsourceに持つ）として、receiptの`follow_ups`と同じ経路（深さ・origin・runtimeのplannerの束ね方）で登録する。
   同じrunでは文書のpathと節で1つにし、同じreviewの別のjob、同じrunの次の回のreview、supervisorの再起動の後の同じverdictの判定し直しで重ねて作らない。
4. **runtimeが持つのは汎用の部分だけにする。** runtimeと同梱のbuilt-inの定義が持つのは、verdictのfollow_upsの形・保存・重複の防止と、文書の指摘のreasonsとfollow_upsへの振り分けだけにする。
   書き写し・経緯の混入を差し戻す判定などrepository固有の規則は、このrepositoryでは`.dagq/`の定義と[documents.md](../development/documents.md)が持つ（[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定5、[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。

## Amendsの判断

変える決定はADR-t1942-2の4個のうち1個と、ADR-t1428-1の5個のうち1個で、どちらも大半ではないので[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)によりamendsにする。

- **ADR-t1428-1決定5**（reviewの照合）: 「見つけたずれは今までどおり指摘する」を、差分が触った文書の食い違いと不変条件・約束の食い違いは差し戻しの理由に、差分が触っていない文書の古い記述はfollow_upsにする、に変える。taskの記述・実装の差分・関連文書・summaryの根拠を照合すること、文書の差分だけで正しいとしないことは保つ。
- **ADR-t1942-2決定4**（両方向の照合、ADR-t2389-1がamendsした形）: 欠けの指摘（差分と食い違う古い記述、変わった流れ・境界・不変条件・約束の書き漏れ）のうち、差分が触っていない文書にあるものの行き先をfollow_upsに変える。両方向に照合すること、名前の欠けだけでは指摘しないこと、差分が入れたものだけを書き写しと経緯として指摘すること（差分が変えた文書の指摘なので差し戻しの理由に残る）、照合を全体のreviewが行うことは保つ。

## Alternatives

- **差し戻したまま、注意として軽く扱う**: 差し戻しの理由である限りrunは往復し、待ちは減らない。理由の重さを区別しても、行き先が同じ`revise`なら効果が無い。
- **workerにreceiptのfollow_upを足させるreviseにする**: 文書の古さをworkerがfollow_upに書き直すだけの往復が残り、待ちは差し戻しと同じだけ積もる。reviewが見つけたものをreviewが直接返すほうが往復が無い。
- **差分が触っていない文書を照合しない**: 差分が変えた挙動の説明が別の文書で古くなったことを誰も拾わなくなる。follow_upにすれば、拾ったうえでrunの着地を止めない。

## Consequences

- 文書の指摘だけの差し戻しと、その待ちが減ることを期待する。前後の比較はgoal 194の測定が行う。
- 差分が触っていない文書の古さは`docs_drift`のfollow_upのdraftとして溜まり、runtimeのplannerが元のrunのreceiptのfollow_upと束ねて採るか捨てるかを決める。
- reviewのjobがfollow_upsに入れるべきものをreasonsに入れたときは、今までどおり差し戻される。振り分けの誤りはreviewの指示とrepositoryの規則で直す。
