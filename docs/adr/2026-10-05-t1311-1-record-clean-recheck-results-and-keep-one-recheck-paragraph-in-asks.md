---
id: adr-t1311-1
type: adr
title: landing recheckがきれいと確かめた結果も待つrunと開いているaskに残し、askのrecheckの段落は追記を重ねずに最新の結果の1つに置き換える（ADR-0068決定4をamends）
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amends:
  - adr-0068 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0068
  - adr-t1310-1
  - adr-t1091-1
  - design-supervisor-lifecycle-landing-recheck
---

# ADR-t1311-1: landing recheckがきれいと確かめた結果も待つrunと開いているaskに残し、askのrecheckの段落は追記を重ねずに最新の結果の1つに置き換える（ADR-0068決定4をamends）

## Context

[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)決定4は、landing recheckが着地しなくなったrunを見つけたときだけ、その事実を閉じていないaskのquestionの末尾に段落で足すと決めた。きれいに着地できると確かめたrunには、決定6の`landing_recheck_finished`の集計（`clean`）が増えるだけで、runにもaskにも何も残らない。人はaskを見ても、そのrunが今のmainにまだ載るのか、まだ確かめていないのかを区別できない。goal 39の受け入れ条件「mainが動いたら、待つrunがまだきれいにrebaseできるかを確かめ、runと開いているaskに記録する」を、きれいな結果について満たさない（goal review 21）。

また決定4は段落を末尾に足していくので、mainが動くたびに失敗の段落が積み重なり、どれが今のmainに対する結果かを人が読み分けることになる。

ADR-0068は番号付きの決定を6つ持ち、変えるのは決定4だけなので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)に従ってamendsで直す。

## Decision

1. **きれいな結果もrunとaskに残す。** recheckがきれいと確かめたrun（merge-treeが衝突せず、`[recheck] command`があればそれも通った）には、確かめた`main`・`head`・command（merge-treeだけならnull）・mainを動かした着地（あれば）を持つeventをrunに記録する。runのaskのうち閉じていないもの（答えの有無を問わない）のquestionに、そのmainの短いcommitと、まだきれいに着地すること（commandを流したかどうか）を書いた段落を置き、失敗のときと同じく`ask_updated`を記録する（`why`はきれいな結果と分かる値）。
2. **askのrecheckの段落は最新の結果の1つだけにする。** 失敗でもきれいでも、recheckが段落を置くときは、questionにある前のrecheckの段落を全部除いてから新しい段落を末尾に置く。段落が積み重ならず、askはいつも最新のmainに対する結果を1つだけ見せる。置いた結果のquestionが前と同じならaskを書き直さず、`ask_updated`も記録しない。
3. 変えないもの: 失敗の記録（`landing_recheck_failed`、resume・heldの扱い）と失敗の段落の文、askを閉じないこと、resumeの後に答えを待つこと（ADR-0068決定4の残り）、recheckの間にrunが動いた（headやstatusが変わった、他のprocessがleaseを取った）ときは何も記録しないこと（決定3）、`landing_recheck_finished`の集計と`stats`の数え方（決定6）。

## Alternatives

- **きれいな結果は`landing_recheck_finished`の集計だけに残す（今まで）**: どのrunがきれいだったかはrunからもaskからも読めず、人は答える前にmainに載るかを確かめられない。
- **段落を末尾に足し続ける**: 経緯は残るが、mainが動くたびにaskが長くなり、どれが今の結果かを読み分ける手間が人に移る。経緯はrunのevent（失敗ときれいな結果の記録）が持つ。
- **きれいな結果はaskにだけ書き、runには記録しない**: askの無いrun（recoverを待つもの、slotに持つもの）に残らず、`show`やstatsから読めない。

## Consequences

- 人はaskを見れば、そのrunが最後に動いたmainにまだきれいに着地するか、着地しなくなってresumeへ回ったかを、最新の1つの段落で読める。
- mainが動くたびに、待つrunごとにきれいな結果のeventが1つ増え、askの`ask_updated`が1つ増える（文が変わらなければ増えない）。
- 同じmainとheadに対してきれいと確かめたrunは、失敗と同じく同じmainに対して確かめ直さない。
- eventの種類の名前・payload・段落の文は[Landing recheck](../design/supervisor-lifecycle/landing-recheck.md)が持つ。実装とdesignの更新はtask 1311が本ADRと同時に行う。
