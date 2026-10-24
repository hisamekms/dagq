---
id: plan-codex-inbox-hook-trust-spike
type: plan
title: スパイク：Codex の対話の TUI で、dagq が起動の引数で渡す hook だけに起動の引数で trust を与え、人の ~/.codex/config.toml を書かずに動かせるか
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
  - adr-t813-3
  - plan-codex-inbox-spike
---

# スパイク：Codex の対話の TUI で、dagq の hook だけに起動の引数で trust を与えられるか

goal 77 の Spike（task 3336）。
[ADR-t1181-1](../adr/2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md) 決定2 と「実装の順」(b) は、hook の trust の省略を dagq が起動の引数で渡す hook だけに限れるかを、実装の前に確かめるとした。
有効な全 hook の trust を飛ばす引数は使わない（決定2）。
この task は測って書くだけで、`src/` と `docs/design/` は変えていない。

問い: codex-cli の対話の TUI を `--no-daemon` で起動するとき、dagq が `-c hooks.<Event>=…` で渡す hook（SessionStart・SessionEnd・Stop・PreToolUse）だけに起動の引数で trust を与え、trust の画面を出さずに動かせるか。
同時に、起動の引数で渡していない hook には trust の確認が残り、人の `~/.codex/config.toml`・`~/.codex/auth.json` は書かれないか。

## 環境と対象

- 対象の commit: `eed007b652d015a660449061fbd264b1165db27d`（この run の base）。dagq の runtime は使っていない
- 日付: 2026-10-25 03:40〜03:58（JST。1 巡目は 03:42〜03:45、2 巡目は 03:56〜03:58）。host は macOS 14.7.4（arm64、Darwin 23.6.0）
- codex-cli **0.160.0**（`~/.local/bin/codex` → `~/.codex/packages/standalone/current/bin/codex` → `releases/0.160.0-aarch64-apple-darwin/bin/codex`）。
  この session の PATH の先頭の `codex` は cmux の shim で 0.164.0 を返すが、dagq の runtime が使う実体と同じ `~/.local/bin/codex` で測った。
  0.164.0 は測っていない
- route: 対話の TUI を `--no-daemon` で起動した（人の Codex の app-server の daemon に繋がない）。
  model は指定せず人の設定の既定の `gpt-6.1-sol`（provider `openai`、ChatGPT の login）で、`-c model_reasoning_effort="low"` を渡した
- scratch: `$TMPDIR/dagq-worker-t3336-NpaEvU/repo`（`git init -b main`、`seed.txt` の 1 commit）。queue も supervisor も使っていない
- pty: tmux 3.6a を専用の socket（`tmux -L dagq-t3336 -f /dev/null`）の detached の session で動かし、`send-keys` で打ち込み、`capture-pane -p` で画面を読んだ
- env: worker の session のものを継いだ。`dagq watch` も、本番の queue の状態を変える dagq のコマンドも打っていない

### 使わなかった引数と選ばなかった選択肢

- `--dangerously-bypass-hook-trust` は使っていない。sandbox・承認を外す引数（`--dangerously-bypass-approvals-and-sandbox`・`--sandbox danger-full-access`・`-c approval_policy=never` など）も使っていない。
  sandbox と承認は trusted の project の既定（`workspace-write`・`on-request`）のまま。
- trust の画面では毎回「3. Continue without trusting (hooks won't run)」だけを選んだ。「Review hooks」と「Trust all and continue」は選んでいない。
- 人の `~/.codex`・`CODEX_HOME`・`~/.claude` は書き換えていない。`auth.json` は写していない（下の sha256 は `shasum` で読んだだけ）。

## key と trusted_hash の求め方

openai/codex の tag `rust-v0.160.0` の source を読んで求めた（読むだけ。build していない）。

- **trust の状態を読む層**: `codex-rs/hooks/src/config_rules.rs` の `hook_states_from_stack` は、`hooks.state` を User の層（`~/.codex/config.toml`）と SessionFlags の層（`-c`）だけから読む。
  project の `.codex/` と managed と plugin の層は hook を見つけるが、trust の状態は書けない（comment にそう書いてある）。
  したがって `-c hooks.state.…` は起動の時だけの trust の状態になり、repository に置いた設定が自分の hook を trust することはできない。
- **key**: `codex-rs/hooks/src/lib.rs` の `hook_key` は `<key_source>:<event の label>:<group の番号>:<handler の番号>`。
  `-c` の hook の `key_source` は SessionFlags の層の仮の path `"/<session-flags>/config.toml"`（`discovery.rs` の `config_toml_source_path` と `synthetic_layer_path`。Windows 以外は `/` を前に付ける）。
  event の label は `session_start`・`session_end`・`stop`・`pre_tool_use` など（`hook_event_key_label`）。
  例: `-c hooks.SessionStart=[{hooks=[{…}]}]` の最初の group の最初の handler は `/<session-flags>/config.toml:session_start:0:0`。
- **hash**: `discovery.rs` の `hook_hash` は、正規化した hook を `{event_name: <label>, matcher?: …, hooks: [<正規化した handler 1 つ>]}` の TOML の値にし、`codex-rs/config/src/fingerprint.rs` の `version_for_toml` で JSON にして object の key を辞書順に並べ、空白の無い直列化の sha256 を `sha256:<hex>` にする。
  command の handler の正規化は `type = "command"`・`command`（そのままの文字列）・`timeout`・`async`（未指定なら false）。
  `timeout` は未指定なら SessionEnd と Interrupt は 1、他は 600 で、SessionEnd と Interrupt は 3 を上限に切り詰めた値が入る（`normalize_command_hook`。既定の 1 は source の comment と、測った hash が合ったことで確かめた）。
  `statusMessage` は指定したときだけ入る（None は TOML に直さないため）。`commandWindows` は入らない。
  `additionalContextLimit` は PreToolUse・PostToolUse・SessionStart・UserPromptSubmit・SubagentStart で既定（2,500）と違うときだけ入り、他の event では捨てられる。
  `matcher` は event ごとの正規化（`matcher_pattern_for_event`）を通した値が入る。この関数は読んでおらず、matcher のある hook は測っていない。
  hash が合えば `Trusted`、key に別の hash があれば `Modified`、無ければ `Untrusted` で、`Trusted` か managed でない hook は動かない（`hook_trust_status` と `append_hook_events`）。
- この測定で使った計算（matcher・timeout・statusMessage を指定しない command の hook）:

  ```python
  # python3 -I hash.py <event label> <command> <timeout>
  ident = {"event_name": ev, "hooks": [{"type": "command", "command": cmd, "timeout": t, "async": False}]}
  s = json.dumps(ident, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
  print("sha256:" + hashlib.sha256(s.encode()).hexdigest())
  ```

- key に `.`（`config.toml`）を含むので、`-c hooks.state.<key>.trusted_hash=…` の点で区切る形は使わず、`-c 'hooks.state={"<key>"={trusted_hash="…"},…}'` の 1 つの inline table で渡した。
  点で区切る形を codex が引用符の key として解くかは試していない。

## 打った起動の引数と hook の script

`launch.sh <A|C>`（scratch の `bin/`。B は A と同じ引数で、scratch の project に `.codex/config.toml` を置いた）:

```sh
S=<scratch>          # pwd -P で解いた path
for pair in SessionStart:session_start:600 SessionEnd:session_end:1 Stop:stop:600 PreToolUse:pre_tool_use:600; do
  Ev=${pair%%:*}; rest=${pair#*:}; lab=${rest%%:*}; to=${rest#*:}
  cmd="sh $S/bin/hook.sh dagq $Ev"
  set -- "$@" -c "hooks.$Ev=[{hooks=[{type=\"command\",command=\"$cmd\"}]}]"
  h=$(python3 -I "$S/bin/hash.py" "$lab" "$cmd" "$to")
  state="$state${state:+,}\"/<session-flags>/config.toml:$lab:0:0\"={trusted_hash=\"$h\"}"
done
[ "$cond" = A ] && set -- "$@" -c "hooks.state={$state}"
cd "$S/repo"
exec ~/.local/bin/codex --no-daemon \
  -c "projects={\"$S/repo\"={trust_level=\"trusted\"}}" \
  -c 'model_reasoning_effort="low"' \
  -c 'check_for_update_on_startup=false' \
  "$@"
```

A の回に渡った `hooks.state`（実際の値）:

```text
hooks.state={"/<session-flags>/config.toml:session_start:0:0"={trusted_hash="sha256:8d33ed88d73d3485ae3d1d8820642940571d12980ec108eaafa67f3a9bb70ba0"},"/<session-flags>/config.toml:session_end:0:0"={trusted_hash="sha256:8f21f5d7ad8985c07396a7d191e5dc09febe69a7b12198a896a5dd9b00a9acef"},"/<session-flags>/config.toml:stop:0:0"={trusted_hash="sha256:47f07108b959f9a6f1703a7b585d91457c6ced0940ac3a843a8547b332e6f723"},"/<session-flags>/config.toml:pre_tool_use:0:0"={trusted_hash="sha256:e7f5fd1a6ac4cd8da468412e25026bc1d5a59f49cba15c551918082cfcbaa5c7"}}
```

起動の引数は回ごとに `args-<A|C>-<時刻>.txt` に残した（ファイル名の時刻が起動の時刻）: 回 1 は `args-C-034238`、回 2 は `args-A-034320`、回 3 は `args-A-034352`、回 4 は `args-A-034431`、回 5 は `args-A-034509`、回 6 は `args-C-035612`、回 7 は `args-A-035639`、回 8 は `args-A-035702`、回 9 は `args-A-035728`、回 10 は `args-A-035750`。
回 5 と回 10 は、`launch.sh` の `session_start` の hash だけを `sha256:0000` に替えた `launch-wrong.sh` で起動した。

`hook.sh` は `<時刻> <渡した側> <event> <stdin の JSON の先頭 400 byte>` を `hooks.log` に足すだけで、stdout に何も出さない。

B で scratch の project に置いた `.codex/config.toml`（起動の引数では渡さない hook）:

```toml
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "sh <scratch>/bin/hook.sh project SessionStart"
```

各回の操作（`turn.sh`。2 巡目は `round.sh` がこれを包む）: 起動して 8 秒後に画面を読む → trust の画面が出たら `3` と Enter → `Run the shell command \`echo probe\` and reply with its output only.` を送る（SessionStart は起動ではなく最初の turn の前に出るため、PreToolUse はコマンドの実行で出るため）→ 答えを待って `/quit`（SessionEnd）。

## 結果表

測定の記録は [codex-inbox-hook-trust-spike/](codex-inbox-hook-trust-spike/) に置いた。
この dir は 1 回きりの測定の出力で、設計の文書ではない。
`bin/` は測定の script、`args/` は回ごとの起動の引数、`hooks.log` は hook の log、`snap.log` は人の設定の更新時刻と sha256、`screens/` は 2 巡目の画面（`capture-pane -p` の出力）。
`bin/` の script と scratch の作り方（上の「環境と対象」）があれば、同じ版の codex で作り直せる。

C→A→B→A（再現）と試行を 2 巡流した。

- 1 巡目（回 1〜5）は、人の設定を全部の回の前（03:42:20）と各回の後に記録した。
  各回の後の記録は、次の回の起動より前に取った（回 1 の後は 03:43:15 で回 2 の起動は 03:43:20、回 2 の後は 03:43:45 で回 3 の起動は 03:43:52、回 3 の後は 03:44:23 で回 4 の起動は 03:44:31、回 4 の後は 03:44:53 で回 5 の起動は 03:45:09）。
  回 2〜5 の起動の直前の記録は取っていない。
  そこで、回 2〜5 の前の値には前の回の後の記録を当てた。その間に起動した Codex は無い。
- 2 巡目（回 6〜10）は、回ごとの起動の直前と、終わった直後に記録した（`round.sh`）。
  1 巡目に足りなかった回ごとの起動の前の記録を補うための巡で、画面も file に残した。

人の設定の値（全部の記録で同じだった）:

- `~/.codex/config.toml`: 更新時刻 1790953911（2026-10-03 00:11:51 JST）、sha256 `5d764192d9c005b1984f043a9c7471f147481224523c646ae789feb3e9829f0b`
- `~/.codex/auth.json`: 更新時刻 1792155657（2026-10-16 22:00:57 JST）、sha256 `4bf49f0e4e067515275330365a730ecb7f5a346daa7cb6ee16e056656a82c7c7`
- `~/.codex/hooks.json` は前後とも無い

| 回 | 条件 | 起動の引数（key と hash の求め方） | trust の画面の有無と文言 | 選んだ選択肢 | hook ごとの log の有無 | 人の config.toml と auth.json の sha256 と更新時刻が前後で一致したか（記録の時刻） |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | C（対照） | dagq の 4 hook を `-c hooks.<Event>`。`hooks.state` は渡さない | 出た: `Hooks need review` / `4 hooks are new or changed.` / `Hooks can run outside the sandbox after you trust them.` / `1. Review hooks` `2. Trust all and continue` `3. Continue without trusting (hooks won't run)` | 3 | 無し（`hooks.log` が作られない）。turn は動き、`echo probe` は実行された | 一致（前 03:42:20、後 03:43:15） |
| 2 | A | 回 1 に加え `-c hooks.state={…}`（key は `/<session-flags>/config.toml:<label>:0:0`、hash は上の計算） | 出ない（起動して入力欄 `› Ask Codex to do anything`） | なし | dagq の SessionStart（`source: startup`・`model: gpt-6.1-sol`）・PreToolUse・Stop・SessionEnd（`reason: other`）の 4 行 | 一致（前は回 1 の後の 03:43:15、後 03:43:45） |
| 3 | B | 回 2 と同じ。scratch の `repo/.codex/config.toml` に project の SessionStart の hook | 出た: `Hooks need review` / `1 hook is new or changed.`（以下同じ文言） | 3 | dagq の SessionStart・PreToolUse・Stop・SessionEnd の 4 行。project の SessionStart は無し | 一致（前は回 2 の後の 03:43:45、後 03:44:23） |
| 4 | A（再現の確認） | 回 2 と同じ（project の `.codex/` は消した） | 出ない | なし | dagq の 4 行（回 2 と同じ） | 一致（前は回 3 の後の 03:44:23、後 03:44:53） |
| 5 | 試行 | 回 2 のうち `session_start` の key の hash だけを `sha256:0000` にした | 出た: `Hooks need review` / `1 hook is new or changed.` | 3 | dagq の PreToolUse・Stop・SessionEnd の 3 行。SessionStart は無し | 一致（前は回 4 の後の 03:44:53、後 03:45:34） |
| 6 | C（対照） | 回 1 と同じ | 出た: `4 hooks are new or changed.`（`screens/run6-start.txt`） | 3 | 無し | 一致（前 03:56:12、後 03:56:38） |
| 7 | A | 回 2 と同じ | 出ない（`screens/run7-start.txt`） | なし | dagq の 4 行 | 一致（前 03:56:38、後 03:57:01） |
| 8 | B | 回 3 と同じ | 出た: `1 hook is new or changed.`（`screens/run8-start.txt`） | 3 | dagq の 4 行。project の SessionStart は無し | 一致（前 03:57:01、後 03:57:27） |
| 9 | A（再現の確認） | 回 4 と同じ | 出ない（`screens/run9-start.txt`） | なし | dagq の 4 行 | 一致（前 03:57:27、後 03:57:50） |
| 10 | 試行 | 回 5 と同じ | 出た: `1 hook is new or changed.`（`screens/run10-start.txt`） | 3 | dagq の PreToolUse・Stop・SessionEnd の 3 行。SessionStart は無し | 一致（前 03:57:50、後 03:58:16） |

- 1 巡目の画面の文言は、各回に `capture-pane -p` が返した出力をこの session で読んで写した。file には残していない。
  2 巡目の画面は `screens/` にある（`run<回>-start.txt` は起動から 8 秒後、`run<回>-end.txt` は turn と `/quit` の後）。
  `hooks.log` の 2 巡目は、回ごとに `=== run <回> start` の行で区切った。
- 起動は 10 回（上限 15 回）、時間は約 18 分（上限 60 分）。key と hash は source から求めたので、求め方を探る試行の起動は無い。
  回 5・10 は、key だけでなく hash が照合されることを確かめるために足した。
  2 巡目は反復の平均を取るためではなく、回ごとの起動の前の記録を補うためのもので、結果は 1 巡目と同じだった。
- 回 3・8 で「Continue without trusting」を選んでも、trust を与えた dagq の hook は動いた。画面の選択肢の「hooks won't run」は、trust されていない hook だけを指す。
- 回 3 の後、scratch の `repo/.codex/config.toml` は置いたときのまま（Codex は書き足していない）。

## 判定

**holds。**

根拠:

- 対照の C（回 1・6）では、起動の引数で渡した dagq の hook にも trust の画面が出て、hook は 1 つも動かなかった。
  A（回 2・4・7・9）では `hooks.state` の key と hash を起動の引数で渡しただけで、trust の画面が出ずに 4 つの hook が全部動いた（`hooks.log` の SessionStart・PreToolUse・Stop・SessionEnd）。
  違いは `-c hooks.state` の有無だけなので、trust は起動の引数で与えられた。
- B（回 3・8）では、起動の引数で渡していない project の hook 1 つだけが確認に残り（`1 hook is new or changed.`）、trust しなければ動かず、dagq の hook は動いた。
  試行（回 5・10）では、hash を変えた hook 1 つだけが確認に残った。
  trust は渡した key と hash の hook にだけ与えられ、有効な全 hook の trust を飛ばす形にならない。
- 人の `~/.codex/config.toml`・`auth.json` の更新時刻と sha256 は、全部の記録で変わらなかった。
  2 巡目は回ごとに起動の直前と終わった直後に記録し、5 回とも前後で一致した。
  1 巡目は、全部の回の前と各回の後に記録した（各回の前の値は、前の回の後の記録）。
  `hooks.state` を読む層は User と SessionFlags だけで（source）、`-c` の trust はその起動だけのもので人の設定に残らない。
- 条件: codex-cli 0.160.0、`--no-daemon` の対話の TUI、matcher の無い command の hook。
  key と hash の求め方は Codex の公開の設定の説明ではなく source の内部の実装（`hook_key`・`hook_hash`・`version_for_toml`）なので、Codex の版が上がって変われば hash が合わなくなりうる。
  そのときは、回 5・10 と同じく合わない hook が確認の画面に戻り、trust しなければ動かない（黙って動くことは無い）。

## 後続の計画への推奨

判定は holds なので、上の 1 つ目を採る。
下の 2 つ目は、Codex の版が上がって hash が合わなくなったときの戻り先として書く。

### holds のとき（今回）

- **task 3331**（Codex の起動の経路）: dagq が渡す hook ごとに、`-c hooks.<Event>=[…]` と、同じ起動の `-c 'hooks.state={"/<session-flags>/config.toml:<label>:<group>:<handler>"={trusted_hash="sha256:…"},…}'` を組み立てる。
  hash は渡す hook の正規化した形（event の label・matcher・command の文字列・timeout の既定（SessionEnd は 1、他は 600）・async・statusMessage・既定でない additionalContextLimit）から上の計算で求め、引数の組み立てと同じ値から作る（command の path を変えたら hash も変わる）。
  key に `.` を含むので、`hooks.state` は 1 つの inline table で渡す。
  `--dangerously-bypass-hook-trust` は使わない。
  hash の計算を、版を名指した既知の値（この文書の A の 4 つの値）と照らす unit test を置き、Codex の版が上がったら測り直す。
  起動の後に trust の画面が出ないことは manual-smoke で確かめる。
  trust は hook ごとの key と hash で決まるので、版が上がると一部の hook の hash だけが合わなくなりうる。
  そのとき、どの hook が合わないかは、画面からも SessionStart が届いたかからも分からない（3333・3334）。
  そのため、unit test の既知の値は hook ごとに置く。
- **task 3333**（区間と起動の記録）: trust が与えられれば SessionStart・SessionEnd は動く。
  ただし Codex の版が変わって hash が合わなくなると、合わない hook は画面に戻り、人が「Continue without trusting」を選べば動かない。
  trust は hook ごとに独立に決まり、一部の hook だけが動かないことがある。
  回 5・10 では SessionStart だけが動かず、PreToolUse・Stop・SessionEnd は動いた。
  逆に SessionStart だけが合い、他の hook が合わないこともありうる。
  起動の記録から、SessionStart が届かない区間（起動はあるが区間が開かない）を分かるようにしておく。
  SessionStart が届いたことで分かるのは SessionStart の hook が動いたことだけで、PreToolUse（guardrail）・Stop・SessionEnd の hook が動くことは分からない。
  trust の画面が出たかどうかでも、dagq の hook が動いたかは分からない（回 3・8 では画面が出たが、dagq の hook は動いた）。
  hook ごとの状態を確かめる手段を調べる。
  候補は、`discovery.rs` の `HookListEntry` が hook ごとに持つ `key`・`current_hash`・`trust_status` で、TUI の hooks/list がこの一覧を受けとるとみられる。
  dagq が起動の外からこれを読めるかは確かめていない。
  読めれば、dagq が渡す hook ごとに `trust_status` が Trusted か（dagq の計算した hash が `current_hash` と同じか）を確かめ、起動の記録に残す。
- **task 3334**（skill と manual-smoke）: trust の画面が出るのは、確認の済んでいない hook があるときと書く。
  dagq が渡す hook の hash が今の Codex と合わないとき（回 5・10）のほか、repository の `.codex/` や人の設定の hook など、dagq が渡していない hook が新しいか変わったとき（回 3・8）にも出る。
  画面の表示だけでは、どちらか、または両方かを決めない。
  trust は hook ごとに決まるので、dagq の hook の一部だけが合わないこともある。
  dagq の SessionStart が届いたことで確かめられるのは SessionStart の hook だけで、PreToolUse（guardrail）・Stop・SessionEnd の不一致は除外できない。
  どちらの場合も「Trust all and continue」は選ばない（人の `~/.codex/config.toml` に trust を書くとみられる。実行ファイルの文字列の `config/batchWrite failed while updating hook trust in TUI` からの推定で、選んで確かめてはいない）。
  手順は次のとおり。
  「Continue without trusting」で進める。
  確認に残った hook に dagq の hook が含まれないと確かめられるまでは、dagq の hook のどれかが動いていないとみなす。
  確かめる手段は、確認の対象の hook の一覧か、dagq の hook ごとの状態（3333 の候補）である。
  一覧は「1. Review hooks」で見られる見込みだが、この測定では開いていないので、manual-smoke で確かめる。
  確かめられないうちは guardrail と記録の hook が動かないおそれがあるので、`dagq inbox` を Claude で開き直すか報告する。
  確認に残ったのが dagq が渡していない hook だけだと確かめられたときだけ、人がその hook の中身を確かめて判断する。

### does_not_hold のとき（Codex の版が変わり、hash の求め方が変わって合わせられなくなったとき）

- **task 3331**: 起動の時の trust の確認を残し、人がその hook を確かめて認める（ADR-t1181-1 決定2）。
- **task 3333**: trust されず hook が動かない区間の欠けを、起動の記録から分かるようにする。
- **task 3334**: skill と manual-smoke に、人が trust の画面で hook を確かめて認める手順を書く。

### unresolved のとき（新しい版で測り直して結論が出なかったとき）

does_not_hold と同じ扱いで計画し直す。
3331 は起動の時の trust の確認を残し、3333 は hook が動かない区間の欠けを分かるようにし、3334 は人の trust の手順を書く。
そのうえで、どの版のどの条件が決まらなかったかを測り直しの task にする。

### guardrail と自動 compaction の Spike

task 3337（guardrail）と task 3339（自動 compaction）は、PreToolUse・PreCompact・PostCompact などの hook を、この文書と同じ `-c hooks.<Event>` と `-c hooks.state` の形で trust を与えて測れる。
`--dangerously-bypass-hook-trust` は要らない。
key の event の label は `pre_tool_use`・`pre_compact`・`post_compact`。

## 残ったもの

Codex 自身の記録（`~/.codex/sessions/2026/10/25/rollout-*.jsonl` の scratch の thread、`session_index.jsonl`・`history.jsonl` の行、`~/.codex/*.sqlite`）は Codex の状態で、人の設定ではない。
前の測定（[codex-inbox-spike](codex-inbox-spike.md)）と同じく、消していない。
scratch の dir は `$TMPDIR` の下に残した。
測定の script・起動の引数・`hooks.log`・`snap.log`・2 巡目の画面は、[codex-inbox-hook-trust-spike/](codex-inbox-hook-trust-spike/) に写した。
`t` は専用の socket の tmux の wrapper。
回 3 の project の `.codex/` は回 4 の前に消した。
tmux の server は止めた。
