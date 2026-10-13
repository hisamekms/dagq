---
id: plan-manual-smoke-results
type: plan
title: 実ClaudeとCodexを含む経路の手動スモークの結果の記録（2026-09-22〜10-05）
status: completed
created: 2026-10-13
owners:
  - hisamekms
tags:
  - testing
  - measurement
  - provider
related:
  - development-manual-smoke
  - plan-headless-worker-spike
  - plan-headless-worker-measurement
  - adr-t813-1
  - adr-t813-3
  - adr-t1063-1
  - adr-t1207-1
---

# 実ClaudeとCodexを含む経路の手動スモークの結果の記録

[手動スモークの手順](../development/manual-smoke.md)を流した（または本番の queue の記録に確認点を当てた）結果を、日付ごとに残す。
手順は開発文書が持ち、ここは当時の版・観察・未確認の点の記録で、書き換えない。
これより後の結果は、依頼した task の receipt の `summary`（または `note`）に残す（記録の中の「この節に足す」は当時の書き方で、今はここに足さない）。

## 故障経路のスモーク

### 観測済みの環境依存

- **wrapper が SIGKILL で死ぬと** `run_processes.exited_at` は wrapper 行も agent 行も null のまま残る。`doctor` は PID の生死で補うので判定は変わらない。
- **未確認のまま残っているもの**: 検証コマンドの 30 分 timeout、`main` を進めた後の DB 更新失敗（[ADR-0008](../adr/0008-merge-queue-squash-landing.md) の既知の限界）。`exit_request_timed_out` の実機は、task 1437 で worker の `/exit` とともに撤去したので確かめない。

## 独立 task 1 件の完走

2026-09-22 の初回（固定バイナリ 0.1.0、README の手順の追記 1 件）は、登録から着地まで約 5 分、DB を手で直さずに通った（当時は review と `integrate` を常駐 session が手で行った）。worktree が信頼済みの repository のものだったので trust dialog は出なかった。

## 作業の内訳のスモーク

### 結果（2026-09-28、Claude Code 2.1.283、cmux 0.64.25 (106)、dagq 0.4.0-dev+bdb0259）

worker の区間 1 つ（166 秒、`active_secs` 114）。`session_closed.work` と `session_exited.work_breakdown` は `kind` / `attempt` を除いて同じ値で、`stats --full` の `runs[].work_breakdown`・`overall.work_breakdown`（`runs` 1、`runs_with_repeats` 1）と `timeline` の `commands`（9 行、`event` は `session_closed` の id）もそれと一致した。`secs` の合計（model 57・test 52・idle 38・subagent 11・build 4・chain / git / other_command / tool 各 1）は区間の長さと一致した。

| シナリオ | transcript | 記録 | 一致 |
| --- | --- | --- | --- |
| background の `cargo build` / `cargo test` | `tool_use`（`run_in_background: true`）→ すぐ `tool_result`（`Command running in background with ID: <id>`、`toolUseResult.backgroundTaskId`）→ 完了の `queue-operation` `enqueue` | `background: true`、`end` は `enqueue` の時刻、`status: completed`・`exit_code: 0`・`failed: false`、`finished: true`。`cargo test`（17 秒）は `full_tests` と `verification_repeats` に数えた | 一致 |
| background の失敗（`cargo test --test does_not_exist`） | 通知の `<status>failed</status>`、summary `... failed with exit code 101` | `status: failed`・`exit_code: 101`・`failed: true`、`commands.test.failed` 1。target を選ぶので `verification_repeats` に数えない | 一致 |
| async の Agent | `tool_result` は `Async agent launched successfully ...`（`isAsync: true`）。報告は先に `<agent-message from="<agent id>">` の `queue-operation`（`enqueue` / `dequeue`）で届き、約 2 秒後に `<task-notification>` の `enqueue`、続けて同じ内容の `remove` が書かれた（`user` の `<task-notification>` は書かれなかった）。subagent の transcript は main の JSONL に sidechain として入らず `<session_id>/subagents/` に分かれる | `subagent`、`background: true`、`end` は `<task-notification>` の `enqueue`（11 秒）、`status: completed`、`finished: true` | 一致（終わりは報告の到着ではなく通知の `enqueue`） |
| Ctrl-B で background に移したコマンド | `tool_result` が `Command was manually backgrounded by user with ID: <id>`、`toolUseResult.backgroundedByUser: true`。その後に通常の background と同じ通知（`queue-operation` の `enqueue` / `dequeue` と `user` の `<task-notification>`） | worker の run では移せなかった（下の注）。別の対話 session で transcript の形だけ確かめ、runtime の規則（shell の `tool_use` に通知があれば通知までの background）が当てはまる形であることを確かめた | 人か inbox が `dagq-smoke` の worker に上の pty 手順で送り、開始時刻・category で対応づけた `worktime.jsonl` と `session_exited.work_breakdown.heavy` の時刻・`background`・終了状態を突き合わせる（未実施） |
| timeout で background に移ったコマンド | `tool_result` が `Command did not complete within its 600s timeout and was moved to the background (ID: <id>)`、`toolUseResult.timedOutAfterMs`。通知は `queue-operation` の `enqueue` と `remove` で、`user` の `<task-notification>` は書かれず、同じ時刻に `attachment` の record が書かれた | この smoke を見ていた session の transcript で形だけ確認。人か inbox が `dagq-smoke` の worker で timeout を超える foreground コマンドを実行し、開始時刻・category で対応づけた `worktime.jsonl` と `session_exited.work_breakdown.heavy` を通知の時刻・終了状態と突き合わせる（未実施） | 形は規則どおり |
| つないだコマンド（`cargo fmt && cargo test`） | foreground の `tool_use` / `tool_result` | `test`（`fmt` は重いコマンドでないので `chain` にならない。規則どおり）、17 秒、`full_tests` と `verification_repeats` に数えた | 一致 |
| foreground の `cargo test --release` | 同上 | `test`、17 秒。`--release` は target を絞らないので全体の test として `verification_repeats` に数えた（計 3） | 一致 |

通知の形の実例（`<output-file>` の path を伏せた。background のコマンドの完了）:

```text
{"type":"queue-operation","operation":"enqueue","timestamp":"…","sessionId":"…","content":"<task-notification>\n<task-id>bofdpskt9</task-id>\n<tool-use-id>toolu_…KbFFNvx</tool-use-id>\n<output-file>…</output-file>\n<status>completed</status>\n<summary>Background command \"Build the crate in background\" completed (exit code 0)</summary>\n</task-notification>"}
{"type":"queue-operation","operation":"dequeue","timestamp":"…","sessionId":"…"}
{"type":"user","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>…（同じ内容）"}, …}
```

（`sessionId` の値と、`user` の record の他の欄（`uuid`・`parentUuid`・`cwd`・`version` など）は省いた。）

background のコマンドの summary は `Background command "<Bash tool の description>" completed (exit code N)` か `... failed with exit code N` で、`description` の無いコマンドはコマンドの全文が入る。async の Agent の完了は `<summary>Agent "<description>" finished</summary>` に、`<note>`（同じ task-id が複数回通知しうる）・`<result>`（報告は agent-message で渡した旨）・`<usage>`（`subagent_tokens`・`tool_uses`・`duration_ms`）が続く。

不一致・注意（receipt の follow_ups に出した）:

- **引数や heredoc の中の `cargo` で分類が誤る**: `dagq ask --question '... sleep 40 && cargo build --release ...'` が `build` に、receipt を heredoc で書く `cat > receipt.json.tmp <<'EOF' ... EOF`（本文に `cargo build`・`cargo test` などの経過を書いた）が `chain` に数えられた。分類は `&&` / `||` / `;` / 改行で分けた部分ごとに、部分の中のどこかにある `cargo` の後の語を subcommand として見るので、引用符や heredoc の中の文字列も数える。
- **harness が拒んだコマンドも失敗した実行に数える**: `sleep 40 && cargo build --release` は `<tool_use_error>Blocked: ...`（`is_error: true`、0 秒）で実行されなかったが、`commands.build` の `runs` と `failed` に 1 ずつ入った。
- **Ctrl-B**: pty への `\x02` で移ったのは別の対話 session。worker の run の記録は未確認のまま、task 1437 で worker が非対話になり人が worker に Ctrl-B を届けられなくなったので、この確認は行わない。以前の予定は、worker の transcript に `backgroundedByUser: true` と完了の `enqueue` があり、対応する `worktime.jsonl` と `session_exited.work_breakdown.heavy` の `start` が `tool_use`、`end` が完了通知の時刻で、`background: true`、`finished: true`、`failed` が通知と一致すること（`exit_code`・`status` は `worktime.jsonl` で照合）を確認して結果を追記することだった（timeout の行も同じ）。
- **worker の `dagq`**: task 547 では worker の workspace は supervisor の `XDG_DATA_HOME` を継がず、PATH の `dagq` は固定バイナリ `~/.local/bin/dagq` に解決した。最初の `dagq ask` は既定の `~/.local/share/dagq/<hash>/queue.db`（存在しない）を開こうとして失敗し、XDG を付けた打ち直しでは固定バイナリが scratch の queue に ask を書いた（本番 queue には触れていない）。[隔離の設定と確認](../development/manual-smoke.md#scratch-の-worker-が打つ-dagq-の環境)に `[run.env]` のコード上の根拠と手順を記した。その設定で実 worker が scratch のバイナリ・queue を使えるかは、人か inbox が `dagq-smoke` で解決先と ask / answer を確認して結果を追記する。

## 非対話の worker のスモーク

### 結果（2026-09-29、task 820 の run cdcc8000）

行っていない。この手順は task 820（goal 57 の文書の task）の受け入れ条件の 1 つだが、task 820 は dagq の worker として動き、worker は使い捨ての queue を作れず操作もできない（[隔離](../development/manual-smoke.md#隔離)。`init`・`add`・`supervise` は `authorization_denied` になる。task 983、ask 195 で人が policy を変えないと決めた）。worker は env を外して迂回しない。そのため手順だけを書き、実行は receipt の `follow_ups`（`ops`）として人か inbox に任せた。その時点の host の版は Claude Code 2.1.284（`~/.local/bin/claude` の link 先）と codex-cli 0.155.1（`~/.local/bin/codex` の link 先は `~/.codex/packages/standalone/releases/0.155.1-aarch64-apple-darwin/bin/codex`）。stub の provider での自動 test（`tests/it/runtime_headless.rs`・`runtime_codex.rs`・`runtime_codex_ask.rs`・`runtime_provider_switch.rs`）は着地済みの task 815〜818 が持つ。人か inbox が流したら、この節に版・run ごとの確認点の結果・見つけた問題を足す。

### 結果（2026-09-30、本番の queue の記録から、task 1102）

使い捨ての queue の手作業（手順 1〜5・7・8）の代わりに、本番の queue に着地した実物の非対話の worker の run の記録を読んで、手順 6 の確認点を当てた（ask 214 で人が決めた。worker は使い捨ての queue を操作できない）。読んだのは固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+088c8ec`）の状態を変えないコマンド（`show 1103 --full`・`events --run <RUN> --full --all`・`timeline <RUN>`・`stats --full`・`kpi --since 2026-09-29T00:00:00Z --until 2026-09-30T00:00:00Z --by provider` と `--by route`）と、run dir の `turns/` と `idle.json`、着地した commit（`git show --stat`・`git ls-tree`）、`~/.codex/config.toml` の更新時刻。queue の状態は変えていない。

- **Codex**: task 1103（`--provider codex`、非対話）の run `0a8042c2-349e-40f6-afa5-2e5a8f06f0fb`。2026-09-29T18:24:51Z に claim、18:44:08Z に着地（commit `b9643d5`）。codex-cli 0.155.1（`run_claimed` の `codex_version`・`provider_version`）、supervisor の Claude Code は 2.1.283（`claude_version`）、supervisor の dagq は `0.4.0-dev+4f87a20`。
- **Claude**: 非対話の Claude の run は **無かった**。task 821 は本番の queue に非対話の Claude の run を 1 本も挙げておらず（[headless-worker-measurement](headless-worker-measurement.md)。planner が `--headless` を付けた task が無い）、この task の時点の `stats --full` の 661 本も provider・actual_provider・route の組は `claude`/`claude`/`null`（460 本、task 819 の前）・`claude`/`claude`/`interactive`（200 本）・`codex`/`codex`/`headless`（上の 1 本）だけだった。Claude の分の確認点は全て確かめていない。確かめる続け方は receipt の `follow_ups` に出した。この時点の host の Claude Code は 2.1.285、codex-cli は 0.155.1。

Codex の run の確認点ごとの観察と結論:

| 確認点 | 観察（読んだコマンドと値） | 結論 |
| --- | --- | --- |
| task と run の状態 | `show 1103 --full`: task `status: completed`、run `status: integrated`、`task_status_changed` in_progress→completed（event 45475）、`run_integrated`（45473、commit `b9643d5`）、`push_finished`（45482） | 着地した |
| provider と切り替え | `show`: task `provider: codex`、run `requested_provider: codex`・`actual_provider: codex`。`events --full --all` に `provider_switched` は無く、`timeline` の `provider_switches: []`、`stats` の `runs[].provider_switches: 0` と全体の `provider_switches.count: 0` | 切り替えは無い |
| 経路 | task・run の `worker_mode: headless`、`session_opened`（45425）の `provider: codex`・`route: headless`、`timeline` の `worker_mode: headless` | 非対話の経路で動いた |
| `run_claimed` | `provider: codex`・`requested_provider: codex`・`worker_mode: headless`・`provider_version: 0.155.1`・`codex_version: 0.155.1`・`claude_version: 2.1.283`。`model: null`・`ladder_model: claude-opus-5-5`・`model_unknown`（codex は自分の model で動く旨） | 設計どおり（[provider-lifecycle](../design/provider-lifecycle.md) の model の記録） |
| turn の列 | turn は 1 つだけ。`turn_started`（45426、`turn: 1`・`resume: false`・`session_id: null`・`request: null`・`silence_secs: 900`・`limit_secs: 14400`）→ `turn_session_identified`（45427、2 秒後、thread `01a0ee69-a210-7613-8db8-d5125e407d9c`）→ `turn_finished`（45432、`outcome: succeeded`・`failure: null`・`exit_code: 0`・`permission_denials: 0`・`denied_tools: []`・`session_created: true`・`model: gpt-6-astra`、`usage` は `input_tokens` 1,283,330・`cached_input_tokens` 1,227,136・`output_tokens` 2,881・`reasoning_output_tokens` 241、`tokens` と `tokens_total` は input 56,194・cache_read 1,227,136・output 2,881）。最初の turn は task の prompt なので `turn_requested` は無い（依頼の file が無い）。`cost_usd`・`duration_ms`・`num_turns` は null（Codex の出力が持たない） | 設計どおり。`tokens.input` は `input_tokens` から cache の分を引いた値 |
| resume の turn | answer・revise・needs_session の resume は無かった（`stats` の `resumes: 0`、`review_verdict: pass`、`ask` 0）。integrate の 1 回目の検証が `verification_flaky`（`runtime_adopt::a_supervisor_that_lost_its_lease_stops_touching_the_run`）で、resume せずに着地をやり直した（`integration_retried`） | 同じ thread の resume は確かめていない |
| `turns/` の出力 | `turn-000001.jsonl` は `codex exec --json` の JSONL の 28 行（`thread.started` 1・`turn.started` 1・`item.started`/`item.completed` の `command_execution` 各 9・`file_change` 各 1、`item.completed` の `agent_message` 5、`turn.completed` 1。最後の `turn.completed` の `usage` が `turn_finished` の `usage` と一致）。`turn-000001.err` は `Reading additional input from stdin...` の 1 行、`limits.json` は `{"silence_secs":900,"limit_secs":14400}`、`exit`（終了の依頼）は空の file。`request-*.json` は無い。`idle.json` は `dagq_turn`（`turn: 1`・`outcome: succeeded`）を持つ | 設計どおり（[非対話のworker](../design/supervisor-lifecycle/headless-worker.md#run-dirのturns)） |
| sandbox の中の作業 | `command_execution` 9 件は全て `exit_code: 0`: `cargo fmt --all --check && cargo clippy ... && cargo test --locked --bin dagq`、`cargo run --locked --bin dagq -- supervise --help`、`git commit`、receipt の書き込み（一時 file と rename） | cargo の build と test・run branch への commit・receipt が通った。`dagq ask` と `pkill` / `killall` の拒否はこの run では試されていない |
| `.codex/rules/dagq-deny.rules` | 着地した commit `b9643d5` の差分は `src/main.rs` だけ（`git show --stat`）で、`git ls-tree -r` の tree に `.codex/` は無い | commit に入っていない |
| `~/.codex/config.toml` | 更新時刻が 2026-09-30 03:24:53 JST（= 18:24:53Z、`turn_started` と同じ秒）。中身の末尾に `[tui.model_availability_nux]` の `gpt-6-astra = 4` があり、run の worktree の `[projects."…"]` の行は無い。dagq は書かない（design）ので、起動された Codex CLI が自分で書いたとみられる | 手順 6 の「更新時刻が変わっていない」は満たさない。設計の前提（人の設定を変えない）と違う観察として receipt の `follow_ups` に出した |
| `origin/main` の commit | `git log --oneline --grep 'Dagq-Task: 1103' main` は `b9643d5` の 1 行だけ。`push_finished`（45482）は `remote: origin`・`branch: main`・commit `b9643d5` | task に 1 commit が push された |
| stats の層 | `stats --full` の `runs[]` は `provider: codex`・`actual_provider: codex`・`route: headless`・`provider_version: 0.155.1`・`codex_version: 0.155.1`・`claude_version: null`・`worker_model: gpt-6-astra`・`turns.by_provider.codex`（`count` 1・`failed` 0・`secs` 216・`tokens.total` 1,286,211）・`work` 214 秒・`startup` 185 秒 | provider・経路・turn が見える |
| kpi の層 | 2026-09-29 の 1 日の窓で `--by provider` に `provider=codex`（`landings` 1、`phase.work` 214、`phase.startup` 185、`session_active.worker` 216、`resumes_per_run` 0）と `provider=claude`（landings 69）、`--by route` に `route=headless`（1）と `route=interactive`（69）が分かれて出た。goal review の Codex の job も `job.count.goal_review` の `provider=codex` に出た | 分かれて出る |
| `timeline` | `requested_provider`・`actual_provider`・`worker_mode` を持ち、`commands` は空。`session span 45425 (worker): no model or work breakdown, transcript_not_claude` と注記 | Codex の session は作業の内訳を持たない（設計どおり） |

本番の記録からは確かめていないもの:

- 非対話の Claude の run の全ての確認点（run が無い。Claude の answer の turn が同じ session の resume になること、`turn_started` の `session_id` が run の id であること、stream-json の `turns/` を含む）。
- Codex の同じ thread の resume（answer・revise・needs_session の resume が無かった）。Codex の worker の `dagq ask` は run の dir への要求にする途中（task 890）で、まだ queue に届かない。
- Codex の sandbox が `pkill` / `killall` を拒むこと（run が打たなかった）。
- 手順 7 の `executable_missing` による切り替え（本番では `--codex` を壊せない）と、手順 8 の後始末（使い捨ての queue を作っていない）。
- 手順 4 の `status` の `supervisors[].providers`（run の当時の値は記録に残らない）。
- run の当時の `~/.codex/config.toml` の中身（前後を比べていない。更新時刻と今の中身だけ）。

### 結果（2026-10-05、task 1174 後の本番の Codex の記録から、task 1205）

ask 214 の方針に従い、本番の既存記録を読み取った。worker は実 Codex を起動せず、queue・runtime・`~/.codex/config.toml` を変更していない。以下の時刻は UTC。観測する event の窓は **2026-09-30T05:27:31Z 以上、2026-10-05T08:00:00Z 未満**に固定した（53,379 event、最後は 2026-10-05T07:59:54.965Z）。測定時の `stat` は 2026-10-05T10:04Z 頃の値で、task context の 2026-09-30T12:27:29Z から変わっていたため、新しい更新時刻を含む期間で照合した。

読んだコマンド（`dagq` は `which dagq` で確認した固定バイナリ `~/.local/bin/dagq`）:

```sh
dagq events --full --kind update_installed --all --since 2026-09-30T05:27:31Z --limit 100000
dagq events --full --kind turn_started --all --since 2026-09-30T05:27:31Z
dagq events --full --all --since 2026-09-30T05:27:31Z --until 2026-10-05T08:00:00Z --limit 1000 --after <cursor>
dagq status
stat -f '%m %Sm' -t '%Y-%m-%dT%H:%M:%S%z' ~/.codex/config.toml
rg -A1 '^\[projects' ~/.codex/config.toml
```

`--all` は件数の上限を外さない。2 行目の既定 100 件だけでは窓を覆わないので、3 行目を `--after 0` から返された `cursor` で繰り返し、1000 件未満の最後の page まで取得した。全 event を `--limit 100000` で一度に読む試みは queue service の応答の 16 MiB 境界で失敗したため、集計には使っていない。取得した JSON を Python で読み、worker は `kind: turn_started`・`payload.provider: codex`・`run_id` 有り、job は `*_started` の `payload.launch.provider: codex` を選んだ。終了は run・turn、または job ID・attempt を対応づけた。

| 確認点 | 観察 | 結論 |
| --- | --- | --- |
| 固定バイナリの更新 | `update_installed` event 49415、2026-09-30T05:27:31.861Z、commit `fddd507508fb4e7862a1931d98720384c742c57a`、version `0.4.0-dev+fddd507508fb4e7862a1931d98720384c742c57a` | task 1174 の trust override を含む更新の後を読んだ |
| Codex CLI の版 | `status.supervisors[].providers` の codex は `found: true`、`modes: [headless]`、executable `/Users/shinnosukeooyama/.codex/packages/standalone/releases/0.160.0-aarch64-apple-darwin/bin/codex` | 現在は 0.160.0。task 1174 の source 調査は 0.155.1。窓内の Codex の `run_claimed.codex_version` は 0.155.1 が 95 件、0.159.2 が 1 件、0.160.0 が 4 件（resume や窓の前に claim された run の版はこの件数に含めない） |
| config の更新時刻 | `stat`: epoch `1790953911`、`2026-10-03T00:11:51+0900` = **2026-10-02T15:11:51Z**。窓内の同じ秒の event は 0 件 | Codex の exec・resume・job の開始と一致しない。書いた process はこの記録だけでは特定できない |
| worker の開始 | 104 run・168 turn（`resume: false` 102、`true` 66）。最初は下表の task 1142、最後は task 1437 turn 5 | 更新時刻と同じ秒の exec・resume は 0。更新後にも 7 exec・10 resume があり、その開始・終了より config の mtime は古い |
| job の開始 | Codex は plan review 395、run review 206、goal review 48、throughput review 14。config の更新後はそれぞれ 346・132・41・12 件 | 同じ秒の job 開始は 0。ただし plan review 617 の実行区間は更新時刻を含む（下表）。開始との不一致だけで、その実行中に書かなかったとは判定しない |

読んだ worker の具体例を以下に示す。先頭 3 行は更新後の最初の exec・最初の resume・mtime 直前の最後の worker 開始で、それ以降は **mtime 後の 17 turn 全件**。`resume: false` を exec、`true` を resume とした（turn 番号が 2 以上でも新 thread の exec がある）。task 705 のこの turn には終了 event が無く、成功した resume の証拠には使わない。

| task | run ID | turn / 起動 | 開始 → 終了（UTC） | event ID |
| --- | --- | --- | --- | --- |
| 1142 | `e94e23b4-ff84-451a-85fc-e0589f18e82b` | 1 / exec | 2026-09-30T05:31:22.148Z → 2026-09-30T05:38:28.597Z | 49442 → 49478 |
| 705 | `86e8a830-ddef-4f8e-92d9-e775f50c225a` | 2 / resume | 2026-09-30T09:12:22.832Z → 終了 event 無し | 51430 → — |
| 955 | `0ce0d8a2-a1d4-49e1-a173-3fc90086c562` | 8 / resume | 2026-10-02T00:04:27.886Z → 2026-10-02T00:08:57.188Z | 58854 → 58923 |
| 1460 | `30d8c537-bc04-4589-a9ad-ecc7f0c97d1a` | 1 / exec | 2026-10-03T03:30:38.796Z → 2026-10-03T04:00:32.145Z | 72173 → 72434 |
| 1437 | `3307f9a3-948a-4530-b597-e8b1cd1c2996` | 2 / exec | 2026-10-04T02:18:08.140Z → 2026-10-04T02:31:59.767Z | 81234 → 81335 |
| 1408 | `7caa494a-1516-4edf-b56c-899f9366b9cf` | 1 / exec | 2026-10-04T02:19:02.233Z → 2026-10-04T02:22:33.281Z | 81263 → 81267 |
| 1505 | `0de6cd72-bb17-45b8-815a-67df22a1101b` | 1 / exec | 2026-10-04T02:30:20.069Z → 2026-10-04T03:06:15.901Z | 81309 → 81638 |
| 1520 | `e9536c58-fd75-45ad-a56f-142528c22ca6` | 3 / exec | 2026-10-04T02:30:32.401Z → 2026-10-04T02:43:53.591Z | 81332 → 81425 |
| 1583 | `6357a771-18de-44db-a425-e73f6efbaead` | 1 / exec | 2026-10-04T02:33:23.079Z → 2026-10-04T02:38:58.770Z | 81357 → 81389 |
| 1189 | `9874facf-b0e8-47dd-9d93-5d4052d079a2` | 1 / exec | 2026-10-04T02:38:34.804Z → 2026-10-04T02:47:26.123Z | 81387 → 81464 |
| 1437 | `3307f9a3-948a-4530-b597-e8b1cd1c2996` | 3 / resume | 2026-10-04T02:44:16.513Z → 2026-10-04T02:50:14.141Z | 81458 → 81506 |
| 1520 | `e9536c58-fd75-45ad-a56f-142528c22ca6` | 4 / resume | 2026-10-04T02:47:54.292Z → 2026-10-04T02:52:07.426Z | 81492 → 81548 |
| 1583 | `6357a771-18de-44db-a425-e73f6efbaead` | 2 / resume | 2026-10-04T02:50:09.841Z → 2026-10-04T02:50:44.993Z | 81505 → 81513 |
| 1189 | `9874facf-b0e8-47dd-9d93-5d4052d079a2` | 2 / resume | 2026-10-04T02:51:12.926Z → 2026-10-04T02:51:46.072Z | 81541 → 81543 |
| 1189 | `9874facf-b0e8-47dd-9d93-5d4052d079a2` | 3 / resume | 2026-10-04T02:53:48.566Z → 2026-10-04T02:55:58.217Z | 81563 → 81579 |
| 1437 | `3307f9a3-948a-4530-b597-e8b1cd1c2996` | 4 / resume | 2026-10-04T03:06:07.742Z → 2026-10-04T04:06:08.536Z | 81634 → 81955 |
| 1505 | `0de6cd72-bb17-45b8-815a-67df22a1101b` | 2 / resume | 2026-10-04T03:09:36.242Z → 2026-10-04T03:17:07.756Z | 81691 → 81723 |
| 1505 | `0de6cd72-bb17-45b8-815a-67df22a1101b` | 3 / resume | 2026-10-04T03:19:32.376Z → 2026-10-04T03:31:32.944Z | 81757 → 81832 |
| 1583 | `6357a771-18de-44db-a425-e73f6efbaead` | 3 / resume | 2026-10-04T03:28:05.737Z → 2026-10-04T03:50:48.003Z | 81821 → 81894 |
| 1437 | `3307f9a3-948a-4530-b597-e8b1cd1c2996` | 5 / resume | 2026-10-04T04:09:27.090Z → 2026-10-04T04:47:46.592Z | 81985 → 82197 |

job の時刻の照合（開始・終了の event を読む）:

| job / task・goal | 開始 → 終了（UTC） | event ID | mtime との関係 |
| --- | --- | --- | --- |
| plan review 616 / task 1411 | 2026-10-02T15:06:42.718Z → 15:09:48.273Z | 64429 → 64467 | 更新前に終了 |
| plan review 617 / task 1418 | 2026-10-02T15:09:48.615Z → 15:13:14.789Z | 64470 → 64522 | 開始は一致しないが、実行中に更新時刻がある |
| plan review 618 / task 1420 | 2026-10-02T15:13:15.327Z → 15:17:27.576Z | 64525 → 64592 | 更新後に開始・終了 |
| goal review 18 / goal 75 | 2026-10-01T18:31:35.474Z → 18:33:12.503Z | 56080 → 56104 | task 1174 の更新後の最初の Codex goal review、mtime より前 |
| goal review 24 / goal 58 | 2026-10-02T09:39:40.609Z → 09:44:28.945Z | 61726 → 61745 | mtime に最も近い goal review の開始、更新前に終了 |

plan review 617 と goal review 24 は `<queue>/plan-reviews/617/review.out`・`<queue>/goal-reviews/24/review.out` の `thread.started` から rollout を探して読んだ（`<queue>` は `~/.local/share/dagq/77067154921b9014`）。thread は順に `01a0fd2a-247e-7351-a195-c2046852f855`・`01a0fbfb-d9f8-7950-81b2-08f38ec2ab18`。`~/.codex/sessions/2026/10/03/rollout-2026-10-03T00-09-52-01a0fd2a-247e-7351-a195-c2046852f855.jsonl` と `2026/10/02/rollout-2026-10-02T18-39-42-01a0fbfb-d9f8-7950-81b2-08f38ec2ab18.jsonl` の `session_meta` はともに `originator: codex_exec`・`cli_version: 0.159.2`、最初の `turn_context`（15:09:55.302Z / 09:39:45.389Z）は `sandbox_policy: {type: read-only, network_access: true}`・`approval_policy: never`・cwd は main checkout。これは job の設定の観察であり、job 617 と config 更新の因果関係の証拠にはしない。

`[projects]` の現在の一覧（path の先頭の `~` は `/Users/shinnosukeooyama`）:

| project | trust_level |
| --- | --- |
| `~/.anyenv` | untrusted |
| `~/.local/share/chezmoi` | trusted |
| `~/ghq/github.com/hisamekms/wt-playground` | trusted |
| `~/ghq/github.com/hisamekms/formula` | untrusted |
| `~/work/nightingale` | trusted |
| `~/ghq/github.com/hisamekms/toma-app` | trusted |
| `~/ghq/github.com/hisamekms/cmux-taskq` | trusted |
| `/private/tmp/claude-501/-Users-shinnosukeooyama-ghq-github-com-hisamekms-dagq/35ae56f1-3752-4068-afbd-1559abe9ad3e/scratchpad/hl/repo` | trusted |
| `~/.local/share/dagq/77067154921b9014/runs/7cf22aa5-ed53-4e47-9956-322c0a18daa9/spike/main` | trusted |
| `~/ghq/github.com/hisamekms/dagq`（main checkout） | trusted |

**結論**: task 1174 後の起動と同じ秒に config が更新された観察は無く、少なくとも最後の更新より後に実行された worker の exec 7 件・resume 10 件は config を書き直していない。更新後に始まった job も同様に、測定時点まで mtime を進めていない。現在の `[projects]` に上表の実測 run の worktree の trust は無く、queue の `runs/` 配下の key は spike の `spike/main` 1 件だけだった。main checkout の trust は存在するが、更新前の config の snapshot が無いため、これら 2 欄がいつ足されたか・task 1174 後に追加や同じ内容の書き直しがあったかは判定できない。mtime より前の turn・job は後の更新に隠れるため、全期間の前後不変を証明したとはしない。job 617 の実行中の更新も書き込み元・追加された欄は未特定で、exec・resume・job による書き込みが残っているという defect の証拠は得られなかった。task 1102 の開始と同じ秒の更新という観察は、今回の worker の記録では再現されなかった。

### 結果（2026-09-30、本番の Claude の記録から、task 1175）

task 1102 の後、ask 224 で人が了承して planner が `--headless` を付けた次の 7 task が着地した。ask 214 と同じく使い捨ての queue は作らず、本番の記録に手順 6 を当てた。固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+0e8c4020daa113cddac40504c1a2980d3ea56649`）で、各 task の `show <ID> --full`、各 run の `events --run <RUN> --full --all`・`timeline <RUN>`、`stats --full`、`kpi --since 2026-09-30T00:00:00Z --until 2026-09-30T03:00:00Z --by provider` と `--by route` を読んだ。加えて run dir（`~/.local/share/dagq/77067154921b9014/runs/<RUN>/`）の `turns/`・`claude-headless-settings.json` と Git の記録を読んだ。queue の状態は変えていない。以下の時刻は UTC。

| task | run ID | claim → integrated（2026-09-30） | 着地 commit | turn 数 |
| --- | --- | --- | --- | --- |
| 1097 | `b80aa96e-00af-4fbf-9dfc-2d311f366230` | 01:33:36 → 02:08:06 | `701ce4f9` | 1 |
| 1096 | `b1adcaab-f8f5-497d-bf8e-c254f991d54c` | 00:33:03 → 00:50:46 | `0aec4d9d` | 1 |
| 1042 | `934022a1-25ff-47b9-b619-4872779dc632` | 00:33:05 → 00:57:25 | `34447280` | 1 |
| 1058 | `c6ef5297-f626-41a8-ac91-97cccd00e2f7` | 00:57:33 → 01:33:29 | `eb5c5d27` | 1 |
| 1110 | `aeb46a29-d188-4fac-964f-267d80b2c3bb` | 02:08:14 → 02:09:25 | `5ddbf68e` | 1 |
| 1107 | `4fa32d81-5aa5-41ee-8620-da278ff6b055` | 00:22:46 → 00:32:59 | `d33dfc9d` | 1 |
| 1050 | `2d1f5abd-0c40-4cd7-8073-9f0ce058ae9a` | 00:57:33 → 02:01:25 | `530b32fb` | 2（revise 1 回） |

**Claude Code は全 7 run・8 turn とも 2.1.283**（`run_claimed.provider_version`・`claude_version` と stream の `system/init.claude_code_version` が一致）。現在の host の版から推測した値ではない。claim 時の dagq は task 1107・1096・1042 が `b502eb9c`、1058・1050 が `d33dfc9d`、1097 が `34447280`、1110 が `20f6bd62` の dev build。

| 確認点 | 観察（読んだコマンドと値） | 結論 |
| --- | --- | --- |
| task と run の状態 | 7 件の `show --full` が task `status: completed`、run `status: integrated`。各 `events` に表の commit の `run_integrated` と `task_status_changed`（in_progress→completed） | 全て着地した |
| provider と切り替え | 7 件とも task `provider: claude`、run `requested_provider: claude`・`actual_provider: claude`。各 `events` の `provider_switched` は 0 件、各 `timeline.provider_switches: []`、`stats.runs[].provider_switches: 0` | 切り替え無し。切り替えの reason とフォールバックの実動作は未確認 |
| 経路と claim | `show` と `timeline` の `worker_mode: headless`。7 件の `run_claimed` は `provider: claude`・`requested_provider: claude`・`worker_mode: headless`・`provider_version: 2.1.283`・`claude_version: 2.1.283`・`codex_version: 0.155.1`・`model: claude-opus-5-5` | Claude の非対話の経路で動いた |
| turn の列 | 初回は 7 件とも `turn_started` の `turn: 1`・`resume: false`・`request: null`・`what: the task's prompt`・`session_id: <RUN>`。最初の prompt に `turn_requested` は無い。開始→終了の event ID は 1097: 48512→48592、1096: 48118→48193、1042: 48129→48199、1058: 48288→48437、1110: 48688→48703、1107: 48035→48040、1050: 48292→48420。8 turn 全て `turn_finished` は `outcome: succeeded`・`failure: null`・`exit_code: 0`・`permission_denials: 0`・`denied_tools: []`・`session_created: true`・`model: claude-opus-5-5`、`session_id` は run ID | 開始・終了・model・結果を記録できた。最初の turn に依頼の file が無いのも設計どおり |
| revise の resume | 1050 の `review_finished`（48427）が `verdict: revise` → `turn_requested`（48433、`seq: 1`・`what: revise request`）→ `turn_started`（48434、`turn: 2`・`request: 1`・`resume: true`・`session_id: 2d1f5abd-0c40-4cd7-8073-9f0ce058ae9a`）→ `turn_finished`（48474、成功）→ review pass（48479）→ integrated（48633）。`timeline` に worker と revise の command 区間がある | 同じ session の revise を確認。run の再作成ではない |
| answer・needs_session の resume | 全 7 件の `events --full --all` に `ask_opened`・`session_resumed` は無く、needs_session への遷移も無い。依頼は上の revise 1 件だけ。`stats.runs[].resumes: 0` は全 7 件（revise はこの欄の resume に数えない） | worker_question と answer、needs_session からの同じ session の resume は確かめていない。1050 でも質問は出なかった |
| usage と tokens | 8 個の JSONL の最後の `result.usage` が各 `turn_finished.usage` と一致。`usage.input_tokens`・`cache_creation_input_tokens`・`cache_read_input_tokens`・`output_tokens` が `tokens.input`・`cache_creation`・`cache_read`・`output` に対応（下表）。`tokens_total: null`、`tokens.messages: 1`。`duration_ms` と `num_turns` も記録される | turn の usage と token 数を読めた。`num_turns` は CLI 内の turn 数で、dagq の turn の数ではない。費用には下記の旧記録の注意がある |
| `turns/` の出力と依頼 | 全て stream-json。各 file に `system/init` と最後の `result`（`subtype: success`・`is_error: false`）が 1 件ずつあり、両方の `session_id` は run ID、init の `permissionMode: auto`。間に `assistant`・`user`・`rate_limit_event`・`system/thinking_tokens`、tool の進捗などがある。行数は下表。8 個の `.err` は空。全 7 dir の `limits.json` は `silence_secs: 900`・`limit_secs: 14400`、`exit` が残る。1050 だけ `request-000001.taken.json`（`seq: 1`・`what: revise request`）と `turn-000002.jsonl` があり、他の 6 dir に request は無い | stream と依頼・event が対応。resume 後の stream も同じ session ID |
| 人の Claude の設定 | 全 7 run の `claude-headless-settings.json` に `permissions.deny`（`Bash(pkill:*)`・`Bash(killall:*)` など）と `autoMode.environment: ["$defaults"]` があり、hooks は無い。claim の 4 build を `git show <commit>:src/infrastructure/adapters.rs` で読み、`turn_command` の書き込み先が `run_dir.join(HEADLESS_SETTINGS)`、起動引数が `--settings <その file>` であることを確認。`~/.claude/settings.json` の現在の mtime は 2026-09-30T02:43:57.387735Z（7 run の終了後） | runtime の worker 起動が人の設定でなく run ごとの設定を書くことは、当時のコードと残った file で確認。実行前の設定の控え・書き込み監査が無いため、`~/.claude` 全体の不変や CLI 自身・他 session による更新の有無は証明できない |
| stats の層 | `stats --full` の対象 7 run は全て `provider: claude`・`actual_provider: claude`・`route: headless`・`provider_version: 2.1.283`・`claude_version: 2.1.283`・`worker_model: claude-opus-5-5`。`turns.by_provider.claude` は計 8 turn・failed 0。1050 は count 2・secs 1849・tokens.total 11,508,379、他は count 1（下表） | provider・経路・turn が見える。revise の turn も含む |
| kpi の層 | 指定した 00:00〜03:00 の窓では `--by provider` の `landings.provider=claude` が 16、`--by route` は `route=headless` が 7・`route=interactive` が 9。headless の `phase.work` は n 7・中央値 323 秒、`session_active.worker` は n 7・中央値 326 秒、`resumes_per_run` は n 7・value 0 | stats の対象 7 run と照合して Claude/headless の着地を確認。provider と route は別の切り口で、Claude の 16 件を全て headless と読まない |
| `origin/main` の commit | `git log --format=%h --grep 'Dagq-Task: <ID>$' HEAD` は各 task に表の 1 commit。`git merge-base --is-ancestor <commit> origin/main` は 7 件とも 0。6 件に `push_finished` があるが、1097 は `push_failed`（48659、remote の main の ref lock 競合）で、その run の `push_finished` は無い | local の origin/main 参照には 7 commit が含まれる。1097 自身の push 成功とは言えない（live remote は問い合わせていない） |

`turn_finished` と stream から読んだ turn ごとの値（tokens は input / cache_creation / cache_read / output の順。行数は `turn-NNNNNN.jsonl`、stats の秒は `turns.by_provider.claude.secs`）:

| task / turn | tokens | duration_ms / num_turns | JSONL 行数 | stats 秒 / tokens.total（run 全体） |
| --- | --- | --- | --- | --- |
| 1097 / 1 | 52 / 85,486 / 2,060,689 / 12,197 | 560138 / 28 | 235 | 562 / 2,158,424 |
| 1096 / 1 | 24 / 58,007 / 734,543 / 4,349 | 324556 / 12 | 58 | 326 / 796,923 |
| 1042 / 1 | 32 / 74,384 / 1,151,169 / 6,454 | 324854 / 16 | 98 | 326 / 1,232,039 |
| 1058 / 1 | 98 / 124,624 / 4,761,313 / 33,417 | 1699967 / 49 | 453 | 1701 / 4,919,452 |
| 1110 / 1 | 10 / 59,008 / 280,380 / 2,148 | 32229 / 5 | 16 | 34 / 341,546 |
| 1107 / 1 | 12 / 55,921 / 328,179 / 2,167 | 47933 / 6 | 23 | 50 / 386,279 |
| 1050 / 1 | 120 / 165,175 / 7,626,404 / 44,474 | 1431499 / 63 | 450 | 1849 / 11,508,379（2 turn 合計） |
| 1050 / 2 | 38 / 27,085 / 3,627,284 / 17,799 | 414080 / 19 | 102 | 同上 |

旧記録の注意: 1050 の `turn_finished.cost_usd` は 1 回目 4.721233400000003、2 回目 6.019502200000002 で、stream の `result.total_cost_usd`（session の累計）をそのまま記録している。`stats.turns.by_provider.claude.tokens.cost_usd` は 10.740735 となり、turn の費用として足すと重複する。これは [非対話の worker の Claude の読み手](../design/supervisor-lifecycle/headless-worker.md) に記載された task 1199 より前の記録で、修正後も過去の記録は補正しない。今回見た不一致と、修正後の実 resume の測定を receipt の follow_ups に残した。

この 7 run では、Claude の着地と revise の同一 session の resume を確認できた。未確認なのは answer と needs_session の resume、provider が使えない場合の切り替え、設定の実行前後の比較、手順 1〜5・7・8 の使い捨て queue での操作。Codex 固有の rules・sandbox・ask-requests の確認点は今回の Claude の対象外で、task 1102 の未確認事項を解消したとはみなさない（その後 task 1236 で Codex の worker の `dagq ask` は queue service 経由になり、確認点は[手順](../development/manual-smoke.md#非対話の-worker-のスモーク)の「Codex の run の ask」に変わった）。

## 非対話の Claude の前提の確認

非対話の Claude の経路（[非対話の worker](../design/supervisor-lifecycle/headless-worker.md)）が前提にしていて、stub では確かめられない 3 点を、実 `claude -p` で確かめた（task 864、2026-09-30）。queue も supervisor も使わず、scratch の使い捨て repository `dagq-worker-t864-1e1fe057`（`git init -b main` と `seed.txt` の 1 commit）で `claude` を直接起動した。Claude Code は 2.1.285（`~/.local/bin/claude` の link 先 `~/.local/share/claude/versions/2.1.285`）。本番の queue・`~/.local/bin/dagq`・`~/.claude` の設定は変えていない（2. で `~/.claude/projects` に置いた transcript と、1. の turn が書いた transcript は、終わってから消した）。Claude Code の版を上げたとき、または読み手（`src/infrastructure/claude_turns.rs`）・turn の止め方（`headless_session.rs` の `stop_turn`）・`turn_session_exists` を変えたときに、同じ手順で確かめ直す。

### 1. turn の process group と Bash tool の子

打ったもの: python の driver（`subprocess.Popen(..., start_new_session=True)`。runtime の `CommandSpec::new_session` と同じく turn を自分の process group と session で起動する）で次を起動した。

```sh
claude -p --output-format stream-json --verbose --session-id <uuid> --permission-mode auto --model sonnet -- '<prompt>'
```

prompt は Bash tool で 1 つずつ次を打たせるもの: `nohup sleep 7101 >/dev/null 2>&1 &`、`(sleep 7102 &)`、`python3 -c 'import os,time; os.setsid(); time.sleep(7103)' >/dev/null 2>&1 &`、`run_in_background: true` の `sleep 7104`、前景の `python3 -c 'import time; time.sleep(40)'`（前景の `sleep 40` は Claude Code の harness が「standalone sleep」として拒むので python にした）。前景の python が走っている間に `ps -axo pid,ppid,pgid,command` を取り、turn の group に signal を送ってから、もう一度 `ps` を取った。3 通り流した: (a) turn が自分で終わる、(b) group に SIGTERM、(c) group に SIGKILL（runtime の `kill_group` と同じ signal）。

観察（(c) の run。pid は一例）:

| process | pid | ppid | pgid |
| --- | --- | --- | --- |
| `claude -p ...`（turn） | 50530 | driver | 50530 |
| Bash tool の shell（`/bin/zsh -c source ~/.claude/shell-snapshots/... eval 'sleep 7104'`、background） | 59046 | 50530 | 59046 |
| `sleep 7104` | 59051 | 59046 | 59046 |
| Bash tool の shell（前景の python） | 61377 | 50530 | 61377 |
| 前景の python | 61386 | 61377 | 61377 |
| `nohup sleep 7101 &` | 54609 | 1 | 54607 |
| `(sleep 7102 &)` | 55530 | 1 | 55526 |
| `os.setsid()` の python | 56873 | 1 | 56873 |

- Claude Code は Bash tool の 1 回ごとに shell を別の process group（pgid がその shell の pid）で起動する。turn の group にいるのは `claude` 自身だけで、tool のコマンドはどれも turn の group に入らない（Codex と同じ形。[codex-headless-jobs-spike](codex-headless-jobs-spike.md) の 4.）。
- `&` で切り離したもの（`nohup … &`・`( … &)`・`setsid`）は、tool の shell が終わった時点で親が 1 になり、turn の子孫でもなくなる。
- (a) turn が自分で終わったとき: `claude` は終わる前に `run_in_background` の task（`sleep 7104`）を止めた（stream に `task_updated`・`task_notification`）。`&` で切り離した 3 つは親 1 のまま残った。
- (b) group に SIGTERM: `claude` が自分で片付け、前景の python は exit code 137（`claude` が SIGKILL で止めた。stream の `tool_result` が `Exit code 137`）、`sleep 7104` も止まった。`claude` は 143 で終わった。`&` で切り離した 3 つは残った。
- (c) group に SIGKILL: `claude` だけが止まり、2 つの tool の shell とその子（`sleep 7104`・前景の python）は親 1 になって残った。`&` で切り離した 3 つも残った。

結論: group への signal だけでは Claude の turn の tool のコマンドに届かない。今の実装の `stop_turn` は group を止める前に子孫を pid で集めて止めるので、(c) で残った tool の shell とその子は止まる（前提どおり）。一方 `&` で切り離したものは、turn の途中で止めても自分で終わっても、group にも子孫にも入らずに残る。[非対話の worker](../design/supervisor-lifecycle/headless-worker.md#wrapperがturnを止めるとき) の「turn が自分で終わったときも group に残ったもの（`nohup … &` など）を止める」は Claude では当たらない（`nohup … &` は group に居ない）ので、その節を直した。残ったものは run の worktree で動く process として復旧 job の `stop_processes` が pid で止められる（cwd が worktree のものに限る）。runtime の修正の要否は receipt の follow_up にした。後始末: 残った process は pid で止めた。

### 2. 未ログインの最初の turn

打ったもの（未ログインは、空の dir を `CLAUDE_CONFIG_DIR` にして作った。人の `~/.claude` と keychain には触れない）:

```sh
env -u ANTHROPIC_API_KEY CLAUDE_CONFIG_DIR=<scratch>/cfg-empty \
  claude -p --output-format stream-json --verbose --session-id <uuid> --permission-mode auto -- 'say ok' </dev/null
```

観察:

- 終了コード 1。stream は 3 行: `system/init`（`apiKeySource: "none"`、`claude_code_version: "2.1.285"`、`permissionMode: "auto"`）、`assistant`（`"error": "authentication_failed"`、`model: "<synthetic>"`、text `Not logged in · Please run /login`）、`result`（`subtype: "success"`・`is_error: true`・`api_error_status: null`・`terminal_reason: "api_error"`）。task 812 の測定（[headless-worker-spike](headless-worker-spike.md)）と同じ形。
- transcript は残る: `<CLAUDE_CONFIG_DIR>/projects/<cwd を符号化した名前>/<uuid>.jsonl`（27 行。`user` の prompt、`<synthetic>` の `assistant`、`last-prompt`、`cost-state` など）。したがって `turn_session_exists` は真になり、次の turn は `--resume` になる（前提どおり）。
- 同じ `--session-id` でもう一度呼ぶと、stream に何も出さず stderr に `Error: Session ID <uuid> is already in use.` を出して終了コード 1（前提どおり。resume しなければ次の turn は必ず失敗する）。
- 未ログインのまま `--resume <uuid>` すると、同じ `session_id` で 1 と同じ `authentication_failed` の 3 行になる（終了コード 1）。
- その transcript をログイン済みの設定の `~/.claude/projects/<同じ名前>/` に写し、`claude -p --output-format stream-json --verbose --resume <uuid> --permission-mode auto --model haiku -- 'Quote my previous message in this conversation verbatim, or say NONE.'` を打つと、`session_id` がそのままで `result` が `is_error: false`・`result: "say ok"`（終了コード 0）。未ログインで終わった session に、ログインした後の `--resume` で続けられる。
- 気づいたこと: cwd の path が長いと（この run では 207 文字）、Claude Code は project の dir の名前を途中で切って hash を付ける（`...-dagq-worker-t864-1-tcfei3`）。`ClaudeTranscripts::path` は `encode_cwd` の名前で見つからなければ `projects/` の全ての dir から `<session id>.jsonl` を探すので、`turn_session_exists` はこの場合も真になる。run の worktree の path（`~/.local/share/dagq/<hash>/runs/<run id>/worktree`）は 110 文字ほどで切られない。

結論: 実装の前提（答える前に失敗した turn も transcript を残し、次は `--resume` になり、その resume で続けられる）と合っている。runtime の修正は要らない。

### 3. `rate_limit_event` の `status: rejected` と overage

再現できなかった。この測定の間の `rate_limit_event` はどれも上限の手前で、`status` は `allowed_warning` だった（2 回の turn で 4 件）:

```json
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1791090000,"rateLimitType":"seven_day","utilization":0.87,"isUsingOverage":false,"surpassedThreshold":0.75,"unifiedWindows":{"five_hour":{"utilization":0.07,"resetsAt":1790726400},"seven_day":{"utilization":0.87,"resetsAt":1791090000}}}}
```

`allowed_warning` は task 812 の測定には無かった値で、読み手は `rejected` だけを見るので、この値で turn を止めることはない（前提どおり）。このアカウントは task 812 の測定で `overageStatus: "rejected"`・`overageDisabledReason: "out_of_credits"` で、overage で賄える状態を作れない。

公開の文書に `rate_limit_event` の各欄の意味の説明は見つけられなかったので、CLI の実行ファイル（2.1.285）の文字列から読んだ（実装の細部で、版で変わりうる）:

- 応答の header から作る値は、`status` が header の `anthropic-ratelimit-unified-status`（`allowed_warning` は `allowed` に畳んでから、しきい値を超えた window があれば `allowed_warning` に戻す）、`isUsingOverage` が「`status` が `rejected` で、かつ `overageStatus` が `allowed` か `allowed_warning`」。つまり overage で賄っているときは `status: "rejected"` と `isUsingOverage: true` が同時に出る。
- CLI 自身が「止まっている」とみなす条件は `status === "rejected" && isUsingOverage !== true && overageInUse !== true`（と `resetsAt` があること）で、`isUsingOverage` が真なら上限のエラーの文を出さない（`overageStatus` が `allowed_warning` なら「usage limit に近い」の警告だけ）。
- 429 の応答から作る値は `status: "rejected"`・`isUsingOverage: false` で、header の `anthropic-ratelimit-unified-overage-status` があれば `overageStatus` に入る。

結論: overage で賄える状態では `status: rejected` が出うる（`isUsingOverage: true` を伴う）。今の読み手（`ClaudeTurnReader::line`）は `status == "rejected"` だけで `usage_limit` にして turn を止めるので、その状態では誤って止める。`isUsingOverage` か `overageInUse` が真なら止めない、という runtime の修正を receipt の follow_up にした。実物の stream では確かめていないので、次に確かめる手順は[開発文書](../development/manual-smoke.md#非対話の-claude-の前提の確認)の 3. にある。

読み手の修正（task 1153）: `ClaudeTurnReader::line` は `rate_limit_event` の `status: rejected` でも `isUsingOverage` か `overageInUse` が真なら turn を止めないようにした（CLI 自身の条件に合わせた）。実物の stream での確認は[開発文書](../development/manual-smoke.md#非対話の-claude-の前提の確認)の 3. の手順のまま残る。


## Codex の goal review のスモーク

### 2026-09-30: 本番の記録による確認（task 1114）

使い捨て queue の手作業に代えて、task 1067 の着地後に本番で動いた Codex の goal review 2 件を読んだ（2026-09-30 15:53 JST 時点）。worker は本番の状態を変えず、固定バイナリ `~/.local/bin/dagq` の読み取りコマンドと既存の file だけを読んだ。新しい job の起動・書き込みの試行・設定の変更はしていない。

版: `codex --version` は `codex-cli 0.155.1`、`claude --version` は `2.1.285 (Claude Code)`。対象 2 件の rollout の `session_meta.payload.cli_version` も `0.155.1`（`originator: codex_exec`）。Claude の版は測定時の版で、過去の Claude の job の版を示すものではない。今回の `codex --version` は PATH alias を作れない警告（`Operation not permitted`）を出したが、下記の job の `review.err` にはその警告は無い。

読んだコマンド（以下の `dagq` は全て固定バイナリ）:

```sh
~/.local/bin/dagq locate
~/.local/bin/dagq events --full --all --run ad2a9f09-1eb3-482c-8329-126dbdf35207
~/.local/bin/dagq events --full --all --kind goal_review_started --kind goal_review_finished --kind goal_review_failed --since 2026-09-29T17:52:27Z
~/.local/bin/dagq stats --full
codex --version
claude --version
```

`locate` の `db` は `~/.local/share/dagq/77067154921b9014/queue.db`。その親を以下の `<queue>` とする。1067 の `run_integrated`（event 45157、2026-09-29T17:52:27.312Z）は commit `49add659cebea4607cec6d6db7000036ceb1021e`。この後の goal review の event は開始 2 件・完了 2 件・失敗 0 件だった（読み取り時の cursor 49989）。

| goal ID / job ID（`goal_review_id`） | 開始 → 完了（UTC）/ event ID | `session_id`（完了時） | 判定 / 時間 |
| --- | --- | --- | --- |
| 67 / 16（attempt 1） | 2026-09-29T22:35:00.800Z → 22:36:07.364Z / 46663 → 46691 | `01a0ef4e-a0e3-7080-9136-997c701080d7` | `verdict: achieved`・`decision: achieved` / 66 秒 |
| 69 / 17（attempt 1） | 2026-09-30T00:33:00.230Z → 00:34:40.532Z / 48090 → 48142 | `01a0efba-a6f8-7021-9c9b-7fa3876966b5` | `verdict: achieved`・`decision: achieved` / 100 秒 |

file は Python の `pathlib.Path.read_text()` と `json.loads()` で JSONL を行ごとに読み、`type` と `item.type` ごとに数え、`command_execution` の `command`・`exit_code` と stderr 全文を確認した。対象は `<queue>/goal-reviews/{16,17}/review.out`・`review.err` と、`~/.codex/sessions` から thread ID で探した次の 2 file:

- `2026/09/30/rollout-2026-09-30T07-35-02-01a0ef4e-a0e3-7080-9136-997c701080d7.jsonl`
- `2026/09/30/rollout-2026-09-30T09-33-02-01a0efba-a6f8-7021-9c9b-7fa3876966b5.jsonl`

手順 5 の確認点ごとの結果:

| 確認点 | 観察（上記のコマンド・file の値） | 結論 |
| --- | --- | --- |
| launch・thread・decision | 開始 2 件とも `launch: {role: goal_review, provider: codex, model: null, effort: medium, source: dagq.toml}`、`session_id: null`。完了 2 件とも上表の thread ID、`model: gpt-6-astra`、`model_unknown` 無し、`overridden: null` | Codex で起動して実モデルと判定を記録できた。本番は effort を省略しているため `medium` であり、手順 2 の `low` は試していない |
| stdout の JSONL と最終の返答 | job 16 は 17 行（`thread.started` 1、`turn.started` 1、`item.started` 6、`item.completed` 8、`turn.completed` 1）。job 17 は 24 行（同じ順に 1・1・9・12・1）。それぞれ最初の thread ID は完了 event と一致し、最後は `turn.completed` | 実 Codex の JSONL を読み、runtime が両方の verdict を `achieved` として記録できた |
| sandbox 内の dagq の読み取り・書き込みの拒否 | 完了した `command_execution` は job 16 が 6 件、17 が 9 件で全て `exit_code: 0`。16 は `~/.local/bin/dagq list`・`goal show 67 --full`・`show 972`・`show 973`・`show 974`、17 は `list`・`goal show 69 --full`・`show 1105` と `kpi/add/edit --help` を含む。他は `cat`・`sed`・`rg`・`git show/log` などの読み取り。書き込みのコマンドは無い | dagq の読み取りは成功。書き込みを sandbox が拒むことは未確認（試行が無い）。`add/edit --help` は help を読むだけで task の登録・編集ではない |
| stderr と bypass の警告 | 2 件の `review.err` はどちらも `Reading additional input from stdin...` の 1 行だけ。`--dangerously` などの bypass の警告は無い | 警告は観察されなかった。ただし警告が無いことだけで起動の全引数を証明することはできない |
| `ps` の起動の引数 | 終了済みの job のため当時の `ps` は無い。代わりに両 rollout の `turn_context` が `sandbox_policy: {type: read-only}`・`approval_policy: never`・`effort: medium`、`cwd` が本番 main checkout を持つ。JSONL の形は上記のとおり | read-only の実効設定は記録で確認できた。`--json --sandbox read-only -C ...` と bypass flag の不在を argv そのもので確かめる点は本番の記録では見られない |
| rollout の model | 両 rollout の `turn_context.payload.model` は `gpt-6-astra` | それぞれの `goal_review_finished.model` と一致 |
| stats の provider / model | `jobs.goal_review.by_provider.codex` と `by_model.gpt-6-astra` はともに `count: 2`・`failed: 0`・`failed_rate: 0.0`・`secs: {count: 2, median: 83, total: 166}`・`verdicts: {achieved: 2}` | 2 件の時間（66 + 100 秒）・判定と一致。手順の 1 件の代わりに本番の 2 件を数えた |
| `~/.codex/config.toml` の更新時刻 | 対象 job の起動前と終了後の時刻を控えた記録は無い | 本番の記録では見られない。現在の時刻だけでは当時変わらなかったと判定できない |

同じ `stats --full` の `jobs.goal_review.by_provider.claude` は `count: 10`・`failed: 0`・`failed_rate: 0.0`・`secs: {count: 10, median: 23, total: 249}`・`verdicts: {achieved: 9, ask: 1}`。`by_model.unknown` も同じ値だった。provider 別に並べて読めることは確認できたが、goal の内容と件数が異なり、Claude の実モデルも unknown なので、83 秒と 23 秒を provider の性能差とは判断しない。この stats は `--since` を付けない集計であり、上記の event の期間限定とは区別する。

本番の記録では見られない点と、次に見るために必要なこと:

- 手順 6 の切り替え: 上記期間の開始 2 件の launch に `switched_from`・`switch_reason` は無く、失敗 event も無い。`executable_missing` による Claude への切り替えは未確認。人か inbox が使い捨て queue で手順 6 を行い、切り替え先の launch と完了を採取する必要がある。本番で実行ファイルを壊して試さない。
- 書き込みの拒否: 人か inbox が使い捨て queue の read-only job に無害な file の書き込みを試させ、拒否の出力を採取する必要がある。今回の成功した読み取りだけでは拒否の挙動は分からない。
- argv と config の前後比較: 人か inbox が使い捨て queue の job の実行中に `ps` で引数を採取し、`~/.codex/config.toml` の更新時刻を起動前・終了後に控える必要がある。rollout の read-only 設定は argv の全体や config の不変性の代わりにはならない。
- 手順 7 の使い捨て queue の後始末は今回は対象外（作成していない）。次に手作業のスモークを行った場合は手順どおりに行う。

### 2026-10-05: task 1174 後の config 更新時刻の照合（task 1205）

[非対話の worker の結果（task 1205）](#結果2026-10-05task-1174-後の本番の-codex-の記録からtask-1205)で、更新後の本番の Codex goal review 48 件を含む job の開始を config の mtime（2026-10-02T15:11:51Z）と照合した。同じ秒に始まった goal review は無く、最も近い job 24（goal 58、codex-cli 0.159.2）は 09:39:40.609Z → 09:44:28.945Z に終了している。mtime 後に始まった 41 件の goal review は測定時点まで config の更新時刻を進めていない。job 24 の rollout も read-only・never を持つ。task 1114 の job 16・17 の起動前後の snapshot が無いという制限は残る。今回も各 job の前後の中身は採取しておらず、main checkout の既存の trust の追加時期は分からない。観察のコマンド・job ID・時刻・現在の `[projects]` 一覧と結論はリンク先にある。

## Codex の run review のスモーク

### 結果（2026-10-01）

別 queue で実 Codex 0.159.2 の worker と review を実行し、task 2 の review が `pass`、run `ca586e69-4833-43c2-a269-f7d90fa2152f` が `908060719bbc3f599cbb061fe1e130827472595e` として main に着地・push した。最初の task 1 は実行側のホスト権限制約で Codex review の app-server 初期化が拒まれ、`review_failed` と ask になった。権限付きでの再試験は成功し、試験用の cmux group は削除した。
