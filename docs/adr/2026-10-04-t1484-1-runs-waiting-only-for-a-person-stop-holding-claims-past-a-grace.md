---
id: adr-t1484-1
type: adr
title: 人のaskだけを待つ進行中のrunは、待ちが猶予を過ぎたらhotspotの控えで進行中のrunに数えない
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0080 decision 2
  - adr-0080 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - performance
related:
  - adr-0071
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-conflict-thresholds
---

# ADR-t1484-1: 人のaskだけを待つ進行中のrunは、待ちが猶予を過ぎたらhotspotの控えで進行中のrunに数えない

## Context

ADR-0080の決定2は、hotspotの控え（`reason: hot_files`）の相手になる進行中のrunに、`needs_session`で人やresumeを待つものを含め、決定6は控えを`[conflicts] defer_max_secs`（既定1時間）で終える。observerは、人のaskで止まったrun（`worker_question`、`approve_landing`、`stuck_exit`）がslotを持たないまま、hotspotで後ろのtaskのclaimを上限まで止めたことを13回記録した（finding 64、task 1484、goal 45）。人の答えは数時間かかることがあり（ask 200は約3時間）、892の例では7本のtaskが控えられた。

待ちのrunは答えが来るまで着地しないので、控えても衝突は先送りになるだけで、その間に他のtaskが先に着地すれば、待ちのrunが着地の前の再確認かresumeでrebaseする。衝突を待ちの側の1本に寄せる方が、後ろの複数のtaskを止めるより安い。

## Decision

1. ADR-0080の決定2を補う。進行中のrunのうち、runに紐づく、答えも閉じられもしていない人のask（`worker_question`・`approve_landing`・`stuck_exit`・`answer_prompt`・`stalled`・`decide`）を持ち、かつ[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)の待ち（戻り待ちは除く）にあるか、どのsupervisorのleaseも持たない（resumeでも作業でもなくaskで止まっている）runは、**人だけを待つrun**とする。その待ちの始まりは、ADR-0071の待ちならその始まり、leaseが無いならrunの最後の`lease_acquired` / `lease_released`で、どちらもそのaskの最も古いものより前にはしない。
2. 人だけを待つrunは、待ちの始まりから`[conflicts]`の新しい欄`waiting_owner_grace_secs`（既定600秒。正の整数）を過ぎたら、控えの判定で進行中のrunに数えない。新しく控えを始めるときも、控えを続けるかの判定でも同じ規則で数える。
3. ADR-0080の決定6を補う。控えていたtaskの邪魔なrunが全て決定2で数えないrunになったら、`defer_max_secs`を待たずに控えを終えてclaimし、`claim_deferral_ended`の`why`に新しい値`owner_waiting`を書く（`cleared`のまま理由の欄を足す案は採らない。`stats`の`claim_deferrals.by_end`が終わり方ごとに数えるので、新しい値ならそのまま別に数えられる）。
4. 待ちが終わってrunが動き出したら（答え・閉じる・待ちから戻る・leaseの取得）、また進行中のrunに数える。控えていない候補はそこから決定4（ADR-0080）のとおり新しく控える。
5. 猶予はADR-0080の決定11・12（読み直しと不正な値の扱い）と、ADR-t774-1・ADR-t775-1に従う。`conflicts_config_changed`の`from` / `to`もこの欄を持つ。

## Alternatives

- **全体の`defer_max_secs`を下げる**: 作業中のrunとの衝突の回避まで弱めるので採らない。
- **人のaskを持つrunを猶予なしで数えない**: inboxが見ていて答えがすぐ来るときにも控えを外し、直後に動き出すrunと衝突しうる。猶予600秒はすぐ来る答えのときに控えを保ち、来ないときだけ外すため。
- **leaseのあるrunも、askがあれば数えない**: validating・review・着地待ち・resume中のrunはaskと並んで動くことがある（landing recheckのresumeは`approve_landing`の答えを待たない）。ADR-0071の待ちに入っているものだけを人だけを待つとみなす。

## Consequences

- 人の答えが猶予を過ぎて来ないとき、hotspotで重なるtaskは待ちのrunを追い越して走り、先に着地しうる。待ちのrunの衝突はそのrunのrebaseに寄る。
- 進行中のrunのファイルは60秒のcacheなので（ADR-0080決定10）、待ちの始まり・終わりの反映は最大60秒（かclaimまで）遅れうる。猶予の経過そのものはpassごとに判定する。
- 新しい欄が`conflicts_config_changed`の値に加わるので、この変更の後に`dagq.toml`の`[conflicts]`で起動したsupervisorは、ADR-t775-1の起動時の比べで1回`source: start`の記録を残しうる。
- 現在の仕様は[claimを控える（衝突の多いファイル）](../design/supervisor-lifecycle/claim-defer.md)と[Conflict thresholds](../design/supervisor-lifecycle/conflict-thresholds.md)に置く。
