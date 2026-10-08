---
id: adr-t2114-4
type: adr
title: --network noneのcontainerのsccacheのserverはsupervisorがcontainerの中に起動し、guardはcontainerの中のloopbackを確かめ、worker・resumeが書くcacheとintegrateのverification・hostのserverが信頼するcacheを分ける（ADR-t2086-1決定3・5、ADR-t1215-1決定1、ADR-0049決定6、ADR-t2113-3決定1、ADR-t813-3決定4をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t2086-1 decision 3
  - adr-t2086-1 decision 5
  - adr-t1215-1 decision 1
  - adr-0049 decision 6
  - adr-t2113-3 decision 1
  - adr-t813-3 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - security
  - performance
related:
  - adr-0049
  - adr-t1215-1
  - adr-t2086-1
  - adr-t813-3
  - adr-t2113-3
  - adr-t2114-1
  - adr-t2114-3
  - design-supervisor-lifecycle-run-environment
---

# ADR-t2114-4: containerのsccacheのserverはcontainerの中に置き、cacheを書き手の信頼で分ける

## Context

worker・resumeとintegrateのverificationは`--network none`のcontainerで動き、外への経路はqueueのbrokerの3つの宛先だけである（[ADR-t2114-1](2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)・[ADR-t2114-3](2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)）。
今のsccacheのserverはhostに1つで、supervisorだけが起動し（[ADR-t2086-1](2026-10-08-t2086-1-no-runtime-process-starts-the-sccache-server.md)）、`[run.env]`を受けるprocessはguardがそのserverのportを確かめてcompileする。
sccacheのserverは、clientから受けたcompileを自分のfilesystemの上で自分で実行する。
hostのserverにcontainerのclientをつなぐと、containerの中のpathがhostで合わず、信頼しないコード（build.rs・proc macro）のcompileをhostで走らせて隔離を破る。
cacheのdirに書けるprocessは、任意のentryを置いて後で読む者のcompileの結果を汚せる。

## Decision

1. **serverの置き場はrunのcontainerの中で、起動するのはsupervisorだけ。**
   supervisorは必要なときにserverを起動する: containerを起動した後、agentと検証コマンドより前に、containerの中でserverを起動する（`podman exec`、idleで止まらない設定、起動を拒む`SCCACHE_ERROR_LOG`を渡さない環境）。
   起動と失敗をeventに残す。
   serverはcontainerと一緒に終わる。
   ADR-t2086-1決定4の、起動の前の確認が居ないserverを起動する機会でもあること（supervisorの確認は起動も兼ねる）は、containerの中のserverにも当てはまる。
2. **clientからの経路はcontainerの中のloopbackとguard。**
   containerの中で`[run.env]`を受けるprocessには、ADR-t2086-1決定2の開けない`SCCACHE_ERROR_LOG`を渡してserverの起動を拒み、決定3のguardを`RUSTC_WRAPPER`に渡す。
   guardが確かめるportはcontainerの中のloopbackである。
   `--network none`でもloopbackは在るので、外への経路もbrokerの宛先も増えない。
3. **届かないときはcacheなしでcompileする。**
   supervisorがserverを起動できなかった・確かめられなかったときは、ADR-t2086-1決定4のとおり`RUSTC_WRAPPER`を外して起動し、eventに残す。
   serverが途中で止まれば、guardがcompilerを直接実行する（cacheなしで正しく通る）。
4. **cacheを書き手の信頼で分ける。**
   - worker・resumeのcontainerは信頼しないコード（build.rs・proc macro・test）を走らせ、それがmountされたcacheのdirに任意のentryを書ける。
     そのcacheを「workerのcache」（信頼しない）とし、worker・resumeのcontainerどうしだけが共有する。
   - hostのsupervisorのserverのcache（hostのsandboxのheadlessのjobが使う）を「信頼するcache」とし、どのcontainerにもmountしない（読み取り専用でもmountしない）。
   - integrateのverificationは、workerのcacheを読み取り専用でもmountしない。
     verificationは保護されたcache（workerのcontainerも、verification自身の検証コマンドが走らせるコードも書けず、verificationのserverだけが書けるもの）を用意できるときだけそれを使う。
     用意できない間は`RUSTC_WRAPPER`を外してcacheなしでcompileし、外したことをeventに残す（ADR-t2086-1決定4と同じ扱い）。
   - 保護されたcacheの形（例: serverを別のuidか別のmount namespaceに置き、検証コマンドのprocessからcacheのdirが見えないようにする）と、cacheなしのverificationのcompileの時間の増え方は、実装と実測の段で確かめる。
     このADRが決めるのは境界と「用意できなければcacheなし」の方針だけである。
5. **workerのcacheの同時の書き込みと大きさ。**
   複数のworkerのcontainerのserverが同じworkerのcacheのdirを同時に使う。
   sccacheのlocalのdisk cacheが複数のserverからの同時の書き込みと大きさの上限の管理に耐えるかは、実装の段で確かめる。
   耐えないときは、workerのcacheをrunごとに分けて種から複写する形か、cacheをworker・resumeの側の1つのserverに寄せる形（そのserverもworkerのcacheと同じく信頼しない）に替え、替えたことをこのADRのamendsで残す。
6. **汚された影響の範囲。**
   workerのcacheが汚されても、影響はworker・resumeのrunに留まる。
   着地するものは、cacheを使わないか保護されたcacheだけを使うverificationを通る。
7. **変えないもの。**
   containerは`--network none`のままで、brokerの宛先は3つのまま（sccacheを宛先に足さない）。
   hostのOSのsandboxで動くheadlessのjob（Codexのreviewなど。ADR-t2113-3決定2）は、今までどおりhostのsupervisorのserverを使う（ADR-t2086-1決定5のhostの適用先は変えない）。

## 既存の決定との関係

- [ADR-t2086-1](2026-10-08-t2086-1-no-runtime-process-starts-the-sccache-server.md)決定3: containerの中では、guardが確かめるserverはhostのsupervisorのserverではなくcontainerの中のserverになる。
- ADR-t2086-1決定5: 「どの適用先もsupervisorのserverのcacheを使う」は、containerの適用先では決定4の分け方（workerのcacheか保護されたcacheかcacheなし）に変わる。
- [ADR-t1215-1](2026-10-02-t1215-1-supervisor-owns-the-sccache-server.md)決定1: 「起動するのはsupervisorだけ」を保ち、serverの置き場をsandboxの外のhostからcontainerの中にも広げる。
- [ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定6: run間のcacheの共有をworker・resumeの間に限り、integrateのverificationを共有から外す。
- [ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1: containerに見せるのはworktreeとrun dirだけ、の例外に、worker・resumeのcontainerへのworkerのcacheのdirと、verificationの保護されたcacheを足す。
- [ADR-t813-3](2026-09-28-t813-3-codex-worker-permissions.md)決定4: containerのCodexのworkerは、hostのserverへnetworkを開けず、containerの中のserverを使い、registryへはbrokerを通る。

元のADRの本文は書き換えない。

## Alternatives

- **hostのserverへunix socketのrelayでつなぐ**: serverはcompileを自分のfilesystemで実行するので、containerのpathがhostで合わず、信頼しないコードのcompileをhostで走らせて隔離を破る。
- **integrateのverificationにworkerの共有のcacheを読み取り専用でmountして汚染の対策とする**: 読み取り専用は新しい書き込みを止めるだけで、workerが既に汚したcompileの結果をverificationが読む経路が残る。
  着地するcommitの検証が信頼しない書き手の結果に依るので、汚染を防がない。
- **hostの信頼するcacheをcontainerにmountする**: 書き込みを許せばhostのserverの結果を汚せる。
  読み取り専用でも、信頼しないコードがhostのcacheの中身を読め、どのcacheを誰が信頼するかの境界が曖昧になる。
- **containerでは常に`RUSTC_WRAPPER`を外す**: run間のcacheの共有（ADR-0049決定6）を失い、workerのbuildが毎回cacheなしになる。
- **containerのclientにserverを自動で起動させる**: serverの起動者をsupervisorだけにする決定（ADR-t2086-1）を破り、起動の記録が残らない。

## Consequences

- containerのrunのcompileはcontainerの中で閉じ、hostのserverとcacheは信頼しないコードから切り離される。
- 保護されたcacheを用意するまで、integrateのverificationはcacheなしでcompileし、着地の時間が延びる。
  延びる量は実測で確かめ、保護されたcacheの形を決める後続の判断の材料にする。
- containerごとにserverのprocessが1つ増え、supervisorはcontainerの中のserverの起動とeventを持つ。
- 実装と実測（compileの時間・cacheの当たり率・同時の書き込み）は後続に回す。
  名前・設定・eventは[Run environment](../design/supervisor-lifecycle/run-environment.md)の「sccacheのserver」とコードに書く。
