---
id: adr-0080
type: adr
title: 衝突の多いファイルで進行中のrunと重なるtaskはそのpassでclaimせずに次の候補へ進み、控えた理由と時間をstatusとstatsに出し、supervisorは[conflicts]の変更を止まらずに読み直す（ADR-0069を統合）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amended_by:
  - adr-t774-1
  - adr-t775-1
  - adr-t1484-1
  - adr-t1981-1
supersedes:
  - adr-0069
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - performance
related:
  - adr-0029
  - adr-0046
  - adr-0049
  - adr-0051
  - adr-0068
  - adr-0069
  - adr-t615-1
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-conflict-thresholds
  - design-supervisor-lifecycle-claim-hold
---

# ADR-0080: 衝突の多いファイルで進行中のrunと重なるtaskはそのpassでclaimせずに次の候補へ進み、控えた理由と時間をstatusとstatsに出し、supervisorは`[conflicts]`の変更を止まらずに読み直す（ADR-0069を統合）

## Context

[ADR-0069](0069-do-not-claim-tasks-overlapping-hot-files.md)（goal 45、task 463）は、衝突の多いファイル（hotspot）で進行中のrunと重なるtaskのclaimを控える仕組みを入れた。閾値と控えの上限は`dagq.toml`の`[conflicts]`にあるが、supervisorはそれを起動時にだけ読むので、goal 45の効果を見ながら上限や閾値を調整するたびに`down --wait`で走行中のrunをdrainしてから`up`し直す必要があった（ADR-0069の帰結）。一方`[run.env]`はpassごとにmain checkoutの`dagq.toml`を読み直して変化を印にしている（[ADR-0051](0051-kpi-time-series-report-and-push.md)決定11）。

このADRはADR-0069を丸ごと置き換える。決定1〜10はADR-0069の決定をそのまま引き継ぎ、決定11〜13で読み直しを足し、帰結の「変えたら`down --wait` → `up`」を改める（task 585）。

## Decision

1. **予想するファイル**: 候補のtaskが触ると予想するファイルは、宣言した`--paths`（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)。globのまま扱う）。宣言が無ければ、`dagq related`（[ADR-0046](0046-full-text-search-related-and-duplicate-of.md)）で最も似た`completed`のtask 3件の着地commitが変えたファイル。どちらも無ければ何も予想せず、控えない
2. **進行中のrunのファイル**: `in_progress`のtaskの最新のrun（走っている、validating・review中、着地待ち、`needs_session`で人やresumeを待つものを含む）ごとに、base commitからrunのheadまでの差分のファイルと、そのtaskの予想するファイル（決定1と同じ規則）を合わせたもの
3. **hotspot**: `stats`の`conflict_hotspots`のうち`alert`になるファイル（mainから消えたものを除き、名前が変わったものは今の名前）。閾値は`[conflicts]`の衝突の回数と割合で、alertと同じく両方を満たすもの。alertとplan reviewのpromptと同じ基準にし、閾値を1か所で調整できるようにする
4. **控える**: 候補を順に見て、予想するファイルと、どれか1つの進行中のrunのファイルが同じhotspotを触れば（globが一致すれば触るとみなす）、そのpassではそのtaskをclaimせず、次の候補に進む。claimするたびに進行中のrunを読み直す
5. **interruptは控えない**: 効く優先度が`interrupt`のtaskは重なっても控えず、控えていたtaskがinterruptになれば控えを終える
6. **上限**: 控えは`[conflicts]`の上限の秒数で終わる。数え始めは最初の控えの記録で、supervisorが入れ替わっても続く（queueのeventから読み直す）。上限を過ぎたtaskは重なっていてもclaimし、そのtaskが次にclaimされるまで控え直さない。既定はrunのwork時間の中央値（2026-09-26で約24分）から、邪魔なrunが着地まで進むのに足り、控えたtaskの待ちがrun 2本分程度に収まる1時間
7. **他にclaimできるtaskが無く、slotが空いているとき**: 控えたまま待つ（直列にする）。空いたslotは`stats`のalertで見える
8. **記録**: 控え始めたときと終わったときにtaskのeventを1回ずつ書き（重なったhotspot・邪魔なrun・上限、終わり方）、控えている間は書き直さない。load averageの控え（task 327）はqueue全体のeventなので、別のkindにし、`status`と`stats`の出し方をそろえる
9. **`status`と`stats`**: `status`は今控えているtaskを、`stats`はwindowの中の控えの回数と時間、終わり方ごと、hotspotごとの内訳と今の控えを出す。空きslotがあり控えているtaskがあれば、空きslotのalertの代わりに控えのalertを出す（load averageの控えのalertが先）
10. **hotspotの読み直しの間隔**: hotspotの計算はqueueの全eventとmainの履歴を読むので、supervisorは10分ごとに読み直し、taskの予想するファイルも同じ間隔で読み直す。進行中のrunのファイルは60秒ごとか、claimの後に読み直す。hotspotが無いか、進行中のrunがどのhotspotも触らなければ、候補のファイルは読まない
11. **`[conflicts]`の読み直し**: supervisorはpassごとに、claimの前にmain checkoutの`dagq.toml`の`[conflicts]`を読み直す（`[run.env]`と同じ間隔。ファイル1つを読むだけで軽く、決定10の10分に合わせると調整の効きが最大10分遅れ、testでも確かめにくい）。値が変われば、以後のhotspotの判定（決定3。変わった時点でcacheしたhotspotを捨てて計算し直す）・控えの上限（決定6）・plan reviewのpromptが新しい値を使う。上限の変更は進行中の控えにも効き、数え始めは最初の控えの記録のまま。設定がtestなどで起動の引数から与えられたときは読み直さない
12. **読めない・不正な`[conflicts]`**: 起動時は今どおり既定値で起動する。起動の後は、読めない・不正な値のときも、`dagq.toml`が無いとき（checkoutの書き換えの途中でありうる）も、直前に使っていた値を保ち、既定値に戻さない。読めないことはwarnに出し、同じエラーでは繰り返さない。直前の値を保つのは、調整の打ち間違い1つで控えの基準が黙って既定値に変わり、直したときにも変更として数えられるのを避けるため（なお`dagq.toml`全体が読めない間は、着地先のbranchも解決できずclaimと着地は止まる。[ADR-t615-1](2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)）
13. **変更の記録**: 値が変わったら、queueのeventを1回残す（前の値・新しい値・読んだ元・supervisor）。同じqueueの別のsupervisorがすでに同じ新しい値への変更を記録していれば重ねない。起動時に読んだ値は変更として記録しない

## Consequences

- hotspotを触る2つのtaskが同時に走らなくなり、rebaseの衝突と着地待ちが減る見込み。効果は`stats`の`conflict_hotspots`と控えの前後で見る
- 予想が外れると、実際には触らないtaskを控える（上限まで）か、触るtaskを控えない。宣言の`--paths`が広いglobだと控えやすくなる
- 同じqueueに2つのsupervisorが居ると、それぞれが同じtaskの控えを書くことがある。`stats`は後の方で前の控えを終える
- `[conflicts]`の上限と閾値は、main checkoutの`dagq.toml`に着地した時点で、`down --wait` → `up`なしに次のpassから効く。変えた時点はqueueのeventで分かる
- `[stall]`など他の表は今どおり起動時にだけ読む（範囲外）
