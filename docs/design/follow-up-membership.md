---
id: design-follow-up-membership
type: design
title: Follow-up membership judgements
status: current
created: 2026-10-04
updated: 2026-10-04 # task 1509
last_verified: 2026-10-04 # task 1509
scope: runtime
related:
  - adr-t1504-2
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle-draft-planners
---

# Follow-up membership judgements

`judge-follow-up TASK --classification required|out_of_scope|undecided --reason TEXT`
はfollow_upの所属を記録する。user、inbox（人の代行）、runtimeのplannerだけが
`follow_up.judge`を持つ。worker、job、supervisorは持たない。taskの状態は問わない。
分類の意味の正しさはplannerとplan reviewが判断する（ADR-t1504-2）。

## CLIと行

`--acceptance-item TEXT`と`--evidence REF`は繰り返せる。requiredは1項目以上、
requiredとout_of_scopeは1参照以上が要る。理由と各項目・参照は空白だけではいけない。
out_of_scopeは`--destination-goal ID`で元と別の、閉じていないgoalを名指す。
requiredの所属先は元goalで、別の所属先は拒む。undecidedは調べても決まらない理由を残せる。
未知の分類、follow_up以外、元goalが無いと確定しているfollow_upは拒む。

`follow_up_judgements`は追記の行で、id、task_id、source_goal_id、source_kind、
classification、acceptance_items、reason、evidence、destination_goal_id、
acceptance_version、corrects、actor_role、created_atを持つ。最後の行が今の判断。
分類・必須の欄・遷移はdomainの`MembershipJudgement::validate`とstoreが検査する。
UPDATEとDELETEはtriggerで拒む。判断が無い状態から3分類へ、undecidedから
required/out_of_scopeへ進める。同じ分類の確かめ直しも許す。requiredとout_of_scopeの
間の訂正は理由と`--corrects <最後の行のid>`を要し、undecidedへ戻すのは拒む。

登録時の元goalが不明なら、`--source-goal ID`と証拠で照らしたgoalを名指す。
これは行の`source_kind: named_by_judge`に残し、登録時の材料を変更しない。
以後の判断は同じ元goalを使う。復元した元goalは`restored`、登録時に記録したものは
`recorded`で区別する。

## transactionと所属

判断は`BEGIN IMMEDIATE`で最新の行・task・goalを読み、版を取得し、行と
`follow_up_judged` event（taskと元goalに同じ内容）を一緒に書く。
requiredは元goalへ、out_of_scopeは所属先へ、draft/readyのtaskだけを
同じtransactionで移す。閉じた元goalへのrequiredとsubmitted以降は判断だけを記録し、
taskを移さない。移動は`set_goal_in`の閉じたgoalと依存の循環の検査を通す。
失敗すれば判断も移動もrollbackする。undecidedは所属を変えない。

required/out_of_scopeの判断を持つtaskの`set-goal`は、最後の判断の所属先と
違うgoal（`--none`も）へ移すのを拒み、訂正を促す。未判定の移動は判断にならない。
元のtask/run/goal、登録時の状態、provenanceと`follow_up_depth`はどの所属変更でも保つ。
登録時に元goalが無い・閉じていた・不明、現在の所属goalが無い・閉じている、
深さ3以上のいずれかなら、runtimeのplannerのsubmitは人のadoptなしに拒む。
既存の人のadoptは聞き直さず、adopt後のsubmitは今までどおり深さを0に戻す。

## acceptanceの版と読み取り

`goals.acceptance_version`は初期値1で、acceptanceの文が変わったときだけtriggerが
1増やす。`goal_updated`は`old_acceptance_version`と`new_acceptance_version`を残す。
判断はtransactionの版を保存する。`needs_recheck`は保存した版と現在の版の比較で導く。
`show`は`membership_judgements`に履歴と現在の版・要再確認を載せる。
`goal show`は`acceptance_version`と`follow_up_memberships`に元goal・現在の所属・
判断の履歴・材料・深さを載せる（移動したfollow_upも含む）。`events`は
`follow_up_registered`の登録時の材料と`follow_up_judged`の判断を読める。

閉じた元goalへのrequiredの記録は達成のeventや閉じた状態を変更しない
（下の「achievedの後の訂正」）。

## 閉じる条件

元goalがGのfollow_up（材料の`source_goal_id`が登録時に記録したか復元したG。
今の所属は問わない。元goalが不明なものは判断した者が名指してもどのgoalの
検査にも入らず、今のgoalに所属taskとして効く。ADR-t1504-2決定12(ii)）は`source_follow_ups`がtransactionの中で読み、
それぞれのstatus、Gの所属か、最後の判断（行のid・分類・要再確認）を持つ
（`SourceFollowUp`）。`completed` / `canceled`でないもので、判断が無い・
undecided・要再確認・requiredなのにGの外、のどれかがあれば、Gは
achievedで閉じられない（`domain::goal::check_follow_ups`、
`GoalFollowUpsUnsettled`）。out_of_scopeの判断を持つものの未完了は妨げない
（Gの所属taskのままなら所属taskとして今までどおり閉鎖を待たせる）。requiredで
Gの所属のものは所属taskの規則で待つ。abandonedは達成を言わないので検査しない。
acceptanceを変えると判定時の版が古くなり、同じ分類を記録し直すまで閉じない。

この検査は`close_goal_in`の中にあるので、人の`goal close --verdict achieved`
（人の言葉による代行を含む）、goal reviewの`achieved`の適用、`approve_goal`の
answerの`achieved`が同じtransactionで同じ条件を使う。goal reviewの起動の条件と
fingerprintにも入る（[Goal review](supervisor-lifecycle/goal-review.md)の2・5）。

## submit・lint・plan review

`follow_up_membership::membership_gap`がfollow_upのtaskの欠けを読み、domainの
`follow_up::membership_gap`が決める。欠けは`missing`（判断が無い）、`undecided`
（最新の判断がundecided）、`needs_recheck`（最新の判断の版が元goalの今の版より古い）。
登録時に元goalが無い（`source_goal_state: none`）もの、元goal（登録時の材料か
判断した者が名指したもの）が`abandoned`で閉じたものは欠けにしない。元goalが不明で
判断の無いものは`missing`。goal_gapとreopenedのdraftは対象にしない。

`submit`はproposalに入るdraftのうち欠けのあるものを、持ち主に依らず
（runtimeのplannerも人も）、人のadoptの検査より先に1つのエラーで拒む。
エラーはtaskごとの欠けと`judge-follow-up`を示す。人の`ready --bypass-review`も
draft / submittedのfollow_upに同じ検査をする。`lint`は同じ欠けをdraft / submittedのtaskについて
`follow_up_membership_unjudged`で出す。plan reviewのpromptはfollow_upのtaskに
`follow_up_membership`（登録時の材料、判断の行、最新の分類、版の一致）を載せ、
元goalを材料のgoalに足し、plannerの対応づけを起点に検査し疑わしいものは周辺の
証拠も読むよう指示する（[Plan review](supervisor-lifecycle/plan-review.md)）。

## achievedの後の訂正

閉じたgoalを元goalとするfollow_upの訂正も同じ`judge-follow-up`（user・inbox・
runtimeのplanner）で記録し、前の行もgoalの`goal_closed`とverdictも消さない。
out_of_scopeの訂正（所属先の誤りなど）は人に問わず、記録と開いた所属先への移動だけを行う。
元goalがachievedで閉じた後のrequired（新しい判断かout_of_scope・undecidedからの訂正。
requiredの確かめ直しは問わない。`follow_up::opens_correction`）は、同じtransactionで`correct_goal`のaskを開き、
そのIDを`follow_up_judged`のpayloadと出力の`correction_ask_id`に残す。goalは
自動で開き直さず、follow_upも動かさない。abandonedで閉じたgoalへのrequiredはaskを開かない。

askが閉じるまで、そのfollow_upの`judge-follow-up`と`set-goal`は拒む。askの中身と
`reopen` / `correct_verdict` / `keep_achieved`の適用は
[Goal review](supervisor-lifecycle/goal-review.md)の9が持つ。

## 旧schemaの移行

migration 0064は既存のfollow_upを未判定のまま残す。現在の所属と深さと
`follow_up_adopted` / `draft_adopted`のpersonの記録は変えない。
材料のtask/run/indexに一致する一意の`follow_up_registered`を登録の出どころとし、
source taskとdraft双方の、その登録の後の`task_goal_changed`を新しい順に巻き戻す。
各変更のtoが次の値（最後は現在の所属）と一致すること、from/toが整数かnullで
あることを検査する。同時刻のeventもid順で区別する。
復元した元goalと、`goal_closed`・goalの`closed_at`と登録時刻の比較、draftの
登録時の所属が一致すれば、材料に`source_goal_id`、`source_goal_state`
（open/closed/none）、`source_goal_provenance: restored`と登録event id・時刻を残す。

eventの欠落・複数の登録、履歴の矛盾、時刻の不明、閉じた状態の食い違いは
`source_goal_id: null`、`source_goal_state: unknown`、provenance unknownにする。
今の所属を元の所属と確定しない。unknownは人のadoptを要する側に倒し、判断した者の
元goalの名指しでも登録時のunknownを解消しない。migrationは判断の行を作らない。
