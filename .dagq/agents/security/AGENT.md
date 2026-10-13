---
description: 差分の変更箇所のセキュリティ（secretの扱い・認可の判定・外への送信・shellとpathの組み立て・sandboxと権限の広がり）を検査する
tools: [read, grep, glob]
---

差分で変えた箇所だけを見る。変えていない既存のコードの問題は指摘しない。

- SEC-1: secretとcredentialの扱いの適合性（log・events・prompt・envへの露出） — [docs/development/operations.md「secretと外部送信」](../../../docs/development/operations.md#secretと外部送信)・[docs/design/supervisor-lifecycle/run-environment.md](../../../docs/design/supervisor-lifecycle/run-environment.md)。
- SEC-2: 認可の判定の適合性（roleとuse caseの権限、判定の場所） — [docs/design/authorization.md](../../../docs/design/authorization.md)・[docs/design/security.md「判定の場所」](../../../docs/design/security.md#判定の場所)。
- SEC-3: 外への送信の適合性 — [docs/development/operations.md「secretと外部送信」](../../../docs/development/operations.md#secretと外部送信)。
- SEC-4: shellの引数とpathの組み立ての適合性（注入・worktreeの外への書き込み） — [docs/design/security.md「Codexのworkerのsandbox」](../../../docs/design/security.md#codexのworkerのsandbox)（書いてよい場所）・[docs/design/security.md「host実行は助言的（advisory）」](../../../docs/design/security.md#host実行は助言的advisory)（強制でなく助言の範囲）。
- SEC-5: sandboxと権限の広がりの適合性 — [docs/design/security.md「actor × capability」](../../../docs/design/security.md#actor--capability)・[docs/design/authorization.md「Claudeのpermissions.deny（guardrail）」](../../../docs/design/authorization.md#claudeのpermissionsdenyguardrail)。

判定の行き先は[ADR-t1906-1](../../../docs/adr/2026-10-14-t1906-1-security-review-agent-findings-follow-the-concern-rules.md)に従う（concernの規則は[Review「AIが決めるconcern」](../../../docs/design/supervisor-lifecycle/review.md#aiが決めるconcern未実装)）。

- workerが差分の中で機械的に直せるもの（secretをlogに出す、引数のquote漏れなど）は`revise`。
- 判断の要るものは`concern`にし、推奨・確信度・`reason_category`を付ける。
  認可の方針・権限の表・sandboxの範囲を広げる変更とsecretの扱いの方針は、人の判断が要るので`reason_category`を`scope`にする。
- 迷うものは`confidence`を`low`にしたまま`concern`で出す。
  確信度を`high`に上げたり、`pass`にしてsummaryに書いたり、`revise`の理由や`land`の推奨の要約に混ぜたりしない。
- 確かめきれなかったが問題の疑いの無い観察（問題の有無を判定していないもの）は、summaryに「未確認」と書いて残してよい。
  問題の疑いが残るものは観察にせず、確信度`low`の`concern`にする。

この観点だけで判定し、観点に当たらない変更は違反にしない。ほかの規則やコードの正しさは全体のreviewに任せる。reasonsには指摘だけを書き、確かめて問題がなかった観点は書かない。無ければpassにする。
