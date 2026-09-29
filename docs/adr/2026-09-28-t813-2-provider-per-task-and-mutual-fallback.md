---
id: adr-t813-2
type: adr
title: worker の provider を task ごとに選び（claude / codex、既定 claude）、provider が使えないときだけ両方向にもう一方へ切り替え、切り替え先は同じ worktree で新しい session を始める。認証と利用上限の控えを provider ごとにし、worker の claim を止めるのは両方が使えないときにする。Claude だけの役割が止まる Claude の控えは今の ask を開き、Codex だけの控えは ask を開かない（ADR-0004 決定 1、ADR-0047 決定 42 を amends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0004 decision 1
  - adr-0047 decision 42
amended_by:
  - adr-t1063-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
related:
  - adr-0004
  - adr-0047
  - adr-t598-1
  - adr-t813-1
  - adr-t813-3
  - design-supervisor-lifecycle-queue-hold
  - plan-headless-worker-spike
---

# ADR-t813-2: worker の provider を task ごとに選び（claude / codex、既定 claude）、provider が使えないときだけ両方向にもう一方へ切り替え、切り替え先は同じ worktree で新しい session を始める。認証と利用上限の控えを provider ごとにし、worker の claim を止めるのは両方が使えないときにする。Claude だけの役割が止まる Claude の控えは今の ask を開き、Codex だけの控えは ask を開かない（ADR-0004 決定 1、ADR-0047 決定 42 を amends）

## Context

[ADR-0004](0004-agent-provider-abstraction.md) は Claude と Codex を provider として抽象化し、run に requested / actual の provider を記録すると決め、fallback を「Claude を起動できない場合」の一方向（Context）と、起動不能など安全に判断できる段階だけ（Consequences）に限った（ADR-0004 の Decision は番号の無い 1 つなので、この ADR では Decision と、それが前提にする Context と Consequences の fallback の条件の組を決定 1 と呼ぶ）。実装は Claude だけだった。

2026-09-27 に人は planner に、provider の選び方は「task ごとでいいが、何かしらの理由で使えなかった時に相互フォールバックしたい」と答えた（goal 57）。[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) 決定 42 は、認証切れと利用上限で queue 全体の新しい claim と headless の job を止め、queue に種類ごとに 1 件の `queue_hold` の ask を開く（task 361・437、[queue-hold](../design/supervisor-lifecycle/queue-hold.md)）。Codex を足すと、片方の provider だけが使えないときに queue 全体を止める理由が無くなる。一方、review・復旧・plan review・goal review の job、observer、planner、inbox は Claude だけで動き（goal 57 の制約）、切り替え先が無い。

spike（[headless-worker-spike](../plans/headless-worker-spike.md)）で、認証切れは両 provider とも stream の最初の再試行の event で分かること、会話は provider をまたいで引き継げないこと（session の形式が違う）が分かった。利用上限の実物の出力は再現できていない。

## Decision

1. **task ごとに provider を選ぶ。** worker の provider は task の属性で、値は `claude` / `codex`、既定は `claude`。Claude は対話と非対話のどちらの経路でも、Codex は非対話の経路だけで動く（[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md) 決定 7）。
2. **使えない provider からは両方向に切り替える（ADR-0004 決定 1 を amends）。** 「使えない」は次に限る: 実行ファイルが無い、起動できない、認証が要る、利用上限や rate limit に当たった。判定は起動の前後だけでなく turn の途中の失敗にも当て、turn の失敗が認証か利用上限なら、そのとき process を止め、次の呼び出しからもう一方の provider を使う。ADR-0004 の「Claude を起動できないとき Codex」の一方向と「起動の段階だけ」を、この条件での両方向と turn の失敗にまで広げる。
3. **一般的な失敗では切り替えない。** build や test の失敗、非 0 の終了、時間の上限、permission の拒否、model の誤りなど、provider が使えないこと以外の失敗は、ADR-0004 の線引きのまま切り替えず、ADR-t813-1 決定 9 の 3 層で扱う。
4. **切り替え先は同じ worktree と branch で新しい session を始める。** 会話は引き継がない。切り替え先の最初の呼び出しは resume の prompt（task の prompt に加え、今までの commit、worktree の未 commit の変更、送るはずだった answer・revise・催促の文）で始める。その run の以後の turn は切り替え先の session で続け、切り替え先も使えなくなったときだけ元に戻る（そのときも新しい session）。run の `actual_provider` と、切り替えと理由の event が残る。新しい run は task の provider から選び直す。
5. **対話の Claude の run が認証切れか利用上限で止まったときは、非対話の Codex に切り替える。** runtime は止まった対話の session の workspace を閉じ（worktree とその変更は残す）、同じ worktree で非対話の Codex の新しい session を決定 4 のとおり始める。Codex も使えなければ、ADR-0047 決定 42 のまま session を開いたまま待ち、`done` の answer で「続けて」を送る。
6. **認証と利用上限の控えを provider ごとにする（ADR-0047 決定 42 を amends）。** 決定 42 の「queue 全体の控え」を「provider ごとの控え」に読み替える。
   - worker の claim と turn は、使える provider があれば止めない。task の provider が控えられていれば、claim の時点からもう一方の provider で始める。
   - 両方の provider が控えられたときだけ、worker の新しい claim を止め、今の `queue_hold` の ask（`authentication` か、`cost` の利用上限）を開く。止まった run は失敗にせず待ち、ask の `affected` に入る。answer の `done` / `cancel_affected` の扱いは決定 42 のまま。
   - Claude の控えは、切り替え先の無い Claude だけの役割（review・復旧・plan review・goal review の job、observer、runtime の planner の起動）を今までどおり止める。これらは人が要るので、Claude が控えられたときは Codex が使えても今の `queue_hold` の ask を開く。worker は Codex で進む。
   - Codex だけが控えられたときは ask を開かない（Claude で進むので人を待つものが無い）。控えと理由は event と `status` に残し、runtime が一定の間隔（利用上限は分かれば解ける時刻）の後の次の Codex の呼び出しで確かめ直して解く。
7. **provider と経路を記録と集計の属性にする。** run の requested / actual の provider と経路（対話 / 非対話）を記録し、`show`・`stats`・`kpi` で分けて読めるようにする。非対話の turn の usage も記録する。

欄名・flag の綴り・event の kind・確かめ直しの間隔・判定する文言の型は後続の実装 task が [docs/design/](../design/) に書く。

## Alternatives

- **Claude から Codex への一方向だけ**（ADR-0004 のまま）: 人の「相互フォールバック」に合わず、Codex を選んだ task が Codex の認証切れで止まる。
- **一般的な失敗でも切り替える**: 実装途中の失敗は provider のせいではなく、切り替えは会話を失って同じ失敗を繰り返しうる。ADR-0004 の線引きを保つ。
- **切り替え先で会話を引き継ぐ（transcript を変換して渡す）**: 形式が違い、tool の呼び出しの意味も違うので正しく写せない。worktree と commit が作業の状態を持つので、新しい session の resume の prompt で足りる。
- **控えを queue 全体のままにする**（決定 42 のまま）: 片方が使えれば進める worker まで止まり、フォールバックの意味が無くなる。
- **Codex だけが使えないときも ask を開く**: 人を待つものが無いのに inbox に届き、決定 41 の「人が要るものだけ」に反する。
- **対話の Claude の run は切り替えず決定 42 で待つ**: 人の「相互フォールバック」に合わず、人がログインか上限の回復まで run が止まる。

## Consequences

- 片方の provider が使えなくても worker は進む。Claude が使えないときは review などが止まるので、着地は人が Claude を戻すまで待つ。
- 切り替えた run は会話の文脈を失うので、turn が増えうる。切り替えの回数と理由は event と集計で見る。
- 利用上限の実物の出力は未確認で、判定の文言は実物を見た時点で design と実装を直す（spike の follow-up）。
- [queue-hold](../design/supervisor-lifecycle/queue-hold.md) の控えの単位は、実装の task が provider ごとに直す。
