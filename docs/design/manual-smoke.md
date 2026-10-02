---
id: design-manual-smoke
type: design
title: Manual smoke of the paths that include real Claude and Codex
status: current
created: 2026-09-25
updated: 2026-10-02
last_verified: 2026-10-02
scope: operations
related:
  - adr-0036
  - design-supervisor-lifecycle
  - design-provider-lifecycle
  - adr-t813-1
  - adr-t813-2
  - adr-t813-3
  - adr-t1233-2
---

# Manual smoke of the paths that include real Claude and Codex

実 Claude Code と実 Codex CLI を含む経路は自動 test にしない（AGENTS.md の「テストの制約」）。`tests/e2e.rs` のハッピーパスは stub を provider にするので、実 Claude の起動・ダイアログ・resume と、異常系の組み合わせはこの手順で人（または人に頼まれた session）が確かめる。runtime の振る舞いを大きく変えたとき、Claude Code か cmux の版を上げたときに流す。結果は task の receipt の `summary`（または `note`）に、版・シナリオごとの結果・見つけた問題を残す。

手順は 6 つある。

- [故障経路のスモーク](#故障経路のスモーク): 使い捨て repository で異常系 6 シナリオを起こし、二重起動と成果の喪失が無いことを確かめる。
- [独立 task 1 件の完走](#独立-task-1-件の完走): この repository の本番 queue で、登録から着地まで人が DB を直さずに通ることを確かめる。
- [作業の内訳のスモーク](#作業の内訳のスモーク): 使い捨て repository で実 Claude Code の worker に background のコマンド・async の subagent・つないだコマンドをさせ、作業の内訳（task 514）の記録を transcript と突き合わせる。
- [非対話の worker のスモーク](#非対話の-worker-のスモーク): 使い捨て repository で実 Codex と実 Claude の非対話の worker を 1 本ずつ着地させ、turn の記録と provider・経路の記録を確かめる。
- [Codex の goal review のスモーク](#codex-の-goal-review-のスモーク): 使い捨て repository で goal review を実 Codex で動かし、launch と sandbox と記録を確かめる。
- [Codex の run review のスモーク](#codex-の-run-review-のスモーク): 使い捨て repository で通常 run の review を実 Codex で動かし、verdict から着地まで確かめる。
- [他の repository のスモーク](#他の-repository-のスモーク): dagq のソースでない使い捨て repository（default branch が `master`、`origin` なし、`Cargo.toml` も `AGENTS.md` も無い）で、`cargo install` したバイナリと公式の手順で入れた plugin を使い、`up`・`plan` から着地までを通す。

## 故障経路のスモーク

### 隔離

本番 queue と固定バイナリ `~/.local/bin/dagq` を汚さないため、すべてを scratch directory に閉じる。

このスモークは人か inbox が行う。worker（dagq の run の session）は使い捨ての queue を作れず操作もできない（authorization の policy は worker に `queue.admin` を与えず、worker の状態を変えるコマンドは自分の run と task にしか効かない。[Authorization](authorization.md)、ADR-t728-1。task 983 で `dagq init` が `authorization_denied` になり、ask 195 で人が policy を変えないと決めた）。worker は env を外して迂回せず、実バイナリでの確認は integration test（`tests/it` の fixture）か e2e（`tests/e2e.rs`）に書き（e2e は worker が流さず、要る run には review の pass の後に runtime が host で流す。[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)、[Review](supervisor-lifecycle/review.md#着地の前のe2e)）、実 queue での手の確認が要るものは receipt の `follow_ups` にして人か inbox に任せる。

- **バイナリ**: 確かめたい commit で `cargo build --locked` したものを scratch にコピーして使う。`target/` のバイナリで本番 queue を開かない（開いただけでは migrate しなくなった（ADR-0045 決定 5）が、状態を変えるコマンドで未着地の遷移を本番に持ち込まない。緩める範囲は ADR-0045 決定 18）。
- **repository**: `git init` した使い捨て repository。ディレクトリ名は `dagq-smoke` にする（task 710）。runtime は queue の cmux の workspace group を `[<repository のディレクトリ名>]`（例 `[dagq-smoke]`）、workspace の title を `[<ディレクトリ名>]worker#...` などと名付けるので、残った group がどのスモークのものか名前で分かる（`tests/e2e.rs` の fixture は `dagq-e2e` で、group は `[dagq-e2e]`）。`repo` のような汎用の名前にしない。`seed.txt`（検証コマンドが見る）、3 行の `shared.txt`（衝突用）、`CLAUDE.md`、`.claude/settings.json`（`permissions.defaultMode: auto`）を commit しておく。scratch に置いた bare repository を `origin` にする（supervisor の着地は push まで行い、`origin` が無いと `push_failed` の attention になる。手で `integrate` するときは `--no-push` でもよい）。
- **queue**: 全コマンドを `XDG_DATA_HOME=<scratch>/xdg` で、repository を cwd にして打つ（queue は `<scratch>/xdg/dagq/<hash>/queue.db` に解決される）。これを 1 行の wrapper script（例 `tq`）にしておく。
- **supervisor**: 専用の cmux workspace で `supervise --parallel 2 --claude <agent>` を起動し、`--log-dir` か `tee` で log を残す。`up` は使わない（inbox / planner の workspace と launchd agent を作るため）。`--once` は付けない。
- **folder trust**: 実 Claude を使う前に、使い捨て repository の root で一度 `claude` を起動して trust dialog を承認する。worktree で dialog が出るかは親 repository の root が信頼済みかで決まる（[provider-lifecycle](provider-lifecycle.md#trust-prompt)）。承認しないと、最初の承認より前に起動した run session がすべて dialog で止まる。

#### scratch の worker が打つ `dagq` の環境

人か inbox が `dagq-smoke` に立てる worker には、使い捨て repository の main checkout の `dagq.toml` の `[run.env]` で `XDG_DATA_HOME` と、scratch の bin を先頭にした `PATH` を渡せる。これはコードで確認した渡し方であり、実 cmux・Claude Code の shell まで含む確認は下の手順で行う。supervisor を起動する terminal の env だけには頼らない。

コードの根拠（2026-09-30）:

- `src/infrastructure/run_env.rs` の `parse_run_env` / `parse_config` は両変数を受け付ける。`load_run_env` / `expand` は `${DAGQ_QUEUE_DIR}` と `${DAGQ_RUN_DIR}` だけを展開する。`$PATH`・`${PATH}`・`$HOME`・`~` は展開されないので、PATH は先頭に足す部分も既存の部分も、展開済みの絶対パスで書く。
- 同 file の `ShellVerifier::run_env` が main checkout の設定を読み、`src/application/supervise/session.rs` の `Supervisor::provision` がそれを渡す。`src/application/actor_executor.rs` の `HostActorExecutor::spawn`（`RunWorkspace`）は `actor_env` の後に `run_env` を足し、`src/infrastructure/adapters.rs` の `workspace_create_arguments` は各値を `--env KEY=VALUE` にする。この経路で `XDG_DATA_HOME` や `PATH` を後から上書きする処理は無い。PATH の値を丸ごと指定できるので scratch の bin を先頭にできる。ただし cmux や shell の起動設定による変更は実機で確認する。
- `actor_env` は `DAGQ_ROLE`・`DAGQ_QUEUE`・`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID` を設定する。`[run.env]` は `DAGQ_` で始まる key を拒むため、これらを上書きできない。`DAGQ_QUEUE` が渡ることと CLI の DB の解決は別で、`src/main.rs` の `execute` は `QueueLocation::resolve(cli.db.as_deref(), &cwd)` を呼ぶ。`src/infrastructure/location.rs` の `QueueLocation::resolve` / `data_home` は `--db` があればそれ、無ければ cwd の Git common directory と `XDG_DATA_HOME`（無ければ `~/.local/share`）から解決する。`DAGQ_QUEUE` だけでは通常の CLI の DB の指定にならない。

人か inbox が行う設定と確認:

1. 確認対象のバイナリを `<scratch>/bin/dagq` に置き、`dagq-smoke/dagq.toml` に次を足して commit する。`<scratch>` と `<既存のPATHの展開済みの値>` は実際の絶対パスに置き換える。後者には cargo・git などを解決する元の PATH 全体を残す。supervisor も同じ `<scratch>/xdg` と scratch のバイナリで起動する。

   ```toml
   [run.env]
   XDG_DATA_HOME = "<scratch>/xdg"
   PATH = "<scratch>/bin:<既存のPATHの展開済みの値>"
   ```

2. task の description に、開始時に `command -v dagq`、`printenv XDG_DATA_HOME PATH DAGQ_ROLE DAGQ_QUEUE DAGQ_RUN_ID` と `dagq locate` を記録するよう書く。scratch の worker 自身の run に対する `dagq ask --run "$DAGQ_RUN_ID" --kind worker_question --because scope --topic other --question 'スモークを続けてよいか'` と、answer 後の再開も指定する。queue の作成・登録・起動・answer・後始末は人か inbox が行い、worker は自分の task / run の許された操作だけを行う。
3. **実機では未確認の確認項目**: 人か inbox が `dagq-smoke` で上記の出力を集める。`command -v` が `<scratch>/bin/dagq`、XDG が `<scratch>/xdg`、role が `worker`、`dagq locate` の `db` が人側の `tq locate` と同じ scratch の queue.db であることを見る。その queue の `tq events --all --full --run RUN` に worker の ask が入り、人か inbox の answer で再開すれば、scratch の worker が scratch のバイナリで自分の queue に打てたと判定する。版・run ID・解決先・ask の ID を結果に残す。
4. shell の起動設定が PATH を変えるなどして一致しない場合は、原因と実際の PATH を残し、description で `<scratch>/bin/dagq` を絶対パスで指定する。XDG の指定も維持されない場合や `--db` で作った queue なら、worker 用 wrapper を別に作り、`exec '<scratch>/bin/dagq' --db '<scratch の queue.db の絶対パス>' "$@"` としてその絶対パスを description に書く。worker 用 wrapper は `DAGQ_*` を消さず、権限を変えない。人用の `env -u ...` を含む `tq` を worker に使わせない。

### stub と実 Claude の使い分け

- **stub**（`tests/e2e.rs` の `STUB` を拡張した shell script）で runtime の状態遷移を見るシナリオ（2〜5）を流す。token を使わず、時間を制御できる。拡張の例: title の `[break]` で `seed.txt` を消して commit し検証を壊す、`[hang]` で commit も receipt も書かずに待つ、`delay=N` で N 秒待ってから commit する。
- **実 Claude**（`--claude` に実体の path）で、Claude 自身の終了と resume が絡むシナリオ（1、6）を流す。cmux の terminal の PATH では session ごとの shim が先に解決されるので、`--claude ~/.local/bin/claude` のように実体を渡す（AGENTS.md の「起動と停止」と同じ理由）。
- stub は headless の review / triage（`claude -p`）にも使われる。`tests/e2e.rs` の stub は本文に `E2E-REVIEW-PASS` を含む task の review だけを pass にし、それ以外は失敗するので、stub のシナリオでは review が `review by hand`、triage が失敗の attention になるのが期待どおり。review と triage の verdict まで見たいシナリオは実 Claude で流す。検証は `integrate` の 1 回だけなので、review で止まった run は検証まで進まない。stub で着地まで流すシナリオ（2〜5）は task の description に `E2E-REVIEW-PASS` を入れて review を pass させるか、`review by hand` になった run に人が `integrate ID` を打つ。
- e2e: worker（stub も実 Claude も実 Codex も）は e2e を流さず、receipt の `e2e` は理由つきの `not_applicable` になる。e2e が要る run（`validation_finished` の `e2e_requirement.required` が true）には、review の pass の後、着地の前に supervisor が host で e2e を流す（[Review](supervisor-lifecycle/review.md#着地の前のe2e)）。使い捨て repository は dagq のソースでないので runtime の知る e2e が無く、要る run は `run_e2e_finished`（`outcome: not_configured`）を記録して着地へ進む。`dagq.toml` を置かない使い捨て repository では `[e2e] paths` が無く、`--evidence e2e` を付けない task は e2e が要らない。この工程そのもの（host での実行・1 本ずつの lock・落ちたときの `needs_session` と resume・流せないときの流し直しと `check the e2e host`）は `tests/it/runtime_e2e.rs` が stub のコマンドで確かめる。本番の固定バイナリの入れ替えの前には、自動更新と `install` の関門が全部の e2e を流す（[Auto-update](supervisor-lifecycle/auto-update.md)・[install](supervisor-lifecycle/install.md)）。

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
2. `tq add` で task を 1 件登録し（`--verify 'cargo test'`、`--change feature`）、`tq ready ID --bypass-review`。description で worker に順番どおり次をさせる: (a) `cargo build`、`cargo test`、`cargo test --test does_not_exist`（失敗する）をそれぞれ `run_in_background: true` で起動して完了の通知を待つ、(b) Agent tool を `run_in_background: true` で起動して完了の通知を待つ、(c) foreground で 15 秒以上かかるコマンド（`cargo test --release`）を実行し、その間に人が worker の workspace に Ctrl-B を届けて background に移す（送り方は次の段落）、(d) foreground で `cargo fmt && cargo test`、(e) commit と receipt。`sleep N && ...` は Claude Code の harness が `Blocked: sleep N followed by: ...` で拒むので使わない。

   Ctrl-B は、対象がこの scratch の worker の workspace / session であることを人が確認し、その Claude Code を収容する pty の入力側（master）に **1 byte の `0x02`（`\x02`）** を送る。pty を保持する操作ツールの stdin 送信で、文字列 `\x02` の 4 文字ではなく制御文字を渡す（JSON の入力なら `"\u0002"`、master fd を保持する Python の操作元なら `os.write(master_fd, b"\x02")`）。対象 pty への入力ハンドルが無ければ、人が対象 workspace にフォーカスして Ctrl-B を打つ。別 session の pty で代用しない。`/dev/ttys...` の slave への通常の書き込みは入力注入ではない。task 547 では `cmux send-key --workspace <worker> ctrl+b` は `OK` でも移らず、別の対話 session で pty に `\x02` を書くと移った。送信の成功だけで済ませず、この worker の transcript の `backgroundedByUser: true` と完了通知を確認する。これは対話の Claude worker 用で、非対話の worker には当たらない。
3. 専用の cmux workspace で `tq supervise --parallel 1 --claude <実体の path> --log-dir ... --observe-interval 0 --observe-daily false --report-daily false --forecast-snapshots false --host-metrics-interval 0` を起動する。
4. run が閉じたら次を集める: `tq events --all --full --run RUN`（`session_closed` の `work` と `session_exited` の `work_breakdown`）、run dir の `worktime.jsonl`、`tq stats --full`（`runs[].work_breakdown` と `overall.work_breakdown`）、`tq timeline RUN`（`commands`）、session の transcript（`~/.claude/projects/<cwd を符号化した名前>/<session_id>.jsonl`。worker の session_id は run ID と同じ）。
5. transcript の中では tool-use-id で `tool_use`（開始）と `tool_result` / 完了通知を結び、`tool_use` の開始時刻・category・コマンドで `worktime.jsonl` の行と対応づける。`worktime.jsonl` と `session_exited.work_breakdown.heavy` の `start` / `end`・`background`・`finished`・`failed` を transcript と比べ、`exit_code`・`status` は `worktime.jsonl` で比べる。`src/domain/worktime.rs` の `Command::line` と `Breakdown::payload` の出力には tool-use-id が無く、event の `heavy` には `exit_code`・`status` も無い。`secs` の合計が区間の長さ（`session_opened` から `session_closed`）と一致すること、`stats` と `timeline` が `session_closed.work` と同じ値を出すことも見る。
6. [故障経路のスモーク](#シナリオ)の後始末と同じく supervisor を止め、workspace group を消す。

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
- **Ctrl-B**: pty への `\x02` で移ったのは別の対話 session。worker の run の記録は未確認なので、人か inbox が `dagq-smoke` で上の手順を実施する。worker の transcript に `backgroundedByUser: true` と完了の `enqueue` があり、対応する `worktime.jsonl` と `session_exited.work_breakdown.heavy` の `start` が `tool_use`、`end` が完了通知の時刻で、`background: true`、`finished: true`、`failed` が通知と一致すること（`exit_code`・`status` は `worktime.jsonl` で照合）を確認して結果を追記する。timeout の行も同じ記録を確認する。
- **worker の `dagq`**: task 547 では worker の workspace は supervisor の `XDG_DATA_HOME` を継がず、PATH の `dagq` は固定バイナリ `~/.local/bin/dagq` に解決した。最初の `dagq ask` は既定の `~/.local/share/dagq/<hash>/queue.db`（存在しない）を開こうとして失敗し、XDG を付けた打ち直しでは固定バイナリが scratch の queue に ask を書いた（本番 queue には触れていない）。[隔離の設定と確認](#scratch-の-worker-が打つ-dagq-の環境)に `[run.env]` のコード上の根拠と手順を記した。その設定で実 worker が scratch のバイナリ・queue を使えるかは、人か inbox が `dagq-smoke` で解決先と ask / answer を確認して結果を追記する。

## 非対話の worker のスモーク

非対話の worker の経路（[ADR-t813-1](../adr/2026-09-28-t813-1-headless-worker-path.md)、[非対話のworker](supervisor-lifecycle/headless-worker.md)）と Codex の worker（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)、[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)）は、stub の provider（`tests/it/runtime_headless.rs`・`runtime_codex.rs`・`runtime_codex_ask.rs`・`runtime_provider_switch.rs`）でだけ自動 test される（stub の turn は sandbox なしで走る）。実物の CLI の出力の形・認証・sandbox・session（thread）の resume は stub では確かめられないので、Claude Code か Codex CLI の版を上げたとき、turn の組み立て（`turn_command`）か出力の読み手（`ClaudeTurnReader`・`CodexTurnReader`）か Codex の権限（`-c` と rules）を変えたときに、実 Codex と実 Claude の非対話の worker を 1 本ずつ使い捨て repository で着地させる。人か inbox が行う（worker は使い捨ての queue を作れない。[隔離](#隔離)）。

### 手順

1. [隔離](#隔離)のとおり scratch に使い捨て repository `dagq-smoke`（`seed.txt` と `CLAUDE.md` を commit）、bare の `origin`、確かめたい commit の scratch のバイナリ、`env -u DAGQ_ROLE -u DAGQ_QUEUE -u DAGQ_ACTOR_ID -u DAGQ_RUN_ID -u DAGQ_TASK_ID XDG_DATA_HOME=<scratch>/xdg <scratch のバイナリ>` の wrapper（`tq`）を用意し、`tq init` する。非対話の経路は folder trust の dialog に当たらない（[provider-lifecycle](provider-lifecycle.md#workerのproviderと経路)）ので、trust の承認は要らない。
2. 前提を確かめて記録する: `~/.local/bin/claude --version`、`~/.local/bin/codex --version`、`codex login status`（ログイン済み）、Claude のログイン（`claude -p 'say ok'` が答える）。`~/.codex/config.toml`・`~/.codex/auth.json`・`~/.claude` は書き換えない（runtime も書かない）。
3. task を 2 件登録して `ready` にする。どちらも `--verify 'test -f seed.txt'` と、description に「`smoke-<provider>.txt` に 1 行足して commit し、receipt を書く」。
   - Codex: `tq add "codex headless smoke" --provider codex ...`
   - Claude: `tq add "claude headless smoke" --headless ...`
   - どちらの description にも「最初に `dagq ask --run $DAGQ_RUN_ID --kind worker_question --because scope --topic other --question '続けてよいか'` を打って turn を終え、答えを受けてから作業する」を足し、answer の turn（同じ session か thread の resume）も通す。Codex の worker の sandbox の中の `dagq ask` は queue に書かず、run dir の `ask-requests/` への要求になり、supervisor がそれを検査して ask を開く（[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)、task 890）。実 Codex の sandbox の中でこの経路が通ることは stub の test では確かめられないので、ここで確かめる。Codex の worker が打つ `dagq` は task 890 を含むバイナリにする（それより前の固定バイナリは queue を開こうとして sandbox に拒まれる）: description に scratch のバイナリの path を書くか、`[run.env]` で scratch のバイナリを先にした `PATH` を渡す。Codex の `dagq ask` は queue の場所を解決しないので `XDG_DATA_HOME` は要らない。
   - worker が `dagq` を打つ task のために、使い捨て repository の `dagq.toml` の `[run.env]` に `XDG_DATA_HOME = "<scratch>/xdg"` を置く（[作業の内訳のスモーク](#作業の内訳のスモーク)の注の「worker の `dagq`」）。
   - `tq ready ID --bypass-review` を add が返した ID だけに打つ。
4. 専用の cmux workspace で `tq supervise --parallel 2 --claude ~/.local/bin/claude --codex ~/.local/bin/codex --log-dir <scratch>/logs --observe-interval 0 --observe-daily false --report-daily false --forecast-snapshots false --host-metrics-interval 0` を起動する。`tq status` の `supervisors[].providers` で claude と codex がどちらも `found: true`、codex の `modes` が `["headless"]` であることを見る。
5. 両方の task の `worker_question` に `tq answer <id> --text 'yes'` と答える。
6. 両方の run が着地したら次を集めて確かめる。
   - `tq show ID`: task `completed`、run `integrated`、run の `requested_provider` と `actual_provider` が task の provider のまま（切り替えが無い）、`worker_mode: headless`。
   - `tq events --run RUN --full`: `run_claimed` の `provider`・`requested_provider`・`worker_mode`・`provider_version`・`codex_version`（supervisor が Codex の worker を動かすので、どちらの run にも載る）。`turn_requested` / `turn_started` / `turn_finished` の列（`outcome: succeeded`、`failure: null`、`usage` と `tokens`）。Codex は `turn_session_identified`（thread の id）と、`turn_finished` の `tokens_total`。Claude は answer の turn が同じ session の resume（`turn_started` の `session_id` が run の id）。`provider_switched` が無い。
   - run dir の `turns/turn-NNNNNN.jsonl` が CLI の出力（Claude は stream-json、Codex は `--json` の JSONL）で、`turns/` の依頼の記録と対になっている。
   - Codex の run: 着地した commit に `.codex/rules/dagq-deny.rules` が入っていない（`info/exclude`）。`~/.codex/config.toml` の更新時刻が変わっていない（変わっていれば、中身に main checkout の `[projects."…"]` の `trust_level` が足されていないかを見る。codex exec が thread の開始で書く project の trust は、task 1174 から worker の turn が `-c` で worktree の trust を渡して止めている。[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)）。
   - Codex の run の ask: 最初の turn の出力（`turns/turn-000001.jsonl` の `command_execution`）で `dagq ask` が `"requested": true` を出して 0 で終わっている。run の `ask_request_taken`（`outcome: opened`、`ask_id`）が 1 件で、run dir の `ask-requests/` には `<id>.taken` だけが残る。ask の `asked_by` が `worker`。answer の turn は `turn_requested` の `what` が `answer of ask N`、`turn_started` の `session_id` が `turn_session_identified` の thread（`codex exec resume`）。`authorization_denied` が無い。
   - `tq stats --full` の `runs[]` の `provider`・`actual_provider`・`route`・`turns`（`by_provider`）。`tq kpi --by provider` と `--by route` に 2 本が分かれて出る。
   - `origin/main` に task ごとに 1 commit。
7. 余力があれば切り替えも見る: supervisor を止め、`--codex <scratch>/no-such-codex` で起動し直して Codex の task をもう 1 件流すと、非対話の Claude で始まり（`provider_switched`、`phase: start`、`reason: executable_missing`）着地する。
8. [故障経路のスモーク](#シナリオ)の後始末と同じく supervisor を止め、`doctor` の `unfinished_runs` が空で、run の pid の Claude・Codex が残っていないことを見て、workspace group を消す。

### 結果（2026-09-29、task 820 の run cdcc8000）

行っていない。この手順は task 820（goal 57 の文書の task）の受け入れ条件の 1 つだが、task 820 は dagq の worker として動き、worker は使い捨ての queue を作れず操作もできない（[隔離](#隔離)。`init`・`add`・`supervise` は `authorization_denied` になる。task 983、ask 195 で人が policy を変えないと決めた）。worker は env を外して迂回しない。そのため手順だけを書き、実行は receipt の `follow_ups`（`ops`）として人か inbox に任せた。その時点の host の版は Claude Code 2.1.284（`~/.local/bin/claude` の link 先）と codex-cli 0.155.1（`~/.local/bin/codex` の link 先は `~/.codex/packages/standalone/releases/0.155.1-aarch64-apple-darwin/bin/codex`）。stub の provider での自動 test（上に挙げた `tests/it` の 3 本）は着地済みの task 815〜818 が持つ。人か inbox が流したら、この節に版・run ごとの確認点の結果・見つけた問題を足す。

### 結果（2026-09-30、本番の queue の記録から、task 1102）

使い捨ての queue の手作業（手順 1〜5・7・8）の代わりに、本番の queue に着地した実物の非対話の worker の run の記録を読んで、手順 6 の確認点を当てた（ask 214 で人が決めた。worker は使い捨ての queue を操作できない）。読んだのは固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+088c8ec`）の状態を変えないコマンド（`show 1103 --full`・`events --run <RUN> --full --all`・`timeline <RUN>`・`stats --full`・`kpi --since 2026-09-29T00:00:00Z --until 2026-09-30T00:00:00Z --by provider` と `--by route`）と、run dir の `turns/` と `idle.json`、着地した commit（`git show --stat`・`git ls-tree`）、`~/.codex/config.toml` の更新時刻。queue の状態は変えていない。

- **Codex**: task 1103（`--provider codex`、非対話）の run `0a8042c2-349e-40f6-afa5-2e5a8f06f0fb`。2026-09-29T18:24:51Z に claim、18:44:08Z に着地（commit `b9643d5`）。codex-cli 0.155.1（`run_claimed` の `codex_version`・`provider_version`）、supervisor の Claude Code は 2.1.283（`claude_version`）、supervisor の dagq は `0.4.0-dev+4f87a20`。
- **Claude**: 非対話の Claude の run は **無かった**。task 821 は本番の queue に非対話の Claude の run を 1 本も挙げておらず（[headless-worker-measurement](../plans/headless-worker-measurement.md)。planner が `--headless` を付けた task が無い）、この task の時点の `stats --full` の 661 本も provider・actual_provider・route の組は `claude`/`claude`/`null`（460 本、task 819 の前）・`claude`/`claude`/`interactive`（200 本）・`codex`/`codex`/`headless`（上の 1 本）だけだった。Claude の分の確認点は全て確かめていない。確かめる続け方は receipt の `follow_ups` に出した。この時点の host の Claude Code は 2.1.285、codex-cli は 0.155.1。

Codex の run の確認点ごとの観察と結論:

| 確認点 | 観察（読んだコマンドと値） | 結論 |
| --- | --- | --- |
| task と run の状態 | `show 1103 --full`: task `status: completed`、run `status: integrated`、`task_status_changed` in_progress→completed（event 45475）、`run_integrated`（45473、commit `b9643d5`）、`push_finished`（45482） | 着地した |
| provider と切り替え | `show`: task `provider: codex`、run `requested_provider: codex`・`actual_provider: codex`。`events --full --all` に `provider_switched` は無く、`timeline` の `provider_switches: []`、`stats` の `runs[].provider_switches: 0` と全体の `provider_switches.count: 0` | 切り替えは無い |
| 経路 | task・run の `worker_mode: headless`、`session_opened`（45425）の `provider: codex`・`route: headless`、`timeline` の `worker_mode: headless` | 非対話の経路で動いた |
| `run_claimed` | `provider: codex`・`requested_provider: codex`・`worker_mode: headless`・`provider_version: 0.155.1`・`codex_version: 0.155.1`・`claude_version: 2.1.283`。`model: null`・`ladder_model: claude-opus-5-5`・`model_unknown`（codex は自分の model で動く旨） | 設計どおり（[provider-lifecycle](provider-lifecycle.md) の model の記録） |
| turn の列 | turn は 1 つだけ。`turn_started`（45426、`turn: 1`・`resume: false`・`session_id: null`・`request: null`・`silence_secs: 900`・`limit_secs: 14400`）→ `turn_session_identified`（45427、2 秒後、thread `01a0ee69-a210-7613-8db8-d5125e407d9c`）→ `turn_finished`（45432、`outcome: succeeded`・`failure: null`・`exit_code: 0`・`permission_denials: 0`・`denied_tools: []`・`session_created: true`・`model: gpt-6-astra`、`usage` は `input_tokens` 1,283,330・`cached_input_tokens` 1,227,136・`output_tokens` 2,881・`reasoning_output_tokens` 241、`tokens` と `tokens_total` は input 56,194・cache_read 1,227,136・output 2,881）。最初の turn は task の prompt なので `turn_requested` は無い（依頼の file が無い）。`cost_usd`・`duration_ms`・`num_turns` は null（Codex の出力が持たない） | 設計どおり。`tokens.input` は `input_tokens` から cache の分を引いた値 |
| resume の turn | answer・revise・needs_session の resume は無かった（`stats` の `resumes: 0`、`review_verdict: pass`、`ask` 0）。integrate の 1 回目の検証が `verification_flaky`（`runtime_adopt::a_supervisor_that_lost_its_lease_stops_touching_the_run`）で、resume せずに着地をやり直した（`integration_retried`） | 同じ thread の resume は確かめていない |
| `turns/` の出力 | `turn-000001.jsonl` は `codex exec --json` の JSONL の 28 行（`thread.started` 1・`turn.started` 1・`item.started`/`item.completed` の `command_execution` 各 9・`file_change` 各 1、`item.completed` の `agent_message` 5、`turn.completed` 1。最後の `turn.completed` の `usage` が `turn_finished` の `usage` と一致）。`turn-000001.err` は `Reading additional input from stdin...` の 1 行、`limits.json` は `{"silence_secs":900,"limit_secs":14400}`、`exit`（終了の依頼）は空の file。`request-*.json` は無い。`idle.json` は `dagq_turn`（`turn: 1`・`outcome: succeeded`）を持つ | 設計どおり（[非対話のworker](supervisor-lifecycle/headless-worker.md#run-dirのturns)） |
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

旧記録の注意: 1050 の `turn_finished.cost_usd` は 1 回目 4.721233400000003、2 回目 6.019502200000002 で、stream の `result.total_cost_usd`（session の累計）をそのまま記録している。`stats.turns.by_provider.claude.tokens.cost_usd` は 10.740735 となり、turn の費用として足すと重複する。これは [非対話の worker の Claude の読み手](supervisor-lifecycle/headless-worker.md) に記載された task 1199 より前の記録で、修正後も過去の記録は補正しない。今回見た不一致と、修正後の実 resume の測定を receipt の follow_ups に残した。

この 7 run では、Claude の着地と revise の同一 session の resume を確認できた。未確認なのは answer と needs_session の resume、provider が使えない場合の切り替え、設定の実行前後の比較、手順 1〜5・7・8 の使い捨て queue での操作。Codex 固有の rules・sandbox・ask-requests の確認点は今回の Claude の対象外で、task 1102 の未確認事項を解消したとはみなさない。

## 非対話の Claude の前提の確認

非対話の Claude の経路（[非対話の worker](supervisor-lifecycle/headless-worker.md)）が前提にしていて、stub では確かめられない 3 点を、実 `claude -p` で確かめた（task 864、2026-09-30）。queue も supervisor も使わず、scratch の使い捨て repository `dagq-worker-t864-1e1fe057`（`git init -b main` と `seed.txt` の 1 commit）で `claude` を直接起動した。Claude Code は 2.1.285（`~/.local/bin/claude` の link 先 `~/.local/share/claude/versions/2.1.285`）。本番の queue・`~/.local/bin/dagq`・`~/.claude` の設定は変えていない（2. で `~/.claude/projects` に置いた transcript と、1. の turn が書いた transcript は、終わってから消した）。Claude Code の版を上げたとき、または読み手（`src/infrastructure/claude_turns.rs`）・turn の止め方（`headless_session.rs` の `stop_turn`）・`turn_session_exists` を変えたときに、同じ手順で確かめ直す。

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

- Claude Code は Bash tool の 1 回ごとに shell を別の process group（pgid がその shell の pid）で起動する。turn の group にいるのは `claude` 自身だけで、tool のコマンドはどれも turn の group に入らない（Codex と同じ形。[codex-headless-jobs-spike](../plans/codex-headless-jobs-spike.md) の 4.）。
- `&` で切り離したもの（`nohup … &`・`( … &)`・`setsid`）は、tool の shell が終わった時点で親が 1 になり、turn の子孫でもなくなる。
- (a) turn が自分で終わったとき: `claude` は終わる前に `run_in_background` の task（`sleep 7104`）を止めた（stream に `task_updated`・`task_notification`）。`&` で切り離した 3 つは親 1 のまま残った。
- (b) group に SIGTERM: `claude` が自分で片付け、前景の python は exit code 137（`claude` が SIGKILL で止めた。stream の `tool_result` が `Exit code 137`）、`sleep 7104` も止まった。`claude` は 143 で終わった。`&` で切り離した 3 つは残った。
- (c) group に SIGKILL: `claude` だけが止まり、2 つの tool の shell とその子（`sleep 7104`・前景の python）は親 1 になって残った。`&` で切り離した 3 つも残った。

結論: group への signal だけでは Claude の turn の tool のコマンドに届かない。今の実装の `stop_turn` は group を止める前に子孫を pid で集めて止めるので、(c) で残った tool の shell とその子は止まる（前提どおり）。一方 `&` で切り離したものは、turn の途中で止めても自分で終わっても、group にも子孫にも入らずに残る。[非対話の worker](supervisor-lifecycle/headless-worker.md#wrapperがturnを止めるとき) の「turn が自分で終わったときも group に残ったもの（`nohup … &` など）を止める」は Claude では当たらない（`nohup … &` は group に居ない）ので、その節を直した。残ったものは run の worktree で動く process として復旧 job の `stop_processes` が pid で止められる（cwd が worktree のものに限る）。runtime の修正の要否は receipt の follow_up にした。後始末: 残った process は pid で止めた。

### 2. 未ログインの最初の turn

打ったもの（未ログインは、空の dir を `CLAUDE_CONFIG_DIR` にして作った。人の `~/.claude` と keychain には触れない）:

```sh
env -u ANTHROPIC_API_KEY CLAUDE_CONFIG_DIR=<scratch>/cfg-empty \
  claude -p --output-format stream-json --verbose --session-id <uuid> --permission-mode auto -- 'say ok' </dev/null
```

観察:

- 終了コード 1。stream は 3 行: `system/init`（`apiKeySource: "none"`、`claude_code_version: "2.1.285"`、`permissionMode: "auto"`）、`assistant`（`"error": "authentication_failed"`、`model: "<synthetic>"`、text `Not logged in · Please run /login`）、`result`（`subtype: "success"`・`is_error: true`・`api_error_status: null`・`terminal_reason: "api_error"`）。task 812 の測定（[headless-worker-spike](../plans/headless-worker-spike.md)）と同じ形。
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

結論: overage で賄える状態では `status: rejected` が出うる（`isUsingOverage: true` を伴う）。今の読み手（`ClaudeTurnReader::line`）は `status == "rejected"` だけで `usage_limit` にして turn を止めるので、その状態では誤って止める。`isUsingOverage` か `overageInUse` が真なら止めない、という runtime の修正を receipt の follow_up にした。実物の stream では確かめていないので、次に確かめる手順を残す。

読み手の修正（task 1153）: `ClaudeTurnReader::line` は `rate_limit_event` の `status: rejected` でも `isUsingOverage` か `overageInUse` が真なら turn を止めないようにした（CLI 自身の条件に合わせた）。実物の stream での確認は下の手順のまま残る。

次に上限に当たったときの手順:

1. 非対話の Claude の run の `turns/turn-NNNNNN.jsonl` が stream の全体を持つ。`turn_finished` の `failure` が `usage_limit` になった run があれば、その file を scratch に写して残す（run dir は後始末で消えうる）。
2. `grep rate_limit_event turns/turn-*.jsonl` で、`status`・`isUsingOverage`・`overageInUse`・`overageStatus`・`overageDisabledReason`・`rateLimitType` を読む。同じ turn に `system/api_retry`（`error_status: 429`）や `result.api_error_status: 429` があるかも見る。
3. 対話の session の worker が上限に当たったときは stream が無いので、使い捨て repository で `claude -p --output-format stream-json --verbose -- 'say ok'` を 1 回打って保存する（上限の間は短い turn でも同じ event が出る見込み）。
4. overage が使えるアカウント（`overageStatus` が `allowed`）で、`status: rejected` と `isUsingOverage: true` の行の後に turn が `is_error: false` で終わるかを見る。見られたら、この節に行と結論を足す。

## Codex の goal review のスモーク

`[roles.goal_review]` の `provider = "codex"` の goal review（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)、[Goal review](supervisor-lifecycle/goal-review.md) の 3、[provider-lifecycle](provider-lifecycle.md#codexのheadless-job)）は、spike の JSONL と rollout を書く stub の `codex`（`tests/it/goal_review_codex.rs`）でだけ自動 test される。実 Codex の出力の形・read-only の sandbox の中の `dagq` の読み取り・rollout の model・認証の失敗の文言は stub では確かめられないので、codex-cli の版を上げたとき、Codex の job の起動（`Codex::headless_command`・`apply_launch`）か読み手（`last_message`・`job_session`・`job_failure`）を変えたときに、使い捨て repository で 1 回流す。人か inbox が行う（worker は使い捨ての queue を作れない。[隔離](#隔離)）。

### 手順

1. [非対話の worker のスモーク](#非対話の-worker-のスモーク)の 1・2 と同じ用意（使い捨て repository `dagq-smoke`、bare の `origin`、scratch のバイナリの wrapper `tq`、`tq init`、`codex login status` がログイン済み）。`~/.codex/config.toml`・`~/.codex/auth.json`・`CODEX_HOME` は書き換えない。`~/.codex/config.toml` の更新時刻を控える。
2. 使い捨て repository の `dagq.toml` に次を書いて commit する（main checkout の作業ファイルが読まれる）。

   ```toml
   [roles.goal_review]
   provider = "codex"
   effort = "low"
   ```

   `tq doctor` の `roles.goal_review` が `{"provider": "codex", "source": "dagq.toml", "model": null, "effort": "low"}` であることを見る。
3. goal を 1 件（acceptance に「`seed.txt` がある」）と、その goal の task を 1 件（`--verify 'test -f seed.txt'`、description に「`smoke-goal.txt` に 1 行足して commit し、receipt を書く」）登録して `tq ready ID --bypass-review` する（add が返した ID だけに打つ）。
4. [非対話の worker のスモーク](#非対話の-worker-のスモーク)の 4 と同じ `tq supervise ... --codex ~/.local/bin/codex` を起動する。task が着地すると goal が候補になり、goal review が Codex で動く。
5. 確かめる。
   - `tq events --goal GOAL --full`: `goal_review_started` の `launch` が `{"role": "goal_review", "provider": "codex", "model": null, "effort": "low", "source": "dagq.toml"}` で `session_id` が null。`goal_review_finished` の `decision`（`achieved` のはず）、`session_id`（Codex の thread の id）、`model`（Codex が実際に使った model。読めなければ `model_unknown` の理由）。
   - goal の `goal-reviews/<id>/review.out` が `codex exec --json` の JSONL（`thread.started` … `turn.completed`）で、`command_execution` に `dagq show` などの読み取りがあり、書き込みのコマンドがあれば sandbox に拒まれている。`review.err` に `--dangerously` の類の警告が無い。
   - `ps` で見た job の process（`codex exec --json --sandbox read-only -C <checkout> -c model_reasoning_effort="low" -- ...`）に bypass の flag が無い（job が短ければ `review.out` の `thread.started` と `tq status` の時刻で代える）。
   - `~/.codex/sessions/YYYY/MM/DD/rollout-*-<thread>.jsonl` の `turn_context` の `model` が `goal_review_finished` の `model` と同じ。
   - `tq stats --full` の `jobs.goal_review.by_provider.codex` と `by_model.<model>` に 1 件。
   - `~/.codex/config.toml` の更新時刻が変わっていない。
6. 余力があれば切り替えも見る: supervisor を止め、`--codex <scratch>/no-such-codex` で起動し直し、goal に task を足して着地させると、次の goal review が Claude で動き、`goal_review_started` の `launch` に `switched_from: codex`・`switch_reason: executable_missing` がある。
7. [故障経路のスモーク](#シナリオ)の後始末と同じく supervisor を止め、workspace group を消す。

### 結果

#### 2026-09-30: 本番の記録による確認（task 1114）

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

## Codex の run review のスモーク

`[roles.review] provider = "codex"` の通常 run review（[ADR-t1207-1](../adr/2026-09-30-t1207-1-codex-run-review.md)）は、使い捨て repository の `dagq.toml` に設定して確かめる。人か inbox が行い、本番 queue と固定バイナリには開発中のビルドを使わない。

1. [非対話の worker のスモーク](#非対話の-worker-のスモーク)の隔離に従い、`dagq-smoke` と bare の `origin`、開発中のバイナリのコピー、専用 queue を作る。`dagq.toml` に `[roles.review] provider = "codex"` を書いて commit する。
2. Codex の headless task を 1 件、簡単なファイルの追加とその存在を確認する verify で登録し、使い捨て queue だけで `ready --bypass-review` にする。`supervise --no-claude --parallel 1 --once --codex <実体の path>` で流す。
3. `events --run RUN --full` の `review_started.launch.provider` が `codex`、`review_finished.verdict` が `pass` で、`show ID` の task が `completed`、run が `integrated`、`origin/main` が進んだことを確かめる。run の dir の `review-<N>.out` は `codex exec --json` の JSONL で、起動引数は `--sandbox read-only` を含む。実 Codex が使えない権限環境では `review_failed` と `approve_landing` ask を確認し、権限を直して別 task で再試験する。
4. supervisor の終了と試験用 group の削除を確認する。group は `cmux workspace-group delete <group> --close-workspaces` で anchor ごと消す。

2026-10-01 の結果: 別 queue で実 Codex 0.159.2 の worker と review を実行し、task 2 の review が `pass`、run `ca586e69-4833-43c2-a269-f7d90fa2152f` が `908060719bbc3f599cbb061fe1e130827472595e` として main に着地・push した。最初の task 1 は実行側のホスト権限制約で Codex review の app-server 初期化が拒まれ、`review_failed` と ask になった。権限付きでの再試験は成功し、試験用の cmux group は削除した。

## 他の repository のスモーク

dagq のソースでない repository で使えること（goal 52）は、stub の provider の e2e（`tests/e2e/other_repository.rs` の `a_task_lands_on_master_of_a_repository_without_origin_cargo_toml_or_agents_md`。default branch が `master`・`origin` なし・`Cargo.toml` と `AGENTS.md` なしの repository で、`supervise --once` の task が着地し、main が取った番号の migration が振り直されない）でだけ自動 test される。e2e は `target/` のバイナリを `--claude` の stub と `supervise` で直接動かし、配布の経路（`cargo install`、marketplace から入れた plugin、plugin の launcher がバイナリを PATH から解決すること、`--plugin-dir` の無い `up` と `plan`）と実 Claude の planner・worker・review は通らない。それをこの手順で確かめる。リリースの前と、配布・`up`・`plan`・着地先の branch・push の解決を変えたときに流す。

### 手順

1. [隔離](#隔離)のとおり scratch を作る。ただしバイナリは scratch のコピーではなく、`cargo install --locked --root <scratch>/cargo dagq`（リリースを確かめるとき。まだ crates.io に無い版は、確かめたい commit の checkout で `cargo install --locked --root <scratch>/cargo --path .`）で入れる。`--root` を付け、`~/.cargo/bin` と固定バイナリ `~/.local/bin/dagq` を置き換えない。以後のコマンドは `PATH=<scratch>/cargo/bin:$PATH` と `XDG_DATA_HOME=<scratch>/xdg` で打ち（plugin の launcher と `up` は PATH で最初に見つかる `dagq` を使う）、`env -u DAGQ_ROLE -u DAGQ_QUEUE -u DAGQ_ACTOR_ID -u DAGQ_RUN_ID -u DAGQ_TASK_ID` で session の actor の env を外す。これを wrapper（`tq`）にし、`tq --version` が入れた版であることを見る。
2. 使い捨て repository `dagq-smoke` を `git init -b master` で作り、`seed.txt` と `migrations/0001_x.sql`（中身は何でもよい）だけを commit する。`Cargo.toml`・`AGENTS.md`・`CLAUDE.md`・`dagq.toml` は置かず、`origin` も足さない。`git branch --list main` が空であることを見る。
3. plugin を公式の手順で入れる: `claude plugin marketplace add hisamekms/dagq` と `claude plugin install claude-dagq@dagq --scope local`（使い捨て repository の中で打ち、その repository だけに入れる。user の scope の plugin を置き換えない）。`claude plugin list` に `claude-dagq@dagq` が入れた scope で出て、plugin の version がバイナリの version と major.minor で一致することを見る（食い違えば launcher が `{"warning": ...}` を stderr に出す。README の update の節）。
4. repository の root で `claude` を一度起動して folder trust を承認し、`tq init` と `tq doctor` を打つ。`doctor` の `repository` が `branch: master`・`branch_source: master`・`remote: origin`・`remote_exists: false`・`push: true` で `error` が無いことを見る。
5. 専用の cmux workspace で `tq up --in-cmux --claude ~/.local/bin/claude`（`--plugin-dir` も `--auto-update` も付けない。ソースでない repository の `--auto-update` は拒まれる）を打つ。`[dagq-smoke]supervisor` と `[dagq-smoke]inbox` が開き、出力の `repository` が 4 と同じで、preflight が installed の plugin を見つけて通ることを見る。
6. `tq plan`（`--plugin-dir` なし）で planner を開き、「`smoke.txt` に 1 行足す task を 1 件、`--verify 'test -f seed.txt'` で」と頼む。planner が AGENTS.md の無い repository で verify・paths・evidence を決めて `submit` し、plan review が pass して task が ready になり、supervisor が claim することを見る（plan review が concern なら inbox の `approve_plan` に `ready` と答える）。
7. run が着地したら確かめる。
   - `tq show ID`: task `completed`、run `integrated`。
   - `master` に 1 つの squash commit が乗り、checkout が追従して clean。`main` の branch は作られていない。
   - `tq events --run RUN --full`: `push_skipped`（`remote: origin`・`branch: master`・`reason: the repository has no remote origin`）があり、`push_failed` の attention が inbox に出ていない。`migration_renumbered` と `migration_number_taken` が無い。
   - `tq stats` と `tq kpi` が止まらずに出て、cargo 専用の計測（`work_breakdown` の `llvm_cov` など、`kpi --by toolchain`）が無い。
   - worker・review・plan review の prompt（run dir と proposal の dir）に、dagq の repository に固有の規則（cargo・llvm-cov・e2e の推奨、ADR の番号、`AGENTS.md` を名指すこと）が載っていない。`[e2e] paths` が無く task が `e2e` を要らないので、worker の prompt に e2e の行（「E2E: do not run the e2e」）が無く、run に `run_e2e_*` の event が無い。
8. [故障経路のスモーク](#シナリオ)の後始末と同じく `tq down --wait` で supervisor を止め、inbox と planner の workspace を閉じ、`cmux workspace-group delete '[dagq-smoke]' --close-workspaces` で group を消す。`claude plugin uninstall claude-dagq@dagq --scope local` で入れた plugin を外す。

### 結果

まだ行っていない（task 629 は worker として動き、使い捨ての queue を作れず、plugin も入れられないため。receipt の `follow_ups`（`ops`）で人か inbox に任せた）。流したら、この節に版（dagq・plugin・Claude Code・cmux）と確認点の結果を足す。
