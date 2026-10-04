---
id: adr-t1688-1
type: adr
title: workerは文書の候補を、変えた名前でrepositoryの文書を探して拾い、summaryに探した名前を書く。探す手段は名指さず検査の工程にしない（ADR-t1428-1決定4をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-t1428-1 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - worker
  - review
related:
  - adr-t1428-1
  - adr-t1420-1
  - adr-t1453-2
  - design-supervisor-lifecycle-prompt
  - development-documents
---

# ADR-t1688-1: workerは文書の候補を、変えた名前でrepositoryの文書を探して拾い、summaryに探した名前を書く。探す手段は名指さず検査の工程にしない（ADR-t1428-1決定4をamends）

## Context

2026-10-04の人の依頼（request 16「文書の更新漏れを初回レビュー前に減らす」の改善案2）の検討（goal 112）。[ADR-t1428-1](2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)の決定4は、workerがreceiptの前に「taskが名指す文書と作業中に見つけた関連文書」を差分と照合し、summaryに更新したpath・節か不要の理由を書くと決めた。

- **見落としは「読まない」ことではなく「候補を見つける手段が無い」ことから来た。** task 1400と1462のreceiptのsummaryはどちらも照合した文書を書いており、AGENTS.md → documents.mdの「workerの文書の照合」とworkerのpromptの`DOCS_CHECK`は読まれ適用されていた。runのreviewが`docs_drift`で差し戻した文書は、taskが名指さずworkerが作業中に出会わなかった、同じ挙動を別の角度から書く文書だった。task 1400は`docs/design/overview.md`のplannerの行と退役した役割の段落・`docs/design/authorization.md`・`docs/design/supervisor-lifecycle/roles.md`・`triage.md`（役割の説明が散らばる）、task 1462は`docs/design/plugin-integration.md`「読み込みと検証」の`tests/plugin.rs`の検査の一覧。どれも変えた名前（`plan`・`up` / `down`・planner・`tests/plugin.rs`）で文書を探せば出る。`DOCS_CHECK`は候補を「named by the task or found as you work」とし、探し方を持たない。
- **`docs_drift`の増加は悪化の根拠にならない。** `kpi --compare 2026-10-03T01:21:58Z --area runtime`の主理由の`docs_drift`は4.5%→37.3%に増えたが、同じ後の窓にreviewのsubagent（`.dagq/review-agents/`のdesign-consistency・adr-rulesほか、commit 9384cb25、2026-10-03T04:12Z）とtask 1429の`REVIEW_DOCS_CHECK`（03:51Zのhandoff）が入り、検出の条件が変わった。前後比較は検出の条件をそろえた区間を基準にする（測定はgoal 112の測定のtask）。

## Decision

ADR-t1428-1の決定4のうち、候補の拾い方とsummaryに書くものを次のとおり変える。照合した文書の扱い（pathsの中の古いものを直す、外は`docs_drift`のfollow_upにpathと節を書く、日付だけの差分を強いない）と、決定1〜3・5・6は変えない。

1. **workerは文書の候補を、taskが名指すものと作業中に見つけたものに加え、変えた名前（コマンド・flag・設定のkey・役割・fileのpathなど、差分が変える挙動の名前）でrepositoryの文書を探して拾う。** 探す手段（検索のツール）は名指さず、検査の工程（決まったコマンドの実行や結果の提出）にしない。runtimeのpromptの句は汎用にし、repository固有の探す範囲と見落としやすい型は、このrepositoryでは[documents.md](../development/documents.md)の「workerの文書の照合」に書く（[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）。
2. **summaryには、照合したpath・節か不要の理由に加えて、探した名前を短く書く。** これは読んだことの申告ではなく、reviewが同じ名前で探し直して見落としを確かめられる成果の根拠である。このrepositoryでは、runのreviewのdesign-consistencyのsubagent（`.dagq/review-agents/design-consistency.md`）が、探した名前が差分の変える名前を覆っているかを確かめ、足りなければその名前で探し直してずれを`docs_drift`で指摘する。そのsubagentが当たるpathは`dagq.toml`の`[review.subagents.design-consistency]`のとおりで（pluginや`tests/`だけの変更には当たらない）、広げるかはこのADRでは決めない。

ADR-t1428-1が退けた「workerにgrepなどの検査のコマンドを課す」は、決まったコマンドの実行を課さない点で引き続き退ける。この決定は探す対象を名前で決めるだけで、重い検査の工程を足さない方針（ADR-t1428-1決定6）と両立する。Task・receipt・queue DBの構造とschema、reviewのverdictの形と分類コードは変えない。文面と文字数は[Prompt](../design/supervisor-lifecycle/prompt.md#文書の照合)が持つ。

## Alternatives

- **リンク・見出し・frontmatterを検査するscriptを足す**: 実例の差し戻し（task 1400・1462）は意味のずれで、機械の検査では見つからない。adr-rulesのsubagentがリンクとIDの解決を既に見ており、それによる差し戻しの記録が無い。
- **毎回、独立したagentのreviewを足す**: runのreviewが既にdesign-consistencyのsubagentを`src/`・`crates/`・`migrations/`・`docs/design/`・`docs/development/`の変更に当て、ほかの変更も基本のreviewが`REVIEW_DOCS_CHECK`で文書を照合しており、もう1回のreviewの費用に見合う根拠が無い。
- **summaryかreceiptに文書の照合の欄を足す**: Task・receipt・schemaを変えないと決めた（ADR-t1428-1決定1）。

## Consequences

- workerの作業に数回の検索と数件の節の読みが加わる（数十秒〜1、2分の見込み）。減らしたいのは、初回のreviewで`docs_drift`が付いたrunのreviseと再review（goal 91の記録で主理由3件のrevise 615秒、1回の再reviewの中央値216秒）。
- 効果と費用は、検出の条件をそろえた前後の区間で、初回のreviewの`docs_drift`の率・文書の照合の記録が無いrunの数・reviseと再reviewの時間・workerの作業時間をgoal 112の測定のtaskが数える。
- workerのsummaryに探した名前の1句が増える。
