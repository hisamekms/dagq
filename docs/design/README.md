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
- [Architecture](architecture.md)（レイヤーとコンテキスト（計画管理・実行と着地・観測と分析・host運用）の2軸の境界。contextごとの所有する状態・判断・操作・公開するport・依存の向き、境界をまたぐtransactionの一覧、検査できる規則と今の違反。ADR-t1545-1）
- [計測](measurement.md)（計測の作り直しの予定。冒頭の「SSOTとビュー」が記録の論理ストアと材料の区分を持ち、計測の層のストアと共有の記録を足すtaskはそこに区分を書く。区間とタグ・台帳の形・畳む関数・台帳を作る係・送る口・コマンドの形と分類。ADR-t1662-1〜3、まだ実装は無いdraft）
- [agentのeval](agent-eval.md)（agentの定義とケースの置き場`.dagq/agents/<name>/`・ケースの欄とpatchの共有・採点と漏れの検査・`dagq agent eval`のCLIとevent・費用の上限と既定値・runのslotを使わないevalの枠・着地の前の採用の判定・productionのケースと見張り・道具の宣言とproviderごとの変換・本番のagentのjobと共有する起動経路・programのreviewの当て方。ADR-t1728-1・2。実装したのは定義のpathと`dagq doctor`の`agents`の検査、道具の宣言の検査とproviderごとの変換の関数、ケースの読み手と採点と漏れの検査の関数だけで、残りはdraft）
- [Domain model](domain-model.md)
- [Persistence](persistence.md)
- [Follow-up membership judgements](follow-up-membership.md)（所属の分類と必須の欄・acceptanceの版・出どころと深さ・所属の変更と人のadopt・旧schemaの移行）
- [Authorization](authorization.md)
- [Security](security.md)（信頼の区分・actorとcapability・host実行は助言的で隔離ではないこと・Podmanとqueue serviceへの道筋）
- [Queue service](queue-service.md)（hostで動きqueue DBを開くservice。unix socket・APIのversion・tokenによるprincipalとservice側の認可・ask・show・noteのユースケース・`up`・`down`・supervisorによる起動と停止・落ちたときのattention。goal 82）
- [Supervisor lifecycle](supervisor-lifecycle.md)（目次。各節は[`supervisor-lifecycle/`](supervisor-lifecycle/)の下の別のファイルにある）
- [Provider lifecycle](provider-lifecycle.md)
- [Executionのトークン数](execution-tokens.md)（非対話のturnとheadlessのjobの1回ごとのトークン数の記録の形・providerごとの数える元・今の穴）
- [Provider executables](provider-executables.md)（providerを起動するpathをsymlinkのまま持つこと、pathが無いときの名前での解決し直し）
- [Plugin integration](plugin-integration.md)
- [Manual smoke](manual-smoke.md)
- [Stress CI](stress-ci.md)（mainで直近に足した・変えたtestをGitHub Actionsの定時実行で繰り返し、落ちたらflaky-testのissueで知らせる）
- [CI failure issues](ci-failure-issues.md)（mainへのpushのCI（ci.yml）が落ちたらci-failureのissueを開くか追記し、次に通ったら閉じる。workflow全体の結論で決め、continue-on-errorのjobの失敗だけでは開かない）
- [Slow tests](slow-tests.md)（nextestの出力から遅いtestの上位と1秒・5秒・30秒を超えた本数と合計とtest binaryごとの本数と合計を出すscripts/slow-tests.sh。CIのjob summaryにも出す。差分で足した・変えたtests/itのtestの時間の関門scripts/check-it-test-time.shと許可の一覧.config/it-slow-allow.toml）
- [Linux CI](linux-ci.md)（CIのubuntuのjobでcargo buildと全体のtestを流し、落ちたtestの名前をjob summaryに出す。失敗を通さず、task 1238で扱ったLinuxの失敗とmacOSに固有として分けたtestを挙げる。goal 83）
- [Test fixture templates](test-fixtures.md)（tests/common/template.rsのqueue・repository・stubのtemplateの作り方（targetの一時ディレクトリ・file lockとrename・FORMAT・完成の印）と、CIのrust-cacheが残した中身の無いtemplateでmainのCIが赤くなった原因と直し方。task 1886）
