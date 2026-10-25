---
id: adr-t3331-1
type: adr
title: Codexのinboxは人のCodexの設定を書かない。TUIが自分で書き足す欄は、起動の引数で止められるものは止め、止められないものはexecの前に人のconfig.tomlを読むだけで確かめ、欠けていれば開かずに人が足す行を出す。dagqもCodexも人の設定を書き換えない（ADR-t1181-1決定2をamends）
status: accepted
created: 2026-10-25
updated: 2026-10-25
accepted_on: 2026-10-25
amends:
  - adr-t1181-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - provider
  - inbox
related:
  - adr-t1181-1
  - adr-t813-3
  - adr-t1063-1
  - plan-codex-inbox-config-writes-spike
  - plan-codex-inbox-hook-trust-spike
  - design-provider-lifecycle
  - design-supervisor-lifecycle-up-down
---

# ADR-t3331-1: Codexのinboxは人のCodexの設定を書かず、書き足されるなら開かない（ADR-t1181-1決定2をamends）

## Context

[ADR-t1181-1](2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md)決定2は、Codexのinboxの足りない機能を起動の引数と指示で埋め、人の`~/.codex`の設定と`CODEX_HOME`を書き換えないとした。Consequencesは、TUIの起動が人の`config.toml`を書くかを実装のtaskが確かめるとした。

測定（[codex-inbox-config-writes-spike](../plans/codex-inbox-config-writes-spike.md)、codex-cli 0.160.0）で分かったこと:

- 読み上げの検出の済みの印は、人の`config.toml`（Userの層）に欄が無いと、TUIが最初の起動で書き足す。書くかはUserの層だけを見て決めるので、起動の引数では止まらない。
- modelの案内の表示回数は、sourceでは案内を出さない設定の起動の引数が書く前の判断で返すので止まる見込み（loginした起動では確かめていない）。表示回数そのものを起動の引数で渡す形は、名指していないmodelの案内で引数の値を含めた表ごと人のfileに書かれるので、止める手段にならない。

人はask 1204で、dagqもCodexも人のCodexの設定を勝手に変えないと決めた。ask 1184で人が認めたのは、dagqが起動の引数で渡すhookのtrustの扱い（その起動だけのtrust）だけで、TUIが人のfileに書き足すことを許すものではない。

## Decision

1. **dagqもCodexも人のCodexの設定を書き換えない。** Codexのinboxの起動は、TUIが自分で人の`config.toml`に書き足す欄を次のどちらかで防ぐ。どちらにもできない欄が分かれば、Codexのinboxを開かない側に倒す。
   - 起動の引数で書き足しを止められる欄は、引数で止める。止まらない引数（書き足しを止めない、または値を人のfileに書かせる引数）は足さない。
   - 起動の引数で止められない欄は、execの前にdagqが人の`config.toml`（`CODEX_HOME`があればその下、無ければ`~/.codex`）を読むだけで確かめる。欄が無い・fileが無い・読めないときはCodexを開かず、理由・fileの場所・人が自分で足す行を人のterminalに出して0でなく終わる。Claudeに替えず、askを開かない（ADR-t1181-1決定3と同じ出方）。dagqは人のfileを書かない。
2. **版が上がって新しい自動の書き足しが分かったら、止める引数か確かめる欄の一覧に足す。** どちらにもできなければ、Codexのinboxを開かない側に倒す。一覧は起動の引数と確かめを作る定義のそばに置く（[provider-lifecycle](../design/provider-lifecycle.md)のinboxの起動の節が入口を持つ）。
3. **人がTUIで自分で選ぶと人の設定に書かれる操作は、起動を断る対象にしない。** hookのtrustの画面の「Trust all」や、loginした後のmodelの移行の案内の選択は人の操作で、dagqやCodexが勝手に書くものではない（ADR-t1181-1のConsequencesがtrustで取る扱いと同じ）。人にはそれを選ばないか、書かれると知って選ぶよう、skillが知らせる。

ADR-t1181-1決定2のそれ以外（見張り・askの提示・指示・復帰と区間とStopのhook・trustをdagqのhookだけに限ること・guardrail・画面の判定を持たないこと・sandboxからの状態を変えるdagq・権限と記録）は変えない。

## Alternatives

- **TUIの書き足しを許し、文書に書くだけにする**: goal 77の制約（人の`~/.codex`の設定を書き換えない）に反し、ask 1204で人が退けた。
- **dagqが人のfileに欄を書く**: 書くのがCodexからdagqに替わるだけで、人の設定を書き換える。
- **人の`CODEX_HOME`にprofileのfileを置いて起動の引数でそれを選ぶ**: Userの層に足すので書き足しを避けうるが、fileは人の`CODEX_HOME`の中に置くしかなく、人の設定を書き換える。
- **scratchの`CODEX_HOME`に人の`auth.json`を写して開く**: 人のloginを写して分けることになり、tokenの更新で人のloginを壊しうる。

## Consequences

- 人の`config.toml`に読み上げの検出の済みの印が無いhostでは、Codexのinboxは開かず、人が1度その行を足すまで待つ。今のhostの人の設定には欄がある。
- 止める引数と確かめる欄は、Codexの版で変わりうる。版が上がって書き足しが変わったかは、手動スモークの起動の前後の人の`config.toml`の更新時刻と中身の確かめで見る。
- loginした起動で案内を出さない設定がmodelの案内の表示回数の書き足しを止めるかは、手動スモークで確かめる。止まらなければ、決定2に従い確かめる欄に移すか開かない側に倒す。
