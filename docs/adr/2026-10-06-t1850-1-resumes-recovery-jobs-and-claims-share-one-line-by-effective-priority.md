---
id: adr-t1850-1
type: adr
title: supervisorのfill passで、needs_sessionのrunの再開・終わったrunの復旧job・readyのtaskのclaimを1つの候補の列にしてtaskの効く優先度の降順にslotを渡し（同じ優先度は再開 → 復旧job → claim）、in_progressのtaskの効く優先度もreadyと同じに求め、人とinboxはin_progressのtaskにもset-priorityを打てる（ADR-0047決定24・39、ADR-0049決定4、ADR-t1639-1決定3、ADR-t1811-1決定2をamends）
status: accepted
created: 2026-10-06
updated: 2026-10-06
accepted_on: 2026-10-06
amends:
  - adr-0047 decision 24
  - adr-0047 decision 39
  - adr-0049 decision 4
  - adr-t1639-1 decision 3
  - adr-t1811-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - priority
related:
  - adr-0047
  - adr-0049
  - adr-t1639-1
  - adr-t1811-1
  - adr-t1487-2
  - design-supervisor-lifecycle-supervise
  - design-domain-model
---

# ADR-t1850-1: 再開・復旧job・claimを1つの列にして効く優先度の順にslotを渡し、in_progressのtaskにもset-priorityを効かせる

## Context

supervisorのfill passは、空いたslotを次の順に固定で渡していた: `needs_session`のrunの再開（`resume_parked_runs`、古い順で優先度を見ない）→ `failed` / `interrupted`のrunの復旧job（`triage_runs`）→ readyのtaskのclaim（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4の効く優先度の順）。優先度が効くのはclaimの中の並びだけで、再開と復旧jobはどの優先度のclaimよりも先に、また互いには種で決まった順に始まった。

2026-10-05、Claudeの利用上限のあいだにCodexで取られたrunがreviewのsubagentを動かせずに`review_failed`になり、まとめて`send_back`されて`needs_session`が33本溜まった。highのtask 1440の復旧job（ask 492の`retry_inherit`）は、normal・lowの再開の後ろで1時間以上始まらなかった。interruptのtaskも、claimの中でしか先にならず、再開と復旧jobには負ける。

また`set-priority`は`draft` / `submitted` / `ready`のtaskにしか効かない（ADR-0049決定4の「変えられるのは`draft` / `ready`の間だけ」、`src/domain/task.rs`の`set_priority`）。そのため、再開待ちのtaskを人が後回しにする手が無かった。

人はrequest 31で「再開を優先度順にする。再開より優先度が高いreadyがあればそちらを優先。同じ列に入れる。再開待ちのrunも優先順位の変更で切る」と決めた。同じ優先度で再開を先にするのはinboxの案で、人の「同じ列に入れる」はその提案への答え。

## Decision

1. **fill passでslotを取る3種、`needs_session`のrunの再開、`failed` / `interrupted`のrunの復旧job、readyのtaskのclaimを1つの候補の列にし、候補のtaskの効く優先度の降順にslotを渡す。** [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定24の再開と決定39の復旧jobも、claimと同じ優先度の物差しで並ぶ。
2. **同じ効く優先度では、再開 → 復旧job → claimの順にする。同じ種の中は今の順のまま**（再開は古い順、復旧jobは`runs_to_triage`の順、claimは効く優先度 → 解放数の降順 → IDの昇順と[ADR-t1487-2](2026-10-04-t1487-2-spikes-share-the-worker-slots-under-a-cap-with-cross-class-aging.md)決定2・3のaging）。走りかけの作業を先に終わらせてworktreeを早く空けるため。
3. **`in_progress`のtaskの効く優先度は、readyのtaskと同じく、自分の基の優先度と、自分を推移的に待っているreadyのtaskの基の優先度の最大値にする。** 辿り方と除くもの（`draft`・`canceled`・`completed`のtask、draftのgoalのtask、abandonedで閉じたgoalに依存するtask）はADR-0049決定4と[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)決定4のとおり。
4. **各種の今の保留の条件は、その種の候補をそのpassの列から外すだけにする。** 再開: brokerの`required`・着地のbranchが解決しない・`[run.env]`のprogramが無い・Claudeを使えない・生きたleaseかwrapper。復旧job: 利用上限などの`queue_hold`・着地のbranch・`wait`の待ち。claim: queue serviceの停止・`hold_claims`（disk・load）・claimの間隔・軽い枠（[ADR-t1591-1](2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)）・hotspotの`claim_deferred`・providerの不在・`wait_for_build`。保留で取れない上位の候補はslotを予約しない（今も再開がclaimの保留に関わらず進むのと同じ）。軽い枠は今どおりclaimだけが使い、再開と復旧jobは通常のslotだけを使う。
5. **slotの有無によらない処理は列に入れず、今の位置のままにする**: 試行を使い切った再開の`exhaust`、`stuck_exit`のaskのclose、adopt、着地、`review_recovered_runs`、ended runのsweep。
6. **走っているrunは止めない**（ADR-0049決定4の「割り込みで止めない」）。**自動のaging（待ち時間で優先度を上げる）は入れない**（ADR-0049決定4の飢餓への対策を入れない方針を保つ）。低い再開が高いreadyに押され続けるときは、人が`set-priority`かcancelで決める。
7. **人とinboxは`in_progress`のtask（再開待ちのrunか復旧を待つrunを持つもの）にも`set-priority`で自分の優先度を置け、`--inherit`で外せる。** それは1の列の次の順に効き、走っているrunは止めない。`completed` / `canceled`は今どおり拒む。plannerは今どおり`in_progress`のtaskを変えない（plannerがclaimの前のtaskだけを扱う規則を保つ）。記録は今の`task_priority_changed`のeventをそのまま使う。ADR-0049決定4の「変えられるのは`draft` / `ready`の間だけ」と、ADR-t1639-1決定3・[ADR-t1811-1](2026-10-05-t1811-1-tasks-without-own-priority-follow-the-goal-in-every-status.md)決定2の「claimの順に効くのは`ready`のtaskの次のclaimだけ」は、この1・7に改める（goalの優先度の変更も、個別の指定の無い`in_progress`のtaskの次の再開と復旧jobの順に効く）。

## Alternatives

- **再開・復旧job・claimの種の順を保ち、種の中だけを優先度で並べる**: 高いreadyが低い再開の後ろに残り、task 1440のように高い復旧jobが低い再開の後ろで待つことは解けない。人の「再開より優先度が高いreadyがあればそちらを優先」に合わない。
- **同じ優先度ではclaimを先にする**: 走りかけのrunのworktreeとdiskを持ち続ける時間が延び、着地の近い作業が後になる。
- **飢餓への自動のaging、再開待ちのworktreeの解放、`needs_session`の本数の警告をいま入れる**: 人の方針で保留した（goal 146のdescriptionに案を残した。必要になれば人が選ぶ）。
- **走っているrunを優先度で止めて入れ替える**: 作業の途中の状態を捨てるか退避する仕組みが要り、ADR-0049決定4の「割り込みで止めない」に反する。

## Consequences

- 列の順はdomainの純粋関数（`src/domain/slot_order.rs`）が決め、`fill_slots`（`src/application/supervise/mod.rs`）は再開・復旧jobの候補を集め、claimの候補を読み、列の先頭から種ごとの開始処理を呼ぶ。claimは1つの優先度の候補をまとめて今のclaimのloopで取り、低い優先度の再開・復旧jobに順を譲る。
- 低い優先度の`needs_session`は、高いreadyが続く間はslotを得ない。溜まり方は`status`と`list`で見え、人が`set-priority`（7）かcancelで決める。
- `set-priority`の範囲と権限が変わる（実装はtask 1851）。`docs/design/authorization.md`の表、`docs/design/domain-model.md`、pluginの`set-priority`の案内が今の姿を書く。
