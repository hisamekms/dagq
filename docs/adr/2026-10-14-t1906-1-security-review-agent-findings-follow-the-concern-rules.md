---
id: adr-t1906-1
type: adr
title: 変更箇所のセキュリティを見るreviewのagentの指摘は、既存のreviseとconcernの規則で行き先を決め、セキュリティだけの経路や緩めた扱いを作らない
status: accepted
created: 2026-10-14
updated: 2026-10-14
accepted_on: 2026-10-14
owners:
  - hisamekms
tags:
  - runtime
  - review
  - security
related:
  - adr-t451-1
  - adr-t1453-1
  - adr-t1895-1
  - adr-t1728-1
  - design-supervisor-lifecycle-review
  - design-security
  - design-authorization
---

# ADR-t1906-1: 変更箇所のセキュリティを見るreviewのagentの指摘は、既存のreviseとconcernの規則で行き先を決め、セキュリティだけの経路や緩めた扱いを作らない

## Context

runのreviewはpathsで選んだagentを1本ずつのjobで動かし（[ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)）、判定ごとに最も重い行き先を機械で取る（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定7）。
全ての差分に当たるagentとして、変更箇所のsecretの扱い・認可の判定・外への送信・shellとpathの組み立て・sandboxと権限の広がりを見るagentを足す。
セキュリティの指摘には、workerが差分の中で直せるものと、認可の方針や権限の表のように人が決めるものが混じる。
指摘をworkerに返すか人に回すかを決めないと、agentが確信の無い指摘を`pass`の要約に落として着地させたり、逆に機械的な直しまで人に上げたりする。

## Decision

1. **行き先は既存の規則をそのまま当てる。**
   agentの判定は[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定1・3と[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定7の規則で行き先に直し、セキュリティだけの別の経路・段・緩めた扱いを作らない。
   決めるのは、agentがどの指摘にどの判定を出すかだけである。
2. **workerが差分の中で機械的に直せるものは`revise`。**
   例: secretやcredentialをlog・events・promptに出す行、shellの引数のquote漏れ、worktreeの外に向くpathの組み立て。
3. **判断の要るものは`concern`にし、推奨・確信度・`reason_category`を付ける。**
   - 人の判断が要るもの（認可の方針・権限の表・sandboxの範囲を広げる変更、secretの扱いの方針）は`reason_category`を`scope`にし、ADR-t451-1決定3のとおりaskに回る。
   - 確信度が`low`のものは`confidence`を`low`のまま出し、既存の確信度`low`の経路でaskに回る。
   - 確信度が`high`で人が要る理由の無いものは、runtimeが推奨（`land` / `send_back`）を適用する。
4. **確信度`low`の指摘をask以外に回さない。**
   確信度を`high`に上げること、`pass`にすること、`land`の推奨の要約やsend_backの理由に混ぜて自動で進めることをしない。
5. **未確認の観察と確信度`low`の`concern`を分ける。**
   確かめきれなかったが問題の疑いの無い観察（問題の有無を判定していないもの）は、判定の確信度とは別にsummaryに「未確認」と書いて残してよい。
   問題の疑いが残るものは観察にせず、確信度`low`の`concern`にする。

## Alternatives

- **セキュリティの指摘は全て人に回す**: 見落としは減るが、quote漏れのような機械的な直しまで人の答え待ちになり、ADR-t451-1決定1の原則（推奨が出せる判断はAIが決める）に反する。
- **確信度`low`の指摘をaskにせず`send_back`の理由やlandのnotesに載せる**: 人の待ちは減るが、AI自身が迷ったものが人に届かずに着地しうる。ADR-t451-1の歯止め（`low`は人へ）を外すので採らない。
- **セキュリティを特別な段（reviewの前後の別のjob）にする**: 既存のagentのjobと判定の集約で足り、別の経路は保守と食い違いを増やす。

## Consequences

- agentの定義（`.dagq/agents/security/AGENT.md`）はこの判定の出し分けを指示し、行き先の実装は変えない。
- 全てのrunのreviewにagentのjobが1本増え、reviewのjobの同時実行の枠を使う。
- セキュリティの指摘が人に上がるのは、既存のconcernの規則が人へ回すもの（`scope`・`discard`・確信度`low`・推奨の無いもの・reviseの上限超え）で、この決定はその例外を狭めも広げもしない。
