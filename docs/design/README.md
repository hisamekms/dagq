---
id: design-index
type: design
title: Current design documents
status: current
created: 2026-09-21
updated: 2026-10-05
last_verified: 2026-10-04
tags:
  - architecture
---

# Current design documents

ここは現在の実装を説明する。決定の理由は [ADR](../adr/README.md)、実装順は [plans](../plans/current.md) を参照する。

- [Overview](overview.md)
- [Architecture](architecture.md)（レイヤーとコンテキスト（計画管理・実行と着地・観測と分析・host運用）の2軸の境界。contextごとの所有する状態・判断・操作・公開するport・依存の向き、境界をまたぐtransactionの一覧、検査できる規則と今の違反。ADR-t1545-1）
- [計測](measurement.md)（計測の作り直しの予定。冒頭の「SSOTとビュー」が記録の論理ストアと材料の区分を持ち、新しいストアやビューを足すtaskはそこに区分を書く。区間とタグ・台帳の形・畳む関数・台帳を作る係・送る口・コマンドの形と分類。ADR-t1662-1〜3、まだ実装は無いdraft）
- [Domain model](domain-model.md)
- [Persistence](persistence.md)
- [Follow-up membership judgements](follow-up-membership.md)（所属の分類と必須の欄・acceptanceの版・出どころと深さ・所属の変更と人のadopt・旧schemaの移行）
- [Authorization](authorization.md)
- [Security](security.md)（信頼の区分・actorとcapability・host実行は助言的で隔離ではないこと・Podmanとqueue serviceへの道筋）
- [Queue service](queue-service.md)（hostで動きqueue DBを開くservice。unix socket・APIのversion・tokenによるprincipalとservice側の認可・ask・show・noteのユースケース・`up`・`down`・supervisorによる起動と停止・落ちたときのattention。goal 82）
- [Resource broker](broker.md)（fs・process・gitを仲介するdagq-broker。crateと配布・transport・runごとのtoken・mountと閉じ込め・containerとPodman machine・workerのMCPの道具・audit・mode。goal 58で実装中のdraft）
- [Supervisor lifecycle](supervisor-lifecycle.md)（目次。各節は[`supervisor-lifecycle/`](supervisor-lifecycle/)の下の別のファイルにある）
- [Provider lifecycle](provider-lifecycle.md)
- [Plugin integration](plugin-integration.md)
- [Manual smoke](manual-smoke.md)
- [Stress CI](stress-ci.md)（mainで直近に足した・変えたtestをGitHub Actionsの定時実行で繰り返し、落ちたらflaky-testのissueで知らせる）
- [CI failure issues](ci-failure-issues.md)（mainへのpushのCI（ci.yml）が落ちたらci-failureのissueを開くか追記し、次に通ったら閉じる。workflow全体の結論で決め、continue-on-errorのjobの失敗だけでは開かない）
- [Slow tests](slow-tests.md)（nextestの出力から遅いtestの上位と1秒・5秒・30秒を超えた本数と合計を出すscripts/slow-tests.sh。CIのjob summaryにも出す。差分で足した・変えたtests/itのtestの時間の関門scripts/check-it-test-time.shと許可の一覧.config/it-slow-allow.toml）
- [Linux CI](linux-ci.md)（CIのubuntuのjobでcargo buildと全体のtestを流し、落ちたtestの名前をjob summaryに出す。macOSに固有のtestを分けるまではcontinue-on-errorで失敗を通す。goal 83）
