---
id: adr-t2086-1
type: adr
title: runtimeが[run.env]を渡すすべてのprocessはsccacheのserverの起動を拒まれ、serverを確かめたものはguardを通してcompileし、serverを起動するのはsupervisorだけにする（ADR-t2008-1を置き換え、ADR-t1215-1決定1・2をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
supersedes:
  - adr-t2008-1
amends:
  - adr-t1215-1 decision 1
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
  - adr-t2008-1
  - adr-0049
  - adr-t813-3
  - design-supervisor-lifecycle-run-environment
---

# ADR-t2086-1: runtimeが[run.env]を渡すすべてのprocessはsccacheのserverの起動を拒まれ、serverを確かめたものはguardを通してcompileし、serverを起動するのはsupervisorだけにする（ADR-t2008-1を置き換え、ADR-t1215-1決定1・2をamends）

## Context

[ADR-t1215-1](2026-10-02-t1215-1-supervisor-owns-the-sccache-server.md)はsccacheのserverをsupervisorがsandboxの外で起動して持つと決め、[ADR-t2008-1](2026-10-07-t2008-1-sandboxed-turns-and-jobs-refuse-the-sccache-server-start.md)はsandboxの中のCodexのturn・resume・jobにだけ、serverの起動の拒否（開けない`SCCACHE_ERROR_LOG`）とguardの`RUSTC_WRAPPER`を渡した。ADR-t1215-1決定1は、Claudeのworker・`integrate`の検証・着地前の再確認・自動更新のe2eを「sandboxの外なので今までどおり」とした。

そのためこれらは素のsccacheを使い、serverが止まった直後に最初にcompileしたclientがserverを自動で起動する（idle_timeoutの指定なし、supervisorの記録なし）。2026-10-07 23:52 JSTに人がserverを止めた3秒後、Claudeの非対話のworkerのcargoがserverを起動し、supervisorの10秒ごとの確認より先だった（出どころ不明として検知された）。goal 79の受け入れ(2)「supervisorがserverを起動・維持する」と、(3)の検知の意味（supervisor以外の起動は異常）が保てない。

sccache 0.18の振る舞い（clientは自分を`SCCACHE_START_SERVER=1`で起動し直してserverにし、その子はportのbindより前に`SCCACHE_ERROR_LOG`を開き、開けなければ終わる。起動を止める設定は無い）はADR-t2008-1のContextのとおり。

## Decision

1. **適用先は、runtimeが`[run.env]`を渡すすべてのprocessとする。** providerを問わないworkerのturnと`needs_session`のresumeのturn、providerを問わないrunのreviewのjob、`integrate`の検証コマンド、supervisorが`[run.env]`で走らせるbuild（着地前の再確認のコマンド、e2eとその再試行）。sandboxの中か外かで分けない。
2. **適用先のすべてに、どのprocessも開けない`SCCACHE_ERROR_LOG`を渡し、serverになれないようにする。** serverを確かめたかどうか、guardを用意できたかどうかに依らず渡す。supervisorが自分でserverを起動するときの環境には渡さない。
3. **直前にserverを確かめたものには、`RUSTC_WRAPPER`としてsccacheの代わりにguardを渡す。** guardはdagqのbinaryで、compileごとにserverのportを確かめ、listenしていれば`[run.env]`のsccacheを通してcompileし、listenしていなければ、またはsccacheが自分の失敗（決定2で起動を拒まれた）で終わればcompilerを直接実行する。compilerの失敗はそのままcompileの結果にする。
4. **直前にserverを確かめられなかったもの、guardを用意できなかったものは、適用先のどれでも`RUSTC_WRAPPER`を外して起動し、外したことをeventに残す**（決定2は外したものにも渡す）。buildはcacheなしで正しく通る。supervisorが起動するものの前の確認は、supervisorが居ないserverを起動する機会でもある。ADR-0049決定8との関係はADR-t1215-1決定2のとおり（ツールはあり、外すたびにeventに残すので黙らない）。
5. **保つもの**: [ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定6（sccacheでrun間のcacheを共有し、`CARGO_TARGET_DIR`を共有しない）と[ADR-t813-3](2026-09-28-t813-3-codex-worker-permissions.md)の決定4（Codexのsandboxはnetworkを開けてsupervisorのserverに接続する）。serverが居る間はどの適用先もguardを通してそのserverのcacheを使う。人の`~/.codex`とsccacheの設定ファイルは書き換えず、適用先の環境だけで行う。runtimeの外（人のterminalなど）からの起動は今までどおりADR-t1215-1決定3の検知が扱う。

ADR-t1215-1決定1は、直前の確認の対象を「sandboxの中で走るもの」から決定1の適用先に広げ、「Claudeのworker、`integrate`の検証、着地前の再確認、自動更新のe2eは今までどおり」を除く。決定2は決定4に置き換わる。ADR-t1215-1決定3・4は変えない。

## Alternatives

- **sandboxの外のものはguardだけを渡し、起動の拒否を渡さない**: guardのportの確認とclientの接続の間にserverが止まると、clientがserverを起動する。確認の直後の競合が残るので採らない（ADR-t2008-1のAlternatives）。
- **supervisorの確認の間隔を縮める**: 競合の窓を狭めるだけで消せない。
- **出どころ不明の健康なserverをsupervisorが止めて起動し直す**: 走っているbuildを壊す。
- **起動を拒むだけでguardを置かない**: 確認の後にserverが止まったprocessのcompileが全部失敗する。
- **適用先では常に`RUSTC_WRAPPER`を外す**: run間のcacheの共有（ADR-0049決定6）を失う。
- **PATHの前のshim・`SCCACHE_NO_DAEMON`・client-side mode**: ADR-t2008-1のAlternativesのとおり、起動の経路を止めない。

## Consequences

- runtimeが起動したprocessは、sandboxの中でも外でも、途中でserverが止まっても確認の直後の競合でも、serverを起動しない。serverを起動するのはsupervisorだけになり、それ以外の起動は人の操作などruntimeの外のものとして検知される。
- 確認の直後の競合に当たったcompileだけは、clientがserverの起動を待つ分だけ遅れてからcompilerで通る。compileごとにguardの起動とportへの接続が1回ずつ増える費用は、sandboxの外の適用先にも掛かる。
- 決定2はsccache 0.18の起動の順番（serverが最初にerrorのlogを開く）に依る。sccacheを上げるときはこの順番を確かめ直す。
- 適用先・guardの名前・変数・置き場所・eventは[Run environment](../design/supervisor-lifecycle/run-environment.md)の「sccacheのserver」とコードのdoc commentに書く。
