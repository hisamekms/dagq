---
id: plan-codex-inbox-spike
type: plan
title: スパイク：Codex CLI の対話の session で inbox に要るもの（常駐・background の見張り・answer・指示の読み込み・復帰・区間の記録・画面の判定・sandbox からの状態を変える dagq）ができるか
status: completed
created: 2026-10-24
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - provider
  - inbox
related:
  - adr-t2159-1
  - adr-t1063-1
  - adr-t1233-5
  - adr-t813-3
  - adr-t728-3
  - adr-t1394-1
  - adr-t1433-5
  - adr-t3102-1
  - adr-t3102-2
  - adr-t3105-1
  - plan-codex-headless-jobs-spike
  - development-manual-smoke
  - design-provider-lifecycle
  - design-queue-service
---

# スパイク：Codex CLI の対話の session で inbox に要るものができるか

goal 77（inbox を Codex CLI の対話の session でも動かす）の測定の task（task 1180）。
inbox に要る 8 項目を、queue も supervisor も使わずに scratch の使い捨て repository で実 Codex の対話の TUI を動かして確かめた。
結論は ADR（task 1181）の材料にする。
この task は測って書くだけで、`src/` と `docs/design/` は変えていない。

前提は [ADR-t2159-1](../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)。
dagq は cmux を呼ばず、inbox は人が自分の terminal で `dagq inbox` を打って開く対話の session で、`up` は inbox を開かない（決定 1〜3）。
supervisor は inbox の画面を読まない（[ADR-t1433-5](../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md) 決定 1）。
Codex の inbox は guardrail と同じ趣旨（dagq の CLI を通す）を Codex の手段で持つ（決定 5）。
この測定と下の手順は cmux を使わない。

## 環境と対象

- 日付: 2026-10-24 05:36〜05:53（JST）。host は macOS 14（arm64、Darwin 23.6.0）
- codex-cli **0.160.0**（`~/.local/bin/codex` → `~/.codex/packages/standalone/releases/0.160.0-aarch64-apple-darwin/bin/codex`）。
  task の登録の時の 0.155.1 から standalone の自動更新で上がっていた。
  起動のたびに 0.164.0 への更新の画面が出た（下の 6）。
  PATH の先頭の `codex` は mise の 0.162.1 だが、dagq の runtime が使う実体と同じ `~/.local/bin/codex` で測った
- model は指定せず、人の `~/.codex/config.toml` の既定の `gpt-6.1-sol`（provider `openai`）。
  時間を抑えるため `-c model_reasoning_effort="low"` を渡した。
  `codex login status` は `Logged in using ChatGPT`
- scratch: `$TMPDIR/dagq-worker-t1180-uS1woi/repo`（`git init -b main`、`seed.txt`・`AGENTS.md`・`.agents/skills/probe-skill/SKILL.md` の 1 commit）。
  項目 7 の queue の dir の代わりに、sandbox の書き込みの範囲の外の `~/.local/share/dagq-worker-t1180-q-VUpI7h/` を作り、python で listen する UNIX socket `svc.sock` を置いた
- pty: tmux 3.6a（`~/.local/share/mise/installs/tmux/3.6a/bin/tmux`）を専用の socket（`tmux -L dagq-t1180 -f /dev/null`）の detached の session で動かし、`send-keys -l` で打ち込み、`capture-pane -p` で画面を読んだ。
  tmux は測定のための道具で、dagq の仕組みではない
- env: worker の session のもの（`DAGQ_ROLE=worker` など）をそのまま継いだ。
  `DAGQ_ROLE` は外していない。
  `dagq watch` も、本番の queue に対して状態を変える dagq のコマンドも打っていない
- 起動の形（`launch.sh`。各回で足した引数は項目ごとに書く）:

  ```sh
  cd <scratch>/repo
  exec ~/.local/bin/codex --no-daemon \
    -c 'projects={"<scratch>/repo"={trust_level="trusted"}}' \
    -c 'model_reasoning_effort="low"' \
    -c 'notify=["sh","<scratch>/notify.sh"]' \
    -c 'hooks.SessionStart=[{hooks=[{type="command",command="sh <scratch>/hook.sh SessionStart"}]}]' \
    ...（UserPromptSubmit・Stop・SessionEnd・PreCompact・PostCompact も同じ形）
  ```

  `hook.sh` は stdin の JSON を `hooks.log` に足し、`HOOK-CONTEXT-<event>: ...` の 1 行を stdout に出す。
  `notify.sh` は引数の JSON を `notify.log` に足す。
- `--no-daemon`: 0.160.0 の TUI は既定で、共有の app-server の daemon（`codex app-server daemon`）に繋ぐ。
  この host では人の Codex が起動した 0.164.0 の daemon が動いていた。
  daemon に繋ぐと、command は TUI の env ではなく daemon の env で動く見込みで、`-c` の trust が daemon に渡るかも分からない（渡らなければ daemon が project の trust を `~/.codex/config.toml` に書くおそれがある。task 1174 の原因）。
  人の daemon と設定に触れないため、全部の回で `--no-daemon` を付けた。
  daemon の経路は測っていない

### task 1174 の結果と `~/.codex/config.toml`

task 1174 は、`codex exec` が `~/.codex/config.toml` を書き直す原因を、thread の開始で cwd の project の trust が無いと main checkout を trusted と書く処理とした。
止める手段として `-c projects={"<worktree>"={trust_level="trusted"}}` を見つけた（[provider-lifecycle](../design/provider-lifecycle.md#codexの非対話のworker)）。
この測定の TUI の起動にも、scratch の repository の trust を同じ形で必ず渡した。
`tui.model_availability_nux` は TUI の起動の時の案内（startup の tooltip）が書く欄で、task 1174 はこれを止める引数を見つけていない（exec と resume は TUI を通らないので書かない、という結論）。

| | 更新時刻（mtime） | sha256 |
| --- | --- | --- |
| 測定の前（05:34） | 2026-10-03 00:11:51（1790953911） | `5d764192d9c005b1984f043a9c7471f147481224523c646ae789feb3e9829f0b` |
| 測定の後（05:53、TUI を 5 回起動し、`codex queue`・`codex debug prompt-input`・`codex sandbox` を打った後） | 2026-10-03 00:11:51（1790953911） | 同じ |

- **変わらなかった**。
  `[tui.model_availability_nux]` には測定の前から `gpt-6-astra = 4`・`"gpt-6.1-sol" = 4` があり、TUI はこの欄を書き足さなかった。
  上限の回数に達しているためとみられるが、閾値は source で確かめていない。
  人の設定にこの欄が無い host で `dagq inbox` が Codex の TUI を起動すれば、Codex 自身がこの欄を書きうる。
- 空の `CODEX_HOME`（下の 8 の例外の測定）で起動した TUI は、その dir の `config.toml` に `[tui] screen_reader_detection_done = true` を書いた。
  人の `config.toml` には既にこの欄があるので、書かれなかった。
- 戻す必要は無く、receipt の follow_ups に人の作業は出していない。

### 残ったもの

Codex 自身の記録（`~/.codex/sessions/2026/10/24/rollout-*.jsonl` の scratch の thread、`session_index.jsonl`・`history.jsonl` の行、`~/.codex/*.sqlite`）は Codex の状態で、人の設定ではない。
これらは消していない（`~/.codex` の下を消すと人の Codex の状態を壊しうるため）。
`~/.codex/rules/default.rules` も更新時刻が変わっていない（承認は一度きりを選んだ）。

## 1. 常駐と background の見張り

**結論: 条件付き。**
Codex に、turn が終わった後に background の command の終わりで次の turn を始める仕組みは無い。
代わりの手段は 2 つあり、どちらも inbox の見張りに使える。

- (a) 前景の待ち: turn の中で待つ。
- (b) Codex の外からの `codex queue`: 人の terminal への打ち込みではなく、Codex の API で thread に次の turn を積む。

dagq の `watch --role inbox` そのものは worker が打てない（worker の role に `queue.watch` が無い。[Authorization](../design/authorization.md)）。
そこで、scratch の file に `ready` の行が現れるまで待つ loop を代わりの長く返らないコマンドにした。
`dagq watch --role inbox` を Codex の inbox で回す確認は、[手動スモーク](../development/manual-smoke.md)の「Codex の inbox のスモーク」に回した。

### 打ったものと画面

1. `&`・`nohup` で切り離す: 「turn の後も走り続けるように background で起動して turn を終えよ」と頼んだ。
   Codex は `nohup sh -c 'until grep -q ready …/signal; do sleep 1; done; cat …/signal' …` を打ち、`zsh:1: nice(5) failed: operation not permitted` を出した。
   Codex は自分で「次のメッセージで確かめられるが、turn の後に自動で知らせることはできない（automatic follow-up isn't available）」と答えた。
   直後の `ps` にその process は無く、出力の log には grep の 1 行だけが残っていた。
   command の process group は command の終わりで止められる（[provider-lifecycle](../design/provider-lifecycle.md#codexの非対話のworker) の、Codex が command ごとに process group を作るという観察と合う）。
2. Codex の background terminal（unified exec）: 「exec tool の長く走る background terminal として起動し、yield したら待たずに turn を終えよ」と頼んだ。
   画面:

   ```text
   • Running in exec session 29952. It yielded and is waiting for the signal file to appear and contain ready.
     Worked for 7s • 05:38
     1 background terminal running · /ps to view · /stop to close
   › Ask Codex to do anything
   ```

   tmux の別の窓から `echo ready-from-outside-… > signal` を書いた。
   30 秒後、`1 background terminal running` の行は消えたが、Codex は turn を始めなかった。
   次に人として「終わったか」と打つと、出力を読んで `ready-from-outside-1792787910` と答えた。
   background terminal は turn をまたいで走り、次の turn で読めるが、終わっても session は起きない。
3. 前景の待ち（a）: 「前景で走らせ、終わるまで待て」と頼み、待ちの間（20 秒後）に別の窓から signal を書いた。
   待ちの間の画面:

   ```text
   Waiting for background terminal (18s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close
   ```

   signal の後、同じ turn で `ready-second-1792787996` を報告した（`Worked for 22s`）。
   待ちの間に人が打った文は送信の待ちに入り、次の tool の呼び出しの後に渡った。

   ```text
   • Messages to be submitted after next tool call (press esc to interrupt and send immediately)
     ↳ person here: what is 2+3? answer while you keep waiting
   ```

   Codex は `2 + 3 = 5. I’m still waiting for the command to exit.` と答えて待ちを続け、signal の後に結果を報告した（`Worked for 52s`）。
   待ちの間も人と話せる。
   ただし turn が開いたままで、待ちの間の token と、model が待ちを続けるかは prompt 次第である。
4. 外からの `codex queue`（b）: 待ちの無い入力待ちの TUI の thread に、sandbox の外の shell から打った。

   ```sh
   codex queue --thread 01a16a7a-8e08-77d0-933c-250a2423e714 --message "queued from outside: reply QUEUE-OK"
   # Queued message 01a16a7f-d474-7703-a40b-beba4aabc261 for thread 01a16a7a-….
   ```

   入力待ちの TUI がその文で新しい turn を始め、`QUEUE-OK` と答えた（`--no-daemon` の TUI でも届いた）。
   これは端末への打ち込みではなく、Codex の thread への API の送信である。
5. sandbox の中からの `codex queue`: background terminal で「signal を待ってから `codex queue --thread <自分の thread>` を打つ」を走らせた。
   command の env に `CODEX_THREAD_ID=01a16a7a-…`（自分の thread）があった。
   signal の後、Codex は起きなかった。
   出力は次のとおりで、sandbox の中では `~/.codex` に書けず、`codex queue` が失敗した。

   ```text
   WARNING: proceeding, even though we could not create PATH aliases: Operation not permitted (os error 1)
   Error: failed to start embedded app server: Operation not permitted (os error 1)
   queue-rc=1
   ```

### 代わりの手段

- (a) 前景の待ち: inbox の turn が `watch --role inbox --until-attention` を前景で走らせ、返ったら扱い、また走らせる。
  人は待ちの間も打てる（次の tool の呼び出しの後に渡る）。
  Claude の inbox の「watch を background で回し、返りで起きる」に一番近い。
  watch は timeout が無いので、待ちの長さは Codex の tool の上限と model の粘りに左右される。
  そのため待ちの上限（例: `--timeout`）と、返ったら必ず打ち直すことを prompt と skill に書く必要がある。
  dagq の watch で長時間（時間単位）続くかは、手動スモークで確かめる。
- (b) Codex の外の見張りの process が watch を回し、返ったら `codex queue --thread <inbox の thread>` で inbox を起こす。
  `dagq inbox` が Codex を前面で exec する前に、子の process として起こしておく形になる。
  ADR-t1433-5 決定 2（runtime は inbox の terminal に打ち込まない）の「端末への打ち込み」には当たらない。
  ただし、dagq が inbox に話しかける経路を持つこと自体を決定 2 の趣旨に照らして許すかは、ADR の問いである。
  thread の id は SessionStart の hook の `session_id` か `notify` の `thread-id` で分かる（下の 5）。
  sandbox の中の inbox 自身は `codex queue` を打てない（上の 5）。
- 外から端末に打ち込む案（tmux の `send-keys` など）は、ADR-t2159-1 の後は取らない。

## 2. ask を人に見せ answer を書く

**結論: できる（選択肢の提示は条件付き）。**

- 対話の往復: TUI は普通の会話なので、ask の本文を見せ、人の答えを受け取れる。
- 選択肢の提示: Codex の `request_user_input` の tool が、Claude の `AskUserQuestion` に当たる。
  ただし 0.160.0 の既定（Default の collaboration mode）では、この tool は出ない。
  model に見える prompt には `Use the request_user_input tool only when it is listed in the available tools for this turn.` と、Default mode では控えよとの指示がある（`codex debug prompt-input` の出力）。
  `--enable default_mode_request_user_input`（feature は `under development`）で起動すると、Default mode で使えた。

  ```text
    Question 1/1 (1 unanswered)
    ask 12 (approve_landing): land this run?
    › 1. land               Land this run.
      2. cancel             Cancel landing this run.
      3. None of the above  Optionally, add details in notes (tab)
    tab to add notes | enter to submit answer | esc to interrupt
  ```

  ↓ と Enter で `cancel` を選ぶと、`answer: cancel` が返り、Codex は `You chose: cancel.` と答えた。
  開発中の feature なので、起動の時に `Under-development features enabled: default_mode_request_user_input. … set suppress_unstable_features_warning = true in ~/.codex/config.toml` の警告が出た（人の設定には書いていない）。
  「None of the above」と notes で、人自身の言葉も返せる。
- answer を書く: 人の答えを `dagq answer` で queue に書くのは shell のコマンドなので、下の 7 の経路の問題になる。

### 代わりの手段

- feature を有効にしない場合は、選択肢を番号つきの文で見せ、人が文で答える（Claude の skill が `AskUserQuestion` を「使えるとき」としているのと同じ扱い）。
- feature を有効にする場合は、開発中の feature に頼ることと、毎回の警告を受け入れる。

## 3. 指示の読み込み

**結論: AGENTS.md と repository の skill は自動で読み、初期の指示は起動の引数で渡せる。plugin の skill を起動の引数だけで足す手段は見つからなかった（条件付き）。**

`codex debug prompt-input -c '<trust>' hello`（scratch の repository の中）で、model に見える入力の列を出した。

- **AGENTS.md**: cwd の repository の `AGENTS.md` を user の message `# AGENTS.md instructions for <cwd>` として自動で読む。
  TUI で「agents marker?」と聞くと `MARKER-AGENTS-7731` と答えた。
  `/status` にも `Agents.md: AGENTS.md` と出る。
  dagq の repository の inbox は、Claude と同じく AGENTS.md を読む。
- **skill**: `~/.codex/skills`・`~/.codex/skills/.system`・cwd の repository の `.agents/skills` の `SKILL.md` を、`<skills_instructions>` の一覧として読む（name と description、path）。
  scratch の `.agents/skills/probe-skill` は、そこに `probe-skill: Use when … (file: r2/probe-skill/SKILL.md)` と出た。
  `-c 'skills.config=[{path="<plugins/claude-dagq/skills/dagq-inbox/SKILL.md>",enabled=true}]'` を足しても、一覧に `dagq-inbox` は出なかった。
  `skills.config` は有効・無効の切り替えで、skill を足すものではないとみられる。
- **plugin**: Codex にも plugin（`codex plugin add`・`codex plugin marketplace add`）と `hooks/hooks.json` を持つ plugin がある。
  ただし入れることは marketplace と plugin を人の `~/.codex` の設定に書くことなので、試していない（人の設定を書き換えないという goal の制約に当たる）。
- **初期の指示**: `-c 'developer_instructions="…"'` が developer の message として入った（`DEVMARK-5521 you are the dagq inbox`）。
  `codex [PROMPT]` の位置引数は初期の user の prompt（`codex --help`）。

`AgentProvider::inbox_command`（Claude）の各部分と Codex で当たるもの:

| Claude の部分 | 役目 | Codex で当たるもの |
| --- | --- | --- |
| `--settings <queue dir>/inbox-settings.json`（`permissions.deny`） | guardrail（role の dagq のコマンドと身元の env の拒否） | 直接は無い。候補は PreToolUse の hook（hook の trust が要る。下の 5）か `prefix_rule` の rules（`.codex/rules` は repository か `~/.codex` に置く。起動の引数で rules の path を渡せるかは未確認）。sandbox は書き込みと network を止めるが、コマンドの拒否ではない |
| `--plugin-dir <plugin>` | dagq・dagq-inbox などの skill と hook | 起動の引数で足す手段は見つからなかった。代わりは `developer_instructions` か初期の prompt で SKILL.md の絶対 path を名指して読ませること。hook は `-c hooks.<Event>=…` で渡せる（trust が要る） |
| `-- <inbox の prompt>` | 初期の prompt | `codex [PROMPT]` の位置引数。常に効く指示は `-c developer_instructions` |
| env の `DAGQ_ROLE=inbox`・`DAGQ_QUEUE`・`DAGQ_SERVICE_CREDENTIAL_FILE` | role と queue と inbox の token の file | `--no-daemon` の TUI では、shell の command に起動の env がそのまま渡った（`echo "$DAGQ_ROLE"` → `worker`）。daemon の経路は未確認 |

## 4. 復帰（compaction と会話のやり直し）

**結論: できる（hook の trust が条件）。**
Codex の `SessionStart` の hook は `source` に `startup`・`compact`・`clear`・`resume` を持ち、stdout が context に入る。
Claude の SessionStart の hook の `status --role inbox` に当たるものを差し込める。

hook は `-c hooks.<Event>=[…]` で起動の引数から渡せた。
ただし `-c` の hook も trust が要り、最初の起動で次の画面が出た。

```text
  Hooks need review
  6 hooks are new or changed.
  Hooks can run outside the sandbox after you trust them.
› 1. Review hooks
  2. Trust all and continue
  3. Continue without trusting (hooks won't run)
```

「Trust all」は hook の trust を人の設定に書く（binary の文字列に `config/batchWrite failed while updating hook trust in TUI`・`failed to write hook trust` がある）ので選ばず、3 を選んだ（hook は動かなかった）。
2 回目からは `--dangerously-bypass-hook-trust`（その起動だけ trust を飛ばす。`codex --help`）を付けて測った。

| 操作 | hook の記録（`hooks.log`） | context |
| --- | --- | --- |
| 起動の後の最初の turn | `SessionStart`（`source: startup`、`session_id: 01a16a84-0010-…`、`model: gpt-6.1-sol`）。起動しただけでは出ず、最初の turn の前に出た | 「HOOK-CONTEXT の行を引け」に `HOOK-CONTEXT-SessionStart: …` と答えた |
| `/compact` | `PreCompact`・`PostCompact`（`trigger: manual`）。次の turn の前に `SessionStart`（`source: compact`、同じ `session_id`） | 次の turn で SessionStart の行が context にあった |
| `/clear` | 次の turn の前に `SessionStart`（`source: clear`、**新しい** `session_id: 01a16a86-85a4-…`）。古い session の `SessionEnd` は出なかった | SessionStart の行があった |
| `/quit` の後の `codex resume 01a16a86-85a4-…` | 最初の turn の前に `SessionStart`（`source: resume`、同じ `session_id`） | SessionStart の行があった |

- 自動の compaction は起こしていない（context の 97% が残る短い session だった）。
  `PreCompact` の `trigger` に `manual` 以外の値があるかは未確認。
- `Stop` の hook は stdout が JSON でないと `hook returned invalid stop hook JSON output` で失敗する。
  `{"decision":"block","reason":"STOP-GUARD: …"}` を 1 回だけ返す hook では、画面に `Blocked by hook` と理由が出た。
  Codex は `STOP-GUARD-SEEN. What watch command should I start?` と続けた。
  Claude の inbox の Stop hook（watch が無ければ turn の終わりを止める）に当たるものも作れる。
- 起動の引数の `-c` は `codex resume` にも渡す必要がある（resume は新しい process で、前の `-c` を持たない）。

### 代わりの手段

- hook の trust の扱いは 3 つ。
  - (i) 起動ごとに `--dangerously-bypass-hook-trust` を渡す。人の設定は書かないが、flag の名前どおり、有効な hook を全部確かめずに動かす。
  - (ii) 最初の起動で人が trust する。人の `~/.codex/config.toml` に書かれるので、dagq が書くのではないが、goal の制約との関係を ADR が決める。
  - (iii) hook を使わず、`developer_instructions` に「compaction・`/clear`・resume の後は最初に `dagq status --role inbox` を打て」と書く（model 任せで、確実さは落ちる）。

## 5. 区間の記録

**結論: できる（hook の trust が条件。区間の始まりは最初の turn まで遅れ、`/clear` の古い session の終わりは process の終わりまで遅れる）。**

| 手段 | 出るもの | 観察 |
| --- | --- | --- |
| `SessionStart` の hook | `session_id`・`transcript_path`（rollout の path）・`cwd`・`model`・`permission_mode`・`source` | 起動では出ず、最初の turn の前に出る。`/clear` で新しい `session_id` |
| `SessionEnd` の hook | `session_id`・`transcript_path`・`cwd`・`reason`（`other`） | `/quit` で、今の session と `/clear` の前の session の両方の 2 行が同時に出た。`/clear` の時点では出ない |
| `notify`（`-c notify=[…]`） | `type: agent-turn-complete`・`thread-id`・`turn-id`・`cwd`・`client: codex-tui`・`input-messages`・`last-assistant-message` | turn の終わりごとに出る。hook と違い trust は要らなかった。thread の題を作る内部の turn も別の `thread-id` で出る。始まりと終わりの event は無い |
| rollout（`~/.codex/sessions/YYYY/MM/DD/rollout-<時刻>-<thread>.jsonl`） | `session_meta` の `id`・`originator: codex-tui`・`cli_version: 0.160.0`・`source: cli`・`model_provider: openai`・`cwd`。`turn_context` の `model: gpt-6.1-sol`・`effort: low`・`approval_policy: on-request`・`sandbox_policy` | path は hook の `transcript_path` で分かる |
| process の終わり | `/quit` で TUI は exit 0 で終わり、`To continue this session, run: codex resume <id>` を出す | `dagq inbox` は exec で前面に置くので、dagq の process は残らず、終わりを見る親が居ない（Claude の inbox と同じ） |

- provider は hook の入力には無いが、rollout の `model_provider` と、起動した provider（dagq が知る）で分かる。
  実際の model は hook の `model` と rollout の `turn_context.model` の両方で分かる。
- Claude の inbox の区間の記録（plugin の `session-event.sh` の `open` / `close`）に当たるものは、`SessionStart` / `SessionEnd` の hook で書ける。
  ただし、項目 4 と同じ trust の条件がある。
- `/clear` の古い session の終わりが process の終わりまで出ないので、Claude と同じ区間の形にするなら、新しい `SessionStart`（`source: clear`）で前の区間を閉じる推定が要る（[ADR-t2022-1](../adr/2026-10-08-t2022-1-supervisor-infers-ended-hook-sessions-without-cmux.md) の推定と同じ考え方）。

## 6. 画面の状態（測定のためだけ）

**結論: tmux の `capture-pane` で、入力待ち・作業中・承認・ユーザーへの問い・認証・更新の画面を見分けられた。**
これはこの測定で Codex を操るための読み取りで、dagq の仕組みとして画面を読むことではない。

| 状態 | 手がかり（画面の文字列と形） |
| --- | --- |
| 入力待ち | 最後の方に、入力欄の placeholder `› Ask Codex to do anything` と、その下の `<Model> <effort> · <cwd>` と `? for shortcuts`。直前の turn の後には `Worked for <N>s • <時刻>` |
| 打ち込んだが送っていない | placeholder の代わりに `› <打った文>` が入力欄（model の行のすぐ上）に残り、`Working` が無い |
| 送った（Enter の後） | 打った文が履歴に上がり、`Working (0s • esc to interrupt)`、入力欄は placeholder に戻り、model の行の末尾に spinner（`⠋`） |
| 作業中で background terminal を待つ | `Waiting for background terminal (18s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close` |
| 作業中に打った文 | `• Messages to be submitted after next tool call (press esc to interrupt and send immediately)` と `↳ <文>` |
| 承認のダイアログ | `Would you like to run the following command?`・`Reason: …`・`$ <command>`・`› 1. Yes, proceed (y)`・`2. Yes, and don't ask again for commands that start with … (p)`・`3. No, and tell Codex what to do differently (esc)`・`Press enter to confirm or esc to cancel` |
| `request_user_input` の問い | `Question 1/1 (1 unanswered)`、番号つきの選択肢、`tab to add notes \| enter to submit answer \| esc to interrupt` |
| hook の trust | `Hooks need review`・`Hooks can run outside the sandbox after you trust them.` |
| 更新の案内（起動のたび） | `Update available · 0.160.0 → 0.164.0`・`1. Update now …`・`2. Skip`・`3. Skip until next version`・`enter continue · esc skip`。`-c check_for_update_on_startup=false` で出なくなった（resume と以後の起動） |
| 認証が要る | `Welcome to Codex, OpenAI's command-line coding agent`・`Sign in with ChatGPT …`・`> 1. Sign in with ChatGPT`・`2. Sign in with Device Code`・`3. Provide your own API key`・`Press enter to continue`（下の 8） |
| 終わった | `To continue this session, run:`・`codex resume <id>`（tmux の `remain-on-exit` で `Pane is dead (status 0, …)`） |

- 起動の時の `⚠ N warnings · f2 to view` は開発中の feature や hook の trust を飛ばしたことの警告で、入力は受ける。
- 承認のダイアログの 2（don't ask again）は rules を人の設定に残しうるので、1（一度きり）を選んだ。
  `~/.codex/rules/default.rules` の更新時刻は変わらなかった。

dagq は inbox の画面を読まず（ADR-t1433-5 決定 1）、ADR-t2159-1 で cmux の adapter と inbox の画面を読むコードも無くなった。
Codex の inbox で runtime が画面の状態を知る必要があるか（例: 承認のダイアログや `request_user_input` で止まった inbox を runtime が知るか）は、下の ADR の問いの候補にした。
画面を読まずに知る手がかりは次の 2 つで、どちらも承認のダイアログの「待っている」状態そのものは出さない。

- `notify` の `agent-turn-complete`（turn の終わり）。
- 0.160.0 の hook の event に `PermissionRequest` がある（binary の文字列。測っていない）。

## 7. sandbox からの状態を変える dagq

**結論: 条件付き。**
今の経路（sandbox から host の queue service の socket に届き、状態を変える操作を service のユースケースとして送り、service が token の principal で認可する。[ADR-t1233-5](../adr/2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md) 決定 4・5、[Queue service](../design/queue-service.md#クライアントモード)）は、socket への到達の部分は対話の TUI でも成り立った。
inbox の principal と token は既に決まっていて一部は実装済みで、残りは inbox の書き込みのユースケースと Codex の sandbox の設定である（下の「既決・未実装・問い」）。

本番の queue と queue service には触れず、scratch の UNIX socket と SQLite の file で代えた。

| sandbox の設定（TUI の起動の引数） | socket に繋ぐ（`python3 -I -c "…s.connect('<q>/svc.sock')…"`） | sandbox の外の dir の SQLite に書く（`sqlite3 <q>/q.db 'create table …'`） | 他 |
| --- | --- | --- | --- |
| 既定（trusted の project: `workspace-write`、`approval_policy: on-request`、network は閉じる。`/status` は `Permissions: Workspace (Ask for approval)`） | **拒否**: `PermissionError: [Errno 1] Operation not permitted`（exit 1） | **拒否**: `Error: unable to open database "…/q.db": unable to open database file`（exit 1） | `$TMPDIR` の SQLite は書けた（`1`、exit 0） |
| 既定で、Codex が escalation（`require_escalated`）を求め、人が「Yes, proceed」を選ぶ | 通った: `b'PONG from-codex-default'` | 試していない | 承認は一度きり。毎回の承認になる |
| `-c sandbox_workspace_write.network_access=true`（worker の turn と同じ） | 通った: `b'PONG from-codex-network-on'`（escalation なし） | **拒否**（同じ文） | TCP も開く（worker と同じ） |
| job と同じ socket だけの permission profile（`-c features.network_proxy=true -c default_permissions="dagq_inbox" -c 'permissions.dagq_inbox.extends=":read-only"' -c permissions.dagq_inbox.network.enabled=true -c 'permissions.dagq_inbox.network.unix_sockets={"<q>/svc.sock"="allow"}'`） | 通った: `b'PONG from-codex-profile'` | — | `curl https://example.com` は `CONNECT tunnel failed, response 403`（exit 56）、repository への `touch` は `Operation not permitted` |
| `codex sandbox -c 'sandbox_mode="workspace-write"' -c 'sandbox_workspace_write.writable_roots=["<q>"]' -- sqlite3 <q>/q.db …`（TUI ではなく同じ seatbelt の `codex sandbox`） | — | 書けた（`1`）。`writable_roots` が無ければ同じ拒否 | |

### 既決・未実装・問い

この測定の時の main（d7dbc989）で、inbox の principal は次のとおりである。

- **既決**（[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md) 決定 2〜4、[ADR-t3102-2](../adr/2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md) 決定 1・5・6）:
  - inbox の token は、user を親として、inbox の session を開く前に発行する。対象はその inbox の session で、session の終わりと開き直しで失効する（ADR-t3102-1 決定 2 の表）。
  - service は inbox を、対象を 1 つ持つ token の principal として受け、inbox の principal で認可し記録する（決定 3）。
    inbox の session の hook の記録も inbox の principal のままで、inbox は user と同じ権限である（決定 4、[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)）。
  - 段(5)の後に DB を開くのは service（と `init`）だけで、inbox の書き込みも service 経由にする（ADR-t3102-2 決定 1・5、決定 6 の移行の段 4）。
- **実装済み**（[hostのCLI](../design/queue-service-host-cli.md)）:
  - `dagq inbox` が自分の process を session として inbox の token を発行し、file の場所を `DAGQ_SERVICE_CREDENTIAL_FILE` で agent に渡す（`Principal::inbox`）。
    file は queue の dir の `service/credentials/` に置く。
    発行は今は制御側が file に直接書く（[ADR-t3105-1](../adr/2026-10-23-t3105-1-reopen-the-inbox-once-after-the-host-cli-reaches-the-queue-service.md) 決定 5 の例外）。
  - inbox の読み取り・`watch`・`status` と、service にユースケースのあるコマンドは、socket に送られ DB を開かない。
- **未実装**: ユースケースの無い書き込み（`answer`・`ask close`・`request add` など）は、今は DB を直接開く（[hostのCLI](../design/queue-service-host-cli.md)の「送る範囲」、移行の段 4 が service 経由にする）。
  Claude の inbox は sandbox を持たないので書けるが、Codex の inbox の sandbox の中では、上の表の SQLite の書き込みと同じく DB を開けずに失敗する見込みである。
- **後続の ADR（task 1181）で決める問い**（Codex に固有のものだけ）:
  - Codex の inbox の sandbox で socket に届かせる設定。
    network を開ける（TCP も開く）か、socket だけの profile（file は read-only を継ぐ）か、書き込みも要るなら `:workspace` に当たる profile を継げるか（未確認）。
  - token の file と env が sandbox の中の `dagq` に届くこと。
    macOS の sandbox は読み取りを全域で許すので、file は読める見込みである。
    env の名前は Codex が command から外す `KEY`・`SECRET`・`TOKEN` を含まない（`CREDENTIAL_FILE_ENV` の doc comment）。
    `--no-daemon` の TUI で起動の env が command に渡ることは、この測定の `DAGQ_ROLE` で確かめた。
  - Codex の inbox を、書き込みの service 経由（移行の段 4）の着地を待って開けるようにするか、それまでの間だけ下の緩める案で動かすか。

### 候補

- **主な候補: 今の経路を inbox に当てはめる。**
  inbox の Codex は queue の DB を開かず、クライアントモードの `dagq` で、inbox の principal の token とともに service の socket に送る。
  principal・token・認可は上の既決のとおりで、socket への到達は上の 2 つの設定（network を開ける、か socket だけの profile）で成り立った。
  足りないのは、移行の段 4 の inbox の書き込みのユースケースと、Codex の起動の sandbox の設定である。
- **比べる案: sandbox を起動の引数で緩める。**
  `writable_roots` に queue の dir を足し、inbox が今の Claude の inbox と同じく DB を直接開く。
  `codex sandbox` で書けることは確かめた。
  移行の段 4 を待たずに済むが、次の点がある。
  - Codex の inbox が DB を直接書く経路が残り、ADR-t3102-2 決定 1（段(5)の後に DB を開くのは service だけ）に反する。段 4 までの間の案にしかならない。
  - socket と違い、書く範囲を DB の操作に絞れない。
  - WAL の `-shm`・`-wal` と lock の file も同じ dir に要る。
  `--sandbox danger-full-access` や `--dangerously-bypass-approvals-and-sandbox` は、sandbox を外すことになり候補にしない。
- 既定の sandbox のまま、Codex が escalation を求めて人が毎回承認する形は、inbox の操作のたびに人の承認が要るので inbox の目的に合わない。
- 歴史的な案: run の dir への要求を supervisor が取り込む経路（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md) 決定 3）は、ADR-t1233-5 決定 5 が service への送信に amends した歴史的な案で、主な候補にしない。

## 8. Codex が使えないとき

制約（`CODEX_HOME` を変えない）の内で確かめたのは、実行ファイルが無いときと、利用上限の残りの表示である。
未ログインは、測定の 1 process だけ `CODEX_HOME` を使い捨ての空の dir にして再現した。
これは task の Context の「`CODEX_HOME` は変えない」の例外で、人の永続の設定を変えていないので人が認めた（ask 1159）。
下の表の「範囲」の列で両者を分ける。

| 状態 | 範囲 | 再現 | 形 |
| --- | --- | --- | --- |
| 実行ファイルが無い | 制約の内 | `sh -c '<scratch>/no-such-codex --no-daemon; echo exit-rc=$?'` | `sh: …/no-such-codex: No such file or directory`、`exit-rc=127`。`dagq inbox` が exec する前に、実行ファイルがあるかを見られる |
| 未ログイン | 例外（ask 1159） | `CODEX_HOME=<scratch>/empty-codex-home` の 1 process だけ（人の `~/.codex` と、人の環境の `CODEX_HOME` は変えていない）。その `CODEX_HOME` で `codex login status` は `Not logged in` で exit 1 | TUI は終わらず、`Welcome to Codex …`・`Sign in with ChatGPT …` の対話の画面で止まる。Ctrl-C で exit 0。起動の前に `codex login status` の exit code で見分けられる |
| 利用上限 | 制約の内 | 再現していない | **未確認**。`/status` に `Weekly limit: [██████…] 89% left (resets 02:42 on 31 Oct)` の行があり、上限の残りは画面で見える。上限に当たった時の対話の画面と終わり方は未確認 |

## 結論の表

| # | 項目 | 結論 | 代わりの手段・条件 |
| --- | --- | --- | --- |
| 1 | 常駐と background の見張り | 条件付き | background の終わりで session は起きない。前景の待ち（人は待ちの間も打てる）か、外の process が `codex queue --thread` で起こす。sandbox の中からの `codex queue` は失敗する。`dagq watch --role inbox` そのものは手動スモークで確かめる |
| 2 | ask を見せ answer を書く | できる（選択肢は条件付き） | `request_user_input` は `--enable default_mode_request_user_input`（開発中）で Default mode でも使える。無ければ番号つきの文。`dagq answer` は 7 の経路 |
| 3 | 指示の読み込み | 条件付き | AGENTS.md と `.agents/skills` は自動。初期の指示は `developer_instructions` と `[PROMPT]`。plugin の skill を起動の引数で足す手段は無く、SKILL.md の path を指示で名指す。guardrail の settings に当たるものは無い |
| 4 | 復帰 | できる（hook の trust が条件） | `SessionStart` の `source` が `startup`・`compact`・`clear`・`resume`、stdout が context に入る。`-c` の hook は trust が要り、`--dangerously-bypass-hook-trust` か人の trust。Stop の hook は JSON で block できる |
| 5 | 区間の記録 | できる（同じ条件） | `SessionStart` / `SessionEnd` の hook に `session_id`・`model`、rollout に `model_provider`・`cli_version`・`model`。`notify` は trust なしで turn の終わりごと。始まりは最初の turn まで、`/clear` の前の session の終わりは `/quit` まで遅れる |
| 6 | 画面の状態（測定） | 見分けられた | 入力待ち・送信前・作業中・承認・問い・trust・更新・認証の文字列。起動のたびの更新の案内は `-c check_for_update_on_startup=false` で消える |
| 7 | sandbox からの状態を変える dagq | 条件付き | socket へは network を開けるか socket だけの profile で届く。inbox の principal と token は既決で実装済み（ADR-t3102-1）。未実装は `answer` などの書き込みのユースケース（ADR-t3102-2 の移行の段 4）。緩める案は段 4 までの間の `writable_roots` |
| 8 | 使えないとき | 一部確認 | 実行ファイルが無い: 127（制約の内）。未ログイン: ログインの画面で止まり、`codex login status` が exit 1（`CODEX_HOME` の例外の測定、ask 1159）。利用上限: 未確認 |

## ADR（task 1181）で決めることの候補

1. **inbox の provider をどこで選ぶか**: `[roles.inbox]`（ADR-t1063-1 の役割ごとの provider を inbox に広げる）か、`dagq inbox` の起動の引数（例 `--provider codex`）か、両方（設定を既定にして引数で上書き）か。
   根拠: `dagq inbox` は人が自分の terminal で打つ（ADR-t2159-1 決定 2）ので、引数はその場で選べる。
   一方、区間と起動の記録を役割の provider と並べて比べる（goal 77 の受け入れ (3)）には、設定の `source`（`dagq.toml` など）が残る形が揃う。
   Codex の起動には Claude に無い引数（trust の `-c`、`--no-daemon`、hook、sandbox の `-c`、`check_for_update_on_startup=false`）が要り、どちらで選んでも provider の adapter が持つ。
2. **見張りの埋め方**: 前景の待ち（prompt と skill で watch を前景で回させる）か、外の process が `codex queue` で起こすか。
   根拠: 項目 1。前景の待ちは dagq に新しい経路が要らないが、model 任せで、turn が長く開く。
   `codex queue` は確実に起こすが、`dagq inbox` が見張りの process を持ち、dagq が inbox に話しかける経路を持つことが ADR-t1433-5 決定 2 の趣旨に合うかを決める必要がある。
3. **復帰・区間・guardrail の hook の trust**: `--dangerously-bypass-hook-trust` を `dagq inbox` が渡すか、人が最初の起動で trust するか（人の `~/.codex/config.toml` に Codex が書く）、hook を使わず指示と `notify` で代えるか。
   根拠: 項目 4・5。goal 77 の制約（人の設定を書き換えない）と、dangerously の flag を worker と job に付けない今の方針との関係。
4. **ADR-t2159-1 決定 5 の guardrail を Codex でどう持つか**: PreToolUse の hook、rules、sandbox（書き込みと network の制限）のどれで「dagq の CLI を通す」を持つか、手段が無ければ design にそう書くか。
   根拠: 項目 3 の表。Codex に `--settings` の `permissions.deny` に当たるものが無い。
5. **sandbox からの状態を変える dagq の Codex の部分**: inbox の principal（user を親に inbox の session に発行する token、inbox の principal での認可と記録、user と同じ権限）は ADR-t3102-1 決定 2〜4 で既決なので、問いにしない。
   問いは次の 2 つ。
   - Codex の inbox の sandbox の設定（network を開けるか、socket だけの profile か、書き込みの要る範囲）。
   - inbox の書き込みが service 経由になる ADR-t3102-2 の移行の段 4 の着地を、Codex の inbox の前提にするか。それまでの間だけ `writable_roots` で DB を直接開かせるか。
   根拠: 項目 7 の「既決・未実装・問い」。今の main では `answer`・`ask close`・`request add` が DB を直接開き、Codex の sandbox ではそれが書けない。
6. **Codex が使えないときに Claude に戻すか**: `dagq inbox` が起動の前に実行ファイルと `codex login status` を見て、使えなければ Claude で開くか、止めて人に言うか。
   根拠: 項目 8。実行ファイルが無いとき（制約の内）と未ログイン（`CODEX_HOME` の例外の測定）は、起動の前に見分けられる。利用上限は未確認なので、起動の後に上限に当たった inbox の扱い（人が開き直す）も決める。
   inbox は人が開くので、黙って provider を変えると比べる記録が混ざる（受け入れ (3)）。
7. **項目 6 の画面の判定が ADR-t2159-1 の後も要るか**: 承認のダイアログ・`request_user_input`・ログインの画面で止まった Codex の inbox を runtime が知る必要があるか。
   要るなら画面ではなく `notify`（turn の終わり）や `PermissionRequest` の hook で代えられるか。
   根拠: 項目 6。dagq は inbox の画面を読まない（ADR-t1433-5 決定 1、ADR-t2159-1 決定 1）。Claude の inbox でも止まった inbox は人が見ている前提である。
8. **daemon の扱い**: `--no-daemon` を常に渡すか。
   根拠: 共有の daemon に繋ぐと、`DAGQ_ROLE=inbox` などの env と `-c` が command に渡るかが未確認で、渡らなければ role の判定と trust が崩れる。

## planner に当てはまるか

人が開く planner は廃止され、`dagq plan` は拒まれる（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）。
planner は runtime が非対話の background の job として立てる。
runtime の planner を Codex で動かすなら、それは `codex exec`（と `exec resume`）の経路で、この測定の対話の TUI ではない。

- 当てはまらないもの:
  - 1（見張り）: planner は watch を回さず、turn ごとに runtime が起こす。
  - 2（人との対話）: planner は人に直接聞かず、ask は inbox に宛てる。
  - 6（画面）: 非対話の job には画面が無い。
  - 8 の未ログインの画面: exec は対話の画面を出さずに失敗で終わる見込み。
  - 4 の `/compact`・`/clear` の操作: 非対話では人が打たない。
- 当てはまりうるもの:
  - 3: `codex exec` も AGENTS.md と `.agents/skills` を読む。plugin の skill（dagq-planner）を引数で足す手段が無いことは同じである。
  - 5: rollout の `session_meta`・`turn_context` から provider・model・thread を読むことは、worker と job で既に使っている（[provider-lifecycle](../design/provider-lifecycle.md#codexのheadless-job)）。
  - 7: 当てはまるのは socket への到達の結果だけである。
    sandbox から service の socket へは、network を開けるか socket だけの profile で届いた。
    planner の書き込みは、まだその経路に乗らない。
    planner の token と、読み取り・`ask` などのユースケースのあるコマンドは、socket に送られる（[hostのCLI](../design/queue-service-host-cli.md)）。
    一方、goal・task の登録（`add`・`goal add`・`submit` など）はクライアントモードのユースケース（`src/main.rs` の `client_request`）に無く、host の planner は今は DB を直接開く（[hostのCLI](../design/queue-service-host-cli.md)の「送る範囲」）。
    この書き込みを service 経由にするのは、[ADR-t3102-2](../adr/2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md) 決定 6 の移行の段 4 である。
    sandbox の中の Codex の planner が登録を書くには、inbox と同じく段 4 の書き込みのユースケースが要る。
- hook: `codex exec` で hook が動くか、trust の扱いが TUI と同じかは測っていない（binary に `exec/src/lib.rs` の `bypass_hook_trust` の文字列があり、exec にも trust の扱いがあるとみられる）。
- 対話の TUI の結論のうち planner に持ち込めるのは 3・5 と、7 の socket への到達である。7 の書き込みは段 4 の着地が前提になる。1・2・4・6 は inbox に固有である。
  Codex の planner は、Codex の headless job（[Codex の headless job](../design/provider-lifecycle.md#codexのheadless-job)）の延長として別の goal で調べる。

## 手動スモークに回したもの

人の terminal と使い捨ての queue が要る次の確認は、worker にはできない。
[手動スモーク](../development/manual-smoke.md)の「Codex の inbox のスモーク」に手順を書き、人か inbox が流す。

- 人が `dagq inbox` で Codex の inbox を開くこと。
- `dagq watch --role inbox` を Codex の inbox で回して attention に気づくこと。
- 使い捨ての queue の ask への answer。
- 画面の観察。
