---
id: plan-headless-worker-measurement
type: plan
title: 本番の queue での Claude の非対話の worker と対話の worker の比較と、既定を切り替えるかの推奨
status: active
created: 2026-09-30
updated: 2026-10-02
owners:
  - hisamekms
tags:
  - measurement
  - worker
  - provider
related:
  - plan-headless-worker-spike
  - adr-t813-1
  - adr-t813-2
  - adr-t980-1
  - adr-0051
---

# 本番の queue での Claude の非対話の worker と対話の worker の比較と、既定を切り替えるかの推奨

goal 57 の測定。Claude の worker を非対話の経路（[ADR-t813-1](../adr/2026-09-28-t813-1-headless-worker-path.md)）で動かした本番の run を、同じ期間の対話の run と比べ、Claude の既定を非対話に切り替えるかを推奨する。

- 1 回目（task 821、2026-09-30）: 非対話の Claude の run が 0 本で、推奨を出さなかった。その時点の対話の run の基準値は、この文書の git の履歴にある
- 2 回目（task 1132）: ask 224 で人が了承し、planner が 7 本の task（1097・1096・1042・1058・1110・1107・1050）に `--headless` を付けた。7 本とも Claude の非対話で着地したので、推奨を出す
- 3 回目（task 1200、2026-10-02）: ask 242 で人が了承し、planner 589 が推奨の条件 2 のために 6 本の task（1021・1053・1098・1119・1129・1197）に `--headless` を付けた。6 本とも Claude の非対話で着地した。推奨の 3 つの条件がそろったかは「[3 回目](#3-回目task-1200)」に書く

この task は測って書くだけで、既定は変えていない。`add` で経路を指定しない task は今も Claude の対話の経路。

## 結論

**条件付きで、Claude の既定を非対話に切り替えることを推奨する。** 下の「推奨」の 3 つの条件がそろってから、既定の経路を決める ADR を書いて切り替える。

**3 回目（2026-10-02）の時点で、条件はまだそろっていない。** 条件 1（task 1179 の着地）と条件 3（cost の記録）はそろった。条件 2 は、L の task と `[e2e] paths` に触れる task が非対話の Claude で着地したが、`worker_question` の answer の turn と `needs_session` の resume は 6 本のどれにも起きなかった。同じ 2 つの流れは、Claude から Codex にフォールバックした非対話の run で本番を通っている（「[3 回目](#3-回目task-1200)」）。

**人の決定（2026-10-02）**: 人が planner の session で、条件 2 の残り（2a・2b）を待たずに Claude の既定を非対話にすると決め、既定を切り替える ADR と実装を task 1340（goal 85）として登録した（task 1200 の note 60084）。この文書は測定の記録として条件 2 を未達のまま残し、既定の切り替えは task 1340 が行う。この task（1200）は既定を変えていない。

以下の「結論」の箇条書きから「測定を続けるには」までは 2 回目（task 1132）の内容。

- 7 本とも 1 回目の turn が `succeeded` で終わった。失敗・停止・催促・権限の拒否・人への ask・resume は 0 件だった。review の差し戻し（1 件）は、同じ session の resume の turn で直って着地した
- 同じ area・大きさの対話の run より、work は短く（層ごとの中央値で約 40〜55% 短い）、worker の token は少ない（大きさをそろえた層の中央値で約 40〜75% 少ない）。load の高い帯で動いた run が多いのに短かった
- ただし標本は 7 本で、6 本が大きさ S。worker の質問の answer が次の turn として届く流れ、`needs_session` の resume、L の task、`[e2e] paths` に触れる task は本番でまだ 1 本も通っていない。差を断定はしない（「交絡の扱い」）

## 読んだ範囲と方法

- 読んだ時点: 2026-09-30T02:32Z（UTC）ごろ。非対話の 7 本の claim は 2026-09-30T00:22Z〜02:08Z
- 比べる期間: 2026-09-29T12:00Z〜2026-09-30T03:00Z に claim され、着地（`integrated`）した Claude の run。対話 52 本、非対話 7 本。Codex の非対話の run（task 1103、run `0a8042c2`）は Claude の経路の比較に入れない
- 固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+b60e763`）で、状態を変えないコマンドだけを使った

```sh
dagq stats --full                    # 677 run。runs[] の route / provider / actual_provider / startup / work / tokens / turns / work_breakdown
dagq kpi --since 2026-09-29T12:00:00Z --by provider --by route
dagq kpi --since 2026-09-29T12:00:00Z --by route --area runtime
dagq events --run <run> --all --full --limit 2000   # 7 本それぞれ。turn_started / turn_finished / turn_requested / stall_* / ask_opened
dagq events --all --full --kind ask_opened --since 2026-09-29T12:00:00Z
dagq events --all --full --kind run_claimed --kind agent_started --kind receipt_observed --kind exit_requested --kind session_exited --since 2026-09-29T12:00:00Z
dagq events --all --kind provider_switched --kind provider_waiting --since 2026-09-29T12:00:00Z   # 0 件
dagq timeline c6ef5297-f626-41a8-ac91-97cccd00e2f7
```

中央値と層の値は `stats --full` の `runs` を jq で絞って求めた。`kpi` の `route=headless` の層は Codex の 1 本を含む 8 本で、`--area runtime` と `--by route` は掛け合わされない（`area=runtime` と `route=*` が別の層に出る）。そのため比較の表は `stats` の runs から作り、`kpi` の値は照合に使った。

## 非対話で動かした Claude の run の一覧

| task | run | change | area | 予測の大きさ | claim の load の帯 | turn の数 | 結果 |
|---|---|---|---|---|---|---|---|
| 1107 | `4fa32d81` | docs | docs | S | 16-32 | 1 | 着地（review pass） |
| 1096 | `b1adcaab` | test | tests | S | 16-32 | 1 | 着地（review pass） |
| 1042 | `934022a1` | fix | docs+runtime | S | 16-32 | 1 | 着地（review pass） |
| 1058 | `c6ef5297` | test | tests | S | 8-16 | 1 | 着地（review pass） |
| 1050 | `2d1f5abd` | fix | docs+runtime | M | 16-32 | 2 | 着地（review revise 1 回の後に pass） |
| 1097 | `b80aa96e` | test | tests | S | 8-16 | 1 | 着地（review pass） |
| 1110 | `aeb46a29` | docs | docs | S | 4-8 | 1 | 着地（review pass） |

- 7 本とも `provider` = `actual_provider` = `claude`、`provider_switches` は 0。model は `claude-opus-5-5`、effort は `medium`（対話の 52 本も同じ）。Claude Code は 2.1.283（対話の 52 本も同じ）
- 7 本の change は `test` 3・`fix` 2・`docs` 2。`refactor`・`feature` は無い

run ごとの時間と token（秒。`startup` は `agent_started`→最初の commit、`work` は `agent_started`→receipt。token は worker の区間）:

| task | startup | work | turn の時間 | claim→着地 | wait_to_land | worker の token（total / output） | `cost_usd` |
|---|---|---|---|---|---|---|---|
| 1107 | 92 | 107 | 50 | 613 | 502 | 38.6 万 / 2,167 | 0.56 |
| 1096 | 302 | 316 | 326 | 1063 | 732 | 79.7 万 / 4,349 | 0.70 |
| 1042 | 312 | 323 | 326 | 1460 | 1129 | 123.2 万 / 6,454 | 1.48 |
| 1058 | 1664 | 1688 | 1701 | 2156 | 451 | 491.9 万 / 33,417 | 3.10 |
| 1050 | 1237 | 1425 | 1849（2 turn） | 3832 | 2395 | 783.6 万 / 44,474（revise の turn は別に 367.2 万 / 17,799） | 4.72（revise の turn は 6.02 と記録。下の「cost の記録」） |
| 1097 | 319 | 558 | 562 | 2070 | 1503 | 215.8 万 / 12,197 | 2.13 |
| 1110 | 21 | 31 | 34 | 71 | 31 | 34.2 万 / 2,148 | 0.57 |

## 同じ期間の対話の run との比較

全体（中央値。`needs_session` と resume は 1 run あたりの平均）:

| 値 | 非対話（7） | 対話（52） |
|---|---|---|
| startup（秒） | 312 | 666 |
| work（秒） | 323 | 964 |
| `work_breakdown` の model（秒） | 67 | 190 |
| `work_breakdown` の idle（秒） | 25 | 43 |
| `work_breakdown` の test（秒） | 188 | 195 |
| wait_to_land（秒） | 732 | 720 |
| claim→着地（秒） | 1460 | 1790 |
| 同（`land_phases.ask` を除く） | 1460 | 1778 |
| claim→`agent_started`（秒） | 2.1 | 2.3 |
| `needs_session` / run | 0 | 0.21 |
| resume / run | 0 | 0.15 |
| integrate の検証の失敗のあった run | 1（flaky だけ） | 9（うち flaky でないもの 3） |
| review の revise のあった run | 1 | 5 |
| worker の token total | 123.2 万 | 406.6 万 |
| worker の token output | 6,454 | 19,980 |
| 人に届いた ask（run に紐づくもの） | 0 | 5（run 3 本。下） |

全体の値は task の大きさの違いを含む（非対話は 7 本中 6 本が S、対話は 52 本中 S 14・M 27・L 8・不明 3）。area と大きさで層に分けると:

| 層 | 本数（非対話 / 対話） | startup | work | claim→着地 | worker の token total | output |
|---|---|---|---|---|---|---|
| docs だけ、S | 2 / 5 | 57 / 86 | 69 / 151 | 342 / 518 | 36.4 万 / 134.2 万 | 2,158 / 7,650 |
| tests だけ（非対話は S、対話は M 6・L 1） | 3 / 7 | 319 / 452 | 558 / 974 | 2070 / 1980 | 215.8 万 / 377.7 万 | 12,197 / 19,664 |
| runtime を含む、S | 1 / 5 | 312 / 501 | 323 / 684 | 1460 / 1738 | 123.2 万 / 331.7 万 | 6,454 / 14,657 |
| runtime を含む、全て（非対話は S 1・M 1、対話は S 5・M 18・L 7・不明 2） | 2 / 32 | 775 / 936 | 874 / 1165 | 2646 / 1939 | 453.4 万 / 719.9 万 | 25,464 / 30,530 |
| 大きさ S の全て | 6 / 14 | 307 / 160 | 320 / 332 | 1262 / 1374 | 101.4 万 / 184.7 万 | 5,402 / 10,968 |

`kpi --since 2026-09-29T12:00:00Z --by provider --by route` の照合（中央値。`route=headless` は Codex の 1103 を含む 8 本）: `phase.work` は headless 319 / interactive 971、`phase.startup` は 307 / 698、`session_active.worker` は 326 / 763、`resumes_per_run` は 0.0 / 0.228。`asks_per_landing` は `all` の 0.391（64 着地）だけで経路の層は無い。どれも本数が `min_samples`（5）を超えるが、前の期間の値が無いので判定（judged）は無い。

読み方:

- **work と startup**: 同じ層（docs の S、tests、runtime の S）ではどれも非対話が短い。`work_breakdown` では model の時間（67 秒 / 190 秒）が主な差で、test の時間はほぼ同じ（188 / 195）。大きさ S の全てでは work がほぼ同じ（320 / 332）で startup は逆に非対話が長いが、非対話の S は tests と runtime が 4 本、対話の S は docs だけが 5 本と ci+docs・config+docs が 4 本と中身が違う（下の「交絡」）
- **token**: 大きさをそろえた 3 つの層（docs の S・tests・runtime の S）では非対話が少ない（total で約 40〜75%、output で約 40〜70%）。大きさをそろえない runtime を含む全ての層では total で 37%、output で 17% 少ないだけ。対話の区間は transcript から、非対話の区間は turn の `result.usage` から数える（[provider-lifecycle](../design/provider-lifecycle.md)）ので、数え方の違いが差の一部を作りうる
- **claim→着地**: 着地の待ち（wait_to_land）は経路と関係の無い integrate の直列の待ちが支配し、全体で 732 / 720 とほぼ同じ。tests の層は非対話の方が長い（2070 / 1980）のは 1097 の wait_to_land（1503 秒）のため
- **startup の意味**: startup は最初の commit までなので、1 turn で作業して最後に commit する非対話では work とほぼ同じになる（work − startup の中央値 11 秒）。対話では最初の commit の後の test・subagent review・receipt の時間が約 300 秒あり、この差は経路の違いというより worker の作業の順番の違いとして読む
- **人に届いた ask**: 窓の中で claim された run に紐づいて開いた ask は、対話の run の 5 件（task 890 の `stuck_exit`・`blocked`・`approve_landing`、task 1067 の `decide`、task 699 の `worker_question`）。非対話は 0。窓の前に claim された run の ask（task 1047 の `blocked`、14:36Z）は数えない。ほかに queue の `queue_hold`（`cost`）が 22:07Z〜22:23Z に 4 件あり、非対話の run の期間の前に閉じている
- **Claude の `cost_usd`**: 非対話は turn ごとに `total_cost_usd` を記録する。7 本の worker の区間の中央値は 1.48 USD、合計 13.3 USD（1050 の revise の turn を除く）。対話の区間は記録されないので比べられない

## run ごとの、非対話で起きた問題

| task | `turn_finished` の outcome / failure | stall の reason | 催促（`turn_requested` の `nudge`） | `permission_denials` | そのほか |
|---|---|---|---|---|---|
| 1107 | succeeded / null（6 turns 内部、48 秒） | 無し | 0 | 0 | 無し |
| 1096 | succeeded / null（12、325 秒） | 無し | 0 | 0 | follow-up の draft 1 件 |
| 1042 | succeeded / null（16、325 秒） | 無し | 0 | 0 | 無し |
| 1058 | succeeded / null（49、1700 秒） | 無し | 0 | 0 | run の途中で supervisor の引き継ぎ（`supervisor_handed_off`）があり、そのまま着地。background の test（611 秒）を turn の中で待った |
| 1050 | 1 turn 目 succeeded / null（63、1431 秒）、2 turn 目（`revise request` の resume）succeeded / null（19、414 秒） | 無し | 0 | 0 | review が `test_gap` で revise、同じ session id の resume で直して pass。integrate の検証で `plan_review::` の test が落ちたが全て flaky で、着地を 1 回やり直して着地（`integration_retried`、`verification_flaky`。ADR-t768-1）。supervisor の引き継ぎあり |
| 1097 | succeeded / null（28、560 秒） | 無し | 0 | 0 | 着地の push が 1 回 `push_failed`（`cannot lock ref 'refs/heads/main'`。main が別の着地で動いた競合で、経路と関係しない）。run は `integrated` |
| 1110 | succeeded / null（5、32 秒） | 無し | 0 | 0 | 無し |

- 7 本とも `exit_code` 0、`stopped` null、`denied_tools` は空。`stall_*`・`provider_switched`・`provider_waiting`・`ask_opened` の event は 7 本のどれにも無い
- 1050 は worker_question を出しやすい task として選ばれたが、worker は ask を出さずに決めた。answer が次の turn として届く流れは本番ではまだ通っていない

### cost の記録

1050 の 2 turn 目（`revise request` の resume）の `turn_finished` は `cost_usd` 6.02 を記録した。同じ turn の token（cache read 363 万・output 1.8 万）は 1 turn 目（cache read 763 万・output 4.4 万、4.72 USD）の半分以下なので、6.02 は resume した session の累計（4.72 + 約 1.30）と見られる。design（[非対話の worker](../design/supervisor-lifecycle/headless-worker.md) の `turn_finished`、[provider-lifecycle](../design/provider-lifecycle.md) のturnのトークン数）は Claude の `total_cost_usd` をその turn のものとして扱い、`stats` の run の `turns.by_provider.claude.cost_usd` は 2 turn を足した 10.74 になる。標本は 1 件なので、receipt の follow-up にした。token の数は turn ごとの値に見え、この比較の token の値には影響しない。

## 交絡の扱い

[ADR-0051](../adr/0051-kpi-time-series-report-and-push.md) の前後比較の規則に従う。経路は時刻ではなく task ごとの指定で決まるので、`--compare` の前後比較ではなく、同じ窓の中の経路の層を並べた。

- **同じ期間**: 窓（2026-09-29T12:00Z〜2026-09-30T03:00Z）に `kpi` の印は supervisor の起動と引き継ぎ（auto-update による build の入れ替え）が 24 件、人の印が 1 件（17:52Z の「goal review を Codex に」。worker には効かない）、Claude Code の version の変化が 1 件（窓の始まりの 13:33Z に 2.1.284 → 2.1.283）あり、dagq の build は非対話の 7 本で 4 種類、対話の 52 本で 22 種類だった。Claude Code の version（2.1.283）・model・effort・`parallel`（3）は両側で同じ。非対話の 7 本は約 2 時間（00:22Z〜02:08Z）に集まり、対話は約 15 時間に広がる。build ごとには分けていない（非対話の側が 1〜3 本ずつになるため）
- **task の種類と大きさ**: planner が目安どおり小さな task を選んだので、非対話は S が 7 本中 6 本、対話は S が 52 本中 14 本。そのため上の表では area（docs だけ・tests だけ・runtime を含む）と大きさ（S）で層に分けた。同じ層の中では非対話が短く、token が少ない。大きさ S の全体では work がほぼ同じで、これは S の中の area の偏り（非対話は tests と runtime、対話は docs と ci・config が多い）による。`fix`・`test` の多い非対話と、`feature`・change の無い古い task が多い対話で change も偏る
- **host の負荷**: claim の時点の load の帯は、非対話が 16-32 に 4 本・8-16 に 2 本・4-8 に 1 本、対話が 16-32 に 5 本・8-16 に 25 本・4-8 に 20 本・0-4 に 2 本。非対話の方が高い負荷の帯に偏るので、負荷は非対話を遅く見せる向きに効く。帯ごとの中央値は非対話の本数（1〜4 本）が少なすぎて並べていない
- **人の答え待ち**: 両側とも `land_phases.ask` はほぼ 0 で、除いても値はほとんど変わらない（claim→着地の中央値で対話 1790 → 1778、非対話は変わらない）
- **少ない本数**: 7 本の中央値は 1 本で動く。差の向き（work が短い・token が少ない）は 3 つの層でそろうが、大きさは断定しない。`kpi` の判定は前の期間が無いので出ていない

## 推奨

**条件付きで切り替える。** 次の 3 つがそろったら、既定の経路を決める ADR を書いて Claude の既定を非対話にする。

1. **task 1179 の着地**: 非対話の run の `stalled` の ask から `intervene` を外し、届いた `intervene` の答えで run が行き止まりにならないようにする（今 in progress）。既定にすると `stalled` の ask に当たる run が増える
2. **本番でまだ通っていない流れを 1 回ずつ通す**: worker の質問の answer が次の turn として届く流れ（`worker_question`）、`needs_session` の resume（integrate の検証の失敗か rebase の衝突）、大きさ L か `[e2e] paths` に触れる task。それぞれ 1 本以上の非対話の Claude の run が着地するまで、planner が人の了承のもとで `--headless` を付け続ける
3. **cost の記録の確認**: 上の「cost の記録」を確かめ、累計なら turn ごとの値に直す。切り替えの判断には効かないが、切り替えた後の cost の比較（`kpi`）が誤る

理由:

- 7 本の全ての turn が成功し、止まり・催促・権限の拒否・provider の切り替え・人への ask が無かった。対話の経路が自動修正で扱っている画面・ダイアログ・`/exit` の問題（`stuck_exit` など）は、非対話では起きようがない
- 同じ層の比較で work が短く token が少ない。model の時間が短いのは、画面の判定や Stop hook を待たずに 1 回の呼び出しで作業を終えるためと見られる
- review の revise は同じ session の resume で直り、supervisor の引き継ぎも 2 本の run を跨いで問題が無かった
- 切り替えない理由になる問題は見つからなかった。ただし標本が小さく、上の 2 の流れは測れていないので、無条件には切り替えない

切り替えると失うもの: 非対話の run には画面が無く、人が worker の terminal を見て割り込むことはできない（`stalled` の ask に指示の文で答えるか、turn の出力を run dir の `turns/` で読む）。人が画面で見たい task は `--interactive` で対話に戻せるように残す。

### 切り替えるときに要る変更

- **ADR**: Claude の既定の経路を非対話にする決定を、[ADR-t813-1](../adr/2026-09-28-t813-1-headless-worker-path.md) の決定 7（「対話の経路は Claude の既定に残る」「既定を切り替えるかは測定の後に別の ADR で決める」）を amends する小さな ADR として書く。repository ごとに選べる設定にするか、runtime の既定そのものを変えるかもこの ADR で決める
- **`add` の既定**: 経路を指定しない task の `worker_mode` の既定（`add` / `edit` と domain の task の既定値）。`--interactive` を `add` でも受けるようにする。登録済みの draft / ready の task の経路を保存の値で持つか既定の参照で持つかを確かめ、切り替えの時点の task をどちらに寄せるかを決める
- **`dagq.toml` の設定**（repository ごとに選べるようにするなら）: 例えば `[tasks]` か `[supervisor]` に既定の経路の欄を足し、この repository は非対話にする。欄の追加は古い固定バイナリが読めないので、固定バイナリが対応してから `dagq.toml` に書く
- **plugin の文書**: `plugins/claude-dagq/skills/dagq/reference/provider.md` の経路の表と「Claude headless (`--headless`): only when the person asks for it」、`dagq-planner` skill の経路の選び方
- **AGENTS.md と design**: AGENTS.md の「worker の provider と経路」（「指定の無い task は Claude の対話の経路」「人が言ったときだけ Claude の非対話の `--headless` を付け」）、[provider-lifecycle](../design/provider-lifecycle.md) と [非対話の worker](../design/supervisor-lifecycle/headless-worker.md) の既定の記述

## 測定を続けるには（planner 向け）

推奨の条件 2 のために、planner は人の了承（task の context に書く）のもとで、次の task に `--headless` を付ける（[provider の選び方](../../plugins/claude-dagq/skills/dagq/reference/provider.md)）。

- 判断を含み worker_question を出しやすい task を 1〜2 本（1050 は ask を出さなかった）
- runtime の M の task を 2〜3 本。integrate の検証の失敗による `needs_session` の resume が起きうる
- L の task か `[e2e] paths` に触れる task を 1 本。途絶えの停止（900 秒）と turn の上限（14400 秒）に e2e の長い待ちが重なっても turn が止まらないかを見る

集めた後は、この文書の比較の表に run を足し、`turn_finished` の `outcome`・`failure`、`stall_*` の reason、催促の回数、`permission_denials` を run ごとに書き、条件がそろったかを書く。同じ窓の対話の run と、area と大きさの層で並べることは変えない。

## 3 回目（task 1200）

推奨の条件 2 のために `--headless` を付けた 6 本（task 1132 の receipt の follow-up、ask 242 で人が adopt、planner 589 が付けた）の着地の後に、本番の queue を読んだ。

### 読んだ範囲と方法

- 読んだ時点: 2026-10-02T03:10Z（UTC）ごろ。6 本の claim は 2026-10-02T00:36Z〜02:24Z、着地は 00:40Z〜02:51Z
- 比べる期間: 2026-10-01T12:00Z〜2026-10-02T03:00Z に claim され、着地した、`actual_provider` が `claude` の run。前の run の commit を引き継いだと見られる run（`startup` が 60 秒未満。2026-09-30 の Codex へのフォールバックの期間に失敗した task のやり直しが多い）は両側から外した。外したのは対話 15 本、非対話 3 本。比べるのは planner が測定のために `--headless` を付けた 6 本だけにし、同じ窓のほかの非対話の run（下の一覧の後の 4 本）は入れない
- 固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+2f81af3`）で、状態を変えないコマンドだけを使った

```sh
dagq stats --full                                    # 829 run。runs[] の route / provider / actual_provider / prediction / startup / work / turns / tokens / work_breakdown / land_phases / e2e
dagq events --run <run> --all --full --limit 2000    # 6 本それぞれ。turn_started / turn_finished / turn_requested / review_* / run_e2e_finished / supervisor_handed_off
dagq events --all --full --kind ask_opened --since 2026-09-30T00:00:00Z --limit 2000   # 52 件。非対話の run に紐づくものを run_id で絞った
dagq events --run <run> --all --full                 # Codex にフォールバックした run 8e2e0094・3f7a1148・e12b273b（下の「Codex で通った流れ」）
dagq timeline fb9ba422-9b2c-4ebe-ad1d-642af0bbe79c   # 1098 の turn の中の background の test
dagq kpi --since 2026-10-01T12:00:00Z --by provider --by route --area runtime
```

`kpi` の `route=headless` の層は 27〜31 本で、Codex の run（フォールバックを含む）が 17〜21 本を占める（`provider=codex` と `route` は掛け合わされない）。Claude の非対話の 6 本だけの値は出ないので、比較の表は `stats` の runs から作った。照合の値: `phase.work` は headless 436 / interactive 688、`session_active.worker` は 441 / 620、`resumes_per_run` は 0.968 / 0.286（headless は Codex の resume を含む）、`area=runtime` の `phase.work` は 1060（33 本）。

### 非対話で動かした Claude の run の一覧（3 回目）

| task | run | change | area | 予測の大きさ | claim の load の帯 | turn の数 | 結果 |
|---|---|---|---|---|---|---|---|
| 1053 | `4aa1430a` | config | ci+docs | M | 8-16 | 1 | 着地（review pass） |
| 1021 | `62f3a648` | feature | docs+plugin+runtime+tests | L | 4-8 | 2 | 着地（review revise 1 回の後に pass、着地の前の e2e passed） |
| 1098 | `fb9ba422` | fix | docs+runtime | M | 4-8 | 1 | 着地（review pass、着地の前の e2e passed） |
| 1119 | `6000103e` | fix | docs+runtime+tests | M | 4-8 | 1 | 着地（review pass） |
| 1129 | `b2174d09` | fix | docs+plugin+runtime+tests | M | 4-8 | 2 | 着地（review revise 1 回の後に pass） |
| 1197 | `c488e968` | fix | docs+runtime+tests | M | 0-4 | 1 | 着地（review pass、着地の前の e2e passed） |

- 6 本とも `provider` = `actual_provider` = `claude`、`provider_switches` は 0。model は `claude-opus-5-5`、effort は `medium`、Claude Code は 2.1.286（比べる対話の run も同じ）
- area は着地の差分から求めた値（[ADR-t980-1](../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）。予測の大きさは plan review の `prediction.size`。計画のときの見込み（1021・1053 は判断を含む task、1098 は `[e2e] paths` に触れる task）どおり、1021 は L、1098・1021・1197 は `validation_finished` の `e2e_requirement` が `paths` で着地の前の e2e を流した
- 同じ窓には、ほかに非対話の Claude の run が 4 本ある（task 1144・1095・1087・1115。2026-10-01T15:53Z〜17:43Z）。どれも 2026-09-30 に Codex へフォールバックして失敗した task の次の run で、`worker_mode` が `headless` で claim された。1 turn で着地し、ask・resume・停止は無い。1095 のほかは `startup` が 60 秒未満で前の run の commit を引き継いだと見られるので、比較の表には入れていない

run ごとの時間と token（秒。定義は 2 回目と同じ。turn の時間は dagq の `turn_started`→`turn_finished`）:

| task | startup | work | turn の時間 | claim→着地 | wait_to_land | worker の token（total / output） | `cost_usd`（turn ごと） |
|---|---|---|---|---|---|---|---|
| 1053 | 86 | 188 | 193 | 236 | 38 | 126.7 万 / 14,647 | 1.53 |
| 1021 | 4269 | 4446 | 4533（2 turn。revise の turn は 80） | 5607 | 1148 | 850.4 万 / 35,835（revise の turn は別に 116.6 万 / 2,836） | 4.54 + revise 0.34 |
| 1098 | 4123 | 4144 | 4149 | 4855 | 699 | 177.3 万 / 10,749 | 2.49 |
| 1119 | 2028 | 2389 | 2398 | 3154 | 751 | 797.1 万 / 49,099 | 5.35 |
| 1129 | 737 | 964 | 1116（2 turn。revise の turn は 147） | 1903 | 926 | 661.7 万 / 40,396（revise の turn は別に 150.6 万 / 9,398） | 4.43 + revise 0.60 |
| 1197 | 439 | 570 | 578 | 1635 | 1052 | 383.8 万 / 22,236 | 2.66 |

### 同じ期間の対話の run との比較（3 回目）

中央値（`needs_session` と resume は 1 run あたりの平均）。対話は 20 本（S 5・M 8・L 7）:

| 層 | 本数（非対話 / 対話） | startup | work | `work_breakdown` の model | test | claim→着地 | worker の token total | output |
|---|---|---|---|---|---|---|---|---|
| 全て | 6 / 20 | 1383 / 459 | 1677 / 590 | 863 / 297 | 357 / 233 | 2529 / 1555 | 522.7 万 / 381.6 万 | 29,036 / 28,793 |
| runtime を含む、M | 4 / 3 | 1383 / 822 | 1677 / 1060 | 863 / 362 | 357 / 309 | 2529 / 2267 | 522.7 万 / 934.1 万 | 31,316 / 36,741 |
| runtime を含む、L | 1 / 6 | 4269 / 2411 | 4446 / 3114 | 1274 / 1233 | 1167 / 888 | 5607 / 5377 | 850.4 万 / 3427.6 万 | 35,835 / 118,602 |
| runtime を含まない、M | 1 / 5 | 86 / 237 | 188 / 427 | 134 / 159 | 0 / 0 | 236 / 671 | 126.7 万 / 316.3 万 | 14,647 / 16,873 |

| 値 | 非対話（6） | 対話（20） |
|---|---|---|
| `needs_session` / run | 0 | 0.35 |
| resume / run | 0 | 0.2 |
| review の revise のあった run | 2 | 8 |
| integrate の検証の失敗のあった run | 0 | 1 |
| 人に届いた ask（run に紐づくもの） | 0 | 4（run 2 本。task 1236 の `approve_landing`、task 1218 の `stalled` 2 件と `approve_landing`） |

読み方:

- **work と startup**: 2 回目と違い、runtime を含む M と L の層では非対話の方が長い。M の 4 本のうち 1098（work 4144 秒）は turn の中で background の test を 2 回（1028 秒と 808 秒。`timeline` の `commands`）待ち、1119（2389 秒）は足した test の stress を 2 回流した（turn の出力）。どちらも turn が自分の test の終わりを待つ時間で、2 回目の 1058 と同じ形。対話の M の 3 本はどれも 8-16 の帯、非対話の M の 4 本は 0-4 か 4-8 の帯で、負荷は非対話を速く見せる向きに効く。本数は M で 4 / 3、L で 1 / 6 で、経路の差として断定はしない
- **token**: runtime を含む M・L と runtime を含まない M のどれでも非対話の worker の token は少ない（total で約 45〜75%、output で約 13〜70%）。2 回目と同じ向き。全てでは非対話が多いが、非対話は M と L だけ、対話は S を 5 本含む
- **review の revise**: 1021・1129 とも `test_gap` で、同じ session id の resume（`turn_requested` の `revise request`、`turn_started` の `resume: true`）の 1 turn で直って pass した
- **人の答え待ち**: 6 本とも `land_phases.ask` は 0

### run ごとの、非対話で起きた問題（3 回目）

| task | `turn_finished` の outcome / failure | stall の reason | 催促（`nudge`） | `permission_denials` | そのほか |
|---|---|---|---|---|---|
| 1053 | succeeded / null（17 turns 内部） | 無し | 0 | 0 | 無し |
| 1021 | 1 turn 目 succeeded / null（62）、2 turn 目（`revise request`）succeeded / null（7） | 無し | 0 | 1（`Write`） | 1 turn 目の途中に supervisor の引き継ぎ（00:41Z）があり、そのまま続いた。`Write` の拒否は 1 回で、turn は成功した。着地の前の e2e は 222 秒で passed（`requirement.paths` は `src/application/integrate.rs`） |
| 1098 | succeeded / null（16） | 無し | 0 | 0 | turn の途中に supervisor の引き継ぎ（00:41Z）。turn の中の background の test 1028 秒の間も途絶えの停止（900 秒）にかからなかった。着地の前の e2e は 124 秒で passed（`src/infrastructure/adapters.rs`） |
| 1119 | succeeded / null（67） | 無し | 0 | 0 | turn の途中（02:10Z）と review の途中（02:13Z）に supervisor の引き継ぎ。review は引き継ぎで attempt 2 から始め直して pass（経路と関係しない） |
| 1129 | 1 turn 目 succeeded / null（55）、2 turn 目（`revise request`）succeeded / null（8） | 無し | 0 | 0 | turn の途中と review の途中に supervisor の引き継ぎ。review は attempt 2 で `test_gap` の revise、resume の turn で直して attempt 3 で pass |
| 1197 | succeeded / null（37） | 無し | 0 | 0 | turn の途中に supervisor の引き継ぎ（02:27Z）。着地の前の e2e は 187 秒で passed（`src/infrastructure/sessions.rs`） |

- 6 本とも `exit_code` 0、`failure` null。`stall_*`・`provider_switched`・`provider_waiting`・`ask_opened`・`needs_session` の event は 6 本のどれにも無い。integrate の検証は 6 本とも 1 回で通った
- Claude の `result` の `duration_ms` は、turn の中で background の処理を待った run では dagq の turn の時間より短い（1098 は 323 秒 / 4149 秒、1021 の 1 turn 目は 821 秒、1119 は 1640 秒 / 2398 秒）。上の表の turn の時間は dagq の値を使った

### 推奨の条件がそろったか

| 条件 | そろったか | 根拠 |
|---|---|---|
| 1. task 1179 の着地 | そろった | run `2f1f9628` が 2026-09-30T03:14Z に `integrated` |
| 2a. `worker_question` の answer の turn | 非対話の Claude では起きなかった | 6 本（と同じ窓のほかの 4 本）に `ask_opened` が無い。判断を含む task として選んだ 1021（ask から inbox が見るまでの KPI）・1053（CI の失敗を issue にする）も worker は ask を出さずに決めた |
| 2b. `needs_session` の resume | 非対話の Claude では起きなかった | 6 本とも `needs_session` 0・resume 0。integrate の検証は 1 回で通り、rebase の衝突も無かった |
| 2c. L か `[e2e] paths` に触れる task | そろった | L の 1021（`62f3a648`、turn 4453 秒）と、`[e2e] paths` に触れる 1098（`fb9ba422`）・1021・1197（`c488e968`）が非対話の Claude で着地した。どれも着地の前の e2e が passed（`run_e2e_finished`）。1 turn が 4149 秒の 1098 でも途絶えの停止・turn の上限（14400 秒）にかからなかった |
| 3. cost の記録 | そろった | task 1199（run `989a0924`、2026-09-30T03:31Z 着地）の後、revise の turn の `turn_finished` は `cost_usd` にその turn の値（1021: 0.34、1129: 0.60）、`session_cost_usd` に累計（4.88、5.03）を記録した。`stats` の `turns.by_provider.claude.tokens.cost_usd` は 4.88・5.03 で、2 回目の 1050 のような二重の数え上げは無い |

**3 つの条件はそろっていない。** 条件 2 の 2a と 2b が非対話の Claude で起きていない。

#### Codex で通った流れ

2a と 2b の流れは、2026-09-30 に Claude の利用上限で Claude から Codex にフォールバックした非対話の run（`provider_switched` の `reason: provider_disabled`）で本番を通っている。

- `worker_question` の answer の turn: task 784 の run `8e2e0094`（ask 263）は、answer の後に `turn_requested`（`what: answer of ask 263`）が送られ、次の turn が `succeeded` で終わった。task 824 の run `3f7a1148` も `worker_question` を開いた
- `needs_session` の resume: task 1028 の run `e12b273b` ほか、フォールバックした run の多くが `turn_requested`（`resolution request`）の turn で直して着地した（`stats` の `needs_session`・`resumes` が 1 以上で `integrated` の run が 20 本）

answer と resume の turn を送る runtime の側（`turn_requested`、session id での resume の呼び出し）は provider に依らない。Claude の `--resume` の呼び出しそのものは、revise の turn で 3 回（2 回目の 1050、3 回目の 1021・1129）通っている。Claude で通っていないのは「answer と resolution の依頼の文を Claude の resume の turn として渡す」組み合わせだけで、リスクは小さいと見る。ただし推奨の条件は「非対話の Claude の run が着地すること」なので、そろったとはしない。

### 続け方

- **人の決定**: 2026-10-02 に人が、2a・2b を待たずに既定を非対話に切り替えると決めた（note 60084）。既定を切り替える ADR と実装は task 1340（goal 85）が行う。「切り替えるときに要る変更」はその task が扱う範囲の手がかりとして残す
- **切り替えの後に確かめる**: 切り替えの後は経路を指定しない task が非対話の Claude で動くので、2a・2b は普通の run で起きる見込みが高い（同じ窓の対話の M・L の run（15 本）の `needs_session` は 1 run あたり 0.33、runtime を含む L は 0.83。`worker_question` は 2026-09-30 以降の ask 52 件のうち 2 件と稀）。task 1340 の着地の後、最初の `worker_question` の answer の turn と `needs_session` の resume の turn が非対話の Claude で成功したかを `turn_requested`（`answer of ask N`・`resolution request`）と `turn_finished` で確かめる。非対話の経路の問題が見つかったら、それを直す task にする
- 次にこの文書に足すときは、同じ形（一覧・run ごとの時間・同じ窓の対話の run との層の比較・run ごとの問題・条件の表）で足す
