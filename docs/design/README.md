---
id: design-index
type: design
title: Current design documents
status: current
created: 2026-09-21
updated: 2026-10-02
last_verified: 2026-10-02
tags:
  - architecture
---

# Current design documents

ここは現在の実装を説明する。決定の理由は [ADR](../adr/README.md)、実装順は [plans](../plans/current.md) を参照する。

- [Overview](overview.md)
- [Domain model](domain-model.md)
- [Persistence](persistence.md)
- [Authorization](authorization.md)
- [Security](security.md)（信頼の区分・actorとcapability・host実行は助言的で隔離ではないこと・Podmanとqueue serviceへの道筋）
- [Queue service](queue-service.md)（hostで動きqueue DBを開くservice。unix socket・APIのversion・tokenによるprincipalとservice側の認可・ask・show・noteのユースケース・`up`・`down`・supervisorによる起動と停止・落ちたときのattention。goal 82）
- [Resource broker](broker.md)（fs・process・gitを仲介するdagq-broker。crateと配布・transport・runごとのtoken・mountと閉じ込め・containerとPodman machine・workerのMCPの道具・audit・mode。goal 58で実装中のdraft）
- [Supervisor lifecycle](supervisor-lifecycle.md)（目次。各節は[`supervisor-lifecycle/`](supervisor-lifecycle/)の下の別のファイルにある）
- [Provider lifecycle](provider-lifecycle.md)
- [Plugin integration](plugin-integration.md)
- [Manual smoke](manual-smoke.md)
- [Stress CI](stress-ci.md)（mainで直近に足した・変えたtestをGitHub Actionsの定時実行で繰り返し、落ちたらflaky-testのissueで知らせる）
- [Slow tests](slow-tests.md)（nextestの出力から遅いtestの上位と1秒・5秒・30秒を超えた本数と合計を出すscripts/slow-tests.sh。CIのjob summaryにも出す）
- [Linux CI](linux-ci.md)（CIのubuntuのjobでcargo buildと全体のtestを流し、落ちたtestの名前をjob summaryに出す。macOSに固有のtestを分けるまではcontinue-on-errorで失敗を通す。goal 83）
