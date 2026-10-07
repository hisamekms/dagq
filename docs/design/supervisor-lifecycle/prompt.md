---
id: design-supervisor-lifecycle-prompt
type: design
title: "Prompt"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1566-1
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
文面そのものは`src/application/prompt.rs`の定数と組み立ての関数が持ち、この文書は節の目的・組み立ての順・約束だけを書く。

## 目的

- workerが最初のcommitまでにqueueやdocs全体を読まずに済むよう、runに要る情報をclaimの時点でpromptに載せる。
- runtimeはどのrepositoryでも動くので、検証・paths・ADRの規則のようなrepositoryの規則を持たず、sessionをrepositoryの指示（AGENTS.mdかCLAUDE.md）へ向ける。
- headlessのjobのpromptは判断の材料だけを上限の中で載せ、残りは読む方法を示して取りに行かせる（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）。

## 流れ

```text
claim ──> prompt(task, run, goal, predecessors, goal_predecessors, siblings) ──> prompt.txt（1回だけ書く）
            │
            ├─ task・検証・evidence・paths・e2eの行
            ├─ Goal / Context / Predecessor tasks / Sibling tasks（常に4節）
            ├─ 受け入れ条件の対応づけ → 文書の照合 → receiptの契約
            └─ 最後の段落（非対話のturn、providerの1行）＋言語の指示
claim ──> 指示の版のhash ──> run_claimed（下の「workerが読んだ指示の版」）

runの途中 ──> resume・revise・促し・askの答え・復旧jobの指示 ──> 次のturnのprompt

supervisorのjob ──> plan review・observer・goal review・スループットの見直し・runのreview・復旧job
                     └─ 節ごとの上限 → promptのbyte数をeventに記録 → stdinで渡す
runtimeのplanner ──> 節ごとの上限（prompt_fit::Fit）→ 記録 → 非対話のturnのコマンドで渡す
```

## 責務と境界

- `src/application/prompt.rs`: 全てのpromptと依頼の文面と組み立て。
  queueもファイルも直接は読まず、呼び出し側が集めた材料（`PlanReviewMaterial`・`RecoveryMaterial`など）を文にする。
- `src/application/prompt_fit.rs`: 節ごとの件数とbyteの上限、切り方、省いたことの注記、`PromptBytes`の集計。
- 材料を集めるのはsupervisorとapplicationの各use case（claim、plan review、復旧job、plannerの起動）で、queueの読み取りは`TaskStore`の読み取り専用の操作を使う。
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

## workerのprompt

- 知りたいこと→入口:
  - 全体の組み立て: `prompt::prompt`。
  - 最初に読むものの限定: `WORKER_READING`（worktreeだけで作業する指示の直後）。
  - 手元で流す検証: `local_checks`。
  - 依存元の行: `PredecessorSummary::from_predecessor`と`GoalPredecessorSummary::from_goal_predecessor`、取得は`TaskStore::predecessors`と`TaskStore::goal_predecessors`。
  - 兄弟taskの行: `siblings_in_progress`。
  - e2eの行: `e2e_line`。
  - follow_upの種類と所属の提案: `follow_up_categories_line`・`FOLLOW_UP_PROPOSAL`。
  - askの手順と分類コード: `ASK_RULES_FIRST`・`worker_question_topics_line`。
  - 要るevidence: `domain::required_of`。
- verification commandsは「integrateがrebase後に1回流す」という見出しで見せ、直後の`local_checks`の一文が、worktreeで流すのはrepositoryの指示がworkerに求める検証だと言う。
  指示が何も求めないときだけverification commandsを流す。
  resume・reviseの依頼も同じ一文を持ち、検証が落ちて戻ったresume（integrateのrebase後、着地の枝での再確認、e2e）はその落ちたものの再現を許す。
  検証の正は同じcommitに対するintegrateの1回である（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定1）。
  このrepositoryでworkerが流すものは[手元の検証](../../development/local-checks.md)が持つ。
- taskに`paths`があれば、宣言の外が要るときはaskにせず`failed`のreceiptに要るpathを書くよう言う（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)決定5）。
  走っているtaskのpathsは変えられず、plannerが広げて登録し直すため。
- e2eはworkerが流さない（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。
  runがe2eを要りうるときだけ、runtimeがreviewのpassの後にhostで流すことと、receiptの`e2e`を`not_applicable`にすることを1行で言う。
  e2eが落ちて戻ったrunのresumeだけが、落ちたtestを名前で絞って再現するよう頼む。
  関門の印は着地の前のe2eがlanding branchのtreeから読み、promptには載せない。
- Predecessor tasksにはtask依存に続けてgoal依存（[ADR-0038](../../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)）のgoalと、その完了したtaskを並べる。
  goalのtaskは多くなりうるので、summaryを短く切る（`GOAL_TASK_SUMMARY_CHARS`）。
- 落とし穴: 兄弟taskの一覧はclaimの順で非対称になる（同じpassで後にclaimしたtaskだけが先のtaskを知る）。
  理由は`siblings_in_progress`のdoc comment。
- 落とし穴: `tests/e2e.rs`のstubはpromptの1行目とreceiptのpathの行だけを読むので、節を足してもstubは変わらない。
- 言語の設定が解決できるときは、promptと依頼の末尾に言語の指示を足す（[Language](language.md#promptへの渡し方)）。

## 受け入れ条件の対応づけ

- workerのpromptはreceiptの書き方の直前に`ACCEPTANCE_MAP`を置く（[ADR-t1420-1](../../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)）。
  各項目を満たすものへ対応づけ、満たせない項目を`follow_ups`に回して`succeeded`にしない。
- resumeとreviseの依頼はreceiptを書き直す手順に`ACCEPTANCE_REMAP`と`FOLLOW_UP_PROPOSAL_AGAIN`を足す。
- どのproviderのworkerも同じ文で、新しい検査のコマンドは求めない。
- Codexのworkerの自分のdiffの見直しはこの対応づけの手順に寄せ、受け入れ条件との照合を2度言わない（下の[subagent review](#subagent-review)）。
- runのreviewの側は[Review](review.md#文書の照合)が持つ。

## 文書の照合

workerのpromptは`ACCEPTANCE_MAP`の直後に`DOCS_CHECK`を置き、対応づけの続きとして文書の照合を指示する（[ADR-t1428-1](../../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)、[ADR-t1942-2](../../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)）。

- 候補: taskが名指す文書、作業中に見つけた文書、変えた名前でrepositoryを探して見つけた文書。
  `summary`の探した名前は、reviewが同じ名前で探し直すための根拠である。
- 書く条件: 流れ・境界・不変条件・コードから読めない約束が変わったときと、記述がコードと食い違うときだけ。
  名前が文書に無いことはずれではなく、識別子の列挙と経緯を足さず、細かい事実は定義のそばのdoc commentに書かせる。
- 行き先: taskのpathsの中は直し、外は`docs_drift`のfollow_upにする。
  示すためだけに文書を触らない。
- 汎用に保つ: 探す道具、repository固有のpath、文書の層や予算は名指さない（ADR-t1453-2）。
  このrepositoryの範囲と書き方は[documents.md](../../development/documents.md#workerの文書の照合)が持つ。
- どのproviderのworkerのpromptも同じ文で、検査のコマンドやtestの実行は求めない。

resumeとreviseの依頼は`ACCEPTANCE_REMAP`の「`summary`の句を書き直す」に照合した文書を含める。
reviewの側は[Review](review.md#文書の照合)が持つ（`REVIEW_DOCS_CHECK`）。

## 経路とproviderごとの文面

- workerに送る文（`prompt.txt`、resume・revise・食い違い・古いreceipt・receiptの無い促し・閉じたaskの知らせ、askの答え、復旧jobの指示、holdの後の続き）は全て非対話のsessionの文である（[非対話のworker](headless-worker.md)、[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)）。
- 分けるのはprovider（`Route::of(run)`、runの`actual_provider`）だけで、違いは最後の段落のproviderの1行と[subagent review](#subagent-review)である。
- 対話と記録されたrunもclaimとresumeで非対話に変わり、Claudeの非対話のrunと同じ文を受け取る（[非対話のworker](headless-worker.md#対話と記録されたtaskのclaimとresume)）。
- 入口:
  - 最後の段落: `HEADLESS_WORKER`とproviderの1行。
  - 自分の処理を止める: `HEADLESS_STOP`。
  - 依頼の先頭と最後の手順: `HEADLESS_NEXT_TURN`・`HEADLESS_DONE`。
  - 次のturnとして届く文の後ろ: `HEADLESS_GO_ON`（`answer_text`・`recovery_instruction`・`continue_text`）。
  - receiptの無い促し: `stall_nudge`、閉じたaskの知らせ: `closed_question_notice`。
- 約束: 依頼は先頭で前のturnが終わりbackgroundに残したものが止められたことを言い、最後の手順はこのturnで終えてreceiptかaskで終わることを言う。
- 約束: askの前に、repositoryの指示がその問いをaskにせず自分で決めるか`failed`のreceiptにすると定めていないかを確かめさせる（`ASK_RULES_FIRST`）。
  workerのprompt、依頼の最後の手順、receiptの無い促しの選択肢の3か所に同じ文を置く。
- 約束: 自分が始めた処理はpidで止め、名前やパターンで送らない（他のrunのsessionとhostの検査にも当たるため）。
- 落とし穴: Claude Codeはturnの終わりに`run_in_background`のshellを止めるので、待つためにturnを終えると処理も止まる。
  receiptの無い促しは、それならforegroundで流し直すよう言う。
- `WORKER_READING`の「AGENTS.mdかCLAUDE.md」はどのproviderでも同じ（Codexは起動時にAGENTS.mdを読む）。

### workerが読んだ指示の版

claimのたびに、supervisorはworkerが読む指示の内容のhashを3つ求め、runのproviderの値を`run_claimed`に別々のキーで記録する。
値は内容だけから決まり、commitのIDや時刻を含まない。
求められないものは`unknown`にし、claimは止めない。
読み方は[kpi](kpi.md#workerが読んだ指示の版)が持つ。

- promptの雛形: taskとrunの値の代わりにplaceholderを入れたworkerのpromptで、providerごとに1つある（`worker_template`）。
  節の見出しと定型の文とproviderで変わる行が対象で、定型の文を変えるとhashが変わり、taskの値では変わらない。
  taskの値を差し込んだ後のpromptは、taskごとに違って層にならないので対象にしない。
  言語の指示、resource brokerのqueueだけが足す文、resume・reviseなどの依頼も対象にしない。
- plugin: ClaudeのworkerがClaude Codeから読むdagqのpluginのファイルの内容（`infrastructure::adapters::worker_plugin_hash`）。
  Codexのworkerにはpluginを渡さないので`none`にする。
- repositoryの指示の文書: runのbaseのcommitでの`AGENTS.md`・`CLAUDE.md`・`docs/development/`の内容（`domain::instructions::REPOSITORY_INSTRUCTIONS`）。

### subagent review

- 入口: `review_line`と`domain::required_of`。
- Claude: 該当すればClaude Codeのsubagentで同じturnの中でreviewし、receiptの`subagent_review`にevidenceか理由を書く。
- Codex: `codex exec`にsubagentは無く、`codex exec review`の入れ子はsandboxで`$CODEX_HOME`のsessionを書けず費用も倍になる（[spike](../../plans/headless-worker-spike.md)）。
  そこで自分のdiffを読んで見直し、`subagent_review`を`not_applicable`にして理由と見つけたことを書く。
  着地の前には全runと同じくsupervisorのreview jobがcommitをreviewする。
- 約束: taskが`subagent_review`を要るevidenceにしていても、runの`actual_provider`がCodexなら`required_of`が外す。
  validating・integrateのreceiptの検査・resumeの解決の判定・promptの`Required evidence:`の行はどれもこの絞った一覧を使う。
- 落とし穴: runが途中でClaudeに切り替わった後は`actual_provider`がClaudeなので、要るevidenceに戻る。

### 復旧jobのprompt

- 入口: `recovery_prompt`と`HEADLESS_NEVER`。
- どのworkerの記録でも非対話の操作だけを案内し、画面のダイアログに答える操作と閉じて進める操作は許す操作から除く。
  返されてもsupervisorの`check_live`（`src/application/supervise/recovery.rs`）が拒む。
- `send_instruction`は次のturnの依頼で、画面の代わりにturnの抜粋を読む。
- workerに打鍵する操作は無い。
- 判定と上限は[復旧job](background-recovery-job.md)と下の[上限](#goal-reviewrunのreview復旧jobruntimeのplannerの上限)が持つ。

## repositoryの規則を読む順

runtimeはrepositoryの規則（検証のコマンド、宣言するpaths、要るevidence、ADRのような記録の規則）を持たず、promptはsessionをrepositoryの指示へ向けるだけにする。
promptと固定の文字列には、dagqのrepositoryの規則（ADRの索引や番号の付け方、Rustのlinterの名前など）を書かない。

- worker: `WORKER_READING`と`local_checks`が「AGENTS.mdかCLAUDE.md」を名指す。
- planner: `repository_rules`が、AGENTS.md →（無ければ）CLAUDE.md →（どちらも無ければ）README・CIの設定・buildの設定の順で決め、決まらなければaskすると言う。
  runtimeが立てるplannerのaskは`RUNTIME_PLANNER_ASK`で、材料で決めきれず人の判断に当たるか確信度が低いときだけ`planner_question`にする（ADR-t451-1決定5）。
- plan review: repositoryの指示とそれが名指す文書を当てはめさせ、AGENTS.mdが無ければ同じ順で判断し、決まらなければ`concern`にする。
  このrepositoryではAGENTS.mdの「plan review」の節から[taskの登録](../../development/task-registration.md)の「plan reviewが当てはめる規則」へ辿る。
- review: `REVIEW_RULES`がproviderに依らずworktreeのrootの指示とそれが名指す文書で差分を判定させ、資料の行の後、`REVIEW_DOCS_CHECK`の前に置く。
  Claudeのreviewは`--setting-sources ""`で起動して`CLAUDE.md`をmemoryとして読まないので、promptで名指す（[ADR-t1470-1](../../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)決定2）。
- follow_up・goal gap・findingのdraftの`context`の見出しは英語の固定の文で、言語の設定に従わない（[Language](language.md#日本語が残っていた固定の文字列)）。

## headlessのjobのprompt

supervisorが起動するheadlessのjobとruntimeのplannerのpromptの共通の方針で、[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)の決定に沿う。
各jobの文書（[Headless job processes](headless-job-processes.md)、[Plan review](plan-review.md)、[Observer](observer.md)、[Goal review](goal-review.md)、[スループットの見直し](throughput-review.md)、[Review](review.md)、[復旧job](background-recovery-job.md)、[Session prompts](session-prompts.md)）はpromptの大きさと渡し方についてここを指す。
workerの`prompt.txt`はこの方針の範囲に含めない。

- 渡し方（決定1）: 大きさに関係なくstdinで渡し、引数で渡さない。
  `AgentProvider::headless_command`と`review_command`がpromptを`CommandSpec::stdin`に持たせ、spawnerが本人だけが読める一時ファイルに書いてすぐunlinkし、子のstdinにする（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
  引数で渡すとhostの`ARG_MAX`を超えたpromptでjobが起動できない。
  起動の失敗（`E2BIG`）と一時ファイルを用意できない失敗は`job_start_failure`が`other`にし、そのjobの失敗にだけ数えてproviderを控えない。
- 例外: runtimeのplannerは非対話のturnのコマンドで動き、jobのstdinの経路を使わない（[非対話のworker](headless-worker.md)）。
- 載せるもの（決定2）: 判断の材料だけ。
  一覧と全文の大量のデータはIDと要約にし、中身は読む経路で必要なものだけ読ませる。
- 取りに行く経路（決定3）: jobの権限の意図（`JobAccess`）と許す道具で実際に読める経路だけを書く。
  `queue_cli`のjob（observer・スループットの見直し）はファイルを読めないので、jobのdirのファイルを読む方法にしない。
  `read_files`のjob（runのreview・復旧job）は意図として`dagq`を打てないので、省いてよいのはworktreeとrun directoryのファイルにあるものだけ。
  落とし穴: Codexのreviewはsandboxの都合で読むコマンドを打てても、promptはそれを読む方法にしない。
  読むだけの`dagq`のコマンドの一覧（`PLAN_REVIEW_READS`・`GOAL_REVIEW_READS`・`PLANNER_READS`）は、そのjobかplannerのroleで打てることをtestが確かめる。
- 上限と選ぶ順（決定4）: 節ごとの件数かbyteの上限と全体の上限を持ち、超えたときに残す順は決まった規則で決める。
- 省いたことの明示（決定5）: 節ごとに省いた件数・IDと読む方法をpromptに書く。
- 記録（決定6）: jobごとにpromptのbyte数をeventに記録し、上限をtestで確かめる。
  どのeventのどの欄かは各jobの組み立ての関数とevent kindの定義が持つ。

| job | 権限の意図 | 組み立ての入口 | 上限の定数 |
| --- | --- | --- | --- |
| plan review | `read_files_and_queue_cli` | `plan_review_prompt`（`PlanReviewMaterial`） | `PLAN_REVIEW_*`ほか（[下](#plan-reviewの上限)） |
| observer | `queue_cli` | `src/application/observer/input.rs`（[Observer](observer.md#promptの入力の上限と選ぶ順)） | そのmoduleの`PROMPT_LIMIT` |
| goal review | `read_files_and_queue_cli` | `goal_review_prompt` | `GOAL_REVIEW_*` |
| スループットの見直し | `queue_cli` | `src/application/throughput_review.rs`の`review_prompt`（[スループットの見直し](throughput-review.md)） | そのmoduleの`PROMPT_LIMIT`・`PROMPT_INPUT_LIMIT` |
| runのreview | `read_files` | `prompt.rs`の`review_prompt` | `RUN_REVIEW_*` |
| 復旧job | `read_files` | `recovery_prompt`（`RecoveryMaterial`） | `RECOVERY_*` |
| runtimeのplanner | plannerのrole | `runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`・`request_planner_prompt` | `RUNTIME_PLANNER_*`・`DRAFT_*`・`FINDING_*`・`REQUEST_*`・`PLANNER_*` |

### plan reviewの上限

- 入口: `src/application/prompt.rs`の`PLAN_REVIEW_PROMPT_LIMIT`から並ぶ定数と、`plan_review_prompt`の省いてよい節の取り方。
  仕組み（必須の節の替え方、選ぶ順、省いたことの書き方）は[Plan review](plan-review.md)の4の「上限と選ぶ順」が持つ。
- 値の理由と本番の大きさは各定数のdoc commentが持つ。
- 約束: 省いてよい節は、全体の残りを依存と重複の判断に直接効くものから取り、CLIの一覧で代えやすい要約を最後にする。
- 約束: 必須の節（指示と検査とverdictの形、proposalのtaskの全文、予想するファイル、goal、`lint`、言語の指示）は省かない。
  必須の節だけで上限を超えるときだけ、大きいものから読むだけの`dagq`の案内に替え、jobのdirのファイルには退避しない。
- 落とし穴: 上限はagentの文脈のためのもので、stdinで渡す今は`ARG_MAX`の制約ではない。

### goal review・runのreview・復旧job・runtimeのplannerの上限

- 入口: 値は`src/application/prompt.rs`の各jobの定数（`GOAL_REVIEW_*`・`RUN_REVIEW_*`・`RECOVERY_*`・`RUNTIME_PLANNER_*`・`DRAFT_*`・`FINDING_*`・`REQUEST_*`・`PLANNER_*`）、仕組みは`src/application/prompt_fit.rs`（`pick`・`cut`・`shrink`・`Fit`）。
  値の理由と本番の大きさは各定数のdoc commentが持つ。
- 選ぶ順: 一覧の節は決まった順で取り、入らない項目は飛ばして次を試し（1件の巨大な項目が残りを隠さない）、残したものはもとの順で載せる。
  多くの節は新しい順に取り、goal reviewのtaskは着地したものを先に、復旧jobの依存先は今のbuildが含まないか言えないものを先に取る。
  draftの束・依頼の参照先・reviseの指摘は与えられた順のまま取る。
- 切り方: 長い文は先頭を残して切り、省いたbyte数と読む方法の注記を付ける。
  JSONの項目は長い文字列から切り、それでも入らなければIDと読む方法だけの行にする。
- 読む方法: 読めるファイルにも打てるコマンドにも無いもの（復旧jobのalertの事実・プロセス・過去のverdict、読めなかった依頼の参照）は、読む方法が無いと書き、読めない場所に退避しない。
- 必須の節: goal reviewのgoal、runのreviewのacceptance、復旧jobのtaskの記述と検証、draftのdescription、findingの見立ては省かず、自分の上限で切って`over_limit`に書く。
- 不変条件: 節の上限の和は全体の上限（言語の指示の分の空きを除く）に収まるように決める。
  それでも超えたときだけ、先頭と末尾（指示とverdictのschema）を残して中ほどを切る（`Fit::finish`）。
- 約束: 復旧jobの固定バイナリの節はretryの判断の材料なので、alertの事実の直後に置き、中ほどを切っても残る側に置く（`binary_sections`）。
- 記録: どのjobも`PromptBytes`を`prompt_bytes`としてeventに記録し、plan reviewと同じ形を持つ。
  省いた件数（`omitted`）は節ごとに、省いた項目と残して切った項目を数える。
  1件を1回だけ数える節とその単位、欄ごとに数える例外の節は`PromptBytes`のdoc commentが持つ。
- 上限を確かめるunit testは`src/application/prompt.rs`の「最も大きな入力でも上限に収まる」test群で、eventの記録は各jobの`tests/it`のmoduleが確かめる。
