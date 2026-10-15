---
id: design-supervisor-lifecycle-worker-model
type: design
title: "Worker model"
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-plan-review
  - adr-0079
  - design-supervisor-lifecycle-actor-model
---

# Worker model

workerのsessionのmodelとeffortの選び方と記録（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定3・4・5）。
予測の記録は[plan review](plan-review.md)の6、読み方と段上げの件数は[stats](commands.md)と[kpi](commands.md)。
worker以外のアクター（決定7の(b)(c)）は[Actor model](actor-model.md)。

## 既定を明示して渡す

- runtimeはworkerの起動・resumeのたびに、modelとeffortをproviderに明示して渡す（`AgentProvider::select_model`）。
  Claude Codeの既定やユーザー設定に依らない。
- 既定は`domain::worker_model::WorkerSession::default`で、試しが無効なら全runがこれで起動する。
- 決めるのはclaimとresumeとreviseで、runのsession wrapper（`application::session`）は起動の直前にそのrunで最後にsessionを開いたeventの値を読んで渡す（`WorkerSession::current`）。
  resumeは`resume_started`を記録してからsessionを開くので、その値（下の段上げで上がったものを含む）で開き直す。
  記録の無いrunは既定で起動する。
- headlessのworker（`application::headless_session`）はturnごとに別のprocessなので、turnを起動するたびに同じ`WorkerSession::current`を読み直す。
  reviseで上がった段は次のturnから効く。

## 記録

sessionを開くeventのpayloadにmodelとeffortと群を持たせる（`WorkerSession::fields`）。

| event | いつ |
|---|---|
| `run_claimed` | claim（retryで新しいrunをclaimしたときも） |
| `resume_started` | `needs_session`のresume。値は直前のsessionのもので、taskに由来する失敗の後は1段上げたもの |
| `revise_requested` | reviewの`revise`の差し戻し。値は直前のsessionを1段上げたもの |

### Codexのrun

Codexのworkerは段のClaudeのmodelを使わず（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）、effortだけを使う。
そこでCodexで動くrun（`actual_provider`が`codex`。claimで切り替わったrunも、途中でCodexへ移ったrunのその後のresume・reviseも）のsessionを開くeventは、段のmodelを別に持ち群を持たない（`WorkerSession::fields_on`）。

- 読み戻し（`WorkerSession::current`）はその段のmodelを段として読むので、段上げとtaskへの引き継ぎはClaudeのrunと同じに働く（効くのはeffortだけ）
- Codexが実際に使ったmodelは、turnごとに`turn_finished`に残る（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。
  `stats`・`kpi`のrunのmodelはCodexで始まったrunではこの最初のturnのmodel（[stats](commands.md)）
- Codexで動くclaimは試しに入らない: `choose`を呼ばず、群も百分位も持たず、交互の順番も取らない（群を持ったtaskでもそのrunは群null）。
  Claudeでclaimして群を持ったrunが途中でCodexへ移ったとき（ADR-t813-2のフォールバック）は、claimの群は記録に残るが、`stats`・`kpi`はCodexで作業したrunとして試しの集計とClaudeの層から外す（[stats](commands.md)）。
  taskの群は変わらないので、そのtaskの後のClaudeのrunは同じ群で起動する

段上げしたeventは前の値と理由も持つ（`WorkerSession::fields_raised`）。

## 段上げ（決定5）

試しの有無に関係なく全taskに効き、設定は持たない。

- **段**: `LADDER`。
  最上段では上げない（`WorkerSession::raised`がnone）。
  段の外の値（`dagq.toml`で決めたものではなく記録の読めない値など）も上げない。
  群（`group`）は変えない。
- **上げる**: taskに由来する失敗（ADR-0079の決定1）の後に開くsession。
  - resume: runを`needs_session`にした最新のeventのcodeがtaskに由来する失敗なら上げる（`worker_model::for_resume`）。
    そのeventが最後にsessionを開いたeventより後にあるときだけ上げるので、1つの失敗で2回上げない（sessionを開かずに終わったresumeの後の次のresumeは同じ段）。
    判定はresumeを始めるtransaction（`begin_resume`）の中で行い、`resume_started`に書く。
  - revise: reviewの`revise`の差し戻しは必ず上げる。
- **上げない**: rebaseの衝突（integrateのrebase・merge-treeの事前検査・landing recheck）、外からのkill（[needs_session](needs-session.md)）、`worker_model::raises`が上げないcodeのresume。
  conflict_precheckの差し戻し（生きているsessionへの衝突の解消の依頼）も上げない。
- **reviseの切り替え**: reviseはsessionに切り替えの入力を送らず、次のturnが上げた値で起動する。
- **取り下げた差し戻し**: `revise_requested`の後に同じattemptの`revise_unsent`がある（差し戻しを打てなかった）ものは、sessionを開いたeventに数えない（`worker_model`の`openings`）。
  その段上げは直前の値にも、taskへの引き継ぎにも、`stats`の段上げの件数にも入らず、その後の人の`send_back`によるresumeは実際に走っていたsessionから1段上げる。
- **taskに引き継ぐ**: 上げた段はそのtaskの以後のrun（retry、引き継ぐretryを含む）に引き継ぐ（`WorkerSession::inheriting`）。
- **層**: `kpi`のmodel・effortの層はrunの最初の`run_claimed`の値で分けるので、resume・reviseで上げた段は層に出ず、retryで引き継いだ段は出る。

## 限定の試し（`[worker.trial]`）

`dagq.toml`の`[worker.trial]`で有効にしたときだけ働く。
有効にするのは人の判断（書式は[Run environment](run-environment.md)）。

- **読む時点**: supervisorがclaimのpassごとに読む（`Verifier::worker_trial`）ので、有効・無効の切り替えは再起動なしに次のclaimから効く。
  読めないときはwarnをlogに出して試しの外でclaimする（壊れた`dagq.toml`はprovisioningがerrorにする）。
- **選ぶ場所**: claimのtransactionの中（`infrastructure::sqlite::claim_task`）で、queueの`task_weight_predicted`と`run_claimed`を読み、純粋関数`domain::worker_model::choose`が決める。
  交互の順番をtransactionの中で決めるので、並行するclaimが同じ番を取らない。
- **群の固定**: 前のrunの`run_claimed`で群を持ったtaskは、同じ群のmodel / effortで起動する（resume・revise・retryで変えない。予測が後で変わっても変えない）。
  試しを無効に戻すと、群を持ったtaskも既定で起動する。
- **対象と割り当て**: 群を持たないtaskのうち、`choose`の条件に当たるものが、前に群を持ったtaskと交互の群になる。
- **対象外**: 条件に当たらないtaskは既定のまま（群はnull）。
  `rework_probability`は選択に使わない。

## 判定

群ごとの比較は[stats](commands.md)と[kpi](commands.md)のrunの層（runの最初の`run_claimed`の値で層別する）で読む。
判定は人とplannerが行い、runtimeは自動で止めたり全面適用したりしない（ADR-0079の決定6）。
止めるのは`[worker.trial]`を無効に戻すこと。
