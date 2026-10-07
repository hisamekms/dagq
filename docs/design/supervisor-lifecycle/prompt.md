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

`prompt.txt`は`src/application/prompt.rs`の`prompt(task, run, goal, predecessors, goal_predecessors, siblings)`（`runtime::prompt`として再公開）が生成するclaim時点のスナップショットで、run中にqueueが変わっても書き換えない。goalの`goal edit`も、兄弟taskの状態変化も、走行中のrunには届かず、次のclaimのpromptから反映される。task単体の情報（ID、run ID、title、description、acceptance、verification_commands）、receiptの契約（pathとJSONの形）に加えて、[ADR-0009](../../adr/0009-goal-groups-tasks.md)の次の4節をこの順で検証コマンドとreceiptの契約の間に載せる。どれも常に書き、該当がなければ`none`にして、promptの節構成をgoal・context・依存・並列の有無で変えない。

- **Goal**: taskに`goal_id`があれば、claim時点の`TaskStore::show_goal()`のgoalを`Goal ID` / `Goal title` / `Goal description` / `Goal acceptance` / `Goal constraints` / `Goal doc`の行で載せる。`doc`はrepository内のpathをそのまま書き、内容は読まない（なければ`Goal doc: none`）。goalのないtaskは`Goal: none, this task stands alone`。
- **Context**: taskの`context`が空白でなければ本文をそのまま載せ、空なら`Context: none`。
- **Predecessor tasks**: taskの直接の依存元（`task_dependencies`のpredecessor）ごとに1行、`- task <ID>: <title>; result commit <sha>; summary: <text>`。`result commit`は依存元の`integrated` runの`result_commit`（`integrate`がmainに積んだsquash commit）、`summary`はそのrunのreceipt（`receipt_path`。なければ`<run-dir>/receipt.json`）の`summary`（空白を1つに畳む。空なら`(no summary)`）。receiptが読めない・parseできない・pathが不明なら`(receipt unavailable)`、integrated runがなければ（手で`completed`にしたなど）`result commit (not landed)`と書き、いずれもprovisionを止めない。取得はqueueの読み取り専用操作`TaskStore::predecessors(task_id)`（ID順。依存元の`Task`と`integrated` runの`Option<TaskRun>`）で、summaryの読み取りはapplication側（`application::prompt::PredecessorSummary::from_predecessor`）が`RunFiles`越しに行う。task依存の行に続けて、taskのgoal依存（[ADR-0038](../../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)。claim時点ではすべて`achieved`で閉じている）ごとに`- goal <ID> (closed as achieved): <title>; its completed tasks:`を書き、その下にgoalの`completed`のtaskを1行ずつ`  - task <ID>: <title>; result commit <sha>; summary: <text>`で並べる（無ければ`  - none`）。行の作り方はtask依存と同じだが、goalはtaskが多くなりうるのでsummaryを`GOAL_TASK_SUMMARY_CHARS`（200文字）で切って`…`を付ける。取得は`TaskStore::goal_predecessors(task_id)`（goal ID順の`GoalPredecessor`: goalと、その`completed`のtaskの`Predecessor`のID順）、組み立ては`GoalPredecessorSummary::from_goal_predecessor`。依存元もgoal依存もなければ`Predecessor tasks: none`。
- **Sibling tasks in progress**: `TaskStore::tasks_in_progress()`が返す`in_progress`のtask（ID順）から自分のtaskを除き、taskにgoalがあれば同じ`goal_id`のtaskに限定したものを`- task <ID>: <title>`で並べる（`siblings_in_progress`）。goalのないtaskはgoalの有無を問わず全`in_progress` taskを見る。claimは`fill_slots`で1件ずつ順に行うので、同じpassで後にclaimされたtaskのpromptには先にclaimされたtaskが載り、その逆は載らない。`awaiting_integration`や`needs_session`のrunを持つtaskも`in_progress`なので載る。なければ`Sibling tasks in progress: none`。

冒頭（worktreeだけで作業する指示の直後）に、最初に読むものを`WORKER_READING`の一文に限定する: repository instructions（AGENTS.mdかCLAUDE.md。`local_checks`と揃える）のworker節、この下のtask context（とそれが名指す文書）、goal doc、依存元のsummaryだけを読み、`dagq list` / `dagq show`は打たず、docs全体は読まず、他のファイルはtaskが必要とするときだけ開く（goal 11の決定4。runに要る情報はpromptに載っていて、queueの一覧やdocs全体を読むのは最初のcommitを遅らせるだけ）。

判断が要るときの手順も載せる: 返事に質問を書いてturnを終えるのではなく、worktreeで`dagq ask --run <run-id> --kind worker_question --because scope --topic <code> --question '...'`を打ち、短く報告してturnを終える。askの前に、repositoryの指示（AGENTS.mdかCLAUDE.md）がその問いをaskにせず自分で決める・`failed`のreceiptにすると定めていないかを確かめ、定めていればそれに従うよう書く（`ASK_RULES_FIRST`。runtimeはrepositoryの規則を持たないので、ADRのIDの衝突など個別の規則は書かない。task 978）。promptのaskの手順の文の前に置き、依頼の`HEADLESS_DONE`の`If you need a decision`の前と、receiptの無い促し（`stall_nudge`）の選択肢の2（`<ASK_RULES_FIRST> Otherwise, if you need a decision, run ...`）にも同じ文を置く。`--topic`には問いの中身の分類コード（[ADR-t947-2](../../adr/2026-09-28-t947-2-worker-questions-carry-topic-codes.md)）を主から付け、promptはコードの一覧と定義と主の選び方（`worker_question_topics_line`。一覧は`domain::WORKER_QUESTION_TOPICS`、[ask](ask.md#worker_questionの分類コード)）を載せる。回答は`answer to ask <id>: ...`として同じsessionの次のturnのpromptで届く（[workerの質問への回答の送信](worker-question-answer.md#workerの質問への回答の送信)）。

末尾の「receiptを書いたら短く報告してturnを終える」の前に`HEADLESS_STOP`を置き、turnを終える前に自分が始めてまだ走っている処理（`nohup … &`はturnの後も残る）をpidで止めるよう指示する（名前やパターンで送らない。`pkill` / `killall`の禁止。task 359）。対話のworkerに送った`STOP_BACKGROUND`（`/exit`の確認画面の説明つき）と`answer_known_dialog`の自動応答は、対話のworkerの廃止（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、task 1437・1438）で消した。

verification commandsは`Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):`の見出しで一覧を見せ、その直後に`local_checks`の一文を置く: worktreeで流すのはrepositoryの指示（AGENTS.mdかCLAUDE.md）がworkerに求める検証で、それはverification commandsの一部をintegrateに任せてよく、指示が何も求めないときはverification commandsを流す。同じ一文をresume（`evidence_missing`・`scope_violation`・`sent_back`・triage・rebase（`Landing`）・reviewのpass後の衝突（`Precheck`））とreviseの手順2にも載せ（そこではverification commandsをJSONの一覧で書く）、integrateのrebase（`Landing`）の手順2には「reasonがintegrateのrebase後に落ちた検証コマンドなら、そのコマンドを手元で流して再現して直してよい」を足す。retryが引き継いだrunの節も「上の検証を流し直す」と書く。runtimeはcargoやllvm-covなど特定のツールの名前を決め打ちしない（dagqは他のrepositoryでも動く。どの検証をintegrateだけに任せるかはrepositoryの指示が決める）（task 510）。新しいADRは作らない: [ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定1の「同じcommitのverificationはintegrateの1回が正」に沿ってpromptの文面を直すだけで、決定は変わらないため。

taskの`required_evidence`のうちworkerが裏付けるcheck（`domain::required_of`。`e2e`は含めず、Codexのworkerは`subagent_review`も含めない）があれば、verification commandsの直後（4節の前）に`Required evidence: tests (each must be passed with evidence in the receipt, or the run waits for a session to add it)`の1行を載せ、workerに事前に知らせる（無ければ行ごと出さない）。taskに`paths`があれば、その次に`Paths you may change (globs from the repository root; ...): docs/**, *.md. A commit that changes any other path is not accepted: the run waits for a session to take it out. If the task needs another path, do not change it and do not run dagq ask: write the receipt with result failed and name in summary the paths it needs and what to change there (a running task's paths cannot change; the planner registers it again with wider paths).`の1行を載せる（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の決定5。宣言外のパスが要るときはaskにせず`failed`のreceiptに必要なパスを書き、plannerが`--paths`を広げて登録し直す。`scope_violation`のresumeの依頼と同じ。task 978。無ければ行ごと出さない）。

**e2e**（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定1・6、task 1239。`prompt::e2e_line`）: workerはe2eを流さない。e2eが要るrunにはruntimeがreviewのpassの後にhostで流す（[Review](review.md#着地の前のe2e)）。runがe2eを要りうるとき（taskの`required_evidence`に`e2e`があるか、main checkoutの`dagq.toml`に`[e2e] paths`があるとき）だけ、`Required evidence:`の行の次に`E2E: do not run the e2e (tests/e2e.rs) yourself. When the run needs it (...), the runtime runs it on the host after the review passes, before the run lands, and sends the run back to a session if it fails. Report `e2e` in the receipt as not_applicable with that reason.`の1行を載せる。e2eのコマンド、`[e2e] paths`のglobと見込み（task 965の`e2e_expectation`）、関門の印（task 1167・1198の`e2e_marks_line`）、Codexのworkerの除外（task 1206の`codex_worker_e2e_line`）は載せない（ADR-t963-1決定2・5とADR-t1165-1決定6はADR-t1233-2がamendsした）。印は着地の前のe2eがlanding branchの着地したcommitのtreeから読む（`Supervisor::main_quarantine`）。resumeの依頼にもe2eの行は無く、e2eが落ちて`needs_session`になったrunのresume（`ResumeKind::E2e`）だけが、reasonの落ちたtestとlogを読んで直してcommitし、再現は名前で絞った1本（`cargo test --locked --test e2e -- --ignored --exact <name>`）にとどめ全体のe2eは流さないことを頼む。最初の段落の検証の一文は「unit test・subagent reviewを行う」（Codexは「unit testを行う」）で、E2Eを含めない。testは`src/application/prompt.rs`の`the_worker_and_resume_prompts_leave_the_e2e_to_the_runtime`、`tests/it/runtime_evidence.rs`の`the_e2e_a_run_needs_is_recorded_and_its_receipt_backs_none`。

4節の後に「担当はこのtaskだけ。兄弟taskの範囲を変えず、範囲外の仕事を見つけたら受け持たずにreceiptの`follow_ups`に書く」の一文を置き、receipt JSONの例に任意の`follow_ups`（`{title, description, category, membership_proposal}`の配列。`Receipt::check`は配列であることだけを見る）を含める。その後の「follow_ups is optional」の行に、`follow_up_categories_line`が種類の一覧（`FOLLOW_UP_CATEGORIES`のコードと短い定義）と付け方（迷ったら片付けたときに何が変わるかで選ぶ、重複は種類にしない）を足す（[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)、[follow_upsの分類コード](receipt-and-session-exit.md#follow_upsの分類コード)）。その次の段落の`FOLLOW_UP_PROPOSAL`は、follow_upごとに問題と根拠をdescriptionに書き、任意の`membership_proposal`（元goalのacceptanceのどの項目に関わるか、実施しなくても満たせると考えるか）を提案として書き、自分で判断も移動もしないことを言う（ADR-t1504-2決定11、[follow_upsの所属の提案](receipt-and-session-exit.md#follow_upsの所属の提案)）。

schemaとCLIは変えない。`tests/e2e.rs`のstubはpromptの1行目とreceipt pathの行だけを読み、`follow_ups`のないreceiptを書くので、節の追加に影響されない。

言語の設定（`[language]`）が解決できるときは、promptの末尾に言語の指示の段落を足す。resumeとreviseの依頼文も同じ（[Language](language.md#promptへの渡し方)、ADR-t616-2）。

## 受け入れ条件の対応づけ

[ADR-t1420-1](../../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)（goal 90、task 1420）。workerのpromptは、receiptの書き方（`Write a completion receipt to ...`の行）の直前に`ACCEPTANCE_MAP`の1段落（英語で564文字）を置く: receiptの前に受け入れ条件の各項目を満たすもの（変えたファイル・testの名前・receiptのevidence・文書の節や測るコマンド）へ対応づけ、まだ何も満たしていない項目はその場で直す。満たせない項目を`follow_ups`に回して`succeeded`を書かず、人の判断が要れば`worker_question`（`--because scope`）、範囲の外ならfailedのreceiptにする。対応は`summary`に項目ごとの短い句で書く。Claude・Codexのどのworkerのpromptも同じ文で、新しいtestの実行や検査のコマンドは求めない。

resumeの解消依頼（`resume_request`。全ての`ResumeKind`）とreviseの依頼（`revise_request`）は、receiptを書き直す手順5の末尾に`ACCEPTANCE_REMAP`（英語で237文字。task 1428で文書の照合の記録を含めて286文字）と、その後にfollow_upの所属の提案の短い形`FOLLOW_UP_PROPOSAL_AGAIN`（task 1508）を足す: 直した項目の対応を改めて満たすものへ対応づけて`summary`の句を（照合した文書とともに。下の[文書の照合](#文書の照合)）書き直し、taskの中で満たせない項目は`worker_question`（`--because scope`）かfailedのreceiptにしてfollow_upにしない。

Codexのworkerの`review_line`（下の[subagent review](#subagent-review)）は自分のdiffを読む見直しをこの対応づけの手順に寄せ、受け入れ条件との照合を2度言わない。runのreviewの判定の基準は変えない。runのreviewのprompt（`review_prompt`）の文書の照合はtask 1429が足した（[Review](review.md#文書の照合)）。testは`src/application/prompt.rs`の`every_worker_text_that_writes_a_receipt_maps_the_acceptance_once`。

## 文書の照合

workerのpromptは`ACCEPTANCE_MAP`の直後に`DOCS_CHECK`の1段落を置き、対応づけの続き（`Then ...`）として文書の照合を指示する（[ADR-t1428-1](../../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)、[ADR-t1942-2](../../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)）。

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
reviewの側は[Review](review.md#文書の照合)、testの入口は`src/application/prompt.rs`の`every_worker_text_that_writes_a_receipt_checks_the_documents_once`。

## 経路とproviderごとの文面

workerに送る文（`prompt.txt`・resumeの解消依頼・revise・receiptの食い違い・古いreceiptの促し・receiptの無い促し・答えが届かずに閉じたaskの知らせ・askの答え・復旧jobの`send_instruction`・queueのholdの後の「続けて」）は、どれも非対話のsession（[非対話のworker](headless-worker.md)、[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)）の文面で、1 turnが1回の呼び出しで、`/exit`も画面への打ち込みも無い。分けるのはprovider（`Route::of(run)`: runの`actual_provider`）だけで、違いは最後の段落のproviderの1行と下の[subagent review](#subagent-review)である（task 817）。対話のworkerの文面（`Route::Interactive`、`STOP_BACKGROUND`、`INTERACTIVE_DONE`、terminalに書いて待たず答えはterminalに届く、`/exit`を打たない）は、対話のworkerの廃止（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、task 1438）で消した。`worker_mode`が`interactive`と記録されたrunはclaimとresumeで非対話に変わり（[非対話のworker](headless-worker.md#対話と記録されたtaskのclaimとresume)）、送る文面はClaudeの非対話のrunと同じになる（testは`src/application/prompt.rs`の`a_run_recorded_as_interactive_is_sent_the_headless_claude_texts`と`the_nudge_is_the_next_turn_whatever_mode_the_run_recorded`）。

| 箇所 | 文面 |
| --- | --- |
| 最後の段落 | `HEADLESS_WORKER`（このturnで全部を終え、receiptかaskでturnを終える。答え・revise・続きの依頼は同じsessionの次のturnのpromptで届く。backgroundの処理に頼らず、build・test・待ちはforegroundで終わりまで待つ）と、providerの1行（Claude: turnの終わりにClaude Codeが`run_in_background`のshellを止めるので、それで待つためにturnを終えない。Codex: コマンドの終わりを待ってから答える） |
| 自分の処理を止める一文 | `HEADLESS_STOP`（turnを終える前に、自分が起動してまだ走っているもの（`nohup … &`は残る）をpidで止める。名前やパターンで送らない。`pkill` / `killall`の禁止） |
| 判断が要るとき | 返事に質問を書いてturnを終えず`dagq ask`、短く報告してturnを終える。答えは同じsessionの次のturnのpromptで届く |
| receiptの後 | 短く報告してturnを終える。次のturnはreview・着地・人が差し戻したときだけ |
| subagent review | Claudeは「unit test・subagent reviewを行う」。Codexは下の[subagent review](#subagent-review) |
| 依頼（resume・revise・食い違い・古いreceipt・促し・閉じたaskの知らせ） | 先頭に`HEADLESS_NEXT_TURN`（前のturnは終わり、backgroundに残したものは止められた）の1行を置き、最後の手順は`HEADLESS_DONE`（repositoryのworker向けの指示（AGENTS.mdかCLAUDE.md）に従い、このturnで行い、askの前に`ASK_RULES_FIRST`を確かめ、判断が要れば`dagq ask --run <run> --kind worker_question`を打ってturnを終える（答えは次のturnのprompt）、終わったら短く報告してturnを終える）。古いreceiptの促しは「runがturnを終えたが、receiptが別のcommitを名指す」と書く。receiptの無い促し（`stall_nudge(run)`）は「前のturnがreceiptもaskも無く終わった」と書き、選択肢の3は「待つためにturnを終えたなら、それはturnと一緒に止められたのでforegroundで流し直す」にする。閉じたaskの知らせ（`closed_question_notice`）は「このturnで」自分で決めるかfailedのreceiptを書くよう頼み、続けばsupervisorが復旧jobに渡すと書く |
| askの答え・復旧jobの指示・holdの後の続き（`answer_text`・`recovery_instruction`・`continue_text`） | `answer to ask N: ...` / `dagq: the supervisor's recovery job ... asks: ...` / `CONTINUE_TEXT`の後に`HEADLESS_GO_ON`（これは次のturnのprompt。このturnで続け、receiptかaskで終える）を足す。先頭の行は変えない |

WORKER_READINGの「AGENTS.mdかCLAUDE.md」の指示はどのproviderでも同じ（Codexは起動時にAGENTS.mdを読む）。

### subagent review

providerごとに決める（task 817）。

- **Claude**: 該当すればsubagent（Claude Codeのsubagent）でreviewし、receiptの`subagent_review`にevidenceか該当しない理由を書く。subagentは同じturnの中で動く。
- **Codex**: `codex exec`の中にsubagentは無く、`codex exec review`を入れ子で起動すると、workspace-writeのsandboxでは`$CODEX_HOME`（`~/.codex`）のsessionを書けず、呼び出しと費用も倍になる（[spike](../../plans/headless-worker-spike.md)の1.と4.）。そこでCodexのworkerはsubagent reviewをしない。promptは代わりに、receiptの前の受け入れ条件の対応づけ（[受け入れ条件の対応づけ](#受け入れ条件の対応づけ)）のときに自分のdiff（`git diff <base commit>..HEAD`）を読んで見直して直し、`subagent_review`を`not_applicable`にして理由（`codex worker: no subagent review; self-reviewed the diff, the supervisor's review job reviews the commit`）と見直しで見つけたことを書くよう指示する。着地の前には全runと同じくsupervisorのheadlessのreview job（Claude）がcommitをreviewする。
- **taskの`required_evidence`に`subagent_review`があるとき**: `domain::required_of(required, provider)`が、runの`actual_provider`がCodexなら`subagent_review`を要るevidenceから外す。validating（`check_receipt`）・`integrate`のreceiptの検査・resumeの解決の判定・promptの`Required evidence:`の行は、どれもこれで絞った一覧を使う。Codexのrunのreceiptの`subagent_review`は要らないcheckと同じ扱いになり、`failed`でなく理由のあることだけを見る（`not_applicable`と理由で通る）。runが途中でClaudeに切り替わった（ADR-t813-2のフォールバック）後は`actual_provider`がClaudeなので、要るevidenceに戻る。testは`src/domain/receipt.rs`の`a_codex_run_does_not_back_a_required_subagent_review`と`src/application/integrate.rs`の`a_codex_receipt_passes_without_a_subagent_review_the_task_requires`。

### 復旧jobのprompt

`recovery_prompt` はどの worker の記録でも非対話の操作を案内する。answer_known_dialog / close_and_proceed は許す操作から除き、返された場合も check_live が拒む。send_instruction は次の turn の依頼で、画面の代わりに turn の抜粋を読む。stalled は turn_without_receipt / permission_denied、idle_process はプロセスの CPU 観測で判定する。worker に打鍵する操作は無い。

## repositoryの規則を読む順

runtimeはrepositoryの規則（検証のコマンド、宣言するpaths、要るevidence、ADRのような記録の規則）を持たず、promptはsessionをrepositoryの指示へ向けるだけにする。promptと固定の文字列には、dagqのrepositoryの規則（ADRの索引や番号の付け方、dagqのADR番号、Rustのlinterの名前など）を書かない。

- **worker**: `WORKER_READING`と`local_checks`が「AGENTS.mdかCLAUDE.md」を名指す。
- **planner**: `repository_rules(ask)`の一文が、taskの`--verify`・`--paths`・`--evidence`をrepositoryの指示とそれが名指す文書・規則から、AGENTS.md → （無ければ）CLAUDE.md → （どちらも無ければ）README・CIの設定・buildの設定の順で決め、どれでも決まらなければ`ask`する、と指示する。`ask`はruntimeが立てるplanner（`runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`）では`RUNTIME_PLANNER_ASK`で、source・ADR・人の先例から自分で決め、その材料で決めきれず人の判断（`scope`・`discard`）に当たるか確信度が`low`のときだけ`planner_question`のaskにする（ADR-t451-1決定5）。
- **plan review**: repositoryの指示（AGENTS.md・CLAUDE.md）と、それが名指す文書・規則（とくにplan review向けの記述）を読んで当てはめさせ、AGENTS.mdが無いrepositoryではCLAUDE.md → README・CIの設定・buildの設定の順で判断し、どれでも決まらなければ`concern`にさせる。dagqのrepositoryでは、AGENTS.mdの「plan review」の節が`docs/development/task-registration.md`の「plan reviewが当てはめる規則」を名指し、そこが`docs/adr/INDEX.md`（無ければ`sh scripts/adr-index.sh`で作る）とADRのIDの規則（documents.mdの「ADRのID」）を名指す。
- **review**: `review_prompt`の`REVIEW_RULES`の一文が、providerに依らず、worktreeのrootのrepositoryの指示（AGENTS.md・CLAUDE.mdのあるもの）とそれが名指す文書を読み、変更に当たる規則で差分を判定させる。資料の行の後、`REVIEW_DOCS_CHECK`（文書の照合）の前に置く。Claudeのreviewは`--setting-sources ""`で起動して`CLAUDE.md`をmemoryとして読まないので、promptで名指す（[ADR-t1470-1](../../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)決定2、[Review](review.md)の「headless実行」）。`revise`の例は「repositoryのformatter・linter・その他の検査の指摘」で、特定の言語のツールを名指さない。

follow_up・goal gap・findingのdraftの`context`の見出しは英語（`follow-up draft (proposed by the receipt of run <run> of task <id>)`、`goal gap draft (proposed by the judgment of goal <id>)`、`from finding <id> (<kind>)`。[Language](language.md#日本語が残っていた固定の文字列)）。testは`src/application/prompt.rs`の`prompts_take_the_rules_from_the_repository_in_order`と`the_review_prompt_names_the_repositorys_instructions`（reviewの`REVIEW_RULES`の位置と中身）、`tests/it/plan_review.rs`。

## headlessのjobのprompt

[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)（task 1566）。supervisorが起動するheadlessのjobとruntimeのplannerのpromptの共通の方針で、この節がjobごとの今の姿の正本。各jobの文書（[Headless job processes](headless-job-processes.md)、[Plan review](plan-review.md)、[Observer](observer.md)、[Goal review](goal-review.md)、[スループットの見直し](throughput-review.md)、[Review](review.md)、[復旧job](background-recovery-job.md)、[Session prompts](session-prompts.md)）はpromptの大きさと渡し方についてここを指す。

- **渡し方**（決定1）: 大きさに関係なくファイルかstdinで渡し、引数で渡さない。今はどのjob（下の表のruntimeのplannerを除く）もstdinで渡す（task 1560）。`AgentProvider::headless_command`と`review_command`はpromptを`CommandSpec::stdin`に持たせ、spawnerがそれを本人だけが読める一時ファイルに書いてすぐunlinkし、子のstdinにする。argvに`--`もpromptも無く、Claudeは`claude -p`、Codexは`codex exec`が引数の無いpromptをstdinから読む（形と確かめ方は[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)の「promptの渡し方」）。前は引数（`claude -p … -- <prompt>`、`codex exec --json … -- <prompt>`の位置引数）で渡し、引数とenvの合計がhostの`ARG_MAX`（macOSで1MiB）を超えると`Argument list too long (os error 7)`で起動できなかった（2026-10-03のplan review、2026-10-01T13:54Zからのobserver）。今もこの起動の失敗（`E2BIG`）と一時ファイルを用意できない失敗（`StdinUnprepared`）は`job_start_failure`が`other`にし、そのjobの失敗にだけ数えてproviderを控えない。
- **載せるもの**（決定2）: 判断の材料だけ。一覧・全文の大量のデータはIDと要約にし、中身は下の表の「取りに行く経路」で必要なものだけ読ませる。
- **取りに行く経路**（決定3）: jobの権限の意図（`JobAccess`、[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）と許す道具で実際に読める経路だけを読む方法として書く。`queue_cli`のjob（observer・スループットの見直し）はファイルを読めないので、jobのdirの`input.json`をファイルとして読む方法にしない（人が読み、observerの`input.json`はqueue serviceの`observe --input`を通してだけ読む。[Observer](observer.md#promptの入力の上限と選ぶ順)）。`read_files`のjob（runのreview・復旧job）は意図として`dagq`を打てないので（Codexのreviewはsandboxの都合で読むコマンドを打てても、promptはそれを読む方法にしない）、省いてよいのはそのjobが読めるファイル（worktreeとrun directory）にあるものだけ。
- **上限と選ぶ順**（決定4）: 節ごとの件数かbyteの上限と全体の上限を持ち、超えたときに残す順（関連の強さ、新しさ）は決まった規則で決める。
- **省いたことの明示**（決定5）: 節ごとに省いた件数と読む方法をpromptに書く。
- **記録とtest**（決定6）: jobごとにpromptのbyte数をeventに記録し、上限をtestで確かめる。記録するのはplan review（`plan_review_finished`と`plan_review_failed`の`prompt_bytes`。下の「plan reviewの上限」）、observer（`observe_started`の`prompt_bytes`・`prompt_limit`・`prompt_sections`、task 1567）、goal review（`goal_review_finished`と`goal_review_failed`の`prompt_bytes`）、runのreview（`review_started`の`prompt_bytes`）、復旧job（`recovery_prompt_written`の`prompt_bytes`）、runtimeのplanner（queueのevent `planner_prompt_written`の`prompt_bytes`）で、後の4つはtask 1571（下の「goal review・runのreview・復旧job・runtimeのplannerの上限」）。スループットの見直しは`throughput_review_started`と`throughput_review_finished`の`prompt_bytes`・`prompt_limit`・`input_bytes`・`input_limit`・`omitted_to_fit`（observerと同じ平たい欄の名前、task 1572）。

| job | 今の渡し方 | 権限の意図 | 今の節（材料） | 今の上限 | 取りに行く経路 |
| --- | --- | --- | --- | --- | --- |
| plan review（[Plan review](plan-review.md)の4） | stdin（Claude・Codex。task 1560） | `read_files_and_queue_cli`（読むだけの`dagq`とファイルの読み取り） | proposalのtaskの全field、関係するgoal、`lint`、他の`submitted` / `revising`のproposal、`ready` / `in_progress`のtaskの要約と一部の全文、予想するファイル、人が答えたask、衝突の多いファイル、重複と実装済みの候補（2026-10-03のplan review 724で1,148,345 byte。`ready` / `in_progress`のtaskの全文が72%、要約が12%、人が答えたask・hotspot・重複の候補が合わせて8%（人が答えたaskだけでは20,712 byte、約2%）。内訳は下の「plan reviewの上限」） | 全体400,000 byte（`PLAN_REVIEW_PROMPT_LIMIT`）、必須の節200,000 byte、全文20件・100,000 byte、要約64,000 byte、人が答えたask 20件・16,000 byte、hotspot 16,000 byte、重複の候補48,000 byte、他のproposal 48,000 byte（task 1561。値・理由・選ぶ順・必須の節は下の「plan reviewの上限」）。読む件数は要約200件（`QUEUED_TASKS`）、hotspot 15件、重複の候補はtaskごとに5件、要約の行の`expected_files`は10件（`SUMMARY_EXPECTED_FILES`）。byte数は`plan_review_finished` / `plan_review_failed`の`prompt_bytes` | `dagq show`・`proposal show`・`search`・`related`・`findings`・`stats`・`events --full`・`timeline`、上限で省いたものを読む方法として`dagq show ID --full`・`goal show ID --full`・`proposal show ID`・`lint --proposal ID`・`related ID`・`search`・`asks --all`・`stats`・`list --status ready,in_progress --limit 200`（`prompt::PLAN_REVIEW_READS`。task 1561）、repositoryのファイル |
| observer（[Observer](observer.md)） | stdin（`prompt.md`はjobのdirに書き、同じtextをstdinで渡す。task 1560） | `queue_cli`（`Bash(dagq:*)`だけ） | 指示（必須の節）と入力の節: 必須の資料の`kpi.breaches`・`open_asks`・`stats.alerts`・`stats.running_alerts`、省いてよい`stats`・`kpi`（`trend`・`forecast`は要約）・`findings`・`improvements`・`notes`・`graph.critical`・`graph.candidates`（2026-10-03の本番は1,226,750 byteで`stats`が51%・`kpi`が31%・`findings`が8.5%。上限の後の推定は約127KB） | 全体`PROMPT_LIMIT` 160,000 byte。必須の資料は全体の上限だけで切られ、他の節は節ごとの件数とbyteの上限（例: `stats` 40,000 byte・1 key 8,000、`findings` 100件・48,000）。選ぶ順・必須の節・項目の縮め方は[Observer](observer.md#promptの入力の上限と選ぶ順)（task 1567）。byte数は`observe_started`の`prompt_bytes`・`prompt_limit`・`prompt_sections`と`observe --history` | 省いたものはobservationの時点の入力を`observe --input <observation> --section <節>`、今の状態を`stats`・`kpi`・`findings`・`notes`・`asks --open`・`graph`・`candidates`。ほかに`forecast`・`events --full`・`timeline`・`observe --history` |
| goal review（[Goal review](goal-review.md)の4） | stdin（Claude・Codex。task 1560） | `read_files_and_queue_cli` | goal、所属taskのdescription・acceptance、着地したrunのreceipt、goalのfollow_upと所属の判断（`follow_up_memberships`。out_of_scopeは達成の判定に含めず、requiredは含める指示つき。ADR-t1504-2）、goalのevent、前回までのgoal review（本番の最大は218,427 byteで、所属taskが200,200） | 全体`GOAL_REVIEW_PROMPT_LIMIT` 200,000 byte、goal 16,000（必須）、所属task 110,000（1件8,000、着地したものから新しい順）と省いたtaskの要約8,000、follow_up・note 16,000、前回のreview 12,000（1件4,000、新しい順）（task 1571。値と理由は下の「goal review・runのreview・復旧job・runtimeのplannerの上限」）。byte数は`goal_review_finished` / `goal_review_failed`の`prompt_bytes` | `dagq show`・`goal show --full`・`findings`・`events --goal`・`search`、上限で省いたものを読む方法として`dagq show ID --full`・`goal show ID --full`・`events --full --all --goal ID`・`events --full --goal ID --kind goal_review_finished`・`events --full --task ID --kind integration_receipt`（`prompt::GOAL_REVIEW_READS`）、repositoryのファイル |
| スループットの見直し（[スループットの見直し](throughput-review.md#promptの入力task-1099)） | stdin（Claude・Codex。task 1560） | `queue_cli` | 指示・手順（pluginの`reference/kpi.md`の節）と入力の要約 | prompt全体が`PROMPT_LIMIT`（128KiB）、入力が`PROMPT_INPUT_LIMIT`（96KiB）。超えたら`DROP_ORDER`で落として`omitted_to_fit`に名を残し、promptが細部を読むコマンドを示す（task 1099）。byte数は`throughput_review_started`と`throughput_review_finished`の`prompt_bytes`・`prompt_limit`・`input_bytes`・`input_limit`・`omitted_to_fit`（task 1572。[スループットの見直し](throughput-review.md#promptの入力task-1099)） | `kpi`・`stats --full`・`timeline`・`events --full` |
| runのreview（[Review](review.md)） | stdin（Claude・Codex。task 1560） | `read_files`（worktreeとrun directoryのファイルだけ） | taskの記述・context、`review.md`の資料（promptは本番でp50 6,385・p90 10,218・最大18,535 byte） | 全体`RUN_REVIEW_PROMPT_LIMIT` 32,000 byte、taskのtitle 1,000、acceptance 8,000（必須）、必須のsubagentの一覧8,000（task 1571。下の「goal review・runのreview・復旧job・runtimeのplannerの上限」）。byte数は`review_started`の`prompt_bytes` | worktreeとrun directoryのファイル（切ったacceptanceは`review.md`の「Acceptance」、subagentの一覧は`review-subagents-<attempt>.json`） |
| 復旧job（[復旧job](background-recovery-job.md)） | stdin（Claude。task 1560） | `read_files` | taskのdescription・acceptance・verification_commandsと`task_edited`、alertの意味と事実、非対話のsessionの最後のturn（task 1437 からは対話と記録されたrunもturn）、runのプロセスの一覧、worktreeのHEADとreceiptの`commit`と`git status`、そのrunの過去の自動修正とverdict、supervisorの今のbuild識別子とcommit・taskの依存先の着地commitと今のbuildがそれを含むか・runのclaimより後の`update_installed`と`supervisor_handed_off`（task 1633）、終わったrunの資料（[復旧job](background-recovery-job.md)の`recovery_prompt`。本番の最大は66,814 byteで、最後のturnが46,559） | 全体`RECOVERY_PROMPT_LIMIT` 96,000 byte、task（title 1,000・description 6,000・acceptance 4,000・verify 2,000。必須）、alertの事実12,000、画面か最後のturn 16,000、プロセス4,000、`git status` 4,000、過去のverdictと`task_edited` 8,000（新しい順）、終わったrunの資料24,000、build 500・依存先3,000（1件400、含まないものから）・入れ替え10件3,000（1件400、新しい順）（task 1571・1633。下の「goal review・runのreview・復旧job・runtimeのplannerの上限」）。byte数は`recovery_prompt_written`の`prompt_bytes` | run directoryとworktreeのファイル（claim時のtaskは`prompt.txt`、turnの全文は`turns/turn-NNNNNN.jsonl`、receipt・検証のlog・`terminal-final.txt`、build・依存先・入れ替えの全体は`recovery-<alert>-<attempt>.binary.json`）。alertの事実・プロセス・過去のverdictはファイルに無いので、読めないと書く |
| runtimeのplanner（[Session prompts](session-prompts.md)、[非対話のworker](headless-worker.md)） | 非対話のturnの`claude -p … -- <prompt>`の引数（[非対話のworker](headless-worker.md)）。task 1560はjobの`headless_command`と`review_command`だけを替え、plannerとworkerのturnの`turn_command`は替えていない（task 643の範囲） | plannerのrole（読むコマンドと計画のコマンド。[Authorization](../authorization.md)） | `runtime_planner_prompt`（reviseの指摘とtaskの行。本番の最大10,976 byte）・`draft_planner_prompt`（draft・出どころ・goal。最大38,241）・`finding_planner_prompt`（findingと根拠のevent。最大276,417で、根拠が270,669）・`request_planner_prompt`（依頼の参照先とgoal。本番の標本は無い） | 全体はrevise 32,000、draft・finding・依頼80,000 byte。節ごとの上限（reasons・draft・出どころ・goal・根拠のevent・参照先・ask・answer）を持つ（task 1571。下の「goal review・runのreview・復旧job・runtimeのplannerの上限」）。byte数はqueueのevent `planner_prompt_written`（`planner_id`・`prompt`）の`prompt_bytes` | `dagq show`・`proposal show`・`search`・`related`・`findings`・`events --full`、上限で省いたものを読む方法として`prompt::PLANNER_READS`（`dagq show ID --full`・`goal show ID --full`・`proposal show ID`・`findings ID --full`・`requests ID`・`asks --all`・`events --full --task <anchor> --kind plan_review_finished`（最新のreviewの記録先taskを具体的なIDで名指す。reopenも元のreviewのanchor。eventが無ければ読む既知の方法は無いと書く）・`events --full --task ID --kind integration_receipt`・`events --full --run ID --kind integration_receipt`・`events --full --all --after ID --limit 1`）、repositoryのファイル |

### plan reviewの上限

task 1561。仕組み（必須の節の替え方、選ぶ順、省いたことの書き方、`prompt_bytes`の欄）は[Plan review](plan-review.md)の4の「上限と選ぶ順」が持ち、ここは値と理由を持つ。値は`src/application/prompt.rs`の定数。基にしたのは2026-10-03のplan review 724（proposal 559、15 task）の`prompt.txt`（1,148,345 byte）を節の見出しで分けて数えた内訳: `ready` / `in_progress`のtaskの全文821,084 byte（179件、1件平均4,587 byte）、要約132,986 byte（200件、平均665 byte）、人が答えたask 20,712 byte（30件、平均690 byte）、重複の候補62,000 byte（15行、平均4,107 byte）、hotspot 4,815 byte、proposalのtask 60,828 byte、goal・予想するファイル・lint・他のproposal・指示など45,920 byte。

| 定数 | 値 | 理由 |
| --- | --- | --- |
| `PLAN_REVIEW_PROMPT_LIMIT`（全体、言語の指示を含む） | 400,000 byte | macOSの`ARG_MAX`（1,048,576 byte）の4割未満で、引数で渡していたとき（task 1560の前）もenvを足して余裕があった。今はstdinで渡すので、上限はagentの文脈のため。724の必須の節（約107KB）と省いてよい節の上限の和に近く、724は全体の上限に当たらずに約335KBに収まる |
| `PLAN_REVIEW_REQUIRED_LIMIT`（必須の節） | 200,000 byte | 全体の半分。724の必須の節（約107KB）の2倍弱で、普通のproposalでは替えが起きず、超えても省いてよい節に半分が残る |
| `QUEUED_FULL_TASKS` / `QUEUED_FULL_BYTES`（全文） | 20件 / 100,000 byte | 全文が724の72%を占めた。重なりの強い上位20件（平均4.6KBで約92KB）を読めば依存の判断に足り、残りは`dagq show ID --full`で読める。byteの上限は1件の巨大なtaskが節を塞がないため |
| `QUEUED_SUMMARY_BYTES`（要約） | 64,000 byte | 724の要約（約133KB）の半分弱で、平均の行で約96件。重なるものから載せ、残りは`dagq list`で読める |
| `PRECEDENT_ASKS` / `PRECEDENT_BYTES`（人が答えたask） | 20件 / 16,000 byte | 1件の問いと答えはそれぞれ400文字まで（`PRECEDENT_CHARS`）で日本語なら2KBを超えうる。新しい20件で先例の候補に足り、古いものは`dagq asks --all`で読める |
| `HOTSPOT_BYTES`（衝突の多いファイル） | 16,000 byte | 15件（`HOTSPOT_FILES`）で約5KB。依存の判断の要なので最初に残し、上限は`queued_tasks`の並びが長いときだけに当たる |
| `CANDIDATE_BYTES`（重複と実装済みの候補） | 48,000 byte | 724で約62KB（taskごと約4KB）。15 taskのうち11 task分に当たり、残りは`dagq related` / `dagq search`で引ける |
| `OTHER_PROPOSAL_BYTES`（他のproposal） | 48,000 byte | 他のproposalのtaskのdescription・acceptanceを丸ごと載せるので、待つproposalが多いと伸びる。前に出されたものとの食い違いの検査に要るので重複の候補の次に残し、残りは`dagq proposal show ID`で読める |
| `OMISSION_NOTE_BYTES`（節ごとの注記の空き） | 2,000 byte | 注記は省いた件数・読むコマンド・IDの並び（40件まで）で収まる |

省いてよい節は、全体の上限の残りを衝突の多いファイル → 重複の候補 → 他のproposal → 人が答えたask → 全文 → 要約の順に取る（依存と重複の判断に直接効くものを先に、CLIの一覧で代えやすい要約を最後にする）。必須の節（指示と検査とverdictの形、proposalのtaskの全文、予想するファイル、goal、`lint`、最後の言語の指示）は省かず、必須の節だけで200,000 byteを超えるときだけ大きいものから読むコマンドの案内に替える（jobの今の権限で打てる読むだけの`dagq`。jobのdirのファイルには退避しない）。

### goal review・runのreview・復旧job・runtimeのplannerの上限

task 1571。plan review（task 1561）とobserver（task 1567）と同じ形で、goal review・runのreview・復旧jobと、runtimeのplannerの4つのprompt（`runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`・`request_planner_prompt`）に、節ごとの件数かbyteの上限と全体の上限を持たせた。値は`src/application/prompt.rs`の定数、仕組みは`src/application/prompt_fit.rs`。

- **選ぶ順**: 一覧の節は決まった順で取り、入らない項目は飛ばして次を試す（`prompt_fit::pick`。1件の巨大な項目が残りを隠さない）。残したものはもとの順で載せる。goal reviewのtaskは着地したもの（`landed`のあるもの）を新しい順、続けて残りを新しい順。note・前回のreview・follow_up・復旧jobの過去のverdictと固定バイナリの入れ替え・findingの根拠のevent・ask・goalのtaskの行は新しい順。復旧jobの依存先は今のbuildが含まないか言えないものをIDの昇順、続けて含むもの（task 1633）。draftの束・依頼の参照先・reviseの指摘（reasons）はもとの順（束の古い順、inboxが並べた順、plan reviewが書いた順）。
- **切り方**: 長い文は先頭を残して切り（画面の末尾は末尾を残す）、`[… N bytes left out by the prompt's limit; <読む方法>]`を付ける（`prompt_fit::cut`）。JSONの項目は長い文字列から切り、`cut`に省いたbyte数と読む方法を持たせ、それでも入らなければID・kind・titleと`left_out_bytes`・`read_with`だけの行にする（`prompt_fit::shrink`）。
- **省いたことの明示**（決定5）: 節ごとに、省いた件数・ID（40件まで）・読む方法の注記（`(N tasks left out by this section's limit: … To read them: ….)`）を書く。goal reviewの省いたtaskはID・title・status・着地したかの要約の行に替える（決定2）。
- **読む方法**（決定3）: 依頼の読めなかった参照（`RequestRefMaterial::Unreadable`）は、切った注記と省いた参照の一覧に、読めなかったため読む方法が無いと書く。goal reviewとruntimeのplannerは読むだけの`dagq`（`prompt::GOAL_REVIEW_READS`・`prompt::PLANNER_READS`。どれもそのroleで打てることを`src/main.rs`の`the_goal_review_job_and_the_planners_may_run_each_read_their_prompts_name`が確かめ、goal review jobのものはqueue serviceのユースケースがあることも確かめる）。runのreviewと復旧jobはworktreeとrun directoryのファイルだけ（`review.md`、`review-subagents-<attempt>.json`、`prompt.txt`、`turns/turn-NNNNNN.jsonl`、receipt・検証のlog・`terminal-final.txt`、復旧jobの固定バイナリの節（build・依存先・入れ替え）の全体の`recovery-<alert>-<attempt>.binary.json`（task 1633））で、ファイルに無いもの（alertの事実、プロセスの一覧、過去のverdict）は`it is in no file you can read`と書き、読めない場所に退避しない。
- **必須の節**: goal reviewのgoal、runのreviewのacceptance、復旧jobのtaskのdescription・acceptance・verification_commands、draftのdescription、findingの見立ては省かず、自分の上限で切って`over_limit`に何をどれだけ切ったかを書く。どの節の上限も足して全体に収まるように決めてあり、それでも全体（言語の指示の分`LANGUAGE_ROOM` 1,000 byteを除く）を超えるときだけ、promptの先頭と末尾8,000 byte（指示とverdictのschema）を残して中ほどを切り、`over_limit`に書く（`Fit::finish`）。復旧jobの固定バイナリの節（build・依存先・入れ替え。task 1633）はretryの判断の材料なので、alertの事実と同じく残る側に置く: alertの事実の直後（promptの先頭から節の上限の和で約31,500 byteまで）に置くので、中ほどを切っても先頭の側に残る。入れ替えは節の中で新しい順に残す。
- **記録**（決定6）: どのjobも`PromptBytes`（`total`・`limit`・`sections`（節ごとのbyte。指示は`instructions`、言語の指示は`language`）・`omitted`（節ごとの省いた・切った件数）・`over_limit`）を`prompt_bytes`としてeventに記録する（plan reviewと同じ形で、observerの`prompt_bytes`・`prompt_limit`・`prompt_sections`と同じ中身を1つの欄に持つ）。`Fit::lines`で選ぶ一覧では1項目を1回だけ数え、残した項目の切りだけを切りとして数え、省いた項目は切っていても省いた件数だけに数える。draft plannerの`origin`は節全体を1項目とし、内側の欄の切りと節全体の切りが重なっても、どれかを切ったときに1回だけ数える。この1回の数え方は`Fit::lines`と`origin`に限る。draft plannerの`drafts`、`planner_asks`、`planner_goals`では、内側の欄の切りと`pick`後の省略を別々に加算し、同じ項目を複数回数える場合がある。

| job | event | 欄 |
| --- | --- | --- |
| goal review | `goal_review_finished`・`goal_review_failed`（goalのevent） | `prompt_bytes`（起動の前に失敗してpromptが無いときは`null`） |
| runのreview | `review_started`（runのevent） | `prompt_bytes`（資料かpromptを書けなかったときは無い） |
| 復旧job | `recovery_prompt_written`（runのevent。生きているsessionのjobも終わったrunのjobも`start_job`がpromptを書いた直後） | `alert`・`attempt`・`prompt_bytes` |
| runtimeのplanner | `planner_prompt_written`（queueのevent。`launch_planner`がbackgroundのwrapperを起動する前） | `planner_id`・`subject: "planner"`・`prompt`（`runtime` / `draft` / `finding` / `request`）・`prompt_bytes` |

値と理由（本番の大きさは2026-10-03T10:50Zにplannerが本番のqueue dirで`wc -c`と節の見出しで測った表。goal reviewは全27件、runのreviewは新しい200件、復旧jobは全146件、plannerは全844件）:

| 定数 | 値 | 理由 |
| --- | --- | --- |
| `GOAL_REVIEW_PROMPT_LIMIT` | 200,000 byte | 本番の最大はjob 24（goal 58）の218,427で、所属taskが200,200（50件、1件平均4,000・最大12,126。うち着地したreceipt 124,432）。次のjob 25（146,341）は収まる。節の上限の和（178,000）と指示（約3,000）と注記・言語の指示が収まる値 |
| `GOAL_REVIEW_TASKS_BYTES` / `GOAL_REVIEW_TASK_BYTES` | 110,000 / 8,000 byte | 所属taskの節（job 24で200,200）を約半分にし、平均4,000のtaskなら着地したものから約27件を全文で残す。1件の上限は最大12,126のtaskを切り、1件が節を塞がないため。残りは要約の行（`GOAL_REVIEW_STUB_BYTES` 8,000）と`dagq show ID --full` |
| `GOAL_REVIEW_GOAL_BYTES` | 16,000 byte | goalは本番で最大5,241。必須なのでその3倍 |
| `GOAL_REVIEW_FOLLOW_UPS_BYTES` / `GOAL_REVIEW_EVENTS_BYTES` / `GOAL_REVIEW_PREVIOUS_BYTES` / `GOAL_REVIEW_ITEM_BYTES` | 16,000 / 16,000 / 12,000 / 4,000 byte | noteは本番で最大10,877で、16,000で収まる。follow_upは同じ大きさ、前回のreviewは新しいものだけで足りるので少なめ。1件の上限は1件が節を塞がないため |
| `RUN_REVIEW_PROMPT_LIMIT` | 32,000 byte | 本番はp50 6,385・p90 10,218・最大18,535（run 3397cb54のreview 8で、acceptance 1,544、指示と資料が約15,000）。節の上限の和（17,000）と指示（約5,500）で約23,000、最大の約1.7倍 |
| `RUN_REVIEW_ACCEPTANCE_BYTES` / `RUN_REVIEW_TITLE_BYTES` / `RUN_REVIEW_SUBAGENTS_BYTES` | 8,000 / 1,000 / 8,000 byte | acceptanceは本番で最大1,544の5倍。全文は`review.md`にある。subagentの一覧はagentごとの1行で、全部は`review-subagents-<attempt>.json`。一覧の後の実行と報告の指示（`review::SUBAGENTS_INSTRUCTION`）は切らず、上限はそれを含む |
| `RECOVERY_PROMPT_LIMIT` | 96,000 byte | 本番はp50 19,773・p90 27,553・最大66,814（run ae4bbfa9のrecovery-idle_process-2で、非対話のsessionの最後のturn 46,559、alertの事実6,806、過去のverdictと`task_edited` 6,748、description 2,166）。節の上限の和（81,000、task 1633の固定バイナリの節を足して87,500）と指示（約4,000）と注記・言語の指示 |
| `RECOVERY_SCREEN_BYTES` | 16,000 byte | 最後のturnの46,559を約3分の1にする。新しいturnが先なので先頭を残し、全文は`turns/turn-NNNNNN.jsonl` |
| `RECOVERY_FACTS_BYTES` / `RECOVERY_HISTORY_BYTES` / `RECOVERY_HISTORY_ITEM_BYTES` | 12,000 / 8,000 / 2,000 byte | 本番の最大（事実6,806、過去のverdictと`task_edited` 6,748）が収まる。ファイルに無いので省いたものは読めないと書く |
| `RECOVERY_DESCRIPTION_BYTES` / `RECOVERY_ACCEPTANCE_BYTES` / `RECOVERY_VERIFY_BYTES` / `RECOVERY_TITLE_BYTES` | 6,000 / 4,000 / 2,000 / 1,000 byte | descriptionは本番で最大2,166の約3倍。claim時の全文はrun directoryの`prompt.txt` |
| `RECOVERY_BINARY_BYTES` / `RECOVERY_DEPENDENCIES_BYTES` / `RECOVERY_DEPENDENCY_BYTES` / `RECOVERY_REPLACEMENTS` / `RECOVERY_REPLACEMENTS_BYTES` / `RECOVERY_REPLACEMENT_BYTES` | 500 / 3,000 / 400 byte / 10件 / 3,000 / 400 byte | task 1633。buildは約100 byteの1行。依存先は1行約100 byte（commitの全文）で約30件、言えない理由のついた行も1件400に収める。入れ替えは1行約250 byte（build識別子2つとcommit）で、claimから復旧jobまでの数時間の入れ替えは数件なので新しい10件。節の上限の和は87,500になるが、unit testの最も大きな入力でも全体に収まるので`RECOVERY_PROMPT_LIMIT`は変えない。省いたものの全体は`recovery-<alert>-<attempt>.binary.json` |
| `RECOVERY_PROCESSES_BYTES` / `RECOVERY_STATUS_BYTES` / `RECOVERY_ENDED_BYTES` | 4,000 / 4,000 / 24,000 byte | プロセスは1行に`command`の末尾300文字で約10件、`git status`は数十行。終わったrunの資料は各log・receipt・画面の末尾3,000（`TRIAGE_TAIL_BYTES`）がlog 8件まで並ぶので、その大半 |
| `RUNTIME_PLANNER_PROMPT_LIMIT` / `RUNTIME_PLANNER_REASONS_BYTES` / `RUNTIME_PLANNER_REASON_BYTES` / `RUNTIME_PLANNER_TASKS_BYTES` | 32,000 / 12,000 / 4,000 / 8,000 byte | reviseのplannerは本番でp50 2,849・p90 5,084・最大10,976（指摘とtaskの行）。最大の約3倍 |
| `DRAFT_PLANNER_PROMPT_LIMIT` | 80,000 byte | 本番はp50 18,021・p90 26,352・最大38,241（planner 828で、元のtaskのreceiptの`summary` 8,423、goal 7,398、goalの他のtask 4,303）。節の上限の和（約62,000、task 1540の再検討の節を足して約70,000）と指示（約6,000）で約76,000 |
| `DRAFT_RECEIPT_SUMMARY_BYTES` / `DRAFT_RECEIPT_FOLLOW_UPS_BYTES` / `DRAFT_ORIGIN_BYTES` / `DRAFT_SOURCE_TEXT_BYTES` | 12,000 / 6,000 / 20,000 / 3,000 byte | receiptの`summary`は本番で最大8,423で、12,000で収まる。出どころの節は元のtaskとreceiptと理由を合わせて20,000 |
| `DRAFT_MEMBERS_BYTES` / `DRAFT_DESCRIPTION_BYTES` / `DRAFT_CONTEXT_BYTES` / `DRAFT_TITLE_BYTES` | 20,000 / 6,000 / 4,000 / 1,000 byte | draftはworkerのfollow_upの1件で、descriptionは数KB。束が大きいと伸びるので、入らないdraftは`dagq show ID --full` |
| `DRAFT_REVISIT_BYTES` / `DRAFT_REVISIT_ITEM_BYTES` | 8,000 / 2,000 byte | task 1540（[ADR-t1540-1](../../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）。再検討の時刻が来たdraftの`## Revisit of draft <id>`の節（前回の`planner_question`とnote、新しい順）。時刻付きで残したdraftの前回の判断は問い1〜2件とnote 1〜2件（draft 1537は問い1件・note 1件）で、1件の上限で問いかnote 4件は入る。全体の上限の中に収めるため、他の節の和（約62,000）と指示（約6,000）に足して約76,000にした。入らないものは`dagq show ID --full` |
| `PLANNER_GOALS_BYTES` / `PLANNER_GOAL_TEXT_BYTES` / `PLANNER_GOAL_TASKS_BYTES` | 16,000 / 4,000 / 4,000 byte | goalは本番で最大7,398（draft）、goalの他のtaskは4,303。goalの欄（description・acceptanceは4,000、constraintsは2,000）とtaskの行で1件約14,000 |
| `PLANNER_ASKS_BYTES` / `PLANNER_ASK_TEXT_BYTES` / `PLANNER_ANSWER_BYTES` | 8,000 / 1,000 / 3,000 byte | findingと依頼の前の質問は新しいものから。answerを運ぶplannerの元の質問と答えはそれぞれ3,000 |
| `FINDING_PLANNER_PROMPT_LIMIT` / `FINDING_EVIDENCE_BYTES` / `FINDING_EVENT_BYTES` | 80,000 / 28,000 / 8,000 byte | 本番はp50 37,974・p90 156,358・最大276,417（planner 768、finding 44で、根拠が270,669）。根拠の節を28,000（全体のp50 37,974より小さい）にし、新しいeventから1件8,000まで。節の上限の和は約72,000。全部は`dagq findings ID --full` |
| `FINDING_DETAIL_BYTES` / `FINDING_SHORT_BYTES` | 8,000 / 2,000 byte | 見立ては必須。summary・subject・印の理由は短い |
| `REQUEST_PLANNER_PROMPT_LIMIT` / `REQUEST_REFS_BYTES` / `REQUEST_REF_BYTES` / `REQUEST_NOTE_BYTES` | 80,000 / 32,000 / 8,000 / 8,000 byte | 本番の標本は無い。他のplannerの表の値（draftのp90 26,352・最大38,241）に揃えて全体をdraft・findingと同じにし、参照先は1件8,000で4件以上を残す。unit testの最も大きな入力（参照先40件・各20KB、note 50KB、goal 10件、ask 100件）で66,371 byte（task 1681のUnreadableのfixtureの変更前） |

unit testの大きな入力でのbyte数（言語の指示を除く。reviseと依頼の値はtask 1681のanchorの案内・Unreadableのfixtureの変更前の測定）: goal review 175,504（所属task 200件・各約14KB、goal 50KB）、runのreview 22,471（acceptance 100KB、subagent 50KB）、復旧job 66,975（非対話のsession。turn 200KB、事実100KB、依存先500件、入れ替え500件）と90,967（終わったrun。資料100KB）、revise 18,368（task 500件、reasons 100件・各5KB）、draft 57,841（draft 50件・各20KB、receipt 50KB、goal 10件）、finding 71,304（根拠のevent 500件・各5KB）、依頼66,371。testは`src/application/prompt.rs`の`a_goal_review_prompt_of_many_large_tasks_stays_within_its_limits`・`a_run_review_prompt_of_a_huge_acceptance_stays_within_its_limits`・`a_recovery_prompt_of_huge_turns_and_facts_stays_within_its_limits`（task 1633の依存先500件・入れ替え500件の節の上限・省いた件数・`binary.json`を読む方法を含む）・`a_runtime_planner_prompt_of_many_reasons_stays_within_its_limits`・`a_draft_planner_prompt_of_a_large_bundle_stays_within_its_limits`・`a_finding_planner_prompt_of_huge_evidence_stays_within_its_limits`・`a_request_planner_prompt_of_huge_references_stays_within_its_limits`（全体の上限・省いた件数・読む方法・必須の節の`over_limit`・言語の指示を足した`prompt_bytes`）、`src/application/prompt_fit.rs`のunit test（切り方・中ほどの切り方）。eventは`tests/it/goal_review.rs`の`an_achieved_goal_is_closed_with_its_evidence`と`a_failed_goal_review_waits_for_a_person_until_rearmed`、`tests/it/runtime_review.rs`の`a_passing_review_exits_the_live_session_and_lands_it`、`tests/it/runtime_repair.rs`の`a_process_without_cpu_progress_is_an_idle_process_alert_for_the_recovery_job`（生きているsession）と`tests/it/runtime_triage.rs`の`a_failed_run_without_commits_is_retried_by_its_recovery_job_and_lands`（終わったrun）と`the_recovery_job_reads_the_supervisors_build_and_the_dependencies_it_holds`（固定バイナリの節`binary`・`dependencies`・`replacements`が`prompt_bytes`の`sections`に載り、`recovery-failed-1.binary.json`が書かれること。task 1633）、`tests/it/lifecycle_plan.rs`の`the_runtime_opens_a_planner_for_a_proposal_with_its_reasons`・`tests/it/draft_bundles.rs`の`one_runs_follow_ups_are_one_bundle_for_one_planner`・`tests/it/finding_planner.rs`の`a_marked_finding_gets_one_planner_whose_proposal_plan_review_readies`・`tests/it/request_planner.rs`の`a_request_the_inbox_records_gets_one_planner_whose_submission_proposes_it`（`prompt_bytes`の`total`が渡したpromptのbyte数で、節の和と同じこと）。

workerの`prompt.txt`（この文書の上の節）はADR-t1566-1の範囲に含めない。
