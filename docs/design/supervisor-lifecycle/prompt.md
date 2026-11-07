---
id: design-supervisor-lifecycle-prompt
type: design
title: "Prompt"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1566-1
  - adr-t1818-2
  - adr-t2072-1
  - adr-t2080-1
  - adr-t1892-1
  - adr-t1428-1
  - adr-t1942-2
  - adr-t1453-2
  - adr-t1420-1
  - adr-t963-1
  - adr-t1165-1
  - adr-t1233-2
  - design-supervisor-lifecycle
  - adr-0009
  - adr-0038
  - adr-0029
  - design-supervisor-lifecycle-language
  - design-supervisor-lifecycle-headless-worker
  - adr-t813-1
  - adr-t1433-2
---

# Prompt

runtimeがagentに渡す文（workerの`prompt.txt`と依頼、headlessのjobとruntimeのplannerのprompt）の組み立て方の地図。
文面そのものは`src/application/prompt/`の定数と組み立ての関数が持ち、この文書は節の目的・組み立ての順・約束だけを書く。

## 目的

- workerが最初のcommitまでにqueueやdocs全体を読まずに済むよう、runに要る情報をclaimの時点でpromptに載せる。
- runtimeはどのrepositoryでも動くので、検証・paths・ADRの規則のようなrepositoryの規則を持たず、sessionをrepositoryの指示（AGENTS.mdかCLAUDE.md）へ向ける。
- headlessのjob・runtimeのplanner・workerのpromptは判断の材料だけを上限の中で載せ、残りは読む方法を示して取りに行かせる（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)、workerは[ADR-t2072-1](../../adr/2026-10-08-t2072-1-worker-prompts-and-next-turns-carry-decision-material-within-limits.md)）。

## 流れ

```text
claim ──> prompt::prompt ──> prompt.txt（1回だけ書く）
            │                └─ 節ごとの上限 → wrapper_launchedのprompt_bytes
            ├─ task・検証・evidence・paths・e2eの行
            ├─ Goal / Context / Predecessor tasks / Sibling tasks（常に4節）
            ├─ 受け入れ条件の対応づけ → 文書の照合 → receiptの契約
            └─ 最後の段落（非対話のturn、providerの1行）＋言語の指示
claim ──> 指示の版のhash ──> run_claimed

runの途中 ──> resume・revise・促し・askの答え・復旧jobの指示 ──> 次のturnのprompt
                 └─ 節ごとの上限 → turn_requestedのprompt_bytes

supervisorのjob ──> plan review・observer・goal review・スループットの見直し・runのreview・復旧job
                     └─ 節ごとの上限 → promptのbyte数をeventに記録 → stdinで渡す
runtimeのplanner ──> 節ごとの上限（prompt_fit::Fit）→ 記録 → 非対話のturnのコマンドで渡す
```

## 責務と境界

- `src/application/prompt/`: 全てのpromptと依頼の文面と組み立て。
  queueもファイルも直接は読まず、呼び出し側が集めた材料を文にする。
- `src/application/prompt_fit.rs`: 節ごとの件数とbyteの上限、切り方、省いたことの注記、`PromptBytes`の集計。
- 材料を集めるのはsupervisorとapplicationの各use case（claim、plan review、復旧job、plannerの起動）で、queueの読み取りは`TaskReads`の操作を使う。
- promptをagentに渡すのはproviderの層（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
- このrepositoryに固有の規則（文書の層と予算、検証の選び方）はpromptに書かず、`docs/development/`が持つ（[ADR-t1453-2](../../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）。

## 不変条件

- `prompt.txt`はclaim時点のスナップショットで、runの途中に書き換えない。
  goalの編集や兄弟taskの変化は次のclaimのpromptから届く。
- Goal・Context・Predecessor tasks・Sibling tasksの4節は常に書き、該当が無ければ`none`にする（[ADR-0009](../../adr/0009-goal-groups-tasks.md)）。
  promptの形はgoal・context・依存・並列の有無で変わらない。
- 依存元のreceiptが読めないことや着地が無いことは文で示し、provisionを止めない。
- promptと固定の文字列はdagqのrepositoryの規則も特定のツールの名前も名指さない。
- workerに送る文は全て非対話のsessionの文で、1 turnが1回の呼び出し、`/exit`も画面への打ち込みも無い（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
- headlessのjobのpromptは引数で渡さず、どれも全体の上限を持つ。
- Claudeのworkerとplannerのturnのprompt（`prompt.txt`も次のturnの文も）もargvに載せず、stdinで渡す（仕組みは[Agent provider lifecycle](../provider-lifecycle.md#workerのproviderと経路)）。
  promptは検証コマンドの名前やtaskのtitleを含むので、argvに載せると、workerが自分の検証を止めるつもりで打った`pkill -f`・`kill $(pgrep -f ...)`のパターンが他のrunのsessionのcommand lineにも一致し、それを止める。
  `permissions.deny`が拒めるのは`pkill`・`killall`で始まるコマンドだけなので、argvから本文を外して当たらなくする。

## workerのprompt

- 知りたいこと→入口:
  - 全体の組み立て: `prompt::prompt`。
  - 最初に読むものの限定: `WORKER_READING`。
  - 手元で流す検証: `local_checks`。
  - 依存元の行: `PredecessorSummary`。
  - 兄弟taskの行: `siblings_in_progress`。
  - e2eの行: `e2e_line`。
  - follow_upの所属の提案: `FOLLOW_UP_PROPOSAL`。
  - askの分類コード: `worker_question_topics_line`。
  - 要るevidence: `domain::required_of`。

verification commandsと手元の検証の入口は`local_checks`と`requests::ResumeKind`で、検証の正は同じcommitに対するintegrateの1回である（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定1）。
このrepositoryでworkerが流すものは[手元の検証](../../development/local-checks.md)が持つ。

- taskに`paths`があれば、宣言の外が要るときはaskにせず`failed`のreceiptに要るpathを書くよう言う（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)決定5）。
  走っているtaskのpathsは変えられず、plannerが広げて登録し直すため。
- e2eはworkerが流さない（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。
  e2eのファイルや再現のコマンドはpromptに書かない（どのrepositoryにも渡る文なので）。
  このrepositoryのそれは[手元の検証](../../development/local-checks.md)の「e2eを流さない」が持つ。
  関門の印は着地の前のe2eがlanding branchのtreeから読み、promptには載せない。
- 上限（[ADR-t2072-1](../../adr/2026-10-08-t2072-1-worker-prompts-and-next-turns-carry-decision-material-within-limits.md)）: 上限は`WORKER_*`の定数で、値の理由と節ごとの取る順はそのdoc commentが持つ。
  省いたら読む方法か読めないことを示す。
- 前のrunの人の判断（[ADR-t4018-1](../../adr/2026-11-02-t4018-1-worker-prompt-carries-unapplied-person-decisions-of-the-previous-run.md)）: 入口は`domain::unapplied_decisions`と`PreviousDecisions`。
  taskの直前に終わったrunの人の判断は、新しいrunがどう始まっても（誰のretryでも、素の`ready`やholdの解除でも）、sessionに届いたかを添えて全部、新しいrunのpromptが必ず従う判断として本文のまま入れる。
  落とし穴: `resume_started`も`wrapper_launched`も届いた印にならない（その後の起動や送信が失敗しうる）。
  復旧jobの`retry`の理由は判断が無くても渡し、askの答えが読めなければrunを始めない。
- 初期promptと引き継ぎの計測は`PromptBytes`、新しいsessionの文の接続は`domain::turn::new_session_prompt`が入口。
- 落とし穴: `tests/e2e.rs`のstubはpromptの1行目とreceiptのpathの行だけを読むので、節を足してもstubは変わらない。
- 既に落ちているtestの節: 一覧があるときだけclaimのpromptとresumeの依頼に載せ、文面・選ぶ順・上限・読む経路は[CI watch](ci-watch.md#workerとreviewの材料)が持つ。
  新しいsessionに送るresumeの依頼は節を持たず、引き継ぎのpromptがrunの触るファイルに絞った一覧を持つ（同じ節）。
- 言語の設定が解決できるときは、promptと依頼の末尾に言語の指示を足す（[Language](language.md#promptへの渡し方)）。

## 受け入れ条件の対応づけ

receiptの前に各条件を満たすものへ対応づけ、満たせない項目をfollow_upへ回して成功にしない（[ADR-t1420-1](../../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)）。
文面の入口は`ACCEPTANCE_MAP`と、再開時の`ACCEPTANCE_REMAP`。
baseがすでに満たす場合の契約は[receipt](worker-receipt.md#baseがすでに満たすときalready_satisfied)が持つ。

- Codexのworkerの自分のdiffの見直しはこの対応づけの手順に寄せ、受け入れ条件との照合を2度言わない（下の[subagent review](#subagent-review)）。

## 文書の照合

対応づけに続けて文書を照合させる入口は`DOCS_CHECK`（[ADR-t1428-1](../../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)、[ADR-t1942-2](../../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)）。
探す道具やrepository固有のpath・文書の層と予算はpromptに名指さない（ADR-t1453-2）。
このrepositoryの範囲と書き方は[documents.md](../../development/documents.md#workerの文書の照合)、reviewの側は[Review](review.md#文書の照合)が持つ。

## 経路とproviderごとの文面

- 分けるのはprovider（`Route::of(run)`、runの`actual_provider`）だけで、違いは最後の段落のproviderの1行と[subagent review](#subagent-review)である。
- 対話と記録されたrunもclaimとresumeで非対話に変わり、Claudeの非対話のrunと同じ文を受け取る（[非対話のworker](headless-worker.md#対話と記録されたtaskのclaimとresume)）。
- 入口:
  - 最後の段落: `HEADLESS_WORKER`とproviderの1行。
  - 自分の処理を止める: `HEADLESS_STOP`。
  - 依頼の先頭: `HEADLESS_NEXT_TURN`。
  - 依頼の最後の手順: `HEADLESS_DONE`。
  - 次のturnとして届く文の後ろ: `HEADLESS_GO_ON`。
  - receiptの無い促し: `stall_nudge`。
  - 閉じたaskの知らせ: `closed_question_notice`。
- 約束: 依頼は先頭で前のturnが終わりbackgroundに残したものが止められたことを言い、最後の手順はこのturnで終えてreceiptかaskで終わることを言う。
- 約束: askの前に、repositoryの指示がその問いをaskにせず自分で決めるか`failed`のreceiptにすると定めていないかを確かめさせる（`ASK_RULES_FIRST`）。
  workerのprompt、依頼の最後の手順、receiptの無い促しの選択肢の3か所に同じ文を置く。
- 約束: 自分が始めた処理はpidで止め、名前やパターンで送らない（他のrunのsessionとhostの検査にも当たるため）。
- 落とし穴: Claude Codeはturnの終わりに`run_in_background`のshellを止めるので、待つためにturnを終えると処理も止まる。
  receiptの無い促しは、それならforegroundで流し直すよう言う。
- `WORKER_READING`の「AGENTS.mdかCLAUDE.md」はどのproviderでも同じ（Codexは起動時にAGENTS.mdを読む）。

### workerが読んだ指示の版

claimのたびに、supervisorはrunのproviderのworkerが読む指示のhashを3つ、`run_claimed`に別々のキーで記録する。

- 入口: 対象と求め方は`domain::instructions`のmoduleのdoc commentが持つ。
- 約束: 各hashは読む指示の内容だけで決まり、taskの値・commitのid・時刻によらず、同じ内容のrunは同じ版として比べられる。
  求められないhashは不明の値になり、claimは止まらない。

### subagent review

- 入口: `review_line`。
- providerごとのreviewの指示は`review_line`のdoc commentが持つ（[spike](../../plans/headless-worker-spike.md)）。
- 約束: taskが`subagent_review`を要るevidenceにしていても、runの`actual_provider`がCodexなら`required_of`が外す。
  validating・integrateのreceiptの検査・resumeの解決の判定・promptの`Required evidence:`の行はどれもこの絞った一覧を使う。
- 落とし穴: runが途中でClaudeに切り替わった後は`actual_provider`がClaudeなので、要るevidenceに戻る。

### 復旧jobのprompt

- 入口: `recovery_prompt`。
- 非対話の操作だけを案内する入口は`recovery_prompt`、許さない操作の検査は`supervise::execution::recovery::check_live`。
- verdictの規則は末尾に置き、中ほどを切っても残す（[ADR-t1818-2](../../adr/2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)）。
- 引き渡された主張の判定の入口は`claim_review_prompt`。
- 判定と上限は[復旧job](background-recovery-job.md)と下の[上限](#goal-reviewrunのreview復旧jobruntimeのplannerの上限)が持つ。

## repositoryの規則を読む順

promptと固定の文字列には、dagqのrepositoryの規則（ADRの索引や番号の付け方、Rustのlinterの名前など）を書かない。

読む規則の探し方は`repository_rules`、workerの読む範囲は`WORKER_READING`が入口。
plan reviewへの適用は`plan_review_prompt`、全体のreviewへの挿入位置は`run_review::review_prompt`のdoc commentが持つ（[ADR-t2259-1](../../adr/2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)決定6）。

- follow_up・goal gap・findingのdraftの`context`の見出しは英語の固定の文で、言語の設定に従わない（[Language](language.md#runtimeの固定の文字列)）。

## headlessのjobのprompt

supervisorが起動するheadlessのjobとruntimeのplannerのpromptの共通の方針で、[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)の決定に沿う。
workerの初期promptと次のturnの文も[ADR-t2072-1](../../adr/2026-10-08-t2072-1-worker-prompts-and-next-turns-carry-decision-material-within-limits.md)で同じ方針に入り、渡し方（決定1）だけは`prompt.txt`と非対話のturnによる。

- 渡し方（決定1）: 大きさに関係なくstdinで渡し、引数で渡さない。
  仕組み（`CommandSpec::stdin`）と起動の失敗の扱いは[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)が持つ。
- 例外: runtimeのplannerは非対話のturnのコマンドで動き、jobの経路を使わない（[非対話のworker](headless-worker.md)）。
  Claudeのturnのコマンドもpromptをstdinで渡す。
- 載せるもの（決定2）: 判断の材料だけ。
  一覧と全文の大量のデータはIDと要約にし、中身は読む経路で必要なものだけ読ませる。
- 取りに行く経路（決定3）: jobの権限の意図（`JobAccess`）と許す道具で実際に読める経路だけを書く。
  `queue_cli`のjob（observer・スループットの見直し）はファイルを読めないので、jobのdirのファイルを読む方法にしない。
  `read_files`のjob（runのreview・復旧job）は意図として`dagq`を打てないので、省いてよいのはworktreeとrun directoryのファイルにあるものだけ。
  落とし穴: Codexのreviewはsandboxの都合で読むコマンドを打てても、promptはそれを読む方法にしない。
- 上限と選ぶ順（決定4）: 節ごとの件数かbyteの上限と全体の上限を持ち、超えたときに残す順は決まった規則で決める。
- 省いたことの明示（決定5）: 節ごとに省いた件数・IDと読む方法をpromptに書く。
- 記録（決定6）: jobごとにpromptのbyte数をeventに記録し、上限をtestで確かめる。
  どのeventのどの欄かは各jobの組み立ての関数とevent kindの定義が持つ。

| job | 権限の意図 | 組み立ての入口 | 上限の定数 |
| --- | --- | --- | --- |
| plan review | `read_files_and_queue_cli` | `plan_review_prompt` | `PLAN_REVIEW_*`（[下](#plan-reviewの上限)） |
| observer | `queue_cli` | `src/application/observer/input.rs`（[Observer](observer.md#promptの入力の上限と選ぶ順)） | そのmoduleの`PROMPT_LIMIT` |
| goal review | `read_files_and_queue_cli` | `goal_review_prompt` | `GOAL_REVIEW_*` |
| スループットの見直し | `queue_cli` | `src/application/throughput_review.rs`（[スループットの見直し](throughput-review.md)） | そのmoduleの`PROMPT_*` |
| runの全体のreview | `read_files` | `prompt::run_review`の`review_prompt` | `RUN_REVIEW_*` |
| `already_satisfied`の主張の判定（[Review](review.md#already_satisfiedの主張の判定)） | `read_files` | `claim_review_prompt` | `CLAIM_REVIEW_*` |
| agentのjob（[下](#agentのjobの上限)） | `read_files`を定義の道具に狭める | `agent_job::build` | `AGENT_JOB_*` |
| 復旧job | `read_files` | `recovery_prompt` | `RECOVERY_*` |
| runtimeのplanner | plannerのrole | `src/application/prompt/planners.rs` | そのmoduleの定数 |
| workerの初期prompt | worktreeとrun directoryのファイル・git・goalのdoc | `prompt::prompt`（[上](#workerのprompt)） | `WORKER_*` |
| workerの次のturnの文 | worktreeとrun directoryのファイル・git | `src/application/prompt/requests.rs`（[下](#次のturnの文の上限)） | 依頼ごとの`*_LIMIT` |

### plan reviewの上限

- 入口: `plan_review_prompt`。
  仕組み（必須の節と省いてよい節、必須の節の替え方、選ぶ順、省いたことの書き方）は`plan_review_prompt`のdoc commentとコメントが持つ（入口は[Plan review](plan-review.md#入口)）。
- 値の理由と本番の大きさは各定数のdoc commentが持つ。
- 落とし穴: 上限はagentの文脈のためのもので、stdinで渡すので`ARG_MAX`の制約ではない。

### agentのjobの上限

- 入口は`agent_job::build`で、agentのevalとrunのreviewのagentのjobが共有する（[Agent eval](../agent-eval.md)）。
- stdinで渡し、権限の意図は読み取りだけで、読む方法は材料のファイルと変更のtreeのファイルだけ。
- 定義の節は切らず、上限を超える定義ではjobを起動しない（[ADR-t1869-1](../../adr/2026-10-09-t1869-1-agent-jobs-carry-the-whole-definition-and-do-not-start-over-the-limit.md)）。

### 次のturnの文の上限

- 入口: 表の行のmoduleと定数。
  値の理由と本番の大きさは各定数のdoc commentが持つ。
- 省略時の読む経路と必須の節は`requests`のmodule doc、引き継ぎは`handoff_text`、依頼の再送は`restored_request`が入口（[ADR-t1892-1](../../adr/2026-10-07-t1892-1-resume-request-lists-landed-tasks-by-title-with-a-cap.md)）。

### goal review・runのreview・復旧job・runtimeのplannerの上限

- 入口: 値は`src/application/prompt/`の各jobの定数（上の表）、仕組みは`src/application/prompt_fit.rs`。
  値の理由と本番の大きさは各定数のdoc commentが持つ。
- 項目の選び方と切り方は`prompt_fit::Fit`のdoc commentが持つ。
- 前のplannerはresumeせず、queueの記録から材料を集めて引き継ぐ（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定3）。
  引き継ぐ材料の詳細は`planners::PLANNER_HANDOVER_BYTES`のdoc commentが入口。
- 不変条件: 節の上限の和は全体の上限（言語の指示の分の空きを除く）に収まるように決める。
  それでも超えたときだけ、先頭と末尾（指示とverdictのschema）を残して中ほどを切る（`Fit::finish`）。
- 約束: 復旧jobの固定バイナリの節はretryの判断の材料なので、alertの事実の直後に置き、中ほどを切っても残る側に置く（`binary_sections`）。
- 記録の数え方とeventへの対応は`PromptBytes`のdoc commentが持つ。
