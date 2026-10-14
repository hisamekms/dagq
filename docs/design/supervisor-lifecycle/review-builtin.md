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

## 今の節と食い違う境界

決定済みで、実装の着地まで今の節が正しい境界。

- **built-inのagentのeval**: 今のevalは定義とケースを`.dagq/agents/<name>/`から読み、ケースの集合のdigestをケースのidとpatchの中身から作り、landing branchのcommitのprogramをケースのtreeに先に流す（[Agent eval](../agent-eval.md)の「定義とケースの置き場」「hold-outの1回の制限」「programのreviewの当て方」）。
  決定後は、built-inのうちsecurity・testsだけを、dagqに同梱した定義とケースで測り、acceptance（全体のreview）は対象にしない。
  同梱のケースは小さなbaselineとpatchを持ち、evalは周ごとに対象のrepositoryと独立の一時のGit repositoryにbaselineをbase、patchを当てたcommitをheadとして作り、対象のrepositoryの履歴・ref・worktreeを読まず変えない。
  同梱のケースの集合のdigestはbaselineとpatchの中身から作る。
  built-inのevalには、pjの規則のfile（`.dagq/review/builtin-<名前>.md`）もpjのprogramも当てない。
  productionのケースは本番のrunのcommitをbaseにするので、今どおり対象のrepositoryのGitから読む。
  pjのagentのevalは今の節のまま変わらない。
- **文書の照合**: 今は全体のreviewのpromptの`REVIEW_DOCS_CHECK`が、変えた挙動に対する文書の古さ・欠けと、コードの書き写し・経緯の指摘を持つ（[Review](review.md#文書の照合)）。
  決定後も変えた挙動に対する文書の古さ・欠けの照合は全体のreview（`builtin:acceptance`）だけが持ち、`builtin:docs`は同梱しない（[ADR-t2389-1](../../adr/2026-10-12-t2389-1-docs-check-only-in-the-overall-review.md)）。
  コードの書き写しと経緯の指摘はruntimeに置かず、pjの規則（このrepositoryでは`.dagq/review/builtin-acceptance.md`）が持つ。

## 生成

- pjの規則のfileと`dagq.toml`の設定は、人が打つ生成のコマンドがcoding agentにpjを一度だけ読ませて一時の場所に作り、差分を人が承認したときだけmain checkoutの作業ツリーに適用する。
  commitとpushはしない。
