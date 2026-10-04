---
id: design-follow-up-membership
type: design
title: Follow-up membership judgements
status: current
created: 2026-10-04
updated: 2026-10-04
last_verified: 2026-10-04
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

このtaskが実装したのは記録・所属・登録時のadopt制限と移行。
判断をsubmitの必須条件にする検査とplan reviewの資料、goal review/closeの条件と
fingerprint、achieved後のaskと訂正の適用はADR-t1504-2の後続の実装範囲である。
閉じた元goalへのrequiredの記録は達成のeventや閉じた状態を変更しない。

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
