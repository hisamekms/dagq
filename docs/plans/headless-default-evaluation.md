---
id: plan-headless-default-evaluation
type: plan
title: Claude の worker の既定を非対話にした変更（task 1340、印 61916）の基準値と評価のコマンドと、既定を対話に戻す基準の案
status: active
created: 2026-10-02
updated: 2026-10-02
owners:
  - hisamekms
tags:
  - measurement
  - worker
  - provider
related:
  - plan-headless-worker-measurement
  - adr-t1340-1
  - adr-t813-1
  - adr-t980-1
  - adr-0051
---

# Claude の worker の既定を非対話にした変更（task 1340、印 61916）の基準値と評価のコマンドと、既定を対話に戻す基準の案

goal 86 の task 1368 の測定。task 1340（[ADR-t1340-1](../adr/2026-10-02-t1340-1-claude-worker-defaults-to-headless.md)）で、経路を指定しない Claude の task の worker が非対話になった。既定は固定バイナリ `0.4.0-dev+c05e8644` の自動更新（2026-10-02T08:05:36Z）から効いていて、その時刻に印 61916（「Claude worker default → headless (task 1340)」、`--at` で効いた時刻を指す）がある。

この文書は、1 週間後（2026-10-09 以降）に切り替えを評価するための準備で、次の 4 つを書く。

1. 基準値: 印の前の 7 日の Claude の worker の run を、経路・area・大きさの層に分けた値
2. 非対話に固有の止まり方の基準値
3. 1 週間後に同じ値を読む評価のコマンド
4. 既定を対話に戻す基準の案と理由（戻すかどうかは人が決める）

測って書くだけで、既定も設定も変えていない。読み方と層の切り方は [非対話の worker の測定](headless-worker-measurement.md)（task 1132・1200）の比較の表に合わせた。

## 読んだ範囲と方法

- 読んだ時点: 2026-10-02T11:00Z（UTC）ごろ。固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+87aa72af`）で、状態を変えないコマンドだけを使った（worker の session のクライアントモードで queue service から読んだ）
- 基準の窓: `2026-09-25T08:05:36Z` 以上 `2026-10-02T08:05:36Z` 未満に claim された、`actual_provider` が `claude` の worker の run。1 の表は窓の終わりまでに着地した run（565 本）と `failed`・`interrupted` の run（12 本）で、窓の後に着地した 4 本は入れない。event も窓の終わりまで（`--until`）だけを数える。どちらも、読む日によって値が変わらないようにするため（後の窓も同じ規則で読む）
- 経路: `stats` の run の `route`。`route` は 2026-09-27T18:14Z より前の claim では記録されていない（`null`、298 本）。その時点で Claude の非対話の経路はまだ無かったので、`null` は対話に数えた（`kpi` は `route=unknown` の層に分ける）
- area: 着地 commit の差分から求めた `areas`（[ADR-t980-1](../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）を、task 1132 と同じく「runtime を含む」（`runtime`・`migrations`・`broker` のどれか）、「tests（runtime なし）」、「docs だけ」、「その他」（ci・config・plugin など）に寄せた
- 大きさ: plan review の `prediction.size`。古い task は予測を持たず「大きさ不明」になる（対話の 295 本）
- 比べない run: Claude から Codex にフォールバックした run（97 本。`provider_switched` の `provider_disabled`、2026-09-30 の Claude の無効の期間）と Codex の task の run（3 本）は、Claude の経路の比較から外し、止まり方の表で参考として並べる

```sh
dagq stats --full                      # 850 run（読んだ時点で終わっていた run）。runs[] の route / actual_provider / areas / prediction / startup / work / validate / wait_to_land / land_phases / resumes / needs_session / review_reasons / verify_failures / tokens / turns
dagq kpi --compare 2026-09-25T08:05:36Z..2026-10-02T08:05:36Z,2026-10-02T08:05:36Z..2026-10-09T08:05:36Z --by provider --by route --cross
dagq kpi --compare 2026-09-25T08:05:36Z..2026-10-02T08:05:36Z,2026-10-02T08:05:36Z..2026-10-09T08:05:36Z --by provider --by route --area runtime --cross
dagq events --all --full --kind <下の「評価のコマンド」の kind> --since 2026-09-25T08:05:36Z --until 2026-10-02T08:05:36Z --limit 20000
```

## 1. 基準値（印 61916 の前の 7 日）

時間は秒の中央値。率は着地した run 1 本あたり。表の値は全て下の「評価のコマンド」の `route.jq` の出力（層の名前の対応は `route.jq` の後に書く）。

- **人の答え待ちを除く**: 人の答え待ちを、それが入っている区間から 1 回だけ引く。work の区間（`agent_started`→receipt）の中の待ち（`run_waiting_ended` の時刻から `waited_secs` だけ遡った区間のうち、receipt の時刻（`validated_at` − `validate`）− work から receipt の時刻までに重なる秒。worker の作業中の `worker_question`・`stalled` の答え待ち）は work と claim→着地の両方から引く。receipt の後の待ち（review の差し戻しや resume の最中の `worker_question` など）は wait_to_land の中にあり、着地の段の ask の待ち（`land_phases.ask`）が数えているので、claim→着地から `land_phases.ask` を引くだけにして、待ちの秒を重ねて引かない（例: task 699 の run `b47b26cd` は revise の最中に 11,366 秒待ち、その待ちは `land_phases.ask` の 11,690 秒に入っている。work は 245 秒のまま）。括弧は人の答え待ちのあった run の数
- **1 回で通った率**: run 単位で、resume・`needs_session`・review の revise・着地のやり直し（`integrate_attempts` が 2 以上）がどれも 0 の run の割合。`kpi` の `first_pass_rate`（task 単位で、task の run が 1 本だけのもの）とは違う。`kpi` の値は表の後に並べる
- **revise**: review の差し戻しのあった run の割合。**検証の失敗**: `integrate` の検証が 1 回以上落ちた run の割合（流し直しで通った flaky だけのものを含む）
- **token**: worker の区間（`tokens.by_kind.worker`）。対話は transcript から、非対話は turn の `result.usage` から数える（数え方の違いは task 1132 の「読み方」）

| 層 / 経路 | 着地（`failed`・`interrupted`） | startup | work | work（待ちを除く） | validate | wait_to_land | claim→着地 | 同（人の答え待ちを除く） | 1 回で通った率 | resume / run | `needs_session` / run | revise | 検証の失敗 | worker の token total | output |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 全て / 非対話 | 17（0） | 312 | 558 | 558 | 10 | 732 | 1619 | 1619（0） | 0.82 | 0.00 | 0.00 | 0.18 | 0.06 | 161.1 万 | 10,749 |
| 全て / 対話 | 548（12） | 698 | 996 | 976 | 10 | 483 | 1638 | 1632（37） | 0.76 | 0.22 | 0.31 | 0.14 | 0.10 | 491.9 万 | 27,607 |
| runtime を含む、全て / 非対話 | 10（0） | 588 | 802 | 802 | 11 | 838 | 1769 | 1769（0） | 0.70 | 0.00 | 0.00 | 0.30 | 0.10 | 280.6 万 | 16,492 |
| runtime を含む、全て / 対話 | 359（0） | 966 | 1212 | 1212 | 10 | 621 | 1996 | 1996（27） | 0.70 | 0.31 | 0.44 | 0.16 | 0.14 | 706.7 万 | 35,930 |
| runtime を含む、S / 非対話 | 1（0） | 312 | 323 | 323 | 6 | 1129 | 1460 | 1460（0） | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 123.2 万 | 6,454 |
| runtime を含む、S / 対話 | 15（0） | 483 | 684 | 684 | 8 | 621 | 1467 | 1467（0） | 0.93 | 0.07 | 0.07 | 0.00 | 0.13 | 232.9 万 | 11,905 |
| runtime を含む、M / 非対話 | 8（0） | 588 | 802 | 802 | 11 | 725 | 1769 | 1769（0） | 0.75 | 0.00 | 0.00 | 0.25 | 0.13 | 280.6 万 | 16,492 |
| runtime を含む、M / 対話 | 91（0） | 868 | 1126 | 1126 | 10 | 752 | 1999 | 1999（5） | 0.70 | 0.21 | 0.31 | 0.19 | 0.13 | 613.5 万 | 31,870 |
| runtime を含む、L / 非対話 | 1（0） | 4269 | 4446 | 4446 | 11 | 1148 | 5607 | 5607（0） | 0.00 | 0.00 | 0.00 | 1.00 | 0.00 | 850.4 万 | 35,835 |
| runtime を含む、L / 対話 | 56（0） | 1772 | 1956 | 1931 | 12 | 1352 | 3564 | 3446（9） | 0.48 | 0.57 | 0.95 | 0.32 | 0.36 | 2001.2 万 | 75,756 |
| runtime を含む、大きさ不明 / 対話 | 197（0） | 860 | 1114 | 1114 | 10 | 464 | 1737 | 1737（13） | 0.75 | 0.29 | 0.39 | 0.11 | 0.09 | 574.5 万 | 29,413 |
| tests（runtime なし） / 非対話 | 4（0） | 310 | 437 | 437 | 12 | 1031 | 1844 | 1844（0） | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 147.8 万 | 8,273 |
| tests（runtime なし） / 対話 | 49（0） | 576 | 744 | 744 | 9 | 521 | 1634 | 1634（3） | 0.92 | 0.06 | 0.06 | 0.04 | 0.10 | 301.9 万 | 17,415 |
| docs だけ、全て / 非対話 | 2（0） | 56 | 69 | 69 | 5 | 266 | 342 | 342（0） | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 36.4 万 | 2,158 |
| docs だけ、全て / 対話 | 80（0） | 280 | 384 | 384 | 10 | 33 | 565 | 548（5） | 0.85 | 0.04 | 0.04 | 0.14 | 0.00 | 264.0 万 | 21,706 |
| docs だけ、S / 非対話 | 2（0） | 56 | 69 | 69 | 5 | 266 | 342 | 342（0） | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 36.4 万 | 2,158 |
| docs だけ、S / 対話 | 14（0） | 111 | 152 | 152 | 10 | 36 | 298 | 298（0） | 0.93 | 0.00 | 0.00 | 0.07 | 0.00 | 126.5 万 | 9,292 |
| その他 / 非対話 | 1（0） | 86 | 188 | 188 | 9 | 38 | 236 | 236（0） | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 126.7 万 | 14,647 |
| その他 / 対話 | 60（0） | 182 | 301 | 301 | 9 | 44 | 431 | 431（2） | 0.83 | 0.03 | 0.05 | 0.15 | 0.00 | 178.2 万 | 9,571 |
| 大きさ S の全て / 非対話 | 6（0） | 307 | 320 | 320 | 8 | 617 | 1262 | 1262（0） | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 101.4 万 | 5,402 |
| 大きさ S の全て / 対話 | 52（1） | 156 | 270 | 270 | 8 | 396 | 805 | 805（1） | 0.85 | 0.06 | 0.08 | 0.12 | 0.06 | 167.4 万 | 9,758 |
| 大きさ M の全て / 非対話 | 10（0） | 310 | 606 | 606 | 10 | 725 | 1627 | 1627（0） | 0.80 | 0.00 | 0.00 | 0.20 | 0.10 | 169.2 万 | 12,698 |
| 大きさ M の全て / 対話 | 138（3） | 726 | 1016 | 1006 | 10 | 600 | 1790 | 1782（8） | 0.77 | 0.14 | 0.21 | 0.15 | 0.11 | 561.9 万 | 31,320 |
| 大きさ L の全て / 非対話 | 1（0） | 4269 | 4446 | 4446 | 11 | 1148 | 5607 | 5607（0） | 0.00 | 0.00 | 0.00 | 1.00 | 0.00 | 850.4 万 | 35,835 |
| 大きさ L の全て / 対話 | 63（1） | 1747 | 1936 | 1908 | 12 | 1158 | 3426 | 3365（10） | 0.51 | 0.51 | 0.84 | 0.32 | 0.32 | 1734.4 万 | 74,267 |

`kpi` の照合（`--by provider --by route --cross` の前の窓。`provider` は切り替えの後の provider）:

| `kpi` の層 | 本数 | `phase.work` | `phase.startup` | `phase.wait_to_land` | `session_active.worker` | `first_pass_rate`（task 単位） | `resumes_per_run` |
|---|---|---|---|---|---|---|---|
| `provider=claude\|route=headless` | 17 | 558 | 312 | 732 | 562 | 0.588 | 0.0 |
| `provider=claude\|route=interactive` | 256 | 985 | 669 | 613 | 852 | 0.68 | 0.221 |
| `provider=claude\|route=unknown`（route の記録の前。全て対話） | 294 | 999 | 710 | 372 | 819 | 0.779 | 0.266 |
| `area=runtime\|provider=claude\|route=headless` | 10 | 802 | 588 | 838 | 809 | 0.4 | 0.0 |
| `area=runtime\|provider=claude\|route=interactive` | 150 | 1309 | 996 | 808 | 1239 | 0.613 | 0.32 |
| `area=runtime\|provider=claude\|route=unknown` | 205 | 1197 | 936 | 464 | 1044 | 0.732 | 0.307 |
| `area=runtime`（全ての provider・経路） | 374 | 1227 | 980 | 655 | — | 0.66 | 0.332 |

非対話の `first_pass_rate`（task 単位）が run 単位の率より低いのは、17 本のうちに 2026-09-30 に Codex へフォールバックして失敗した task の次の run（task 1144・1095・1087・1115）が入り、task としては 2 本目の run になるため。

### Claude の `cost_usd`

非対話の run だけが turn ごとに `total_cost_usd` を記録する（対話の区間は記録されないので経路の比較はできない。worker の token を代わりの目安にする）。値は `stats` の `turns.by_provider.claude.tokens.cost_usd`（USD）。

| 層（非対話） | 本数 | 中央値 | 合計 | 最大 |
|---|---|---|---|---|
| 全て | 17 | 1.61 | 47.01 | 10.74 |
| runtime を含む、全て | 10 | 2.58 | 37.31 | 10.74 |
| runtime を含む、S | 1 | 1.48 | 1.48 | 1.48 |
| runtime を含む、M | 8 | 2.58 | 30.94 | 10.74 |
| runtime を含む、L | 1 | 4.88 | 4.88 | 4.88 |
| tests（runtime なし） | 4 | 1.62 | 7.04 | 3.10 |
| docs だけ、全て | 2 | 0.56 | 1.13 | 0.57 |
| docs だけ、S | 2 | 0.56 | 1.13 | 0.57 |
| その他 | 1 | 1.53 | 1.53 | 1.53 |
| 大きさ S の全て | 6 | 1.09 | 8.53 | 3.10 |
| 大きさ M の全て | 10 | 2.05 | 33.59 | 10.74 |
| 大きさ L の全て | 1 | 4.88 | 4.88 | 4.88 |

最大の 10.74 は task 1050（run `2d1f5abd`）で、task 1199 より前の revise の turn が session の累計を記録した二重の数え上げを含む（[非対話の worker の測定](headless-worker-measurement.md) の「cost の記録」）。task 1199 の後の run は turn ごとの値。

### 人に届いた ask（run に紐づくもの）

窓に claim された run（`failed`・`interrupted` を含む）に `run_id` で紐づき、窓の終わりまでに開いた `ask_opened`。`route.jq` の `runs`・`runs_with_ask`・`asks`・`asks_by_kind`。

| 経路 / 層 | run | ask のあった run | ask | ask / run | kind ごと |
|---|---|---|---|---|---|
| 非対話 / 全て | 17 | 0 | 0 | 0.000 | — |
| 非対話 / runtime を含む | 10 | 0 | 0 | 0.000 | — |
| 対話 / 全て | 560 | 60 | 84 | 0.150 | `approve_landing` 32、`blocked` 19、`decide` 12、`worker_question` 11、`stuck_exit` 5、`stalled` 4、`answer_prompt` 1 |
| 対話 / runtime を含む | 359 | 39 | 49 | 0.136 | `approve_landing` 23、`blocked` 15、`worker_question` 4、`stuck_exit` 3、`stalled` 3、`answer_prompt` 1 |

run に紐づかない queue の `queue_hold`（`cost`）は窓に 24 件（2026-09-29 に 13 件）。経路ではなく Claude の利用上限の記録なので、上の表には入れない。

## 2. 非対話に固有の止まり方の基準値

窓に claim された run に紐づき、窓の終わりまでに記録された event（`--since 2026-09-25T08:05:36Z --until 2026-10-02T08:05:36Z`）を、run の provider と経路で分けた。run の本数は窓に claim された run の数（窓の後に着地した run を含む）。下の「止まり方を events から数える」の出力。Codex の 2 列は参考（Claude の非対話の run が 17 本と少ないため、同じ非対話の経路の Codex の run の止まり方を並べる）。

| event | Claude 非対話（17 run） | Claude 対話（564 run） | Codex（Claude からのフォールバック 97 run） | Codex の task（3 run） |
|---|---|---|---|---|
| `turn_finished` の outcome / failure | 20 turn、`succeeded`/null 20 | —（turn を持たない） | 124 turn: `succeeded`/null 119、`failed`/`usage_limit` 4、`stopped`/`other` 1 | 6 turn、`succeeded` 6 |
| `turn_finished` の `permission_denials` が 1 以上の turn | 1（task 1021 の `Write` 1 回。turn は成功） | — | 50 | 1 |
| `turn_requested` の依頼 | `revise request` 3 | — | `resolution request` 43、`revise request` 5、provider の切り替え 4、`answer of ask` 1 | `resolution request` 3 |
| `stall_nudged` | 0 | 7 | 0 | 0 |
| 復旧 job の `stalled`（`turn_without_receipt`・`permission_denied`） | 0 | —（対話の `stalled` は `idle_without_receipt` 1・`send_unconfirmed` 1） | 0（`send_unconfirmed` 2） | 0 |
| 生きている run の他の alert（`long_background`・`idle_process`・`stuck_exit`・`prompt_waiting`） | 0 | 19（10・5・2・2） | 0 | 0 |
| `provider_switched` | 0 | 0 | 97（全て `provider_disabled`、phase `start`） | 0 |
| `provider_waiting` | 0 | 0 | 4 | 0 |
| 待ち（`run_waiting_started`） | 0 | 12 run・12 回（`worker_question` 9、`stalled` 3）、`waited_secs` 中央値 272、合計 61,597、最大 23,542。終わり方は `answered` 9・`session_moved` 3 | 2 run・2 回（`worker_question` 2）、9,117 秒と 7,055 秒 | 0 |
| 待ちが session の終わりで終わった（`run_waiting_ended` の `cause: session_exited`） | 0 | 0 | 2（run `8e2e0094` の ask 263 と run `3f7a1148` の ask 264。どちらも 2026-09-30T13:53Z に `cause: session_exited`・`ask_id: null` で終わった。待ちの区間の中に `session_exited` の event は無く、wrapper が event を残さずに居なくなったと見られる） | 0 |
| `wrapper_heartbeat_expired` | 0 | 0 | 1（run `1930d5ff`、2026-09-30T05:53Z。待ちの外） | 0 |

読み方:

- Claude の非対話の 17 本は、全ての turn が成功し、催促・停止・権限の拒否による停止・provider の切り替え・待ちが無かった。基準は「0 件」で、1 週間後は率ではなく件数と中身を見る
- 待ちの最中に session（wrapper）を失った run は、基準の窓では Codex の 2 本だけ。goal 86 の受け入れ条件 (5)（待ちの最中に wrapper を失った非対話の run を開き直す）がこれを直す。直る前に Claude の非対話の run で同じことが起きても、それは既定の切り替えの悪化ではなく既知の穴として数える
- `worker_question` の answer の turn（`turn_requested` の `answer of ask N`）と `needs_session` の resume の turn（`resolution request`）は、基準の窓では Claude の非対話の run に 1 回も無い（[非対話の worker の測定](headless-worker-measurement.md) の条件 2a・2b）。印の後の 3 時間では、task 1272（run `33daa379`）の `resolution request` の turn が `succeeded` で終わって着地した（2b が本番を通った）。2a はまだ無い
- `stats` の `waiting` は長い窓で空になることがある（task 1370 が直す）ので、待ちの数と時間は `run_waiting_started` / `run_waiting_ended` の event から数えた

## 3. 評価のコマンド（2026-10-09 以降）

### kpi の前後比較

印 61916 で `--compare` すると、印の前後に supervisor の引き継ぎ（自動更新）が短い間隔で続くため、`kpi` は印を「重なった変更」にまとめ（`split.separable: false`）、後の窓をまとまりの最後の引き継ぎの後から始める（2026-10-02T11:00Z に読んだときは後の窓が 10:57Z から始まった）。自動更新は着地ごとに起きるので、1 週間後もまとまりが伸びて後の窓がずれうる。そこで窓を時刻で明示する。

```sh
B=2026-09-25T08:05:36Z..2026-10-02T08:05:36Z
A=2026-10-02T08:05:36Z..2026-10-09T08:05:36Z

# runtime の層と、provider × 経路の掛け合わせ（cross:area=runtime|provider=claude|route=headless など）
dagq kpi --compare "$B,$A" --area runtime --by provider --by route --cross > kpi-runtime.json
# 全体の provider × 経路
dagq kpi --compare "$B,$A" --by provider --by route --cross > kpi-route.json
# change ごと（change_summary に出る）
dagq kpi --compare "$B,$A" --change feature --change fix --change test --change refactor > kpi-change.json
# 印での比較（参考。separable と overlapping を確かめる）
dagq kpi --compare 61916 --area runtime > kpi-mark.json

jq -c '.compare | {b: .before | {start, end, runs, partial}, a: .after | {start, end, runs, partial}}' kpi-runtime.json
for k in phase.startup phase.work phase.wait_to_land lead_time session_active.worker first_pass_rate resumes_per_run land_phase.revise land_phase.verify; do
  jq -c --arg k "$k" '.compare.strata[$k] | to_entries[] | select(.key | test("^(all|area=runtime|cross:.*provider=claude.*)$"))
    | {k: $k, l: .key, b: (.value.before | {n, median, value}), a: (.value.after | {n, median, value}), verdict: .value.verdict}' kpi-runtime.json
done
jq -c '.compare.strata.asks_per_landing.all, .compare.strata["ask.worker_question_wait"].all' kpi-route.json
jq -c '.compare.change_summary' kpi-change.json
jq -c '.compare.confounders[] | select(.kind == "mark_recorded") | {at, label, position}' kpi-runtime.json   # 窓の中と間の人の印（下の「交絡」）。build と supervisor の印は数百あるので外す
```

`kpi` の `route=headless` は Codex の run も含むので、Claude の非対話は `cross:provider=claude|route=headless` の層で読む。`asks_per_landing` は経路の層を持たないので、経路ごとの ask は下の events から数える。`first_pass_rate` は task 単位の値。

### 窓の event を集める

`route.jq` と止まり方の集計は、同じ窓の event のファイルを使う。`--until` で窓の終わりまでに絞るので、2026-10-09T08:05:36Z より後に何度読んでも同じ値になる（stats は窓の後に着地した run も持つが、`route.jq` が窓の終わりまでに着地した run だけを数える）。

```sh
F=2026-10-02T08:05:36Z; T=2026-10-09T08:05:36Z     # 基準の窓は F=2026-09-25T08:05:36Z; T=2026-10-02T08:05:36Z
dagq stats --full > stats.json
for k in turn_finished turn_requested stall_nudged recovery_requested provider_switched provider_waiting \
         run_waiting_started run_waiting_ended session_exited wrapper_heartbeat_expired ask_opened; do
  dagq events --all --full --kind $k --since $F --until $T --limit 20000
done | jq -s '[.[].events[]]' > ev.json
jq 'length' ev.json      # 基準の窓は 1543。--limit に届いていないことを kind ごとに確かめる
jq -c 'group_by(.kind) | map({(.[0].kind): length}) | add' ev.json
```

### stats の runs を経路と層で分ける jq（1 の表の全ての値）

次の jq を `route.jq` として保存する。経路（`route`）× 層（`stratum`）ごとに、1 の表の全ての列（startup・work・待ちを除いた work・validate・wait_to_land・claim→着地・人の答え待ちを除いた claim→着地と、待ちのあった run の数・1 回で通った率・resume / `needs_session` / revise / 検証の失敗の率・worker の token）、cost の表の値（`claude_cost_*`）、ask の表の値（`runs`・`runs_with_ask`・`asks`・`asks_by_kind`）を出す。人の答え待ちは、`ev.json` の `run_waiting_ended` から待ちの区間を作り、work の区間に重なる秒だけを work と claim→着地から引き、claim→着地からはさらに `land_phases.ask` を引く（定義は 1 の表の前の箇条書き）。

```jq
# 使い方（評価の窓 [from, to) と、その窓の event を 1 つのファイルにまとめて渡す）:
#   jq -n --slurpfile s stats.json --slurpfile e ev.json --arg from <窓の始まり> --arg to <窓の終わり> -f route.jq
def med: map(select(. != null)) | sort | if length == 0 then null elif length % 2 == 1 then .[length/2|floor] else (.[length/2-1] + .[length/2]) / 2 end;
def ts: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
def rate($n): if $n == 0 then null else (. / $n * 100 | round / 100) end;
def area: (.areas // []) as $a
  | if ($a | length) == 0 then "unknown"
    elif ($a | any(. == "runtime" or . == "migrations" or . == "broker")) then "runtime"
    elif ($a | all(. == "docs")) then "docs"
    elif ($a | any(. == "tests")) then "tests"
    else "other" end;
# 待ちの区間（run_waiting_ended の時刻から waited_secs だけ遡る）
($e[0] | map(select(.kind == "run_waiting_ended" and .run_id != null))
   | group_by(.run_id) | map({key: .[0].run_id,
       value: map((.created_at | ts) as $end | {start: ($end - (.payload.waited_secs // 0)), end: $end})})
   | from_entries) as $waits
| ($e[0] | map(select(.kind == "ask_opened" and .run_id != null))
   | group_by(.run_id) | map({key: .[0].run_id, value: map(.payload.kind)}) | from_entries) as $asks
| [$s[0].runs[]
   | select(.actual_provider == "claude" and .claimed_at >= $from and .claimed_at < $to)
   # 着地した run は窓の終わりまでに着地したものだけ（読む日によって値が変わらないように）
   | select(.status != "integrated" or .landed_at < $to)
   | (.route // "interactive") as $r | area as $a | (.prediction.size // "?") as $z
   # 人の答え待ちのうち work の区間（receipt の時刻 − work 〜 receipt の時刻）に重なる秒。
   # receipt の後の待ち（revise・resume・/exit の間）は wait_to_land の中にあり、land_phases.ask が数えるので引かない
   | (if .validated_at != null and .validate != null and .work != null
      then ((.validated_at | ts) - .validate) as $rec | ($rec - .work) as $ws
        | [($waits[.run_id] // [])[] | ([.end, $rec] | min) - ([.start, $ws] | max) | select(. > 0)] | add // 0
      else 0 end) as $w
   | . + {r: $r, w: $w, ask_kinds: ($asks[.run_id] // []),
          keys: ["全て", "area=\($a)", "area=\($a) size=\($z)", "size=\($z)"],
          c2l: (if .status == "integrated" then (.landed_at | ts) - (.claimed_at | ts) else null end)}
   | . + {c2l_without_human: (if .c2l then .c2l - (.land_phases.ask // 0) - .w else null end),
          work_without_wait: (if .work then .work - .w else null end),
          first_pass: (.resumes == 0 and .needs_session == 0 and (.review_reasons | length) == 0
                       and (.land_phases.revise // 0) == 0 and (.integrate_attempts // 1) <= 1),
          revised: ((.land_phases.revise // 0) > 0 or (.review_reasons | length) > 0),
          verify_failed: ((.verify_failures // []) | length > 0)}]
| [.[] | . as $run | .keys[] | {route: $run.r, stratum: ., run: $run}]
| group_by([.route, .stratum]) | map(
    (map(.run)) as $all | ($all | map(select(.status == "integrated"))) as $i | ($i | length) as $n
    | {route: .[0].route, stratum: .[0].stratum, integrated: $n, not_integrated: (($all | length) - $n),
       startup: ($i | map(.startup) | med), work: ($i | map(.work) | med),
       work_without_wait: ($i | map(.work_without_wait) | med),
       validate: ($i | map(.validate) | med), wait_to_land: ($i | map(.wait_to_land) | med),
       claim_to_land: ($i | map(.c2l) | med),
       claim_to_land_without_human: ($i | map(.c2l_without_human) | med),
       human_wait_runs: ($i | map(select((.land_phases.ask // 0) + .w > 0)) | length),
       first_pass_rate: ($i | map(select(.first_pass)) | length | rate($n)),
       resumes_per_run: ($i | map(.resumes) | add // 0 | rate($n)),
       needs_session_per_run: ($i | map(.needs_session) | add // 0 | rate($n)),
       revise_rate: ($i | map(select(.revised)) | length | rate($n)),
       verify_failure_rate: ($i | map(select(.verify_failed)) | length | rate($n)),
       worker_tokens_total: ($i | map(.tokens.by_kind.worker.total) | med),
       worker_tokens_output: ($i | map(.tokens.by_kind.worker.output) | med),
       claude_cost_runs: ($i | map(select(.turns.by_provider.claude.tokens.cost_usd != null)) | length),
       claude_cost_usd_median: ($i | map(.turns.by_provider.claude.tokens.cost_usd) | med),
       claude_cost_usd_sum: ($i | map(.turns.by_provider.claude.tokens.cost_usd // 0) | add),
       claude_cost_usd_max: ($i | map(.turns.by_provider.claude.tokens.cost_usd // 0) | max),
       runs: ($all | length),
       runs_with_ask: ($all | map(select(.ask_kinds | length > 0)) | length),
       asks: ($all | map(.ask_kinds[]) | length),
       asks_by_kind: ($all | map(.ask_kinds[]) | group_by(.) | map({key: .[0], value: length}) | from_entries)})
```

```sh
jq -n --slurpfile s stats.json --slurpfile e ev.json --arg from $F --arg to $T -f route.jq > route.json
# 1 の表の形で出す（層・経路の後は 1 の表の列の順。人の答え待ちのあった run の数は claim→着地（人の答え待ちを除く）の次の列）
jq -r '.[] | [.stratum, .route, .integrated, .not_integrated, .startup, .work, .work_without_wait, .validate, .wait_to_land,
              .claim_to_land, .claim_to_land_without_human, .human_wait_runs, .first_pass_rate, .resumes_per_run,
              .needs_session_per_run, .revise_rate, .verify_failure_rate, .worker_tokens_total, .worker_tokens_output] | @tsv' route.json
# cost の表と ask の表
jq -r '.[] | select(.claude_cost_runs > 0) | [.stratum, .route, .claude_cost_runs, .claude_cost_usd_median, .claude_cost_usd_sum, .claude_cost_usd_max] | @tsv' route.json
jq -c '.[] | select(.stratum == "全て" or .stratum == "area=runtime") | {route, stratum, runs, runs_with_ask, asks, asks_by_kind}' route.json
```

層の名前の対応: `全て`、`area=runtime`（runtime を含む、全て）、`area=runtime size=S|M|L|?`（runtime を含む、S / M / L / 大きさ不明）、`area=tests`（tests（runtime なし））、`area=docs`・`area=docs size=S`（docs だけ）、`area=other`（その他）、`size=S|M|L`（大きさ S / M / L の全て）。`area=unknown` は着地しなかった run（area を持たない）。経路は `headless`（非対話）と `interactive`（対話。route の記録の前の run を含む）。

### 止まり方を events から数える

2 の表は、上の `stats.json` と `ev.json` から次で数える。

```sh
jq -n --slurpfile s stats.json --slurpfile e ev.json --arg from $F --arg to $T '
  ($s[0].runs | map(select(.claimed_at >= $from and .claimed_at < $to)
     | {key: .run_id, value: "\(.actual_provider)/\(.route // "interactive")/\(if .provider != .actual_provider then "switched" else "same" end)"}) | from_entries) as $g
  | $e[0] | map(select(.run_id != null and $g[.run_id] != null) | . + {g: $g[.run_id]})
  | group_by([.g, .kind]) | map({g: .[0].g, kind: .[0].kind, n: length, runs: (map(.run_id) | unique | length),
      detail: (if .[0].kind == "turn_finished" then {outcome: (map(.payload | "\(.outcome)/\(.failure)") | group_by(.) | map({(.[0]): length}) | add),
                                                     permission_denied_turns: (map(select(.payload.permission_denials > 0)) | length)}
               elif .[0].kind == "recovery_requested" then (map(.payload | "\(.alert)/\(.reason // "")" | .[0:40]) | group_by(.) | map({(.[0]): length}) | add)
               elif .[0].kind == "run_waiting_started" then (map(.payload.ask_kind) | group_by(.) | map({(.[0]): length}) | add)
               elif .[0].kind == "run_waiting_ended" then {waited_secs: (map(.payload.waited_secs) | sort), causes: (map(.payload.cause) | group_by(.) | map({(.[0]): length}) | add)}
               elif .[0].kind == "turn_requested" then (map(.payload.what | split(" ")[0]) | group_by(.) | map({(.[0]): length}) | add)
               elif .[0].kind == "ask_opened" then (map(.payload.kind) | group_by(.) | map({(.[0]): length}) | add)
               else null end)})' | jq -c '.[] | select(.kind != "session_exited")'
# 経路ごとの run の本数（2 の表の見出しの数）
jq -c --arg from $F --arg to $T '[.runs[] | select(.claimed_at >= $from and .claimed_at < $to)]
  | group_by([.actual_provider, (.route // "interactive"), .provider != .actual_provider]) | map({g: .[0] | "\(.actual_provider)/\(.route // "interactive")", switched: (.[0].provider != .[0].actual_provider), n: length})' stats.json
```

- `g` は `<actual_provider>/<route>/<same|switched>`。`codex/headless/switched` が Claude から Codex にフォールバックした run、`codex/headless/same` が Codex の task の run。印の後は経路を指定しない Claude の task が `claude/headless/same` に入る
- 生きている run の alert の率（4 の目安）は `recovery_requested` の `stalled`・`long_background`・`idle_process`・`stuck_exit`・`prompt_waiting` の件数 ÷ 経路ごとの run の本数
- 待ちの最中に session を失ったもの: `run_waiting_ended` の `causes` の `session_exited`。その run は `dagq events --run <run> --all --full` で待ちの始まりから終わりまでの event を読む
- 復旧 job の `stalled` は `recovery_requested` の `alert: stalled` と `reason`（`turn_without_receipt`・`permission_denied`）。人に届いたかは `ask_opened` の `stalled` と `decide`
- `worker_question` の answer の turn と resume の turn は、`turn_requested` の `answer`・`resolution` と、その次の `turn_finished` の `outcome`
- Claude の利用上限は run に紐づかない `queue_hold`（`cost`）なので、`jq '[.[] | select(.kind == "ask_opened" and .payload.kind == "queue_hold")] | length' ev.json` で数える

## 4. 既定を対話に戻す基準の案

戻すかどうかは人が決める。以下は 1 週間後に planner と人が判断するための案で、値はこの文書の基準値から置いた。

| 見る値 | 層 | 基準値（前の 7 日） | 戻すことを検討する目安（後の 7 日） | 理由 |
|---|---|---|---|---|
| 非対話に固有の停止（復旧 job の `stalled` の `turn_without_receipt`・`permission_denied`、促しを使い切った turn） | Claude 非対話の全て | 0 / 17 run | 着地した run の 5% 以上、かつ 3 件以上 | 対話の生きている run の alert（`stalled`・`long_background`・`idle_process`・`stuck_exit`・`prompt_waiting`）は 564 run に 21 件（約 3.7%）。非対話は画面・`/exit`・ダイアログの止まり方を持たないので、対話より多く止まるなら経路の問題と見る。本数が少ないうちの 1〜2 件で動かないよう件数の下限を置く |
| 人に届く非対話の `stalled` の ask | Claude 非対話の全て | 0 / 17 run | 着地 run の 2% 以上 | 対話の `stalled` の ask は 560 run に 4 件（0.7%）。その約 3 倍を超えたら、人の手が増えるので切り替えの得を打ち消す。`decide` と `worker_question` は経路に依らず起きる（対話でも 560 run に 12 件・11 件）ので目安にせず、件数と中身を記録する |
| 着地の前の手戻り（`needs_session` / run と revise の率） | runtime を含む、M | 対話 0.31・0.19、非対話 0.00・0.25（8 本） | 非対話の `needs_session` / run が 0.45 を超える、または revise の率が 0.30 を超える | 対話の M の値に約 1.4〜1.5 倍の幅を置いた。`needs_session` の多くは integrate の検証の失敗と rebase の衝突で経路に依らないので、超えたら理由（`resume_attempts` の `reason`）を見て、経路に依るもの（receipt の催促、turn の失敗）だけを数える |
| claim→着地の中央値（人の答え待ちを除く） | runtime を含む、M | 対話 1999、非対話 1769 | 非対話が 2500 秒（対話の基準の約 1.25 倍）を超え、同じ窓の対話（`--interactive` の task）より長い | claim→着地は integrate の直列の待ち（wait_to_land）が大きく、経路と関係しない揺れが大きい。3 回目の測定（task 1200）で非対話の M の work が turn の中で background の test を待って長くなった例があるので、work が原因で伸びたときだけ経路の問題と見る |
| work の中央値（待ちを除く） | runtime を含む、M | 対話 1126、非対話 802 | 非対話が 1460 秒（対話の約 1.3 倍）を超える | 基準では非対話が短い。1.3 倍は load の帯の偏りで出る差（task 1132・1200 の層の比較で 2〜3 割動いた）を超える幅 |
| `first_pass_rate`（task 単位） | `area=runtime` | 全体 0.66（`provider=claude` の対話 0.613、route の記録の前 0.732） | 0.50 を下回る | 0.15 以上の低下。非対話の基準の 0.4 は前の Codex のフォールバックで失敗した task のやり直しを含むので目安にしない |
| worker の token total の中央値 | runtime を含む、M | 対話 613.5 万、非対話 280.6 万 | 非対話が対話の基準（613.5 万）を超える | 対話の cost は記録されないので、token を代わりの目安にする。基準では非対話が半分以下なので、超えるのは turn の中の繰り返しや resume の turn の増えのしるし |
| Claude の `cost_usd`（turn ごと） | Claude 非対話、runtime を含む | 中央値 2.58、着地あたりの合計 37.31 / 10 本 | 中央値が 5.2（基準の 2 倍）を超える、または `queue_hold`（`cost`）が基準の窓（24 件）を超える | 既定の切り替えで Claude の非対話の run が増えるので合計は増える。1 本あたりの値の倍増と、利用上限による queue の控えの増えを悪化と見る |

判断の手順の案:

1. 上の目安のどれかに当たっても、すぐには戻さない。当たった run の event（`turn_finished` の出力、`recovery_requested`、`resume_attempts`）を読み、経路に固有の原因（turn の失敗・催促・権限・待ちの最中の wrapper の喪失）か、経路に依らない原因（integrate の検証、rebase の衝突、host の load、Claude の利用上限）かを分ける
2. 経路に固有の原因が 1 つの不具合に帰せるなら、戻さずに直す task にする（例: 待ちの最中の wrapper の喪失は goal 86 の受け入れ条件 (5) の task が直す）
3. 経路に固有の原因が複数あるか直し方が見えないときに、人に「既定を対話に戻す」を提案する。戻すのは ADR-t1340-1 を変える決定なので、人が決めてから ADR を書く task にする（一時の回避は、該当する task に planner が `--interactive` を付ける）

## 交絡

- **基準の窓の中の印**: 基準の窓の中に、人の印が 4 件ある（49333、2026-09-30T05:14Z「Claude 無効・臨時 Codex inbox による手動代行開始」、51928・51978 の Codex の既定の model の変更、60152、2026-10-02T03:07Z「run review を Codex に」）。後の窓に入った印は 2026-10-09 に `confounders` の `position: after` で読む。run review の Codex 化は基準の窓の最後の 5 時間だけに効き、後の窓には全体に効くので、revise の率は前後で review の provider が違う。revise の変化は `stats` の `review_reasons` の中身で読む
- **Claude の無効の期間**: 2026-09-30 に Claude が無効になり、Claude の task の 97 本が Codex で動いた。その日の Claude の run は少なく、その後の Claude の非対話の run の一部は失敗した task のやり直しになる
- **非対話の基準の標本**: 17 本は planner が測定のために選んだ task（小さめで、判断や `[e2e] paths` を含むものを意図して選んだ）とやり直しの run で、ふつうの task の分布と違う。後の窓では経路を指定しない task の全てが非対話になるので、後の窓の非対話は基準の対話の分布に近づく。前後比較は「後の窓の Claude 非対話」と「基準の窓の Claude 対話」を同じ area・大きさの層で並べるのを主にし、基準の非対話の値は止まり方の件数の照合に使う
- **自動更新**: 着地ごとに supervisor が入れ替わる（基準の窓の dagq の build は 200 種類を超える）。build ごとには分けない
- **後の窓の対話**: 後の窓の対話は `--interactive` を明示した task だけになり、本数が少ない。同じ窓の対話との比較は本数が `min_samples`（5）に満たなければ参考にとどめる

## 続け方

- 2026-10-09 以降に、3 のコマンドで後の窓の値を読み、1・2 の表と同じ形で後の窓の列を足し、4 の目安に当たったかを表にする
- 待ちの最中の wrapper の喪失（goal 86 の (5)）と `stats` の `waiting` の修正（task 1370）が着地したら、その時刻を交絡に足す
