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
  - adr-t3957-1
  - adr-t4005-1
  - design-supervisor-lifecycle-actor-model
---

# Worker model

workerのsessionのmodelとeffortの選び方と記録（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定3・4・5）。
予測の記録は[plan review](plan-review.md#verdictと適用)、読み方と段上げの件数は[stats](commands.md)と[kpi](commands.md)。
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
| `task_released`（`model`あり） | holdの解除が選んだmodel。以後のresume・reviseとtaskの以後のrunのclaimはそこから開き、段上げもそこから（`WorkerSession::claimed`） |

### Codexのrun

Codexのworkerは段のClaudeのmodelを使わず（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）、effortだけを使う。
そこでCodexで動くrun（`actual_provider`が`codex`。claimで切り替わったrunも、途中でCodexへ移ったrunのその後のresume・reviseも）のsessionを開くeventは、段のmodelを別に持ち群を持たない（`WorkerSession::fields_on`）。

- 読み戻し（`WorkerSession::current`）はその段のmodelを段として読むので、段上げとtaskへの引き継ぎはClaudeのrunと同じに働く（効くのはeffortだけ）
- Codexが実際に使ったmodelは、turnごとに`turn_finished`に残る（[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。
  `stats`・`kpi`のrunのmodelはCodexで始まったrunではこの最初のturnのmodel（[stats](commands.md)）
- Codexで動くclaimは試しに入らない: `choose`を呼ばず、群も百分位も持たない（群を持ったtaskでもそのrunは群null）。
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
- **群の候補から上げる**: 試しの群の候補は段の上の組だけを受け付け（段の外の組は上がらないため）、段上げは群の候補の段から次の段へ上げて群を保つ（[ADR-t3957-1](../../adr/2026-11-02-t3957-1-production-ab-of-worker-and-runtime-planner-model-and-effort.md)決定2）。
- **層**: `kpi`のmodel・effortの層はrunの最初の`run_claimed`の値で分けるので、resume・reviseで上げた段は層に出ず、retryで引き継いだ段は出る。

## 限定の試し（`[worker.trial]`）

`dagq.toml`の`[worker.trial]`で有効にしたときだけ働く（[ADR-t3957-1](../../adr/2026-11-02-t3957-1-production-ab-of-worker-and-runtime-planner-model-and-effort.md)、ADR-0079の決定4をamends）。
有効にするのは人の判断。
keyと既定値の意味は`domain::worker_model::WorkerTrial`のdoc commentが持つ。

- **読む時点**: supervisorがclaimのpassごとに読む（`Verifier::worker_trial`）ので、設定の変更は再起動なしに次のclaimから効く。
  読めないときはwarnをlogに出して試しの外でclaimする（壊れた`dagq.toml`はprovisioningがerrorにする）。
- **設定の検査**: 候補が段（`LADDER`）の上の組でないものなど、`WorkerTrial::check`が断る値は`dagq.toml`の読み込みが断る。
  段の外の組は段上げで上がらないため（決定2）。
- **選ぶ場所**: claimのtransactionの中（`infrastructure::sqlite::claim_task`）で、queueの`task_weight_predicted`と`run_claimed`を読み、純粋関数`domain::worker_model::choose`が決める。
- **決定的な割り当て**: 群をまだ持たない対象のtaskは、試しの名前とtaskのIDのhashを割合と比べて群を決める（`WorkerTrial::draw`）。
  claimの順や並行に依らず、同じ設定なら同じtaskは同じ群になる。
- **群の固定**: 前のrunの`run_claimed`で群を持ったtaskは、その群の今の候補で起動する（resume・revise・retryで変えない。予測・割合・試しの名前が後で変わっても変えない）。
  試しを無効に戻すと、群を持ったtaskも既定で起動する。
- **対象外**: 対象に当たらないtask、予測の無いtask、holdの解除がmodelを選んだtaskは群null。
- **記録**: 群・model・effortは`run_claimed`の属性の組に置き、群を持つclaimは群を引いた試しの名前も残す。
  台帳とkpiに群専用の欄は足さない（一般のA/Bへ移せる形。決定7）。

[ADR-t4005-1](../../adr/2026-11-02-t4005-1-production-ab-of-worker-and-review-provider-pairs.md)（ADR-t3957-1の決定2をamends）は、model・effortの試しと別の試しとして、workerとそのrunのreviewのproviderの組を群に分けると決めた。
`dagq.toml`の`[worker.provider_trial]`で有効にしたときだけ働き、keyと既定値の意味は`domain::provider_trial::ProviderTrial`のdoc commentが持つ。
supervisorは`[worker.trial]`と同じpassで読み（読めなければ両方の試しの外でclaimする）、`ProviderTrial::check`が断る値（範囲の外の割合、Bのworkerとreviewが同じprovider）と、有効にしたときの`[worker.trial]`と同じ名前は`dagq.toml`の読み込みが断る。
群を引く場所はmodel・effortの試しと同じclaimのtransactionで、次を約束する。

- **独立の割り当て**: providerの組の群もtaskのhashで決め、名前の違う試しなのでmodel・effortの群と互いに独立になり、一度持った群はresume・revise・retryで変えない。
- **重なり**: Codexで動く群のclaimは今どおりmodel・effortの試しの外で、Claudeで動く群のclaimには両方の試しの群が重なりうるので、両方をrunの属性の組に記録する。
- **試しの外**: taskのproviderがrepositoryの既定でないtask・headlessでないclaim・Spikeは群nullとその理由を記録する。
  既定のproviderを明示したtaskは既定と見分けられず対象に入る。
  holdの解除でproviderを選んだtaskも、人の選択を群より優先して試しの外にする。
- **Bのclaim**: Bのtaskはworkerの要求のproviderをBのworkerのproviderにしてclaimし、使えないときの切り替えはどのworkerとも同じ規則に従う。
  claimの前の延期の判定もclaimと同じ要求のworkerで判じ、そのrouteが無ければ他のtaskと同じく延期を記録して待つ。
- **記録と読み口**: 群・試しの名前・割り当てたworkerとreviewのprovider（または試しの外の理由）は`run_claimed`の属性の組に置く。
  reviewの起動はrunの群と割り当てたreviewのproviderを`domain::provider_trial::RunPair::of_run`で読む。

## 判定

群ごとの比較は[stats](commands.md)と[kpi](commands.md)のrunの層（runの最初の`run_claimed`の値で層別する）で読む。
判定は人とplannerが行い、runtimeは自動で止めたり全面適用したりしない（ADR-0079の決定6）。
止めるのは`[worker.trial]`を無効に戻すこと。
守る指標の差が大きく超えたときに止める判定も人とplannerが行い、自動では止めない（[ADR-t3957-1](../../adr/2026-11-02-t3957-1-production-ab-of-worker-and-runtime-planner-model-and-effort.md)決定6）。
providerの組の試しは、割り当てどおりに動いた組を主に、切り替えたrunを割り当てた群ごとの別の層に、判定できないrunも含む割り当てた群を副に読む（[ADR-t4118-1](../../adr/2026-11-04-t4118-1-provider-pair-primary-and-switched-strata.md)）。
環境が原因のrunは、切り替えの判定と別の軸で主から外し、割り当てた群ごとの別の層に読み、副には残す（[ADR-t4163-1](../../adr/2026-11-05-t4163-1-environment-caused-failures-are-judged-by-the-recovery-job-and-kept-in-their-own-stratum.md)）。
切り替えを判定できないrunは、環境が原因なら環境の層にも入るほかは副だけに含め、群ごとに件数を見せ、切り替えたrunの数には入れない。
主の品質の差、切り替えの多さやBの環境が原因のrunの多さで主のBの件数が溜まらないこと、workerを待たせるrepositoryの控えの待ちを見て、止める・広げる判定を人とplannerが行う。
層の判定と集計の入口は`kpi::pair`のdoc comment。
