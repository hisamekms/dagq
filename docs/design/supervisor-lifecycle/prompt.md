---
id: design-supervisor-lifecycle-prompt
type: design
title: "Prompt"
status: current
created: 2026-09-26
updated: 2026-10-03 # task 1399: dagq plan opens nothing (ADR-t1394-1; after task 1567's observer limits)
last_verified: 2026-10-03 # task 1399
scope: runtime
related:
  - adr-t1566-1
  - adr-t1428-1
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
---

# Prompt

`prompt.txt`は`src/application/prompt.rs`の`prompt(task, run, goal, predecessors, goal_predecessors, siblings)`（`runtime::prompt`として再公開）が生成するclaim時点のスナップショットで、run中にqueueが変わっても書き換えない。goalの`goal edit`も、兄弟taskの状態変化も、走行中のrunには届かず、次のclaimのpromptから反映される。task単体の情報（ID、run ID、title、description、acceptance、verification_commands）、receiptの契約（pathとJSONの形）に加えて、[ADR-0009](../../adr/0009-goal-groups-tasks.md)の次の4節をこの順で検証コマンドとreceiptの契約の間に載せる。どれも常に書き、該当がなければ`none`にして、promptの節構成をgoal・context・依存・並列の有無で変えない。

- **Goal**: taskに`goal_id`があれば、claim時点の`TaskStore::show_goal()`のgoalを`Goal ID` / `Goal title` / `Goal description` / `Goal acceptance` / `Goal constraints` / `Goal doc`の行で載せる。`doc`はrepository内のpathをそのまま書き、内容は読まない（なければ`Goal doc: none`）。goalのないtaskは`Goal: none, this task stands alone`。
- **Context**: taskの`context`が空白でなければ本文をそのまま載せ、空なら`Context: none`。
- **Predecessor tasks**: taskの直接の依存元（`task_dependencies`のpredecessor）ごとに1行、`- task <ID>: <title>; result commit <sha>; summary: <text>`。`result commit`は依存元の`integrated` runの`result_commit`（`integrate`がmainに積んだsquash commit）、`summary`はそのrunのreceipt（`receipt_path`。なければ`<run-dir>/receipt.json`）の`summary`（空白を1つに畳む。空なら`(no summary)`）。receiptが読めない・parseできない・pathが不明なら`(receipt unavailable)`、integrated runがなければ（手で`completed`にしたなど）`result commit (not landed)`と書き、いずれもprovisionを止めない。取得はqueueの読み取り専用操作`TaskStore::predecessors(task_id)`（ID順。依存元の`Task`と`integrated` runの`Option<TaskRun>`）で、summaryの読み取りはapplication側（`application::prompt::PredecessorSummary::from_predecessor`）が`RunFiles`越しに行う。task依存の行に続けて、taskのgoal依存（[ADR-0038](../../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)。claim時点ではすべて`achieved`で閉じている）ごとに`- goal <ID> (closed as achieved): <title>; its completed tasks:`を書き、その下にgoalの`completed`のtaskを1行ずつ`  - task <ID>: <title>; result commit <sha>; summary: <text>`で並べる（無ければ`  - none`）。行の作り方はtask依存と同じだが、goalはtaskが多くなりうるのでsummaryを`GOAL_TASK_SUMMARY_CHARS`（200文字）で切って`…`を付ける。取得は`TaskStore::goal_predecessors(task_id)`（goal ID順の`GoalPredecessor`: goalと、その`completed`のtaskの`Predecessor`のID順）、組み立ては`GoalPredecessorSummary::from_goal_predecessor`。依存元もgoal依存もなければ`Predecessor tasks: none`。
- **Sibling tasks in progress**: `TaskStore::tasks_in_progress()`が返す`in_progress`のtask（ID順）から自分のtaskを除き、taskにgoalがあれば同じ`goal_id`のtaskに限定したものを`- task <ID>: <title>`で並べる（`siblings_in_progress`）。goalのないtaskはgoalの有無を問わず全`in_progress` taskを見る。claimは`fill_slots`で1件ずつ順に行うので、同じpassで後にclaimされたtaskのpromptには先にclaimされたtaskが載り、その逆は載らない。`awaiting_integration`や`needs_session`のrunを持つtaskも`in_progress`なので載る。なければ`Sibling tasks in progress: none`。

冒頭（worktreeだけで作業する指示の直後）に、最初に読むものを`WORKER_READING`の一文に限定する: repository instructions（AGENTS.mdかCLAUDE.md。`local_checks`と揃える）のworker節、この下のtask context（とそれが名指す文書）、goal doc、依存元のsummaryだけを読み、`dagq list` / `dagq show`は打たず、docs全体は読まず、他のファイルはtaskが必要とするときだけ開く（goal 11の決定4。runに要る情報はpromptに載っていて、queueの一覧やdocs全体を読むのは最初のcommitを遅らせるだけ）。

判断が要るときの手順も載せる: terminalに質問を書いて待つのではなく、worktreeで`dagq ask --run <run-id> --kind worker_question --because scope --topic <code> --question '...'`を打ち、短く報告して止まる。askの前に、repositoryの指示（AGENTS.mdかCLAUDE.md）がその問いをaskにせず自分で決める・`failed`のreceiptにすると定めていないかを確かめ、定めていればそれに従うよう書く（`ASK_RULES_FIRST`。runtimeはrepositoryの規則を持たないので、ADRのIDの衝突など個別の規則は書かない。task 978）。対話と非対話の両方のpromptでaskの手順の文の前に置き、非対話の依頼の`HEADLESS_DONE`の`If you need a decision`の前と、receiptの無い促し（`stall_nudge`）の選択肢の2（`<ASK_RULES_FIRST> Otherwise, if you need a decision, run ...`）にも同じ文を置く。`--topic`には問いの中身の分類コード（[ADR-t947-2](../../adr/2026-09-28-t947-2-worker-questions-carry-topic-codes.md)）を主から付け、promptはコードの一覧と定義と主の選び方（`worker_question_topics_line`。一覧は`domain::WORKER_QUESTION_TOPICS`、[ask](ask.md#worker_questionの分類コード)）を載せる。回答は`answer to ask <id>: ...`としてterminalに届く（[workerの質問への回答の送信](worker-question-answer.md#workerの質問への回答の送信)）。

末尾の「receiptを書いたら短く報告して止まる」の直前に`STOP_BACKGROUND`の一文を置く: receiptを書く前に、自分が起動したbackgroundの処理（`run_in_background`のshell、待ちループ、watchなど）をすべて止める。残っているとClaude Codeが`/exit`に「Background work is running — Exit and stop tasks / Move to background and exit / Stay」の確認画面を出して止まり、`exit_request_timed_out`になる（2026-09-23にtask 49・75・74・118で起きた）。同じ一文をresumeの定型の解消依頼（[`needs_session`](needs-session.md#needs_session)）にも手順4として載せる。残った確認画面は、`/exit`のexit timeoutでsupervisorが画面を読み、worktreeがcleanでreceiptの`commit`がHEADのときだけ「Exit and stop tasks」を選ぶ（ADR-0047の決定29。[既知のダイアログ](prompt-waiting.md#既知のダイアログ)）。条件がそろわなければ今までどおり`stuck_exit`のaskになる。同じ定数の後半は、止めるのは自分が起動したものだけをpidかtaskで、名前やパターン（`pkill`、`killall`、`kill $(pgrep ...)`）では送らないと書く。どのrunのsessionもcommand lineにpromptを持つので、`pkill -f llvm-cov`が他のrunのsessionと`integrate`の検証を止めた（task 359。設定側の`permissions.deny`は[provider lifecycle](../provider-lifecycle.md)）。

verification commandsは`Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):`の見出しで一覧を見せ、その直後に`local_checks`の一文を置く: worktreeで流すのはrepositoryの指示（AGENTS.mdかCLAUDE.md）がworkerに求める検証で、それはverification commandsの一部をintegrateに任せてよく、指示が何も求めないときはverification commandsを流す。同じ一文をresume（`evidence_missing`・`scope_violation`・`sent_back`・triage・rebase（`Landing`）・reviewのpass後の衝突（`Precheck`））とreviseの手順2にも載せ（そこではverification commandsをJSONの一覧で書く）、integrateのrebase（`Landing`）の手順2には「reasonがintegrateのrebase後に落ちた検証コマンドなら、そのコマンドを手元で流して再現して直してよい」を足す。retryが引き継いだrunの節も「上の検証を流し直す」と書く。runtimeはcargoやllvm-covなど特定のツールの名前を決め打ちしない（dagqは他のrepositoryでも動く。どの検証をintegrateだけに任せるかはrepositoryの指示が決める）（task 510）。新しいADRは作らない: [ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定1の「同じcommitのverificationはintegrateの1回が正」に沿ってpromptの文面を直すだけで、決定は変わらないため。

taskの`required_evidence`のうちworkerが裏付けるcheck（`domain::required_of`。`e2e`は含めず、Codexのworkerは`subagent_review`も含めない）があれば、verification commandsの直後（4節の前）に`Required evidence: tests (each must be passed with evidence in the receipt, or the run waits for a session to add it)`の1行を載せ、workerに事前に知らせる（無ければ行ごと出さない）。taskに`paths`があれば、その次に`Paths you may change (globs from the repository root; ...): docs/**, *.md. A commit that changes any other path is not accepted: the run waits for a session to take it out. If the task needs another path, do not change it and do not run dagq ask: write the receipt with result failed and name in summary the paths it needs and what to change there (a running task's paths cannot change; the planner registers it again with wider paths).`の1行を載せる（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の決定5。宣言外のパスが要るときはaskにせず`failed`のreceiptに必要なパスを書き、plannerが`--paths`を広げて登録し直す。`scope_violation`のresumeの依頼と同じ。task 978。無ければ行ごと出さない）。

**e2e**（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定1・6、task 1239。`prompt::e2e_line`）: workerはe2eを流さない。e2eが要るrunにはruntimeがreviewのpassの後にhostで流す（[Review](review.md#着地の前のe2e)）。runがe2eを要りうるとき（taskの`required_evidence`に`e2e`があるか、main checkoutの`dagq.toml`に`[e2e] paths`があるとき）だけ、`Required evidence:`の行の次に`E2E: do not run the e2e (tests/e2e.rs) yourself. When the run needs it (...), the runtime runs it on the host after the review passes, before the run lands, and sends the run back to a session if it fails. Report `e2e` in the receipt as not_applicable with that reason.`の1行を載せる。e2eのコマンド、`[e2e] paths`のglobと見込み（task 965の`e2e_expectation`）、関門の印（task 1167・1198の`e2e_marks_line`）、Codexのworkerの除外（task 1206の`codex_worker_e2e_line`）は載せない（ADR-t963-1決定2・5とADR-t1165-1決定6はADR-t1233-2がamendsした）。印は着地の前のe2eがlanding branchの着地したcommitのtreeから読む（`Supervisor::main_quarantine`）。resumeの依頼にもe2eの行は無く、e2eが落ちて`needs_session`になったrunのresume（`ResumeKind::E2e`）だけが、reasonの落ちたtestとlogを読んで直してcommitし、再現は名前で絞った1本（`cargo test --locked --test e2e -- --ignored --exact <name>`）にとどめ全体のe2eは流さないことを頼む。最初の段落の検証の一文は「unit test・subagent reviewを行う」（Codexは「unit testを行う」）で、E2Eを含めない。testは`src/application/prompt.rs`の`the_worker_and_resume_prompts_leave_the_e2e_to_the_runtime`、`tests/it/runtime_evidence.rs`の`the_e2e_a_run_needs_is_recorded_and_its_receipt_backs_none`。

4節の後に「担当はこのtaskだけ。兄弟taskの範囲を変えず、範囲外の仕事を見つけたら受け持たずにreceiptの`follow_ups`に書く」の一文を置き、receipt JSONの例に任意の`follow_ups`（`{title, description, category}`の配列。`Receipt::check`は配列であることだけを見る）を含める。その後の「follow_ups is optional」の行に、`follow_up_categories_line`が種類の一覧（`FOLLOW_UP_CATEGORIES`のコードと短い定義）と付け方（迷ったら片付けたときに何が変わるかで選ぶ、重複は種類にしない）を足す（[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)、[follow_upsの分類コード](receipt-and-session-exit.md#follow_upsの分類コード)）。

schemaとCLIは変えない。`tests/e2e.rs`のstubはpromptの1行目とreceipt pathの行だけを読み、`follow_ups`のないreceiptを書くので、節の追加に影響されない。

言語の設定（`[language]`）が解決できるときは、promptの末尾に言語の指示の段落を足す。resumeとreviseの依頼文も同じ（[Language](language.md#promptへの渡し方)、ADR-t616-2）。

## 受け入れ条件の対応づけ

[ADR-t1420-1](../../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)（goal 90、task 1420）。workerのpromptは、receiptの書き方（`Write a completion receipt to ...`の行）の直前に`ACCEPTANCE_MAP`の1段落（英語で564文字）を置く: receiptの前に受け入れ条件の各項目を満たすもの（変えたファイル・testの名前・receiptのevidence・文書の節や測るコマンド）へ対応づけ、まだ何も満たしていない項目はその場で直す。満たせない項目を`follow_ups`に回して`succeeded`を書かず、人の判断が要れば`worker_question`（`--because scope`）、範囲の外ならfailedのreceiptにする。対応は`summary`に項目ごとの短い句で書く。対話・非対話、Claude・Codexのどのworkerのpromptも同じ文で、新しいtestの実行や検査のコマンドは求めない。

resumeの解消依頼（`resume_request`。全ての`ResumeKind`）とreviseの依頼（`revise_request`）は、receiptを書き直す手順5の末尾に`ACCEPTANCE_REMAP`（英語で237文字。task 1428で文書の照合の記録を含めて286文字）を足す: 直した項目の対応を改めて満たすものへ対応づけて`summary`の句を（照合した文書とともに。下の[文書の照合](#文書の照合)）書き直し、taskの中で満たせない項目は`worker_question`（`--because scope`）かfailedのreceiptにしてfollow_upにしない。

Codexのworkerの`review_line`（下の[subagent review](#subagent-review)）は自分のdiffを読む見直しをこの対応づけの手順に寄せ、受け入れ条件との照合を2度言わない。runのreviewの判定の基準は変えない。runのreviewのprompt（`review_prompt`）の文書の照合はtask 1429が足した（[Review](review.md#文書の照合)）。testは`src/application/prompt.rs`の`every_worker_text_that_writes_a_receipt_maps_the_acceptance_once`。

## 文書の照合

[ADR-t1428-1](../../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)（goal 91、task 1428）。workerのpromptは`ACCEPTANCE_MAP`の直後（`Write a completion receipt to ...`の行の前）に`DOCS_CHECK`の1段落（英語で394文字）を置く: 変えた挙動を説明する文書（taskが名指すものと作業中に見つけたもの）を差分と照合し、taskのpathsの中の古いものを直し、pathsの外のものは`docs_drift`のfollow_upにpathと節を書く（受け入れ条件が求める文書は上の対応づけの項目として扱い、直せなければ`worker_question`かfailedのreceipt）。`summary`に更新したpathと節か、更新が要らない理由を書き、示すためだけに文書を触らない。対応づけの手順の続き（`Then ...`）で、受け入れ条件の対応づけを2度言わない。対話・非対話、Claude・Codexのどのworkerのpromptも同じ文で、Codexの`review_line`（自分のdiffを読む文）は文書に触れない。新しい検査のコマンドやtestの実行は求めない。

resumeとreviseの依頼は別の文を足さず、`ACCEPTANCE_REMAP`の「`summary`の句を書き直す」に`, with the documents you checked against the diff`（英語で49文字。`ACCEPTANCE_REMAP`は286文字）を含める。runのreviewのpromptと資料の側（文書の照合とtaskのcontext）はtask 1429が足した（[Review](review.md#文書の照合)）。testは`src/application/prompt.rs`の`every_worker_text_that_writes_a_receipt_checks_the_documents_once`。

## 経路とproviderごとの文面

workerに送る文（`prompt.txt`・resumeの解消依頼・revise・receiptの食い違い・古いreceiptの促し・receiptの無い促し・askの答え・復旧jobの`send_instruction`・queueのholdの後の「続けて」）は、runの経路とprovider（`Route::of(run)`: `worker_mode`が`interactive`なら`Interactive`、`headless`なら`actual_provider`の`Headless(provider)`）で分ける（task 817）。対話のrunの文面は前と同じで、上の説明はすべて対話のrunのもの。非対話のrun（[非対話のworker](headless-worker.md)、[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)）は1 turnが1回の呼び出しで、`/exit`も画面への打ち込みも無いので、次のように替える。

| 箇所 | 対話 | 非対話 |
| --- | --- | --- |
| 最後の段落 | なし | `HEADLESS_WORKER`（このturnで全部を終え、receiptかaskでturnを終える。答え・revise・続きの依頼は同じsessionの次のturnのpromptで届く。backgroundの処理に頼らず、build・test・待ちはforegroundで終わりまで待つ）と、providerの1行（Claude: turnの終わりにClaude Codeが`run_in_background`のshellを止めるので、それで待つためにturnを終えない。Codex: コマンドの終わりを待ってから答える） |
| 自分の処理を止める一文 | `STOP_BACKGROUND`（`/exit`の確認画面の説明つき） | `HEADLESS_STOP`（turnを終える前に、自分が起動してまだ走っているもの（`nohup … &`は残る）をpidで止める。名前やパターンで送らない。`pkill` / `killall`の禁止は同じ） |
| 判断が要るとき | terminalに書いて待たず`dagq ask`、短く報告して止まる。答えはこのterminalに届く | 返事に質問を書いてturnを終えず`dagq ask`、短く報告してturnを終える。答えは同じsessionの次のturnのpromptで届く |
| receiptの後 | 短く報告して止まる。`/exit`を打たない | 短く報告してturnを終える。次のturnはreview・着地・人が差し戻したときだけ |
| subagent review | 「unit test・subagent reviewを行う」 | Claudeは対話と同じ。Codexは下の[subagent review](#subagent-review) |
| 依頼（resume・revise・食い違い・古いreceipt・促し） | `dagq: ...`で始まり、最後の手順は`INTERACTIVE_DONE`（`/exit`を打たない） | 先頭に`HEADLESS_NEXT_TURN`（前のturnは終わり、backgroundに残したものは止められた）の1行を置き、最後の手順は`HEADLESS_DONE`（repositoryのworker向けの指示（AGENTS.mdかCLAUDE.md）に従い、このturnで行い、askの前に`ASK_RULES_FIRST`を確かめ、判断が要れば`dagq ask --run <run> --kind worker_question`を打ってturnを終える（答えは次のturnのprompt）、終わったら短く報告してturnを終える）。receiptの無い促し（`stall_nudge`）は「前のturnがreceiptもaskも無く終わった」と書き、選択肢の3は「待つためにturnを終えたなら、それはturnと一緒に止められたのでforegroundで流し直す」にする |
| askの答え・復旧jobの指示・holdの後の続き（`answer_text`・`recovery_instruction`・`continue_text`） | `answer to ask N: ...` / `dagq: the supervisor's recovery job ... asks: ...` / `CONTINUE_TEXT`のまま | 同じ文の後に`HEADLESS_GO_ON`（これは次のturnのprompt。このturnで続け、receiptかaskで終える）を足す。先頭の行は変えない |

WORKER_READINGの「AGENTS.mdかCLAUDE.md」の指示は両方の経路で同じ（Codexは起動時にAGENTS.mdを読む）。

### subagent review

providerごとに決める（task 817）。

- **Claude（対話・非対話）**: 今までどおり、該当すればsubagent（Claude Codeのsubagent）でreviewし、receiptの`subagent_review`にevidenceか該当しない理由を書く。非対話でもsubagentは同じturnの中で動くので変えない。
- **Codex**: `codex exec`の中にsubagentは無く、`codex exec review`を入れ子で起動すると、workspace-writeのsandboxでは`$CODEX_HOME`（`~/.codex`）のsessionを書けず、呼び出しと費用も倍になる（[spike](../../plans/headless-worker-spike.md)の1.と4.）。そこでCodexのworkerはsubagent reviewをしない。promptは代わりに、receiptの前の受け入れ条件の対応づけ（[受け入れ条件の対応づけ](#受け入れ条件の対応づけ)）のときに自分のdiff（`git diff <base commit>..HEAD`）を読んで見直して直し、`subagent_review`を`not_applicable`にして理由（`codex worker: no subagent review; self-reviewed the diff, the supervisor's review job reviews the commit`）と見直しで見つけたことを書くよう指示する。着地の前には全runと同じくsupervisorのheadlessのreview job（Claude）がcommitをreviewする。
- **taskの`required_evidence`に`subagent_review`があるとき**: `domain::required_of(required, provider)`が、runの`actual_provider`がCodexなら`subagent_review`を要るevidenceから外す。validating（`check_receipt`）・`integrate`のreceiptの検査・resumeの解決の判定・promptの`Required evidence:`の行は、どれもこれで絞った一覧を使う。Codexのrunのreceiptの`subagent_review`は要らないcheckと同じ扱いになり、`failed`でなく理由のあることだけを見る（`not_applicable`と理由で通る）。runが途中でClaudeに切り替わった（ADR-t813-2のフォールバック）後は`actual_provider`がClaudeなので、要るevidenceに戻る。testは`src/domain/receipt.rs`の`a_codex_run_does_not_back_a_required_subagent_review`と`src/application/integrate.rs`の`a_codex_receipt_passes_without_a_subagent_review_the_task_requires`。

### 復旧jobのprompt

`recovery_prompt`も経路で分ける。非対話のrunでは、許す操作から`answer_known_dialog`と`close_and_proceed`を必ず外し（`HEADLESS_NEVER`。呼び出し側が渡しても出さず、jobがそれを返してもruntimeの前提の検査（`check_live`）が拒んでverdictをescalationにする）、`send_instruction`の説明を「次のturnのpromptとして1回送る」にし、画面の見出しを「Last turns of the headless session (it has no screen)」に、`stalled`の意味を`turn_without_receipt` / `permission_denied`の説明に、禁止の一覧の「既知でないダイアログへのキー」を「sessionへの打ち込み（非対話のsessionはキーを取らない）」にする。終わった非対話のrunのtriageの画面の欄は「(the session is gone: its turns are above)」、`ended_run_material`の最後の画面の見出しは非対話のsessionには画面が無くturnが続くと書く。対話のrunの文面は変えない。testは`src/application/prompt.rs`の`headless_sessions_are_told_to_finish_in_a_turn_and_never_about_exit`・`interactive_sessions_keep_their_texts`・`a_headless_recovery_job_is_never_offered_a_dialog`。

## repositoryの規則を読む順

runtimeはrepositoryの規則（検証のコマンド、宣言するpaths、要るevidence、ADRのような記録の規則）を持たず、promptはsessionをrepositoryの指示へ向けるだけにする（goal 52、task 625）。promptと固定の文字列には、dagqのrepositoryの規則（ADRの索引や番号の付け方、dagqのADR番号、Rustのlinterの名前など）を書かない。

- **worker**: `WORKER_READING`と`local_checks`が「AGENTS.mdかCLAUDE.md」を名指す。
- **planner**: `repository_rules(ask)`の一文が、taskの`--verify`・`--paths`・`--evidence`をrepositoryの指示とそれが名指す文書・規則から、AGENTS.md → （無ければ）CLAUDE.md → （どちらも無ければ）README・CIの設定・buildの設定の順で決め、どれでも決まらなければ`ask`する、と指示する。`ask`はruntimeが立てるplanner（`runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`）では`RUNTIME_PLANNER_ASK`で、source・ADR・人の先例から自分で決め、その材料で決めきれず人の判断（`scope`・`discard`）に当たるか確信度が`low`のときだけ`planner_question`のaskにする（ADR-t451-1決定5、task 1320）。
- **plan review**: repositoryの指示（AGENTS.md・CLAUDE.md）と、それが名指す文書・規則（とくにplan review向けの記述）を読んで当てはめさせ、AGENTS.mdが無いrepositoryではCLAUDE.md → README・CIの設定・buildの設定の順で判断し、どれでも決まらなければ`concern`にさせる。dagqのrepositoryでは、AGENTS.mdの「plan review」の節が`docs/development/task-registration.md`の「plan reviewが当てはめる規則」を名指し、そこが`docs/adr/README.md`とADRのIDの規則（`docs/development/documents.md`の「ADRのID」）を名指す。
- **review**: `review_prompt`の`REVIEW_RULES`の一文が、providerに依らず、worktreeのrootのrepositoryの指示（AGENTS.md・CLAUDE.mdのあるもの）とそれが名指す文書を読み、変更に当たる規則で差分を判定させる。資料の行の後、`REVIEW_DOCS_CHECK`（文書の照合）の前に置く。Claudeのreviewは`--setting-sources ""`で起動して`CLAUDE.md`をmemoryとして読まないので、promptで名指す（[ADR-t1470-1](../../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)決定2、[Review](review.md)の「headless実行」）。`revise`の例は「repositoryのformatter・linter・その他の検査の指摘」で、特定の言語のツールを名指さない。

follow_up・goal gap・findingのdraftの`context`の見出しは英語（`follow-up draft (proposed by the receipt of run <run> of task <id>)`、`goal gap draft (proposed by the judgment of goal <id>)`、`from finding <id> (<kind>)`。[Language](language.md#日本語が残っていた固定の文字列)）。testは`src/application/prompt.rs`の`prompts_take_the_rules_from_the_repository_in_order`と`the_review_prompt_names_the_repositorys_instructions`（reviewの`REVIEW_RULES`の位置と中身）、`tests/it/plan_review.rs`。

## headlessのjobのprompt

[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)（task 1566）。supervisorが起動するheadlessのjobとruntimeのplannerのpromptの共通の方針で、この節がjobごとの今の姿の正本。各jobの文書（[Headless job processes](headless-job-processes.md)、[Plan review](plan-review.md)、[Observer](observer.md)、[Goal review](goal-review.md)、[スループットの見直し](throughput-review.md)、[Review](review.md)、[復旧job](background-recovery-job.md)、[Session prompts](session-prompts.md)）はpromptの大きさと渡し方についてここを指す。

- **渡し方**（決定1）: 大きさに関係なくファイルかstdinで渡し、引数で渡さない。今はどのjobも引数（Claudeは`claude -p … -- <prompt>`、Codexは`codex exec --json … -- <prompt>`の位置引数。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）で渡し、引数とenvの合計がhostの`ARG_MAX`（macOSで1MiB）を超えると`Argument list too long (os error 7)`で起動できない（2026-10-03のplan review、2026-10-01T13:54Zからのobserver）。ファイルかstdinへの切り替えはtask 1560が行い、決まった形はそのtaskがここと[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)に書く。
- **載せるもの**（決定2）: 判断の材料だけ。一覧・全文の大量のデータはIDと要約にし、中身は下の表の「取りに行く経路」で必要なものだけ読ませる。
- **取りに行く経路**（決定3）: jobの権限の意図（`JobAccess`、[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）と許す道具で実際に読める経路だけを読む方法として書く。`queue_cli`のjob（observer・スループットの見直し）はファイルを読めないので、jobのdirの`input.json`をファイルとして読む方法にしない（人が読み、observerの`input.json`はqueue serviceの`observe --input`を通してだけ読む。[Observer](observer.md#promptの入力の上限と選ぶ順)）。`read_files`のjob（runのreview・復旧job）は意図として`dagq`を打てないので（Codexのreviewはsandboxの都合で読むコマンドを打てても、promptはそれを読む方法にしない）、省いてよいのはそのjobが読めるファイル（worktreeとrun directory）にあるものだけ。
- **上限と選ぶ順**（決定4）: 節ごとの件数かbyteの上限と全体の上限を持ち、超えたときに残す順（関連の強さ、新しさ）は決まった規則で決める。
- **省いたことの明示**（決定5）: 節ごとに省いた件数と読む方法をpromptに書く。
- **記録とtest**（決定6）: jobごとにpromptのbyte数をeventに記録し、上限をtestで確かめる。記録するのは今はplan review（`plan_review_finished`と`plan_review_failed`の`prompt_bytes`。下の「plan reviewの上限」）とobserver（`observe_started`の`prompt_bytes`・`prompt_limit`・`prompt_sections`、task 1567）。ほかのjobのeventと欄の名前は、各jobの上限を入れるtaskがここに書く。

| job | 今の渡し方 | 権限の意図 | 今の節（材料） | 今の上限 | 取りに行く経路 |
| --- | --- | --- | --- | --- | --- |
| plan review（[Plan review](plan-review.md)の4） | 引数（Claude・Codex。task 1560が替える） | `read_files_and_queue_cli`（読むだけの`dagq`とファイルの読み取り） | proposalのtaskの全field、関係するgoal、`lint`、他の`submitted` / `revising`のproposal、`ready` / `in_progress`のtaskの要約と一部の全文、予想するファイル、人が答えたask、衝突の多いファイル、重複と実装済みの候補（2026-10-03のplan review 724で1,148,345 byte。`ready` / `in_progress`のtaskの全文が72%、要約が12%、人が答えたask・hotspot・重複の候補が合わせて8%（人が答えたaskだけでは20,712 byte、約2%）。内訳は下の「plan reviewの上限」） | 全体400,000 byte（`PLAN_REVIEW_PROMPT_LIMIT`）、必須の節200,000 byte、全文20件・100,000 byte、要約64,000 byte、人が答えたask 20件・16,000 byte、hotspot 16,000 byte、重複の候補48,000 byte、他のproposal 48,000 byte（task 1561。値・理由・選ぶ順・必須の節は下の「plan reviewの上限」）。読む件数は要約200件（`QUEUED_TASKS`）、hotspot 15件、重複の候補はtaskごとに5件、要約の行の`expected_files`は10件（`SUMMARY_EXPECTED_FILES`）。byte数は`plan_review_finished` / `plan_review_failed`の`prompt_bytes` | `dagq show`・`proposal show`・`search`・`related`・`findings`・`stats`・`events --full`・`timeline`、上限で省いたものを読む方法として`dagq show ID --full`・`goal show ID --full`・`proposal show ID`・`lint --proposal ID`・`related ID`・`search`・`asks --all`・`stats`・`list --status ready,in_progress --limit 200`（`prompt::PLAN_REVIEW_READS`。task 1561）、repositoryのファイル |
| observer（[Observer](observer.md)） | 引数（`prompt.md`はjobのdirに書くが、渡すのは引数。task 1560が替える） | `queue_cli`（`Bash(dagq:*)`だけ） | 指示（必須の節）と入力の節: 必須の資料の`kpi.breaches`・`open_asks`・`stats.alerts`・`stats.running_alerts`、省いてよい`stats`・`kpi`（`trend`・`forecast`は要約）・`findings`・`improvements`・`notes`・`graph.critical`・`graph.candidates`（2026-10-03の本番は1,226,750 byteで`stats`が51%・`kpi`が31%・`findings`が8.5%。上限の後の推定は約127KB） | 全体`PROMPT_LIMIT` 160,000 byte。必須の資料は全体の上限だけで切られ、他の節は節ごとの件数とbyteの上限（例: `stats` 40,000 byte・1 key 8,000、`findings` 100件・48,000）。選ぶ順・必須の節・項目の縮め方は[Observer](observer.md#promptの入力の上限と選ぶ順)（task 1567）。byte数は`observe_started`の`prompt_bytes`・`prompt_limit`・`prompt_sections`と`observe --history` | 省いたものはobservationの時点の入力を`observe --input <observation> --section <節>`、今の状態を`stats`・`kpi`・`findings`・`notes`・`asks --open`・`graph`・`candidates`。ほかに`forecast`・`events --full`・`timeline`・`observe --history` |
| goal review（[Goal review](goal-review.md)の4） | 引数（task 1560が替える） | `read_files_and_queue_cli` | goal、所属taskのdescription・acceptance、着地したrunのreceipt、goalのevent、前回までのgoal review（150〜220KB） | 無い。上限を入れるtaskが決めてここに書く（まだ登録されていない） | `dagq show`・`goal show --full`・`findings`・`events --goal`・`search`、repositoryのファイル |
| スループットの見直し（[スループットの見直し](throughput-review.md#promptの入力task-1099)） | 引数（上限で`ARG_MAX`に当たらない。task 1560が替える） | `queue_cli` | 指示・手順（pluginの`reference/kpi.md`の節）と入力の要約 | prompt全体が`PROMPT_LIMIT`（128KiB）、入力が`PROMPT_INPUT_LIMIT`（96KiB）。超えたら`DROP_ORDER`で落として`omitted_to_fit`に名を残し、promptが細部を読むコマンドを示す（task 1099）。byte数の記録は無い | `kpi`・`stats --full`・`timeline`・`events --full` |
| runのreview（[Review](review.md)） | 引数（task 1560が替える） | `read_files`（worktreeとrun directoryのファイルだけ） | taskの記述・context、`review.md`の資料（約10KB） | 無い。上限を入れるtaskが決めてここに書く（まだ登録されていない） | worktreeとrun directoryのファイル |
| 復旧job（[復旧job](background-recovery-job.md)） | 引数（task 1560が替える） | `read_files` | taskのdescription・acceptance・verification_commandsと`task_edited`、alertの意味と事実、`capture`した画面の末尾、runのプロセスの一覧、worktreeのHEADとreceiptの`commit`と`git status`、そのrunの過去の自動修正とverdict（[復旧job](background-recovery-job.md)の`recovery_prompt`） | 無い。上限を入れるtaskが決めてここに書く（まだ登録されていない） | run directoryのファイル |
| runtimeのplanner（[Session prompts](session-prompts.md)、[非対話のworker](headless-worker.md)） | 非対話のturnの`claude -p … -- <prompt>`の引数（[非対話のworker](headless-worker.md)。task 1560が替えるかは同taskが決める） | plannerのrole（読むコマンドと計画のコマンド。[Authorization](../authorization.md)） | `runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`の指摘とtaskの行 | 無い。上限を入れるtaskが決めてここに書く（まだ登録されていない） | `dagq show`・`proposal show`・`search`・`related`・`findings`・`events --full`、repositoryのファイル |

### plan reviewの上限

task 1561。仕組み（必須の節の替え方、選ぶ順、省いたことの書き方、`prompt_bytes`の欄）は[Plan review](plan-review.md)の4の「上限と選ぶ順」が持ち、ここは値と理由を持つ。値は`src/application/prompt.rs`の定数。基にしたのは2026-10-03のplan review 724（proposal 559、15 task）の`prompt.txt`（1,148,345 byte）を節の見出しで分けて数えた内訳: `ready` / `in_progress`のtaskの全文821,084 byte（179件、1件平均4,587 byte）、要約132,986 byte（200件、平均665 byte）、人が答えたask 20,712 byte（30件、平均690 byte）、重複の候補62,000 byte（15行、平均4,107 byte）、hotspot 4,815 byte、proposalのtask 60,828 byte、goal・予想するファイル・lint・他のproposal・指示など45,920 byte。

| 定数 | 値 | 理由 |
| --- | --- | --- |
| `PLAN_REVIEW_PROMPT_LIMIT`（全体、言語の指示を含む） | 400,000 byte | macOSの`ARG_MAX`（1,048,576 byte）の4割未満で、引数で渡す今（task 1560の前）もenvを足して余裕がある。724の必須の節（約107KB）と省いてよい節の上限の和に近く、724は全体の上限に当たらずに約335KBに収まる |
| `PLAN_REVIEW_REQUIRED_LIMIT`（必須の節） | 200,000 byte | 全体の半分。724の必須の節（約107KB）の2倍弱で、普通のproposalでは替えが起きず、超えても省いてよい節に半分が残る |
| `QUEUED_FULL_TASKS` / `QUEUED_FULL_BYTES`（全文） | 20件 / 100,000 byte | 全文が724の72%を占めた。重なりの強い上位20件（平均4.6KBで約92KB）を読めば依存の判断に足り、残りは`dagq show ID --full`で読める。byteの上限は1件の巨大なtaskが節を塞がないため |
| `QUEUED_SUMMARY_BYTES`（要約） | 64,000 byte | 724の要約（約133KB）の半分弱で、平均の行で約96件。重なるものから載せ、残りは`dagq list`で読める |
| `PRECEDENT_ASKS` / `PRECEDENT_BYTES`（人が答えたask） | 20件 / 16,000 byte | 1件の問いと答えはそれぞれ400文字まで（`PRECEDENT_CHARS`）で日本語なら2KBを超えうる。新しい20件で先例の候補に足り、古いものは`dagq asks --all`で読める |
| `HOTSPOT_BYTES`（衝突の多いファイル） | 16,000 byte | 15件（`HOTSPOT_FILES`）で約5KB。依存の判断の要なので最初に残し、上限は`queued_tasks`の並びが長いときだけに当たる |
| `CANDIDATE_BYTES`（重複と実装済みの候補） | 48,000 byte | 724で約62KB（taskごと約4KB）。15 taskのうち11 task分に当たり、残りは`dagq related` / `dagq search`で引ける |
| `OTHER_PROPOSAL_BYTES`（他のproposal） | 48,000 byte | 他のproposalのtaskのdescription・acceptanceを丸ごと載せるので、待つproposalが多いと伸びる。前に出されたものとの食い違いの検査に要るので重複の候補の次に残し、残りは`dagq proposal show ID`で読める |
| `OMISSION_NOTE_BYTES`（節ごとの注記の空き） | 2,000 byte | 注記は省いた件数・読むコマンド・IDの並び（40件まで）で収まる |

省いてよい節は、全体の上限の残りを衝突の多いファイル → 重複の候補 → 他のproposal → 人が答えたask → 全文 → 要約の順に取る（依存と重複の判断に直接効くものを先に、CLIの一覧で代えやすい要約を最後にする）。必須の節（指示と検査とverdictの形、proposalのtaskの全文、予想するファイル、goal、`lint`、最後の言語の指示）は省かず、必須の節だけで200,000 byteを超えるときだけ大きいものから読むコマンドの案内に替える（jobの今の権限で打てる読むだけの`dagq`。jobのdirのファイルには退避しない）。

workerの`prompt.txt`（この文書の上の節）はADR-t1566-1の範囲に含めない。
