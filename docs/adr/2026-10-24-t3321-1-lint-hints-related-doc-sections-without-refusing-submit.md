---
id: adr-t3321-1
type: adr
title: dagq lintに、repositoryが設定した文書の中からtaskが名指すコードの識別子とpathを含みdescriptionとcontextが名指していない節を、submitを拒まないhintsとして足す。hintsはplan reviewの検査の規則を変えない
status: accepted
created: 2026-10-24
updated: 2026-10-24
accepted_on: 2026-10-24
owners:
  - hisamekms
tags:
  - planning
  - plan-review
  - documents
related:
  - adr-0047
  - adr-t1428-1
  - adr-t1942-2
  - adr-t2352-1
  - adr-t2257-1
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-run-environment
---

# ADR-t3321-1: dagq lintは関連文書の節をsubmitを拒まないhintsとして出す

## Context

挙動を変えるtaskは、関連文書（pathと節）と更新の理由をdescriptionかcontextに書く（[ADR-t1428-1](2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)決定2）。
登録の指示はpluginの登録の手順にあり、plan reviewはその欠けを見つけるとreviseにする。

plan reviewは自分で文書をgrepして欠けを見つけていた（finding 301。16日で22回のrevise）。
plannerはsubmitの前に同じ手がかりを機械的に得られず、毎回1往復（plan reviewとplannerの出し直し）が掛かっていた。
plan reviewのpromptの候補はtaskの重複の手がかりで、文書の候補を持たない。

語の一致だけでは文書の更新の要否は決まらない。
どの文書を照らすかはrepositoryごとに違い、runtimeにこのrepositoryの文書名を埋め込まない（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。

## Decision

1. **lintにsubmitを拒まないhintsを足す。**
   `dagq lint`は違反（violation）とは別に、taskごとのhintsを出す。
   hintは、taskのtitle・description・acceptance・contextが名指すコードの識別子とfileのpathを含む文書の節のうち、descriptionとcontextが文書も節も名指していないものを並べる。
   hintsはviolationにせず、`submit`はhintsを見ずに拒まない。要らない理由を書く余地をplannerに残すためである。
2. **照らす文書はrepositoryの設定で受ける。**
   文書の範囲はrepositoryの設定（文書のglob）が持ち、runtimeは文書名を持たない。
   設定が無ければhintsは出さない。
   文書は他のtaskの設定と同じくqueueが束縛されたmain checkoutから読み、読めない文書は飛ばしてその旨をhintに残す。
3. **hintsはplan reviewの検査の規則を変えない。**
   hintsはplannerの手がかりで、CLIの`lint`の決まった規則にも、plan review jobのLLMの意味の検査の規則にも足さない。

次の現行の決定は保つ。

- [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定10（plan reviewの検査をCLIの`lint`の決まった規則とplan review jobのLLMの意味の検査に分ける）。
- ADR-t1428-1決定1〜3（schemaを変えない・plannerが関連文書を書く・taskの変えてよいpathsは許可で変える文書の一覧ではない）。
- [ADR-t1942-2](2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)決定1〜3が改訂した後のADR-t1428-1決定4（workerは変えた名前で候補を探し、designを書くのは流れ・境界・不変条件・コードから読めない約束が変わったときだけで、summaryに照合したpath・節か不要の理由と探した名前を書く）。
- ADR-t1942-2決定4と[ADR-t2352-1](2026-10-16-t2352-1-review-sends-back-only-documents-the-diff-touched-and-follows-up-the-rest.md)が改訂した後のADR-t1428-1決定5（runのreviewは両方向に照合し、差し戻すのは差分が変えた文書と差分の食い違いと不変条件・約束・境界の食い違いだけで、差分が触っていない文書の古い記述は`docs_drift`のfollow_up）。

上限・識別子の取り出し方・節の切り方はコードのdoc commentが持つ。

## Alternatives

- **欠けをlintのviolationにしてsubmitを拒む**: 語が一致しても更新の要らない節は多く、要らない理由を書く余地が無くなる。
- **plan reviewのpromptに文書の候補を足す**: plan reviewの往復は減らず、plannerがsubmitの前に直せない。
- **照らす文書をruntimeに固定する（`docs/design/`など）**: 外のrepositoryで文書の置き場は違い、ADR-t2257-1に反する。
- **登録の指示を強める**: 指示は既にあり、指示だけでは機械の手がかりが無いことは変わらない。

## Consequences

- plannerは`lint`のhintsを読み、当たる節をdescriptionかcontextに足すか、要らない理由を書いてからsubmitする（pluginの登録の手順）。
- 識別子の一致に頼るので、識別子を名指さないtaskには何も出ず、バッククォートの外のありふれた語は拾わない。plan reviewの文書の範囲の検査はこれまでどおり残る。
- repositoryが設定を置くまで何も変わらない。
