---
id: design-supervisor-lifecycle-prompt
type: design
title: "Prompt"
status: current
created: 2026-09-26
updated: 2026-10-03 # task 1420: the acceptance map before the receipt
last_verified: 2026-10-03 # task 1420
scope: runtime
related:
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

resumeの解消依頼（`resume_request`。全ての`ResumeKind`）とreviseの依頼（`revise_request`）は、receiptを書き直す手順5の末尾に`ACCEPTANCE_REMAP`（英語で237文字）を足す: 直した項目の対応を改めて満たすものへ対応づけて`summary`の句を書き直し、taskの中で満たせない項目は`worker_question`（`--because scope`）かfailedのreceiptにしてfollow_upにしない。

Codexのworkerの`review_line`（下の[subagent review](#subagent-review)）は自分のdiffを読む見直しをこの対応づけの手順に寄せ、受け入れ条件との照合を2度言わない。runのreviewのprompt（`review_prompt`）と判定の基準は変えない。testは`src/application/prompt.rs`の`every_worker_text_that_writes_a_receipt_maps_the_acceptance_once`。

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
- **planner**: `repository_rules(ask)`の一文が、taskの`--verify`・`--paths`・`--evidence`をrepositoryの指示とそれが名指す文書・規則から、AGENTS.md → （無ければ）CLAUDE.md → （どちらも無ければ）README・CIの設定・buildの設定の順で決め、どれでも決まらなければ`ask`する、と指示する。`ask`は人が開くplanner（`planner_prompt`）では`ask the person`、runtimeが立てるplanner（`runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`）では`RUNTIME_PLANNER_ASK`で、source・ADR・人の先例から自分で決め、その材料で決めきれず人の判断（`scope`・`discard`）に当たるか確信度が`low`のときだけ`planner_question`のaskにする（ADR-t451-1決定5、task 1320）。
- **plan review**: repositoryの指示（AGENTS.md・CLAUDE.md）と、それが名指す文書・規則（とくにplan review向けの記述）を読んで当てはめさせ、AGENTS.mdが無いrepositoryではCLAUDE.md → README・CIの設定・buildの設定の順で判断し、どれでも決まらなければ`concern`にさせる。dagqのrepositoryでは、AGENTS.mdの「plan review」の節が`docs/adr/README.md`とADRのIDの規則を名指す。
- **review**: `revise`の例は「repositoryのformatter・linter・その他の検査の指摘」で、特定の言語のツールを名指さない。

follow_up・goal gap・findingのdraftの`context`の見出しは英語（`follow-up draft (proposed by the receipt of run <run> of task <id>)`、`goal gap draft (proposed by the judgment of goal <id>)`、`from finding <id> (<kind>)`。[Language](language.md#日本語が残っていた固定の文字列)）。testは`src/application/prompt.rs`の`prompts_take_the_rules_from_the_repository_in_order`と`tests/it/plan_review.rs`。
