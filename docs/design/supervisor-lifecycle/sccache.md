---
id: design-supervisor-lifecycle-sccache
type: design
title: sccacheのserver
status: current
created: 2026-11-08
scope: runtime
related:
  - design-supervisor-lifecycle-run-environment
  - adr-t1215-1
  - adr-t2086-1
---

# sccacheのserver

hostのsccacheのserverはsupervisorがsandboxの外で起動して持ち、runtimeが`[run.env]`を渡すほかのprocessには、sandboxの中でも外でも起動させない（[ADR-t1215-1](../../adr/2026-10-02-t1215-1-supervisor-owns-the-sccache-server.md)、[ADR-t2086-1](../../adr/2026-10-08-t2086-1-no-runtime-process-starts-the-sccache-server.md)）。
clientが起動したserverはclientのsandboxを引き継いで他のrunのbuildも失敗させ、起動の記録も残さないため。
guardの適用とcacheなしのbuildへの退避は`domain::sccache::GuardLook`、supervisorの確認と起動は`supervise::host_ops::sccache`が持つ。
wrapperのturnと人のintegrateとe2eのgateはhostのserverを起動せず、runtimeの外からの起動は下の検知が扱う。

workerのcontainer内のserverとcacheの境界は`domain::sccache::ContainerServer`が持つ。

## 外部serverと失敗の偏り

起動者の照合は`application::sccache::owner`、新しい失敗の偏りとsandboxの診断は`domain::sccache::FailureWatch`、再起動と通知の照合は`application::sccache`が持つ。
親processの付け替えがあるので、sandboxの印を読めないことはsandboxの外の証拠にならない。
古い失敗率や読めない統計だけで壊れたserverと決めず、別のidentityを古い故障の理由で止めない。
再起動の失敗の通知への対処は[運用の手順](../../development/operations.md#sccacheの再起動の失敗)が持つ。
supervisorが置換の起動を記録すると知らせが消え、人が別のserverを起動して検知されただけでは消えない。

監視と再試行の入口は`supervise::host_ops::sccache`で、drain中も監視する。
guardの適用はturnが`application::headless_session`、ほかは環境を組む各所が持つ。
doctorとstatusの読み取りの境界は`application::sccache::add_diagnostics`。
