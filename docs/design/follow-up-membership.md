---
id: design-follow-up-membership
type: design
title: Follow-up membership judgements
status: current
created: 2026-10-04
scope: runtime
related:
  - adr-t1504-2
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle-draft-planners
---

# Follow-up membership judgements

`judge-follow-up`はfollow_upの所属を記録する（形と各flagの規則は`--help`）。
user、inbox（人の代行）、runtimeのplannerだけが`follow_up.judge`を持つ。
worker、job、supervisorは持たない。
taskの状態は問わない。
分類の意味の正しさはplannerとplan reviewが判断する（ADR-t1504-2）。

## CLIと行

out_of_scopeは元と別の、閉じていないgoalを名指す。
所属先の優先度が`low`より高ければ、所属先の受け入れ条件のうちそのfollow_upを要る項目の名指しも要し、無ければ同じラベルの受け皿の探し方を示して拒む（ADR-t3325-1）。
`low`の所属先（受け皿）は名指しを要さない。
runtimeが見るのは名指しの有無と所属先の優先度だけで、項目の当否はplannerとplan reviewが判断する。
検査はdomainの`follow_up::check_destination_acceptance`で、storeの`judge_follow_up`が所属先を読む同じtransactionで呼ぶ。
requiredの所属先は元goalで、別の所属先は拒む。
未知の分類、follow_up以外、元goalが無いと確定しているfollow_upは拒む。

`follow_up_judgements`は追記の行で、最後の行が今の判断。
分類・必須の欄・遷移はdomainの`MembershipJudgement::validate`とstoreが検査する。
UPDATEとDELETEはtriggerで拒む。
判断が無い状態から3分類へ、undecidedからrequired/out_of_scopeへ進める。
同じ分類の確かめ直しも許す。
requiredとout_of_scopeの間の訂正は理由と最後の行の名指しを要し、undecidedへ戻すのは拒む。

登録時の元goalが不明なら、判断する者が証拠で照らしたgoalを名指す。
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
goal_gapのdraftは所属の判断の対象でないが、runtimeのplannerの`set-goal`が今のgoalの外（別のgoalか`--none`）へ移すのは拒む（ADR-t1504-2決定10、domainの`follow_up::check_goal_gap_move`をstoreが同じtransactionで呼ぶ）。
人（user・inbox）の移動とcancel（`--duplicate-of`を含む）は拒まない。
拒否は`Planning`がtransactionのrollbackの後に`authorization_denied`として記録する（[Authorization](authorization.md)）。
元のtask/run/goal、登録時の状態、provenanceと`follow_up_depth`はどの所属変更でも保つ。
登録時に元goalが無い・閉じていた・不明、現在の所属goalが無い・閉じている、
深さ`FOLLOW_UP_ASK_DEPTH`以上のいずれかなら、runtimeのplannerのsubmitは人のadoptなしに拒む（`proposals::submit`が`follow_up::adopt_needs_person`で検査）。
既存の人のadoptは聞き直さず、adopt後のsubmitは深さを0に戻す。

## acceptanceの版と読み取り

`goals.acceptance_version`は、acceptanceの文が変わったときだけtriggerが1増やす。
判断はtransactionの版を保存する。
`needs_recheck`は保存した版と現在の版の比較で導く。
`show`と`goal show`が載せる判断と版の意味は`TaskDetail`と`GoalDetail`のdoc commentが持つ。
`events`は`follow_up_registered`の登録時の材料と`follow_up_judged`の判断を読める。

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
（Gの所属taskのままなら所属taskとして閉鎖を待たせる）。
requiredで
Gの所属のものは所属taskの規則で待つ。abandonedは達成を言わないので検査しない。
acceptanceを変えると判定時の版が古くなり、同じ分類を記録し直すまで閉じない。

この検査は`close_goal_in`の中にあるので、人の`goal close --verdict achieved`
（人の言葉による代行を含む）、goal reviewの`achieved`の適用、`approve_goal`の
answerの`achieved`が同じtransactionで同じ条件を使う。goal reviewの起動の条件と
fingerprintにも入る（[Goal review](supervisor-lifecycle/goal-review.md)の2・5）。

所属taskが全て終わり（`goal_review::tasks_done`）、この検査だけでgoal reviewが起動しないopenのGは、`status`のattentionに`goal_follow_ups_unsettled`として1件出る。
止めているfollow-upのtask IDと理由の分類（`SourceFollowUp::unsettled_reason`）を並べ、runtimeのplannerが扱う途中の判断の無いfollow_upのdraft、`draft_planner_exhausted`のdraft、closeされていないaskのあるtaskは数えず、`goal_review_failed`で人を待つGには出さない（判定は`follow_up::unshown_unsettled`）。
inboxはplanning requestでplannerに`judge-follow-up`を頼み、判断を記録すれば消える。
runtimeは所属を判定も移動もしない（ADR-t1504-1決定5）。項目の全体は
`application::health::attention`の一覧。

## submit・lint・plan review

`follow_up_membership::membership_gap`がfollow_upのtaskの欠けを読み、domainの
`follow_up::membership_gap`が決める（欠けの種類と意味は`MembershipGap`）。
登録時に元goalが無い（`source_goal_state: none`）もの、元goal（登録時の材料か
判断した者が名指したもの）が`abandoned`で閉じたものは欠けにしない。元goalが不明で
判断の無いものは`missing`。goal_gapとreopenedのdraftは対象にしない。

`submit`はproposalに入るdraftのうち欠けのあるものを、持ち主に依らず
（runtimeのplannerも人も）、人のadoptの検査より先に1つのエラーで拒む。
エラーはtaskごとの欠けと`judge-follow-up`を示す。人の`ready --bypass-review`も
draft / submittedのfollow_upに同じ検査をする。`lint`は同じ欠けをdraft / submittedのtaskについて
`follow_up_membership_unjudged`で出す。plan reviewのpromptはfollow_upのtaskに
`follow_up_membership`を載せ、
元goalを材料のgoalに足し、plannerの対応づけを起点に検査し疑わしいものは周辺の
証拠も読むよう指示する（[Plan review](supervisor-lifecycle/plan-review.md)）。
out_of_scopeの判断が名指した所属先の受け入れ条件の項目も載せ、plan reviewはそれが所属先の受け入れ条件にあり、そのfollow_upを要るかを照らす。

## achievedの後の訂正

閉じたgoalを元goalとするfollow_upの訂正も同じ`judge-follow-up`（user・inbox・
runtimeのplanner）で記録し、前の行もgoalの`goal_closed`とverdictも消さない。
out_of_scopeの訂正（所属先の誤りなど）は人に問わず、記録と開いた所属先への移動だけを行う。
元goalがachievedで閉じた後のrequired（新しい判断かout_of_scope・undecidedからの訂正で、requiredの確かめ直しは除く。`follow_up::opens_correction`）は、同じtransactionで`correct_goal`のaskを開き、そのIDを判断の記録に残す。
goalは自動で開き直さず、follow_upも動かさない。
abandonedで閉じたgoalへのrequiredはaskを開かない。

askが閉じるまで、そのfollow_upの`judge-follow-up`と`set-goal`は拒む。askの中身と
答えの適用は
[Goal review](supervisor-lifecycle/goal-review.md)の9が持つ。

## 旧schemaの移行

既存のfollow_upの移行の契約は[ADR-t1504-2](../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定12とmigration 0064が持つ。
