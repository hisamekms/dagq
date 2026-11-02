---
id: plan-role-eval-snapshot-spike
type: plan
title: スパイク：判定を返す 4 つの役割の job を固定した snapshot だけを入力に本番と同じ組み立てと起動経路で起動し、本番 queue・checkout・元の判定・期待を読ませない形が既存の部品の小さな拡張で作れるか
status: completed
created: 2026-11-03
owners:
  - hisamekms
tags:
  - planning
  - investigation
  - security
related:
  - design-agent-eval
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-goal-review
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-triage
  - design-supervisor-lifecycle-headless-job-processes
  - design-queue-service
---

# スパイク：役割の job の eval を固定した snapshot で起動できるか

問いは、判定を返す 4 つの役割（plan review・goal review・run の review の全体の判定・復旧の job）の job を、eval のケースとして固定した snapshot だけを入力に、本番と同じ prompt の組み立てと起動経路（headless の job の共通の経路・`AgentProvider` の起動の組み立て・`application::AgentReads`）で起動し、本番 queue・main checkout と Git directory・元の判定・期待を読めない形が既存の部品の小さな拡張で作れるか。
結果は goal 246 の draft（3948〜3956）の再計画が読む。
方法はコードを読むことと、run の中だけの一時の unit test（commit していない）で、実 provider は起動していない。

## 判定

**unresolved（全体）。** 役割ごとの判定は次のとおり。

| 役割 | 判定 | 一言の根拠 |
| --- | --- | --- |
| plan review | unresolved | Codex は拡張 E1〜E4・E6 で holds の見込み。Claude の見せない境界（E5）は provider の経路で確かめていない |
| goal review | unresolved | 材料が全て JSON の値で snapshot に最も向く。Claude の境界は plan review と同じく未確認 |
| run の review の全体 | unresolved | 起動の形が agent-eval のケースの job に最も近い（tree と材料の file）。Claude の境界は同じく未確認 |
| 復旧の job | does_not_hold | cwd が queue の data dir の中の run directory で、prompt が run directory の file と live の worktree・process を読ませる。その時点の run directory を残す部品が無い |

全体を unresolved にしたのは、3 役割の残りが同じ 1 点（Claude の job の読み取りを path で限れるか）に絞れ、それが permission の前提で、この run の経路（実 provider を起動しない）では確かめられないため。
Codex の側は、既存の部品（`agent_reads_config`・`case_reads`・agent-eval の tree）を役割の job に付ける拡張で作れることを一時の test で確かめた（下の「打ったもの」）。

調べる中で、eval の今の case の job と本番の役割の job の両方に関わる穴を 1 つ見つけた（「(b) 追加の dagq の読み取り」の「socket に届く job は role を外すと利用者になる」）。後続の設計の前提を変えるので、判定とは別に後続への推奨に書く。

## 条件

- 対象の commit: `74b6ea2eb37bc396247f058e5dd2ec6faa2f4e78`（この run の base）。
- 道具: rustc 1.99.1・cargo 1.99.1、固定バイナリ `dagq 0.4.0-dev+f3cc2e8fd`（版の確かめだけで、queue には打っていない）。
- 環境: macOS 27.2（arm64）、dagq の worker の run（Claude、headless）の worktree。
- provider: 起動していない。Codex・Claude の起動は、argv と env を組む関数を unit test で呼んで調べた。

## 打ったもの

読んだ file:

- prompt の組み立て: `src/application/prompt/plan_review.rs`（plan review と goal review の両方）・`run_review.rs`・`replan_diagnosis.rs`
- 材料の集め方と起動: `src/application/supervise/planning/plan_review.rs`・`goal_review.rs`、`src/application/supervise/execution/landing.rs`・`recovery.rs`・`triage.rs`・`agent_eval.rs`（`case_reads`）
- 起動の組み立て: `src/application/agent_job.rs`、`src/application/ports/execution.rs`（`AgentJobLaunch`・`AgentReads`・`AgentProvider`）、`src/application/actor_executor.rs`（`actor_env`・`client_env`・`ActorProgram::Headless`）、`src/infrastructure/codex.rs`（`headless_command`・`agent_job_command`・`agent_reads_config`・`reach_queue_service`）、`src/infrastructure/adapters.rs`（Claude の `headless_command`・`review_command`・`agent_job_command`・`claude_tools`）
- 権限と client mode: `src/domain/authorization.rs`（`grants`・`resource_permits`）、`src/domain/queue_service.rs`（`client_role`）、`src/infrastructure/queue_service.rs`（`Client::from_env`・`socket_path`）、`src/main.rs`（`unserved`・`unserved_refusal`）、`src/application/queue_service.rs`（`authenticate` と test `no_token_is_the_user_only_from_the_owner_s_peer_with_no_role_named`）
- 文書: [agent-eval](../design/agent-eval.md) の「evalが本番と共有する起動経路」、[plan-review](../design/supervisor-lifecycle/plan-review.md)・[goal-review](../design/supervisor-lifecycle/goal-review.md)・[review](../design/supervisor-lifecycle/review.md)・[triage](../design/supervisor-lifecycle/triage.md)・[headless-job-processes](../design/supervisor-lifecycle/headless-job-processes.md)

一時の unit test（2 回に分けて 6 つ。どれも `cargo test --locked --lib spike_tmp -- --nocapture` で流し、結果をここに写してから消した）:

1. prompt（`src/application/prompt/tests.rs` に追加、役割ごとに 1 つの組み立て）: 4 つの役割の本番の組み立て（`plan_review_prompt`・`goal_review_prompt`・`review_prompt`・`recovery_prompt`）を、repository の path を `/snap/case-1/tree`、review の材料を `/snap/case-1/material/review.md` にした値だけで呼んだ。
   - 4 つとも I/O なしで組み立った（どの組み立ても材料の値と path の文字列だけを読む）。
   - plan review: 21,263 byte、snapshot の path を含む。prompt が促す dagq の読み取りは `show`・`proposal show`・`search`・`related`・`findings`・`stats`・`events --full --task ID`・`timeline RUN`・`lint`・`goal show` など 17 個。節の byte は instructions 16,421・tasks 3,808・summaries 504・hotspots 320・candidates 119・expected_files 61・goals・full_text・other_proposals・precedents・lint 各 6。
   - goal review: 3,697 byte、snapshot の path を含む。促す読み取りは `show ID`・`goal show 7 --full`・`findings`・`events --goal 7 --full`・`search ...`（このほか材料が limit で切れたときに `events --full --goal ID --kind goal_review_finished` を名指す。`plan_review.rs` の `GOAL_REVIEW_READS`）。
   - run の review の全体: 7,457 byte、dagq の読み取りを促さない。読む場所は review の材料の path（引数のまま）と cwd の tree。
   - 復旧の job: 5,597 byte、dagq の読み取りを促さない。run の run directory（`TaskRun::run_dir`、test では `/runs/run`）と receipt を名指す。
2. 起動の env（`src/application/actor_executor.rs` の test module に追加）: plan review・goal review・復旧の job の actor を、`ActorProgram::Headless` の `HeadlessProgram::Job` で fake の provider と queue service に起動した。
   - 3 つとも token を発行し（`issued`）、終わりで返す（`revoked_on_exit`）。env は `DAGQ_ROLE`・`DAGQ_ACTOR_ID`・`DAGQ_SERVICE_SOCKET`・`DAGQ_SERVICE_CREDENTIAL_FILE` で、`DAGQ_QUEUE` を外す。
   - `without_env` に socket と token の名を足すと env からは外れる（`removed` に入る）が、token の発行は同じく起き、`provider.reach_queue_service` も呼ばれる（`actor_executor.rs` の `ActorProgram::Headless` の枝の `client_role` の分岐。`without_env` はその後に env を消すだけ）。
   - run の review の actor（`review_job`）は `HeadlessProgram::Job` では起動を拒まれた（role ごとに起動できる program が決まっている）。そこで本番の経路の `HeadlessProgram::Review` で別に流した（4.）。
3. Codex の permission profile（`src/infrastructure/codex.rs` の test module に追加、既存の `case_layout` を使う）: plan review の `headless_command(tree, ReadFilesAndQueueCli)` に、`agent_job_command` と同じ手順で `agent_reads_config` を当てた（拡張 E1 の試作）。
   - 今の役割の job の argv は `exec --json --skip-git-repo-check --sandbox read-only -C <cwd>` で、読み取りの制限が無い（macOS の read-only の sandbox は全域を読ませる。[codex-headless-jobs-spike](codex-headless-jobs-spike.md) の 1.）。
   - 試作の profile の filesystem の表は、tree と材料が read、main checkout の file・Git directory・queue の DB・socket の file・token の file・ほかの run の receipt が none。
   - `reach_queue_service` の前は network が閉じていて、呼ぶと `features.network_proxy=true`・`network.enabled=true`・socket の path の `unix_sockets` の allow が足される。filesystem の none とは別の欄なので、data dir を読めなくしても socket には届く。
4. run の review の全体の起動の env（`src/application/actor_executor.rs` に追加）: `review_job` の actor を本番と同じ `HeadlessProgram::Review { run, tree, prompt, access: ReadFiles }` で fake の provider に起動した。
   - provider の `review_command` が呼ばれ（fake の argv は `r1 read_files p`）、token を発行し、env は `DAGQ_ROLE=review-job`・`DAGQ_ACTOR_ID`・`DAGQ_SERVICE_SOCKET`・`DAGQ_SERVICE_CREDENTIAL_FILE` で、`DAGQ_QUEUE` を外す。
   - `without_env` に socket と token の名を足しても、2. と同じく env から外れるだけで token は発行される。役割の 4 つとも、起動の env と token は同じ形である。
5. Claude の起動の argv（`src/infrastructure/adapters.rs` の test module に追加）: 役割の job の本番の組み立てを呼んだ。
   - plan review・goal review（`headless_command`、`ReadFilesAndQueueCli`）: `-p --output-format stream-json --verbose --allowedTools Read Grep Glob Bash(dagq:*) --disallowedTools ScheduleWakeup,CronCreate`。`--add-dir` も settings も path の規則も無い。
   - 復旧の job（`headless_command`、`ReadFiles`）: 上から `Bash(dagq:*)` を除いたもの。path の規則は無い。
   - run の review の全体（`review_command`、`ReadFiles`）: `--debug-file <run dir>/claude-review.log --add-dir <run dir> --settings <run dir>/claude-review-settings.json --allowedTools Read,Grep,Glob --disallowedTools Bash,Edit,Write,NotebookEdit,ScheduleWakeup,CronCreate --setting-sources ""`。`Read`・`Grep`・`Glob` に path の規則は無い。起動の組み立てが run directory に `claude-review-settings.json` を書き、その `permissions.deny` は `Bash(dagq watch:*)` などの dagq の command の拒否だけを持つ。
   - どの役割も、queue の data dir と main checkout を読ませない引数は無い。拒めない境界はここで、E5 の path の拒否の規則を足す場所の候補は、run の review では今ある settings の `permissions.deny`、ほかの 3 つでは `--disallowedTools` の引数である。それが print mode で効くかは、provider を起動しないこの run では確かめられない。
6. Codex の run の review の全体と復旧の job（`src/infrastructure/codex.rs` に追加、`case_layout` を使う）。
   - goal review の今の argv は plan review と同じ（同じ `headless_command` と access）で、3. の結果がそのまま当たる。
   - run の review の全体: `review_command` は `--sandbox read-only` と tree の distrust の `-c` を持つ。`--sandbox` を外して case の `agent_reads_config` を当てた（E1 の試作）。tree の file と材料の `review.md` は read、main checkout の file・queue の DB・case の run の dir の log は none になった。
   - 復旧の job: cwd の run directory（data dir の `runs/r1`）は、case の読める場所のままだと none で、cwd そのものを読めない。run directory を readable に足すと、その run の receipt と出力は read、ほかの run の receipt と queue の DB は none になった。ただし本番の run directory は job の後に書き足される同じ dir で、snapshot に固定した写しではない（下の「復旧の job」）。

使った時間: 約 3.3 時間（上限 4 時間、review の差し戻しの後の追加を含む）。試作は、prompt の組み立てが役割ごとに 1 つ、起動が役割ごとに env・Codex・Claude の各 1 つ（plan review と goal review は同じ起動の関数で、1 つの試作に含む）。

## 役割 × prompt の節

「出どころ」は、本番の材料の集め方がその節を queue から読むか repository（main checkout・Git・run の worktree）から読むか。
「そのまま渡せるか」は、snapshot に固定した値を今の組み立ての関数にそのまま渡せるか。
要る拡張の記号は下の「後続への推奨」の E1〜E6。

### plan review（`plan_review_prompt`、材料は `PlanReviewMaterial`）

| 節 | 出どころ | そのまま渡せるか | 要る拡張と file:行 |
| --- | --- | --- | --- |
| instructions | 固定の文と repository の path（`{repo}`）。job に repository の規則と文書を読ませる | 渡せる（`repo_root` を tree の path にする） | E6（tree）。`src/application/prompt/plan_review.rs:71`・`:1123` |
| tasks | queue（`show`） | 型は渡せるが、`TaskDetail`・`Task` が `Deserialize` を持たず file から読めない | E3。集める所 `src/application/supervise/planning/plan_review.rs:584`、型 `src/domain/views.rs:174`・`src/domain/task.rs:99` |
| goals | queue（`show_goal`） | 同上（`Goal` に `Deserialize` が無い） | E3。集める所 `src/application/supervise/planning/plan_review.rs:613`、型 `src/domain/goal.rs:31` |
| full_text・summaries | queue（ready と in_progress の `list`） | 渡せる（`TaskListItem` は `Deserialize` を持つ。`src/application/mod.rs:151`） | E3（固定の時点）。集める所 `src/application/supervise/planning/plan_review.rs:628` |
| other_proposals | queue（`proposals` と各 task の `show`） | `Proposal` は渡せる（`src/domain/proposal.rs:75`）、`Task` は E3 | E3。集める所 `src/application/supervise/planning/plan_review.rs:617`〜`624` |
| candidates | queue（`related`・`search`） | 渡せる（`RelatedTask`・`SearchHit` は `Deserialize` を持つ。`src/domain/related.rs:113`・`src/domain/search.rs:236`） | E3（固定の時点）。集める所 `src/application/supervise/planning/plan_review.rs:641`、`duplicate_candidates` は `src/application/supervise/planning/plan_review.rs:724` |
| precedents | queue（`answered_asks`） | 渡せる（`Ask` は `Deserialize` を持つ。`src/domain/mod.rs:1437`） | E3（固定の時点）。集める所 `src/application/supervise/planning/plan_review.rs:637` |
| expected_files | queue と Git（宣言の path、関係する着地の変更、in_progress の run の変更） | 値は渡せる。作り直しには queue と Git が要る | E3（集めた値を固定）。`src/application/supervise/planning/plan_review.rs:674` |
| hotspots | queue の stats と main（まだある file） | `ConflictHotspot` に `Deserialize` が無い | E3。集める所 `src/application/supervise/planning/plan_review.rs:638`・`:776`、型 `src/domain/stats/conflicts.rs:273` |
| lint | queue（`lint_input`）から domain の lint | `LintViolation` に `Deserialize` が無い | E3。集める所 `src/application/supervise/planning/plan_review.rs:615`、型 `src/domain/lint.rs:74` |
| replacement | queue（置き換えの計画）と data dir の中の保存した作業の path | 計画は渡せる（`ReplanBrief`）、path は data dir を指す | E3（保存した作業を材料の dir に写す）。集める所 `src/application/supervise/planning/plan_review.rs:646`・`:2673`、欄 `src/application/prompt/plan_review.rs:78` |

本番の材料は「今の queue」から集める（`src/application/supervise/planning/plan_review.rs:572` の doc）。過去の job から作り直す読みは無く、残るのは job の `prompt.txt`（`plan_review.rs:493`）だけで、これには本番の main checkout の path が入っている（E4）。

### goal review（`goal_review_prompt`、材料は `GoalReviewMaterial`）

| 節 | 出どころ | そのまま渡せるか | 要る拡張と file:行 |
| --- | --- | --- | --- |
| instructions | 固定の文と repository の path。job に main checkout の文書と source を読ませる | 渡せる（`repo_root` を tree に） | E6。`src/application/prompt/plan_review.rs:1172`・`:1378` |
| goal | queue（`show_goal`） | 渡せる（`serde_json::Value`） | E3（固定の時点）。集める所 `src/application/supervise/planning/goal_review.rs:332` |
| tasks（landed を含む） | queue と run directory の receipt（`landed`） | 渡せる（`Value`） | E3。集める所 `src/application/supervise/planning/goal_review.rs:341`、`landed` は `src/application/supervise/planning/goal_review.rs:399` |
| replacements | queue（置き換えの系譜） | 渡せる（`Value`） | E3。集める所 `src/application/supervise/planning/goal_review.rs:335`・`:367` |
| follow_ups | queue（所属の判定） | 渡せる（`Value`） | E3。集める所 `src/application/supervise/planning/goal_review.rs:388` |
| events | queue（goal の note と edit） | 渡せる。固定の時点より後の event を除く | E3（時点で切る）。`goal_review.rs:372` |
| previous | queue（`goal_reviews`。前の goal review の判定） | 渡せる。ケースの元の判定とそれより後を除く | E3（時点で切る）。`goal_review.rs:378` |
| gaps_in_a_row | queue（続いた gaps の数） | 渡せる（数） | E3。`src/application/supervise/planning/goal_review.rs:391` |
| language | 設定 | 渡せる | なし。`src/application/supervise/planning/goal_review.rs:394`（`with_language`） |

材料が全て `Value` なので、job の起動の時に集めた値をそのまま file に固定して読み直せる。

### run の review の全体（`review_prompt`）

| 節 | 出どころ | そのまま渡せるか | 要る拡張と file:行 |
| --- | --- | --- | --- |
| title・acceptance | queue（`Task`） | 型は渡せるが `Task` に `Deserialize` が無い | E3。集める所 `src/application/supervise/execution/landing.rs:1534`・`:1538` |
| rules | landing branch の commit の `.dagq/review/builtin-acceptance.md`（repository）と、run directory に書いた全文の path | 渡せる（本文と path の文字列） | E6（全文を材料の dir に置く）。`src/application/supervise/execution/landing.rs:1554` |
| answers | queue（`approve_landing` の答え） | 型は渡せるが `PersonAnswers` に `Deserialize` が無い | E3。集める所 `src/application/supervise/execution/landing.rs:1537`（`src/application/review.rs:187`）、型 `src/application/review.rs:211` |
| known_failures | landing branch の CI の記録 | `prompt::KnownFailures` に serde が無い | E3。集める所 `src/application/supervise/execution/landing.rs:1542`、型 `src/application/prompt/known_failures.rs:52` |
| 材料の file（commit と全差分） | Git から作った run directory の `review.md` | 渡せる（path の文字列） | E6（材料の dir に置く）。作る所 `src/application/supervise/execution/landing.rs:1529`、組み立ての引数 `src/application/prompt/run_review.rs:96` |
| tree（cwd） | run の worktree | 渡せる（`HeadlessProgram::Review` の `tree`） | E6（agent-eval のケースの tree を使う）。起動 `src/application/supervise/execution/landing.rs:1673` |
| run directory | Claude の `review_command` が settings・debug の file を書く | `TaskRun::run_dir` を case の dir にする必要がある | E1。`src/infrastructure/adapters.rs:4280` |

### 復旧の job（`recovery_prompt`、材料は `RecoveryMaterial`）

| 節 | 出どころ | そのまま渡せるか | 要る拡張と file:行 |
| --- | --- | --- | --- |
| task（title・description・acceptance・verification） | queue | `Task` は E3 | E3。組み立て `src/application/prompt/run_review.rs:965` |
| facts | queue（`recovery_requested` の payload） | 渡せる（`Value`） | E3。引数 `src/application/supervise/execution/recovery.rs:1179` |
| binary（supervisor の build と commit） | supervisor の build の識別子（`layout.version`） | 文字列は渡せる。`BinaryFacts` に `Deserialize` が無い | E3。集める所 `src/application/supervise/execution/recovery.rs:1229`〜`1252`、節 `src/application/prompt/run_review.rs:848`、型 `src/application/prompt/run_review.rs:696` |
| dependencies（依存先の着地と、build がその commit を含むか） | queue（依存先の task の event の着地の commit）と repository（`is_ancestor`） | 着地の commit と含むかの値は渡せる（`DependencyLanding`、`Deserialize` が無い）。作り直しには build の commit を含む repository が要る | E3（含むかを判定した値を固定）。集める所 `src/application/supervise/execution/recovery.rs:1235`〜`1241`・`:1250`、節 `src/application/prompt/run_review.rs:887`、型 `src/application/prompt/run_review.rs:677` |
| replacements（claim の後の binary の入れ替え） | queue（`update_installed`・`supervisor_handed_off` の event） | 値は渡せる（`Value` の列）。claim の後の時点で切る | E3（時点で切る）。集める所 `src/application/supervise/execution/recovery.rs:1234`・`src/application/prompt/run_review.rs:728`、節 `src/application/prompt/run_review.rs:924` |
| screen | run directory の turns（その時点の最後の turn） | 値は渡せる。job はさらに run directory の file を読む | 作り直せない（下の理由）。集める所 `src/application/supervise/execution/headless.rs:365` |
| ended | 終わった run の error・receipt・log・event（run directory と queue） | 値は渡せる。job はさらに run directory の file を読む | 作り直せない。組み立て `src/application/prompt/run_review.rs:365` |
| processes | host の live の process | 値は渡せる（`ProcessInfo`） | 作り直せない（その時点の host）。集める所 `src/application/supervise/execution/recovery.rs:1141`、節 `src/application/prompt/run_review.rs:1068` |
| git_status・head・receipt の commit | run の worktree（live） | 値は渡せる。job はさらに worktree を読む | 作り直せない。集める所 `src/application/supervise/execution/recovery.rs:1275` |
| history | queue（前の復旧の判定と自動の直し） | 渡せる（`Value`）。時点で切る | E3。集める所 `src/application/supervise/execution/recovery.rs:1307`、節 `src/application/prompt/run_review.rs:1111` |
| allowed | domain の判断 | 渡せる | なし |

job の cwd は run directory で（`src/application/supervise/execution/recovery.rs:1419`、run directory は queue の data dir の中）、prompt は run directory の receipt・log・turns と worktree を名指す。
run directory は job の後も turn・receipt・後の復旧の判定の file が増え、その時点の中身を残す部品は無い。
data dir を読めなくする（E1）と cwd そのものが読めない。
同じ actor の役割の replan の診断（`replan_diagnosis_prompt`）も run の turn と review を材料にし、同じ形になる。

## 問いごとの判定と根拠

### (a) 節の出どころと snapshot に固定して渡せるか

- 4 つの組み立てはどれも I/O をしない純粋な関数で、材料の値と path の文字列だけから prompt を作る（試作 1）。repository の path を tree の path に変えれば、prompt はその path を名指す。
- 固定の障害は 2 つ。一つは値の型で、goal review は全て `Value` で file に固定できるが、plan review・run の review・復旧の job は `Task`・`Goal`・`TaskDetail`・`LintViolation`・`ConflictHotspot`・`PersonAnswers`・`KnownFailures`・`BinaryFacts` が file から読めない（E3）。もう一つは時点で、本番は「今」の queue から集め、過去の job の材料は残らない（E4）。
- 復旧の job は、材料の値を固定しても、job が読みに行く run directory と worktree のその時点の中身が残らない。

### (b) prompt が促す追加の dagq の読み取り

- 本番の 4 つの役割は client role（`src/domain/queue_service.rs:2303`）で、起動のたびに token を発行され、`DAGQ_SERVICE_SOCKET` と `DAGQ_SERVICE_CREDENTIAL_FILE` を env に持ち、Codex は `reach_queue_service` で socket への network を開く（試作 2・3）。権限は queue 全体の `QueueRead`（`src/domain/authorization.rs:693`〜`700`）で、元の判定の event（`plan_review_finished`・`goal_review_finished`・`review_finished`・triage の event）も読める。
- 拒む: 作れる。token を発行せず socket を渡さない起動（E2）にすると、`DAGQ_ROLE` が job の role のままの dagq は queue を開かずに拒まれる（`src/main.rs:4372` の `unserved` と `unserved_refusal`、fail closed）。ただし `DAGQ_ROLE` を外した（空にした）dagq は利用者として動き、cwd か `--db` で queue を解けば socket の既定の path へ送る。Codex は E2 で network が閉じたままなので届かないが、sandbox の無い Claude では、Bash の許可が role を外す command に当たらないことに拠る（下の (d) の Claude、確かめていない）。`without_env` だけでは足りない（試作 2: token は発行され、Codex の network は開く）。
- snapshot から供給する: 既存の部品では作れない。queue service は本番の DB を開き、時点で切った読み取り専用の queue を作る部品は無い。使い捨ての queue を作るのは worker も job もできない（AGENTS.md の境界）。
- 材料に前もって含める: 作れる。組み立ては今でも主な読み取りを材料の節として含めており（試作 1 の節）、limit を超えた分だけを dagq の読み取りに回す。
- 推奨: **拒む＋材料に前もって含める**。prompt は本番と同じに保ち（組み立てを変えない）、促す読み取りは拒まれる。拒まれたときの判定の質はこの Spike の対象でなく、3956 の周の成績が測る。plan review は促す読み取りが 17 個と最も多く、拒むことの影響が最も大きい。

### socket に届く job は role を外すと利用者になる

- queue service は、token が無く role も名乗らない呼び出しを、socket の相手が同じ OS の利用者なら利用者として受ける（`src/application/queue_service.rs` の `authenticate` の doc と test `no_token_is_the_user_only_from_the_owner_s_peer_with_no_role_named`）。
- Codex の job は `reach_queue_service` で socket に届き、shell を持つ（read-only の sandbox は command を許す）。`DAGQ_ROLE` と `DAGQ_SERVICE_CREDENTIAL_FILE` を空にして `dagq` を打てば、利用者として読める（書き込みの use case も通る見込み）。
- agent-eval の今の case の job も role は `review_job`（client role、`src/domain/actor.rs:204`）で、同じく token の発行と `reach_queue_service` を受ける（`codex.rs` の既存の test `a_cases_agent_job_is_a_reviews_agent_job_with_its_reads_profile` が network が開くことを確かめている）。profile は token の file を読ませないが、role を外す経路は残る。
- `src/application/agent_job.rs` の module doc（「it is given no way to read the queue」）と `landing.rs` の注記（「given no way to read the queue, as the eval's is」）はこの起動と合わない。
- 実 provider で打っていないので、Codex の sandbox の中から role を外して届くかは、コードからの判断である。

### (c) 参照する repository の版

- 置ける。agent-eval の tree（`agent_eval.rs` の tree の作り方、[agent-eval](../design/agent-eval.md) の「evalが本番と共有する起動経路」）と同じく、job の時点の main の commit の tree を自分の Git として置き、`repo_root`（plan review・goal review）と `tree`（run の review）にその path を渡せばよい。
- 足すもの: plan review と goal review の job は起動の時の main の head を記録していない（2 つの module に main の head の読みが無い）。ケースを作るには起動の時の commit を材料に残す（E4）。run の review は run の base と head と landing branch の commit を持つので、agent-eval の `base_commit` のケースと同じに作れる。
- 復旧の job は repository でなく run の worktree と run directory を読むので、commit の tree では代わらない。

### (d) 見せない境界

prompt: 組み立ては材料に元の判定を入れない（goal review の `previous` と `events` は時点で切れば元の判定を含まない）。期待のラベルを入れる節は無い。ただし goal review の prompt は limit を超えると `events --full --goal ID --kind goal_review_finished` を名指す（拒めば届かない）。

tree: agent-eval と同じく自分の Git の tree にすれば、本番の object と ref に届かない。

env: 試作 2 のとおり、今の起動は socket と token の file の path を渡す。E2 で両方を渡さない。

Codex: E1 で `agent_reads_config` を役割の job に当てると、main checkout・Git directory・queue の DB・socket の file・token の file・ほかの run の file が none になる（試作 3）。ただし network の socket の allow は filesystem とは別で、`reach_queue_service` を呼ぶと data dir を読めなくしても socket に届く。E2 で呼ばなければ network は閉じたまま。両方を足せば holds（コードと profile の表で確かめた。profile そのものの効き方は `agent_reads_config` の doc が codex-cli 0.162.1 の `codex sandbox` で確かめた結果による）。

Claude: 役割の job の `headless_command` は `--add-dir` も読める場所の制限も持たず、`--allowedTools` の `Read`・`Grep`・`Glob` を path で限らない（`src/infrastructure/adapters.rs:4213`・`claude_tools`）。plan review と goal review には `Bash(dagq:*)` も許す。run の review の全体の `review_command` は run directory を `--add-dir` に足し、`Bash` を拒むが、`Read`・`Grep`・`Glob` はやはり path で限らない（試作 5、`src/infrastructure/adapters.rs:4280`）。
env から socket と token を外し、token を発行しなければ（E2）、`dagq` は job の role のまま拒まれ、本番には届かない。Bash の許可は `dagq` で始まる command だけなので、`DAGQ_ROLE` を外す command は許可に当たらない見込みだが、Claude Code の前置きの env の扱いは確かめていない。
queue の data dir（run directory の receipt・review の判定・plan review の prompt と出力）のファイルを `Read` で読む穴は、起動の引数に制限が無いので残る。E5 の path の拒否の規則で塞げる見込みだが、print mode でそれが効くかはこの run の経路で確かめられず、unresolved とした。

## 後続（3948〜3953）への推奨

拡張の部品（どれも既存の部品を役割の job に広げるもので、新しい起動の経路は作らない）:

- E1 読める場所の欄を役割の job へ: `HeadlessProgram::Job` と `HeadlessProgram::Review` に `AgentReads` の欄を足し、Codex の `headless_command`・`review_command` で `agent_job_command` と同じく `agent_reads_config` を当てる（`src/infrastructure/codex.rs:600`〜`616` の手順）。run の review は `TaskRun` の run directory を case の dir にする。
- E2 queue に届かない起動: `ActorProgram::Headless` に、token を発行せず socket を渡さず `reach_queue_service` を呼ばない形を足す（`src/application/actor_executor.rs` の `client_role` の分岐）。`DAGQ_ROLE` は job の role のまま残し、dagq を fail closed にする。agent-eval の case の job にも同じ形を当てる。
- E3 材料の固定: 役割ごとに組み立ての入力を 1 つの JSON にできる形にする。goal review はそのまま、plan review・run の review は `Task`・`Goal`・`TaskDetail`・`LintViolation`・`ConflictHotspot`・`PersonAnswers`・`KnownFailures`（と復旧の `BinaryFacts`）に読み戻しを足すか、組み立ての入力だけの型を挟む。
- E4 時点の記録: 本番の job の起動の時に、集めた材料と main の head の commit を job の dir に残す（今は `prompt.txt` だけ）。過去の job からはケースを作れないので、ケースはこの記録が始まってからの job から作る。
- E5 Claude の読み取りの拒否: case の役割の job に `AgentReads` の unreadable から `Read`・`Grep`・`Glob` の path の拒否の規則を足し、`Bash(dagq:*)` を外す。効くかは provider の経路で確かめる Spike が先に要る。
- E6 tree と材料の dir: agent-eval の tree の作り方（自分の Git、ほかの ref 無し）を使い、review の材料・rules の全文・plan review の保存した作業を材料の dir に置く。

計画への推奨:

- 3948（ADR）: snapshot に含む材料は E3 と E4 の記録、repository の版は E4 が残す commit の tree（E6）、追加の読み取りは「拒む＋材料に前もって含める」（E2）、見せない境界は Codex が E1＋E2、Claude は E5 の結果しだい、と書く。復旧の job は does_not_hold なので、goal の acceptance 2 から外すかは人の判断になる（`planner_question`）。外さない場合は、その時点の run directory を写す部品を足すかを別の Spike で問う。
- 3949〜3953（harness）: 新しい起動の組み立てを作らず、E1〜E3 と E6 を順に足す形に書き直す。E2 は agent-eval の今の case の job の穴も塞ぐので、役割の eval より先に置く価値がある。
- 次の Spike: 残った点は Claude の print mode で path の拒否の規則（E5）が data dir の file の `Read`・`Grep`・`Glob` を拒むか、の 1 つ。実 provider を起動する経路で確かめる必要がある。
- 判定と別に、socket に届く job が role を外すと利用者になる穴は、本番の役割の job（Codex）と agent-eval の case の job に今ある。eval の計画とは別に直すかを planner が決める。
