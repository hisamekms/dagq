---
id: design-measurement-environment
type: design
title: 計測の実行環境と資源（環境の口・資源の単位・environment_attached・blocker infra・台帳との順序）
status: current
created: 2026-10-21
scope: runtime
related:
  - design-measurement
  - adr-t1662-1
  - adr-t1662-3
  - design-persistence
  - design-supervisor-lifecycle-host-metrics
---

# 計測の実行環境と資源

runが動く環境と、runとnodeの資源の記録で、[計測](measurement.md)の一部。
決めた理由はADR-t1662-1（決定8と(ii)）とADR-t1662-3（決定1〜6）が持つ。

## 環境の口

- portは`application::ports`の`ExecutionEnvironment`で、環境を名乗る・runとnodeの資源を外から測る・外から測れるか・環境が足す待ちを返す。
- 今のアダプタはhostだけ（`infrastructure::environment::HostEnvironment`）。
  containerやk8sのアダプタは同じportに差し込み、hostと同じ適合test一式（`infrastructure::environment::conformance`）を通す。
  一式は、アダプタのtestが値で動かすプロセスとnodeのsampleで、名乗り・待ち・nodeの資源・runの窓ごとの資源の約束を確かめ、OSには触れない。
- hostは、runが登録したプロセス（wrapperとagent）とその下の全てのプロセスを`ps`で外から測る。
- supervisorがrunのためにworkerの木の外で起こすプロセス（reviewと復旧のheadless job、着地の検証とrunを入れたrecheckのコマンド）も、動いている間はそのrunのもの（`domain::environment::RunChildren`）として、その下のプロセスごと同じ窓に数える。
  main自身のrecheckはどのrunのものでもない。
  2つの測りの間に始まって終わったプロセスは見えず、数えない。
  2つの窓の間に終わったプロセスがその間に使った分は数えない。
- hostは、窓より前に始まっていたプロセス（引き取ったrunのもの）を最初の測りから数え、前のsupervisorの窓と二重に数えない。
- 外から測れない環境はrunの値を返さず、executorが中から測ってTelemetrySinkで送る（[送る口](measurement.md#送る口)）。
  台帳は、外から測った値の無いrunでだけexecutorの値を使う。

## environment_attached

- supervisorはrunのclaim・resume・引き取りのたびに、環境の名乗りを`environment_attached`としてrunのeventに書く（実行と着地の種類）。
  欄は`domain::environment::EnvironmentAttached`のdoc commentが持つ。
- `node_id`はNodeSampleStoreの同じnodeの行のもので、hostは`host`。
- 書けなかった名乗りはwarnにし、runはそのまま進み、次の始まりまで資源を測らない。

## 資源の単位

- 共通の単位は`domain::environment::Resources`のdoc commentが持つ（CPU秒・上限・メモリの最大・throttled秒・PSI、load averageは任意）。
- 環境ごとの単位を台帳とストアに持ち込まない。

## runの資源

- 観測と分析の周回が間隔ごとにjobのthreadで、slotのrunのうち名乗りのあるものを測り、`run_resources`に窓を1行ずつ、最後の名乗りの下に書く（`supervise::observation::resources`）。
- runがslotを離れた（sessionの終わり・休む・終わり）後の周回が、最後の窓（`last`）を書いて名乗りを閉じる。
  新しい名乗りは前の名乗りを閉じ、閉じた名乗りの下ではもう測らない。
  測り直す周回（測りを失ったsupervisor）は、その名乗りの最後の窓の終わりから測る。
- 測っていたsupervisorが止まって開いたままの名乗りは、生きたleaseが握っていなければ、どのsupervisorの周回も値の無い最後の窓で閉じる。
- 測れない・書けない周回はwarnにし、runの結果を変えない。

## nodeの資源

- hostの負荷の記録（[hostの負荷](supervisor-lifecycle/host-metrics.md)）は、同じsampleを環境の口で共通の単位にして、NodeSampleStoreの同じ行に書く。
  CSVから取り込んだ行は持たない。
- sampleのCPU秒は、そのsampleが代表する時間（nodeの1つ前の行からの時間。どのsupervisorの行でもよい）に掛けて作る。
  遅れて取ったsampleは遅れた分を、2つのsupervisorが交互に取ったsampleはそれぞれの間を数え、誰も記録しなかった間は間隔の2倍までにする（hostの要約のCPU秒と同じ規則）。
  1つ前の行はsampleを入れる書き込みと同じtransactionで読み、後の行が先に書かれていたsampleは時間を代表しない（後の行がその間を数えた）。
- `stats`・`kpi`・reportのhostの値はこの列を読まず、今の列とCSVの写しから作るので変わらない。

## 台帳との順序

- 台帳を作る係は`run_resources`と`session_resources`の記録した順のidのカーソルを持ち、カーソルの後の窓のrunに印を付ける。
  行の`inputs_through`より後の窓は行をdirtyにし、`final`の後でも畳み直す。
- runの行は区間ごとに資源を畳む（`domain::environment::fold`）。
  外から測った値が無いときだけ、executorが中から測った値を使う。
  畳んだ中からのsampleは行に残し、次に畳むときも数えるので、保持の期間で消えたsampleの分も失わない。
  stepと同じく、保持は行がまだ持たない（書き待ちの）runの中からのsampleを消さない。
- runは最後の名乗りの最後の窓が記録された後にだけ確定する（`environment::resources_recorded`）。
  名乗りの無いrunは待たない。

## blocker: infra

- 環境が足す待ち（配置・imageのpull・用意・結果の戻し）は工程`Phase::Environment`（`blocker: infra`）で記録する。
- 環境の口の`waits`がその環境の待ちを返し、hostには無いので今は書かない。
