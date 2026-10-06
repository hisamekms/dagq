---
id: adr-t1453-1
type: adr
title: dagq.tomlがpathごとに名指すreviewのsubagentを、信頼するmainのcommitから写して親のreview jobの中で実行し、1つのverdictに集約してsupervisorが結果のそろいを検査する
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amended_by:
  - adr-t1470-1
  - adr-t1895-1
  - adr-t1728-1
  - adr-t1728-2
owners:
  - hisamekms
tags:
  - runtime
  - review
  - provider
related:
  - adr-0027
  - adr-t1063-1
  - adr-t1207-1
  - adr-t451-1
  - adr-t1165-1
  - adr-t728-1
  - adr-t728-2
  - adr-t1453-2
  - plan-review-subagents-spike
  - design-supervisor-lifecycle-review
---

# ADR-t1453-1: dagq.tomlがpathごとに名指すreviewのsubagentを、信頼するmainのcommitから写して親のreview jobの中で実行し、1つのverdictに集約してsupervisorが結果のそろいを検査する

## Context

goal 94は、repository固有の意味の規則（designとコードの対応など）をreviewが確かめる仕組みを、runtimeにその規則を埋め込まずに持つと決めた（人が2026-10-03に承認）。今のrunのreviewは1つのheadless job（Claudeか、`[roles.review]`でCodex）が`review.md`（task・receipt・`base...head`の差分）を読み、`pass` / `revise` / `concern`の1つのverdictを返す（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)決定2、[ADR-t1207-1](2026-09-30-t1207-1-codex-run-review.md)）。jobの出力はデータで、supervisorが適用する（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)、[ADR-t728-2](2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）。

`dagq.toml`の読み手は手書きで、知らないtableとkeyを拒み、配列のtable（`[[...]]`）を読まない。名前付きの小さなtableは`[roles.<role>]`と`[kpi.targets."<KPI>"]`の形がある。providerの能力は[review-subagents-spike](../plans/review-subagents-spike.md)で確かめた。

## Decision

1. **設定。** `dagq.toml`に`[review.subagents.<agent>]`のtableを置き、`paths`（repository root起点のglobの配列。`--paths`・`[e2e] paths`・`[areas]`と同じ規則）を必須の1つのkeyにする。`<agent>`はkebab-caseの名前で、定義のファイル名を決める。`[[review.subagents]]`は今の読み手の形（名前付きのtable）に合わせて採らない。
2. **定義の置き場と形式。** 定義は`.dagq/review-agents/<agent>.md`に置く（task 1460のpathsの候補`.dagq/**`の中）。providerに依らない形式で、frontmatterの`description`と、本文の検査項目と参照先の文書だけを持つ。runtimeがproviderごとの渡し方（Claudeの`--agents`、Codexの役割の設定）に変える。subagentのtoolの許可は定義に書かせず、runtimeがreview jobと同じ読み取りだけに固定する。Claudeの`.claude/agents`は採らない: workerやinboxのsessionがproject agentとして自動で読み込み、worktreeの側の写しが効いてしまい、Codexは読まない。
3. **選び方。** supervisorはtaskの`--paths`でなく、reviewするcommitの実際の差分（`review.md`と同じ`base...head`）のpathをglobに照らし、1つでも当たるagentを選ぶ。renameは旧と新の両方、削除は旧のpathを照らす（validatingのe2eの判定と同じrenameを分けない差分）。選んだagentは集合で、同じagentは1回だけ実行する。選ばれたagentはすべて必須で、任意のagentは持たない。
4. **信頼するsnapshot。** 設定と定義は、run branchのworktreeでもmain checkoutの作業ファイルでもなく、landing branchの着地したcommit（mainのhead）のtreeのblobから、reviewの試行ごとに読み、run dirに写してreviewするcommitとともにjobに渡し、そのmainのcommitと選んだagentを記録する（[ADR-t1165-1](2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)の印をlanding branchのblobから読むのと同じ考え）。workerの変更とmain checkoutの未commitの変更は必須のreviewを消せず変えられず、runが変えた設定と定義は着地した後のreviewから効く。mainの`dagq.toml`が読めない・解釈できない、名指したagentの定義がmainに無いときは、必須の検査が分からないのでpassにしない（決定6の経路）。
5. **親のjobで実行して集約する。** 1つの親のreview jobが基本のreviewと選ばれた全ての必須のsubagentを実行し、全部の終わりを待って、既存の1つのverdictを返す。verdictは必須のagentごとの結果を足して持つ。agentごとの結果は、agentの名前と完了したかに加え、親のverdictと同じ形の判定（`pass` / `revise` / `concern`と理由、`concern`なら推奨（`land` / `send_back`）・確信度・人が要る理由（`scope` / `discard`））を持つ。親は集約したverdictをどのagentの判定より軽くせず、agentの理由・推奨・確信度・人が要る理由を落とさずに集約に含める。
6. **supervisorの検査とpassにしない経路。** supervisorは、選んだ全ての必須のagentの結果がそろい、どれも完了していることを検査する。足りないagentがある・失敗した・名前が合わない結果は、今の読めないverdictと同じに扱い、同じ入力で1回だけやり直し、それでもそろわなければ`review_failed`として手動review（`approve_landing`のask。欠けたagentと理由を載せる）に渡す。どの場合もpassにして着地させない。
7. **適用する行き先はsupervisorが判定ごとに求め、最も重いものにする。** 3値の順位だけでは`concern`の推奨・確信度・人が要る理由が弱められるので（例: agentの`revise`を親が`concern`・`land`・`high`・理由なしで包むと[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定3の経路で自動で着地しうる）、supervisorは親の集約を信じて適用しない。親の判定とagentごとの判定の1つずつを、今の`concern`の判定（ADR-t451-1決定3。推奨が無い・確信度が`low`・`scope`・`discard`・reviseの上限は人へ）と同じ規則で、着地（`pass`と、適用できる`concern`の`land`）・差し戻し（`revise`と、適用できる`concern`の`send_back`）・人の判断（`approve_landing`）のどれかに直し、その中で最も重いもの（着地 < 差し戻し < 人の判断）を適用する。agentが1つでも差し戻しか人の判断を求めれば着地せず、どれかが人の判断を求めれば人に聞く。差し戻すときは、差し戻しを求めた全ての判定の理由をsessionに送り、reviseの上限に1回と数える（上限を超えれば人の判断）。人に聞くときは、`approve_landing`のaskに人の判断を求めた判定ごとのagentの名前・理由・人が要る理由（`scope` / `discard`はどれか1つでもあれば載せる）・確信度を載せる。親の集約がこの行き先より軽ければ、行き先を求めた判定と食い違いを記録する。
8. **providerの扱い。** headlessでもsubagentを使う。Claudeは`-p`の`--agents`でsnapshotの定義を渡し、必須のagentがあるreviewに限り、親の許可に`Agent`を足し、setting sourcesを空にしてworktreeの`.claude/agents`と`.claude/settings.json`を読まない。spikeで確かめたのは、setting sourcesを空にするとworktreeの`.claude/agents`が読まれないことと、親の`--disallowedTools`がsubagentにも効くことだけで、`.claude/settings.json`が読まれないこと・review の`--settings`のdenyが併せて効くことは未確認なので、実装のtaskがtestで確かめる。Codexは`codex exec`の読み取りだけのjobで`multi_agent`の役割としてsnapshotの定義を渡す。spikeで役割とsub-agentのtoolは見えたが、sub-agentが実際に動いたこと・sandboxを継ぐこと・worktreeの`.codex`の設定が効かないことは未確認なので、実装のtaskがtestで確かめる。確かめられるまで、またはproviderが必須のsubagentを動かせないときは、このADRが決める新しい行き先として、起動の前に理由（必須のsubagentを動かせない）を記録してもう一方のproviderで起動する。providerそのものは使えるので控え（`ProviderHold`）にせず、そのproviderの他の役割の job は止めない。切り替え先が無い（`--no-claude`など）ときは待たずにpassにせず決定6の手動reviewに渡す。起動の後のsubagentの失敗は一般の失敗で切り替えず、決定6に従う。黙って必須の検査を省く経路は持たない。
9. **Taskとreceiptとqueueのschemaは変えない。** review jobの入力（snapshotと選んだagent）と出力（verdictのagentごとの結果）と、既存のeventのpayloadへの追加だけで解く。
10. **設定の無いrepositoryと旧バイナリ。** `[review.subagents]`の無いrepositoryのreviewは、資料・prompt・起動の引数・判定を今のまま変えない（goal 90の比較を崩さない）。知らないtableを含む`dagq.toml`は旧バイナリが全体を読めなくなりqueueが止まるので、このrepositoryの`dagq.toml`に足すのは固定バイナリが対応した後にする。

## Amendsの判断

既存のADRはamendsしない。ADR-0027決定2のverdictの3値・reviseの上限・`review_finished`の既存の欄はそのままで、agentごとの結果はverdictとpayloadへの欄の追加（同ADRの「kindは追加だけ、既存の欄は変えない」の範囲）。ADR-t1207-1決定2（Codexの最後のmessageを既存の`ReviewVerdict`として読む）は変えない。決定3・4（providerが使えないときの切り替え・控えと、`--no-claude`の手動review）も変えない: 決定8の切り替えは、providerが使えない（実行ファイル・起動・認証・利用上限）ときの規則を広げるのでなく、必須のsubagentを持つreviewだけに効く別の行き先で、providerを控えず、起動の後の失敗は今までどおり一般の失敗として切り替えない。必須のagentの無いreviewは決定3・4のままである。ADR-t1063-1の役割ごとのproviderも変えない。ADR-t451-1決定3（`concern`は推奨と確信度でruntimeが進め、`scope`・`discard`・`low`・reviseの上限は人へ）も変えない: 決定7はその判定を親とagentの判定の1つずつに当てはめ、最も重い行き先を取るだけで、1つの判定の扱いは同じである。必須のagentの無いreviewは親の判定1つで今のまま進む。

## Alternatives

- **supervisorが独立の複数のreview jobを管理する**: jobごとの時間の上限・retry・providerの切り替え・slot・eventと、verdictの集約をruntimeが持つことになり、基本のreviewの文脈も分かれる。goal 94の合意で退けた。
- **taskの`--paths`で選ぶ**: 宣言は実際の変更と一致せず、`--paths`の無いtaskは何も選べない。workerが宣言を広げても狭めても必須の検査がずれる。
- **worktreeかmain checkoutの作業ファイルの設定・定義を読む**: workerの変更や未commitの変更で自分の必須のreviewを消せる。
- **規則の本文を定義に写す**: 正本が2か所になりずれる。定義は検査項目と開発文書への参照だけを持つ（[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）。
- **`[[review.subagents]]`の配列のtable**: 今の読み手が読まず、名前の重複の検査も別に要る。
- **providerが使えないとき必須の検査を省いてpassにする**: 黙って検査が消える。
- **親の集約したverdictを3値の順位で比べて適用する**: `concern`の推奨・確信度・人が要る理由を比べられず、agentの差し戻しや`scope`・`discard`を親の`concern`の`land`・`high`で包めば自動で着地しうる。supervisorが判定ごとに行き先を求めて最も重いものを取る。

## Consequences

- 設定の読み手、差分からの選び方、snapshot、providerごとの渡し方の実装（runtimeのtask）と、親のjobのpromptと集約・検査の実装（runtimeのtask）が続く。名前・欄・eventの綴りの今の姿は[Review](../design/supervisor-lifecycle/review.md)と[Run environment](../design/supervisor-lifecycle/run-environment.md)が実装と一緒に持つ。
- 必須のagentのあるreviewは時間とtokenが増える。時間の上限との関係は実装のtaskが測る。
- 必須のagentのあるClaudeのreviewはworktreeのsettingsを読まなくなる。必須のagentの無いreview（設定の無いrepositoryと、設定があっても差分がどのagentにも当たらないreview）が今も読みうることは、goal 90の後に別に決める。