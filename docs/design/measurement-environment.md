---
id: design-measurement-environment
type: design
title: 計測の実行環境と資源（環境の口・資源の単位・environment_attached・blocker infra・台帳との順序・適合test）
status: current
created: 2026-10-21
scope: runtime
related:
  - design-measurement
  - adr-t1662-1
  - adr-t1662-3
  - design-persistence
  - design-supervisor-lifecycle-host-metrics
  - design-measurement-otel
  - development-testing
---

# 計測の実行環境と資源

runが動く環境と、runとnodeの資源の記録で、[計測](measurement.md)の一部。
決めた理由はADR-t1662-1（決定8と(ii)）とADR-t1662-3（決定1〜6）が持つ。

## 環境の口

- portは`application::ports`の`ExecutionEnvironment`で、環境を名乗る・runとnodeの資源を外から測る・外から測れるか・環境が足す待ちを返す。
- 今のアダプタはhostだけ（`infrastructure::environment::HostEnvironment`）。
  containerやk8sのアダプタは同じportに差し込み、hostと同じ適合test一式（下の「適合test」）を通す。
- hostは、runが登録したプロセス（wrapperとagent）とその下の全てのプロセスを`ps`で外から測る。
- supervisorがrunのためにworkerの木の外で起こすプロセス（reviewと復旧のheadless job、着地の検証とrunを入れたrecheckのコマンド）も、動いている間はそのrunのもの（`domain::environment::RunChildren`）として、その下のプロセスごと同じ窓に数える。
  main自身のrecheckはどのrunのものでもない。
  2つの測りの間に始まって終わったプロセスは見えず、数えない。
  2つの窓の間に終わったプロセスがその間に使った分は数えない。
- hostは、窓より前に始まっていたプロセス（引き取ったrunのもの）を最初の測りから数え、前のsupervisorの窓と二重に数えない。
- 外から測れない環境はrunの値を返さず、executorが中から測ってTelemetrySinkで送る（[送る口](measurement.md#送る口)）。
  台帳は、外から測った値の無い区間でだけexecutorの値を使う（下の「台帳との順序」）。

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
  区間ごとに外から測った値を使い、外から測った値の無い区間だけexecutorが中から測った値を使い、区間と行はどちらの値かを持つ。
  畳んだ中からのsampleとそのsessionの始まりは行に残し、次に畳むときも数えるので、保持の期間で消えたsampleの分も失わず、持ち主が消えたsessionの遅れたsampleも置ける。
  stepと同じく、保持は行がまだ持たない（書き待ちの）runの中からのsampleを消さない。
- runは最後の名乗りの最後の窓が記録された後にだけ確定する（`environment::resources_recorded`）。
  名乗りの無いrunは待たない。

## blocker: infra

- 環境が足す待ち（配置・imageのpull・用意・結果の戻し）は工程`Phase::Environment`（`blocker: infra`）で記録する。
- 環境の口の`waits`がその環境の待ちを返し、hostには無いので今は書かない。

## 適合test

環境の口と送る口の差し替えを、どのアダプタにも同じ約束を確かめる2つの一式で守る（ADR-t1662-3決定3・6）。
一式はアダプタを引数に取る関数で、hostのアダプタが通る。
containerやk8sのアダプタはまだ無く、新しいアダプタは一式を通れば差し込める。

- **環境の一式**: `infrastructure::environment::conformance`の`conforms`で、アダプタのtestが値で動かすプロセスとnodeのsample（`Fixture`）を渡し、OSには触れない。
  確かめるのは次のとおり。
  - 名乗り: 毎回同じで、`environment_attached`の欄を全て持ち（無い値は`null`）、nodeとarchが空でなく、上限は正の値、imageのある環境はdigestを持ち、payloadが読み戻せる。
  - 待ち: 足す待ちはそれぞれ別の工程で、`run_phase_changed`に`blocker: infra`で記録され、その工程として読み戻せる。
  - nodeの資源: CPU秒はsampleの忙しいcoreにsampleが代表する時間を掛けたもので、上限は名乗りの値、どの値も共通の単位で負にならない。
  - runの資源: 窓ごとにrunのプロセスの木が前の窓から使ったCPUと握るメモリを、他のrunと誰のものでもないプロセスと分けて返し、プロセスの無いrunは返さず、忘れたrunの前の分を数え直さない。
    外から測れない環境はどのrunも返さない（executorが中から測る）。
- **送る口の一式**: `tests/it`の`telemetry_conformance`の`conforms`で、届け方のtestが対象のtokenを持つsinkと届かないsink（`Delivery`）を渡し、queueのストアを読んで確かめる。
  確かめるのは次のとおり。
  - stepの重複: 同じ報告を2度送っても`(session_id, seq)`ごとに1つで、2度目は既にあると答える。
  - stepの抜け: 届かなかったseqはsessionの抜けとして見え（推して埋めない）、そのstepが後で届けば抜けでなくなる。
  - 送り直し: 届かなかった分と、届いたのに答えが失われた分は、executorが失敗にせず持ち続けて次に送り、ストアは重複として1つに保つ。
  - 中から測った資源: 外から測れない環境でexecutorが送るsampleは、送ったとおりの共通の単位で1つずつ入る。
  - APIの版: serviceが読まない版（新しいものも古いものも）の報告は拒まれて何も残さない。
- **差し込む手順**:
  1. アダプタは`ExecutionEnvironment`を実装し、そのtestが`Fixture`を作って環境の一式を通す。
  2. 外から測れないアダプタは、executorが中から測った資源を`TelemetrySink`で送る。
     届け方を変えるとき（unix socketの代わりのbrokerなど）は、その`TelemetrySink`と`Delivery`を作って送る口の一式を通す。
  3. 組み立て（`compose::supervisor`がhostのアダプタを作る所）でアダプタを差し替える。
     supervisor・台帳・ストアは変えない。
- 記録とOpenTelemetryとの対応は[計測のOpenTelemetryとの対応](measurement-otel.md)が持つ。
