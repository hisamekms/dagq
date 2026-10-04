---
id: adr-t1504-2
type: adr
title: runtimeがfollow-upの所属の判断を必須の欄つきの行で記録し、許す状態遷移・acceptanceの版・閉じる条件の検査（並行・再起動を含む）・出どころと深さの保持・set-goalによる人のadoptの迂回の封じ・判断の無いdraftのsubmitの拒否・achievedの後の訂正を強制し、既存のfollow-upは推定せず未判定のまま残す（ADR-0047決定8・10・16・20・43、ADR-t808-1決定2をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0047 decision 8
  - adr-0047 decision 10
  - adr-0047 decision 16
  - adr-0047 decision 20
  - adr-0047 decision 43
  - adr-t808-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - follow-up
  - goal
  - migration
related:
  - adr-t1504-1
  - adr-0009
  - adr-0047
  - adr-t808-1
  - adr-t947-3
  - adr-t598-1
  - design-supervisor-lifecycle-goal-review
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-receipt-and-session-exit
  - design-persistence
---

# ADR-t1504-2: runtimeがfollow-upの所属の判断を記録し、閉じる条件と出どころと人のadoptの上限を強制する（ADR-0047決定8・10・16・20・43、ADR-t808-1決定2をamends）

## Context

[ADR-t1504-1](2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)は、follow-upの所属を元のgoalのacceptanceの達成に必要かで決め、goalを必須の作業の完了と所属の判断の完了で閉じると決め、runtimeには汎用の契約を持たせた。今のruntimeには次の穴がある。

- 所属の判断の記録も、判断したときのacceptanceの版も無い（goalの履歴は`goal_updated`のeventだけ）。
- `draft_origins`の材料（`integrate`の`register_follow_ups`が書く）は元のtaskとrunを持つが元のgoalを持たない。登録時にgoalが閉じていた印は`follow_up_registered`のpayloadにだけあり、goalが無かったことはどこにも記録されず元のtaskのgoalから導くしかない。
- `submit`の人の`adopt`の上限（[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)決定2の「goalが無いか閉じているfollow_up」）は、submitの時点のdraftのgoalを読む。runtimeのplannerがgoalの無い・閉じたgoalのfollow_upを`set-goal`で開いたgoalへ移してからsubmitすると、人の`adopt`を経ずに出せる。
- goal reviewの起動とcloseは所属taskの状態だけを見るので、未判定のfollow-upをgoalから外せば閉じられる。
- `register_follow_ups`は元のgoalが閉じているかを登録のtransactionの外で読むので、その間にgoalが閉じると登録が閉じたgoalへの追加で失敗し、follow-upがwarnだけを残して失われる。

欄名・CLIの綴り・eventのkind・migrationの番号はこのADRに書かず、実装のtaskが`docs/design/`に書く（[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)）。

## Decision

1. **所属の判断はfollow-upごとの行で記録する。** 対象は出どころが`follow_up`で元のgoal（下の4。不明（12(ii)）を含む）があるtaskで、状態は問わない（draftに限らず、submitted以降の訂正と、achievedの後の訂正（9）も同じ記録で行う）。1回の判断は次の最小の欄を持つ: 分類（必須＝元のgoalに必須 / 範囲外 / 未判定の3つ。値の綴りは`docs/design/`が持つ）、該当するacceptanceの項目（必須は1つ以上が要る）、理由（要る）、証拠の参照（必須と範囲外は1つ以上が要る。receipt・commit・文書の節・taskなど）、所属先のgoal（範囲外は要り、taskを受け付ける元のgoalと別のgoal）、判定時の元のgoalのacceptanceの版（runtimeが書く）、判断した者（role）と時刻。runtimeは欄の有無と形だけを検査し、意味は判定しない。行は追記で、最新の行がそのfollow-upの今の判断になり、同じ内容をeventにも記録する。
   - **eventだけにしない根拠**: 閉じる条件・submit・`set-goal`は、元のgoalごとの「今の判断」を同じtransactionの中で読んで判定する。eventのpayloadのJSONから最新を引く読み方は、状態の一意さも遷移の検査も持てず、今の`check_adoptions`がsubmitの時点の値に頼って迂回を許したのと同じ形の穴を作る。判断の履歴（訂正の前後）も行で引ける必要がある。
2. **許す状態遷移。** 判断の無い状態は未判定と同じに扱う。判断の無い状態から必須・範囲外・未判定（調べても決まらない理由を残すとき）へ。未判定から必須・範囲外へ。必須と範囲外の間の変更は訂正で、前の判断を名指す理由が要る。同じ分類の記録し直しは確かめ直し（新しい版・証拠の更新）として許す。必須・範囲外から未判定へは戻さない（迷いは訂正か人への問いにする）。判断を記録できるのはruntimeのplannerと人で、worker・plan review・goal reviewのjobは記録しない。
3. **acceptanceの版。** goalはacceptanceの文が変わるたびに増える版を持ち、変更のeventに前後の版を残す（acceptance以外の欄の変更では増やさない。分類の基準はacceptanceだから）。判断が持つ版がgoalの今の版より古ければ、その判断は「要再確認」で、閉じる条件では未判定と同じに扱う。要再確認は書き込まず、判断の版とgoalの版から毎回導く（acceptanceの変更と判断の記録が並んでも状態がずれない）。runtimeはacceptanceの変更が弱める変更かを判定しない（ADR-t1504-1決定6(a)はplannerの手順とplan review・goal reviewの検査が持ち、goal reviewの入力に判断の後のacceptanceの変更を載せる）。
4. **出どころと深さは所属を変えても残す。** `integrate`は`draft_origins`の材料に元のgoal（元のtaskの登録時のgoal。無ければ無い）と、登録時にそれが開いていたかを足す。材料と`follow_up_depth`は、判断の記録・`set-goal`・訂正のどれでも変えない。深さを0に戻すのは今までどおり人の判断を経たsubmitだけ（ADR-t808-1決定2）。
5. **人の`adopt`の上限は登録時の事実で決め、所属の変更で迂回させない。** runtimeのplannerが人の`adopt`なしにsubmitできないfollow_upは、登録時に元のgoalが無いか閉じていた（4の材料。分からなければ無かったとみなす）、submitの時点でdraftのgoalが無いか閉じている、深さが上限以上、のどれかに当たるもの。ADR-t808-1決定2とADR-0047決定20の「goalが無いか閉じているfollow_up」（閉じたgoalのfollow_up）をこの3つに読み替える。既に人が`adopt`したdraftは今までどおり聞き直さない。
6. **所属の変更は判断と一緒に行う。** 範囲外の判断は所属先のgoalへの移動を、必須の判断は元のgoalへの移動（今いなければ）を、記録と同じtransactionで行う。移動は[ADR-0009](0009-goal-groups-tasks.md)の付け替えの規則（`draft` / `ready`のtaskだけ、閉じたgoalへは付け替えない）を変えず、その状態のときだけ行う。それより先の状態（submitted・in_progressなど）のfollow-upは判断だけを記録してgoalを動かさず、範囲外のものは今のgoalのtaskとして今までどおりそのgoalの閉鎖を待たせ、必須のものは下の8で元のgoalの閉鎖を止める。元のgoalが閉じていて必須の判断を移せないときは、taskのgoalを動かさず、achievedなら9の経路に乗せ、abandonedなら下の7の例外とする。必須か範囲外の判断を持つfollow-upを、判断と違うgoalへ`set-goal`で移すことは拒み、判断の記録（訂正）を促す。未判定のfollow-upの`set-goal`は許すが、判断にはならず、元のgoalを閉じる条件（8）にも残る。
7. **submitは所属の判断の無いfollow_upのdraftを拒む。** 元のgoalのあるfollow_upのdraftで、今の判断が無い・未判定・要再確認のものは、誰のsubmit（runtimeのplanner、人のterminal、`ready --bypass-review`）でも拒み、判断の記録を促す（ADR-0047決定8の人のbypassの例外は、このdraftには判断の記録の後に効く。ADR-0047決定8をamends）。元のgoalが不明（12(ii)）のものは元のgoalがあるものとして扱い、判断を求める。元のgoalの無いfollow_upと、元のgoalがabandonedで閉じたfollow_upは、照らすacceptanceが無い・達成を言わないので判断を求めない（どちらも5の人の`adopt`は要る）。ADR-0047決定10のplan reviewの入力に足し、plan reviewの資料には、proposalのfollow_upごとに判断の行（分類・項目・理由・証拠・所属先・版と今の版）、元のgoalのacceptance、workerの提案（11）を載せる。
8. **goalを閉じる条件と、並行・再起動での検査。** goal Gをachievedで閉じられるのは、今までの条件（所属taskがすべて終わっている等）に加えて、元のgoalがGで、状態が`completed` / `canceled`でなく、今の判断が無い・未判定・要再確認か、必須と判定したのに元のgoalの外にあるfollow-upが無いとき。範囲外と判定したfollow-upの未完了は（Gの所属taskでなければ）妨げない。goal reviewの起動の条件、goal reviewの`achieved`の適用、人の`approve_goal`の`achieved`、人の`goal close --verdict achieved`のすべてがこの条件を使う（`abandoned`は達成を言わないので妨げない）。人の`goal close --verdict achieved`の拒否の条件（ADR-0009の「未終端のtaskがあれば拒否」とADR-0047決定8の「`completed` / `canceled`以外があれば拒否」）にこの条件を足す（ADR-0047決定8をamends。ADR-0009のその規則は所属taskについてそのまま残る）。
   - goal reviewのfingerprintに、元のgoalがGのfollow-upごとの今の判断（行の識別と分類）とGのacceptanceの版を含める。適用は今までどおり1つの`BEGIN IMMEDIATE`のtransactionで条件とfingerprintを検査し直し、崩れていれば閉じずに取り直す（入力が古くなったreviewの判定を使わない）。
   - 検査はtransactionの中のqueueの状態だけで行い、supervisorのメモリやjobの出力に頼らないので、再起動・引き継ぎの後も同じ結果になる。
   - `integrate`のfollow-upの登録は、元のgoalとそれが開いているか（4の材料と、draftを入れるgoal）を登録の`BEGIN IMMEDIATE`のtransactionの中で読み直す（今はtransactionの外で読む）。goalのcloseも`BEGIN IMMEDIATE`なので両者は順に並び、closeより前の登録は閉じる条件に入り、closeの後の登録は「登録時に元のgoalが閉じていた」follow-upになって9の経路に乗り、閉じたgoalへの追加で登録が失われることも無くなる。
9. **achievedの後の訂正。** 元のgoalがachievedで閉じた後に、そのfollow-upに必須の判断（新しい判断か、範囲外からの訂正）が記録されたら、runtimeはgoalの閉じた状態と達成の記録を変えず、訂正の記録（判断の行と、goalに付くevent）を残し、inbox宛てのask（`reason_category`は`scope`）をそのfollow-upのtaskについて作る。askは元のgoal・判断の項目と理由・そのgoalを待っていて既に解放されたtaskの一覧（今の状態つき）を載せ、askが開いているあいだfollow-upのgoalは動かさない。選択肢は、goalを再開する（履歴を残して開き直し、follow-upを戻す。今のruntimeにgoalを開き直す操作は無く、実装のtaskが足す）、達成の判定を訂正する（閉じたまま、達成が誤りだった記録を足し、修正は修正のgoalで行う）、達成のままにする（人が元のacceptanceは満たしていたと判断し、follow-upは範囲外として扱う）。runtimeはanswerを適用するが、走っている依存taskを止めず、過去のeventを書き換えない。
   - **ADR-0009の変更**: ADR-0009はgoalに状態機械を持たせず、完了を1回のeventにし、閉じたgoalへのtaskの追加と付け替えを拒み、続きは新しいgoalにすると決めた。この決定はその例外を1つだけ足す: このaskへの人の「再開」のanswerに限り、runtimeが閉じたgoalを開き直し（閉じたeventは残し、開き直したeventを足す。goalは再び閉じられる）、そのfollow-upをgoalに戻す。plannerやjobの判断、人の他の操作では開き直さず、閉じたgoalへの追加と付け替えの拒否、それ以外の続きは新しいgoalにする規則は変えない。範囲外の訂正（所属先の誤りなど）は人に問わず、記録と移動だけを行う。修正のgoalはplannerが既存の適切なgoalを探すか作る（ADR-t1504-1決定3）。
10. **goal_gapとreopenedは所属の判断の対象にしない。** `goal_gap`のdraftはgoal reviewが「acceptanceの項目に足りない」と判定して作ったもので、材料が項目を持ち、生まれつきそのgoalに必須である。runtimeのplannerがそれを別のgoalへ移すことは拒む（必須でないと決めるのは次のgoal reviewか人の`approve_goal`）。重複・実装済みのcancelは今までどおり許し、次のgoal reviewが確かめる。`reopened`のdraftは発見されたfollow-upでなく、元のtaskの所属のまま戻ったものなので対象にしない。
11. **workerの提案。** receiptの`follow_ups`の各要素は、元のgoalのacceptanceとの関係の提案（分類の案・該当する項目・理由）を任意に持てる。runtimeは提案を出どころの材料に記録してplannerとplan reviewに渡すが、判断として扱わず、欠けても形が違ってもreceiptを拒まない（[ADR-t947-3](2026-09-28-t947-3-follow-ups-carry-category-codes.md)決定3と同じ）。workerの`category`とは別の軸で、plannerは`category`を書き換えない。
12. **既存データの移行の契約。**
    - **(i) 復元できるもの**: 既存のfollow_upのdraftについて、元のtaskとrun（`draft_origins`の材料）、登録の時刻（`follow_up_registered`のeventの時刻。`draft_origins`の行の時刻は、導入前のfollow-upではmigrationが埋めた時刻なので使わない）、登録時の元のgoal（元のtaskの今のgoalから、登録の時刻より後の`task_goal_changed`を巻き戻したもの）、登録時にそれが開いていたか（payloadの`goal_closed`と、goalの`closed_at`と登録の時刻の比較）、draft自身のgoalの変更（draftの`task_goal_changed`）。復元した値は材料に「履歴から復元した」印つきで書き、登録時に記録した値と区別する。
    - **(ii) 復元できない・食い違うもの**: `follow_up_registered`のeventが欠ける、`goal_closed`と`closed_at`が食い違う、巻き戻した履歴が今の値に合わない、などは元のgoalを「不明」とする。人の`adopt`の上限では、登録時に元のgoalが無かったとみなし人の`adopt`を要する側に倒す。閉じる条件では、元のgoalが不明のfollow-upはどのgoalの「元のgoalごとの検査」（8）にも入らず、今のgoalには所属taskとして今までどおり効く（未完了なら閉じさせない）。判断の記録では、判断した者が照らしたgoalを根拠つきで名指し、その値は「判断した者が名指した」印で残して復元した値と区別する。
    - **(iii) 既存の人の`adopt`は保つ**: migrationは`follow_up_depth`と`follow_up_adopted` / `draft_adopted`の`by: person`の記録を変えず、既に人が`adopt`したdraftは5の上限で聞き直さない。所属の判断（7）は`adopt`とは別に要る。
    - **(iv) 既に`set-goal`で所属を変えたfollow_up**: 今のgoalのまま残し、移動を判断（範囲外など）と読まない。元のgoalが復元できれば、未完了のあいだはその元のgoalの閉じる条件（8）に入り、判断の記録を待つ。
    - **推定で確定しない**: 今の所属から元の所属を推定して確定しない（今のgoalを元のgoalとして書かない）。
    - **自動で済ませない**: migrationとruntimeは既存のfollow_upに判断の行を書かない。既存のfollow_upは未判定として残り、判断はplannerと人が行う（既存のopenのgoalの棚卸しはtask 1512）。完了・cancel済みのfollow_upは8の条件に入らないので、既存の閉じた経路を塞がない。

## Alternatives

- **eventだけで記録する**: 1の根拠のとおり、同じtransactionで今の判断を読む検査と遷移の検査が持てない。
- **要再確認を行に書き込む**: acceptanceの変更と判断の記録の順で状態がずれ、書き忘れの経路が残る。版の比較で導けば並行でも一意になる。
- **`set-goal`を全面に拒む**: 未判定の既存のfollow-upまで動かせなくなる。判断と食い違う移動だけを拒み、未判定の移動は元のgoalの検査で閉鎖の迂回を封じる。
- **既存のfollow_upを今の所属から判定して埋める**: 移行が人とplannerの判断を代行し、goal 97のconstraint（既存のgoalの所属を勝手に変えない）と食い違う。
- **achievedの後の必須の判断でruntimeがgoalを自動で開き直す**: 解放済みの依存taskが走っているときに巻き戻せず、判断は人のものである（ADR-t1504-1決定6(b)）。

## Consequences

- plannerはfollow_upのdraftを出す前に判断を記録する手間が増える。代わりに範囲外のfollow-upがgoalの閉鎖を止めなくなる。
- 既存のopenのgoalで、元のgoalから外した未判定の既存のfollow-upがあれば、判断が記録されるまでachievedで閉じられなくなる（意図した保守的な側）。
- 実装（行と欄、版、検査、CLI、prompt、eventのkind、migration）はgoal 97の別のtaskが行い、`docs/design/`の[Goal review](../design/supervisor-lifecycle/goal-review.md)・[Draft planners](../design/supervisor-lifecycle/draft-planners.md)・[Receipt and session exit](../design/supervisor-lifecycle/receipt-and-session-exit.md)・[Persistence](../design/persistence.md)に書く。着地するまで今の挙動のままである。
