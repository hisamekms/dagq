---
id: plan-codex-headless-jobs-spike
type: plan
title: スパイク：goal review job を Codex（codex exec）の読み取りだけの sandbox で動かせるか
status: completed
created: 2026-09-29
updated: 2026-09-30
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - provider
related:
  - plan-headless-worker-spike
  - adr-t813-2
  - adr-t813-3
  - adr-0073
---

# スパイク：goal review job を Codex（codex exec）の読み取りだけの sandbox で動かせるか

goal 73（worker 以外の headless の job を provider に透過な形で Codex でも動かし、まず goal review job を Codex に乗せる）の最初の task（task 1061）。設計の前提を実 CLI と本番 queue で確かめる。前例は goal 57 の spike（[headless-worker-spike](headless-worker-spike.md)、以下「前回の spike」）で、同じ版の codex で測った worker の結果はここで繰り返さず参照する。この task は測って書くだけで `src/` は変えていない。

## 環境と対象

- 日付: 2026-09-29（JST）。host は macOS 14（arm64、Darwin 23.6.0）
- codex-cli 0.155.1（`~/.local/bin/codex` → `~/.codex/packages/standalone/current/bin/codex`、standalone の実体）。model は指定せず、codex の既定の `gpt-6-astra`（provider `openai`）で動いた。cmux の terminal では `which codex` が cmux の shim（`$TMPDIR/cmux-cli-shims/<surface>/codex`）に解決するので、前回の spike と同じく実体の path で起動した（runtime の `codex::executable_on` が shim を避けるのと同じ）
- dagq: 固定バイナリ `~/.local/bin/dagq`、build 識別子 `0.4.0-dev+abe81186cdb8980cce1ed6e66020f456fb851f2c`。sandbox の中の dagq も PATH からこれに解決した（下の 2.）
- queue: この repository の本番 queue（`~/.local/share/dagq/77067154921b9014/queue.db`、WAL。supervisor が動いていて `-shm`・`-wal` がある）。使い捨ての queue は作っていない（worker の role は `dagq init` を拒む。ask 195 の案 B）
- 対象の goal: **goal 47**（「tests/*.rs を 3,000 行以下に分ける」、task 490〜493・531 が completed、875 が canceled、2026-09-28 に Claude の goal review で `achieved` に close 済み）。prompt は、runtime がその goal review（goal review 14、`goal-reviews/14/prompt.txt`、30,270 byte）のときに `goal_review_prompt` で組み立てて保存したものをそのまま使った（`dagq` に goal review の `--dry-run` は無い。goal 47 は close 後に task が増えていないので、今組み立て直しても中身は同じ）。codex の verdict は読むだけで、queue には適用していない
- codex の起動の cwd と `-C` は main checkout（`~/ghq/github.com/hisamekms/dagq`。runtime の goal review job の cwd と同じ）。起動の env は worker の session のもの（`DAGQ_ROLE=worker`・`DAGQ_QUEUE=<本番 queue.db>`・`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`）で、`DAGQ_ROLE` は書き換えていない。試験用の変数は `DAGQ_SPIKE_PROBE=1` などを足した
- probe: scratchpad の `probe.sh`（env と PATH を出し、`~/.local/bin/dagq` の `--version`・`show 1034`・`goal show 47 --full`・`events --goal 47 --full`・`search "test file lines"`・`related 1034`・`findings`・`status` を順に打って exit code と出力の byte 数を出す）。状態を変えるコマンドは入れていない。codex には「このコマンドを 1 回だけ実行して出力をそのまま返せ」と頼んで sandbox の中で走らせた

## 1. read-only の sandbox の中で dagq の読み取りが通るか

### 打ったもの

```sh
cd <scratchpad> && DAGQ_SPIKE_PROBE=1 ~/.local/bin/codex exec --json -c model_reasoning_effort='"low"' \
  --sandbox read-only -C <main checkout> -o r1.last \
  "Run exactly this one shell command, once, and wait for it to finish: sh <scratchpad>/probe.sh 47 . Then reply with its full output verbatim and nothing else." < /dev/null > r1.jsonl 2> r1.err
```

同じ probe を sandbox の外（同じ env）でも打ち、byte 数を比べた。

### 結果: 通った。追加の設定は要らない

| コマンド | sandbox の外 | `--sandbox read-only` の中 |
| --- | --- | --- |
| `dagq --version` | rc=0 | rc=0 |
| `dagq show 1034` | rc=0、7,339 byte | rc=0、7,339 byte |
| `dagq goal show 47 --full` | rc=0、11,396 byte | rc=0、11,396 byte |
| `dagq events --goal 47 --full` | rc=0、1,097 byte | rc=0、1,097 byte |
| `dagq search "test file lines"` | rc=0、8,319 byte | rc=0、8,319 byte |
| `dagq related 1034` | rc=0、11,838 byte | rc=0、11,838 byte |
| `dagq findings` | rc=0、52,383 byte | rc=0、52,383 byte |
| `dagq status` | rc=0、7,913 byte | rc=0、7,913 byte |

- `writable_roots` も `-c` も足さずに、全部が sandbox の外と同じ出力になった。macOS の read-only の sandbox（seatbelt）はファイルの読み取りを全域で許し、書き込みだけを拒む
- queue の SQLite は、状態を変えないコマンドが `SQLITE_OPEN_READ_ONLY` で開く（ADR-0073 決定 5・7・18、`src/infrastructure/sqlite.rs` の `open_read_only`）。WAL の `-shm` は書けないが、supervisor が DB を開いていて `-shm` と `-wal` があるので、SQLite は `-shm` を読み取りだけで使って読めた（拒否もエラーの文言も出なかった）。**`-shm` が無いとき（どの process も DB を開いていないとき）の read-only の open は測っていない**。goal review job は supervisor の子で、supervisor が DB を開いている間だけ動くので、実際の運用ではこの場合は起きない見込み
- 書き込みが本当に拒まれることは別に確かめた: sandbox の中の `touch <scratchpad>/write-probe` と `touch $TMPDIR/…` はどちらも `Operation not permitted`（rc=1）で、ファイルはできなかった。本番 queue の dir や DB への書き込みは試していない（状態を変えないという制約のため）。状態を変える dagq のコマンドは、sandbox の中では DB を書き込みで開けずに失敗する見込みで、加えて CLI の role の policy（goal review job の role は状態を変えるコマンドを持たない）が拒む
- read-only の sandbox で困ったこと（goal review の実行で観測）:
  - `/tmp` と `$TMPDIR` も書けない。zsh の heredoc（`cat <<EOF`）が `zsh:6: can't create temp file for here document: operation not permitted` で落ちた（model は `python3 -c` に切り替えて続けた）
  - `/usr/bin/git`（xcrun の shim）が `git: error: couldn't create cache file '/tmp/xcrun_db-…' (errno=Operation not permitted)` を毎回 stderr に出す。git の読み取り（`git log`・`git show`）自体は通った
  - どちらも verdict には響かなかった。気になるなら ADR で `workspace-write` ＋ 狭い `writable_roots` を検討できるが、goal review の目的（書かせない）には read-only が素直

### 推奨

- `codex exec --sandbox read-only -C <main checkout>`（exec の `resume` を使わない 1 回きりの job なので `-c sandbox_mode=…` は要らない）。`writable_roots`・`network_access`・`--add-dir` は付けない。承認は exec の既定（`approval_policy` never。rollout の `turn_context` で確認）
- `--dangerously-bypass-approvals-and-sandbox`・`--approve-for-me`・`--dangerously-bypass-hook-trust` は付けない（前回の spike の 1. と 6.）
- queue への書き込みの経路は要らない（goal review の verdict は stdout で返し、runtime が適用する）

## 2. env が sandbox の中の shell に渡るか

### 打ったもの

```sh
# 既定の shell_environment_policy
DAGQ_SPIKE_PROBE=1 DAGQ_SPIKE_TOKEN=1 DAGQ_SPIKE_KEY=1 RUSTC_WRAPPER=sccache ~/.local/bin/codex exec --json … --sandbox read-only … "Run … sh probe2.sh …"
# 絞る設定と足す設定
DAGQ_SPIKE_PROBE=1 ~/.local/bin/codex exec … -c shell_environment_policy.inherit=core -c 'shell_environment_policy.set={DAGQ_SPIKE_SET="1"}' --sandbox read-only …
DAGQ_SPIKE_PROBE=1 ~/.local/bin/codex exec … -c 'shell_environment_policy.inherit="none"' -c 'shell_environment_policy.exclude=["DAGQ_SPIKE_*"]' --sandbox read-only …
```

### 結果

- 既定では、codex を起動した process の env がそのまま渡った: `DAGQ_ROLE=worker`・`DAGQ_QUEUE`・`DAGQ_ACTOR_ID`・`DAGQ_SPIKE_PROBE=1`・`RUSTC_WRAPPER`、名前に `TOKEN`・`KEY` を含む `DAGQ_SPIKE_TOKEN`・`DAGQ_SPIKE_KEY` も渡った（名前による既定の除外は効いていなかった）
- コマンドは `/bin/zsh -lc '<command>'`（login shell）で走る。PATH は zsh の profile で組み直され、先頭に codex 自身の dir（`~/.codex/packages/standalone/releases/0.155.1-aarch64-apple-darwin/codex-path`、`rg` がある）と `~/.codex/tmp/arg0/…` が入るが、`~/.local/bin` も含まれ、`command -v dagq` は `~/.local/bin/dagq` に解決した。`HOME` と `TMPDIR` もそのまま
- `-c 'shell_environment_policy.set={DAGQ_SPIKE_SET="1"}'` で足した変数は渡った
- `-c shell_environment_policy.inherit=core`、`inherit="none"`、`exclude=["DAGQ_SPIKE_*"]` は、どれも `DAGQ_SPIKE_PROBE` と `DAGQ_ROLE` を落とさなかった（0.155.1 の exec の shell で、絞る設定は効かなかった。設定の名前の誤りか仕様かは切り分けていない）

### 推奨

- runtime は今の Claude の job と同じく、codex の process の env に `actor_env`（`DAGQ_ROLE`（goal review job の role）・`DAGQ_QUEUE`・`DAGQ_ACTOR_ID`）を置けばよい。sandbox の中の `dagq` はそれを継ぎ、queue の解決も role の判定も Claude の job と同じになる。`-c shell_environment_policy.*` は要らない
- 逆に、起動した側の env は全部 model のコマンドに見える（secret の名前でも落ちない）。supervisor の env に secret を置かない前提は Claude の job と同じ。絞りたいなら env を codex の起動の時点で絞る（`CommandSpec` の env を明示する）。`shell_environment_policy` の絞る設定には頼らない

## 3. 最終の返答（verdict）・thread の id・model の取り出し方

### 打ったもの

上の 1. の `--json -o` の実行に加え、goal 47 の goal review の prompt を 2 回渡した（5. と同じ実行）。

```sh
# A: --json と -o
~/.local/bin/codex exec --json --sandbox read-only -C <main checkout> -o gr1.last "$(cat goal-reviews/14/prompt.txt)" < /dev/null > gr1.jsonl 2> gr1.err
# B: --json なし（stdout は最後のメッセージだけ）
~/.local/bin/codex exec --sandbox read-only -C <main checkout> -- "$(cat goal-reviews/14/prompt.txt)" < /dev/null > gr2.out 2> gr2.err
```

### 結果

- `--json` の JSONL は `thread.started`（`thread_id`）→ `turn.started` → `item.started` / `item.completed`（`agent_message` の `text`、`command_execution` の `command`・`exit_code`・`aggregated_output`）→ `turn.completed`（`usage`: `input_tokens`・`cached_input_tokens`・`cache_write_input_tokens`・`output_tokens`・`reasoning_output_tokens`）。前回の spike と同じ形
- 最終の返答: 最後の `item.completed` の `agent_message` の `text`。`-o <file>`（`--output-last-message`）はそれと同じ text を書く（末尾の改行だけ違う）。`-o` は正常終了のときだけ書かれ、SIGTERM / SIGINT で止めたときは書かれなかった（4.）
- `--json` を付けないと、**stdout には最後のメッセージ（verdict の JSON）だけ**が出て、進行（header・実行したコマンドとその出力・`tokens used`）は stderr に出た。B の stdout はそのまま `GoalReviewVerdict::parse`（`parse_json_object`）の読める 1 つの JSON object だった。`--json` の stdout を今の `review.out` にすると、JSONL の全体の最初の `{` から最後の `}` までが 1 つの JSON にならず parse に失敗する
- thread の id: `--json` では `thread.started.thread_id`（例 `01a0eabf-bdf7-7af2-9406-294fc686d5a4`）。`--json` なしでは stderr の header の `session id: <id>` の行（同じ値の種類）
- 実際の model: **`--json` の stdout には出ない**（JSONL に `model` の欄が無い）。出るのは (a) `--json` なしの stderr の header の `model: gpt-6-astra`・`provider: openai`・`reasoning effort: …`・`sandbox: read-only`・`approval: never`、(b) codex が書く rollout（`~/.codex/sessions/YYYY/MM/DD/rollout-<時刻>-<thread_id>.jsonl`）の `turn_context` の `payload.model`・`payload.effort`・`payload.sandbox_policy`・`payload.approval_policy`（`session_meta` は `cli_version: 0.155.1`・`originator: codex_exec`・`model_provider: openai`）。`--ephemeral` を付けると rollout は書かれない
- JSONL の `command_execution` の `aggregated_output` は長い出力の末尾の約 2KB だけだった（probe の 3KB 超の出力の先頭が落ちていた。model の返答には先頭が含まれていた）。コマンドの出力の記録には使えない

### 推奨

- 起動は `codex exec --json … -o <job dir>/<last message file>`。runtime（provider の実装）が
  - verdict の text は `-o` のファイル（無ければ JSONL の最後の `agent_message`）から取り、job には「最終の返答の text」として渡す。今の `review.out` をそのまま読む形にしたいなら、provider が取り出した text を `review.out` に書く
  - thread の id は `thread.started.thread_id`、usage は `turn.completed.usage`、失敗は既存の `CodexTurnReader` / `codex_turns::classify` で読む
  - 実際の model は rollout の `turn_context.payload.model` で読む（rollout の path は thread の id で `~/.codex/sessions` の下から探す。`--ephemeral` は付けない）。読めなければ要求した model（`-m` を付けたならその値、付けないなら不明）を記録する
- より簡単な代案: `--json` を付けず、stdout を今の `review.out`（verdict がそのまま読める）、stderr を `review.err` にして、thread の id と model を stderr の header（`session id:`・`model:`）から読む。ただし header は人向けの表示で、版で変わりうる。失敗の分類も人向けの文言からになる。後続の ADR で A（JSONL）と B（header）のどちらにするかを決める。この spike の推奨は、worker の `CodexTurnReader` を使い回せる A
- `-c model_reasoning_effort=…` は明示する（B の実行では header が `reasoning effort: none`、A では rollout の `effort: null` で、人の config と codex の既定に任された）

## 4. 失敗の出力

### 打ったもの

```sh
# 時間の上限: foreground の長いコマンドの途中で codex に SIGTERM / 自分の process group ごと SIGTERM / SIGINT
~/.local/bin/codex exec --json … --sandbox read-only -C <main checkout> -o k1.last "Run this shell command once in the foreground and wait for it: perl -e 'sleep 241' ; then reply DONE." & kill -TERM $!
perl -e 'setpgrp(0,0); exec @ARGV' ~/.local/bin/codex exec --json … "…sleep 239…" & kill -TERM -- -$!
~/.local/bin/codex exec --json … -o k3.last "…sleep 237…" & kill -INT $!
```

### 結果

- **codex が無い**: 再現はしなかった（host の codex を消さないため）。runtime 側では `codex::executable_on` が PATH に無ければ `<name> was not found on PATH (outside cmux's shims)`、絶対 path が無ければ `resolve executable <path>` の文脈つきの `No such file or directory (os error 2)` で起動の前に失敗し、`AgentProvider::preflight` は `codex --version` の失敗を返す。起動した後の失敗ではないので、JSONL も stderr も無い
- **未ログイン**: 再現しなかった。再現するには空の `CODEX_HOME` で起動する必要があり、この task では CODEX_HOME を変えないため。同じ codex-cli 0.155.1 の前回の spike の 3. の測定（空の `CODEX_HOME`: exit 1、約 17 秒、`error` の `Reconnecting... n/5 (unexpected status 401 Unauthorized: Missing bearer or basic authentication in header …)` の後に `turn.failed`。無効な API key: `… auth error code: invalid_api_key`）がそのまま当たる見込みで、`codex_turns::classify` がどちらも `Authentication` に分類する。sandbox の mode はこの経路に関わらない（API の呼び出しは codex の process が sandbox の外で行う）
- **利用上限**: 再現しなかった（前回の spike と同じ）。`classify` が `usage limit`・`429` などを `UsageLimit` に分類する
- **時間の上限**: codex exec に turn の時間の上限の flag は無い（前回の spike の 4.）ので、起動する側の上限で止める。止め方で結果が違った:

| 止め方 | codex の exit | 走っていたコマンド | JSONL | `-o` |
| --- | --- | --- | --- | --- |
| codex の pid に SIGTERM | 143 | **残った**（親が 1 になった。pid で止めた） | `item.started` で終わり、`turn.completed` / `turn.failed` は出ない | 書かれない |
| codex を自分の process group で起動し、group ごと SIGTERM | 143 | **残った**。コマンドは codex と別の process group（pgid がコマンド自身の pid）で走っていて、group への signal が届かない | 同上 | 書かれない |
| codex の pid に SIGINT | 1（すぐ） | 止まった（残らない） | 同上。stderr に `ERROR codex_core::session: failed to record rollout items: thread … not found` | 書かれない |

- runtime の今の headless job の時間の上限（`supervise/jobs.rs` の `HeadlessJob::poll` → `stop`）は、先に子孫の pid を集め、job の process を kill し、集めた子孫を pid で止める。codex のコマンドの親は codex の process なので、この止め方なら残らない見込み（group には頼っていない）。失敗の文言は provider によらず `the headless goal review did not finish within N seconds`

### 推奨

- 起動の前の失敗（実行ファイルが無い）は今の `executable_on` と `preflight` の文言で、起動の後の失敗（認証・利用上限・model）は JSONL の `error` / `turn.failed` の `message` を `codex_turns::classify` で共通の分類（`TurnFailure`）に訳す。goal review job の `job_wall`（task 438 の壁の判定）は今 Claude の出力の文言を読むので、Codex の job では provider の実装が `classify` の結果を壁に訳す
- 時間の上限で止めるときは、今の `HeadlessJob::stop` の「子孫を pid で止める」を Codex でも使う。止める signal を選べるなら SIGINT が codex 自身にコマンドを片付けさせる
- 前回の spike と `codex.rs` の `turn_command` の doc（「group を一緒に止めないとコマンドが残る」）は、group ごと止めれば片付くという前提だが、この測定では group への SIGTERM でもコマンドが残った。worker の経路（`headless_session.rs` の `kill_group`）は follow-up で確かめる。→ task 1085 で直した: 非対話の worker の turn を止めるとき（時間切れ・無音・exit の要求・wrapper のエラーなど）は、`stop_turn` が先に turn の子孫を pid で集め、turn の process group と合わせて SIGKILL で止める（group だけに頼らない）。今の姿は [非対話の worker の「wrapper が turn を止めるとき」](../design/supervisor-lifecycle/headless-worker.md#wrapperがturnを止めるとき)

## 5. prompt の Claude の plugin への依存と、Codex で verdict が返るか

### 依存

- Claude の goal review job は `claude -p --allowedTools Read Grep Glob 'Bash(dagq:*)' -- <prompt>` で、`--plugin-dir` を付けない（`adapters.rs` の `headless_command`）。prompt（`prompt.rs` の `goal_review_prompt`）は skill を名指さず、使う dagq のコマンド（`dagq show ID`・`dagq goal show <goal> --full`・`dagq findings`・`dagq events --goal <goal> --full`・`dagq search ...`）を本文に書いている。skill・hook・slash command・Claude Code の道具の名前への依存は無い。Codex で足りないものは無かった
- 違い: Claude は許された道具（`Read`・`Grep`・`Glob`・`Bash(dagq:*)`）しか使えないが、Codex は sandbox の中で任意のコマンド（`cat`・`rg`・`python3`・`git log` / `git show`）を打つ。読み取りだけという意図は同じで、権限を道具名でなく sandbox で表すことになる（goal の「透過にするもの」）
- どちらも cwd の repository の指示を読む: Claude は `CLAUDE.md`（`@AGENTS.md`）、Codex は cwd の `AGENTS.md`（rollout の `world_state.agents_md`）。Codex は AGENTS.md の「セッション開始時に読む」に従って `dagq list` と `cat docs/plans/current.md` も打った（読み取りなので害は無い）。人の `~/.codex/config.toml`（`personality = "pragmatic"` など）も読まれる。`~/.codex/AGENTS.md` は無い

### verdict

| | Claude（本番の goal review 14） | Codex A（`--json -o`、effort 未指定） | Codex B（`--json` なし、effort 未指定） |
| --- | --- | --- | --- |
| verdict | `achieved`（criteria 3 件、全て met） | `achieved`（criteria 3 件、全て met、gaps 空） | `achieved`（criteria 3 件、全て met） |
| 所要 | 37 秒（`goal_review_started` 22:09:44 → `goal_review_finished` 22:10:21） | 73.5 秒 | 55 秒 |
| usage | – | input 203,701（cached 169,344）、output 1,976、reasoning 16 | `tokens used 36,473`（stderr） |
| 言語 | 英語 | 日本語 | 日本語 |
| thread | – | `01a0eabf-bdf7-7af2-9406-294fc686d5a4` | `01a0eac3-3d99-7f83-98cb-07ce5a89eb1c` |

- Codex の 2 回とも、verdict は runtime の schema（`GoalReviewVerdict`、`deny_unknown_fields`）の欄だけの 1 つの JSON object で、知らない欄は無かった（`question`・`options`・`reason_category` は省かれたが `#[serde(default)]`）。evidence は commit（`73558732`・`ee6984bc`・`dd6fac6e`）・receipt・`scripts/check-test-file-lines.sh`・`.github/workflows/ci.yml:58` を挙げ、今の main の test ファイルの行数を自分で数えていた
- 出来の比較はこの task ではしていない（goal の acceptance (6) で本番の記録を並べる）

## まとめ（後続の ADR と実装への入力）

- 最小の設定: `codex exec --json --sandbox read-only -C <main checkout> -c model_reasoning_effort="<effort>" [-m <model>] -o <job dir>/<file> -- <prompt>`、stdin は `/dev/null`、env は今の Claude の job と同じ `actor_env`。`writable_roots`・network・`shell_environment_policy`・bypass の flag・hook は要らない。これで goal review が使う dagq の読み取りは全部通り、書き込みは sandbox が拒む
- queue の DB は read-only の sandbox から ADR-0073 の読み取り専用の open で読めた（supervisor が DB を開いて `-shm` がある状態で）。`-shm` が無い状態は測っていない
- 出力: verdict の text は `-o` のファイルか最後の `agent_message`、thread の id は `thread.started`、usage は `turn.completed`、実際の model は rollout の `turn_context.payload.model`（JSONL には無い）。`--json` の stdout を今の `review.out` にしてはいけない（parse できない）
- 失敗: 起動の前は `executable_on` / `preflight`、起動の後は `codex_turns::classify` を共通の分類に使える。時間の上限は runtime の `HeadlessJob::stop`（子孫を pid で止める）で片付く見込み（codex の job では未測定）。SIGTERM と group への SIGTERM ではコマンドが残り、SIGINT では残らなかった。未ログイン・利用上限・codex が無い場合は再現していない
- prompt は Claude の plugin に頼っておらず、同じ prompt で Codex が schema どおりの verdict を返した

## 後片付け

- 使い捨ての queue、cmux の workspace と workspace group は作っていない
- probe の script と出力は run の scratchpad だけに置いた。自分が起動した process（SIGTERM の後に残った `perl -e 'sleep 241'`・`sleep 239`）は pid で止め、残っていないことを確かめた
- 本番 queue に打ったのは状態を変えないコマンドだけ（`show`・`goal show`・`events`・`search`・`related`・`findings`・`status`・`goal list`）。codex の verdict は適用していない（goal review・goal close は打っていない）。`DAGQ_ROLE` は書き換えていない
- codex が自分で書くもの: rollout（`~/.codex/sessions/2026/09/29/` の各 thread の JSONL）は通常の記録として残した。`~/.codex/config.toml`・`auth.json`・`CODEX_HOME`・`~/.claude` の設定・`~/.local/bin/dagq` は変えていない
