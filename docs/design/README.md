---
id: design-index
type: design
title: Current design documents
status: current
created: 2026-09-21
tags:
  - architecture
---

# Current design documents

ここは現在の実装を説明する。決定の理由は [ADR](../adr/README.md)、実装順は [plans](../plans/current.md) を参照する。

- [Overview](overview.md)
- [Architecture](architecture.md)（レイヤーとコンテキスト（計画管理・実行と着地・観測と分析・host運用）の2軸の境界。contextの目的、検査できる規則と検査の範囲。ADR-t1545-1）
- [計測](measurement.md)（計測の作り直し。冒頭の「SSOTとビュー」が記録の論理ストアと材料の区分を持ち、計測の層のストアと共有の記録を足すtaskはそこに区分を書く。区間とタグ・台帳の形・畳む関数・台帳を作る係・送る口。ADR-t1662-1〜3、一部だけ実装済みのdraft）
  コマンドの形と分類は[計測のコマンドの形と分類](measurement-commands.md)が持つ。
- [taskとgoalとfindingの属性の記録](attribute-records.md)（stats・kpi・reportが状態の表から読む属性を状態の変更と同じtransactionでeventに書く項目ごとの元・記録より前のtaskとgoalとfindingと古いbinaryの書き込みを直す基準と照合・畳む関数）
- [mainの履歴の記録](main-history.md)（supervisorが記録するmainのfirst-parentの履歴の流れ・commitの時刻と観測したheadと到達時刻の区別・到達点の間隔と猶予・取り込みの範囲・畳む関数と欠測の理由）
- [agentのeval](agent-eval.md)（agentの定義とケースの置き場`.dagq/agents/<name>/`・ケースの欄とpatchの共有・採点と漏れの検査・queue serviceのユースケースとしての依頼と認可・eventだけから作り直す記録・費用の上限・hold-outの1回の制限・runのslotを使わないevalの枠・本番のagentのjobと共有する起動経路・道具の宣言とproviderごとの変換・programのreviewの当て方・本番のreviewから作りqueue側に置くproductionのケースの集合・定義を変えるrunの着地の前の採用の判定。ADR-t1728-1・2、ADR-t1869-1。productionの見張りは予定）
- [Domain model](domain-model.md)
- [Persistence](persistence.md)
- [Follow-up membership judgements](follow-up-membership.md)（所属の分類と必須の欄・acceptanceの版・出どころと深さ・所属の変更と人のadopt・旧schemaの移行）
- [Authorization](authorization.md)
- [Security](security.md)（信頼の区分・actorとcapability・host実行は助言的で隔離ではないこと・containerとqueue serviceへの道筋）
- [Container runtime](container-runtime.md)（workerとintegrateのverificationをhostかcontainerで動かす適用先・使えないときのfail closedの境界・設定の読み込みと「使えるか」のportへの入口・後続の部品の置き場）
- [Toolchain profiles](toolchain-profiles.md)（言語・道具の知識を持つprofileのportと組み込みの登録・`dagq.toml`の宣言と当て方・runtimeの共通の分類との分担・新しいprofileの足し方）
- [Queue broker](queue-broker.md)（containerのrunの唯一の出口になるhostのプロセス。queue serviceの宛てへのrun tokenの検証と転送・拒否の記録・`up`・`down`・supervisorによる起動と停止）
- [Queue service](queue-service.md)（hostで動きqueue DBを開くservice。unix socket・APIのversion・tokenによるprincipalとservice側の認可・ask・show・noteのユースケース・`up`・`down`・supervisorによる起動と停止・落ちたときのattention）
- [Supervisor lifecycle](supervisor-lifecycle.md)（目次。各節は[`supervisor-lifecycle/`](supervisor-lifecycle/)の下の別のファイルにある）
- [Provider lifecycle](provider-lifecycle.md)
- [Executionのトークン数](execution-tokens.md)（非対話のturnとheadlessのjobの1回ごとのトークン数の記録の形・providerごとの数える元・今の穴）
- [Provider executables](provider-executables.md)（providerを起動するpathをsymlinkのまま持つこと、pathが無いときの名前での解決し直し）
- [Plugin integration](plugin-integration.md)
