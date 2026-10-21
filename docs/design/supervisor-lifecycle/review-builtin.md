---
id: design-supervisor-lifecycle-review-builtin
type: design
title: built-inのreview（一部実装）
status: draft
created: 2026-10-12
scope: runtime
related:
  - adr-t2259-1
  - adr-t2389-1
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-review-programs
  - design-supervisor-lifecycle-run-environment
  - design-agent-eval
---

# built-inのreview（一部実装）

**全体のreviewの`builtin:acceptance`とbuilt-inのagent（security・tests）は実装済みで[Review](review.md)の今の節が、built-inのprogram（secrets）は実装済みで[プログラムのreview](review-programs.md)の今の節が持つ。**
**pjの規則のfileと設定の生成は実装済みで[setup-review](setup-review.md)が持つ。**
**built-inのagent（security・tests）のevalは実装済みで[Agent eval](../agent-eval.md)が持つ。**
**文書の照合は実装済みで[Review](review.md#文書の照合)の今の節が持つ。**
変えた挙動に対する文書の古さ・欠けの照合は全体のreview（`builtin:acceptance`）だけが持ち、`builtin:docs`は同梱しない。
コードの書き写しと経緯の指摘はruntimeに置かず、pjの規則のfile（このrepositoryでは`.dagq/review/builtin-acceptance.md`）が持つ。
残りは決定済みの予定で、まだ実装していない。
決定は[ADR-t2259-1](../../adr/2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)と、それを直した[ADR-t2389-1](../../adr/2026-10-12-t2389-1-docs-check-only-in-the-overall-review.md)が持つ。
未実装の部分の今の挙動は[Review](review.md)と[プログラムのreview](review-programs.md)の今の節のとおりである。
実装したら、各文書の今の節をこの形に直し、名前・欄・既定値の意味は定義のそばのdoc commentに置き、この文書には流れ・境界だけを残すか消す。

## 何が変わるか

- dagqのバイナリがbuilt-inのagentとprogramを持ち、`dagq.toml`の`builtin:<名前>`のtableで有効・無効と範囲を設定する。
  書かないrepositoryでも既定で有効なbuilt-inが流れる。
- 全体のreviewのjobはbuilt-inのacceptanceになり、常に走る（実装済み）。
  無効にできず、範囲とproviderも設定できない（全ての差分で走り、providerは`[roles.review]`が決める）。
- pjが中身を変えたいときはbuilt-inを無効にして自分のagentかprogramを作り、同じ名前で上書きしない。
