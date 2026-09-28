---
id: adr-t947-4
type: adr
title: taskのcancelに理由の分類コードを必ず持たせ、--duplicate-ofは中身を受け持つtaskを指す欄として残し、runtimeが自分で行うcancelは経路から理由を付ける（ADR-0063決定5をamends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0063 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - planning
  - measurement
related:
  - adr-0063
  - adr-0047
  - adr-t728-1
  - adr-t807-1
  - adr-t876-1
  - adr-t947-1
  - adr-t947-3
  - design-domain-model
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-stats
  - plan-cancel-reasons
---

# ADR-t947-4: taskのcancelに理由の分類コードを必ず持たせ、--duplicate-ofは中身を受け持つtaskを指す欄として残し、runtimeが自分で行うcancelは経路から理由を付ける（ADR-0063決定5をamends）

## Context

`dagq cancel`が構造として持つ理由は、重複の先の`--duplicate-of`（ADR-0046の決定を引き継いだ[ADR-0063](0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)決定5）だけで、ほかの理由はplannerのnoteや後継のtaskの自由文にしか無い。runtimeのplannerのcancelの多くは、どこにも理由が残っていない。

task 952の分析（[cancel-reasons](../plans/cancel-reasons.md)）では、canceledのtaskは282件（登録の29%）で、84%がfollow_upのdraftだった。理由が記録から読めたのは59%で、残りは推定した。分類は作り直し58、重複62、実装済み21、取り込み40、方針の変更26、費用に見合わない49などで、runを使ってからのcancelは2件だけ、無駄の中心は計画の側（canceledのtaskに対するruntimeのplannerの起動160回、plan review 10回）にあった。重複と実装済みは`--duplicate-of`の相手の状態で分かれ、取り込みと作り直しも相手のtaskを持つ。

goal 64は、cancelにも理由の分類を持たせると決めた（2026-09-28、人とplanner）。

## Decision

1. **cancelは理由の分類コードを必ず1つ持つ。** `dagq cancel`は理由のコードを受け、どのactor（人・inbox・planner）が打つときも、理由のコードか`--duplicate-of`（決定2でruntimeが理由を補う）のどちらかを必須にする。どれにも当たらなければ`other`にし、短い説明を添える。付けるのはcancelを決めた者で、cancelの時点で理由を最もよく知っている。迷ったら「このtaskの中身は今どこにあるか」で選ぶ。
2. **`--duplicate-of`は中身を受け持つtaskを指す欄として残す。** 重複・実装済み・取り込み・作り直しのように中身が別のtaskにある理由では、その相手を`--duplicate-of`で必ず指す（無ければ拒む）。`--duplicate-of`だけを付けたcancelは、runtimeがcancelの時点の相手の状態から「重複」（相手が開いている）か「実装済み」（相手がcompleted）を補う。ADR-0063決定5の`--duplicate-of`は「重複か実装済み」だけを指していたが、この決定で取り込み・作り直しの相手も指すように広げる。そのため`related`と`search`が重複の組として読み、`show`が`canceled (duplicate of X)`と出し、`stats`が重複のcancelとして数えるのは、重複と実装済みの2つの理由の組だけにし、取り込みと作り直しは`show`と`list`に理由とともに相手を出す（`show X`の一覧には理由を添える）。理由のコードが一覧に無い値なら、重複の組としては読まない。
3. **runtimeが自分で行うcancelは、経路から理由を付ける。** plan reviewの`cancel_duplicate`のaction、`approve_plan`・`approve_landing`・`decide`のaskの`cancel`の答えのように、runtimeが適用するcancelは、runtimeが経路に決まった理由（とaskやproposalのID）を記録し、人やjobに選ばせない。jobは状態を変えるコマンドを打たず、verdictのactionをruntimeが適用する今の形（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)）は変えない。
4. **記録はruntimeが行い、stats と kpiが理由ごとに読めるようにする。** runtimeは理由を`task_status_changed`のpayloadに載せ、`show`と`list`に出す。`stats`は理由ごとに件数・actor・登録からcancelまでの時間・readyから後のcancelの件数・cancelまでに使ったrun・plan review・runtimeのplannerの数を出し、follow_upの種類（[ADR-t947-3](2026-09-28-t947-3-follow-ups-carry-category-codes.md)）と並べて「どの種類のfollow_upがなぜ採られなかったか」を読めるようにする。`kpi`は計画の無駄（ready・submitted・plannerを経たcancel）を理由ごとの系列として出す。理由の無い過去のcancelは書き換えず、`duplicate_of`があれば2の規則で補い、無ければ「未記録」として数える。
5. **一覧と定義はdesignが持ち、ADRなしに足し引きできる。** コードはlabelとして記録し、知らない値も読める（[ADR-t876-1](2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)決定3）。一覧・定義・相手を要る理由・flagの綴りは[Domain model](../design/domain-model.md#cancelの理由の分類コード未実装)が持つ。ADR-0047決定41の`discard`（成果を捨てるかのask）は変えない。

## Alternatives

- **今のまま`--duplicate-of`とnoteだけにする**: 理由の41%が記録に無く、runtimeのplannerのcancelは推定に頼る。
- **後から分析だけで分類する**: 作り直しか方針の変更か費用に見合わないかは、cancelした者の意図で、後からは推定しかできない。
- **理由を任意にする**: 付けなくても済むと、記録が要る所（runtimeのplannerの不採用）ほど欠ける。1つ選ぶ手間は小さい。
- **人が付ける**（planner・jobのcancelの後に人が理由を選ぶ）: cancelの84%はruntimeのplannerがfollow_upのdraftを落とすもので、人はその場に居ない。後から人が選ぶのは、後からの分析と同じく推定になる。
- **`--duplicate-of`を理由のコードに畳む**（`duplicate:<ID>`のような1つの値）: 相手のtaskは`show`の`duplicates`・`related`・`search`がすでに欄として読んでいる。欄を壊さずにコードを足すほうが、過去の記録と今の読み手をそのまま保てる。

## Consequences

- cancelを打つplanner（人が開いたものとruntimeが立てたもの）のskillとpromptに理由の選び方が加わる。理由の無いcancelを打つ古い手順は拒まれる。
- cancelを減らす手（follow_upの登録時の照合、plan reviewでの決定の突き合わせなど）は、この決定に含めない。集計が揃った後に別のtaskとADRで決める。
- 実装（CLIの欄と検査、runtimeの経路の理由、記録、stats と kpiの出力）はgoal 64の別のtaskで行う。着地するまで、cancelは今の形のままである。
