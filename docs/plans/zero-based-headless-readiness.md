---
id: plan-zero-based-headless-readiness
type: plan
title: 非対話の worker の ask と待ちをゼロベースの形へ移す時期（境目のリファクタを始める時期と、新しい実装を広げる範囲）を判断する計測の項目・基準値・条件の案
status: active
created: 2026-10-02
owners:
  - hisamekms
tags:
  - measurement
  - worker
  - provider
related:
  - plan-headless-default-evaluation
  - plan-headless-worker-measurement
  - adr-t1340-1
  - adr-t813-1
  - adr-t813-2
  - adr-0071
---

# 非対話の worker の ask と待ちをゼロベースの形へ移す時期を判断する計測

goal 86 の task 1369 の測定。人の方針（2026-10-02、goal 85 の note、kind decision。`dagq events --goal 85 --kind observation --all --full` で読む）は、非対話の worker の ask と待ちの仕組みを全面的には作り直さず、暫定の対応を入れたうえでゼロベースの形へ移す、というもの。移し方は、claim から receipt まで（turn・ask と答え・revise・resume・促し・待ち・止まりの検出）だけを対話用と非対話用の 2 つの実装に分ける段階式で、

1. 振る舞いを変えずに境目を作るリファクタ
2. 非対話用の新しい実装（run ごとの入力の受け口、turn ごとの wrapper、slot は実行中の turn だけを数える）を切り替えの印つきで、Claude の一部 → Claude 全体 → Codex の順に広げる
3. 共通の見張りから非対話の分岐を外す

この文書は「いつ (1) を始めるか」と「(2) をどこまで広げてよいか」を判断するための計測で、次の 4 つを書く。対話の割合の基準は使わない（方針のとおり）。

1. 項目と取り方（コマンド）
2. 今の値（基準値）: 2026-09-28 以降の非対話の run を Claude と Codex に分けた値
3. 判断の基準の案と理由（決めるのは人）
4. 毎週読む手順

測って書くだけで、runtime も設定も queue の状態も変えていない。

## 読んだ範囲と方法

- 読んだ時点: 2026-10-02T11:30Z（UTC）ごろ。固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+87aa72af`）の状態を変えないコマンドだけを使った（worker の session のクライアントモードで queue service から読んだ）
- 窓: `2026-09-28T00:00:00Z` 以降に claim された run と、その後に記録された event。窓の終わりは読んだ時点（後で読み直すと値が増える。毎週の読み方は 4 で窓を閉じる）
- run の分け方: `run_claimed` の `payload.provider`（claim の時点の実際の provider）と `payload.worker_mode`、`provider_switched` の有無で 4 群に分けた。`stats` の runs は読んだ時点で終わった run だけを持つので、走っている run も数えるために `run_claimed` を使う
  - **Claude 非対話**（`claude-headless`）: 30 run（最初の claim は 2026-09-30T00:22Z。うち印 61916（2026-10-02T08:05:36Z、既定の切り替え）の後が 13 run）。着地 26、走っている 4、`failed`・`interrupted` 0
  - **Claude 対話**（`claude-interactive`）: 232 run（参考。最後の claim は 2026-10-02T08:01Z）。着地 227、`failed` 4、`interrupted` 1
  - **Codex（Claude からのフォールバック）**（`codex-fallback`）: 97 run。全て 2026-09-29T18:24Z〜2026-09-30T13:25Z の Claude の無効の期間の `provider_switched`（`provider_disabled`、phase `start`）。着地 20、`failed` 56、`interrupted` 21
  - **Codex の task**（`codex-task`）: 3 run。着地 2、`failed` 1
- 印の後の 13 run は全て Claude 非対話で、対話の claim は 0（`--interactive` を明示した task がまだ claim されていない）

## 1. 項目と取り方

全ての項目は、下の「event を集める」で作る `ev.json`（窓の event）と `tagged.json`（run の群を付けた event）から数える。`stats` の `waiting` は長い窓で空になることがあった（task 1370 が直した。直った固定バイナリでも、群の分け方をそろえるため event から数える）。

### event を集める

```sh
F=2026-09-28T00:00:00Z      # 窓の始まり。毎週の読み方では 4 の F と T を使う
T=2026-10-02T11:30:00Z      # 窓の終わり（--until）
for k in run_claimed provider_switched provider_waiting turn_started turn_finished turn_requested stall_nudged \
         recovery_requested auto_repaired run_waiting_started run_waiting_ended run_waiting_deferred run_waiting_ask_added \
         session_exited wrapper_started wrapper_heartbeat_expired workspace_created workspace_closed \
         ask_opened ask_closed ask_delivered ask_delivery_failed task_edited; do
  dagq events --all --full --kind $k --since $F --until $T --limit 20000
done | jq -s '[.[].events[]]' > ev.json
jq -c 'group_by(.kind) | map({(.[0].kind): length}) | add' ev.json   # どの kind も --limit に届いていないことを確かめる

# run の群: claude-headless / claude-interactive / codex-fallback / codex-task
cat > g.jq <<'EOF'
($e[0] | map(select(.kind == "provider_switched")) | map(.run_id) | unique) as $sw
| ($e[0] | map(select(.kind == "run_claimed")) | map({key: .run_id, value:
     (if .payload.provider == "codex" then (if (.run_id | IN($sw[])) then "codex-fallback" else "codex-task" end)
      else "claude-\(.payload.worker_mode)" end)}) | from_entries) as $g
| $e[0] | map(select(.run_id != null and $g[.run_id] != null) | . + {g: $g[.run_id]})
EOF
jq -n --slurpfile e ev.json -f g.jq > tagged.json
jq -c 'map(select(.kind == "run_claimed")) | group_by(.g) | map({g: .[0].g, runs: length})' tagged.json
```

窓の始まりより前に claim された run の event は群を持たないので数えない（窓の始まりをまたぐ run を落とすため、窓の始まりの直後の値は少し小さくなる）。

### 項目 A: 合わせ込みに由来する非対話の run の出来事

共通の見張り（画面・`/exit`・ダイアログを前提にした対話の仕組み）に非対話を合わせ込んだことで起きる出来事。経路に依らない失敗（integrate の検証、rebase の衝突、Codex の sandbox の制約、`failed` の receipt）は数えない。

| 出来事 | 取り方 |
|---|---|
| A1 待ちの最中の wrapper の死・session の終わり | `run_waiting_ended` の `payload.cause == "session_exited"` |
| A2 待ちの外の wrapper の死 | `recovery_requested` の `alert == "interrupted"` で `last_error` に `heartbeat expired` を含むもの（resume の経路の文は `resumed session's wrapper heartbeat expired`）（`wrapper_heartbeat_expired` の event は 3 つの経路のうち一部でしか残らないので、`last_error` の文で数える） |
| A3 答えの配送の失敗 | `ask_delivery_failed`。加えて、A1 で失った待ちの ask（終わった待ちの `run_waiting_started` の `ask_id` と、その待ちに `run_waiting_ask_added` で加わった ask）のうち、session を失った後に答えの turn（`turn_requested` の `what` が `answer of ask <id>`）も `ask_delivered` も無いまま閉じたもの（`closed_without_answer`）。まだ閉じていないものは `open` として次の週に読み直す |
| A4 答えの二重送り | 同じ `answer of ask <id>` の `turn_requested` が同じ run に 2 件以上 |
| A5 `turn_without_receipt` の stalled | `recovery_requested` の `alert == "stalled"` と `reason`（`turn_without_receipt`・`permission_denied`・`send_unconfirmed`）。`send_unconfirmed` は対話の送信の確認の判定が非対話に当たったもの |
| A6 非対話の run にかかった復旧 job | `recovery_requested` の全て（`alert` ごと）。このうち A2・A5 と、生きている run の alert（`idle_process`・`long_background`・`stuck_exit`・`prompt_waiting`）を合わせ込み由来とし、`failed`・`resume_exhausted` は `last_error`・`reason` を読んで経路に依るものだけを数える |

```sh
jq -c 'map(select(.kind == "run_waiting_ended")) | group_by(.g) | map({g: .[0].g, causes: (map(.payload.cause) | group_by(.) | map({(.[0]): length}) | add)})' tagged.json   # A1
jq -c 'map(select(.kind == "recovery_requested" and .payload.alert == "interrupted" and ((.payload.last_error // "") | test("heartbeat expired"))))
       | group_by(.g) | map({g: .[0].g, n: length, runs: (map(.run_id) | unique | length), at: (map(.created_at[0:13]) | group_by(.) | map({(.[0]): length}) | add)})' tagged.json   # A2
jq -c 'map(select(.kind == "ask_delivery_failed")) | group_by(.g) | map({g: .[0].g, n: length})' tagged.json   # A3（ask_delivery_failed）
# A3: 待ちの最中に session を失った ask の行方。答えの turn と close は窓の後に来ることがあるので、読む時点までを集める
for k in turn_requested ask_delivered ask_closed; do
  dagq events --all --full --kind $k --since $F --limit 20000
done | jq -s '[.[].events[]]' > late.json
jq -c --slurpfile late late.json '
  . as $ev
  | map(select(.kind == "run_waiting_ended" and .payload.cause == "session_exited")) | map(
      .run_id as $run | .g as $g | .created_at as $lost | .id as $end_id
      # 終わった待ちの始まり（同じ run の、終わりより前の最後の run_waiting_started）と、その間に加わった ask
      | ([$ev[] | select(.kind == "run_waiting_started" and .run_id == $run and .id < $end_id)] | max_by(.id)) as $start
      | ([$start.payload.ask_id] + [$ev[] | select(.kind == "run_waiting_ask_added" and .run_id == $run and .id > $start.id and .id < $end_id) | .payload.ask_id])[]
      | . as $ask
      | ([$late[0][] | select(.run_id == $run and .created_at >= $lost
           and ((.kind == "turn_requested" and .payload.what == "answer of ask \($ask)") or (.kind == "ask_delivered" and .payload.ask_id == $ask)))] | min_by(.created_at)) as $sent
      | ([$late[0][] | select(.kind == "ask_closed" and .payload.ask_id == $ask)] | min_by(.created_at)) as $closed
      | {g: $g, run: $run, ask: $ask, lost_at: $lost,
         outcome: (if $sent then "delivered_after_loss" elif $closed then "closed_without_answer" else "open" end),
         sent_at: $sent.created_at, closed_at: $closed.created_at})' tagged.json
jq -c 'map(select(.kind == "turn_requested" and (.payload.what | test("^answer of ask"))))
       | group_by([.run_id, .payload.what]) | map(select(length > 1)) | length' tagged.json   # A4（0 が正常）
jq -c 'map(select(.kind == "recovery_requested")) | group_by(.g)
       | map({g: .[0].g, alerts: (map("\(.payload.alert)/\(.payload.reason // "" | .[0:30])") | group_by(.) | map({(.[0]): length}) | add)})' tagged.json   # A5・A6
# 経路に依らない failed を分けるために last_error の頭を読む
jq -r 'map(select(.g != "claude-interactive" and .kind == "recovery_requested" and .payload.alert == "failed"))[] | "\(.g)\t\(.payload.last_error // "" | .[0:80])"' tagged.json
```

### 項目 B: 非対話のために待ち・止まり・配送・session の見張りを直した task の数

`src/application/supervise` の `session`・`waiting`・`stall`（`stall_recovery`）・`deliver`・`resume`・`revise`・`exit`（`exit_retry`）・`handoff`・`adopt` と、非対話の経路の本体の `headless.rs` を変えた着地 commit を、件名と `stats` の run の `title`・`change` で照らし合わせる。非対話のための修正かどうかは件名と差分を読んで決める（機械的な目安は件名の `非対話|headless|turn|wrapper|Codex|provider`）。

```sh
git log --after=$F --before=$T --format='%h%x09%cI%x09%s' -- \
  'src/application/supervise/session*' 'src/application/supervise/waiting*' 'src/application/supervise/stall*' \
  'src/application/supervise/deliver*' 'src/application/supervise/resume*' 'src/application/supervise/revise*' \
  'src/application/supervise/exit*' 'src/application/supervise/handoff*' 'src/application/supervise/adopt*' \
  'src/application/supervise/headless*' > commits.tsv
dagq stats --full > stats.json
jq -r -R -s --slurpfile s stats.json 'split("\n") | map(select(length > 0) | split("\t") | {sha: .[0], at: .[1], subj: .[2]})
  | map(. as $c | ($s[0].runs | map(select(.status == "integrated" and .title == $c.subj)) | .[0]) as $r
        | $c + {task: $r.task_id, change: $r.change, hint: ($c.subj | test("非対話|headless|Headless|turn|wrapper|Codex|provider"))})
  | .[] | [.sha, .task, .change, .hint, .subj[0:80]] | @tsv' commits.tsv
```

`change` は task 984 より前の task では `null`（宣言が無い）。着地 commit の件名が task の title と違うもの（古い runtime の着地）は `task` も `null` になるので、件名で読む。

### 項目 C: 経路と provider の割合、`--interactive` の task

```sh
jq -c 'map(select(.kind == "run_claimed")) | group_by(.g) | map({g: .[0].g, runs: length})' tagged.json
jq -c 'map(select(.kind == "run_claimed")) | group_by(.created_at[0:10]) | map({day: .[0].created_at[0:10], by: (group_by(.g) | map({(.[0].g): length}) | add)})' tagged.json
# 経路を明示して変えた edit（to が interactive のものが --interactive）
jq -c 'map(select(.kind == "task_edited" and (.payload.to.worker_mode? // null) != null))
       | map({at: .created_at, task: .task_id, from: .payload.from.worker_mode, to: .payload.to.worker_mode, actor: .actor.role})' ev.json
# 今 interactive を持つ task（ready・draft・submitted）。理由は各 task の context を読む
# list は 1 ページ（--limit）ごとに {tasks, next, total} を返す。next が null になるまで --before で続ける
dagq list --status draft,submitted,ready --limit 500 | jq -r '.tasks[] | select(.worker_mode == "interactive") | .id' \
  | while read id; do dagq show $id | jq -c '.task | {id, status, worker_mode, context: (.context // "" | .[0:200])}'; done
```

`add --interactive` で登録した task は `task_created` の event に経路が残らない（`payload` は `goal_id` だけ）ので、`add` の時点の明示は event から数えられない。今の task の `worker_mode` を `list` と `show` で読む（上の最後のコマンド）か、claim の `run_claimed` の `worker_mode` で数える。理由は `context` の自由な文で、分類のコードは無い（「取れない項目」）。

### 項目 D: 待ちの上限（`max_waiting`）に当たって何も動かずに slot を塞いだ時間

`run_waiting_deferred`（上限に当たって待ちに入れなかった ask ごとに 1 回。payload は `ask_id`・`ask_kind`・`waiting`・`limit`）から、run が slot を離れるか待つ理由が消えるまで。run はその間 slot に居たまま、人の答えを待つ。終わりは次のうち最も早いもの:

- 上限に空きができて、同じ run が同じ ask で待ちに入った（`run_waiting_started` の `ask_id` が同じ。ここで slot を離れる）
- 同じ ask が別の ask の待ちに加わった（`run_waiting_ask_added` の `ask_id` が同じ。その run はすでに slot の外）
- その ask が閉じた（`ask_closed`）

どれも窓の終わりまでに無いものは `open`（まだ slot を塞いでいる）として数え、塞いだ時間は窓の終わりまでで打ち切る。

```sh
jq -c --arg to $T '
  def ts: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
  ($to | ts) as $end
  | (map(select(.kind | IN("run_waiting_started", "run_waiting_ask_added", "ask_closed")))
     | map({ask: (.payload.ask_id | tostring), run: .run_id, kind, at: (.created_at | ts)})) as $ends
  | map(select(.kind == "run_waiting_deferred")) | map(
      (.created_at | ts) as $from | .run_id as $run | (.payload.ask_id | tostring) as $ask
      | ([$ends[] | select(.ask == $ask and .at >= $from and (.kind == "ask_closed" or .run == $run))] | min_by(.at)) as $e
      | {g, run: $run, ask: $ask, until: ($e.kind // "open"), blocked_secs: ((if $e then $e.at else $end end) - $from)})
  | group_by(.g) | map({g: .[0].g, n: length, blocked_secs_sum: (map(.blocked_secs) | add),
                       by_end: (map(.until) | group_by(.) | map({(.[0]): length}) | add)})' tagged.json
```

### 項目 E: 待つ間に抱える資源

| 資源 | 取り方 |
|---|---|
| E1 待ちの最中の wrapper・workspace の数と時間 | `run_waiting_started`→`run_waiting_ended` の区間（`waited_secs`）の数・合計・同時の最大。非対話の run は待つ間も wrapper と workspace を 1 つずつ持つ |
| E2 turn を実行していない間の wrapper の時間 | run ごとに `wrapper_started`→`session_exited` の時間の合計から `turn_started`→`turn_finished` の時間の合計を引いたもの（ゼロベースの形では 0 になる） |
| E3 workspace の数と寿命 | `workspace_created`→`workspace_closed` の組の寿命と、記録の上で閉じていない workspace の数 |
| E4 待ちの run の worktree の target の大きさ | 取れない（「取れない項目」） |

```sh
# E1: 終わった待ちの数と時間
jq -c 'map(select(.kind == "run_waiting_ended")) | group_by(.g)
       | map({g: .[0].g, waits: length, waited_secs: (map(.payload.waited_secs) | sort), sum: (map(.payload.waited_secs) | add)})' tagged.json
# E1: 待ちの同時の最大（群ごとと全体）。run_waiting_started で +1、同じ run の run_waiting_ended で -1。
# 窓の始まりより前に始まった待ちの終わりは数えない（負にしない）。窓の終わりで終わっていない待ちは open に出す
jq -c '
  def peak: sort_by(.at, .d) | reduce .[] as $x ({n: 0, max: 0, at: null};
      .n = ([.n + $x.d, 0] | max) | if .n > .max then .max = .n | .at = $x.at else . end) | {max, at};
  map(select(.kind == "run_waiting_started" or .kind == "run_waiting_ended")
      | {g, run: .run_id, at: .created_at, d: (if .kind == "run_waiting_started" then 1 else -1 end)})
  | {all: peak,
     by_group: (group_by(.g) | map({(.[0].g): peak}) | add),
     open: (group_by(.run) | map(select((map(.d) | add) > 0) | .[0].run))}' tagged.json
# E2（走っている run は除く）
cat > idle.jq <<'EOF'
def ts: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
def med: sort | if length == 0 then null elif length % 2 == 1 then .[length/2|floor] else (.[length/2-1] + .[length/2]) / 2 end;
def span($a; $b): [foreach .[] as $x ({open: null, sum: 0};
    if $x.kind == $a then .open = ($x.created_at | ts)
    elif $x.kind == $b and .open != null then .sum += (($x.created_at | ts) - .open) | .open = null else . end; .)] | last // {open: null, sum: 0};
map(select(.g != "claude-interactive" and (.kind | IN("wrapper_started", "session_exited", "turn_started", "turn_finished"))))
| group_by(.run_id) | map(sort_by(.id) | span("wrapper_started"; "session_exited") as $w | span("turn_started"; "turn_finished") as $t
    | {g: .[0].g, open: $w.open, wrapper: $w.sum, idle: ($w.sum - $t.sum)})
| map(select(.open == null)) | group_by(.g)
| map({g: .[0].g, runs: length, wrapper_h: ((map(.wrapper) | add) / 360 | round / 10), idle_h: ((map(.idle) | add) / 360 | round / 10),
       idle_median_s: (map(.idle) | med | round), idle_max_s: (map(.idle) | max | round),
       idle_share_pct: ((map(.idle) | add) / (map(.wrapper) | add) * 100 | round)})
EOF
jq -c -f idle.jq tagged.json
# E3
jq -c 'def ts: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
       map(select(.kind == "workspace_created" or .kind == "workspace_closed")) | group_by(.payload.workspace_id)
       | map({g: .[0].g, closed: any(.kind == "workspace_closed"),
              life: (if any(.kind == "workspace_closed") and any(.kind == "workspace_created")
                     then (map(select(.kind == "workspace_closed"))[0].created_at | ts) - (map(select(.kind == "workspace_created"))[0].created_at | ts) else null end)})
       | group_by(.g) | map({g: .[0].g, workspaces: length, unclosed: (map(select(.closed | not)) | length),
                            life_sum_h: ((map(.life // 0) | add) / 3600 | round)})' tagged.json
```

## 2. 基準値（2026-09-28T00:00Z〜2026-10-02T11:30Z）

run の本数は「読んだ範囲と方法」の 4 群。Claude 対話は参考に並べる（比べるためで、判断の基準には使わない）。

### A: 合わせ込みに由来する出来事

| 出来事 | Claude 非対話（30 run） | Codex フォールバック（97 run） | Codex の task（3 run） | 参考: Claude 対話（232 run） |
|---|---|---|---|---|
| A1 待ちの最中の session の終わり | 0（待ち 0 回） | 2（run `8e2e0094` の ask 263・run `3f7a1148` の ask 264。どちらも 2026-09-30T13:53Z、`ask_id: null`、`waited_secs` 9,117・7,055） | 0 | 0（待ち 5 回、`answered` 3・`session_moved` 2） |
| A2 待ちの外の wrapper の死（`interrupted`） | 0 | 23 run（2026-09-30T08:42Z に 5、09:12〜09:14Z に 8、09:44Z に 2、10:14〜10:15Z に 5、2026-10-01T14:28〜14:38Z に 3）。`wrapper_heartbeat_expired` の event は 1 件（run `1930d5ff`）だけ | 0 | 1 run（`interrupted`） |
| A3 答えの配送の失敗 | 0（答えの turn 0） | `ask_delivery_failed` 0。A1 の ask 264 は答えの turn が無いまま 2026-10-01T13:48Z に閉じた（答えが配られなかった）。ask 263 は session を失った後の 2026-10-01T14:05Z に答えの turn（`answer of ask 263`）で配られた | 0 | `ask_delivery_failed` 0 |
| A4 答えの二重送り | 0 | 0（答えの turn は 1 回） | 0 | —（turn を持たない） |
| A5 stalled（`turn_without_receipt`・`permission_denied`・`send_unconfirmed`） | 0 | `send_unconfirmed` 2（run `e12b273b` task 1028、run `0ce0d8a2` task 955） | 0 | `idle_without_receipt` 1、`send_unconfirmed` 1 |
| A6 復旧 job（`recovery_requested`） | 1（task 1272 の run `33daa379` の resume 中の `idle_process`。test が残した `target/debug/dagq ... service serve` を `stop_processes` で止めた） | 86 件・79 run: `interrupted` 23（A2）、`stalled` 2（A5）、`failed` 60、`resume_exhausted` 1 | 1（`failed`） | 25 件・13 run |
| うち合わせ込み由来（A1＋A2＋A5＋生きている run の alert、重なりを除いた run の数 ÷ claim した run） | 1 run（3%） | 26 run（27%。run `0ce0d8a2` は A2 と A5 の両方） | 0 | — |

読み方:

- Codex フォールバックの `failed` 60 件は `last_error` を読むと、Codex の sandbox の制約（e2e・`ps`・signal が拒まれた、など）と、実装を commit したうえでの `failed` の receipt が大半で、合わせ込みではなく provider の違いに由来する。`resume_exhausted` 1 は integrate の検証の失敗（test の失敗）で経路に依らない
- 合わせ込み由来の出来事のほとんどは、Codex フォールバックの 2026-09-30 の wrapper のまとめての死（A2）と、その後に待ちの最中の session を失ったもの（A1）。goal 86 の受け入れ条件 (5)（待ちの最中に wrapper を失った非対話の run を開き直す）がこれに当たる
- Claude 非対話の 30 run は、待ちも答えの turn も 0 で、ask は `approve_landing` 5 件だけ（`worker_question` 0）。非対話で一番手の込んだ経路（worker_question の待ち → 答えの turn）は Claude ではまだ本番を通っていない。Codex でも答えの turn は 1 回だけ。基準値は「0 件」ではなく「標本が無い」と読む

### B: 非対話のために見張りを直した task

窓の中の着地 commit で上の module（`headless.rs` を含む）を変えたものは 45 本。件名と差分から非対話のための待ち・止まり・配送の修正と判断したもの:

| commit | task | change | 中身 |
|---|---|---|---|
| `201094ea` | 863 | （宣言前） | 非対話の stalled の ask の答えを crash をまたいで一度だけ配送する |
| `3ada0497` | 890 | （宣言前） | Codex の worker の `dagq ask` を run の dir への要求にして supervisor が取り込む |
| `b502eb9c` | 1104 | feature | 非対話の run の stalled の ask に `stop` を足す |
| `9e5bd1cc` | 1179 | feature | 非対話の run の stalled の ask から `intervene` を外す |
| `6ac46789` | 1141 | fix | 印の無い `mcp.json` が残った非対話の run の答えの turn に道具が渡りうるのを直す |

5 本（約 4.5 日。1 日あたり約 1.1 本）。数えなかったものは、非対話に触れるが待ち・止まり・配送の修正ではないもの（`f6b05f85` provider のフォールバック、`d006c3f2` Codex の sandbox の sccache、`338d7a53` 役割ごとの provider、`36ad60b0` クライアントモード、`4da034bb` Codex の run dir の file の扱い（task 1184）など 6 本）と、残りの約 34 本（対話の見張り（`/exit`・ダイアログ・画面の推定・引き継ぎ）の修正と、e2e・review の工程や test の変更）。非対話のための修正は stalled の ask と答えの配送に集まっている。

### C: 経路と provider の割合

| 群 | claim | 割合 |
|---|---|---|
| Claude 対話 | 232 | 64% |
| Claude 非対話 | 30 | 8% |
| Codex フォールバック | 97 | 27% |
| Codex の task | 3 | 1% |

- 印 61916 の後（2026-10-02T08:05:36Z〜11:30Z）は 13 run が全て Claude 非対話
- `task_edited` で経路を `interactive` にした edit は 3 件（task 1181 は planner が 2026-09-30 に戻し、2026-10-02 に再び headless へ。task 1207 は inbox が 2026-10-01、task 1208 は planner が 2026-10-01）。`headless` にした edit は 20 件（測定のために planner と inbox が選んだもの）
- 印の後に `add --interactive` で登録された task の数と理由は、event から数えられない（項目 C の注）

### D: `max_waiting` に当たって slot を塞いだ時間

`run_waiting_deferred` は窓の中も queue の全期間も 0 件。塞いだ時間は 0 秒。待ちに入った run は窓の中で 7 回（Claude 対話 5・Codex フォールバック 2）、同時の最大は 2（Codex フォールバックの 2 本、2026-09-30T11:55Z〜13:53Z）で、上限 4 に届いたことは無い。

### E: 待つ間に抱える資源

| 資源 | Claude 非対話 | Codex フォールバック | Codex の task | 参考: Claude 対話 |
|---|---|---|---|---|
| E1 待ちの区間 | 0 | 2 回、9,117 秒と 7,055 秒（合計 4.5 時間、wrapper・workspace を 2 つずつ抱えた） | 0 | 5 回、合計 29,780 秒（最大 11,366） |
| E2 turn を実行していない wrapper の時間（閉じた run） | 27 run、wrapper 10.7 h のうち 0.9 h（9%）、run あたり中央値 94 秒・最大 550 秒 | 76 run、20.4 h のうち 1.2 h（6%）、中央値 4 秒・最大 2,259 秒 | 3 run、1%、最大 20 秒 | —（対話は session ＝ wrapper で turn を持たない） |
| E3 workspace: 作った数、閉じた記録の無い数、閉じたものの寿命の合計 | 34 個、4 個、11 h | 140 個、85 個（82 run）、9 h | 6 個、1 個、1 h | 284 個、7 個、133 h |
| E4 待ちの run の target の大きさ | 取れない | 取れない | 取れない | 取れない |

- Claude 非対話の閉じた記録の無い 4 個のうち 3 個は読んだ時点で走っている run のもの。1 個は着地した task 1255 の run `33ee1eb3`（rebase の衝突の resume の後に着地）のもので、閉じたのに記録が無いのか残ったのかは分からない
- Codex フォールバックの 85 個は、A2 の `interrupted` の run と `failed` の run の workspace に `workspace_closed` が無い。実際に cmux に残ったのか、閉じたが記録されなかったのかは event から分からない（「取れない項目」）
- E2 から、今の非対話の wrapper は turn の間もほとんど生きていない（答え待ちの外では、receipt の後に終わり、revise・resume の turn で起動し直す）。ゼロベースの形の「turn ごとの wrapper」との差は、主に E1 の待ちの区間に出る

## 3. 判断の基準の案

決めるのは人。以下は planner と人が毎週の見直しで使う案で、数値は 2 の基準値から置いた。

### (1) 境目のリファクタを始める条件

前提: goal 86 の暫定の対応（待ちの最中に wrapper を失った run の開き直し、答えずに閉じた `worker_question` の伝達、AskUserQuestion の拒否）が着地していること。前提が満たされる前に始めると、リファクタと暫定の対応が同じ見張りのコードで衝突する。

前提が満たされたうえで、次のどれか 1 つで始める。

| 条件 | 目安 | 理由 |
|---|---|---|
| 非対話のための見張りの修正 task（項目 B） | 前提が満たされた後の 14 日で 3 本以上（前提より前の修正は数えない） | 基準は約 4.5 日で 5 本（導入の直後の集中）。暫定の対応を入れた後も 2 週で 3 本の修正が続くなら、共通の見張りへの合わせ込みの手間が続いていて、境目を作る価値がある。基準の 5 本を数えると前提の直後に当たってしまうので、窓を前提の後から始める |
| 合わせ込み由来の出来事（項目 A の A1＋A2＋A5 と生きている run の alert） | Claude 非対話で直近 7 日に 3 run 以上、かつ claim した run の 5% 以上 | 基準は claim した run あたりで Claude 非対話 1/30（3%）、Codex フォールバック 26/97（27%）。Claude が Codex フォールバックの水準に近づく前に止めたい。本数が少ないうちの 1〜2 件で動かないよう件数の下限を置く |
| 待ちの経路の本番の失敗 | A1・A3・A4 のどれかが Claude 非対話で 1 件 | 答えの配送の失敗と二重送りは人の答えを失う・重ねるので、件数でなく 1 件で直し方の見直しに入る。基準の Claude 非対話は 0（標本 0） |
| 遅くとも | 既定の切り替えの評価（[既定の評価](headless-default-evaluation.md)、2026-10-09 以降）で既定を対話に戻さないと決めた後の最初の週次の見直し | 既定が非対話のまま続くと決まれば、対話の実装を凍結する境目を作る向きは変わらない。出来事が少なくても、新しい実装を入れる準備として始めてよい |

### (2) 新しい実装を広げる条件

切り替えの印（run ごとに記録した実装）で、新しい実装の run と今の実装の run を分けて項目 A・D・E を読む。合わせ込み由来の出来事の率は、2 の A の表と同じく、出来事のあった run の数（重なりを除く）÷ claim した run の数。

| 段 | 広げる条件の案 | 理由 |
|---|---|---|
| Claude の一部 → Claude 全体 | 新しい実装の Claude の run が 20 本以上着地し、(a) A1・A3・A4 が 0、(b) 合わせ込み由来の出来事の率が同じ期間の今の実装の Claude 非対話以下、(c) `worker_question` の答えの turn、revise の turn、`needs_session` の resume の turn、答えずに閉じた ask の伝達がそれぞれ 3 回以上本番を通って `turn_finished` が `succeeded`、(d) 待ちの最中に wrapper を持たない（E1 の待ちの区間に wrapper が 0） | 基準の窓では答えの turn が Claude で 0、Codex で 1 回しか無く、ふつうの run だけでは危ない経路が通らない。20 本は Claude 非対話の 1〜2 日分（印の後の 3.5 時間で 13 本）。経路ごとに 3 回は、1 回の偶然で通ったものを除く最小の数。(c) が集まらなければ、planner が判断の要りそうな task を選んで印を付ける |
| Claude 全体 → Codex | 新しい実装の Claude 全体で 7 日以上、(a)〜(d) が続き、項目 D の slot を塞いだ時間が 0（今の実装の基準と同じ）。加えて Codex の task か Codex のフォールバックの run を新しい実装で 5 本以上、印つきで通す | Codex は sandbox と run の dir への ask の要求（`3ada0497`）など provider に固有の経路を持ち、基準でも合わせ込み由来の出来事が Codex に集まった（26/97）。Claude 全体の 1 週は、自動更新の引き継ぎをまたいだ run（走っている run は始めた実装で終える）を含めるため |
| 止める・戻す | A3・A4 が 1 件、または新しい実装の合わせ込み由来の出来事の率が今の実装の 2 倍を超えたら広げるのを止め、印を外して今の実装に戻すかを人が決める | 答えを失う・重ねるのは人の手を増やす。率の 2 倍は、本数が少ない間の揺れを超える幅 |
| (3) 共通の見張りから非対話の分岐を外す | 全ての非対話の run が新しい実装で 14 日動き、今の実装の非対話の run が 0（走っているものも無い） | 分岐を外すと今の実装に戻せないので、戻す必要が出なかった期間を置く |

## 4. 毎週読む手順

週次の見直し（throughput-review の「週次の見直し」）で、次の並びで読む。窓は直前の 7 日（月曜 00:00Z からの 7 日など、毎週同じ区切り）。

```sh
F=$(date -u -v-7d +%Y-%m-%dT00:00:00Z); T=$(date -u +%Y-%m-%dT00:00:00Z)   # macOS の date。区切りを固定するなら手で書く
# 1. event を集め、群を付ける（1 の「event を集める」）
# 2. 群ごとの claim の数（項目 C）と、経路を interactive にした edit
# 3. 合わせ込み由来の出来事（項目 A の A1〜A6）。1 件でもあれば dagq events --run <run> --all --full で中身を読む
# 4. 非対話のための見張りの修正 task（項目 B）。直近 14 日の窓でも数える（F を 14 日前にして git log だけ流し直す）
# 5. max_waiting に当たった時間（項目 D）と、待つ間の資源（項目 E の E1〜E3）
# 6. 3 の (1) の条件と、新しい実装が入っていれば (2) の段の条件に当たったかを表にする
```

新しい実装が入った後は、群に実装（切り替えの印の記録）を足して、新しい実装と今の実装を分けて 3〜5 を読む。読んだ値は note（kind observation）で goal に残し、条件に当たったら planner が人に (1) の開始か (2) の段の進めを提案する。

## 取れない項目と要る記録

| 項目 | 取れない理由 | 要る記録 |
|---|---|---|
| E4 待ちの run の worktree の target の大きさ | run ごとの target の大きさを記録する event が無い（disk の空きの判定は直近の run の大きさを読むが、run ごとには残さない） | `run_waiting_started`・`run_waiting_ended`（と着地待ち）の時点の run の worktree の target の bytes、または host の記録（[host metrics](../design/supervisor-lifecycle/host-metrics.md)）に、待ちと着地待ちの run の target の合計 |
| E3 非対話の workspace が実際に残ったか | `interrupted`・`failed` の非対話の run の workspace に `workspace_closed` が無く、cmux に残ったのか記録されなかったのか分からない | 非対話の run の workspace を閉じた・閉じられなかった経路の全てで `workspace_closed`（理由つき）を残す。後始末で見つけて閉じたものも |
| A2 待ちの外の wrapper の死 | `wrapper_heartbeat_expired` は 3 つの経路（session・resume・exit）のうち一部でしか残らず、`recovery_requested` の `last_error` の文で数えるしかない | 3 つの経路のどれでも `wrapper_heartbeat_expired`（run・phase・待ちの最中か）を残す |
| A1 待ちの最中に失った session の ask | `run_waiting_ended` の `cause: session_exited` が `ask_id: null` で、どの ask の答えを配れなかったかが event に無い | `run_waiting_ended` に待っていた ask の ID を残す |
| C `add --interactive` の数と理由 | `task_created` に経路が無く、理由は `context` の自由な文 | `task_created` に `provider`・`worker_mode`（明示か既定か）を残し、`--interactive` に理由の分類（画面を見たい・割り込みたい・非対話の不具合の回避など）を付ける |

計装の task は receipt の `follow_ups` に出した。
