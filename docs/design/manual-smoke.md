---
id: design-manual-smoke
type: design
title: Manual smoke of the paths that include real Claude
status: current
created: 2026-09-25
updated: 2026-09-29
last_verified: 2026-09-29
scope: operations
related:
  - adr-0036
  - design-supervisor-lifecycle
  - design-provider-lifecycle
---

# Manual smoke of the paths that include real Claude

実 Claude Code を含む経路は自動 test にしない（AGENTS.md の「テストの制約」）。`tests/e2e.rs` のハッピーパスは stub を provider にするので、実 Claude の起動・ダイアログ・resume と、異常系の組み合わせはこの手順で人（または人に頼まれた session）が確かめる。runtime の振る舞いを大きく変えたとき、Claude Code か cmux の版を上げたときに流す。結果は task の receipt の `summary`（または `note`）に、版・シナリオごとの結果・見つけた問題を残す。

手順は 3 つある。

- [故障経路のスモーク](#故障経路のスモーク): 使い捨て repository で異常系 6 シナリオを起こし、二重起動と成果の喪失が無いことを確かめる。
- [独立 task 1 件の完走](#独立-task-1-件の完走): この repository の本番 queue で、登録から着地まで人が DB を直さずに通ることを確かめる。
- [作業の内訳のスモーク](#作業の内訳のスモーク): 使い捨て repository で実 Claude Code の worker に background のコマンド・async の subagent・つないだコマンドをさせ、作業の内訳（task 514）の記録を transcript と突き合わせる。

## 故障経路のスモーク

### 隔離

本番 queue と固定バイナリ `~/.local/bin/dagq` を汚さないため、すべてを scratch directory に閉じる。

このスモークは人か inbox が行う。worker（dagq の run の session）は使い捨ての queue を作れず操作もできない（authorization の policy は worker に `queue.admin` を与えず、worker の状態を変えるコマンドは自分の run と task にしか効かない。[Authorization](authorization.md)、ADR-t728-1。task 983 で `dagq init` が `authorization_denied` になり、ask 195 で人が policy を変えないと決めた）。worker は env を外して迂回せず、実バイナリでの確認は integration test（`tests/it` の fixture）か e2e（`tests/e2e.rs`）に書き、実 queue での手の確認が要るものは receipt の `follow_ups` にして人か inbox に任せる。

- **バイナリ**: 確かめたい commit で `cargo build --locked` したものを scratch にコピーして使う。`target/` のバイナリで本番 queue を開かない（開いただけでは migrate しなくなった（ADR-0045 決定 5）が、状態を変えるコマンドで未着地の遷移を本番に持ち込まない。緩める範囲は ADR-0045 決定 18）。
- **repository**: `git init` した使い捨て repository。ディレクトリ名は `dagq-smoke` にする（task 710）。runtime は queue の cmux の workspace group を `[<repository のディレクトリ名>]`（例 `[dagq-smoke]`）、workspace の title を `[<ディレクトリ名>]worker#...` などと名付けるので、残った group がどのスモークのものか名前で分かる（`tests/e2e.rs` の fixture は `dagq-e2e` で、group は `[dagq-e2e]`）。`repo` のような汎用の名前にしない。`seed.txt`（検証コマンドが見る）、3 行の `shared.txt`（衝突用）、`CLAUDE.md`、`.claude/settings.json`（`permissions.defaultMode: auto`）を commit しておく。scratch に置いた bare repository を `origin` にする（supervisor の着地は push まで行い、`origin` が無いと `push_failed` の attention になる。手で `integrate` するときは `--no-push` でもよい）。
- **queue**: 全コマンドを `XDG_DATA_HOME=<scratch>/xdg` で、repository を cwd にして打つ（queue は `<scratch>/xdg/dagq/<hash>/queue.db` に解決される）。これを 1 行の wrapper script（例 `tq`）にしておく。
- **supervisor**: 専用の cmux workspace で `supervise --parallel 2 --claude <agent>` を起動し、`--log-dir` か `tee` で log を残す。`up` は使わない（inbox / planner の workspace と launchd agent を作るため）。`--once` は付けない。
- **folder trust**: 実 Claude を使う前に、使い捨て repository の root で一度 `claude` を起動して trust dialog を承認する。worktree で dialog が出るかは親 repository の root が信頼済みかで決まる（[provider-lifecycle](provider-lifecycle.md#trust-prompt)）。承認しないと、最初の承認より前に起動した run session がすべて dialog で止まる。

### stub と実 Claude の使い分け

- **stub**（`tests/e2e.rs` の `STUB` を拡張した shell script）で runtime の状態遷移を見るシナリオ（2〜5）を流す。token を使わず、時間を制御できる。拡張の例: title の `[break]` で `seed.txt` を消して commit し検証を壊す、`[hang]` で commit も receipt も書かずに待つ、`delay=N` で N 秒待ってから commit する。
- **実 Claude**（`--claude` に実体の path）で、Claude 自身の終了と resume が絡むシナリオ（1、6）を流す。cmux の terminal の PATH では session ごとの shim が先に解決されるので、`--claude ~/.local/bin/claude` のように実体を渡す（AGENTS.md の「起動と停止」と同じ理由）。
- stub は headless の review / triage（`claude -p`）にも使われる。`tests/e2e.rs` の stub は本文に `E2E-REVIEW-PASS` を含む task の review だけを pass にし、それ以外は失敗するので、stub のシナリオでは review が `review by hand`、triage が失敗の attention になるのが期待どおり。review と triage の verdict まで見たいシナリオは実 Claude で流す。検証は `integrate` の 1 回だけなので、review で止まった run は検証まで進まない。stub で着地まで流すシナリオ（2〜5）は task の description に `E2E-REVIEW-PASS` を入れて review を pass させるか、`review by hand` になった run に人が `integrate ID` を打つ。

### シナリオ

各シナリオで、`show ID`・`status`・`doctor` の出力、run のイベント列、保持されたリソース（`git worktree list`、run branch、run dir、cmux workspace）を記録する。runtime の振る舞いの正は [supervisor-lifecycle](supervisor-lifecycle.md) で、下の「確認点」はそれと食い違えば食い違いを問題として記録する。

| # | 起こすこと | agent | 確認点 |
| --- | --- | --- | --- |
| 1 | Claude の異常終了: commit した直後・receipt の前に、`doctor` の agent pid を `kill -KILL` | 実 Claude | wrapper が `session_exited`（signal 終了は exit 128）を記録し run は `failed`。worktree・branch・run dir が残る。復旧 job が retry / retry_inherit / resume / wait か escalate（`decide` の ask）を決める。並行する run と、空いた slot の次の claim に影響しない |
| 2 | supervisor の再起動: 2 run が `running` の間に supervisor を `kill -KILL` し、同じ workspace で新しい supervisor を起動 | stub | 新しい supervisor は wrapper が生きている run の stale lease を引き継ぎ（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）、run は receipt → 検証 → review まで進む。同じ task に 2 本目の run が立たない |
| 3 | 検証の失敗: `[break]` の task に `--verify 'test -f seed.txt'` | stub | 検証は `integrate` の 1 回だけで、失敗すると run は `needs_session` になり、supervisor が resume する（[`needs_session`](supervisor-lifecycle/needs-session.md#needs_session)）。3 回で解消しなければ `failed` と `decide` の ask。`main` は進まない。`tests/e2e.rs` の stub の resume は `set -eu` の下で `test -f seed.txt` を実行して非0で終わるので、拡張した stub の resume では `[break]` のときに `seed.txt` を戻す（解消を見る）か、壊したまま receipt を書き直す（回数上限を見る）かを決めておく |
| 4 | cleanup の失敗: review が pass して session が終わった（`session_exited`）後、supervisor が workspace を閉じる前に `cmux workspace close <uuid>` で閉じる | stub | supervisor の close は `not_found` で `cleanup_failed` イベントと `last_error` になり、run の状態は変わらず、着地も通る。supervisor は落ちない。worker の session は review の後まで開いたままなので（[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）、それより前に閉じると wrapper が死んでシナリオ 5 と同じ abandon の経路になる。窓は短いので、コマンド終了後に cmux が workspace を自動で閉じる環境（[観測済みの環境依存](#観測済みの環境依存)）ではこれが自然に起きる |
| 5 | 並列中の 1 run の異常: `[hang]` と `delay=60` の 2 task を同時に流し、`[hang]` の wrapper を `kill -KILL` | stub | heartbeat 切れ（30 秒）で `[hang]` の run だけが手放され（`runtime_error`、lease 削除）、`recover run` の attention になる。孤児になった agent が生きている間は `recover` が拒否し、agent を止めると supervisor が `interrupted` にして triage に回す。もう一方の run は影響なく review（`E2E-REVIEW-PASS` が無ければ `review by hand`）まで進む |
| 6 | merge queue の衝突: 2 task が `shared.txt` の同じ行を変える | 実 Claude | 先に着地した run の後、もう一方は着地前の `merge-tree` の事前判定か `integrate` の rebase で衝突を検出し、生きている（または resume した）session に解消を依頼する。session が rebase・解消・receipt の書き直しをして着地し、`main` は 1 task 1 commit の直線になる。review が `concern` を返したら `approve_landing` の ask に答える |

全シナリオの後に確かめること:

- 二重起動が無い（同じ task に同時に 2 本の未完了 run が無い）。
- 成果が失われていない（commit した run の branch か `refs/dagq/runs/<run-id>` が残る）。
- `doctor` の `unfinished_runs` と `run_leases` が空になり、`pgrep` で Claude・stub・wrapper が残っていない。
- 作った cmux workspace（supervisor と、閉じられずに残った run のもの）を `cmux workspace close <uuid>` で閉じる。
- 使い捨て queue の workspace group を `cmux workspace-group delete <group> --close-workspaces` で消す（`<group>` は `cmux --json workspace-group list` で name が `[<repository のディレクトリ名>]`、external ID が queue hash（queue の DB のあるディレクトリの名前）の group の id）。cmux は group を作るときに anchor の workspace を一緒に作るので、worker や supervisor の workspace を閉じるだけでは anchor が残って group が消えない。

### 観測済みの環境依存

- **workspace はコマンド終了後も残る**（cmux 0.64.25 (106)、2026-09-22）。`cmux workspace create --command` はコマンドをログインシェルに打ち込む形で起動し、wrapper が終わってもシェルと workspace は残る（`--command true` / `sleep 1` の probe で 5 秒以上残った）。同じ版の別の環境では 1〜2 秒後に workspace が自動で閉じたことがあり、cmux の設定に依存するとみられる。どちらでも supervisor の手順は同じ（[Cleanup and recovery](supervisor-lifecycle/cleanup-and-recovery.md#cleanup-and-recovery)）。
- **wrapper が SIGKILL で死ぬと** `run_processes.exited_at` は wrapper 行も agent 行も null のまま残る。`doctor` は PID の生死で補うので判定は変わらない。
- **未確認のまま残っているもの**: `exit_request_timed_out` の実機、検証コマンドの 30 分 timeout、`main` を進めた後の DB 更新失敗（[ADR-0008](../adr/0008-merge-queue-squash-landing.md) の既知の限界）。

## 独立 task 1 件の完走

この repository の本番 queue で、依存の無い小さな task（docs の誤りの修正など）を 1 件流し、登録から着地まで DB を手で直さずに通ることを確かめる。本番 queue の操作は固定バイナリ `~/.local/bin/dagq` だけで行う。

1. `which dagq` が `~/.local/bin/dagq` に解決し、`dagq --version` が確かめたい版であることを見る。固定バイナリの入れ替えが要るなら、AGENTS.md の「作業中」の手順で人に報告してから行う。
2. planner の session で task を 1 件登録する（`dagq add` に title・description・acceptance・`--verify`、docs だけなら `--paths`）。返った ID だけに `dagq ready` を打つ。
3. supervisor が居なければ inbox か planner の session から `dagq up --in-cmux ...`（AGENTS.md の「起動と停止」）。
4. 経過は `dagq show ID` と inbox の `watch` で見る。期待する流れ: claim → `[<repo>]worker#<task-id> - <title>` の workspace で worker が作業 → commit と receipt → validating → headless の review → pass なら `/exit`・workspace の close → `integrate`（rebase・検証・squash）→ push → task `completed`、run `integrated`。
5. 人の操作が要るのは、worker が止まったダイアログ（`answer_prompt` の ask）、review の `concern`（`approve_landing` の ask）、`review by hand`、`push_failed` だけで、どれも inbox に届く。それ以外で止まったら詰まりとして記録する。
6. 着地した commit（`Dagq-Task` / `Dagq-Run` trailer）、`refs/dagq/runs/<run-id>`、worktree と branch の削除、`origin/main` への push を確かめ、所要時間と詰まりを残す。

2026-09-22 の初回（固定バイナリ 0.1.0、README の手順の追記 1 件）は、登録から着地まで約 5 分、DB を手で直さずに通った（当時は review と `integrate` を常駐 session が手で行った）。worktree が信頼済みの repository のものだったので trust dialog は出なかった。

## 作業の内訳のスモーク

run の session の作業の内訳（task 514。[provider-lifecycle](provider-lifecycle.md#作業の内訳)、[stats](supervisor-lifecycle/stats.md#作業の内訳)、[timeline](supervisor-lifecycle/timeline.md)）は fixture の transcript だけで test されているので、Claude Code の版を上げたとき、または transcript の読み取り（`domain::transcript`）と分類（`domain::worktime`）を変えたときに、実 Claude Code の transcript と突き合わせる。

### 手順

1. [隔離](#隔離)のとおり scratch に使い捨て repository（`cargo init --lib` の crate に、15 秒 sleep する `#[test]` を 1 本足して commit。background の `cargo test` に時間がかかるようにする）、bare の `origin`、`XDG_DATA_HOME=<scratch>/xdg` と scratch のバイナリで打つ wrapper（`tq`）を用意し、trust dialog を承認しておく。wrapper は `env -u DAGQ_ROLE -u DAGQ_QUEUE -u DAGQ_ACTOR_ID -u DAGQ_RUN_ID -u DAGQ_TASK_ID` を付ける（inbox や planner の session の terminal は本番 queue の `DAGQ_ROLE`（`inbox` / `planner`）と `DAGQ_QUEUE` を workspace の env に持つので、そのまま打つと scratch の queue の操作がその role の権限と記録で扱われる。人が scratch の queue を自分（`user`）として操作するために外すもので、worker の拒否を迂回するためのものではない。このスモークを worker は行わない（[隔離](#隔離)））。
2. `tq add` で task を 1 件登録し（`--verify 'cargo test'`、`--kind runtime`）、`tq ready ID --bypass-review`。description で worker に順番どおり次をさせる: (a) `cargo build`、`cargo test`、`cargo test --test does_not_exist`（失敗する）をそれぞれ `run_in_background: true` で起動して完了の通知を待つ、(b) Agent tool を `run_in_background: true` で起動して完了の通知を待つ、(c) foreground で 15 秒以上かかるコマンド（`cargo test --release`）を実行し、その間に人が Ctrl-B で background に移す、(d) foreground で `cargo fmt && cargo test`、(e) commit と receipt。`sleep N && ...` は Claude Code の harness が `Blocked: sleep N followed by: ...` で拒むので使わない。
3. 専用の cmux workspace で `tq supervise --parallel 1 --claude <実体の path> --log-dir ... --observe-interval 0 --observe-daily false --report-daily false --forecast-snapshots false --host-metrics-interval 0` を起動する。
4. run が閉じたら次を集める: `tq events --all --full --run RUN`（`session_closed` の `work` と `session_exited` の `work_breakdown`）、run dir の `worktime.jsonl`、`tq stats --full`（`runs[].work_breakdown` と `overall.work_breakdown`）、`tq timeline RUN`（`commands`）、session の transcript（`~/.claude/projects/<cwd を符号化した名前>/<session_id>.jsonl`。worker の session_id は run ID と同じ）。
5. transcript の `tool_use` の時刻（開始）と、対の `tool_result` か `<task-notification>` の `queue-operation`（`enqueue`）の時刻（終わり）を、`worktime.jsonl` と `work.heavy` の `start` / `end`・`background`・`finished`・`failed`・`exit_code`・`status` と比べる。`secs` の合計が区間の長さ（`session_opened` から `session_closed`）と一致すること、`stats` と `timeline` が `session_closed.work` と同じ値を出すことも見る。
6. [故障経路のスモーク](#シナリオ)の後始末と同じく supervisor を止め、workspace group を消す。

### 結果（2026-09-28、Claude Code 2.1.283、cmux 0.64.25 (106)、dagq 0.4.0-dev+bdb0259）

worker の区間 1 つ（166 秒、`active_secs` 114）。`session_closed.work` と `session_exited.work_breakdown` は `kind` / `attempt` を除いて同じ値で、`stats --full` の `runs[].work_breakdown`・`overall.work_breakdown`（`runs` 1、`runs_with_repeats` 1）と `timeline` の `commands`（9 行、`event` は `session_closed` の id）もそれと一致した。`secs` の合計（model 57・test 52・idle 38・subagent 11・build 4・chain / git / other_command / tool 各 1）は区間の長さと一致した。

| シナリオ | transcript | 記録 | 一致 |
| --- | --- | --- | --- |
| background の `cargo build` / `cargo test` | `tool_use`（`run_in_background: true`）→ すぐ `tool_result`（`Command running in background with ID: <id>`、`toolUseResult.backgroundTaskId`）→ 完了の `queue-operation` `enqueue` | `background: true`、`end` は `enqueue` の時刻、`status: completed`・`exit_code: 0`・`failed: false`、`finished: true`。`cargo test`（17 秒）は `full_tests` と `verification_repeats` に数えた | 一致 |
| background の失敗（`cargo test --test does_not_exist`） | 通知の `<status>failed</status>`、summary `... failed with exit code 101` | `status: failed`・`exit_code: 101`・`failed: true`、`commands.test.failed` 1。target を選ぶので `verification_repeats` に数えない | 一致 |
| async の Agent | `tool_result` は `Async agent launched successfully ...`（`isAsync: true`）。報告は先に `<agent-message from="<agent id>">` の `queue-operation`（`enqueue` / `dequeue`）で届き、約 2 秒後に `<task-notification>` の `enqueue`、続けて同じ内容の `remove` が書かれた（`user` の `<task-notification>` は書かれなかった）。subagent の transcript は main の JSONL に sidechain として入らず `<session_id>/subagents/` に分かれる | `subagent`、`background: true`、`end` は `<task-notification>` の `enqueue`（11 秒）、`status: completed`、`finished: true` | 一致（終わりは報告の到着ではなく通知の `enqueue`） |
| Ctrl-B で background に移したコマンド | `tool_result` が `Command was manually backgrounded by user with ID: <id>`、`toolUseResult.backgroundedByUser: true`。その後に通常の background と同じ通知（`queue-operation` の `enqueue` / `dequeue` と `user` の `<task-notification>`） | worker の run では移せなかった（下の注）。別の対話 session で transcript の形だけ確かめ、runtime の規則（shell の `tool_use` に通知があれば通知までの background）が当てはまる形であることを確かめた | 形は規則どおり（runtime の記録では未確認） |
| timeout で background に移ったコマンド | `tool_result` が `Command did not complete within its 600s timeout and was moved to the background (ID: <id>)`、`toolUseResult.timedOutAfterMs`。通知は `queue-operation` の `enqueue` と `remove` で、`user` の `<task-notification>` は書かれず、同じ時刻に `attachment` の record が書かれた | runtime の記録では未確認（この smoke を見ていた session の transcript で形だけ確かめた） | 形は規則どおり |
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
- **Ctrl-B**: `cmux send-key --workspace <worker> ctrl+b` は `OK` を返したが、コマンドは background に移らなかった。pty に `\x02` を書くと移った。
- **worker の `dagq`**: worker の workspace は supervisor の `XDG_DATA_HOME` を継がず、PATH の `dagq` は固定バイナリ `~/.local/bin/dagq` に解決する。worker の最初の `dagq ask` は既定の `~/.local/share/dagq/<hash>/queue.db`（存在しない）を開こうとして失敗し、worker は `XDG_DATA_HOME` を付けて打ち直した（固定バイナリが scratch の queue に ask を書いた。本番 queue には触れていない）。worker に `dagq` を打たせるシナリオでは、使い捨て repository の `dagq.toml` の `[run.env]` で `XDG_DATA_HOME` と scratch のバイナリを先にした `PATH` を渡す（未確認）か、task の description に wrapper の path を書く。
