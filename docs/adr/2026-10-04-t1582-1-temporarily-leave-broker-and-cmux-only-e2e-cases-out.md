---
id: adr-t1582-1
type: adr
title: brokerのe2e 1ケースとcmux固有のe2e 3ケースを、担当taskが復帰・撤去するまで期間限定でe2eの登録から外す（ADR-t963-1決定1・3、ADR-t1233-2決定2をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-t963-1 decision 1
  - adr-t963-1 decision 3
  - adr-t1233-2 decision 2
amended_by:
  - adr-t2125-1
owners:
  - hisamekms
tags:
  - testing
  - supervisor
  - operations
related:
  - adr-t963-1
  - adr-t1233-2
  - adr-t1162-1
  - adr-t1433-1
  - adr-t827-1
  - development-testing
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-install
---

# ADR-t1582-1: brokerとcmux固有のe2e 4ケースを、担当taskが復帰・撤去するまで期間限定でe2eの登録から外す（ADR-t963-1決定1・3、ADR-t1233-2決定2をamends）

## Context

[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1は、自動更新と`install`の関門で全部のe2eを流すと決め、決定3は`[e2e] paths`に置く範囲を実cmux・実プロセスの境目（cmuxのadapter・lifecycle・actorの起動など）とし、その境目をe2eが実物で確かめる前提に立つ。[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定2は、着地の前にruntimeがhostで全部のe2eを流すと決めた。

2026-10-02以降の着地の前のe2eは初回成功30件で103〜298秒（中央値162秒）、30件全てでbrokerのe2eが最後に終わり、hostで同時に1本のe2eを待つrunがslotを持ったまま待つ。原因のimageのbuildはgoal 93のtask 1451が直す。goal 93は「測定の前にbrokerのe2eを外さない」としていたが、2026-10-03に人が、1451が入るまでbrokerのe2eを一時的に外すよう明示した。同じ日に人は、cmuxの廃止（[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）の実装を待たず、廃止で消えるか置き換わるcmux固有の3ケースも先に外すよう明示した。

## Decision

1. **期間限定で次の4ケースをe2eの登録から外す。** 本文は残し、`#[cfg(any())]`でcompileと登録から外す（早期returnで成功に見せず、`#[ignore]`だけにもしない）。外れたケースは`--ignored`の実行のどこにも現れず、着地の前のe2eと、自動更新・`install`の関門の全部のe2eからも外れる。結果の`skipped`（podmanに繋がらないときの省略、[ADR-t1162-1](2026-09-30-t1162-1-e2e-gate-skips-podman-e2e-only-when-podman-is-unreachable.md)）とは別で、そこには出ない。
   - `broker::a_preferred_worker_does_its_task_through_the_broker_and_lands`: task 1451が自分の変更でcfgを外して復帰し、着地の前に実podmanでbrokerのe2eを流す。
   - `the_sweep_closes_workspaces_left_in_any_window_after_their_fixture_dir_is_gone`: task 1440が旧ケースとcfgを削除する。
   - `planner::the_runtime_opens_planners_side_by_side_that_submit_go_idle_and_exit`: task 1441が削除するか非対話のケースに置き換え、置き換えたらcfgを外して復帰する（先にtask 1399で置き換わっていればその実装に合わせる）。
   - `up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes`: task 1443が旧ケースとcfgを削除する。
2. **範囲はこの4ケースに限る。** cmuxを経路に使うだけのe2e（着地・質問の回答・並列・adopt・`install`と自動更新の引き継ぎ・Codex）と、inboxを開くlaunchdの`up` / `down`は外さない。外したケースが使うだけになったhelperにだけ同じcfgを付け、担当taskがケースと一緒にそのcfgも始末する。crate全体の`dead_code`の許可は付けない。
3. **終了。** 各ケースは決定1の担当taskの着地で戻るか消え、全てが済めばこの例外は効力を失う。plan reviewは担当taskにこのtaskを先行依存とし、cfgの撤去・復帰の条件を受け入れ条件に書く。新しいケースを足して外すことはこのADRでは行わず、新しいADRが要る。

ADR-t963-1決定1とADR-t1233-2決定2の「全部のe2e」を、この期間に限り決定1の4ケースを除いたものに改める。ADR-t963-1決定3の`[e2e] paths`の範囲（どの差分でe2eを流すか）は変えないが、その範囲のうちbroker・cmuxのlifecycle・plannerの起動・掃除の境目は、この期間はe2eで確かめられない。関門の中身（lock・流し直し・印・podmanの確認）と、goal 93の制約（image のtagと版の一致を弱めない、測定の後に差分で絞る案を決める）は変えない。

## Alternatives

- **早期returnで成功にする**: 流していないtestがpassedに数えられ、復帰の漏れが見えない。
- **`#[ignore]`だけで外す**: e2eはもともと全部`#[ignore]`で`--ignored`で流すので外れない。
- **`.config/e2e-quarantine.toml`の印**: 印は落ちたtestを流し直した後に通すだけで、brokerの時間を縮めない。上限3本にも収まらない。
- **ケースを今消す**: brokerは1451で復帰するので本文が要る。cmuxの3ケースの削除・置換は担当taskの範囲。

## Consequences

- 着地の前のe2eと関門の時間からbrokerの分が抜ける。その間、brokerとcmuxのlifecycle・plannerの起動・掃除の実物の境目はe2eで確かめられず、integration testと人のスモークだけが守る。
- 復帰・撤去は担当taskに縛られる。1451が進まなければbrokerのe2eは外れたままになるので、plan reviewが1451の受け入れ条件に実podmanのe2eを残す。
- 一覧と終了条件は[testの制約](../development/testing.md#e2e)・[Review](../design/supervisor-lifecycle/review.md#着地の前のe2e)・[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[install](../design/supervisor-lifecycle/install.md)が持つ。
