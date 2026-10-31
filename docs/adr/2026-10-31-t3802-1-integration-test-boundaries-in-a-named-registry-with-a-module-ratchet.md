---
id: adr-t3802-1
type: adr
title: tests/itの境界を名前付きの登録簿にし、差分の関門が代表の無い新しいtest・同じidの2本目・新しいidなしのmoduleの本数の増加を落とす（ADR-t3762-1決定1・2をamends）
status: accepted
created: 2026-10-31
updated: 2026-10-31
accepted_on: 2026-10-31
amends:
  - adr-t3762-1 decision 1
  - adr-t3762-1 decision 2
owners:
  - hisamekms
tags:
  - testing
  - ci
  - review
related:
  - adr-t3762-1
  - adr-t1410-1
  - adr-t1707-1
  - adr-t2268-1
  - development-testing
  - development-task-registration
---

# ADR-t3802-1: tests/itの境界を名前付きの登録簿にし、差分の関門が代表の無い新しいtest・同じidの2本目・新しいidなしのmoduleの本数の増加を落とす（ADR-t3762-1決定1・2をamends）

## Context

[ADR-t3762-1](2026-10-31-t3762-1-integration-tests-added-by-a-diff-declare-the-boundary-they-keep.md)は、差分がtests/itに足したtestに守る境界の種類と自由記述の理由の宣言を求めた。
宣言は境界の種類（SQLite・Git・プロセス・supervisorの配線・復旧とadopt）しか持たないので、「supervisorの配線」と書けば同じ境界の2本目（R）も通り、機械はRを見分けられない。
時間の関門の許可の一覧の約380件が理由に「supervisorの配線」を挙げており、宣言が同じ形で増えればITの増え直しは止まらない（goal 239）。
ADR-t3762-1が単一ファイルの数のratchetを退けた理由（並行のtaskが同じファイルの数を取り合って衝突する）は今も成り立つ。

## Decision

1. **境界の登録簿**: tests/itが守る境界は、境界のid・境界の種類・代表のtestの完全な名前・理由・足したtaskを持つ項目の登録簿にする。1つのidは代表のtestを1本だけ持ち、1本のtestは1つのidだけを代表する。登録簿は足すtaskのファイル（`.config/it-boundaries.d/t<task ID>.toml`）を合わせた一覧で、別々のtaskの追加が同じファイルに触れない（ADR-t3762-1決定2・ADR-t1707-1と同じ形）。書式・idの形・数え方は関門のscriptの冒頭が持つ。

2. **差分の関門**: baseからの差分について、runのプログラムのreviewとCIの関門は次を落とす（場所はADR-t3762-1決定3のまま）。
   - (a) tests/itに新しく現れたtestで、移しただけでもなく、どのidの代表でもないもの。足すには、既存のidの代表をそのtestに入れ替える（元の代表は消すか、別のidの代表にするか、unit testへ移す）か、新しいidを足す。
   - (b) 同じidを持つ項目が2つ以上あるもの（R）と、2つのidが代表に持つtest。
   - (c) module（tests/itのファイル）ごとの`#[test]`の本数をbaseとwork treeで数え、増えた本数がそのmoduleに代表を持つ新しいid・入ってきた代表の入れ替え・移してきたtestの数から出ていった代表の入れ替えの数を引いたものを超えるもの。数はファイルに保存せず毎回baseから数えるので、並行のtaskが衝突しない。入れ替えた元の代表を残すと、このratchetで1本の超過になる。
   移しただけのtestの扱いと、既存のtestの本文の変更を対象にしないことはADR-t3762-1決定1のまま。

3. **新しいidの承認**: 新しいidは、項目に足したtaskのIDを書き、そのtaskのacceptanceかdescriptionがidを名指していること（plan reviewが見る）と、理由がそのtestでしか確かめられない境界を書いていること（test-rulesのagentが見る）で承認とする。人がtaskを登録したときは人の承認に当たる。機械（関門のscript）は項目の形と数だけを見て、承認を見ない。

4. **古い置き場を読む移行の期間**: 今の規則で登録され着地していないITを足すtaskのrunは、rebaseの後も`.config/it-boundary.d/t<ID>.toml`に古い書式（`name`・`boundary`・`reason`・`task`）の項目を書く。関門はこの古い置き場も読み続け、古い書式の項目1つを、そのtestを代表に持つ新しいid1つ（`t<task>:<testの完全な名前>`）と読む。だから古い規則のrunは(a)を満たし、(c)で新しいid1つと数え、(b)にも同じに当たる。古い書式の項目の形の誤りは今と同じに落ちる。新しい項目は登録簿に書き、古い置き場には書かない。古い置き場を読むのをやめて退役するのは、この決定に依存する後のtaskで行う（reviewのプログラムはlanding branchのscriptで流れるので、旧い形と新しい形を両方通す変更を先に着地させる。`docs/development/task-registration.md`の「reviewのプログラムが読む形を変えるtask」）。

## Alternatives

- **宣言に「同じ境界を代表するtestの名前」を書き足すだけ**: 書かなければRを見つけられず、自由記述の境界が増える形は残る。idで境界を名指し、2本目を項目の重複として機械で落とす方を採る。
- **tests/itの本数の上限を1つのファイルで持つratchet**: ADR-t3762-1と同じ理由（並行のtaskの衝突）で採らない。moduleの本数は毎回baseから数え、増やせる分は差分が足した新しいidから出す。
- **新しいidの承認を機械でplan reviewの記録に結ぶ**: runtimeにこのrepositoryのtestの構成を入れることになる（ADR-t2268-1）。承認はtaskの文面とplan review・test-rulesのagentに置く。
- **古い置き場をこの変更ですぐ読まなくする**: 今の規則で走っているITを足すtaskのrunが、rebaseの後にCIとプログラムのreviewで落ちて差し戻される。移行の期間を置き、退役は後のtaskに分ける。

## Consequences

- 判断のcaseを並べたITと、同じ境界の2本目は、idを持てないのでagentのreviewの前にプログラムのreviewで返る。
- 新しいITを足すtaskは、登録時に名指すidを決めることになる（taskの文面とplan reviewの規則は`docs/development/task-registration.md`、規則の本文は`docs/development/testing.md`の「判断と境界のtest」）。
- 登録簿は分類（goal 239）が境界の代表のtestにidを入れる置き場になる。idの無い既存のtestは関門の対象にならない（差分で新しく現れたtestと本数の増加だけを見る）。
- testを消すか名前を変えるtaskは、そのtestを代表に持つ項目を同じ変更で直す（scriptは無いtestの代表を警告するだけ）。
