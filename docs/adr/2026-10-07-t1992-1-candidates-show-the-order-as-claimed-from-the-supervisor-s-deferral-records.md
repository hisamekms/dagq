---
id: adr-t1992-1
type: adr
title: candidatesとgraphのcandidatesの既定は、同じ表示の関数で、supervisorが最後に記録したclaimの控え（snapshot）を除いた順にし、控えたtaskと、claimを止める条件・生きたsupervisorが居ないことを順に混ぜず別の欄に出し、規則だけの順をopt-inで残す。判定はsupervisorが持ちCLIは記録を読む（ADR-0049決定4、ADR-t1487-2決定4をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-0049 decision 4
  - adr-t1487-2 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - cli
related:
  - adr-0049
  - adr-t1487-2
  - adr-0080
  - adr-t774-1
  - adr-t775-1
  - adr-t1484-1
  - adr-0069
  - adr-t1850-1
  - design-domain-model
  - design-supervisor-lifecycle-claim-defer
---

# ADR-t1992-1: candidatesとgraphのcandidatesの既定は、同じ表示の関数で、supervisorが最後に記録したclaimの控え（snapshot）を除いた順にし、控えたtaskと、claimを止める条件・生きたsupervisorが居ないことを順に混ぜず別の欄に出し、規則だけの順をopt-inで残す。判定はsupervisorが持ちCLIは記録を読む（ADR-0049決定4、ADR-t1487-2決定4をamends）

## Context

[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定4を引き継いだもの）は、claimの順を「効く優先度 → 解放数 → ID」の1か所の判定にし、`candidates`と`graph`でplanner・inbox・人が次にclaimされるtaskを読めることを目的にした。
pluginの`dagq` skillの`reference/inspect.md`も`candidates`を「the order the supervisor will claim in」「Which tasks the next supervise can pick」と書く。

その後、supervisorはclaimの控えを持った（hotspot、providerが使えない、buildの待ちなど。今の決定は[ADR-0080](0080-supervisor-rereads-conflicts-config.md)とそのamendments（[ADR-t774-1](2026-10-04-t774-1-confirm-conflicts-config-on-consecutive-passes.md)・[ADR-t775-1](2026-10-04-t775-1-record-conflicts-at-start-against-the-latest-change.md)・[ADR-t1484-1](2026-10-04-t1484-1-runs-waiting-only-for-a-person-stop-holding-claims-past-a-grace.md)）、経緯は[ADR-0069](0069-do-not-claim-tasks-overlapping-hot-files.md)）。
`candidates`は控えを反映せず規則の順だけを出していたので、2026-10-06 22:4xには上位の1577・1981・1868・1971が控えでclaimされないのに`candidates`の先頭に並んでいた（request 58）。
文書が示す用途（次にclaimされるtaskを読む）が成り立たないので、人はこれをバグとした。
claimのdry runにあたるコマンドは無い。

[ADR-t1487-2](2026-10-04-t1487-2-spikes-share-the-worker-slots-under-a-cap-with-cross-class-aging.md)決定4は、`candidates`と`graph`が控えの除外（決定3(b)）とagingを同じ関数で求めることを決めた。
ただし控えの判定はsupervisorのmemory（hot filesとその入力のcache、`defer_max_secs`の時計、providerのroutes、buildの待ち）に依るので、CLIで求め直すと判定を写すことになり、ADR-0049決定4の「順序の判定は1か所」に反する。
supervisorは控えを判定するたびに`claim_deferred` / `claim_deferral_ended`を記録しており、`status`の`claim_deferrals`はそれを読む。

## Decision

1. **既定は控えを除いた順。** `candidates`と`graph`の`candidates`の既定は、規則の順（ADR-0049決定4の「効く優先度 → 解放数 → ID」。順の規則と判定の1か所は変えない）から、supervisorが記録した開いている控えのtaskを除いた順にする。
   控えたtaskは別の欄に、記録の理由・ファイル・原因のrun・始まりと一緒に出す。
2. **同じ表示の関数で記録を読む。** `candidates`と`graph`の`candidates`は同じ1つの表示の関数で並べる（ADR-t1487-2決定4の「同じ関数」は保つ）。
   控えの除外はCLIで再計算せず、supervisorの`claimable`が記録した控えを読む（ADR-t1487-2決定4の「控えの除外を同じ関数で求める」を、この形に改める）。
   CLIの経路で控えの判定・hot filesの計算・providerのroutesを走らせない。
   後続の区分の表示、Spikeの上限と調査中の計画の上限で控えた理由、agingは、同じ表示の関数に積む（控えの理由はsupervisorが記録し、表示は記録の値をそのまま出す）。
3. **表示はsnapshot。** 表示は、supervisorが最後に判定した控えの記録のsnapshotで、毎passの更新や遅れの上限は保証しない。
   supervisorはslotが無いときやclaimを止めている間は控えを判定しないので、その間は前の判定の控えが開いたまま残る。
   CLIはそれを推し量って閉じたり足したりせず、読み手は控えの始まりと下の止める条件で判断する。
4. **止める条件は順に混ぜない。** claimを止めている条件のうちsupervisorが記録しているものと、生きたsupervisorが居ないことは、順を変えずに別の欄に出す。
   記録の無い条件をCLIで推し量らない。
5. **規則だけの順はopt-in。** `candidates`のopt-inのフラグで、控えを除かない規則の順を出す（控えと止める条件は同じく添える）。
6. **内部の読み取りは変えない。** supervisorの`fill_slots`とclaimの順、statsとKPIのcandidatesの標本、observerの読み取りが使うqueueのcandidatesと依存グラフの`candidates`は変えない。
   表示の関数はCLIの`candidates`・`graph`の出力の経路だけで使う。

欄の名前・フラグの綴り・JSONの形は`docs/design/`と定義のそばのdoc commentに書く。

## Alternatives

- **CLIで控えを求め直す**: supervisorのmemoryに依る判定を写すことになり、2か所の判定がずれる。採らない。
- **supervisorが毎passに今の順を記録する**: 鮮度は上がるが、slotが埋まっている間も判定と記録を足すことになり、claimの経路を変える。今の記録で用途（次にclaimされるtaskを読む）は足りるので採らない。
- **既定は規則の順のまま、控えを欄で添えるだけ**: 先頭のtaskが次にclaimされないままで、文書の用途が成り立たない。採らない。

## Consequences

- planner・inbox・人は`candidates`の先頭で、supervisorの最後の判定の上で次にclaimされるtaskを読める。
- `candidates`の出力が配列からobjectに変わる（CLIのJSONの非互換）。`graph`は欄を足すだけ。
- 判定から時間が経つと表示は古くなりうる。slotが埋まっている間や止める条件の間の古さは、控えの始まりと止める条件の欄で読む。
- 記録の無い止める条件（CIの見張り・landing branchなど）は、supervisorが記録するまで出ない。
