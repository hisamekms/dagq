---
id: adr-t1634-1
type: adr
title: 自分のcommitを持たないfailedのrunは、triage・復旧・askを待つ間、猶予を待たずにhotspotの控えで進行中のrunに数えない
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amends:
  - adr-t1484-1 decision 2
  - adr-t1484-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - performance
related:
  - adr-0080
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-stats
---

# ADR-t1634-1: 自分のcommitを持たないfailedのrunは、triage・復旧・askを待つ間、猶予を待たずにhotspotの控えで進行中のrunに数えない

## Context

hotspotの控え（ADR-0080）は、`in_progress`のtaskの最新のrunを進行中のrunに数え、その差分とtaskの予想するファイルで候補と重なるかを見る。[ADR-t1484-1](2026-10-04-t1484-1-runs-waiting-only-for-a-person-stop-holding-claims-past-a-grace.md)は人のaskだけを待つrunを猶予（既定600秒）の後に外すが、askの前のtriageと復旧のjobを待つ間は外れない。

2026-09-29、task 1067のrun b4c3f42fは、verifyの関門で差分もslotも無いままfailedになり、triageとdecideのask 220を待った。その間、taskの予想するファイル（`docs/design/broker.md`）でhotspotの控えを持ち、5本のtaskを最大50分控えさせた（finding 76、goal 105）。このrunは自分のcommitを持たないので、着地もrebaseも衝突も起こしえない。retryなら新しいrunが最初から作り直す。控えても避ける衝突が無い。

## Decision

1. ADR-t1484-1の決定2を補う。進行中のrunのうち、最新のrunがfailed（validationで落ち、triage・復旧・askを待つ）で、自分のcommitを持たないもの（headの`result_commit`、無ければbranchが、base commitから何のファイルも変えていない。headが無いものを含む）は、猶予を待たずに控えの判定で進行中のrunに数えない。新しく控えるときも、控えを続けるかの判定でも同じに数える。headの差分が読めないrunは変えたファイルがあるものとして数える。
2. ADR-t1484-1の決定3を補う。控えていたtaskの邪魔なrunが全て決定1で数えないrunになったら、`defer_max_secs`を待たずに控えを終えてclaimし、`claim_deferral_ended`の`why`に新しい値を書く（綴りはdesign）。邪魔なrunに猶予を過ぎた人だけを待つrunが混じれば、今どおり`owner_waiting`で終える。
3. 自分のcommitを持つfailedのrunと、作業中・着地待ち・`needs_session`のrunは今どおり数える。人のaskを待つrunの猶予（ADR-t1484-1）はそのまま残す。retryで新しいrunがclaimされれば、そのrunは今どおり数える。

## Alternatives

- **failedのrunを全て数えない**: commitを持つfailedのrunは、triageがsessionに戻してresumeし、その差分のまま着地しうる。衝突の元の差分があるので控えを残す。
- **ADR-t1484-1の猶予をtriageと復旧の待ちにも広げる**: 猶予の間は控えが残り、差分の無いrunに猶予を払う理由が無い。
- **`result_commit`が無いことだけで判定する**: validationは落ちたreceiptのcommitも`result_commit`に残すので、baseと同じcommitを名指したrunを拾えない。base commitからの差分で判定する。

## Consequences

- 差分の無いfailedのrunの後ろのtaskは、triageと復旧を待たずにclaimされる。そのrunがretryされれば、先に走ったtaskと衝突しうるが、新しいrunは今のmainから始まる。
- 進行中のrunのファイルは60秒のcacheなので（ADR-0080決定10）、failedへの移りと、triageがsessionに戻して動き出したrunをまた数えることの反映は、最大60秒（かclaimまで）遅れうる。
- `stats`の`claim_deferrals.by_end`に新しい終わり方が加わる。
- 現在の仕様は[claimを控える（衝突の多いファイル）](../design/supervisor-lifecycle/claim-defer.md)と[stats](../design/supervisor-lifecycle/stats.md)に置く。
