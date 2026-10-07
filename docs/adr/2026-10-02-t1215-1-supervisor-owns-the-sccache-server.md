---
id: adr-t1215-1
type: adr
title: sccacheのserverはsupervisorがsandboxの外で起動して持ち、sandboxの中のプロセスには起動させない
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amends:
  - adr-0049 decision 3
amended_by:
  - adr-t2008-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
  - operations
related:
  - adr-0049
  - adr-t813-3
  - adr-t1207-1
  - design-supervisor-lifecycle-run-environment
---

# ADR-t1215-1: sccacheのserverはsupervisorがsandboxの外で起動して持ち、sandboxの中のプロセスには起動させない

## Context

この repositoryの`dagq.toml`の`[run.env]`は`RUSTC_WRAPPER = "sccache"`を渡し（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定6）、sccacheのserverは最初にcargoを動かしたプロセスのclientが常駐のdaemonとして起動する。2026-10-01 22:49のsupervisorの起動の直後、`needs_session`のCodexの非対話のworker（run 73947f80・22e0f67b・6ca49051）が最初にcargoを動かし、server（pid 88902）がCodexのworkspace-writeのsandboxを引き継いだまま常駐した。その後そのserverを通す全部のbuild（ほかのrunのworktree、着地前の再確認、自動更新のe2e）が依存crateの`.d`の書き込みで`Operation not permitted`になった（ask 273、9/30の偽の`landing_recheck_failed` 19件）。人が普通のterminalで`sccache --stop-server` → `--start-server`を打って解消した。

sccache 0.18にはclientがserverを自動で起動するのを止める設定が無い。serverは既定で600秒のidleで止まるので、次に最初にcargoを動かすのがsandboxの中のプロセスなら同じことが起きる。人の方針で、いつ・どのプロセスがserverを起動したかが分かることは必須（goal 79）。

ADR-0049の決定3は`[run.env]`をworkerのworkspace（resumeを含む）・`integrate`の検証・reviewのheadless実行にそのまま渡すと決め、決定8はツールが無いhostで黙って別のbuildに落とさないと決めた。

## Decision

1. **`[run.env]`の`RUSTC_WRAPPER`がsccache（basenameが`sccache`）のとき、sccacheのserverはsupervisor（sandboxの外の信頼する制御側）が起動して持ち、runtimeがsandboxの中で走らせるプロセスには起動させない。**
   - supervisorは起動時と以後の周回と、sandboxの中で走るもの（Codexのworkerのturnとresumeのturn、`[run.env]`を受けるCodexのjob。今はCodexで動くrunのreview）を始める直前に、serverが居るかを確かめ、居なければsandboxの外から`SCCACHE_IDLE_TIMEOUT=0`で起動する（idleで止まって次の起動者がsandboxの中になるのを防ぐ）。
   - 確かめる処理は副作用でserverを起動しない（sccacheのclientのコマンドはserverが居ないとそれ自体がserverを起動するため）。
   - 起動したら時刻・serverのpid・port・起動したsupervisor・理由をeventに残し、失敗したら失敗のeventを残す。
   - Claudeのworker（sandboxなし）、`integrate`の検証、着地前の再確認、自動更新のe2eはsandboxの外なので今までどおり。
2. **直前にserverを確かめられなかったsandboxの中のturnとjobは、その環境から`RUSTC_WRAPPER`を外して起動し、外したことをeventに残す。**（ADR-0049の決定3をamends）
   - ADR-0049の決定3は`[run.env]`をworkerのworkspace（resumeを含む）とreviewのheadless実行にそのまま渡すと決めた。これを、sandboxの中のturnとjobでserverを確かめられないときは`RUSTC_WRAPPER`を外して渡す、と条件付きに変える。buildはcacheなしで正しく通る。
   - **決定8との関係**: ADR-0049の決定8が退けたのは、ツールが無いhostで黙ってrustcに落とすwrapperで、run全体が気づかれずにcacheなしになることだった。ここはツールがあり、sandboxの中からserverを起動させないための、turnとjobごとの例外で、外すたびにeventに残すので黙らない。ツールが無いhostの扱い（決定8・9の検知と停止）は変えない。
3. **supervisorが起動したのではないserver（sandboxの中や人のterminalから起動したもの）と、壊れたserver（compileが失敗し続ける・sandboxの中で走る）は、supervisorが検知してeventに残し、壊れたserverはsandboxの外で起動し直す。**（実装は後続のtask 1216）
4. **保つもの**: ADR-0049の決定6（sccacheでrun間のcacheを共有し、`CARGO_TARGET_DIR`を共有しない）と、[ADR-t813-3](2026-09-28-t813-3-codex-worker-permissions.md)の決定4（Codexのsandboxはnetworkを開けてsccacheのserverに接続する）は変えない。sandboxの中のプロセスは、supervisorが起動したserverにclientとして接続してcacheを使う。

## Alternatives

- **Codexのsandboxの中では常に`RUSTC_WRAPPER`を外す**: 起動の心配は無くなるが、Codexのrunのbuildが毎回cacheなしになり、ADR-0049の決定6とADR-t813-3の決定4が守ろうとしたcacheの共有を失う。
- **serverの起動を止める設定を待つ・sccacheを改造する**: sccache 0.18に無く、人の`~/.codex`とsccacheの設定ファイルは書き換えない（goal 79のconstraints）。
- **`sccache --show-stats`で確かめる**: serverが居ないとclientがserverを起動し、そのときのsupervisorのenv（`SCCACHE_IDLE_TIMEOUT=0`なし）でeventも残らない。

## Consequences

- serverはsupervisorの起動の直後から`SCCACHE_IDLE_TIMEOUT=0`で常駐し、誰がいつ起動したかがqueueのeventで読める。
- serverが居ない間にsandboxの中で走るturnとjobはcacheなしでbuildし、その回数はeventで数えられる。
- turnやjobが走っている途中でserverが止まった場合、そのturnの中のclientがserverを起動しうる。これは後続の検知（決定3）が見つけて起動し直す。runtimeの外で人やsessionがsandboxの中から起動したもの（9/30のCodexのinbox）もこのADRでは防げず、決定3の検知が扱う。
- 仕組み（eventの名前と欄、確かめ方、周回の間隔、起動のlog）は[Run environment](../design/supervisor-lifecycle/run-environment.md)に書く。
