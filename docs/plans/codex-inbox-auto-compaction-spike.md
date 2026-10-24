---
id: plan-codex-inbox-auto-compaction-spike
type: plan
title: スパイク：Codex の対話の TUI で自動の compaction が起きた後に、inbox の区間の記録に要る値が hook か rollout から取れるか
status: completed
created: 2026-10-25
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - provider
  - inbox
related:
  - adr-t1181-1
  - plan-codex-inbox-spike
  - plan-codex-inbox-hook-trust-spike
---

# スパイク：Codex の対話の TUI で自動の compaction の後に区間の記録の値が取れるか

goal 77 の Spike（task 3339）。
[ADR-t1181-1](../adr/2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md) 決定5 の inbox の区間の記録について、自動の compaction の後の扱いを確かめる。
[codex-inbox-spike](codex-inbox-spike.md) は手動の `/compact` だけを測り、自動の compaction と PreCompact の `trigger` の `manual` 以外の値を未確認とした。
この task は測って書くだけで、`src/` と `docs/design/` は変えていない。

問い: codex-cli の対話の TUI で自動の compaction が起きたとき、区間の記録に要る値が取れるか。
値は、compaction の後も同じ区間と分かる session_id（thread の id）、実際の model、自動の compaction が起きたことの 3 つ。
経路は、起動の引数で渡す hook（SessionStart の `source`、PreCompact・PostCompact の `trigger`）か、rollout（`~/.codex/sessions` の scratch の thread の `rollout-*.jsonl`）のどちらか。

## 環境と対象

- 対象の commit: `336e29a1a510e0a0c810ed82b9f1823a4d124362`（この run の base）。dagq の runtime は使っていない
- 日付: 2026-10-25 06:25〜06:33（JST）。host は macOS 14.7.4（arm64、Darwin 23.6.0）
- codex-cli **0.160.0**（`~/.local/bin/codex` → `~/.codex/packages/standalone/releases/0.160.0-aarch64-apple-darwin/bin/codex`）。
  この session の PATH の先頭の `codex` は cmux の shim で 0.164.0 を返すが、[codex-inbox-hook-trust-spike](codex-inbox-hook-trust-spike.md) と同じく dagq の runtime が使う実体の `~/.local/bin/codex` で測った。
  0.164.0 は測っていない
- route: 対話の TUI を `--no-daemon` で起動した（人の Codex の daemon に繋がない）。
  model は指定せず人の設定の既定の `gpt-6.1-sol`（provider `openai`、ChatGPT の login。rollout の `model_context_window` は 475000）で、`-c model_reasoning_effort="low"` を渡した
- 使った設定: `-c model_auto_compact_token_limit=12000`（全部の回で同じ）、project の trust の `-c projects={…}`、`-c check_for_update_on_startup=false`、`-c notify=["sh","<scratch>/bin/notify.sh"]`
- scratch: `$TMPDIR/dagq-worker-t3339-tkVJfH/repo`（`git init -b main`、`seed.txt` の 1 commit）。queue も supervisor も使っていない
- pty: tmux 3.6a を専用の socket（`tmux -L dagq-t3339 -f /dev/null`）の detached の session で動かし、`send-keys` で打ち込み、`capture-pane -p` で画面を読んだ
- env: worker の session のものを継いだ。`dagq watch` も、本番の queue の状態を変える dagq のコマンドも打っていない
- 前提の task 3336 は holds（`-c hooks.state` で dagq の hook だけに trust を与えられる）なので、H を 2 回と N を 1 回流した

### 使わなかった引数と選ばなかった選択肢

- `--dangerously-bypass-hook-trust` と、sandbox・承認を外す引数は使っていない。
- N の回の trust の画面では「3. Continue without trusting (hooks won't run)」を選んだ。「Review hooks」と「Trust all and continue」は選んでいない。
- 人の `~/.codex`・`CODEX_HOME`・`~/.claude` は書き換えていない。`auth.json` は sha256 を読んだだけ。

## 打った起動の引数

`launch.sh <H|N> <limit>`（[codex-inbox-auto-compaction-spike/bin/](codex-inbox-auto-compaction-spike/bin/)）は、[codex-inbox-hook-trust-spike](codex-inbox-hook-trust-spike.md) の `launch.sh` の hook を SessionStart・PreCompact・PostCompact・SessionEnd に替えたもの。
key の event の label は `session_start`・`pre_compact`・`post_compact`・`session_end`、timeout の既定は SessionEnd が 1 で他は 600 として hash を求めた。
H は 4 つの hook の `-c hooks.state={…}` を渡し、N は渡さない。
どちらも `-c model_auto_compact_token_limit=<limit>` と `-c notify=[…]` を足す。
回ごとの引数は `args/args-<条件>-<limit>-<時刻>.txt`。

各回の操作（`round.sh`）: 起動の直前に人の設定を記録 → 起動して 8 秒後に画面を読む → trust の画面が出たら `3` と Enter → `Reply with the word ALPHA followed by a 300-word paragraph about tide pools.` → `Reply with the word BETA only.` → `/quit` → 終わった直後に人の設定を記録。
最初の turn の後の context は約 16,000 token で、12000 を超える。

`hook.sh` は stdin の JSON の先頭 900 byte を `hooks.log` に足す（会話の本文は入らない）。
`notify.sh` は引数の JSON を足し、commit した `notify.log` は `type`・`thread-id`・`turn-id`・`client` だけに絞った（`input-messages`・`last-assistant-message` の本文は省いた）。
`rollout.py` は rollout から `session_meta`・`turn_context`・`compacted`・turn の始まりと終わり・`token_count` の行の、id と model と鍵の名前だけを出す。
その出力を `rollout-summary.txt` に置いた。

## 結果表

測定の記録は [codex-inbox-auto-compaction-spike/](codex-inbox-auto-compaction-spike/) に置いた。
この dir は 1 回きりの測定の出力で、設計の文書ではない。
`bin/` は script、`args/` は起動の引数、`hooks.log`・`notify.log`・`snap.log`、`rollout-summary.txt`、`screens/`（`run<回>-start.txt` は起動から 8 秒後、`run<回>-end.txt` は compaction と BETA の行だけを抜いた終わりの画面）。

人の設定の値（全部の記録で同じだった）:

- `~/.codex/config.toml`: 更新時刻 1790953911（2026-10-03 00:11:51 JST）、sha256 `5d764192d9c005b1984f043a9c7471f147481224523c646ae789feb3e9829f0b`
- `~/.codex/auth.json`: 更新時刻 1792155657（2026-10-16 22:00:57 JST）、sha256 `4bf49f0e4e067515275330365a730ecb7f5a346daa7cb6ee16e056656a82c7c7`

| 回 | 条件 | model_auto_compact_token_limit | 自動の compaction が起きたか（画面と rollout の印） | hook の log の SessionStart の source と PreCompact・PostCompact の trigger | hook と rollout の session_id（compaction の前後で同じか）と model | notify に来た値 | 人の config.toml の sha256 と更新時刻が前後で一致したか |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 試行 | 試行（H と同じ引数） | 12000 | 起きた。画面 `• Context compacted · 4s`（この session で `capture-pane -p` の出力を読んだ。file には残していない）。rollout は BETA の turn の `task_started` の後に `compacted`、続いて同じ turn_id の `turn_context` | SessionStart `startup` → PreCompact `auto` → PostCompact `auto` → SessionStart `compact` → SessionEnd `reason: other` | hook は 5 行とも `01a16fcd-c676-…`。rollout の `session_meta.id` と `compacted` の `latest_token_usage_record` の `session_id`・`thread_id` も同じ（`rollout-summary.txt`）。model は hook・`turn_context` とも前後で `gpt-6.1-sol` | 3 行。thread の題の内部の turn（別の thread-id）と、本体の thread-id `01a16fcd-c676-…` の 2 turn。compaction の印は無い | 一致（前 06:25:17、後 06:28:38） |
| 1 | H | 12000 | 起きた。試行と同じ形（BETA の turn の始まりで compaction） | 試行と同じ順と値 | hook は全部 `01a16fd0-ea53-…`、rollout も同じ。model は前後で `gpt-6.1-sol` | 3 行（試行と同じ形） | 一致（前 06:28:42、後 06:29:32） |
| 2 | H（再現の確認） | 12000 | 起きた。画面 `• Context compacted · 5s`。BETA が ALPHA の turn の途中に入り、compaction は ALPHA の turn の中（`token_count` の後に `compacted`、同じ turn_id の `turn_context`） | 試行と同じ順と値（PreCompact・PostCompact の `turn_id` は ALPHA の turn） | hook は全部 `01a16fd3-0d06-…`、rollout も同じ。model は前後で `gpt-6.1-sol` | 2 行（題の turn と、本体の 1 turn） | 一致（前 06:31:03、後 06:31:50） |
| 3 | N | 12000 | 起きた。画面 `• Context compacted · 5s`。回 2 と同じく turn の途中の compaction | 無し（trust の画面 `Hooks need review` / `4 hooks are new or changed.` が出て 3 を選んだ。`hooks.log` に行が無い） | hook は無し。rollout の `session_meta.id` と `compacted` の `latest_token_usage_record` の `session_id`・`thread_id` は `01a16fd3-df0b-…` で同じ。model は `turn_context` で前後とも `gpt-6.1-sol` | 2 行。題の内部の turn（別の thread-id）と、本体の thread-id `01a16fd3-df0b-…`（rollout の file 名 `rollout-2026-10-25T06-31-58-01a16fd3-df0b-….jsonl` の末尾と同じ id） | 一致（前 06:31:50、後 06:32:43） |

- 起動は 4 回（上限 8 回）。測定は 06:25〜06:33 の約 8 分で、準備と書き起こしを含む作業は約 30 分（上限 45 分）。
  12000 は最初の試行で自動の compaction が起きたので、値を探る試行はこの 1 回だけ。
  試行は H と同じ引数で、結果も回 1 と同じだった。
- 回 2・3 は、`round.sh` の turn の終わりの待ちが早く抜け、BETA が ALPHA の turn の途中に送られた。
  そのため compaction は turn の途中で起き、回 1 と試行（次の turn の始まりで起きた）と別の形になった。
  どちらの形でも hook の `trigger` は `auto` で、`SessionStart` の `source: compact` は同じ session_id で来た。
- 回 2・3 を流す前に、引数の渡し方を誤った `round.sh` の呼び出しが 2 回あり、tmux の session が作られず Codex は起動しなかった（`args/` に引数の file が無い）。
  `snap.log` のその 2 回の記録には `(failed invocation: no codex started)` を添えた。
  その 2 回が `hooks.log`・`notify.log` に書いた `=== run` の見出しの行は、回 2・3 の見出しと紛れるので消した（どちらの log にも、その 2 回の hook と notify の行は無かった）。
  `notify.log` は本文を省くために書き直し、試行の見出しの行はもとから無い。
- N の回の終わりにも SessionEnd の hook は来なかった。
  hook が動かないとき、区間の記録は始まりも終わりも hook からは欠ける（dagq の側では起動の記録だけが残る）。

### rollout の自動と手動の見分け

rollout の `compacted` の行には `trigger` の欄が無い（鍵は `compaction_response_id`・`first_window_id`・`latest_token_usage_record`・`message`・`previous_window_id`・`replacement_history`・`replacement_history_metadata`・`resume_metadata`・`retained_context`・`window_id`・`window_number`）。
`event_msg` にも compaction の種類を示す行は無い。
見分けられるのは行の並びだけである。

- 自動（試行・回 1〜3）: `compacted` の後に、同じ turn の `turn_context` が続き、その turn が model の答えを出す。
- 手動の `/compact`（[codex-inbox-spike](codex-inbox-spike.md) の session `01a16a84-0010-…`。`rollout-summary.txt` の末尾）: `compacted` だけの turn（`task_started` → `compacted` → `task_complete`、`turn_context` が無い）。

この見分けは、Codex の公開の形ではなく観察した並びで、手動の標本は 1 つだけである。

## 判定

**holds**（hook の経路で 3 つの値が全部取れる。rollout の経路では session_id と model は取れ、自動の印は並びからの推定に留まる）。

根拠:

- **hook（H の回 1・2 と試行）**: PreCompact・PostCompact に `trigger: "auto"` が来た。
  手動の `/compact` の `manual`（codex-inbox-spike）と別の値なので、自動の compaction が起きたことが hook から分かる。
  compaction の後に `SessionStart`（`source: "compact"`）が来て、その `session_id` は `startup` の SessionStart・PreCompact・PostCompact・SessionEnd と同じだった。
  compaction の後も同じ区間と分かる。
  model は SessionStart・PreCompact・PostCompact の入力の `model`（`gpt-6.1-sol`）にある。
  compaction が turn の始まりで起きた回（1・試行）と turn の途中で起きた回（2）で、同じ値が同じ順に来た。
- **rollout（全部の回）**: `session_meta.id` は compaction の前後で変わらず、`compacted` の `latest_token_usage_record.session_id`・`thread_id` も同じ id だった。
  model は compaction の後の `turn_context.model` にある。
  ただし自動か手動かは、上の「rollout の自動と手動の見分け」の並びでしか分からない。
- **hook が動かないとき（N の回 3）**: hook の行は 1 つも無い。
  compaction の後の区間は hook からは記録できない。
  rollout と notify（trust が要らない）は動いた。
  notify の `thread-id` は rollout の file 名の id と同じなので、notify から rollout を見つけられる。
  notify そのものには model と compaction の印は無い。
- 人の `~/.codex/config.toml`・`auth.json` は、4 回とも起動の直前と終わった直後で更新時刻と sha256 が変わらなかった。
- 条件: codex-cli 0.160.0、`--no-daemon` の対話の TUI、`gpt-6.1-sol`（effort low）、`model_auto_compact_token_limit=12000`。
  hook の trust は task 3336 の `-c hooks.state` の形で、Codex の版が変わって hash が合わなくなれば H は N と同じになる。

## 後続の計画への推奨

判定は holds なので、下の holds の節を採る。

### holds のとき（今回）

- **task 3333**（区間と起動の記録）: 自動の compaction の後の区間は hook の経路で続ける。
  `SessionStart` の `source: compact` を、同じ `session_id` の区間の続きとして読む（新しい区間を開かない）。
  PreCompact・PostCompact の `trigger`（`auto` か `manual`）を compaction の印として区間に残す。
  model は SessionStart・PreCompact・PostCompact の入力の `model` から取る。
  unit test の fixture は `hooks.log` の回 1 の 5 行（SessionStart `startup`・PreCompact `auto`・PostCompact `auto`・SessionStart `compact`・SessionEnd）の形にする（`transcript_path` と `cwd` は fixture 用の値に替える）。
  hook が動かない区間（N の回 3）は hook の記録が始まりから無いので、起動の記録だけがある区間を欠けとして読めるようにする（[codex-inbox-hook-trust-spike](codex-inbox-hook-trust-spike.md) の 3333 の推奨と同じ）。
  hook の欠けを rollout で補う経路は要らないが、補うなら notify の `thread-id` から rollout を見つけ、`session_meta.id` と `turn_context.model` を読む形になる（自動の印は並びからの推定）。
- **task 3334**（skill と manual-smoke）: 実 cmux の Codex の inbox で、自動の compaction の後に `SessionStart` の `source: compact` で inbox として復帰することを manual-smoke の確認点に足す。
  `model_auto_compact_token_limit` を小さくして短い対話で起こせる。

### does_not_hold のとき（Codex の版が変わり、hook に `trigger` か `source: compact` が来なくなったとき）

- **task 3333**: 自動の compaction の後の欠けを記録から分かるようにし（compaction で区間を閉じたと推定する）、design に書く。
  rollout が今の形なら、`session_meta.id` と `turn_context.model` と `compacted` の並びから補う。

### unresolved のとき（新しい版で測り直して結論が出なかったとき）

does_not_hold と同じ扱いで計画し直し、どの版のどの条件が決まらなかったかを測り直しの task にする。

### planner への当てはまり

planner も同じ対話の TUI で動かすなら、同じ hook（SessionStart の `source: compact`、PreCompact・PostCompact の `trigger`）で自動の compaction の後を扱える。
planner では測っていない。

## 残ったもの

Codex 自身の記録（`~/.codex/sessions/2026/10/25/rollout-*.jsonl` の scratch の thread、`session_index.jsonl`・`history.jsonl` の行、`~/.codex/*.sqlite`）は Codex の状態で、人の設定ではない。
前の測定と同じく消していない。
scratch の dir は `$TMPDIR` の下に残した。
tmux の server は止めた。
