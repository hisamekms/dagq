---
id: adr-t1639-1
type: adr
title: goalに5段階の優先度を持たせて正本にし、taskは個別の指定が無ければ所属のgoalから継ぎ、goalの優先度の変更は個別の指定の無い未着手のtaskの次のclaimに効き、既存のtaskは効く値を変えずに移し、goalのラベルの語彙をdagq.tomlに置き、goal listを優先度順にしてラベルで絞り、goalの入れ子とgoal間のrankは作らない（ADR-0049決定4をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0049 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - goal
  - priority
  - configuration
related:
  - adr-t1639-2
  - adr-0049
  - adr-0040
  - adr-t791-1
  - adr-0009
  - adr-0038
  - adr-t1504-1
  - design-domain-model
  - design-persistence
---

# ADR-t1639-1: goalの優先度を正本にしてtaskが継ぎ、goalにラベルを付けてgoal listを優先度順にし、goalの入れ子とrankは作らない（ADR-0049決定4をamends）

## Context

taskの5段階の優先度は[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定4で入り、2026-09-26に[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4が丸ごと引き継いだ（ADR-0040はsupersededで、今の決定はADR-0049決定4と[ADR-t791-1](2026-09-28-t791-1-effective-priority-ignores-tasks-waiting-on-abandoned-goals.md)）。goalには優先度もテーマのラベルも無く、`dagq goal list`はIDの順に並ぶだけなので、今どのgoalがどの順で進み何に関わるかが人から見えにくい。同じgoalのtaskの優先度を揃えるのもtaskごとの`set-priority`の手作業で、揃え漏れが起きる。ADR-0049決定4のclaim順の2番目に置いた「goalの`rank`」（goal 13で入れる予定）も未実装のまま残っている。

2026-10-04に人がrequest 11で次を決めた: goalの階層化（入れ子・parent）は要らない、ラベル付けと優先度付けは要る、taskの優先度はinboxのsessionで出た提案（goalの優先度を正本にしてtaskが継ぐ）のとおり、既存の値は変えない。このADRはそれを決定にする。後回しの判定と受け皿のgoalは[ADR-t1639-2](2026-10-04-t1639-2-defer-improvements-outside-acceptance-to-a-low-goal-per-tag.md)が持つ。

## Decision

1. **goalにADR-0049決定4と同じ5段階の優先度を持たせ、それを正本にする。** 段階と順（`low` < `normal` < `high` < `urgent` < `interrupt`）はtaskと同じで、goalの追加で指定が無ければ`normal`。goalの追加と編集で付けて変えられる。
2. **taskの優先度は、個別の指定があればそれ、無ければ所属のgoalの優先度、goalの無いtaskは`normal`にする。** `add --priority`と`set-priority`は個別の指定を置く。`set-priority`で個別の指定を外し、goalから継ぐ状態に戻せる（綴りはruntimeのtaskが決めて`docs/design/`に書く）。個別の指定はgoalの中の一部のtaskだけを上げ下げするために残す。taskの所属のgoalが変われば（[ADR-0009](0009-goal-groups-tasks.md)の付け替え）、個別の指定の無いtaskは新しいgoalの優先度を継ぐ。
3. **goalの優先度の変更は、そのgoalの個別の指定の無い未着手（`draft`・`submitted`・`ready`）のtaskの次のclaimに効き、走っているrunは止めない。** ADR-0049決定4の「優先度の変更は次のclaimにだけ効く」「走っているrunを割り込みで止めない」をgoalの変更にも当てはめる。claimされた後（`in_progress`以降）のtaskの優先度は変えない。
4. **効く優先度は、2の継いだ値（または個別の指定）を各taskの優先度の起点にして、今と同じく計算する。** 自分と、自分を推移的に待っている`ready`のtaskの優先度の最大値で、辿り方と継承から除くtask（`draft`・`canceled`・`completed`、draftのgoalのtask、ADR-t791-1の`abandoned`で閉じたgoalに依存するtask）は変えない。ADR-0049決定4の「taskに付けた優先度」を「2で決まるtaskの優先度」に読み替える。
5. **既存のtaskは効く値を変えずに移す。** 移行で`normal`のtaskは継ぐ状態に、`normal`以外のtaskは同じ値の個別の指定に読み替える。既存のgoalの優先度は既定の`normal`になるので、移行の前後でどのtaskの優先度も効く優先度も変わらない（既存の値を変えないことは人の指示）。既存のgoalに優先度を付けるのは移行の後に人か権限のあるplannerが行う。
6. **goalにラベル（tag）を0個以上付け、語彙をrepositoryの`dagq.toml`に置く。** 置き場は`[areas]`・`[tasks]`の`changes`と同じ`dagq.toml`で、欄の名前はruntimeのtaskが決めて`docs/design/`に書く。語彙があれば、goalの追加と編集は語彙の外のラベルを拒む（揺れと綴りの違いで絞り込みから漏れるのを防ぐ）。語彙の無いrepositoryはラベルの形だけを検査する。この repositoryの語彙と各ラベルの意味は`dagq.toml`と開発文書が持つ。
7. **`dagq goal list`は優先度の降順 → IDの昇順で並べ、`--tag`で絞り、各goalの優先度とラベルを出し、taskが0件の`draft`のgoalも出す。** 人が「今どれを先にやるか」と「何に関わるか」を一覧で読めるようにする。taskの無いdraftのgoalは計画の途中の印なので、一覧から落とさない。
8. **goalの入れ子（parent）・goal間のrank・roadmap・祖先のgoalの継承は作らない。** 人の決定（request 11）。goal 13が予定した`rank`の役（今どれをやるか）はgoalの優先度（1）が担う。ADR-0049決定4のclaim順の2番目の「goalの`rank`（goal 13で入れる。未実装の間は飛ばす）」は除き、claim順は「効く優先度の降順 → 解放数の降順 → IDの昇順」にする。goalの間の順序の制約は今までどおりgoal依存（[ADR-0038](0038-task-depends-on-a-goal-until-it-is-achieved.md)）で表す。

## Alternatives

- **taskだけが優先度を持つまま（今の形）**: goalの順が見えず、同じgoalのtaskの値を揃える手作業と揃え漏れが残る。
- **goalの優先度をtaskの優先度に足す・掛ける（合成）**: 値の意味が5段階から外れ、人が付けた個別の指定がどう効くかを読めなくなる。継ぐか個別の指定かの2択にする。
- **goalの優先度の変更をclaimされたtaskにも効かせる・走っているrunを止める**: ADR-0049決定4の「割り込みで止めない」に反し、手戻りが出る。次のclaimだけに効かせる。
- **移行で既存のtaskを全部継ぐ状態にする**: goalの既定が`normal`なので、`normal`以外のtaskの効く値が変わる。人の指示（既存の値を変えない）に反する。
- **ラベルを自由な文字列にする**: 綴りの揺れ（`codex`と`Codex`など）で`--tag`の絞り込みから漏れる。語彙を`[areas]`と同じく`dagq.toml`に置く。
- **goalの入れ子・rank・roadmapを作る（goal 13の案）**: 人がrequest 11で要らないと決めた。順はgoalの優先度、テーマはラベル、前後の制約はgoal依存で足りる。

## Consequences

- goalの優先度を1回変えれば、そのgoalの個別の指定の無い未着手のtaskの順がまとめて変わる。個別の指定を持つtaskはgoalの変更に追従しないので、`show`と`graph`はtaskの優先度が個別の指定か継いだ値かを示す必要がある（形は`docs/design/`）。
- schemaにgoalの優先度とラベル、taskの個別の指定の有無が加わり、migrationが要る。新しいflagと`dagq.toml`の欄を案内するpluginと設定の変更は、それを知るバイナリが固定バイナリに入ってから着地させる。
- `docs/design/`（domain model・persistence・claim順）はこのADRの実装のtaskが今の姿に直す。このADRは`docs/design/`を変えない。
- 既存のopen・draftのgoalの優先度とラベルの初期値は、goal 106の棚卸しのtaskが案にし、適用は人か権限のあるplannerが行う。
