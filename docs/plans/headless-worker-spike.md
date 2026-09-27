---
id: plan-headless-worker-spike
type: plan
title: スパイク：Claude（claude -p）と Codex（codex exec）の非対話の worker の測定
status: completed
created: 2026-09-28
updated: 2026-09-28
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - provider
related:
  - adr-0004
  - adr-0047
  - adr-0049
---

# スパイク：Claude（claude -p）と Codex（codex exec）の非対話の worker の測定

goal 57（worker を非対話の経路で動かし、Claude Code と Codex CLI を乗せ、provider を相互にフォールバックする）の前提の測定（task 812）。planner の測定（note 26997、`dagq notes --goal 57` で読める）で残った 7 つの点を実 CLI で測り、planner の測定と合わせてここに残す。後続の task はこの結果で ADR（非対話の経路・provider の選択とフォールバック・Codex の権限）を書く。この task は測って書くだけで `src/` は変えていない。

## 環境

- 日付: 2026-09-27〜28（JST）。host は macOS 14.7.4（arm64）
- Claude Code 2.1.283（`~/.local/bin/claude`、実体）、codex-cli 0.155.1（`~/.local/bin/codex`、standalone の実体。既定の model は `gpt-6-astra`）
- 使い捨ての repository: run の dir の下の `spike/main`（`cargo init` の crate、依存は `anyhow`）と、その git worktree `spike/wt`（branch `dagq/spike`、worker の cwd に当たる）。run の dir に当たる `spike/rundir`、queue の dir に当たる `spike/queue`（`dagq --db spike/queue/queue.db init` で作った使い捨ての queue）
- 測定には `spike/probe.sh`（手順ごとに `*_OK` / `*_FAIL` を出す sh）を使い、`codex exec` に「このコマンドを 1 回だけ実行して」と頼んで sandbox の中で走らせた（`codex sandbox` は `-c sandbox_workspace_write.writable_roots` を読まず `-P :workspace` しか取らないので、権限の測定には使えなかった）
- 注意: cmux の terminal では `which codex` が cmux の shim（`$TMPDIR/cmux-cli-shims/<surface>/codex`）に解決し、shim は `--enable hooks --dangerously-bypass-hook-trust -c hooks.*=...` を足して cmux の hook を注入する（JSONL に `--dangerously-bypass-hook-trust is enabled` の `error` item が 2 つ出る）。`claude` の `--claude` と同じく、runtime は実体（`~/.local/bin/codex`）に解決して固定する必要がある。以下の測定は実体で行った

## planner の測定（note 26997 の要約）

2026-09-27、同じ版の CLI での planner の測定。goal 57 の計画の前提。

- 起動と prompt: `claude -p [flags] -- "<prompt>"`（`--add-dir` などの可変長の引数の後は `--` が要る。stdin を閉じないと 3 秒待つので `< /dev/null`）。`codex exec [flags] "<prompt>"`（prompt を省くか `-` で stdin から読む）。なお codex は `< /dev/null` でも stderr に `Reading additional input from stdin...` を 1 行出す（この測定で確認。害はない）
- session の id: claude は `--session-id <uuid>` で起動時に決められ、出力の init と result に `session_id`。codex は起動時に決められず、`--json` の最初の event の `thread.started` の `thread_id` を拾う
- resume: `claude -p --resume <id>` は同じ会話を続け、`--settings`・`--permission-mode`・`--add-dir` も付けられる。`codex exec resume <id> "<prompt>"` も続けるが `--sandbox`・`--add-dir`・`-C` を持たないので、権限は `-c` で渡し、cwd は起動する側が決める
- 出力: claude `--output-format stream-json --verbose` は `system/init`（`session_id`・`model`・`permissionMode`・`claude_code_version`・`tools`）、`assistant` / `user`、`system/task_started`・`task_notification`、`rate_limit_event`、最後に `result`（`subtype`・`is_error`・`num_turns`・`duration_ms`・`total_cost_usd`・`usage`・`permission_denials`）。codex `--json` は `thread.started`、`turn.started`、`item.started` / `item.completed`（`command_execution` の `command`・`exit_code`・`aggregated_output`、`agent_message` の `text`、`error`）、`turn.completed`（`usage`: `input_tokens`・`cached_input_tokens`・`cache_write_input_tokens`・`output_tokens`・`reasoning_output_tokens`）か `turn.failed` / `error`。`-o <file>` で最後のメッセージを書ける。cost は codex に無い
- 権限: claude は sandbox ではなく許可の仕組み。`--permission-mode auto` が `-p` でも効き、確認が要る操作は止まらずに拒否されて `result.permission_denials` に並ぶ。`--settings` の `permissions.deny`（`Bash(pkill:*)`）も効く。codex は OS の sandbox（macOS は seatbelt）
- hook: claude の `--settings` の Stop hook は `-p` でも turn の終わりに走る。非対話では process の終了が turn の終わり
- background: claude `-p` は result の後、動いている background の shell を止めて終わる
- 失敗: 存在しない model で、claude は exit 1・`result.is_error: true`・`api_error_status: 404`、codex は exit 1・`error` と `turn.failed` に API の 400
- transcript: claude は `~/.claude/projects/<cwd>/<session_id>.jsonl`、codex は `~/.codex/sessions/YYYY/MM/DD/rollout-<時刻>-<thread_id>.jsonl`

## 1. Codex の sandbox で dagq の worker の作業が通る最小の設定

### 打ったもの

```sh
# cwd は worktree（spike/wt）。probe.sh all <ver> が queue への書き込み（dagq --db … note）・git commit・
# receipt の一時ファイル＋rename・/tmp と $TMPDIR・$HOME・curl・sccache・cargo build --offline・
# cargo test --offline・未取得の crate（itoa =<ver>）の cargo fetch を順に試す
~/.local/bin/codex exec --json -c model_reasoning_effort='"low"' <権限の flag> \
  "Run exactly this one shell command, once, …: sh spike/probe.sh all <ver> > probe.log 2>&1 …" < /dev/null
```

権限の flag（G = `git rev-parse --git-common-dir` = `spike/main/.git`）:

| 名前 | flag |
| --- | --- |
| A | `--sandbox workspace-write` |
| B | A に `-c 'sandbox_workspace_write.writable_roots=["G","<run dir>","<queue dir>"]'` |
| C1 | B に `-c sandbox_workspace_write.network_access=true` |
| C2 | C1 の `writable_roots` に `~/.cargo/registry` を足す |
| REC | C2 の `G` を `G/worktrees/<name>`・`G/objects`・`G/refs/heads/dagq`・`G/logs/refs/heads/dagq` に絞る（推奨） |
| D | `--dangerously-bypass-approvals-and-sandbox` |
| E | `--approve-for-me`（workspace-write のまま、承認を自動の review に回す） |

### 結果

`RUSTC_WRAPPER=sccache`・`SCCACHE_IGNORE_SERVER_IO_ERROR=1` を渡したとき（`[run.env]` と同じ）。「cache 済み」は `~/.cargo/registry` に取得済みの `anyhow` だけで build するとき、「取得が要る」は未取得の crate を足した `cargo fetch`。

| 作業 | A | B | C1 | C2 | REC | D | E |
| --- | --- | --- | --- | --- | --- | --- | --- |
| git commit（run branch） | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓※ |
| receipt（run の dir に一時ファイル＋rename） | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓※ |
| queue の SQLite への書き込み | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓※ |
| cargo build / test（cache 済み、sccache あり） | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ | ✗※ |
| cargo fetch（registry の取得が要る） | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ | ✓※ |
| `$HOME` 直下への書き込み | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | ✓※ |
| `/tmp`・`$TMPDIR` への書き込み | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| 外への network（curl。✗ は `NET_OK 000` で応答なし） | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ | ✓※ |
| `refs/heads/main` の書き換え（`git update-ref`） | ✗ | ✓ | – | – | ✗ | – | – |

※ E: 1 回目は sandbox の中で失敗（exit 128）し、model が同じコマンドを昇格して再実行し、自動の review が承認して sandbox の外で全部が走った（`$HOME` にも書けた）。build の ✗ は 1 回目の失敗が `Cargo.toml` を戻せずに残したためで、E の権限の問題ではない。昇格と自動の承認は `--json` に独立の event として出ず、同じ command の `command_execution` が 2 つ並ぶだけ。

`RUSTC_WRAPPER` を渡さないとき（B で測定）は、network が無くても cache 済みの build（`Compiling anyhow` → `Finished`）と test（1 passed）が通り、取得が要る fetch だけが `Could not resolve host: index.crates.io` で落ちた。

途中の試行として、REC から queue の dir を抜き `queue.db` のファイルだけを入れた組み合わせも測った（commit は ✓、queue は ✗。下の「失敗の出方」の queue の項）。

失敗の出方:

- git の共通 dir が書けない: `fatal: Unable to create '…/.git/worktrees/wt/index.lock': Operation not permitted`
- run の dir・`$HOME`: `Operation not permitted`
- queue: `{"error":"unable to open database file: Error code 14: …"}`。`writable_roots` に `queue.db` のファイルだけを入れても同じで（SQLite は同じ dir に journal / `-wal` / `-shm` を作る）、queue の dir が要る
- sccache（network なし）: `sccache: error: Operation not permitted (os error 1)` で rustc の `-vV` から落ち、build も test も fetch も止まる。sandbox は 127.0.0.1:4226 の sccache server への TCP の connect を EPERM にし、`SCCACHE_IGNORE_SERVER_IO_ERROR=1` はこれを救わない（sandbox の中の `sccache --show-stats` は server に届かず 0 件の統計を出す。server を起動し直したりはしなかった）
- network を開けて sccache が server に届けば cache に当たる（C1 の build は 0.53 秒）
- fetch（network あり、`~/.cargo/registry` なし）: `failed to open ~/.cargo/registry/cache/…/itoa-1.0.4.crate: Operation not permitted`。`~/.cargo/registry` を足せば通る（`~/.cargo` 全体は要らなかった。git の依存を使うなら `~/.cargo/git` も要る見込みで、未測定）
- 取得済みの crate だけの `cargo build --offline` は `~/.cargo` に書けなくても通った

### resume でも同じ権限を渡せるか

```sh
codex exec --json resume <thread_id> "<prompt>"                         # R0: -c なし
codex exec --json resume -c 'sandbox_mode="workspace-write"' \
  -c 'sandbox_workspace_write.writable_roots=[…C2 と同じ…]' \
  -c sandbox_workspace_write.network_access=true <thread_id> "<prompt>" # R1
```

R0 は最初の exec の権限を引き継がず、既定の workspace-write（追加の root も network もなし）に戻った（queue・commit・receipt・build・fetch が全部 ✗）。R1 は C2 と同じく全部 ✓。`thread_id` は resume でも同じ値が `thread.started` に出る。つまり権限は resume のたびに `-c` で渡し直す（exec も `-c sandbox_mode=…` で揃えると 1 つの組み立てで済む）。

### 推奨

- `--sandbox workspace-write`（resume は `-c sandbox_mode="workspace-write"`）に、`writable_roots` として次を渡す。承認は `approval_policy="never"` の既定のまま（exec は確認を待たずに失敗させる）
  - run の worktree の管理 dir `G/worktrees/<worktree 名>`、`G/objects`、`G/refs/heads/dagq`、`G/logs/refs/heads/dagq`（G は `git rev-parse --git-common-dir`）。これで run branch への commit が通り、`refs/heads/main` の書き換えは sandbox が拒む。`G` 全体を渡すと main も書き換えられた。`git gc --auto` の `packed-refs` など G 直下への書き込みは通らない（commit 自体は失敗しない見込みで、未測定）
  - run の dir（receipt）
  - queue の dir（`dagq ask` の SQLite）。ただし queue の dir の下には `runs/`（他の run の worktree）もあるので、ここを書けると他の run に書ける。ADR で、(a) 受け入れる、(b) queue を別の dir に分ける、(c) worker の `dagq ask` を run の dir へのファイルの書き込みにして supervisor が取り込む、のどれにするかを決める
  - `~/.cargo/registry`（registry の取得が要る変更のため）
- `sandbox_workspace_write.network_access=true`。sccache の server に届くためと registry の取得のために要る。network を閉じたいなら、codex の run では `RUSTC_WRAPPER` を渡さず（cache は効かない）、依存の取得は起動の前に runtime が sandbox の外で `cargo fetch` する、が代わりになる
- `--dangerously-bypass-approvals-and-sandbox` と `--approve-for-me` は使わない。どちらも実際には sandbox の外で走り（E は model の昇格の要求を自動の review が通した）、`$HOME` や main にも書ける
- `/tmp` と `$TMPDIR` は workspace-write の既定で書ける（cargo と rustc が使うので閉じない。`exclude_slash_tmp` / `exclude_tmpdir_env_var` は未測定）

## 2. pkill / killall の拒否

### 打ったもの

```sh
# run ごとの rules（worktree の .codex/rules/ に置き、.git/info/exclude で git から外した）
cat > .codex/rules/deny.rules <<'EOF'
prefix_rule(pattern=["pkill"], decision="forbidden", justification="dagq: stop only processes you started, by pid")
prefix_rule(pattern=["killall"], decision="forbidden", justification="dagq: stop only processes you started, by pid")
EOF
codex execpolicy check -r .codex/rules/deny.rules pkill -f llvm-cov   # → "decision":"forbidden"
# 自分で起動した目印の process（sh -c 'sleep 900; : dagqspikesleeper'）を置いて、model に pkill / killall を打たせる
codex exec --json --dangerously-bypass-approvals-and-sandbox "Run … pkill -f dagqspikesleeper … killall dagqspikesleeper …"
codex exec --json --dangerously-bypass-approvals-and-sandbox --ignore-rules "…同じ…"
codex sandbox -P :workspace -C . sh -c 'kill -0 <目印の pid>; pgrep -f dagqspikesleeper; pkill -f dagqspikesleeper; kill <pid>'
```

### 結果

- sandbox そのものが外の process への signal を拒む: workspace-write の sandbox の中から、sandbox の外で起動した process への `kill` は `Operation not permitted`、`pgrep` / `pkill` は `Cannot get process list`（`sysmond service not found`、exit 3）で、目印の process は生き残った。sandbox の中で自分が起動した子（`sleep 60 & kill $!`）は止められた
- worktree の `.codex/rules/*.rules`（project の rules）は、trust を設定していない使い捨ての repository でも exec に読まれた。sandbox を外した（D の）状態でも `pkill -f …` と `killall …` は実行されずに拒まれ、目印は生き残った。`cd /tmp && pkill -f …` のような `&&` の複合も拒まれた
- 拒まれたコマンドは `--json` の stdout に出ない（`command_execution` の item も error の item も無い）。stderr の log に `ERROR codex_core::tools::router: error=exec_command failed: CreateProcess { message: "Rejected(\"`/bin/zsh -lc 'pkill -f dagqspikesleeper'` rejected: dagq: stop only processes you started, by pid\")" }` が出て、model はそれを受けて答えに書く
- `--ignore-rules` を付けると rules は読まれず、`pkill -f` が目印を止めた
- `sh -c 'pkill -f …'` のように別の shell の中に入れると rules は当たらず、実行された（D では目印が止まった）。rules は prefix の照合なので回り道ができる
- 実行時の rules を `-c` で渡す設定は見つからなかった。user の rules は `$CODEX_HOME/rules/`（人の `~/.codex/rules/default.rules` がある）で、`CODEX_HOME` を変えると `auth.json` も変わるので使わない
- Claude の `--settings` の `permissions.deny`（`Bash(pkill:*)`・`Bash(killall:*)`）は planner の測定で確認済み。`sh -c` で包んだときに拒めるかは測っていない

### 推奨

- Codex の worker は workspace-write の sandbox の中で動かす。それだけで他の run の session や `integrate` の検証を pkill / killall で止めることはできない（process の一覧すら取れない）。これが主の防御
- 補助として、worktree に `.codex/rules/dagq-deny.rules` を置き（runtime が書き、`info/exclude` か run の worktree だけの exclude で commit に入らないようにする）、`--ignore-rules` は付けない。model が打ったときに `justification` の文で理由が返る。ただし `sh -c` で回避できるので、rules だけには頼らない
- 拒否は `--json` に出ないので、拒否の記録が要るなら stderr の `Rejected(` の行を読む

## 3. 認証切れと利用上限 / rate limit の出力

### 打ったもの

```sh
# Claude: ログインしていない（空の CLAUDE_CONFIG_DIR）と、無効な API key
CLAUDE_CONFIG_DIR=<空の dir> claude -p --output-format stream-json --verbose --model haiku -- "say hi" < /dev/null
CLAUDE_CONFIG_DIR=<空の dir> ANTHROPIC_API_KEY=sk-ant-api03-invalid… claude -p --output-format stream-json --verbose --model haiku -- "say hi" < /dev/null
# Codex: 認証なし（空の CODEX_HOME）と、無効な API key
CODEX_HOME=<空の dir> codex exec --json --skip-git-repo-check "say hi" < /dev/null
CODEX_HOME=<空の dir> CODEX_API_KEY=sk-proj-invalid… codex exec --json --skip-git-repo-check "say hi" < /dev/null
```

人の `~/.claude`・`~/.codex/auth.json` には触れていない（空の dir は使い捨ての repository の下に作って消した）。

### 結果: Claude

| 場合 | exit | 所要 | stream の形 |
| --- | --- | --- | --- |
| ログインなし | 1 | 0.1 秒 | `system/init` の `apiKeySource: "none"`。`assistant` が `"error": "authentication_failed"`、`message.model: "<synthetic>"`、text `Not logged in · Please run /login`。`result` は `subtype: "success"`・`is_error: true`・`api_error_status: null`・`terminal_reason: "api_error"`・`result: "Not logged in · Please run /login"` |
| 無効な API key | 1 | 181 秒 | `system/init` の `apiKeySource: "ANTHROPIC_API_KEY"`。`system/api_retry`（`attempt` 1〜10、`max_retries: 10`、`retry_delay_ms` 537〜37827、`error_status: 401`、`error: "authentication_failed"`）が 10 回。その後 `assistant` の `error: "authentication_failed"`・text `Failed to authenticate. API Error: 401 API key is invalid.`、`result` は `is_error: true`・`api_error_status: 401`・`terminal_reason: "api_error"` |

- 401 は 10 回（約 3 分）再試行されてから失敗する。最初の `system/api_retry` の `error: "authentication_failed"` を見れば待たずに検知できる
- `result.subtype` は `success` のままなので、失敗は `is_error` と `terminal_reason` と `assistant.error` で読む
- 利用上限・rate limit: 再現できなかった。正常な呼び出しのたびに `rate_limit_event` が 1 つ出る: `{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1790535600,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"out_of_credits","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.12,"resetsAt":…},"seven_day":{"utilization":0.18,"resetsAt":…}}}}`。上限に達したときは `status` が `allowed` 以外になり、429 なら `system/api_retry` の `error_status: 429` と `result.api_error_status: 429` になる見込みだが、どちらも実物は見ていない
- 既存の headless の job（review・plan review・goal review・observer）は `claude -p` の text 出力（`--output-format` なし）で動き、`plan-reviews/*/review.{out,err}` などの log に利用上限や 401 の例は無かった。利用上限の検知は task 438 がまだ扱っていない。対話の worker では画面の `API Error: 401` / `Invalid API key` / `OAuth token has expired` / `OAuth token revoked` と `/login` の行で認証切れを読んでいる（`src/infrastructure/claude.rs` の `AUTH_ERRORS`）

### 結果: Codex

| 場合 | exit | 所要 | JSONL の形 |
| --- | --- | --- | --- |
| 認証なし | 1 | 17 秒 | `thread.started`・`turn.started` の後、`{"type":"error","message":"Reconnecting... 2/5 (unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: wss://api.openai.com/v1/responses, …)"}` が websocket で 4 つ、`item.completed` の `error` item（`Falling back from WebSockets to HTTPS transport. unexpected status 401 …`）、HTTPS で `Reconnecting... 1/5`〜`5/5` の `error`、最後に `error` と `turn.failed`（`error.message`: `unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, …, request id: …`） |
| 無効な API key | 1 | 21 秒 | 同じ並びで、最後の `turn.failed` の `error.message` が `unexpected status 401 Unauthorized: Incorrect API key provided: sk-proj-***…. …, auth error: 401, auth error code: invalid_api_key` |

- 失敗の種類は構造化されておらず、`message` の文字列（`401 Unauthorized`、`auth error code: …`）で読む。最初の `error`（websocket の `Reconnecting... 2/5 (unexpected status 401 …`）の `401 Unauthorized` で待たずに検知できる
- 利用上限・rate limit: 再現できなかった。`--json` の stdout には残りの量が出ない。rollout（`~/.codex/sessions/…/rollout-…-<thread_id>.jsonl`）の `token_count` の event に `"rate_limits":{"limit_id":"codex","primary":{"used_percent":10.0,"window_minutes":300,"resets_at":…},"secondary":{"used_percent":2.0,"window_minutes":10080,"resets_at":…},"credits":{"has_credits":false,…}}` がある。上限に達したときの文言は実行ファイルの文字列から `… Try again at <時刻>` の形と見られるが、実物は見ていない

### 推奨

- 認証切れ: Claude は `system/api_retry` か `assistant` の `error: "authentication_failed"`、Codex は `error` / `turn.failed` の `message` の `401 Unauthorized` で「provider が使えない」と判定し、process を待たずに止めてフォールバックの判定に回す
- 利用上限: Claude は `rate_limit_event.rate_limit_info.status` と `utilization`、Codex は rollout の `rate_limits.primary.used_percent` を読めば上限の手前で判定できる。上限に達したときの実際の出力は、次に上限に当たったときに stream を保存して確かめる（follow-up）

## 4. 長い turn

### 打ったもの

```sh
P='Run this shell command exactly once, in the foreground, and wait …: for i in 1 2 3 4; do sleep 50; echo tick $i; done; echo LONGDONE …'
claude -p --output-format stream-json --verbose --model sonnet --permission-mode auto --session-id <uuid> -- "$P" | ts
codex exec --json -c model_reasoning_effort='"low"' --sandbox workspace-write "$P" | ts
# SIGTERM: perl -e 'sleep 297' を foreground で走らせ、子が現れて 5 秒後に agent の pid に kill -TERM。その後 resume して聞く
claude -p … --session-id <uuid> -- "…perl -e 'sleep 297'…" &   kill -TERM $!
codex exec --json … "…sleep 293…" &                              kill -TERM $!
claude -p --output-format stream-json --verbose --resume <uuid> -- "Your previous turn was interrupted. …"
codex exec --json resume <thread_id> "Your previous turn was interrupted. …"
```

### 結果

出力の間隔（約 200 秒のコマンド）:

- Claude: `system/task_started`（7.4 秒）の後、`tool_progress`（`heartbeat: true`、`elapsed_time_seconds` 30・60・…・180）がちょうど 30 秒ごと。204 秒で `task_notification`（`completed`）と tool_result、206 秒で `result`（`num_turns` 2、cost 0.044 USD）。出力が 30 秒より長く途切れることはない
- Codex: `item.started`（`command_execution`、10.6 秒）の後、コマンドの途中の出力の event は無い。exec はコマンドを unified exec の session で走らせて一定時間で model に戻し、model が「まだ走っている」の `agent_message` を約 53 秒ごと（67・120・173 秒）に出しながら待った。210.6 秒で `item.completed`（`exit_code` 0、`aggregated_output` に tick 1〜4）、212.7 秒で `turn.completed`。待つたびに文脈を送り直すので、この turn の `input_tokens` は 178,012（うち cached 167,424）で、同じ内容の短い turn（約 3〜4 万）の 4〜5 倍だった
- Codex の model は待たずに turn を終えることがある: 最初の cmux の shim 経由の測定では、コマンドが走っている間に model が `DONE` と答え、`item.started` に対応する `item.completed` の無いまま `turn.completed` になり、コマンドは exec の終了とともに止められた（probe の log が途中で切れた）。prompt で待つように書くと待った
- Claude Code は foreground の長い `sleep N` を `Blocked: sleep 291 followed by: echo SLEEPDONE. … use run_in_background: true …` の tool error で止める。model はそれを受けて `run_in_background: true` で起動し直し、「完了の通知を待つ」と書いて turn を終え、`-p` は result を出した後その background の shell を止めた（`task_notification` の `status: stopped`）。worker が長い処理を background にして turn を終えると、結果を待たずに turn が終わりうる

SIGTERM（agent の process に送った）:

| | exit | 止めるまで | 走っていた子 | 出力 | resume |
| --- | --- | --- | --- | --- | --- |
| Claude | 143 | すぐ | 止まった（残らない） | `result` は出ない。`task_notification`（`stopped`）で終わる | できた。`--resume <uuid>` の turn は同じ `session_id` で、model は「`perl -e 'sleep 297'` は終わっていない（exit 137、killed）」と答えた |
| Codex | 143 | すぐ | **残った**（`/bin/zsh -lc 'sleep 293; …'` とその子の `sleep` が、codex の終了後も動き続け、pid で止めた） | `item.started` のまま `turn.completed` / `turn.failed` は出ない | できた。同じ `thread_id` で、model は「コマンドは中断されたので終わったか分からない」と答えた。stderr に `ERROR codex_core::util: Custom tool call output is missing for call id: …` が 1 行出た |

- SIGTERM の後の Claude の resume（`perl -e 'sleep 297'`）は `system/init` と `num_turns: 1` の `result` だけだった。一方、上の `sleep 291` を background にして turn を終えた session の resume では、前の turn の残りの `system/task_notification`（`stopped`）と `num_turns: 0` の `result` が先に出てから、新しい `system/init` と turn と `result` が続いた。turn の結果は最後の `result` で読む
- turn の上限: Claude は `--max-turns N`（`--help` には出ないが効く）。`--max-turns 1` で 2 つのツールを頼むと exit 1、`result` の `subtype: "error_max_turns"`・`is_error: true`・`terminal_reason: "max_turns"`・`errors: ["Reached maximum number of turns (1)"]`。ほかに `--max-budget-usd`。Codex の exec に turn の上限の flag は見つからなかった（`--help` にも設定の名前にも無い。`background_terminal_max_timeout`・`tool_timeout_sec` はあるが turn の上限ではない）ので、時間の上限は起動する側が持つ

### 推奨

- turn の生存の判定: Claude は 30 秒ごとの heartbeat があるので「一定時間 stream が無い」で止まりを見られる。Codex は 1 分近く出力が無いのが普通なので、同じ判定は使えず、process が生きていることと起動する側の時間の上限で見る
- 止めるとき: Claude は SIGTERM で子まで止まる。Codex は SIGTERM で子が残るので、runtime が codex を独自の process group で起動し、止めるときは group ごと止める（exec の終了時には子を止めるが、SIGTERM では止めない）
- 止めた後の続きは両方とも同じ id の resume でよい
- Claude の worker の prompt には「長い処理を background にして turn を終えない（終えると `-p` がその処理を止める）」を書く。Codex には「コマンドの終わりを待ってから答える」を書く

## 5. Claude の auto mode の対応 model

### 打ったもの

```sh
for m in haiku sonnet opus fable claude-sonnet-4-6; do
  claude -p --output-format stream-json --verbose --model $m --permission-mode auto --no-session-persistence -- "Reply with the single word OK. Do not use tools." < /dev/null
done   # system/init の model と permissionMode を読む
```

### 結果

| `--model` | init の `model` | init の `permissionMode` |
| --- | --- | --- |
| haiku | claude-haiku-4-5-20251001 | `default`（auto にならない。警告も stderr も無い） |
| sonnet | claude-sonnet-5 | `auto` |
| opus | claude-opus-5-5 | `auto` |
| fable | claude-fable-5-1 | `auto` |
| claude-sonnet-4-6 | claude-sonnet-4-6 | `auto` |

### 推奨

- 起動のたびに `system/init` の `permissionMode` を読み、頼んだ mode（`auto`）と違えば turn を止めて失敗として記録する（黙って `default` になると、確認の要る操作が全部拒否されて `permission_denials` に並ぶだけになる）。model の一覧で決め打ちしない

## 6. Codex の hook が exec で走るか

### 打ったもの

```sh
H() { echo "hooks.$1=[{hooks=[{type=\"command\",command='''spike/hook.sh $1''',timeout=5000}]}]"; }  # hook.sh は stdin をファイルに書く
codex exec --json --sandbox workspace-write --enable hooks --dangerously-bypass-hook-trust \
  -c "$(H SessionStart)" -c "$(H Stop)" -c "$(H PreToolUse)" -c "$(H PostToolUse)" "Run the shell command: echo hi . Then reply DONE."
codex exec --json --sandbox workspace-write --enable hooks -c "$(H Stop)" "Reply DONE."   # trust の bypass なし
```

### 結果

- `--enable hooks --dangerously-bypass-hook-trust` と `-c hooks.<Event>=…` で、exec でも SessionStart・PreToolUse・PostToolUse・Stop の hook が走った（書き方は cmux の shim が注入するものと同じ）。Stop の stdin: `session_id`（= `thread_id`）・`turn_id`・`transcript_path`（rollout）・`cwd`・`hook_event_name`・`model`・`permission_mode`（workspace-write でも `"bypassPermissions"` と出る）・`stop_hook_active`・`last_assistant_message`。SessionStart は `source: "startup"`
- `--dangerously-bypass-hook-trust` を付けないと、`-c` で渡した hook は何も言わずに走らなかった（`--json` にも stderr にも何も出ない）

### 推奨

- 非対話の経路では turn の終わりは process の終了と `turn.completed` / `turn.failed` で分かるので、idle の印（Stop hook）の代わりは要らない。Codex の hook は使わない（使うには `--dangerously-bypass-hook-trust` が要り、runtime の外の hook も信用させることになる）

## 7. 子 process の後片付け

### 打ったもの

```sh
# Claude: background の shell と nohup の & を起動させ、すぐ DONE と答えさせる
claude -p … --permission-mode auto -- "1) Bash with run_in_background: perl -e 'sleep 283' . 2) Bash: nohup perl -e 'sleep 281' >/dev/null 2>&1 & . Then reply DONE immediately …"
# Codex: 待たないコマンドと nohup の & を起動させ、すぐ DONE と答えさせる（workspace-write と、sandbox なし）
codex exec --json --sandbox workspace-write "1) Start … perl -e 'sleep 279' and do not wait … 2) nohup perl -e 'sleep 277' >/dev/null 2>&1 & . Then reply DONE …"
codex exec --json --dangerously-bypass-approvals-and-sandbox "…sleep 275 … sleep 273 …"
# 終了の 3 秒後に pgrep -f '^perl -e sleep <秒>$' で残りを見て、残っていれば pid で止めた
```

### 結果

| | background / 待たないコマンド | `nohup … &` |
| --- | --- | --- |
| Claude `-p` | 止めた（`task_notification` の `status: stopped`） | **残った**（親が 1 になり、pid で止めた） |
| Codex exec（workspace-write） | 止めた（`item.started` に対応する `item.completed` が無いまま `turn.completed`） | 止めた |
| Codex exec（sandbox なし） | 止めた | 止めた |

- Codex exec は正常に終わるとき、自分が起動したコマンドを `nohup … &` まで含めて止める（`setsid` で session を抜けたものは測っていない。macOS に `setsid` のコマンドが無い）。ただし SIGTERM で止めたときは子が残る（4.）
- Claude `-p` は自分が管理する background の shell は止めるが、shell の中で `nohup … &` と切り離したものは残す

### 推奨

- Claude の worker は、今の対話の worker と同じく「自分が起動した process を receipt の前に止める」を prompt に残す。runtime は turn の終わりに run の worktree を cwd に持つ残りの process を見て記録する（止めるのは pid で）
- Codex の worker は正常終了なら後片付けは要らないが、runtime が止めるときは process group ごと止める（4.）

## まとめ（後続の ADR への入力）

- Codex の worker の最小の権限は「workspace-write ＋ `writable_roots`（run の worktree の管理 dir・`objects`・`refs/heads/dagq`・`logs/refs/heads/dagq`・run の dir・queue の dir・`~/.cargo/registry`）＋ `network_access=true`、承認は never、bypass と approve-for-me は使わない」。これで cargo build / test（sccache あり）・registry の取得・run branch への commit・receipt・queue への書き込みが通り、main の ref と `$HOME` と他の process への signal は拒まれる。resume でも同じ `-c` を毎回渡す
- queue の dir を書けることは他の run の worktree を書けることを意味するので、ADR で扱いを決める（1. の推奨）
- pkill / killall は sandbox が主に防ぎ、worktree の `.codex/rules` が補う（`sh -c` で回避できる）
- 認証切れは両方とも stream の最初の再試行の event で分かり、Claude は約 3 分、Codex は約 20 秒で exit 1 になる。利用上限の実物は再現できなかった
- Claude は `permissionMode` を init で確かめる（haiku は auto にならない）。Codex の hook は要らない
- 止め方は provider で違う（Codex は process group ごと）。Codex の長い待ちは token を多く使う

## 後片付け

- 使い捨ての repository（`spike/main`・`spike/wt`）、queue（`spike/queue`）、run の dir の代わり（`spike/rundir`）、空の `CLAUDE_CONFIG_DIR` / `CODEX_HOME`、probe の script と出力は、run の dir の下の `spike/` ごと消した
- 自分が起動した process（目印の `sleep 900`、SIGTERM の後に残った codex の子、Claude が残した `nohup` の子）は pid で止め、`pgrep -f` で残っていないことを確かめた
- CLI 自身が書いたもの: codex の rollout（`~/.codex/sessions/2026/09/28/`）と、`--no-session-persistence` を付けなかった claude の transcript（`~/.claude/projects/` の使い捨ての repository の path のもの）は、各 CLI が自分で書く通常の記録として残した。`~/.cargo/registry` に `itoa` 1.0.4〜1.0.8 の crate が取得された。`~/.codex/config.toml`・`auth.json`・`~/.codex/rules`・`~/.claude` の設定・本番の queue・`~/.local/bin/dagq` は変えていない
