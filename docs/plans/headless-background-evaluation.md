---
id: plan-headless-background-evaluation
type: plan
title: 非対話の worker の workspace のコスト（cmux の呼び出しの失敗・時間切れ・残った workspace・startup・待ちの間の workspace）の基準値と、background の wrapper に切り替えた後の評価のコマンドと戻す基準の案
status: active
created: 2026-10-03
owners:
  - hisamekms
tags:
  - measurement
  - worker
  - cmux
related:
  - adr-t1404-1
  - adr-t813-1
  - adr-t1340-1
  - plan-zero-based-headless-readiness
  - plan-headless-default-evaluation
  - design-supervisor-lifecycle-backend-call-failures
---

# 非対話の worker の workspace のコストの基準値と、background の wrapper に切り替えた後の評価

goal 89 の task 1407 の測定。[ADR-t1404-1](../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md) は、非対話の worker の session wrapper を、`dagq.toml` の設定で選べば cmux の workspace なしで supervisor から切り離した background の process として動かすと決めた（決定 7。まずこの repository だけを切り替え、既定を変えるかは評価の後に別の ADR で決める）。この文書は、その切り替えの前後を同じ物差しで比べるための準備で、次の 4 つを書く。

1. 基準値: 切り替えの前の 7 日の非対話の worker の run の、workspace に由来するコスト
2. 切り替えた後に同じ値を読む評価のコマンドと、読むのに要る標本の数（min_samples）
3. background を workspace に戻す基準の案（turn の失敗・process の残り・adopt の失敗）。戻すかどうかは人が決める
4. 重なる変更の読み方

測って書くだけで、runtime も設定も queue の状態も変えていない。物差しは [zero-based-headless-readiness](zero-based-headless-readiness.md)（task 1369）の E1・E3 と群の分け方を使い、`workspace` を閉じた記録の数え方だけを下の「E3 の数え方」のとおり広げた。

## 読んだ範囲と方法

- 読んだ時点: 2026-10-02T15:40Z（UTC）ごろ。固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+dfdf4456`）の状態を変えないコマンドだけを使った（worker の session のクライアントモードで queue service から読んだ）
- 窓: `2026-09-25T15:30:00Z` 以上 `2026-10-02T15:30:00Z` 未満（7 日）に記録された event。run は窓の中で claim されたもの。窓の始まりより前に claim された run の event は群を持たないので数えない
- 群: `run_claimed` の `payload.provider`・`payload.worker_mode`（無い古い run は対話）と `provider_switched` の有無で 4 群に分けた（zero-based-headless-readiness と同じ）
  - **Claude 非対話**（`claude-headless`）: 44 run（最初の claim は 2026-09-30T00:22Z、うち 27 run は既定の切り替えの印 61916（2026-10-02T08:05:36Z）の後）。読んだ時点で着地 40、`failed` 1、走っている 3
  - **Codex フォールバック**（`codex-fallback`）: 97 run（2026-09-30T05:13Z〜13:25Z の Claude の無効の期間）
  - **Codex の task**（`codex-task`）: 3 run
  - **Claude 対話**（`claude-interactive`、参考と対照）: 542 run（最後の claim は 2026-10-02T08:01Z）。うち 2 run（`76fbb413`・`79260bb1`）は cmux の `create` の時間切れで provisioning に失敗して workspace を持たないので、C2・C3 は 540 run
- 非対話の worker の Claude の run は 7 日のうち後半の 2.6 日にしか無い（既定の切り替えが 2026-10-02）。7 日の窓に入る非対話の run は全て数えた

## 1. 基準値（2026-09-25T15:30Z〜2026-10-02T15:30Z）

### C1: cmux の呼び出しの失敗（`backend_call_failed`）と capture の時間切れ

`backend_call_failed` を、run を持つものはその run の群に、run を持たないものは `payload.workspace_id` を窓の `workspace_created`（resume の workspace を含む）の run に引いて群に寄せた。どの run の workspace でもないもの（inbox・planner・supervisor の workspace など）と workspace を持たない呼び出し（一覧・group）は別に数える。`exhausted` は retry を使い切った失敗（`retry_after_ms` が null）。

| 寄せた先 | 件数 | うち `exhausted` | op / code |
|---|---|---|---|
| Claude 非対話の run の workspace | **0** | 0 | — |
| Codex フォールバック・Codex の task の run の workspace | **0** | 0 | — |
| Claude 対話の run の workspace | 668 | 334 | `capture`/`backend_timeout` 663、`create` 2、`send_exit` 2、`send_text` 1 |
| run の無い workspace（inbox・planner・supervisor など） | 244 | 38 | `exists` 166、`capture` 76、`notify` 1、`send_exit` 1 |
| workspace を持たない呼び出し | 91 | 91 | `listed_workspace_ids` 87、`ensure_group` 2、`create_named` 1、`workspaces_described` 1 |
| 窓の前の run | 1 | 1 | `send_text` 1 |
| 合計 | 1,004 | 464 | capture の時間切れは 738（うち非対話の run の workspace 0。run の無い workspace の `capture` 76 件のうち 1 件は `backend_failed`） |

日ごと（全体。非対話の run の workspace への失敗はどの日も 0）:

| 日 | 件数 | capture の時間切れ | `exhausted` | load_avg の最大 |
|---|---|---|---|---|
| 09-25（15:30Z から） | 220 | 211 | 218 | 89 |
| 09-26 | 36 | 17 | 5 | 58 |
| 09-27 | 132 | 53 | 44 | 131 |
| 09-28 | 432 | 344 | 149 | 154 |
| 09-29 | 56 | 36 | 10 | 74 |
| 09-30 | 34 | 22 | 16 | 166 |
| 10-01 | 81 | 52 | 20 | 140 |
| 10-02（15:30Z まで） | 13 | 3 | 2 | 112 |

読み方:

- **非対話の run の workspace への cmux の呼び出しは、記録の上で 1 件も失敗していない**。非対話の経路は画面を読まない（capture・`send_text`・`send_exit` を run の workspace に打たない）ので、時間切れの大半（capture の 738 件）は対話の run と常駐の workspace のもの。非対話の run が cmux に打つのは主に workspace の作成と close で、これは成功しても event に残らない（下の「取れない項目」）
- したがって切り替えで**直接に**減る `backend_call_failed` は基準の上で 0 件で、切り替えの評価は「失敗が減ること」ではなく「cmux を使わない run が増えても失敗が増えないこと」と、下の C2・C3・C4 で読む
- 間接の影響の候補が 1 つある。2026-10-02T13:12:27Z〜13:16:15Z に run の無い workspace（`72D28AC7`）への `exists` が 3 回時間切れになった（load_avg 27〜44）間、Claude 非対話の 2 run（task 1372 の run `eebaca9b`、task 1334 の run `675b888d`）の wrapper の heartbeat が 207 秒止まり、13:16:15Z に `wrapper_heartbeat_expired` → turn が `stopped`/`other` → receipt が無いまま `failed` → 復旧 job の `resume` になった。cmux が詰まった間に workspace の terminal に書く wrapper が止まった可能性があるが、event からは因果を確かめられない（推定）。同じ run の 14:26:49Z の 3 件目（heartbeat_age 398 秒）は、前後に `backend_call_failed` が無い
- 2026-09-30 の Codex フォールバックの wrapper のまとめての死（`interrupted` 23 run、zero-based-headless-readiness の A2）は、その時刻の前後に `backend_call_failed` が無く、cmux の失敗とは結び付かない

### C2: workspace の数と閉じた記録の無い workspace（E3）

**E3 の数え方**: workspace ごとに `workspace_created` を始まり、`workspace_closed` か、`resume_finished` の `workspace_closed: true`（resume の workspace を閉じた記録。`domain::run` の run の workspace の一覧も同じ規則で閉じたとみなす）を終わりにした。zero-based-headless-readiness の E3 は `workspace_closed` だけを数えたので、Codex フォールバックの閉じた記録の無い数が 85 個から 77 個に減る（resume の workspace の 8 個）。窓の終わりより後の close は数えない。

| 群 | workspace（最初の session / resume） | run | 閉じた記録の無い数（読んだ時点の run の状態） | 閉じたものの寿命の合計 | 寿命の中央値 |
|---|---|---|---|---|---|
| Claude 非対話 | 57（44 / 13） | 44 | **3**（全て走っている run） | 19.4 h | 1,002 秒 |
| Codex フォールバック | 140（97 / 43） | 97 | 77（`failed` 52、`interrupted` 21、着地 4） | 10.2 h | 277 秒 |
| Codex の task | 6（3 / 3） | 3 | 0 | 1.2 h | 358 秒 |
| 参考: Claude 対話 | 659（540 / 119） | 540 | 2（着地 2） | 280.3 h | 893 秒 |

- Claude 非対話は終わった run の workspace を全て閉じた記録がある（走っている 3 個を除いて 0）。閉じた記録の無い workspace は Codex フォールバックの `failed`・`interrupted` の run に集まる（それが cmux に残ったのか、閉じたが記録されなかったのかは event から分からない）
- Claude 非対話の workspace の同時の最大は 3（parallel 3 と同じ）。非対話の run は 1 run あたり 1.3 個の workspace を作った（resume は試行ごとに別の workspace）
- Claude 非対話の turn の時間の合計は 16.9 h（`turn_started` 77。うち 3 turn は窓の終わりに走っていて、C5 の `turn_finished` は 74）で、workspace の寿命の合計（19.4 h、閉じたものだけ）との差は review の待ち・validating・resume の前後に turn を走らせずに抱えた時間

### C3: startup（claim から最初の turn の開始まで）

`run_claimed` → 最初の `turn_started`（対話は `agent_started`）を、間の `workspace_created`・`wrapper_started` で 3 つに分けた（秒）。`stats` の `startup`（`agent_started` → 最初の commit の観測）とは別の物差し。

| 群 | run | claim→workspace の作成 中央値 / p90 / 最大 | workspace→`wrapper_started` | wrapper→最初の turn | claim→最初の turn 中央値 / p90 / 最大 |
|---|---|---|---|---|---|
| Claude 非対話 | 44 | 1 / 1 / 11 | 2 / 3 / 4 | 0 / 0 / 1 | **3 / 4 / 15** |
| Codex フォールバック | 97 | 1 / 1 / 3 | 1 / 3 / 7 | 0 / 0 / 1 | 2 / 3 / 8 |
| Codex の task | 3 | 0 / 0 / 1 | 1 / 1 / 2 | 0 / 0 / 0 | 2 / 2 / 2 |
| 参考: Claude 対話（→`agent_started`） | 540 | 1 / 1 / 22 | 2 / 3 / 13 | 0 / 0 / 1 | 2 / 4 / 28 |

startup のうち workspace の作成と wrapper の起動に使う時間は中央値で 3 秒、最大でも 15 秒で、run の時間（[headless-default-evaluation](headless-default-evaluation.md) の work の中央値 558 秒）に比べて小さい。切り替えで縮む余地は数秒で、評価では「延びないこと」を見る。

### C4: 待ちの間に抱えた workspace（E1）

| 群 | 待ち（`run_waiting_started`→`run_waiting_ended`） | 抱えた時間 |
|---|---|---|
| Claude 非対話 | **0 回**（標本が無い） | 0 |
| Codex フォールバック | 2 回（2026-09-30。どちらも `cause: session_exited`） | 9,117 秒と 7,055 秒（4.5 h） |
| Codex の task | 0 | 0 |
| 参考: Claude 対話 | 12 回（`answered` 9・`session_moved` 3） | 61,597 秒（最大 23,542） |

Claude 非対話は窓の中で待ちが 0 回で、基準は「0」ではなく「標本が無い」と読む（zero-based-headless-readiness と同じ）。

### C5: 戻す基準のための基準値（turn・heartbeat・adopt・引き継ぎ）

| 項目 | Claude 非対話（44 run） | Codex フォールバック（97 run） | Codex の task（3 run） |
|---|---|---|---|
| `turn_finished` の outcome / failure | 74 turn: `succeeded` 71、`stopped`/`other` 3（上の C1 の 2 run の 13:16Z の 2 件と、run `675b888d` の 14:26Z の 1 件。どれも `wrapper_heartbeat_expired` の直後） | 124 turn: `succeeded` 119、`failed`/`usage_limit` 4、`stopped`/`other` 1 | 6 turn、`succeeded` 6 |
| `wrapper_heartbeat_expired` | 3（2 run） | 1 | 0 |
| 復旧 job（`recovery_requested`） | `failed` 2（receipt が無い。上の 2 run）、`resume_exhausted` 2（同じ run `675b888d`）、`idle_process` 1（task 1272、test が残した `dagq ... service serve` を `stop_processes` で止めた）、`stalled`/`send_unconfirmed` 1 | `failed` 60、`interrupted` 23、`stalled` 2、`resume_exhausted` 1 | `failed` 1 |
| `run_adopted`（stale な lease の引き継ぎ） | 0 | 2 | 0 |
| `session_reopen_failed` | 0 | 0 | 0 |
| `auto_repaired` の `resume_adopted`（引き継ぎの後の resume の引き継ぎ） | 3 | 13 | 0 |
| supervisor の引き継ぎ（`supervisor_handed_off`）をまたいだ turn | 74 turn のうち 28 turn が延べ 80 回の引き継ぎをまたぎ、26 が `succeeded`、2 が `stopped`（上の heartbeat の 2 件） | 124 turn のうち 12 turn・延べ 41 回、11 が `succeeded`、1 が `stopped` | 0 |

窓の中の `supervisor_handed_off` は 697 件（2026-09-30 以降 144 件）、`update_installed` は 236 件。今の workspace の wrapper は引き継ぎをまたいで turn を続けている（ADR-t813-1 決定 3 の理由 (a)）。`stopped` の 2 turn は引き継ぎではなく heartbeat の途絶えで止められた。

## 2. 評価のコマンド

### 切り替えの時刻と印

`dagq.toml` の設定（[headless-worker](../design/supervisor-lifecycle/headless-worker.md) の「workspaceなしのbackgroundのwrapper」の `[headless] wrapper = "background"`。task 1408 が足した）が着地して main checkout に反映され、supervisor が読んだ時刻に、人か inbox が固定バイナリで印を打つ（`dagq.toml` の `[run.env]` 以外の変更は runtime が印にしない）。

```sh
dagq mark 'headless wrapper background' --note 'dagq.toml [headless] wrapper = "background"; workspace wrapper before' --at <効いた時刻>
```

設定は session の wrapper を起動する時点で読むので、印の時点で走っている run は workspace のまま終わる。後の窓では run を「wrapper をどちらに置いたか」で分ける（下の `g.jq`）。

### 窓の event を集め、群を付ける

基準の窓は `F=2026-09-25T15:30:00Z; T=2026-10-02T15:30:00Z`。後の窓は印の時刻から 7 日以上（下の min_samples を満たすまで延ばす）。`--until` で窓を閉じるので、後で読み直しても同じ値になる。

```sh
F=<印の時刻>; T=<F + 7 日以上>
for k in run_claimed provider_switched backend_call_failed workspace_created workspace_closed resume_finished \
         turn_started turn_finished wrapper_started agent_started run_waiting_started run_waiting_ended \
         run_adopted session_reopen_failed wrapper_heartbeat_expired recovery_requested auto_repaired \
         wrapper_launched wrapper_stopped update_installed; do   # wrapper_stopped は task 1657 から。その計装の開始 I と、それより前の未計装の停止は下の「wrapper_stopped の計装の開始」
  dagq events --all --full --kind $k --since $F --until $T --limit 20000
done | jq -s '[.[].events[]]' > ev.json
jq -c 'group_by(.kind) | map({(.[0].kind): length}) | add' ev.json   # どの kind も --limit に届いていないことを確かめる
dagq events --all --full --kind supervisor_handed_off --since $F --until $T --limit 20000 | jq '[.events[].created_at]' > ho.json
dagq stats --full | jq -c '(.runs | map({key: .run_id, value: .status}) | from_entries)' > status.json

# 群: claude-headless / claude-headless-bg / claude-interactive / codex-fallback / codex-task（bg は wrapper を background で起動した run）
cat > g.jq <<'EOF'
($e[0] | map(select(.kind == "provider_switched")) | map(.run_id) | unique) as $sw
| ($e[0] | map(select(.kind == "wrapper_launched")) | map(.run_id) | unique) as $bg
| ($e[0] | map(select(.kind == "run_claimed")) | map({key: .run_id, value:
     ((if .payload.provider == "codex" then (if (.run_id | IN($sw[])) then "codex-fallback" else "codex-task" end)
       else "claude-\(.payload.worker_mode // "interactive")" end)
      + (if (.run_id | IN($bg[])) then "-bg" else "" end))}) | from_entries) as $g
| $e[0] | map(. + {g: (if .run_id == null then "none" else ($g[.run_id] // "pre-window") end)})
EOF
jq -n --slurpfile e ev.json -f g.jq > tagged.json
jq -c 'map(select(.kind == "run_claimed")) | group_by(.g) | map({g: .[0].g, runs: length, first: (map(.created_at) | min), last: (map(.created_at) | max)})' tagged.json
```

1 つの run が workspace の session と background の session の両方を持つことは、設定を変えた時点で走っていた run の resume で起こりうる（resume は起動する時点の設定を読む）。そういう run は `-bg` に入るので、本数が少なければ `dagq events --run <run> --all --full` で中身を読む。

### C1: cmux の呼び出しの失敗の寄せ先

```sh
jq -c '(map(select(.kind == "workspace_created" and .run_id != null)) | map({key: .payload.workspace_id, value: .g}) | from_entries) as $w
  | map(select(.kind == "backend_call_failed"))
  | map(. + {a: (if .g != "none" then .g elif .payload.workspace_id == null then "no-workspace" else ($w[.payload.workspace_id] // "other-workspace") end)})
  | group_by(.a) | map({a: .[0].a, n: length, exhausted: (map(select(.payload.retry_after_ms == null)) | length),
                        capture_timeouts: (map(select(.payload.op == "capture" and .payload.code == "backend_timeout")) | length),
                        ops: (map("\(.payload.op)/\(.payload.code)") | group_by(.) | map({(.[0]): length}) | add)})' tagged.json
# 日ごとと load の最大
jq -c 'map(select(.kind == "backend_call_failed")) | group_by(.created_at[0:10])
  | map({day: .[0].created_at[0:10], n: length, capture_to: (map(select(.payload.op == "capture" and .payload.code == "backend_timeout")) | length),
         exhausted: (map(select(.payload.retry_after_ms == null)) | length), maxload: (map(.payload.load_avg // 0) | max | round)})[]' tagged.json
dagq stats --since <cursor> | jq '.backend_failures'      # 参考: by_op・by_load_band（窓の区切りは cursor）
```

`claude-headless-bg` の行は workspace を持たないので 0 のはず（0 でなければ background の run が cmux を呼んでいる。中身を読む）。全体の件数は host の load と対話の run の数に強く依るので、件数の前後ではなく、(a) background の run の workspace の失敗が 0 か、(b) 対話の run 100 本あたりと load の帯ごとの件数が基準と同じ水準か、(c) 非対話の run の wrapper の heartbeat の途絶え（C5）が cmux の時間切れと同じ時刻に起きていないか、で読む。

### C2: workspace の数と閉じた記録の無い数

```sh
cat > e3.jq <<'EOF'
def ts: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
$st[0] as $st
| map(select(.g | test("^(claude-headless|codex-fallback|codex-task|claude-interactive)(-bg)?$")))
| map(select((.kind | IN("workspace_created", "workspace_closed")) or (.kind == "resume_finished" and .payload.workspace_id != null)))
| group_by(.payload.workspace_id) | map(
   (map(select(.kind == "workspace_created")) | sort_by(.id) | .[0]) as $c
   | (map(select(.kind == "workspace_closed" or (.kind == "resume_finished" and .payload.workspace_closed == true))) | sort_by(.id) | .[0]) as $x
   | select($c != null)
   | {g: $c.g, run: $c.run_id, resume: ($c.payload.resume_attempt != null), status: ($st[$c.run_id] // "running"),
      closed: ($x != null), life: (if $x then ($x.created_at | ts) - ($c.created_at | ts) else null end)})
| group_by(.g) | map({g: .[0].g, workspaces: length, runs: (map(.run) | unique | length), resume: (map(select(.resume)) | length),
    unclosed: (map(select(.closed | not)) | length),
    unclosed_by_status: (map(select(.closed | not)) | group_by(.status) | map({(.[0].status): length}) | add // {}),
    life_h: ((map(.life // 0) | add) / 3600 * 10 | round / 10),
    life_med_s: (map(select(.life) | .life) | sort | if length == 0 then null else .[length/2|floor] | round end)})
| .[]
EOF
jq -c --slurpfile st status.json -f e3.jq tagged.json
# 群ごとの workspace の同時の最大（作成で +1、閉じた記録で -1）
jq -c 'group_by(.g) | map(.[0].g as $g | map(select((.kind | IN("workspace_created", "workspace_closed")) or (.kind == "resume_finished" and .payload.workspace_closed == true)))
  | map({at: .created_at, d: (if .kind == "workspace_created" then 1 else -1 end)}) | sort_by(.at, .d)
  | reduce .[] as $x ({g: $g, n: 0, max: 0, at: null}; .n = ([.n + $x.d, 0] | max) | if .n > .max then .max = .n | .at = $x.at else . end)
  | {g, max, at}) | .[]' tagged.json
```

`claude-headless-bg` の run は workspace を作らないので、この表に出ないか、出ても 0 個のはず（出たら background の run が workspace を作った。中身を読む）。workspace の残りに代わる background の残りは C5 の「process の残り」で読む。

### C3: startup

```sh
cat > st.jq <<'EOF'
def ts: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
def med: sort | if length == 0 then null elif length % 2 == 1 then .[length/2|floor] else (.[length/2-1] + .[length/2]) / 2 end;
def p90: sort | if length == 0 then null else .[((length-1)*0.9)|floor] end;
def first_at($k): map(select(.kind == $k)) | sort_by(.id) | (.[0].created_at // null) | if . then ts else null end;
map(select(.g | test("^(claude-headless|codex-fallback|codex-task|claude-interactive)(-bg)?$")))
| group_by(.run_id) | map({g: .[0].g, claim: first_at("run_claimed"),
    start: first_at(if .[0].g | test("-bg$") then "wrapper_launched" else "workspace_created" end),
    wr: first_at("wrapper_started"), first: first_at(if .[0].g == "claude-interactive" then "agent_started" else "turn_started" end)}
  | select(.claim != null))
| group_by(.g) | map({g: .[0].g, runs: length,
    claim_to_start: (map(select(.start) | .start - .claim) | {n: length, med: med, p90: p90, max: max}),
    start_to_wrapper: (map(select(.start and .wr) | .wr - .start) | {n: length, med: med, p90: p90, max: max}),
    wrapper_to_first: (map(select(.wr and .first) | .first - .wr) | {n: length, med: med, p90: p90, max: max}),
    claim_to_first: (map(select(.first) | .first - .claim) | {n: length, med: med, p90: p90, max: max})})
| .[]
EOF
jq -c -f st.jq tagged.json
```

background の run の始まりは workspace の作成の代わりに wrapper の起動の記録（`wrapper_launched`）。

### C4: 待ちの間に抱えた workspace と wrapper

```sh
jq -c 'map(select(.kind == "run_waiting_ended")) | group_by(.g)
       | map({g: .[0].g, waits: length, secs: (map(.payload.waited_secs) | sort), sum: (map(.payload.waited_secs) | add),
              causes: (map(.payload.cause) | group_by(.) | map({(.[0]): length}) | add)})' tagged.json
```

background の run が待つ間に抱えるのは wrapper の process だけ（workspace は 0）。待ちの区間の数と時間は workspace の run と同じ式で数え、`-bg` の行の workspace の時間は 0 と読む。

### C5: turn・heartbeat・adopt・引き継ぎ・process の残り

```sh
jq -c 'map(select(.g | test("headless|codex"))) | group_by(.g) | map({g: .[0].g,
  turns: (map(select(.kind == "turn_finished")) | map("\(.payload.outcome)/\(.payload.failure)") | group_by(.) | map({(.[0]): length}) | add),
  launch_failures: (map(select(.kind == "turn_finished" and .payload.failure == "launch")) | length),
  hb_expired: (map(select(.kind == "wrapper_heartbeat_expired")) | length),
  adopted: (map(select(.kind == "run_adopted")) | length),
  reopen_failed: (map(select(.kind == "session_reopen_failed")) | length),
  recovery: (map(select(.kind == "recovery_requested")) | map(.payload.alert) | group_by(.) | map({(.[0]): length}) | add),
  repaired: (map(select(.kind == "auto_repaired")) | map(.payload.repair) | group_by(.) | map({(.[0]): length}) | add),
  stopped_by_signal: (map(select(.kind == "wrapper_stopped" and .created_at >= $i)) | map("\(.payload.route)/\(.payload.signal)\(if .payload.left_turn_killed then "+left_turn" else "" end)\(if .payload.children_killed > 0 then "+children" else "" end)") | group_by(.) | map({(.[0]): length}) | add),
  unmeasured_stops: (map(select(.kind == "workspace_closed" and .created_at < $i and (.payload.workspace_id // "" | startswith("background:")))) | length)})[]' --arg i "$I" tagged.json
# 引き継ぎをまたいだ turn と、その結末
jq -c --slurpfile ho ho.json 'map(select(.g | test("headless|codex")) | select(.kind | IN("turn_started", "turn_finished")))
  | group_by(.run_id) | map(sort_by(.id) | . as $r | [range(0; length) as $i
      | select($r[$i].kind == "turn_started" and ($r[$i+1].kind? // null) == "turn_finished")
      | {g: $r[$i].g, s: $r[$i].created_at, e: $r[$i+1]}]) | flatten   # 終わりの無い turn を次の turn の終わりと組にしない
  | map(. as $t | {g, outcome: .e.payload.outcome, crossed: ($ho[0] | map(select(. > $t.s and . < $t.e.created_at)) | length)})
  | group_by(.g) | map({g: .[0].g, turns: length, crossed_turns: (map(select(.crossed > 0)) | length),
                        crossed_outcomes: (map(select(.crossed > 0) | .outcome) | group_by(.) | map({(.[0]): length}) | add)})[]' tagged.json
# heartbeat の途絶えと失敗した turn は 1 件ずつ中身を読む（cmux の時間切れ・引き継ぎ・load と同じ時刻か）
jq -c 'map(select(.g | test("-bg$")) | select(.kind == "wrapper_heartbeat_expired" or (.kind == "turn_finished" and .payload.outcome != "succeeded")))
       | .[] | {at: .created_at, kind, task: .task_id, run: .run_id[0:8], p: (.payload | tostring | .[0:160])}' tagged.json
```

process の残りは、終わった run（読んだ時点で `integrated`・`failed`・`cancelled` など）の wrapper か turn の process が生きているかで数える。runtime が wrapper を止めるたびに残す `wrapper_stopped`（task 1657。[非対話のworker](../design/supervisor-lifecycle/headless-worker.md) の「停止の記録」）で、強制の停止を数える: `payload.signal` が `sigkill`（SIGTERM の後の 3 秒で終わらず SIGKILL を送った）か、`payload.left_turn_killed` が真（先に死んだ wrapper が残した turn を止めた）。`sigterm`（SIGTERM で終わった）と `gone`（止め始めに wrapper が居なかった）は強制でない。`payload.children_killed`（wrapper が起動した process のうち、wrapper が終わった直後にまだ見えたものに SIGKILL を送った数）は、wrapper が SIGTERM で turn を止めて終わった直後の、まだ終わりきらない turn も数えうるので強制の判定に入れず、参考に並べる（`sigterm` で多く出るなら wrapper の turn の止め方を疑う）。止めた経路は `payload.route`（`after_review`・`landed`・`triage`・`sweep`・`resume`・`reopen`・`unrecorded`・`planner`・`close`）で、上の `stopped_by_signal` は `route/signal` ごと（残った turn を止めたら `+left_turn`、子に SIGKILL を送ったら `+children`）に数える。`sweep` の停止は終わった run に残った wrapper を掃除が見つけて止めた記録でもある。数えるのは計装の開始 I より後の `wrapper_stopped` だけで、それより前の background の wrapper の停止（`workspace_closed` の `workspace_id` が `background:` で始まるもの）は `unmeasured_stops` として「未計装（停止結果は未知）」に別に数える。加えて人か inbox が host で確かめる（worker は host を見ない）:

```sh
# 走っている run の ID を dagq status から作り、command line に run dir（runs/<run ID>）を持つ process のうち、
# 走っていない run のものを出す（出たら残り。pid で 1 つずつ中身を確かめ、名前やパターンでは止めない）
dagq status | jq -r '.runs[].run_id' > live.txt
ps -axo pid,ppid,lstart,command | grep -E 'runs/[0-9a-f-]{36}' | grep -v grep \
  | awk 'NR == FNR { live[$1]; next } { match($0, /runs\/[0-9a-f-]{36}/); id = substr($0, RSTART + 5, 36); if (!(id in live)) print }' live.txt -
```

この確認は wrapper 以外も拾う。この文書を書いた時点（2026-10-02T15:50Z、切り替えの前）に流すと、`dagq status` の `runs` に無い 10 run の run dir を持つ process が約 20 個出たが、全て test や検証が残した process（親が 1 の `worktree/target/{debug,llvm-cov-target/debug}/dagq --db <一時の DB>`、test の `dagq broker start`・`dagq supervise --once`、test の stub の `claude-headless`）か、終わった直後の run の process（数分後に消えた）か、`dagq status` の `runs` に載らない工程（e2e など）の run の process で、session の wrapper は無かった。読むときは、数分空けて 2 回流して両方に出るものだけを、command line が session の wrapper（`dagq ... session`）か turn（`claude -p`・`codex exec`）のものに絞って数える。test が残した process は切り替えと関係しない既存の残りなので、基準として件数だけ並べる。

### wrapper_stopped の計装の開始

`wrapper_stopped` は task 1657 の着地では記録され始めない。稼働中の固定バイナリと supervisor がその commit を含む build に更新されてから記録される。そこで計装の開始 I を、task 1657 の着地 commit C を含む build が稼働した時刻とし、次の 2 つの遅い方で特定する:

1. `update_installed`（plugin だけの更新 `plugin_only: true` を除く）のうち `payload.commit`（無ければ `payload.version` の `+` の後の commit）が C を祖先に持つ最初のものの時刻（固定バイナリが C を含む build になった）
2. その version への `supervisor_handed_off`（`payload.version`）の最初の時刻（supervisor が exec でその build に入れ替わった）。lease を持つ run が無いまま入れ替わって `supervisor_handed_off` が無ければ、1 の後の最初の `wrapper_stopped` の時刻を使う（未計装の build は `wrapper_stopped` を書かない）

```sh
C=$(git log --format=%H --grep='^Dagq-Task: 1657$' main | tail -1)     # task 1657 の着地 commit
jq -r 'map(select(.kind == "update_installed" and .payload.plugin_only != true)) | sort_by(.id)[] | "\(.created_at) \(.payload.commit // ((.payload.version // "") | split("+")[1] // ""))"' ev.json \
  | while read at commit; do git merge-base --is-ancestor "$C" "$commit" 2>/dev/null && { echo "$at $commit"; break; }; done   # 1 の時刻と commit
dagq events --all --full --kind supervisor_handed_off --since $F --until $T --limit 20000 \
  | jq -r --arg c <1 の commit> '[.events[] | select(.payload.version // "" | endswith($c))] | min_by(.id) | .created_at'   # 2 の時刻
I=<1 と 2 の遅い方>
```

`git merge-base --is-ancestor` は C が祖先なら 0 で終わる（commit が手元に無ければ `git fetch` してから）。I より前の background の wrapper の停止は「未計装（停止結果は未知）」として別に数え（上の `unmeasured_stops`）、process の残りの 5% の分母と、min_samples の「止める経路ごと」の回数には I より後の `wrapper_stopped` だけを数える。窓の始まり F が I より前なら、F から I までの停止は未計装として並べるだけで、率を読むのは I からにする。

### kpi の前後比較

```sh
B=2026-09-25T15:30:00Z..<印の時刻>; A=<印の時刻>..<印の時刻 + 7 日以上>
dagq kpi --compare "$B,$A" --by provider --by route --cross > kpi-route.json
dagq kpi --compare <印の ID> --area runtime > kpi-mark.json            # separable と overlapping を確かめる
jq -c '.compare.strata["phase.startup"], .compare.strata["phase.work"], .compare.strata.first_pass_rate' kpi-route.json
jq -c '.compare.confounders[] | select(.kind == "mark_recorded") | {at, label, position}' kpi-mark.json
```

`kpi` の `phase.startup` は `stats` の `startup`（`agent_started` → 最初の commit）で C3 とは別の物差しなので、C3 は上の `st.jq` で読み、kpi は大きく悪化していないかの確認に使う。

### min_samples

後の窓は印の時刻から 7 日以上で、次の全てを満たすまで延ばす。満たさないうちは戻す基準の率を判断に使わず、1 件で戻す項目（下の 3 の「1 件で」の行）だけを見る。

| 項目 | min_samples | 理由 |
|---|---|---|
| `claude-headless-bg` の claim | 40 run | 基準の Claude 非対話は 44 run で、同じ程度の本数で率を比べる。印 61916 の後の約 7.4 時間で 27 run だったので約 11 時間分（窓は 7 日以上なので、ふつうはこれより多く集まる） |
| `-bg` の turn | 70 turn | 基準の 74 turn と同じ程度。turn の失敗の率（基準 3/74）を比べるため |
| 引き継ぎをまたいだ `-bg` の turn | 20 turn | 基準は 28 turn。切り離した wrapper が supervisor の exec をまたいで生きることが (a) の中心なので、自動更新の着地（日に数回）で自然に集まる |
| `needs_session` の resume の `-bg` の session | 5 回 | resume は別の wrapper を起動する経路。基準の Claude 非対話は resume の workspace が 13 個 |
| 止める経路ごと（review の後の終了 `route: after_review`、`stalled` の `stop`（失敗した run の triage が止める `route: triage`。run の `stall_resolved` の `answered_stop` で分ける）、cancel（着地の ask への cancel は review の後に既に止めた `after_review`、復旧の ask への cancel は `triage`、supervisor の外で終わった run は `sweep` に出るので、run の ask の答えで分ける）、後始末の掃除 `route: sweep`。復旧 job の `stop_processes` は wrapper でなく pid を止めるので `wrapper_stopped` は無く、`auto_repaired` の `processes` の `killed` で数え、その run の wrapper はその後の `after_review` か `triage` で止まる） | review の後 20 回、ほかは 1 回以上（無ければ planner が task を選んで印を付けるか、使い捨ての queue のスモークで人が確かめる）。数えるのは計装の開始 I より後の `wrapper_stopped` だけで、未計装の停止は回数に入れない | process の残りは止める経路ごとに起きうる。review の後の終了は毎 run 通る |
| 待ち（`run_waiting_started`）の `-bg` の run | 3 回（無くても評価は進め、標本が無いと書く） | 基準の Claude 非対話は 0 回で、待ちの間の資源（C4）は標本が集まってから読む |

## 3. 戻す基準の案

決めるのは人。数値は 1 の基準値から置いた。戻すとは、この repository の `dagq.toml` の設定を workspace に戻すこと（着地して main checkout に反映されてから、新しく起動する wrapper から効く）。

| 項目 | 戻す（戻すかを人が決める）基準の案 | 基準値 | 理由 |
|---|---|---|---|
| turn の失敗 | min_samples を満たした `-bg` の turn で、`succeeded` 以外（`usage_limit`・`authentication` の provider の止まりを除く）の率が 8% を超え、かつ 4 件以上。または `failure: launch`（wrapper が turn を起動できない）が 2 件以上 | Claude 非対話 3/74（4%、全て heartbeat の途絶えの後の `stopped`）、`launch` 0 | 基準の 2 倍を超える幅。本数が少ない間の 1〜2 件で動かないよう件数の下限を置く。`launch` は切り離した起動そのもの（env・cwd・process group）の不具合を示すので低い件数で見る |
| process の残り | 1 件で: 終わった run の wrapper か turn の process が生きていた（掃除が止めた `wrapper_stopped`（`route: sweep`）の `signal` が `gone` 以外か、host の確認で見つかった）。または計装の開始 I より後の `route: after_review` の `wrapper_stopped` のうち強制の停止（`signal: sigkill` か `left_turn_killed` が真）が 5% を超える（分母は I より後の `after_review` の `wrapper_stopped` の数で、未計装の停止を入れない） | workspace の run は close（hangup）で止まり、残りは記録の上で 0（`idle_process` の 1 件は test が残した `dagq service serve` で wrapper ではない） | 残った process は CPU と worktree の lock を抱え、名前で止められない（AGENTS.md の signal の規則）。workspace の close が暗に止めていたものを signal で止め損ねていないかを最初に見る |
| adopt の失敗 | 1 件で: 引き継ぎ（`supervisor_handed_off`）か stale な lease の引き継ぎ（`run_adopted`）の後に、生きている `-bg` の wrapper を見失って run が `interrupted`・`failed` になった、または同じ run に 2 つの wrapper が登録された。加えて、引き継ぎをまたいだ `-bg` の turn のうち `succeeded` 以外が 10% を超えた | 引き継ぎをまたいだ Claude 非対話の turn 28 のうち `succeeded` 26、`stopped` 2（引き継ぎでなく heartbeat の途絶え）。`run_adopted` 0、`session_reopen_failed` 0 | 切り離した wrapper は pid と起動時刻で識別し直すので、識別を誤ると turn を失うか二重に走らせる。二重の wrapper は同じ worktree に 2 つの agent を走らせるので 1 件で見る |
| heartbeat の途絶え | `-bg` の `wrapper_heartbeat_expired` が 100 run あたり 7 件を超える（基準 3/44 ≒ 6.8 件）。cmux の時間切れと同じ時刻に起きたものは数えない | 3（2 run。うち 2 件は cmux の 4 分の詰まりと同じ時刻） | background の wrapper は cmux の terminal に書かないので、cmux の詰まりと同じ時刻の途絶えは消えるはず。消えずに別の時刻で増えるなら、切り離した process の heartbeat の書き方を疑う |
| startup | `-bg` の claim→最初の turn の p90 が 30 秒を超える | 3 / 4 / 15（中央値 / p90 / 最大） | workspace の作成を省くので延びないはず。延びたら起動の待ち（`wrapper_launched` の記録と登録の待ち）を疑う |
| 人が出力を追えない | 人か inbox が `dagq run log`で run の出力を追えなかった報告が 1 件で、直す task を作る（戻すのは追えない状態が続くとき） | — | ADR-t1404-1 決定 6 の代わりの見方 |

戻す基準に当たらず、min_samples を満たし、C1 で `-bg` の run の cmux の失敗が 0、C2 で `-bg` の run の workspace が 0 なら、既定を変えるかを決める ADR（ADR-t1404-1 決定 7）の材料にする。

## 4. 重なる変更の読み方

- **同じ週の他の非対話の変更**: goal 87（非対話の runtime の planner。同じ設定で background を選ぶ。ADR-t1404-1 決定 8）、goal 86（待ちの最中に wrapper を失った run の開き直しなど）、既定の非対話化の評価（[headless-default-evaluation](headless-default-evaluation.md)、2026-10-09 以降）が重なる。planner の session は run の群に入らない（`run_id` を持たない）ので、C1〜C5 の run の数字には入らないが、C1 の「run の無い workspace」と「workspace を持たない呼び出し」の件数は planner の workspace が減ると下がる。その減りを worker の切り替えの効果に数えない
- **対照**: 対話の worker（`--interactive`）と inbox は workspace のまま変わらないので、`claude-interactive` の run の C1（100 本あたりの失敗と load の帯ごとの件数）を、host の load と cmux の調子の対照にする。対話の run が少なければ（印 61916 の後は既定が非対話）、run の無い workspace（inbox）への `capture`・`exists` の失敗を対照にする
- **自動更新と引き継ぎ**: runtime の着地ごとに supervisor が exec で入れ替わる（窓に 697 回の `supervisor_handed_off`）。切り替えの印の前後に引き継ぎが続くと、`kpi` は印を重なった変更にまとめる（`split.separable: false`）ので、`kpi` は窓を時刻で明示して読む（[headless-default-evaluation](headless-default-evaluation.md) の 3 と同じ）。引き継ぎは C5 で「またいだ turn」として数えるので、増えること自体は評価の標本になる
- **load と並列数**: cmux の時間切れは load に強く依る（基準の窓で load_avg の最大 166）。`dagq.toml` の `[supervisor] parallel` と `[run.env]` の並列度、host の他の load が変わったら、C1 は load の帯ごとに比べ、`dagq kpi --compare` の confounders の印を並べて読む
- **途中で設定を変えた run**: 印の時点で走っていた run は workspace のまま終わり、その resume は起動の時点の設定で background になりうる。群は「background の wrapper を 1 回でも起動した run」を `-bg` にするので、印の直後の数時間は両方の形を持つ run が混ざる。本数が少なければ、窓を印の 1 時間後から始めて比べてもよい（そのときは基準の窓も同じだけ後ろにずらさない。基準は切り替えの前で閉じている）

## 取れない項目と要る記録

| 項目 | 取れない理由 | 要る記録 |
|---|---|---|
| 非対話の run が cmux に打った呼び出しの数（成功を含む） | `backend_call_failed` は失敗だけを残し、成功した `create`・`close`・`exists` は数えられない。cmux の負荷のうち非対話の run の分が分からない | supervisor が op ごとの呼び出しの数（成功を含む）を一定の間隔で数えて残す（host metrics か `stats` の `backend_calls`） |
| C1 の間接の影響（cmux の詰まりで wrapper の heartbeat が止まったか） | `wrapper_heartbeat_expired` に止まった原因が無く、時刻の一致で推定するしかない | wrapper が heartbeat を書けなかった区間と、そのとき terminal への書き込みで止まっていたか（wrapper の log） |
| C2 の閉じた記録の無い workspace が cmux に残ったか | 記録の無い close と実際の残りを event から分けられない（zero-based-headless-readiness の「取れない項目」と同じ） | 後始末と掃除が閉じた・閉じられなかった全ての経路で `workspace_closed`（理由つき）を残す |
| background の process の残り | 切り替えの前は workspace の close が止めていたので記録が無い。task 1657 の計装の開始 I までの background の wrapper の停止は停止結果が無い（未計装）。I の後も、runtime が見つけなかった残りは event に出ない | runtime が止めた停止は `wrapper_stopped`（I から）。runtime が見つけなかった残りは host の確認で数える |
