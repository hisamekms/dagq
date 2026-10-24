---
id: development-manual-smoke
type: development
title: 実ClaudeとCodexを含む経路の手動スモークの手順（故障経路・独立taskの完走・作業の内訳・非対話のworker・非対話のClaudeの前提・Codexのgoal reviewとrun review・Codexのinbox・runのreviewの実CLI・他のrepository）
status: current
created: 2026-10-13
owners:
  - hisamekms
tags:
  - testing
  - operations
related:
  - development-testing
  - development-operations
  - plan-manual-smoke-results
  - plan-codex-inbox-spike
  - design-provider-lifecycle
  - adr-t813-1
  - adr-t813-3
  - adr-t1233-2
---

# 実ClaudeとCodexを含む経路の手動スモーク

実 Claude Code と実 Codex CLI を含む経路は自動 test にしない（[testの制約](testing.md)の「手動スモーク」）。
`tests/e2e.rs` のハッピーパスは stub を provider にするので、実 Claude と実 Codex の起動・resume と、異常系の組み合わせはこの手順で人（または人に頼まれた inbox）が確かめる。
runtime の振る舞いを大きく変えたとき、Claude Code か Codex CLI の版を上げたときに流す。
結果は依頼した task の receipt の `summary`（または `note`）に、版・シナリオごとの結果・見つけた問題を残す。
これまでの結果の記録は [manual-smoke-results](../plans/manual-smoke-results.md) にある。

手順は次のとおり。

- [故障経路のスモーク](#故障経路のスモーク): 使い捨て repository で異常系 5 シナリオを起こし、二重起動と成果の喪失が無いことを確かめる。
- [独立 task 1 件の完走](#独立-task-1-件の完走): この repository の本番 queue で、登録から着地まで人が DB を直さずに通ることを確かめる。
- [作業の内訳のスモーク](#作業の内訳のスモーク): 使い捨て repository で実 Claude Code の worker に background のコマンド・async の subagent・つないだコマンドをさせ、作業の内訳の記録を transcript と突き合わせる。
- [非対話の worker のスモーク](#非対話の-worker-のスモーク): 使い捨て repository で実 Codex と実 Claude の非対話の worker を 1 本ずつ着地させ、turn の記録と provider・経路の記録を確かめる。
- [非対話の Claude の前提の確認](#非対話の-claude-の前提の確認): queue を使わずに実 `claude -p` を起動し、非対話の経路が前提にする CLI の振る舞いを確かめる。
- [Codex の goal review のスモーク](#codex-の-goal-review-のスモーク): 使い捨て repository で goal review を実 Codex で動かし、launch と sandbox と記録を確かめる。
- [Codex の run review のスモーク](#codex-の-run-review-のスモーク): 使い捨て repository で通常 run の review を実 Codex で動かし、verdict から着地まで確かめる。
- [Codex の inbox のスモーク](#codex-の-inbox-のスモーク): 使い捨て repository の queue に対して人が自分の terminal で Codex の inbox を開き、watch・ask の answer・復帰・区間の記録を確かめる。
- [run の review の実 CLI の確認](#runのreviewの実cliの確認): ignored の IT で run の review の job の制限を実 Claude で確かめる。
- [他の repository のスモーク](#他の-repository-のスモーク): dagq のソースでない使い捨て repository（default branch が `master`、`origin` なし、`Cargo.toml` も `AGENTS.md` も無い）で、`cargo install` したバイナリと公式の手順で入れた plugin を使い、`up`・計画の依頼から着地までを通す。

## 故障経路のスモーク

### 隔離

本番 queue と固定バイナリ `~/.local/bin/dagq` を汚さないため、すべてを scratch directory に閉じる。

このスモークは人か inbox が行う。
worker が使い捨ての queue を操作できない理由と、worker が代わりにすることは[運用の開発文書](operations.md)の「workerがhostと実queueでできないこと」が持つ。

- **バイナリ**: 確かめたい commit で `cargo build --locked` したものを scratch にコピーして使う（本番 queue と開発中のバイナリの境界は AGENTS.md の「本番 queue と開発環境の境界」）。
- **repository**: `git init` した使い捨て repository。
  ディレクトリ名は `dagq-smoke` にする。
  残ったものがどのスモークのものか名前で分かるよう、`repo` のような汎用の名前にしない（`tests/e2e.rs` の fixture は `dagq-e2e`）。
  `seed.txt`（検証コマンドが見る）、3 行の `shared.txt`（衝突用）、`CLAUDE.md`、`.claude/settings.json`（`permissions.defaultMode: auto`）を commit しておく。
  scratch に置いた bare repository を `origin` にする（supervisor の着地は push まで行い、`origin` が無いと `push_failed` の attention になる。手で `integrate` するときは `--no-push` でもよい）。
- **queue**: 全コマンドを `XDG_DATA_HOME=<scratch>/xdg` で、repository を cwd にして打つ（queue は `<scratch>/xdg/dagq/<hash>/queue.db` に解決される）。
  これを 1 行の wrapper script（例 `tq`）にしておく。
- **supervisor**: 専用の terminal で `supervise --parallel 2 --claude <agent>` を起動し、`--log-dir` か `tee` で log を残す。
  `up` は使わない（launchd agent を作るため）。
  `--once` は付けない。
- **folder trust**: inbox のように対話で開く session を使うときは、先に使い捨て repository の root で一度 `claude` を起動して trust dialog を承認する（worktree で dialog が出るかは親 repository の root が信頼済みかで決まる。[provider-lifecycle](../design/provider-lifecycle.md#trust-prompt)）。
  非対話の worker と headless の job はこの dialog に当たらない。

#### scratch の worker が打つ `dagq` の環境

worker の `dagq` はクライアントモードで動く（[Queue service](../design/queue-service.md#クライアントモード)）。
worker の agent には queue の path（`DAGQ_QUEUE`）は渡らず、scratch の supervisor が渡す queue service の socket（`DAGQ_SERVICE_SOCKET`）と worker の token の file（`DAGQ_SERVICE_CREDENTIAL_FILE`）だけが渡るので、worker の `dagq` は `XDG_DATA_HOME` にも cwd にも依らず、その supervisor の queue の service に届く。
`--once` でない `supervise` は queue の service を起動して見張る（無ければ `tq service start`）。
worker の `dagq` に scratch のバイナリを使わせるには、下の `PATH` が要る。

人か inbox が `dagq-smoke` に立てる worker には、使い捨て repository の main checkout の `dagq.toml` の `[run.env]` で `XDG_DATA_HOME` と、scratch の bin を先頭にした `PATH` を渡せる。
supervisor を起動する terminal の env だけには頼らない。

- `[run.env]` の値は `${DAGQ_QUEUE_DIR}` と `${DAGQ_RUN_DIR}` だけが展開され、`$PATH`・`${PATH}`・`$HOME`・`~` は展開されないので、PATH は先頭に足す部分も既存の部分も、展開済みの絶対パスで書く。
- main checkout の `[run.env]` は worker の session wrapper の環境まで渡り、その後に `XDG_DATA_HOME` や `PATH` を上書きする処理は無いので、PATH の値を丸ごと指定して scratch の bin を先頭にできる（入口は `ShellVerifier::run_env` と `BackgroundWrappers::launch`）。
  wrapper はログインシェルを通らずに background で起動し、supervisor の環境のうち terminal の pane と dagq の変数は渡さない（`BackgroundWrappers::launch` の doc comment）。
- `[run.env]` は `DAGQ_` で始まる key を拒むので、actor の変数（`DAGQ_ROLE`・`DAGQ_RUN_ID` など）は上書きできない。

人か inbox が行う設定と確認:

1. 確認対象のバイナリを `<scratch>/bin/dagq` に置き、`dagq-smoke/dagq.toml` に次を足して commit する。
   `<scratch>` と `<既存のPATHの展開済みの値>` は実際の絶対パスに置き換える。
   後者には cargo・git などを解決する元の PATH 全体を残す。
   supervisor も同じ `<scratch>/xdg` と scratch のバイナリで起動する。

   ```toml
   [run.env]
   XDG_DATA_HOME = "<scratch>/xdg"
   PATH = "<scratch>/bin:<既存のPATHの展開済みの値>"
   ```

2. task の description に、開始時に `command -v dagq`、`printenv XDG_DATA_HOME PATH DAGQ_ROLE DAGQ_QUEUE DAGQ_RUN_ID DAGQ_SERVICE_SOCKET DAGQ_SERVICE_CREDENTIAL_FILE` と `dagq locate` を記録するよう書く。
   scratch の worker 自身の run に対する `dagq ask --run "$DAGQ_RUN_ID" --kind worker_question --because scope --topic other --question 'スモークを続けてよいか'` と、answer 後の再開も指定する。
   queue の作成・登録・起動・answer・後始末は人か inbox が行い、worker は自分の task / run の許された操作だけを行う。
3. 人か inbox が `dagq-smoke` で上記の出力を集める。
   `command -v` が `<scratch>/bin/dagq`、XDG が `<scratch>/xdg`、role が `worker`、`DAGQ_QUEUE` が空で、`DAGQ_SERVICE_SOCKET` が人側の `tq service status` の `socket` と同じであること、`dagq locate` が `client_mode: true` とその socket を返す（`db` は null）ことを見る。
   その queue の `tq events --all --full --run RUN` に worker の ask が入り、人か inbox の answer で再開すれば、scratch の worker が scratch のバイナリで自分の queue に打てたと判定する。
   版・run ID・解決先・ask の ID を結果に残す。
4. 一致しない場合は、原因と実際の PATH を残し、description で `<scratch>/bin/dagq` を絶対パスで指定する。
   クライアントモードの worker の `dagq` は `--db` を `queue_named` で拒むので、`--db` を足す worker 用 wrapper は使えない。
   人用の `env -u ...` を含む `tq` を worker に使わせない。

### stub と実 Claude の使い分け

- **stub**（`tests/e2e/stub.rs` の `STUB` を拡張した shell script）で runtime の状態遷移を見るシナリオ（2〜4）を流す。
  token を使わず、時間を制御できる。
  拡張の例: title の `[break]` で `seed.txt` を消して commit し検証を壊す、`[hang]` で commit も receipt も書かずに待つ、`delay=N` で N 秒待ってから commit する。
- **実 Claude**（`--claude` に実体の path）で、Claude 自身の終了と resume が絡むシナリオ（1、5）を流す。
  terminal によっては PATH で pane ごとの shim が先に解決されるので、`--claude ~/.local/bin/claude` のように実体を渡す（理由は plugin の `dagq-recover` skill の `reference/up-down.md` の「up」）。
- stub は headless の review / triage（`claude -p`）にも使われる。
  `tests/e2e.rs` の stub は本文に `E2E-REVIEW-PASS` を含む task の review だけを pass にし、それ以外は失敗するので、stub のシナリオでは review が `review by hand`、triage が失敗の attention になるのが期待どおり。
  review と triage の verdict まで見たいシナリオは実 Claude で流す。
  検証は `integrate` の 1 回だけなので、review で止まった run は検証まで進まない。
  stub で着地まで流すシナリオ（2〜4）は task の description に `E2E-REVIEW-PASS` を入れて review を pass させるか、`review by hand` になった run に人が `integrate ID` を打つ。
- e2e: worker（stub も実 Claude も実 Codex も）は e2e を流さず、receipt の `e2e` は理由つきの `not_applicable` になる。
  e2e が要る run には、review の pass の後、着地の前に supervisor が host で e2e を流す（[着地の前のe2e](../design/supervisor-lifecycle/integrate.md#着地の前のe2e)）。
  使い捨て repository は dagq のソースでないので runtime の知る e2e が無く、要る run は e2e が無いことを記録して着地へ進む。
  `dagq.toml` を置かない使い捨て repository では `[e2e] paths` が無く、`--evidence e2e` を付けない task は e2e が要らない。
  この工程そのもの（host での実行・1 本ずつの lock・落ちたときの `needs_session` と resume・流せないときの流し直し）は `tests/it/runtime_e2e.rs` が stub のコマンドで確かめる。
  本番の固定バイナリの入れ替えの前には、自動更新と `install` の関門が全部の e2e を流す（[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[install](../design/supervisor-lifecycle/install.md)）。

### シナリオ

各シナリオで、`show ID`・`status`・`doctor` の出力、run のイベント列、保持されたリソース（`git worktree list`、run branch、run dir）を記録する。
runtime の振る舞いの正は [supervisor-lifecycle](../design/supervisor-lifecycle.md) で、下の「確認点」はそれと食い違えば食い違いを問題として記録する。

| # | 起こすこと | agent | 確認点 |
| --- | --- | --- | --- |
| 1 | Claude の異常終了: commit した直後・receipt の前に、`doctor` の agent pid を `kill -KILL` | 実 Claude | wrapper が `session_exited`（signal 終了は exit 128）を記録し run は `failed`。worktree・branch・run dir が残る。復旧 job が retry / retry_inherit / resume / wait か escalate（`decide` の ask）を決める。並行する run と、空いた slot の次の claim に影響しない |
| 2 | supervisor の再起動: 2 run が `running` の間に supervisor を `kill -KILL` し、同じ queue で新しい supervisor を起動 | stub | 新しい supervisor は wrapper が生きている run の stale lease を引き継ぎ（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）、run は receipt → 検証 → review まで進む。同じ task に 2 本目の run が立たない |
| 3 | 検証の失敗: `[break]` の task に `--verify 'test -f seed.txt'` | stub | 検証は `integrate` の 1 回だけで、失敗すると run は `needs_session` になり、supervisor が resume する（[`needs_session`](../design/supervisor-lifecycle/needs-session.md#needs_session)）。3 回で解消しなければ `failed` と `decide` の ask。`main` は進まない。`tests/e2e.rs` の stub の resume は `set -eu` の下で `test -f seed.txt` を実行して非0で終わるので、拡張した stub の resume では `[break]` のときに `seed.txt` を戻す（解消を見る）か、壊したまま receipt を書き直す（回数上限を見る）かを決めておく |
| 4 | 並列中の 1 run の異常: `[hang]` と `delay=60` の 2 task を同時に流し、`[hang]` の wrapper を `kill -KILL` | stub | heartbeat 切れで `[hang]` の run だけが手放され（`runtime_error`、lease 削除）、`recover run` の attention になる。孤児になった agent が生きている間は `recover` が拒否し、agent を止めると supervisor が `interrupted` にして triage に回す。もう一方の run は影響なく review（`E2E-REVIEW-PASS` が無ければ `review by hand`）まで進む |
| 5 | merge queue の衝突: 2 task が `shared.txt` の同じ行を変える | 実 Claude | 先に着地した run の後、もう一方は着地前の `merge-tree` の事前判定か `integrate` の rebase で衝突を検出し、生きている（または resume した）session に解消を依頼する。session が rebase・解消・receipt の書き直しをして着地し、`main` は 1 task 1 commit の直線になる。review が `concern` を返したら `approve_landing` の ask に答える |

全シナリオの後に確かめること:

- 二重起動が無い（同じ task に同時に 2 本の未完了 run が無い）。
- 成果が失われていない（commit した run の branch か `refs/dagq/runs/<run-id>` が残る）。
- `doctor` の `unfinished_runs` と `run_leases` が空になり、`pgrep` で Claude・stub・wrapper が残っていない。

### 後始末

専用の terminal の `supervise` を止め、上の「全シナリオの後に確かめること」を見る。
wrapper が SIGKILL で死んだ run は `run_processes.exited_at` が null のまま残ることがあるが、`doctor` は PID の生死で補うので判定は変わらない。

## 独立 task 1 件の完走

この repository の本番 queue で、依存の無い小さな task（docs の誤りの修正など）を 1 件流し、登録から着地まで DB を手で直さずに通ることを確かめる。
本番 queue の操作は固定バイナリ `~/.local/bin/dagq` だけで行う。

1. `which dagq` が `~/.local/bin/dagq` に解決し、`dagq --version` が確かめたい版であることを見る。
   固定バイナリの入れ替えが要るなら、[運用の開発文書](operations.md)の「本番queueと固定バイナリ」のとおり人に報告してから行う。
2. supervisor が居なければ人か inbox が `dagq up ...`（[運用の開発文書](operations.md)の「`up`のコマンド」）。
3. inbox が `dagq request add` で runtime の planner に task を 1 件頼む（title・description・acceptance・`--verify`、docs だけなら `--paths` を planner が決める）。
   supervisor が立てた planner が登録し、plan review が pass して task が ready になるのを見る。
4. 経過は `dagq show ID` と inbox の `watch` で見る。
   期待する流れ: claim → background の session wrapper で worker が作業 → commit と receipt → validating → headless の review → pass なら終了の依頼・wrapper の停止 → `integrate`（rebase・検証・squash）→ push → task `completed`、run `integrated`。
   worker は非対話の turn で動くので、turn の要約は `dagq run log RUN --follow` で読む。
5. 人の操作が要るのは、worker の `worker_question`、review の `concern`（`approve_landing` の ask）、`review by hand`、`push_failed` だけで、どれも inbox に届く。
   ask は inbox が `dagq answer` で答え、worker への答えは supervisor が次の turn として届ける（worker に打ち込まない）。
   それ以外で止まったら詰まりとして記録する。
6. 着地した commit（`Dagq-Task` / `Dagq-Run` trailer）、`refs/dagq/runs/<run-id>`、worktree と branch の削除、`origin/main` への push を確かめ、所要時間と詰まりを残す。

## 作業の内訳のスモーク

run の session の作業の内訳（[provider-lifecycle](../design/provider-lifecycle.md#作業の内訳)）は fixture の transcript だけで test されているので、Claude Code の版を上げたとき、または transcript の読み取り（`domain::transcript`）と分類（`domain::worktime`）を変えたときに、実 Claude Code の transcript と突き合わせる。

### 手順

1. [隔離](#隔離)のとおり scratch に使い捨て repository（`cargo init --lib` の crate に、15 秒 sleep する `#[test]` を 1 本足して commit。background の `cargo test` に時間がかかるようにする）、bare の `origin`、`XDG_DATA_HOME=<scratch>/xdg` と scratch のバイナリで打つ wrapper（`tq`）を用意する。
   wrapper は `env -u DAGQ_ROLE -u DAGQ_QUEUE -u DAGQ_ACTOR_ID -u DAGQ_RUN_ID -u DAGQ_TASK_ID` を付ける（inbox の session の terminal は本番 queue の `DAGQ_ROLE`（`inbox`）と `DAGQ_QUEUE` を session の env に持つので、そのまま打つと scratch の queue の操作がその role の権限と記録で扱われる。
   人が scratch の queue を自分（`user`）として操作するために外すもので、worker の拒否を迂回するためのものではない。
   このスモークを worker は行わない（[隔離](#隔離)））。
2. `tq add` で task を 1 件登録し（`--verify 'cargo test'`、`--change feature`）、`tq ready ID --bypass-review`。
   description で worker に順番どおり次をさせる: (a) `cargo build`、`cargo test`、`cargo test --test does_not_exist`（失敗する）をそれぞれ `run_in_background: true` で起動して完了の通知を待つ、(b) Agent tool を `run_in_background: true` で起動して完了の通知を待つ、(c) foreground で 15 秒以上かかるコマンド（`cargo test --release`）を実行する、(d) foreground で `cargo fmt && cargo test`、(e) commit と receipt。
   `sleep N && ...` は Claude Code の harness が `Blocked: sleep N followed by: ...` で拒むので使わない。

   worker は非対話の turn で動くので、人は worker に打てず、foreground のコマンドを Ctrl-B で background に移すことは起こせない。
   background のコマンドと subagent は同じ turn の中で完了を待たせる（turn の終わりに残ったものは止められる。[非対話のworker](../design/supervisor-lifecycle/headless-worker.md#wrapperがturnを止めるとき)）。
   経過は `tq run log RUN --follow` で読み、worker の ask には `tq answer` で答える。
3. 専用の terminal で `tq supervise --parallel 1 --claude <実体の path> --log-dir ... --observe-interval 0 --observe-daily false --report-daily false --forecast-snapshots false --host-metrics-interval 0` を起動する。
4. run が閉じたら次を集める: `tq events --all --full --run RUN`（`session_closed` の `work` と `session_exited` の `work_breakdown`）、run dir の `worktime.jsonl`、`tq stats --full`（`runs[].work_breakdown` と `overall.work_breakdown`）、`tq timeline RUN`（`commands`）、session の transcript（`~/.claude/projects/<cwd を符号化した名前>/<session_id>.jsonl`。worker の session_id は run ID と同じ）。
5. transcript の中では tool-use-id で `tool_use`（開始）と `tool_result` / 完了通知を結び、`tool_use` の開始時刻・category・コマンドで `worktime.jsonl` の行と対応づける。
   `worktime.jsonl` と `session_exited.work_breakdown.heavy` の `start` / `end`・`background`・`finished`・`failed` を transcript と比べ、`exit_code`・`status` は `worktime.jsonl` で比べる（`worktime.jsonl` の行にも event の `heavy` にも tool-use-id は無く、`heavy` には `exit_code`・`status` も無い）。
   `secs` の合計が区間の長さ（`session_opened` から `session_closed`）と一致すること、`stats` と `timeline` が `session_closed.work` と同じ値を出すことも見る。
6. [後始末](#後始末)と同じく supervisor を止める。

## 非対話の worker のスモーク

非対話の worker の経路（[ADR-t813-1](../adr/2026-09-28-t813-1-headless-worker-path.md)、[非対話のworker](../design/supervisor-lifecycle/headless-worker.md)）と Codex の worker（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)、[provider-lifecycle](../design/provider-lifecycle.md#codexの非対話のworker)）は、stub の provider でだけ自動 test される（stub の turn は sandbox なしで走る）。
実物の CLI の出力の形・認証・sandbox・session（thread）の resume は stub では確かめられないので、Claude Code か Codex CLI の版を上げたとき、turn の組み立て（`turn_command`）か出力の読み手（`ClaudeTurnReader`・`CodexTurnReader`）か Codex の権限（`-c` と rules）を変えたときに、実 Codex と実 Claude の非対話の worker を 1 本ずつ使い捨て repository で着地させる。
人か inbox が行う（worker は使い捨ての queue を作れない。[隔離](#隔離)）。

### 手順

1. [隔離](#隔離)のとおり scratch に使い捨て repository `dagq-smoke`（`seed.txt` と `CLAUDE.md` を commit）、bare の `origin`、確かめたい commit の scratch のバイナリ、`env -u DAGQ_ROLE -u DAGQ_QUEUE -u DAGQ_ACTOR_ID -u DAGQ_RUN_ID -u DAGQ_TASK_ID XDG_DATA_HOME=<scratch>/xdg <scratch のバイナリ>` の wrapper（`tq`）を用意し、`tq init` する。
   非対話の経路は folder trust の dialog に当たらない（[provider-lifecycle](../design/provider-lifecycle.md#workerのproviderと経路)）ので、trust の承認は要らない。
2. 前提を確かめて記録する: `~/.local/bin/claude --version`、`~/.local/bin/codex --version`、`codex login status`（ログイン済み）、Claude のログイン（`claude -p 'say ok'` が答える）。
   `~/.codex/config.toml`・`~/.codex/auth.json`・`~/.claude` は書き換えない（runtime も書かない）。
3. task を 2 件登録して `ready` にする。
   どちらも `--verify 'test -f seed.txt'` と、description に「`smoke-<provider>.txt` に 1 行足して commit し、receipt を書く」。
   - Codex: `tq add "codex headless smoke" --provider codex ...`
   - Claude: `tq add "claude headless smoke" --headless ...`
   - どちらの description にも「最初に `dagq ask --run $DAGQ_RUN_ID --kind worker_question --because scope --topic other --question '続けてよいか'` を打って turn を終え、答えを受けてから作業する」を足し、answer の turn（同じ session か thread の resume）も通す。
     Codex の worker の sandbox の中の `dagq ask` は queue を開かず、クライアントモードで queue service に送られ、service が worker の principal で判定してその場で ask を開く（[Queue service](../design/queue-service.md#クライアントモード)。turn には queue service の socket と token の file が渡る）。
     実 Codex の workspace-write の sandbox の中から service の socket に届くことは stub の test では確かめられないので、ここで確かめる。
     Codex の worker が打つ `dagq` を scratch のバイナリにするには、description に scratch のバイナリの path を書くか、`[run.env]` で scratch のバイナリを先にした `PATH` を渡す。
     クライアントモードの `dagq` は queue の場所を解決しないので `XDG_DATA_HOME` は要らない（scratch の queue の service は、その queue の `up` か `dagq service start` が起動したものを使う）。
   - worker が `dagq` を打つ task のために、使い捨て repository の `dagq.toml` の `[run.env]` を[scratch の worker が打つ `dagq` の環境](#scratch-の-worker-が打つ-dagq-の環境)のとおり置く。
   - `tq ready ID --bypass-review` を add が返した ID だけに打つ。
4. 専用の terminal で `tq supervise --parallel 2 --claude ~/.local/bin/claude --codex ~/.local/bin/codex --log-dir <scratch>/logs --observe-interval 0 --observe-daily false --report-daily false --forecast-snapshots false --host-metrics-interval 0` を起動する。
   `tq status` の `supervisors[].providers` で claude と codex がどちらも `found: true`、codex の `modes` が `["headless"]` であることを見る。
5. 両方の task の `worker_question` に `tq answer <id> --text 'yes'` と答える。
6. 両方の run が着地したら次を集めて確かめる。
   - `tq show ID`: task `completed`、run `integrated`、run の `requested_provider` と `actual_provider` が task の provider のまま（切り替えが無い）、`worker_mode: headless`。
   - `tq events --run RUN --full`: `run_claimed` の provider と版。`turn_requested` / `turn_started` / `turn_finished` の列（`outcome: succeeded`、`failure: null`、token の数）。Codex は `turn_session_identified`（thread の id）。Claude は answer の turn が同じ session の resume（`turn_started` の `session_id` が run の id）。`provider_switched` が無い。
   - run dir の `turns/turn-NNNNNN.jsonl` が CLI の出力（Claude は stream-json、Codex は `--json` の JSONL）で、`turns/` の依頼の記録と対になっている。
   - Codex の run: 着地した commit に `.codex/rules/dagq-deny.rules` が入っていない（`info/exclude`）。
     `~/.codex/config.toml` の更新時刻が変わっていない（変わっていれば、中身に main checkout の `[projects."…"]` の `trust_level` が足されていないかを見る。codex exec が thread の開始で書く project の trust は、worker の turn が `-c` で worktree の trust を渡して止めている。[provider-lifecycle](../design/provider-lifecycle.md#codexの非対話のworker)）。
   - Codex の run の ask: 最初の turn の出力（`turns/turn-000001.jsonl` の `command_execution`）で `dagq ask` が開いた ask（`id`・`kind: worker_question`）を出して 0 で終わっている（`{"error": ..., "queue_service": {"code": "unreachable"}}` なら sandbox から socket に届いていない）。
     `ask_opened` の event の actor が `worker:<run id>`。
     ask の `asked_by` が `worker`。
     answer の turn は `turn_requested` の `what` が `answer of ask N`、`turn_started` の `session_id` が `turn_session_identified` の thread（`codex exec resume`）。
     `authorization_denied` が無い。
   - `tq stats --full` の `runs[]` の provider・経路・turn。
     `tq kpi --by provider` と `--by route` に 2 本が分かれて出る。
   - `origin/main` に task ごとに 1 commit。
7. 余力があれば切り替えも見る: supervisor を止め、`--codex <scratch>/no-such-codex` で起動し直して Codex の task をもう 1 件流すと、非対話の Claude で始まり（`provider_switched`、`phase: start`、`reason: executable_missing`）着地する。
8. [後始末](#後始末)と同じく supervisor を止め、`doctor` の `unfinished_runs` が空で、run の pid の Claude・Codex が残っていないことを見る。

## 非対話の Claude の前提の確認

非対話の Claude の経路（[非対話の worker](../design/supervisor-lifecycle/headless-worker.md)）が前提にしていて stub では確かめられない次の 3 点を、queue も supervisor も使わずに、scratch の使い捨て repository（`git init -b main` と `seed.txt` の 1 commit）で実 `claude -p` を直接起動して確かめる。
Claude Code の版を上げたとき、または読み手（`ClaudeTurnReader`）・turn の止め方（`stop_turn`）・`turn_session_exists` を変えたときに流す。
本番の queue・`~/.local/bin/dagq`・`~/.claude` の設定は変えず、確かめるために置いた transcript は終わってから消す。
打つコマンドと前回の観察は [manual-smoke-results](../plans/manual-smoke-results.md) の「非対話の Claude の前提の確認」にある。

1. **turn の process group と Bash tool の子**: turn を自分の process group と session で起動し（runtime の `CommandSpec::new_session` と同じ）、Bash tool に `&` で切り離したもの・`run_in_background` のもの・前景のものを打たせ、turn が自分で終わる・group に SIGTERM・group に SIGKILL の 3 通りで、`ps -axo pid,ppid,pgid,command` で何が残るかを見る。
   前提は、tool のコマンドが turn の group に入らず、`stop_turn` が group を止める前に子孫を pid で集めて止めることで届くこと。
2. **未ログインの最初の turn**: 空の dir を `CLAUDE_CONFIG_DIR` にして未ログインを作り（人の `~/.claude` と keychain には触れない）、最初の turn が失敗しても transcript を残し、次の turn が `--resume` になり、ログインした後の `--resume` で続けられることを見る。
3. **`rate_limit_event` の `status: rejected` と overage**: overage で賄えている間の `status: rejected`（`isUsingOverage: true` を伴う）で読み手が turn を止めないことを、実物の stream で見る。
   上限に当たったら次の手順で集める。
   1. 非対話の Claude の run の `turns/turn-NNNNNN.jsonl` が stream の全体を持つ。
      `turn_finished` の `failure` が `usage_limit` になった run があれば、その file を scratch に写して残す（run dir は後始末で消えうる）。
   2. `grep rate_limit_event turns/turn-*.jsonl` で、`status`・`isUsingOverage`・`overageInUse`・`overageStatus`・`overageDisabledReason`・`rateLimitType` を読む。
      同じ turn に `system/api_retry`（`error_status: 429`）や `result.api_error_status: 429` があるかも見る。
   3. overage が使えるアカウント（`overageStatus` が `allowed`）で、`status: rejected` と `isUsingOverage: true` の行の後に turn が `is_error: false` で終わるかを見る。

## Codex の goal review のスモーク

`[roles.goal_review]` の `provider = "codex"` の goal review（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)、[Goal review](../design/supervisor-lifecycle/goal-review.md) の 3、[provider-lifecycle](../design/provider-lifecycle.md#codexのheadless-job)）は、spike の JSONL と rollout を書く stub の `codex` でだけ自動 test される。
実 Codex の出力の形・read-only の sandbox の中の `dagq` の読み取り・rollout の model・認証の失敗の文言は stub では確かめられないので、codex-cli の版を上げたとき、Codex の job の起動（`Codex::headless_command`・`apply_launch`）か読み手（`last_message`・`job_session`・`job_failure`）を変えたときに、使い捨て repository で 1 回流す。
人か inbox が行う（worker は使い捨ての queue を作れない。[隔離](#隔離)）。

### 手順

1. [非対話の worker のスモーク](#非対話の-worker-のスモーク)の 1・2 と同じ用意（使い捨て repository `dagq-smoke`、bare の `origin`、scratch のバイナリの wrapper `tq`、`tq init`、`codex login status` がログイン済み）。
   `~/.codex/config.toml`・`~/.codex/auth.json`・`CODEX_HOME` は書き換えない。
   `~/.codex/config.toml` の更新時刻を控える。
2. 使い捨て repository の `dagq.toml` に次を書いて commit する（main checkout の作業ファイルが読まれる）。

   ```toml
   [roles.goal_review]
   provider = "codex"
   effort = "low"
   ```

   `tq doctor` の `roles.goal_review` が `{"provider": "codex", "source": "dagq.toml", "model": null, "effort": "low"}` であることを見る。
3. goal を 1 件（acceptance に「`seed.txt` がある」）と、その goal の task を 1 件（`--verify 'test -f seed.txt'`、description に「`smoke-goal.txt` に 1 行足して commit し、receipt を書く」）登録して `tq ready ID --bypass-review` する（add が返した ID だけに打つ）。
4. [非対話の worker のスモーク](#非対話の-worker-のスモーク)の 4 と同じ `tq supervise ... --codex ~/.local/bin/codex` を起動する。
   task が着地すると goal が候補になり、goal review が Codex で動く。
5. 確かめる。
   - `tq events --goal GOAL --full`: `goal_review_started` の `launch` が `{"role": "goal_review", "provider": "codex", "model": null, "effort": "low", "source": "dagq.toml"}` で `session_id` が null。
     `goal_review_finished` の `decision`（`achieved` のはず）、`session_id`（Codex の thread の id）、`model`（Codex が実際に使った model。読めなければ `model_unknown` の理由）。
   - goal の `goal-reviews/<id>/review.out` が `codex exec --json` の JSONL（`thread.started` … `turn.completed`）で、`command_execution` に `dagq show` などの読み取りがあり、書き込みのコマンドがあれば sandbox に拒まれている。
     `review.err` に `--dangerously` の類の警告が無い。
   - `ps` で見た job の process（`codex exec --json --skip-git-repo-check -C <checkout> -c model_reasoning_effort="low" -c features.network_proxy=true -c default_permissions="dagq_job" -c permissions.dagq_job.extends=":read-only" ...`。
     `--` も prompt も無く、prompt は stdin で渡る。
     `--sandbox read-only` の代わりに queue service の socket だけを許す profile）に bypass の flag が無く、job の `dagq show` などが `queue_service` の `unreachable` で失敗していない（job が短ければ `review.out` の `thread.started` と `tq status` の時刻で代える）。
   - `~/.codex/sessions/YYYY/MM/DD/rollout-*-<thread>.jsonl` の `turn_context` の `model` が `goal_review_finished` の `model` と同じ。
   - `tq stats --full` の `jobs.goal_review.by_provider.codex` と `by_model.<model>` に 1 件。
   - `~/.codex/config.toml` の更新時刻が変わっていない。
   - 余力があれば、read-only の job に無害な file の書き込みを試させ、sandbox の拒否の出力を採取する。
6. 余力があれば切り替えも見る: supervisor を止め、`--codex <scratch>/no-such-codex` で起動し直し、goal に task を足して着地させると、次の goal review が Claude で動き、`goal_review_started` の `launch` に `switched_from: codex`・`switch_reason: executable_missing` がある。
7. [後始末](#後始末)と同じく supervisor を止める。

## Codex の run review のスモーク

`[roles.review] provider = "codex"` の通常 run review（[ADR-t1207-1](../adr/2026-09-30-t1207-1-codex-run-review.md)）は、使い捨て repository の `dagq.toml` に設定して確かめる。
人か inbox が行い、本番 queue と固定バイナリには開発中のビルドを使わない。

1. [隔離](#隔離)に従い、`dagq-smoke` と bare の `origin`、開発中のバイナリのコピー、専用 queue を作る。
   `dagq.toml` に `[roles.review] provider = "codex"` を書いて commit する。
2. Codex の headless task を 1 件、簡単なファイルの追加とその存在を確認する verify で登録し、使い捨て queue だけで `ready --bypass-review` にする。
   `supervise --no-claude --parallel 1 --codex <実体の path>` で流す（`--once` の supervise は queue service を起動しないので付けない）。
3. `events --run RUN --full` の `review_started.launch.provider` が `codex`、`review_finished.verdict` が `pass` で、`show ID` の task が `completed`、run が `integrated`、`origin/main` が進んだことを確かめる。
   run の dir の `review-<N>.out` は `codex exec --json` の JSONL である。
   起動引数は `src/infrastructure/codex.rs` の `Codex::review_command`（`headless_command` に `distrust_config` の `-c projects={"<worktree>"={trust_level="untrusted"}}` を足す。[ADR-t1570-1](../adr/2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)）と照合する。
   この `supervise` の review は executor が queue service に接続させるため、`--sandbox read-only` は `job_service_config` の permission profile `dagq_job` の `-c` に置き換わる（[ADR-t1233-5](../adr/2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md) 決定 4）。
   起動中の process の argv を採取し、次の形を確かめる（`<socket>` は service の socket の実 path、model と effort は launch が指定したときだけ付く。prompt は stdin）。

   ```text
   codex exec --json --skip-git-repo-check -C <worktree>
     -c projects={"<worktree>"={trust_level="untrusted"}}
     [-m <model>] [-c model_reasoning_effort="<effort>"]
     -c features.network_proxy=true
     -c default_permissions="dagq_job"
     -c permissions.dagq_job.extends=":read-only"
     -c permissions.dagq_job.network.enabled=true
     -c permissions.dagq_job.network.unix_sockets={"<socket>"="allow"}
   ```

   queue service 向けの置き換え前の `review_command` 単体は、上の profile の 5 つの `-c` の代わりに `--sandbox read-only` を持つが、worktree の `untrusted` の `-c` は同じである。
   実 Codex が使えない権限環境では `review_failed` と `approve_landing` ask を確認し、権限を直して別 task で再試験する。
4. [後始末](#後始末)と同じく supervisor を止める。

## Codex の inbox のスモーク

inbox を Codex CLI の対話の session で開く経路（goal 77）は、worker が確かめられない。
人の terminal・使い捨ての queue・`watch --role inbox` が要るためである。
Codex の対話の TUI で inbox に要るものを queue なしで調べた結果は [codex-inbox-spike](../plans/codex-inbox-spike.md) にある。
ここでは、それを使い捨ての queue の実物で確かめる。
Codex の inbox を開く経路を入れたとき、Codex CLI の版を上げたとき、inbox の起動（`AgentProvider::inbox_command`）・hook・skill を変えたときに流す。
人か inbox が行う（worker は使い捨ての queue を作れない。[隔離](#隔離)）。
inbox を開く terminal の種類は問わず、cmux も tmux も使わない。

1. [非対話の worker のスモーク](#非対話の-worker-のスモーク)の 1・2 と同じ用意をする。
   使い捨て repository `dagq-smoke`、bare の `origin`、scratch のバイナリの wrapper `tq`、`tq init`、`codex login status` がログイン済みであること。
   `~/.codex/config.toml`・`~/.codex/auth.json`・`CODEX_HOME` は書き換えない。
   `~/.codex/config.toml` の更新時刻と sha256 を控える。
   以後の `tq up` と `tq inbox` は、scratch のバイナリの dir を先頭にした `PATH` で打つ（inbox の Codex が打つ `dagq` も scratch のバイナリにするため。[他の repository のスモーク](#他の-repository-のスモーク)の 1 と同じ）。
   `tq` は人の terminal だけで使い、inbox の中では使わない（`tq` の `env -u DAGQ_ROLE -u DAGQ_QUEUE …` が inbox の身元の環境を外し、inbox の実際の呼び出しの経路を確かめられなくなるため）。
   inbox の中の `dagq` は、この `PATH` で scratch のバイナリに解決される。
   worker の `dagq ask` が scratch の queue に届くよう、使い捨て repository の `dagq.toml` の `[run.env]` を[scratch の worker が打つ `dagq` の環境](#scratch-の-worker-が打つ-dagq-の環境)のとおり置く。
2. inbox の provider を Codex にする手段を確かめる。
   ADR（task 1181）が選んだ設定（`dagq.toml` の `[roles.inbox]`）か、`tq inbox --help` の起動の引数を使う。
   どちらも無ければ、Codex で inbox を開く手段が無いことと scratch のバイナリの版を結果に書き、7 の後始末に進む。
3. 自分の terminal で `tq up --claude ~/.local/bin/claude --codex ~/.local/bin/codex` を打つ。
   [隔離](#隔離)の「`up` は使わない」の例外で、`dagq inbox` の経路を確かめるため `up` を使う（LaunchAgent ができる）。
   supervisor が起動し、inbox は開かずに `dagq inbox` の案内が出ることを見る。
4. 別の terminal（`DAGQ_ROLE` の無い、人自身の terminal）で `dagq-smoke` の中から、2 の設定か引数で `tq inbox` を打つ。
   次を見る。
   - Codex の TUI が前面で開き、`/status` の `Model provider`・`Model`・`Session` が出る。
     `tq events --all --full` の `inbox_opened` に provider と guardrail の有無がある。
     `tq status` の `inbox_guardrail` が ADR の決めたとおりである。
   - 起動の時に更新の案内・hook の trust・開発中の feature の警告のどれが出たかを記録する。
     hook の trust を求められたら、ADR の決めた扱いに従う（人の設定に書く「Trust all」は、ADR がそうと決めたときだけ選ぶ）。
   - inbox の中で `command -v dagq` が scratch のバイナリを指し、`DAGQ_ROLE` が `inbox` であることを、inbox の画面の command の出力で見る。
   - 最初の turn で inbox が `dagq status --role inbox` を読み、`dagq watch --role inbox --until-attention` を ADR の決めた形で回し始める（どちらも inbox の中の `dagq` で、`tq` ではない）。
     Codex の background terminal か前景の待ちか、外の process からの `codex queue` か。
     `tq status` の `inbox_watcher` が running になる。
5. inbox に届くものを起こして確かめる。
   - `dagq-smoke` に task を 1 件登録し、最初に `dagq ask --run $DAGQ_RUN_ID --kind worker_question --because scope --topic other --question '続けてよいか'` を打つよう description に書いて、`tq ready ID --bypass-review` する（add が返した ID だけに打つ）。
     worker の ask が開くと、inbox の watch が返り、inbox が人に ask を見せる。
     選択肢の提示（`request_user_input` か番号つきの文）と、待ちの間に人が打てるかを記録する。
   - 人が答え、inbox がそれを `answer` で書く。
     `tq events --all --full` の `ask_answered` の actor が inbox で、`authorization_denied` と `no_use_case` が無く、worker の run が再開する。
     inbox の Codex の sandbox の中の `dagq` が queue に書けたか（queue service の socket か、ADR の決めた経路か）を、inbox の画面の command の出力で見る。
     inbox の書き込みが service 経由になる前の build（[ADR-t3102-2](../adr/2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md) 決定 6 の段 4 の前）では、`answer` が DB を直接開くので sandbox の中で失敗しうる。その出力を記録する。
   - watch が返った後、inbox が watch をまた回すことを見る。
     30 分以上 attention が無い間も watch が続き、次の attention で起きるかを見る（前景の待ちなら、turn が開いたままか、途中で止まったか）。
6. 復帰と区間を確かめる。
   - inbox で `/compact` を打ち、次の turn で inbox が `status --role inbox` から読み直して watch を続けるかを見る。
   - `/clear` を打ち、新しい session で同じことを見る。
   - `/quit` で終え、`codex resume <id>` ではなく `tq inbox` で開き直す。
   - `tq events --all --full` と `tq stats --full` に inbox の session の区間（開いた・閉じた、provider `codex`、model）が残り、Claude の inbox の区間と見分けられることを見る。
     `/clear` の前の session の区間がいつ閉じたかを記録する。
   - Codex の rollout（`~/.codex/sessions/YYYY/MM/DD/rollout-*-<thread>.jsonl`）の `turn_context` の `model` と、記録の model が同じであることを見る。
7. 後始末。
   - inbox の session を `/quit` で終え（外の見張りの process を `dagq inbox` が起こしていれば、それが残っていないことを pid で見る）、`tq down --wait` で supervisor を止め、`up` が作った LaunchAgent が残っていないことを見る。
   - `tq doctor` の `unfinished_runs` が空であることを見る。
   - 使い捨て repository `dagq-smoke`、bare の `origin`、`<scratch>/xdg` の queue の dir と scratch のバイナリを消す。
   - `~/.codex/config.toml` の更新時刻と sha256 が 1 と同じかを見る。
     変わっていれば、変わった欄（`[projects."…"]` の `trust_level`、`[tui.model_availability_nux]`、hook の trust など）を結果に書き、戻すかは人が決める。
   - 結果は [manual-smoke-results](../plans/manual-smoke-results.md) に、Codex CLI の版・2 の手段・各確認の結果・見つけた問題として残す。

## run の review の実 CLI の確認<a id="runのreviewの実cliの確認"></a>

run の review の job が worktree の設定と agent を読まず（[ADR-t1470-1](../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)）deny と宣言した道具に限られることは、argv・settings・道具の変換を変えたときに人が ignored の IT `review_subagents::the_real_claude_review_jobs_keep_their_limits` で確かめる（流し方と確かめる項目はその doc comment）。

## 他の repository のスモーク

dagq のソースでない repository で使えることは、stub の provider の e2e（`tests/e2e/other_repository.rs`）でだけ自動 test される。
e2e は `target/` のバイナリを `--claude` の stub と `supervise` で直接動かし、配布の経路（`cargo install`、marketplace から入れた plugin、plugin の launcher がバイナリを PATH から解決すること、`--plugin-dir` の無い `up`）と実 Claude の planner・worker・review は通らない。
それをこの手順で確かめる。
リリースの前と、配布・`up`・計画の依頼・着地先の branch・push の解決を変えたときに流す。
人か inbox が行う（worker は使い捨ての queue を作れず、plugin も入れられない）。

### 手順

1. [隔離](#隔離)のとおり scratch を作る。
   ただしバイナリは scratch のコピーではなく、`cargo install --locked --root <scratch>/cargo dagq`（リリースを確かめるとき。まだ crates.io に無い版は、確かめたい commit の checkout で `cargo install --locked --root <scratch>/cargo --path .`）で入れる。
   `--root` を付け、`~/.cargo/bin` と固定バイナリ `~/.local/bin/dagq` を置き換えない。
   以後のコマンドは `PATH=<scratch>/cargo/bin:$PATH` と `XDG_DATA_HOME=<scratch>/xdg` で打ち（plugin の launcher と `up` は PATH で最初に見つかる `dagq` を使う）、`env -u DAGQ_ROLE -u DAGQ_QUEUE -u DAGQ_ACTOR_ID -u DAGQ_RUN_ID -u DAGQ_TASK_ID` で session の actor の env を外す。
   これを wrapper（`tq`）にし、`tq --version` が入れた版であることを見る。
2. 使い捨て repository `dagq-smoke` を `git init -b master` で作り、`seed.txt` と `migrations/0001_x.sql`（中身は何でもよい）だけを commit する。
   `Cargo.toml`・`AGENTS.md`・`CLAUDE.md`・`dagq.toml` は置かず、`origin` も足さない。
   `git branch --list main` が空であることを見る。
3. plugin を公式の手順で入れる: `claude plugin marketplace add hisamekms/dagq` と `claude plugin install claude-dagq@dagq --scope local`（使い捨て repository の中で打ち、その repository だけに入れる。user の scope の plugin を置き換えない）。
   `claude plugin list` に `claude-dagq@dagq` が入れた scope で出て、plugin の version がバイナリの version と major.minor で一致することを見る（食い違えば launcher が `{"warning": ...}` を stderr に出す。README の update の節）。
4. repository の root で `claude` を一度起動して folder trust を承認し、`tq init` と `tq doctor` を打つ。
   `doctor` の `repository` が `branch: master`・`branch_source: master`・`remote: origin`・`remote_exists: false`・`push: true` で `error` が無いことを見る。
5. 自分の terminal で `tq up --claude ~/.local/bin/claude`（`--plugin-dir` も `--auto-update` も付けない。ソースでない repository の `--auto-update` は拒まれる）を打つ。
   supervisor が LaunchAgent で起動し、inbox は開かずに `dagq inbox` の案内が出て、出力の `repository` が 4 と同じで、preflight が installed の plugin を見つけて通ることを見る。
6. 別の terminal で repository の中から `tq inbox` を打って inbox を開き、inbox に「`smoke.txt` に 1 行足す task を 1 件、`--verify 'test -f seed.txt'` で」と頼み、inbox が計画の依頼を記録する（inbox を通さないなら `tq request add --text '...'`。`tq plan` は拒まれる）。
   supervisor が依頼に runtime の planner を立て、planner が AGENTS.md の無い repository で verify・paths・evidence を決めて `submit` し、plan review が pass して task が ready になり、supervisor が claim することを見る（plan review が concern なら inbox の `approve_plan` に `ready` と答える）。
7. run が着地したら確かめる。
   - `tq show ID`: task `completed`、run `integrated`。
   - `master` に 1 つの squash commit が乗り、checkout が追従して clean。
     `main` の branch は作られていない。
   - `tq events --run RUN --full`: `push_skipped`（`remote: origin`・`branch: master`・`reason: the repository has no remote origin`）があり、`push_failed` の attention が inbox に出ていない。
     `migration_renumbered` と `migration_number_taken` が無い。
   - `tq stats` と `tq kpi` が止まらずに出て、cargo 専用の計測（`work_breakdown` の `llvm_cov` など、`kpi --by toolchain`）が無い。
   - worker・review・plan review の prompt（run dir と proposal の dir）に、dagq の repository に固有の規則（cargo・llvm-cov・e2e の推奨、ADR の番号、`AGENTS.md` を名指すこと）が載っていない。
     `[e2e] paths` が無く task が `e2e` を要らないので、worker の prompt に e2e の行（「E2E: do not run the e2e」）が無く、run に `run_e2e_*` の event が無い。
8. `tq down --wait` で supervisor を止め、inbox の terminal を閉じる。
   `claude plugin uninstall claude-dagq@dagq --scope local` で入れた plugin を外す。
