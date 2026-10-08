---
id: adr-t1433-4
type: adr
title: supervisorはcmuxを呼ばずにlaunchdで常駐する。launchd modeのcmuxのpreflightとin-cmux modeをやめ、新しいバイナリは登録済みのin-cmuxのsupervisorの引き継ぎとdownを受け付け、up --in-cmuxは理由と次の手順付きで拒み、launchdを使えないときはforegroundのsuperviseを代わりにする（ADR-0011を置き換え、ADR-0026決定1・ADR-0021決定1・ADR-0073決定10・13・15をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amended_by:
  - adr-t2159-1
supersedes:
  - adr-0011
amends:
  - adr-0026 decision 1
  - adr-0021 decision 1
  - adr-0073 decision 10
  - adr-0073 decision 13
  - adr-0073 decision 15
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - cmux
related:
  - adr-0010
  - adr-0011
  - adr-0021
  - adr-0026
  - adr-0073
  - adr-t1091-1
  - adr-t1433-1
  - design-supervisor-lifecycle-up-down
---

# ADR-t1433-4: supervisorをcmuxなしで常駐させる

## Context

[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)は、launchdが起動したsupervisorはcmuxの中のprocessでないのでcmuxのsocketに接続できないことから、(1) launchd modeはcmuxのsocket passwordを前提にし、`up`がcmux外からの`cmux ping`をpreflightで確かめる、(2) launchdを使わずcmuxのworkspaceの中でsupervisorを動かす`up --in-cmux`をfallbackにする、(3) in-cmux modeのsupervisorのworkspace名を決める、とした。前提は、supervisorがworkerのworkspaceの作成・画面・`cmux notify`のためにcmuxを呼ぶことだった。

2026-10-03の人の決定（goal 92）で、supervisorはcmuxを呼ばなくなる（[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)・[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)・[ADR-t1433-3](2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。ADR-0011の3つの決定はすべて前提を失うので丸ごと置き換える。本番のsupervisorは今in-cmux modeで、`--cmux`を含むargvでexecの引き継ぎをしているので、入れ替えで壊さないことが要る。

## Decision

1. **supervisorはcmuxを呼ばず、launchdで常駐する。** supervisorの常駐はlaunchdのLaunchAgent（`KeepAlive`）だけにする（[ADR-0010](0010-maintainer-and-resident-supervisor.md)決定2の形）。supervisorはcmuxを呼ばないので、launchd modeのcmuxのsocket passwordの前提と、`up`がplistを書く前に行うcmux外からの接続のpreflight（ADR-0011決定1）はやめる。`up`がinboxのworkspaceを開くためのcmuxの確認（`up`を打ったshellから。ADR-t1433-1決定1）は残る。
2. **in-cmux modeを廃止する（ADR-0011決定2・3、ADR-0026決定1、ADR-0021決定1、ADR-0073決定10・13・15をamends）。** `up --in-cmux`は受け付けず、in-cmux modeが廃止されたことと次の手順（`down --wait`の後にflagなしの`up`でlaunchdに移す）を書いて拒む。supervisorのworkspace（名前・`supervisors`の`workspace_id`・`session_workspaces`の行）、in-cmuxのsupervisorを同じworkspaceに留める引き継ぎ（ADR-0073決定10）、引き継ぎが失敗したときに`up --in-cmux`の経路で開き直すこと（決定13）、modeでsignalの送り方を分けること（決定15）は、新しく起動するsupervisorには対象が無い。
3. **走っているin-cmuxのsupervisorの扱い。** 新しいバイナリは、登録済みのin-cmuxのsupervisor（`mode`と`workspace_id`を持つ登録、`--cmux`を含むargv）を読み、その引き継ぎ（exec）と`down`（signalとworkspaceのclose）を受け付ける。引き継いだsupervisorもcmuxを呼ばない（workspaceの中に居続けるだけ）。人は都合のよいときに`down --wait`の後でflagなしの`up`を打ってlaunchdに移す。登録の列は消さず、非互換のmigrationを足さない（goal 92のconstraints、[ADR-0073](0073-kind-additions-are-compatible.md)）。移った後の`down`だけがin-cmuxのworkspaceを閉じ、それがsupervisorについてcmuxを呼ぶ最後になる。
4. **launchdを使えないときの代わり。** launchdを使えないhost（Linux、コンテナ、launchdを許されない環境）では、人かそのhostのservice manager（systemdなど）が`dagq supervise`をforegroundで動かす。dagqはそのためのmodeを`up`に足さない。superviseはcmuxを要らないので、どのterminalやservice managerの下でも同じに動く。Linuxでの常駐の形はgoal 83の段で決める。

実装はgoal 92の後続のtaskが行う。拒む文面・登録の読み方は[`up` / `down`](../design/supervisor-lifecycle/up-down.md)に書く。

## Alternatives

- **in-cmux modeを残す**: supervisorがcmuxを呼ばなくなるので、cmuxの中で動かす理由（socketの接続）が無い。残すとsupervisorのworkspace・modeの分岐・引き継ぎの開き直しの経路とtestが残り、cmuxの無いhostで同じ形にならない。
- **in-cmuxの登録をmigrationで消し、新しいバイナリで即座にlaunchdに移す**: 走っている本番のsupervisorのexecの引き継ぎが新しいバイナリで失敗し、queueが止まる。人が`down --wait`と`up`で移すほうが安全で一度きり。
- **launchdの代わりにdagqが自前のwatchdogを持つ**: ADR-0010とADR-0011が退けたとおり、watchdog自体を誰も守れない。OSのservice managerに任せる。
- **launchd modeのpreflightを残す**: supervisorがcmuxに接続しないので、確かめるものが無い。残すとcmuxの設定がsupervisorの起動を止め続ける。

## Consequences

- supervisorの起動にcmuxのsocket passwordが要らなくなり、cmuxの終了でsupervisorが止まることも無い。
- in-cmuxのsupervisorは、人がlaunchdに移すまで登録のまま引き継がれる。移すときの一度の`down --wait`と`up`は人の操作になる。
- `up` / `down`はinboxのworkspaceについてだけcmuxを呼ぶ。
