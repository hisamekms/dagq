---
id: adr-t3049-1
type: adr
title: leaseも開いたaskも無いawaiting_integrationのrunで最後のreviewが着地を許すものは、人を待つ理由が無ければsupervisorが拾い直して着地の列に戻し、工程の記録とstatusのprogressもそれに合わせる（ADR-0054決定12をamends）
status: accepted
created: 2026-10-21
updated: 2026-10-21
accepted_on: 2026-10-21
amends:
  - adr-0054 decision 12
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - lifecycle
related:
  - adr-0054
  - adr-0039
  - adr-t1662-1
  - adr-t2599-1
---

# ADR-t3049-1: leaseも開いたaskも無いawaiting_integrationのrunで最後のreviewが着地を許すものは、人を待つ理由が無ければsupervisorが拾い直して着地の列に戻し、工程の記録とstatusのprogressもそれに合わせる（ADR-0054決定12をamends）

## Context

着地のslotを待つrun（reviewがpassし、sessionの`/exit`の後に着地の列に並んだもの）は、drainするsupervisorが自分では解けない着地の保留（ディスクの空き不足、`[run.env]`のprogramが無い、着地のbranchが解決しない、e2eが流せない）に当たると、leaseを返して`awaiting_integration`のまま残る。
[ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md)決定12は、leaseの無い`awaiting_integration`を`recover run`ではなく人の着地を待つrun（`review and integrate`、失敗したreviewなら`review by hand`、開いた`approve_landing`のaskがあればそのask）として出すと決めている。
[ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)決定1の引き継ぎ（adopt）はstaleなlease行があるrunだけを対象にし、lease行の無いrunを引き継がないものに挙げる。
そのため、止めるdrainの後や自動更新のhandoffで返されたrunは、次のsupervisorが起きても誰も拾わず、人に届くのは`review and integrate`のattentionだけだった。
2026-10-20〜21に3回起き、1件はreviewがpassのまま13.8時間残った。
そのあいだ工程の記録は人の答えを待つ工程のまま、`status`のprogressは着地の列で待つ工程のままで、実際（leaseもaskも無く、どの列にも居ない）と食い違った。

## Decision

1. **拾い直す。** supervisorはfill passごとに、lease行も閉じていないaskも無い`awaiting_integration`のrunのうち、最後のreviewが着地を許し人の判断を待つ理由の無いものを見つけ、leaseを取ってreviewをやり直さずに続ける。
   続け方はadoptした`awaiting_integration`のrunと同じで、passの後のprecheckか`/exit`の続きを経て着地のslotを待ち、保留が解ければ着地する。
   slotの空きが要り、drainしているsupervisorは拾わない（拾っても同じ保留でまた返すため）。
   空いたslotだけを使い、他のrunのslotを取り上げない。
2. **見分け方はeventの並びで決める。** 返した時に印のeventを足さず、最後のreview以降のeventと、queueが持つlease・ask・taskの状態から判定する。
   印にすると、印を書く経路でしか拾えず、他の経路の取り残しと印より前に残ったrunを拾えないため。
   人を待つために残す経路はどれも判定が読むeventかqueueの状態を残すので、印が無くても見分けられる。
3. **対象の外（人の判断を待つ理由）。** 次のどれかに当たるrunは拾わず、今までどおり人の着地を待つrunとして出す。
   - 最後のreviewが着地を許さない（reviewが無い・終わっていない、`revise`（reviewの3回目以降の差し戻しで人の承認が要るものを含む）、人に回す`concern`、subagentが他へ送ったもの、送ったreviseやconflictの依頼の途中）。
   - 最後のreview以降にaskを開いた（`approve_landing`の答え待ちと、答えたが適用前のものを含む）か、runに閉じていないaskがある。
   - 最後のreview以降に着地を人に保留した、または着地の決定・控え・失敗を記録した。
   - 最後のreview以降にsupervisorがruntimeのerrorでleaseを手放した（拾い直しの続きの失敗を含む）。
   - taskのholdか再計画が掛かっているか、review以降にそのためにparkした。
   - 着地の前のagent evalがpassしなかった、landing recheckがparkした。
   - 他の経路が拾う（`land`の答えか回収した着地で列に並んだ、reviewし直しを待つ）。
   - taskが`in_progress`でない（cancelや完了）。
4. **判定できないrunのaskは開かない。** 判定はeventとqueueの状態だけで決まり、判定できないrunは上の対象の外として人の着地を待つrunに残る。
   拾ったrunの続きがerrorになれば、他のruntimeのerrorと同じくleaseを手放し、そのerrorがinboxのattentionになって人の着地を待つrunに移る。
   そのrunは3の対象の外になるので、次のpassが拾い直して同じerrorを繰り返すことは無い。
   runtimeが`approve_landing`のaskを開く経路は足さない。
5. **ADR-0054決定12をamendsする。** 変えるのは決定12のうち「leaseの無い`awaiting_integration`は`recover run`ではなく人の着地を待つrun（`review and integrate`、失敗したreviewなら`review by hand`、開いた`approve_landing`のaskがあればそのask）として出す」の部分だけである。
   変えた後は、上の1で拾い直すrunはruntimeが着地させるrunとして出し、その対象の外のものだけを今までどおり人の着地を待つrunとして出す。
   決定12のそれ以外（leaseの無い`claimed`・`starting`・`running`・`validating`・`integrating`を`recover run`にすること）は変えない。
6. **ADR-0039決定1との関係。** adopt（staleなlease行を引き継いでtokenを入れ替える）は変えず、amendsしない。
   拾い直しはadoptではない別の経路で、lease行が無いことを条件に新しいleaseを作る。
   1つの`BEGIN IMMEDIATE`のトランザクションで、lease行が無いこと・runがまだ`awaiting_integration`であること・閉じていないaskが無いこと・taskが`in_progress`でholdも再計画も掛かっていないことと、その時点のrunのeventで上の3の対象の外でないことを判定し直してleaseを作り、拾い直したことを記録する（記録の形はleaseを作る操作のdoc comment）。
   同じrunを判定した2つ目のsupervisorはlease行を見つけて何もしない。
   判定の後に他のsupervisorが拾って着地を人に保留したrunも、トランザクションがその記録を読んで拾わない。
   ADR-0039決定1の「lease行のないrunを引き継がない」はadoptの範囲の記述としてそのまま残る。
7. **工程の記録とprogressを実際に合わせる。** slotを離れたleaseの無い`awaiting_integration`のrunの工程は、拾い直す対象ならslotに戻るのを待つ工程（queueが動かすもの）、対象の外なら今までどおり人の答えを待つ工程とする（[ADR-t1662-1](2026-10-04-t1662-1-runs-are-covering-phase-intervals-folded-into-a-ledger.md)の区間の規則のまま）。
   `status`のprogressは、leaseの無いrunを着地の列で待つと出すのを、eventが着地の列に並べたrun（`land`の答えか回収した着地）だけにし、leaseを返されたrunは工程なしと出す。
   工程とprogressの名前と欄はそれぞれの定義のdoc commentが持つ。

## Alternatives

- **返す時に印のeventを足し、印のあるrunだけ拾う**: 判定は単純だが、印を書かない経路の取り残しと、印の前に残ったrunを拾えない。
- **拾えないrunにruntimeが`approve_landing`のaskを開く**: 判定はeventとqueueの状態で決まり、判定できない残りは人の判断を待つ理由のあるrunで、すでに`review and integrate`などのattentionかaskで人に届いているため、重ねて聞かない。
- **adoptを広げてlease行の無いrunも引き継ぐ（ADR-0039決定1をamends）**: adoptは死んだsupervisorのtokenを入れ替える操作で、`recover`や人のためにleaseを外したrunまで同じ経路に乗り、決定1の「引き継がないもの」の理由（abandon・recoverの意図）を崩す。
- **handoffでleaseを返さない修正だけにする**: 自動更新のhandoffの取り残しは無くなるが、止めるdrain（`down`など）の後と他の経路の取り残しが残る。

## Consequences

- 止めるdrainが返したrunは、次に`up`したsupervisorがreviewをやり直さずに着地させる。
  止めるdrain自体が人に返す規則（leaseを返して止まる）は変えない。
- `recover`がleaseだけを外した`awaiting_integration`のrunも、passしていれば拾い直しの対象になる。
- 拾い直しの判定は1 passにつき`awaiting_integration`のrunごとにeventを読むので、そのrunが多いqueueではpassの読み取りが増える。
- 人を待つ理由を新しく足す経路は、その印をreview以降のeventかqueueの状態に残さないと拾い直しの対象に入る。
