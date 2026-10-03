---
id: adr-t1533-1
type: adr
title: 開いているruntimeのplannerへの続きの依頼は、plannerのIDで生きている非対話のplannerだけに次のturnとして届け、対話のplannerには理由付きで拒んでcmuxで送らず、plannerを閉じるCLI（planner close）は作らずruntimeのplannerはsupervisorが閉じる（ADR-t1228-1決定2・3、ADR-t1394-2決定7をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amends:
  - adr-t1228-1 decision 2
  - adr-t1228-1 decision 3
  - adr-t1394-2 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - inbox
  - security
related:
  - adr-t1228-1
  - adr-t1394-1
  - adr-t1394-2
  - adr-t1433-2
  - adr-t1433-3
  - adr-t728-1
  - adr-t728-3
  - adr-t1091-1
  - design-supervisor-lifecycle-plan-planners
  - design-authorization
---

# ADR-t1533-1: 続きの依頼は非対話のplannerにだけplannerのIDで次のturnとして届け、planner closeは作らない

## Context

[ADR-t1228-1](2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)決定2はplannerへの依頼を「生きている対話のplanner」に、supervisorの送信と同じ経路（cmuxの打ち込み）で送るとし、決定3はtask 695の`planner close`をinboxが他のplannerのIDに使えるようにするとした。[ADR-t1394-2](2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定7はこの依頼の宛先を「生きているruntimeのplanner（対話・非対話）」に広げ、非対話のplannerを閉じる操作（決定3の部分）と、終わらせるのはその閉じる操作で行うこと（決定5の部分）を決めた。

その後、人が開くplannerは廃止され（[ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）、新しい計画は依頼ごとにruntimeのplannerを立てる経路になった。対話の経路も廃止され（[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3・5。決定5はADR-t1228-1決定2の宛先から「対話のplanner」とterminalへの送信を除き、ADR-t1394-2決定7の次のturnの依頼として置く形だけを残した）、wrapperはbackgroundだけになる（[ADR-t1433-3](2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。task 695は取り消され後継が無い。2026-10-03に人は、task 1229（対話のplannerへのcmuxの送信と`planner close`を前提にした実装のtask）を取り消し、続きの依頼は非対話のplannerだけに届けること（依頼の案A）と、`planner close`を作らないことを決めた。

## Decision

1. **続きの依頼の宛先と届け方（ADR-t1228-1決定2とADR-t1394-2決定7の決定2の部分をamends）。** 開いているruntimeのplannerへの続きの依頼は、plannerのIDで宛先を指す。宛先は生きている（開いている・作業中・idle）非対話のruntimeのplannerだけにする。依頼のファイルをplannerのディレクトリに写し、写した先を指す決まった文を次のturnの依頼として置く（ADR-t1394-1決定10の受け渡しの形）。作業中のturnがあっても拒まず、今のturnの後に取られる。対話のplanner・人が開いたplanner・閉じた・`lost`・`exited`・終了の依頼を置いたplannerへの依頼は、理由を付けて拒み、何も送らない。対話のplannerへのcmuxの送信は作らない。ADR-t1394-2決定7が宛先に含めた対話のplannerは外し、ADR-t1433-2決定5の読み替えと同じ範囲（非対話の次のturnの形だけ）にそろえる。
   - plannerのIDにするのは、依頼の記録のID（ADR-t1394-1）が新しい計画の単位で、立て直しで1つの依頼に順にplannerが変わり、draft・finding・reviseのために立ったplannerは依頼のIDを持たないため。届く先のsessionを1つに決められるのはplannerのIDだけである。開いたplannerが居ない依頼に人の言葉を足すときは、新しい依頼を記録する。
2. **plannerを閉じるCLIは作らない（ADR-t1228-1決定3とADR-t1394-2決定7の決定3の部分をamends）。** `planner close`は作らない。runtimeのplannerは、仕事を終えたとき・決めずに終わったとき・時間切れのとき・行の片付けのときにsupervisorが閉じる（ADR-t1394-2決定3、ADR-t1300-1）ので、inboxや人が他のplannerを閉じる口は要らない。plannerが自分自身を閉じる口も足さない。
3. **終わらせるのはsupervisor（ADR-t1394-2決定7の決定5の部分をamends）。** 非対話のplannerに送る操作を拒むことと、`planner_question`の答えをsupervisorが次のturnとして届けることは変えない。「終わらせるのは決定3の閉じる操作で行う」は、supervisorが終了の依頼を置いて終わらせると読む。
4. **判定と記録。** 続きの依頼はADR-t1228-1決定7のとおり、applicationの境界でdefault denyのpolicyに通し、userとinboxだけに許し、ほかのrole（planner・worker・observer・job・supervisor）は拒んで`authorization_denied`を残す。成功した依頼はactor付きのeventを残す。

ADR-t1228-1決定4・5の非対話のplannerの扱い（画面が無いことを返しturnの出力を指す・送る操作を拒む）はADR-t1394-2決定7とADR-t1433-2決定5のまま変えない。CLIの綴り・eventのkindと欄・capabilityの名前は[`plan` / `planners`](../design/supervisor-lifecycle/plan-planners.md)と[Authorization](../design/authorization.md)に書く。

## Alternatives

- **依頼の記録のIDで宛先を指す**: 依頼に開いているplannerが無いときや、立て直しの後に別のplannerが同じ依頼を持つときに届く先が揺れ、draft・finding・reviseのplannerには届けられない。
- **対話のplannerにもcmuxで送る**: ADR-t1433-2で対話の経路が廃止され、作っても消える。cmuxをinboxだけが使う方針（ADR-t1433-1）にも反する。
- **作業中のturnがあるときは拒む**: inboxが空くまで待って打ち直すことになる。依頼はturnの順に取られるので置けば足りる（ADR-t1394-2決定7の理由のまま）。
- **`planner close`を作る**: runtimeのplannerはsupervisorが閉じ、人のplannerは廃止された。他のplannerを閉じる口は、誤ってかprompt injectionで仕事中のplannerを止める手段になるだけで、要る場面が無い。

## Consequences

- inboxは開いている非対話のplannerに、人の言葉を次のturnとして足せる。対話のplannerと終わったplannerには届かず、新しい依頼（`request add`）を記録する。
- goal 81の受け入れのうち「plannerを閉じる操作」はこのADRで作らないと決めた。skillとAGENTS.mdの手順はplannerを閉じる手順を持たない（task 1232）。
- 依頼はsupervisorの依頼と同じ`turns/`に順に置かれるので、supervisorの依頼と番号がぶつからないように書く（書き方はdesignが持つ）。
