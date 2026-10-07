---
id: adr-t1967-1
type: adr
title: ADRの索引をADRのfrontmatterから生成してcommitせず、gitignoreしたdocs/adr/INDEX.mdにgitのhookと「無ければ作る」scriptで作り、状態の変更で索引を手で直さない（ADR-t598-1決定10をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t598-1 decision 10
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - adr-t598-1
  - adr-t1091-1
  - adr-t1942-1
  - development-documents
---

# ADR-t1967-1: ADRの索引をfrontmatterから生成し、commitしない（ADR-t598-1決定10をamends）

## Context

[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定10は、`docs/adr/README.md`を索引にし、ADRの状態を変える変更で同じく表を直すと決めた。
ADRを足す・状態を変えるtaskはどれも表の末尾の同じ位置に行を足すので、並行するrunが同じ行で衝突する。
docsだけの衝突135件のうち18件（13%）がこの表の末尾への追記だった（request 46の実測）。
表の中身は各ADRのfrontmatter（`id`・`title`・`status`・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`）の書き写しで、手で直すと欄とずれうる。

生成の時点を人に聞き（ask 536）、人は選択肢のどれでもない別案を選んだ: gitignoreした`docs/adr/INDEX.md`に、post-checkoutのhookと「無ければ作る」scriptで生成し、cloneの後に`core.hooksPath`を1回設定する。

## Decision

1. **索引はADRのfrontmatterから生成し、commitしない。** 状態と後継の正本は各ADRのfrontmatterの欄で、ADRを足す・状態を変える変更は索引を手で直さない。
2. **置き場はgitignoreした`docs/adr/INDEX.md`。** `docs/adr/README.md`は状態の意味と規則への案内だけの手書きの文書にし、表を持たない。
3. **生成はrepositoryに置いたgitのhookと、「無ければ作る」scriptの2つで行う。** hookはcheckout・`git worktree add`・merge（着地でmain checkoutを進めるfast-forwardを含む）のたびに索引を作り直し、失敗してもgitの操作を止めない。hookを設定していない環境（CIや設定前のclone）は、読む側がscriptを打って無ければ作る。hookの有効化はcloneの後に人が1回行い、workerはhostのgitの設定を変えない。

scriptの名前・引数・列と並び、hookの置き場と設定のコマンドは[文書の規則](../development/documents.md)の「ADR」と`scripts/adr-index.sh`が持つ。
ADR-t598-1の決定1〜9・11・12は変えない。

## Alternatives

- **README.mdの表を残し、衝突をruntimeの自動解消に任せる**: 衝突は減らず、解消のたびにrunが待つ。表と欄のずれも残る。
- **索引を生成してcommitする（着地のたびやCIで作り直す）**: 生成物の差分が同じ行で衝突するか、着地の後に別のcommitが要る。人はcommitしない案を選んだ（ask 536）。
- **索引を持たず`grep`で探す**: 状態と後継を一覧で読む手段が無くなり、plan reviewと人が辿りにくい。
- **ADR-t598-1を丸ごと置き換える**: 決定12個のうち変えるのは1つで、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)によりamendsにする。

## Consequences

- ADRを足す・状態を変えるtaskは`docs/adr/README.md`に触れず、索引の行で衝突しない。
- 索引を読む人・plan review・workerは、`docs/adr/INDEX.md`が無ければ`sh scripts/adr-index.sh`で作ってから読む。hookが無いcheckoutや、自分でADRを足した・状態を変えたworktreeでは索引が古いので作り直す。
- frontmatterの欄が索引に要る値を欠くとscriptが落ちるので、CIが`--stdout`で全てのADRを読めることを確かめる。
- 手書きの表にあった題の補足（「〜を統合」など）はfrontmatterの`title`に無ければ索引に出ない。
