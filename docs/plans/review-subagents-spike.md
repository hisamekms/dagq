---
id: plan-review-subagents-spike
type: plan
title: スパイク：run の review job の中で review の subagent を Claude と Codex の非対話の呼び出しで動かせるか
status: completed
created: 2026-10-03
updated: 2026-10-03
owners:
  - hisamekms
tags:
  - planning
  - review
  - provider
related:
  - adr-t1453-1
  - plan-codex-headless-jobs-spike
  - design-supervisor-lifecycle-review
---

# スパイク：run の review job の中で review の subagent を Claude と Codex の非対話の呼び出しで動かせるか

goal 94 の task 1453 が [ADR-t1453-1](../adr/2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md) を決めるために、provider の CLI の能力と許可の設定を確かめた記録。queue は使わず、`/tmp` の使い捨ての Git repository（`git init` だけ）で非対話の呼び出しを数回行った。`src/` は変えていない。

## 環境

- 日付: 2026-10-03（JST）。host は macOS 14（arm64、Darwin 23.6.0）
- Claude Code 2.1.287（`~/.local/bin/claude --version`）。試しの呼び出しは費用を抑えるため `--model claude-haiku-4-5-20251001`
- codex-cli 0.160.0（`~/.local/bin/codex --version`）。model は既定、`-c model_reasoning_effort="low"`
- 今の review の起動（`src/infrastructure/adapters.rs` の Claude の `review_command`、`src/infrastructure/codex.rs` の `headless_command`）: Claude は worktree を cwd に `claude -p --settings <run dir の review の settings> --allowedTools Read,Grep,Glob --disallowedTools Bash,Edit,Write,NotebookEdit`（setting sources は既定）。Codex は `codex exec --json --skip-git-repo-check --sandbox read-only -C <worktree>`（実際は job の permission profile の `-c`）

## Claude

### 1. 非対話で定義を渡す方法

`claude --help` の `--agents <json-or-file>`: 「JSON object defining custom agents, or with --print the path to a file that holds one」。`-p` では JSON の文字列か、それを持つファイルの path を渡せる。各 agent は `description`・`prompt` と任意の `tools` を持つ。

確かめた呼び出し（cwd は使い捨ての repository、`a.txt` に `hello`）:

```sh
claude -p --model claude-haiku-4-5-20251001 --setting-sources "" \
  --agents '{"checker":{"description":"Checks a.txt","prompt":"Read a.txt and reply with its first word in uppercase, nothing else.","tools":["Read"]}}' \
  --allowedTools Read,Grep,Glob,Agent --disallowedTools Bash,Edit,Write \
  -- 'First, list ... Then call the checker subagent and report its exact reply on a second line.'
```

結果: 親が `checker` を呼び、`**Checker agent's exact reply:** HELLO` を返した。非対話の `-p` で、`--agents` で渡した定義の subagent が動き、親がその返事を集めて答えられる。

### 2. worktree の `.claude/agents` を読まない方法

使い捨ての repository に `.claude/agents/planted.md`（worker が置いたものに見立てる）を置き、Agent の tool が受ける subagent の種類を答えさせた。

| setting sources | 答え |
| --- | --- |
| 既定（今の review と同じ） | `claude, Explore, general-purpose, Plan, planted, statusline-setup` |
| `--setting-sources ""` と `--agents` | `checker, claude, Explore, general-purpose, Plan, statusline-setup` |

既定では cwd の project の `.claude/agents` が読み込まれる（worktree の `planted` が見える）。`--setting-sources ""` で project・local・user の settings を外すと読み込まれず、`--agents` の定義だけが加わる。今の review は cwd が worker の worktree で setting sources が既定なので、worktree の `.claude/agents` と `.claude/settings.json`（worker が変えられる）を読み込みうる。

### 3. subagent の tool の許可

`tools: ["Write","Bash","Read"]` を持つ subagent を、親に `--allowedTools Read,Grep,Glob,Agent --disallowedTools Bash,Edit,Write,NotebookEdit` を付けて呼び、b.txt の作成と `touch c.txt` を試させた。

結果: Write も Bash も拒まれた（`Bash is disabled for this session, in subagents as well as here.`）。repository には `a.txt` しか残らなかった。親の `--disallowedTools` は subagent にも効き、定義の `tools` で広げられない。subagent を呼ぶには親の `--allowedTools` に `Agent` が要る。

## Codex

### 4. 非対話での multi-agent の有無

`codex features list`: `multi_agent  stable  true`（`multi_agent_v2` は stable だが false）。

### 5. 役割の定義を渡して呼ぶ

```sh
codex exec --json --skip-git-repo-check --sandbox read-only -C <dir> \
  -c 'model_reasoning_effort="low"' \
  -c 'agents.checker.description="Checks a.txt"' \
  -c 'agents.checker.config_file="<dir>/checker.toml"' \
  -- 'First list ... the tools you have for starting or waiting for sub-agents and the agent types/roles ... Then start a sub-agent of the role "checker" ...'
```

`checker.toml` は `developer_instructions = "Read a.txt and reply with its first word in uppercase, nothing else."` だけ。

結果: 終了コード 0。最初の agent message は `Tools: collaboration.spawn_agent, collaboration.followup_task, collaboration.wait_agent; roles: default, explorer, worker, checker.`、最後の agent message は `HELLO`。`-c agents.<name>.*` で渡した役割が `--sandbox read-only` の `exec` で見え、sub-agent を始めて待つ tool がある。

JSON の event には `collab_tool_call`（`tool: "wait"`）が始まりと終わりの 2 件だけ出て、どちらも `receiver_thread_ids` が空で、spawn の event は出なかった。

## 確かめられなかった点

- **Codex の sub-agent が実際に動いたか。** `HELLO` は sub-agent の返事か、親が自分で `a.txt` を読んだ答えかを、`exec --json` の event から区別できなかった（spawn の event が無く、wait の受け手が空）。runtime は event から sub-agent の実行を確かめられず、verdict のデータに頼る。
- **Codex の sub-agent の sandbox。** 親の `--sandbox read-only`（と job の permission profile）を sub-agent が継ぐか。
- **Codex の project の設定。** worktree の `.codex/config.toml` が `agents.*` を足す・変えるか、`-c` が同じ key の project の値に勝つか。信頼していない project の設定を読まないはずだが試していない。
- **Claude の `--setting-sources ""` と `--settings` の組み合わせと `.claude/settings.json`。** worktree の `.claude/settings.json` が `--setting-sources ""` で読まれなくなるかは試していない。 今の review は `--settings` で deny を渡す。`--setting-sources ""` と併せて `--settings` の deny が効くかは試していない（help は `--restricted` について「managed settings and --settings still apply」とだけ書く）。worktree の `CLAUDE.md` が `--setting-sources ""` で読まれなくなるかも試していない。
- **Claude の subagent の同時実行と時間。** 複数の subagent の並行の可否と、review の時間の上限との関係。
- **定義をファイルで渡す `--agents <file>`。** help の記述だけで、ファイルでは試していない。

確かめられなかった点は ADR-t1453-1 に未確認と書き、runtime の実装の task が test（fixture の provider と、必要なら実 CLI の e2e）で確かめる前提にした。
