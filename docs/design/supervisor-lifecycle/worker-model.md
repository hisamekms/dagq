---
id: design-supervisor-lifecycle-worker-model
type: design
title: "Worker model"
status: current
created: 2026-09-27
updated: 2026-10-04 # task 1437
last_verified: 2026-10-04 # task 1437
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-plan-review
  - adr-0079
  - design-supervisor-lifecycle-actor-model
---

# Worker model

workerのsessionのmodelとeffortの選び方と記録（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定3・4・5、task 576・578）。予測の記録は[plan review](plan-review.md)の6、読み方は[stats](stats.md#workerのmodelと試しの群)と[kpi](kpi.md#kpiと層)の`--by group`、段上げの件数は[stats](stats.md#workerのsessionの段上げ)の`escalations`。worker以外のアクター（決定7の(b)(c)）は[Actor model](actor-model.md)。

## 既定: Opus 5.5・effort mediumを明示して渡す

- runtimeはworkerの起動・resumeのたびに、modelとeffortをproviderに明示して渡す（`AgentProvider::select_model`。Claude Codeでは`--model <model> --effort <effort>`をoptionの末尾、`--`の前に足す）。Claude Codeの既定やユーザー設定に依らない。
- 既定は`claude-opus-5-5`・`medium`（`domain::worker_model::WorkerSession::default`）で、試しが無効なら全runがこれで起動する（今までの挙動と同じ値）。
- 決めるのはclaimとresumeとreviseで、runのsession wrapper（`application::session`）は起動の直前にそのrunで最後にsessionを開いたevent（`run_claimed` / `resume_started` / `revise_requested`のうち`model` / `effort`を持つ最新のもの）の値を読んで渡す（`WorkerSession::current`）。resumeは`resume_started`を記録してからsessionを開くので、その値（下の段上げで上がったものを含む）で開き直す。記録の無いrun（task 576より前にclaimされたもの）は既定で起動する。
- headlessのworker（`application::headless_session`）はturnごとに別のprocessなので、turnを起動するたびに同じ`WorkerSession::current`を読み直す。reviseで上がった段は次のturnから効く。
- interactiveのreviseは生きているsessionへの差し戻しなので起動の引数は無い。段上げで値が変わるときは、差し戻しの前にsessionの中で切り替える（下の段上げ）。

## 記録

sessionを開くeventのpayloadに`model`・`effort`・`group`（`control` / `treatment`、試しの外ならnull）を持たせる（`WorkerSession::fields`）。

| event | いつ |
|---|---|
| `run_claimed` | claim（retryで新しいrunをclaimしたときも）。試しの対象の判定で百分位を出したtaskは`trial_percentile`も持つ |
| `resume_started` | `needs_session`のresume。値は直前のsessionのもので、taskに由来する失敗の後は1段上げたもの |
| `revise_requested` | reviewの`revise`の差し戻し。値は直前のsessionを1段上げたもの（切り替えられなければ直前のまま） |

### Codexのrun（task 892）

Codexのworkerは段のClaudeのmodelを使わず（`-m`を渡さずCodexの既定のmodelで動く。[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）、effortだけを使う。そこでCodexで動くrun（`actual_provider`が`codex`。claimで切り替わったrunも、途中でCodexへ移ったrunのその後のresume・reviseも）のsessionを開くeventは、`WorkerSession::fields_on(Provider::Codex)`で次のように書く。

- `model`はnull、段のmodelは`ladder_model`、`group`はnull、`model_unknown`に理由（`worker_model::CODEX_MODEL_UNKNOWN`: Codexは自分のmodelで動き、各turnの`turn_finished`が使ったmodelを記録する）。`effort`は段のeffort
- 段上げの`escalated_from`も`{model: null, ladder_model, effort}`、切り替えられなかったreviseの`escalation_skipped`も同じ形（`WorkerSession::named_on`）
- 読み戻し（`WorkerSession::current` / `of_run`）は`model`が無ければ`ladder_model`を段として読むので、段上げとtaskへの引き継ぎはClaudeのrunと同じに働く（効くのはeffortだけ）
- Codexが実際に使ったmodelは、turnごとに`turn_finished`の`model`（読めなければnullで、理由を`model_unknown`）に残る（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。`stats`・`kpi`のrunの`model`はCodexで始まったrunではこの最初のturnのmodel（[stats](stats.md)）
- Codexで動くclaimは試しに入らない: `choose`を呼ばず、群も百分位も持たず、交互の順番も取らない（群を持ったtaskでもそのrunは群null）。Claudeでclaimして群を持ったrunが途中でCodexへ移ったとき（ADR-t813-2のフォールバック）は、claimの群は記録に残るが、`stats`・`kpi`はCodexで作業したrunとして試しの集計（`trial_groups`、`group=`）とClaudeの層から外す（[stats](stats.md)）。taskの群は変わらないので、そのtaskの後のClaudeのrunは同じ群で起動する

段上げしたeventは`escalated_from`（前の`{model, effort}`）と`escalation_reason`（`verification_failed` / `sent_back` / `revise`）も持つ（`WorkerSession::fields_raised`）。切り替えられなかった`revise_requested`は`escalated_from`を持たず、`escalation_skipped`（`{model, effort, reason, why}`: 上げるはずだった値、`revise`、切り替えられなかった理由）を持つ。retryのclaimで段を引き継いだ`run_claimed`は`escalation_inherited: true`を持つ。

## 段上げ（決定5）

試しの有無に関係なく全taskに効き、設定は持たない。

- **段**: `LADDER`の **Sonnet 5 medium → Opus 5.5 medium → Opus 5.5 high → Opus 5.5 xhigh**。xhighが上限で、そこでは上げない（`WorkerSession::raised`がnone）。段の外の値（`dagq.toml`で決めたものではなく記録の読めない値など）も上げない。群（`group`）は変えない。
- **上げる**: taskに由来する失敗（ADR-0079の決定1）の後に開くsession。
  - resume: runを`needs_session`にした最新のevent（`domain::resume::parks`: `integration_deferred`・`integration_error`・`evidence_missing`・`scope_violation`・`landing_decided`・`triage_finished`・`triage_decided`、landing recheckの`landing_recheck_failed`）のcodeが`verification_failed`（integrateの検証か、landing recheckの検証の失敗）か`sent_back`（reviewの`concern`の`approve_landing`に人が`send_back`と答えた）なら上げる（`worker_model::raises`、`worker_model::for_resume`）。そのeventが最後にsessionを開いたeventより後にあるときだけ上げるので、1つの失敗で2回上げない（sessionを開かずに終わったresumeの後の次のresumeは同じ段）。判定はresumeを始めるtransaction（`begin_resume`）の中で行い、`resume_started`に書く。
  - revise: reviewの`revise`の差し戻しは必ず上げる（理由`revise`）。
- **上げない**: rebaseの衝突（`rebase_conflict`。integrateのrebase・merge-treeの事前検査・landing recheck）、外からのkill（killされたsessionのrunはfailedになり、復旧jobの`resume`が`triage_finished`（code `triage_resume`）で送り返す）、`evidence_missing`・`scope_violation`・`migration_number_taken`など、上の2つ以外のcodeのresume。conflict_precheckの差し戻し（生きているsessionへの衝突の解消の依頼）も上げない。
- **生きているsessionの切り替え**: task 1437より前のinteractiveのreviseでは、`revise_requested`を記録する前に、agentの切り替えの入力（`AgentSignals::model_switch`。Claude Codeは、modelが変わるときだけ`/model <model>`、続けて`/effort <effort>`）を1つずつ送り（`submit`、what `model switch`）、どれも入力欄を抜けたら上げた値で記録して差し戻す。何もsessionに届いていないとき（agentが切り替えを持たない、最初の入力の打鍵が失敗した。打鍵の失敗は入力欄に何も残さない）は、上げずに直前の値で差し戻し、`escalation_skipped`に理由を書く（ADR-0079の決定3）。入力が入力欄に残った・ダイアログが出た・2つ目の入力が失敗した（`/model`だけ効いた）ときは、差し戻しをその上に打つと入力欄の残りやダイアログに入り、sessionのmodel / effortも記録と食い違うので、差し戻しを打たず`revise_requested`も記録せずに、送れなかった差し戻しと同じく`approve_landing`のaskにする（`Unswitched::Unsettled`）。headlessのreviseは入力を送らず、次のturnが上げた値で起動する。今はworkerのrunがすべて非対話なので（task 1437）、headlessのreviseだけが当たる。
- **取り下げた差し戻し**: `revise_requested`の後に同じattemptの`revise_unsent`がある（差し戻しを打てなかった）ものは、sessionを開いたeventに数えない（`worker_model`の`openings`）。その段上げは直前の値にも、taskへの引き継ぎにも、`stats`の`revise_escalations`にも入らず、その後の人の`send_back`によるresumeは実際に走っていたsessionから1段上げる。
- **taskに引き継ぐ**: 上げた段はそのtaskの以後のrun（retry、引き継ぐretryを含む）に引き継ぐ。claimは試しの選んだsessionと、そのtaskの前のrunで最後にsessionを開いたeventの値を比べ、taskのどれかのeventが`escalated_from`を持ち（一度でも上げた）、前の値の段が高いときは前の値を使う（群は試しの選んだもの。`WorkerSession::inheriting`、`infrastructure::sqlite::claim_task`）。一度も上げていないtaskは今までどおり。
- **層**: `kpi`の`--by model` / `effort`はrunの最初の`run_claimed`の値で層別するので、resume・reviseで上げた段は層に出ず、retryで引き継いだ段は出る。

## 限定の試し（`[worker.trial]`）

`dagq.toml`の`[worker.trial]`で有効にしたときだけ働く。既定は無効で、有効にするのは人の判断（書式は[Run environment](run-environment.md)）。この repositoryの`dagq.toml`には置いていない。

```toml
[worker.trial]
enabled = true   # 既定 false
window = 60      # 既定 60（domain::prediction::PREDICTION_WINDOW）
```

- **読む時点**: supervisorがclaimのpassごとに読む（`Verifier::worker_trial`）ので、有効・無効の切り替えは再起動なしに次のclaimから効く。読めないときはwarnをlogに出して試しの外でclaimする（壊れた`dagq.toml`はprovisioningがerrorにする）。
- **選ぶ場所**: claimのtransactionの中（`infrastructure::sqlite::claim_task`）で、queueの`task_weight_predicted`と`run_claimed`を読み、純粋関数`domain::worker_model::choose`が決める。交互の順番をtransactionの中で決めるので、並行するclaimが同じ番を取らない。
- **群の固定**: 前のrunの`run_claimed`で群を持ったtaskは、同じ群のmodel / effortで起動する（resume・revise・retryで変えない。予測が後で変わっても変えない）。試しを無効に戻すと、群を持ったtaskも既定で起動する。
- **対象**: 群を持たないtaskで、次の全部を満たすもの。
  - そのtaskの最後の予測の`nature`が`mechanical`
  - そのtask自身を除く、他のtaskの最後の予測を新しい順に`window`件並べた中で、`expected_output_tokens`の百分位（`domain::prediction::percentile`。同じ値は半分に数え、小数1桁）が33.3以下（`LOWER_THIRD`）
  - 他のtaskの予測が`window`件ある（足りない間は誰も対象にしない）
- **割り当て**: 対象になったtaskは、最後に群を持ったtask（各taskの最初の群付きの`run_claimed`の順で最後）と反対の群になり、最初の1件は`control`。`control`は`claude-opus-5-5`・`medium`、`treatment`は`claude-sonnet-5`・`medium`（`WorkerSession::of_group`）。
- **対象外**: 予測の無いtask（`ready --bypass-review`・予測の失敗）、`mechanical`でないtask、下位3分の1より上のtaskは既定のまま（群はnull）。`rework_probability`は選択に使わない。

## 判定

群ごとの比較は`stats`の`trial_groups`（[stats](stats.md#workerのmodelと試しの群)）と、`kpi --by group`（`model` / `effort` / `nature`も。runの最初の`run_claimed`の値で層別する。[kpi](kpi.md#kpiと層)）のrunの層で読む。判定（1群45件前後、taskに由来する手戻りの率の差（treatment − control）が+5ポイント以内か、途中で止める目安）は人とplannerが行い、runtimeは自動で止めたり全面適用したりしない（ADR-0079の決定6）。止めるのは`enabled = false`に戻すこと。

## テスト

- `domain::worker_model`の単体テスト（既定、`of_run`、交互の割り当て、群の固定、対象外）
- `infrastructure::run_env`の`parses_the_worker_trial_table`
- `tests/it/worker_model.rs`: 既定の起動と記録、`[worker.trial]`を有効にした`supervise`での交互の割り当てとSonnetでの起動、claimのtransactionでの選択、`stats`の`trial_groups`、Codexのrunが試しの外で`model`をnullにすること（`a_codex_run_is_outside_the_trial`）
- Codexのrun: `domain::worker_model`の`a_codex_session_names_no_claude_model_and_keeps_its_step`、`tests/it/runtime_codex.rs`（claimと差し戻しの`model`がnullで、turnがrolloutのmodelを記録する）
- `tests/it/runtime_session.rs`の`claude_stop_hook_settings_publish_the_idle_marker`（`--model` / `--effort`の位置）、`runtime_triage.rs`と`runtime_review.rs`（`resume_started`と`revise_requested`の記録、resumeの起動の値）
- 段上げ: `domain::worker_model`の単体テスト（段、`current`、`for_resume`、`inheriting`、取り下げた差し戻し）、`infrastructure::claude`の`a_live_session_switches_with_model_and_effort_commands`、`tests/it/worker_escalation.rs`（検証の失敗と`sent_back`で上がり、衝突・kill・`evidence_missing`・`scope_violation`で上がらない、上げたresumeの起動の値と`stats`の`escalations`、retryの引き継ぎ、切り替えられなかったreviseが上げずに進むこと（`a_revise_whose_session_cannot_be_switched_goes_on_unraised`）と、入力が残ったreviseが差し戻しを打たずにaskになること（`a_switch_left_in_the_input_box_asks_a_person_without_typing_the_revise`））、`runtime_review.rs`の`a_revise_verdict_is_fixed_by_the_live_session_and_reviewed_again`（`/effort high`）・`a_third_review_that_does_not_pass_asks_a_person_and_land_lands_it`（high→xhigh）
