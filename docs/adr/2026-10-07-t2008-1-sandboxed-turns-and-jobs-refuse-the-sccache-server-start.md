---
id: adr-t2008-1
type: adr
title: sandboxの中のturnとjobはsccacheのserverの起動を拒まれ、serverを確かめたものはguardを通してcompileする（ADR-t1215-1決定2をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t1215-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
  - operations
related:
  - adr-t1215-1
  - adr-0049
  - adr-t813-3
  - design-supervisor-lifecycle-run-environment
---

# ADR-t2008-1: sandboxの中のturnとjobはsccacheのserverの起動を拒まれ、serverを確かめたものはguardを通してcompileする（ADR-t1215-1決定2をamends）

## Context

[ADR-t1215-1](2026-10-02-t1215-1-supervisor-owns-the-sccache-server.md)は、sccacheのserverをsupervisorがsandboxの外で起動して持ち、sandboxの中で走るもの（Codexのworkerのturnとresume、`[run.env]`を受けるCodexのjob）を始める直前にserverを確かめ、確かめられなければ`RUSTC_WRAPPER`を外すと決めた（決定1・2）。確かめた後にserverが止まると（idleの停止・人のstop・確認直後の競合）、turnの中のsccacheのclientはserverと話せず、自分でserverを起動する。そのserverはsandboxを引き継ぎ、ほかのrunのbuildを`Operation not permitted`で失敗させる（goal 79の発端）。ADR-t1215-1はこれを防げない制限として残し、goal review 85が受け入れ(1)の未達とした。

sccache 0.18のsource（`src/commands.rs`の`connect_or_start_server`と`run_server_process`）で確かめた振る舞いは次のとおり。

- clientはserverに接続できないと、自分の実行ファイル（`current_exe`）を`SCCACHE_START_SERVER=1`で起動し直し、その子がserverになる。子はclientの環境を引き継ぐ。名前やPATHを通らないので、PATHの前のshimでも`RUSTC_WRAPPER`の差し替えでもこの経路は止まらない。
- serverになる子は、daemon化とportのbindより前に`SCCACHE_ERROR_LOG`（あれば）を開き、開けなければ終わる。clientはこの変数を読まない。
- 起動に失敗したclientは`sccache: error:`を出して失敗し、rustcに落ちない（`SCCACHE_IGNORE_SERVER_IO_ERROR`は接続の後の入出力の失敗だけに効く）。clientがserverを待つ時間は既定で10秒。
- serverの自動起動を止める設定は無い。client-side modeもまずserverに接続し（無ければ起動し）てから動く。

## Decision

1. **sandboxの中で走るturnとjobには、どのprocessも開けない`SCCACHE_ERROR_LOG`を渡し、sandboxの中のsccacheがserverになれないようにする。** serverを確かめたかどうかに依らず渡す。cargoを通すものも、sessionが直接打つsccacheも、起動し直した子はlistenの前に終わる。supervisorがsandboxの外で起動するserverには渡さない。
2. **serverを確かめたturnとjobには、`RUSTC_WRAPPER`としてsccacheの代わりにguardを渡す。** guardはdagqのbinary（hostに新しいツールを入れない）で、compileごとにserverのportを確かめ、listenしていれば`[run.env]`のsccacheを通してcompileし（serverのcacheを使う）、listenしていなければcompilerを直接実行する。sccacheが自分の失敗（決定1で起動を拒まれた、確認の直後に止まったserver）で終わったときは、その出力を捨ててcompilerを直接実行する。compilerの失敗はそのままcompileの結果にする。buildはcacheなしでも正しく通る。
3. **ADR-t1215-1決定2の開始直前の確認と`RUSTC_WRAPPER`の除去は残す。** 直前にserverを確かめられなかったturnとjob、guardを用意できなかったものは今までどおり`RUSTC_WRAPPER`を外して起動し、外したことをeventに残す（決定1は外したものにも渡す）。jobの前の確認はsupervisorが居ないserverを起動する機会でもある。決定2は「確かめたものは`[run.env]`のsccacheをそのまま使う」から「確かめたものはguardを通して使う」に変わる。
4. **保つもの**: [ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定6（sccacheでrun間のcacheを共有し、`CARGO_TARGET_DIR`を共有しない）と[ADR-t813-3](2026-09-28-t813-3-codex-worker-permissions.md)の決定4（Codexのsandboxはnetworkを開けてsupervisorのserverに接続する）。serverが居る間のturnとjobはguardを通してそのserverに接続し、cacheを使う。人の`~/.codex`とsccacheの設定ファイルは書き換えず、turnとjobの環境だけで行う。

## Alternatives

- **compileごとのportの確認だけ**: 確認とclientの接続の間にserverが止まれば、clientがserverを起動する。決定1が無いとこの競合が残る。
- **PATHの前のsccacheのshimで起動の経路を拒む**: clientはserverを`current_exe`で起動し直すので、shimを通らない。
- **`SCCACHE_NO_DAEMON`・client-side mode**: 前者はdaemon化しないだけで起動を止めず、後者もまずserverを起動する。
- **sandboxの中では常に`RUSTC_WRAPPER`を外す**: ADR-t1215-1のAlternativesのとおり、Codexのrunのcacheの共有を失う。
- **起動を拒むだけで、guardを置かない**: 確認の後にserverが止まったturnのcompileが全部失敗する。

## Consequences

- sandboxの中のprocessは、turnやjobの途中でserverが止まっても、確認の直後の競合でも、serverを起動しない。止まったserverはsupervisorの周回がsandboxの外で起動し直し、guardのturnはそれからcacheに戻る。
- 確認の直後の競合に当たったcompileだけは、clientがserverの起動を待つ分（既定で10秒）遅れてからcompilerで通る。compileごとにguardの起動とportへの接続が1回ずつ増える。
- 決定1はsccache 0.18の起動の順番（serverが最初にerrorのlogを開く）に依る。sccacheを上げるときはこの順番を確かめ直す。
- runtimeの外で人やsessionがsandboxの中から起動したserverは、ADR-t1215-1決定3の検知が扱う。
- guardの名前・変数・置き場所・eventは[Run environment](../design/supervisor-lifecycle/run-environment.md)の「sccacheのserver」とコードのdoc commentに書く。
