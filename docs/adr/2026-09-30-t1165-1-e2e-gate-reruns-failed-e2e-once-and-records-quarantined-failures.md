---
id: adr-t1165-1
type: adr
title: 固定バイナリを入れ替える前のe2eの関門で、落ちたe2eを名前で絞って1回だけ流し直してflakyを見分けて通し、repositoryにcommitする印の付いたtestが流し直しでも落ちたときは記録だけにして入れ替えを進め、歯止め（続けての失敗・期限・上限）を置く（ADR-t963-1決定1をamends）
status: accepted
created: 2026-09-30
updated: 2026-09-30
accepted_on: 2026-09-30
amends:
  - adr-t963-1 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - testing
  - operations
related:
  - adr-t963-1
  - adr-t1162-1
  - adr-t768-1
  - adr-0073
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-install
---

# ADR-t1165-1: e2eの関門で落ちたe2eを1回流し直してflakyを見分け、印の付いたtestの失敗は記録だけにして入れ替えを進める（ADR-t963-1決定1をamends）

## Context

[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1は、固定バイナリを入れ替える前に全部のe2eを流し、1件でも落ちれば入れ替えないと決めた（podmanに繋がらないときの扱いは[ADR-t1162-1](2026-09-30-t1162-1-e2e-gate-skips-podman-e2e-only-when-podman-is-unreachable.md)が改めた）。

2026-09-29 09:18〜09-30 07:39の自動更新40回のうち13回（33%）がe2eの関門で`update_failed`になった。8回は`up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes`で、ほかも実cmuxに依るtestが入れ替わり落ち、次のbuildではたいてい通った（不安定）。落ちるたびに固定バイナリの更新が次の着地まで遅れ、inboxに`update_failed`のaskが開き、前提のtaskの入ったバイナリが入る前にclaimされたtaskが失敗した（goal 75、finding 76）。個々のtestの安定化は別のtaskが行うが、直るまでのあいだ更新が止まり続ける。2026-09-30に人が、不安定なe2eに印を付け、その失敗は記録するだけで関門を失敗させず、失敗の回数を確かめて随時安定化させると決めた。

着地の検証では[ADR-t768-1](2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)が、落ちたtestを1回だけ流し直して不安定な失敗（FLAKY）を見分けると決めている。

## Decision

1. **関門は、落ちたe2eを名前で絞って1回だけ流し直し、流し直しで通ったtestはflakyとして記録して通す。** 同じcheckout・同じenv・同じtargetで、上限と後始末も1回目と同じにする。上限切れ・始められない・落ちたtestの名前が読めないときは流し直さず、今までどおり落ちる。
2. **印の一覧は、関門が流すcheckoutのrepositoryにcommitするファイル（`dagq.toml`ではない）に置き、付け外しはcommitとplan reviewを通す。** 1つの印はtestの名前・理由・直すtaskのID・期限を持つ。流し直しでも落ちたtestが全て効いている印を持てば、関門は通り、そのtestは印で通した（quarantined）として記録する。印の無いtestが1つでも流し直しで落ちれば、今までどおり落ちる。印は流し直しでも落ちたtestにだけ効く。`dagq.toml`に置かないのは、走っている旧バイナリが読む`dagq.toml`は全runに効き、固定バイナリが対応する前に着地するとqueueが止まるため。関門が流すcheckoutのファイルなら旧バイナリは読まず、着地の順に縛られない。
3. **歯止めを置く。** 期限を過ぎた印は効かない。印が上限（3本）を超えるファイルと読めないファイルでは、印を1つも効かせない。印の付いたtestが続けて決まった回数（既定3回。直前までの関門の記録から数える）流し直しでも落ちたら、不安定ではなく壊れているとして印を効かせずに関門を落とす。どの場合も、今の`update_failed`のask（`retry` / `skip`）の問いと、`install`のerrorに理由を書く。新しいaskのkindは作らない。
4. **流し直しで通したtestと印で通したtestを、testの名前・commit・log・段とともに関門の記録に残し、testごとに数えられるようにする。** 数え方とobserverのfindingへのつなぎは後続のtaskが行う。
5. **入れ替えの後の見張りと、失敗したときに前のバイナリに戻すことは変えない。**
6. **workerの手元のe2e（ADR-t963-1決定2のrun）にも同じ印を効かせる。ただし、runの差分がその印のtestか、印の直すtaskに当たるときは効かせない。** workerのe2eは差分が`[e2e] paths`に触れるrunだけで流れ、変更と無関係な印の付いた不安定なtestでworkerが時間を使いresumeされるのを避ける。本番のバイナリは決定1〜3の関門が守る。そのtestや直すtaskの変更では、印で隠さず実物で確かめる。実装は後続のtaskが行う。

## Alternatives

- **今のまま（1件でも落ちれば入れ替えない）**: 不安定なtestが直るまで、3回に1回の更新が止まり、そのたびに人が`retry` / `skip`を答える。
- **流し直しだけにして印を持たない**: 1回の流し直しでも落ちる不安定なtest（8回落ちたtestはこれに当たる）で更新が止まり続ける。
- **印を`dagq.toml`に置く**: 決定2の理由（旧バイナリが読めず、main checkoutの`dagq.toml`は全runに効く）。
- **流し直しを何度でも行う／印に期限・上限を置かない**: 関門の時間が延び、壊れたtestが印の下で黙って残り、本番のバイナリを守る関門の意味が薄れる。
- **workerの手元のe2eには印を効かせない**: 変更と無関係な不安定なtestで、`[e2e] paths`に触れるrunのworkerが時間を使いresumeされ続ける。

## Consequences

- 不安定なtestが直るまでのあいだも、印を付ければ自動更新は止まらない。印の付いたtestの実物での確かめは、流し直しでも落ちる間は入れ替えの前に行われないが、見張りと戻す仕組みはそのまま残る。
- 印は期限と上限と続けての失敗で外れるので、付けたままの放置は関門の失敗として人に戻る。
- 関門は落ちたときだけ、落ちたtestの流し直しの分だけ延びる。
- ファイルの場所と書式、上限と回数の既定値、記録の欄名、logの場所は[Auto-update](../design/supervisor-lifecycle/auto-update.md)と[install](../design/supervisor-lifecycle/install.md)が持つ。
