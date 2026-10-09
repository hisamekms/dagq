---
id: design-supervisor-lifecycle-review
type: design
title: "Review (supervisor)"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1942-2
  - adr-t1728-2
  - design-supervisor-lifecycle-task-replanning
  - adr-t1570-1
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0027
  - adr-0050
  - adr-t803-1
  - adr-t947-1
  - adr-t1233-2
  - adr-t1582-1
  - adr-t1165-1
  - adr-t1453-1
  - adr-t1984-1
  - adr-t1895-1
  - adr-t1895-2
  - adr-t1428-1
---

# Review (supervisor)

## 概念

### 目的

validationを通ったrunを、着地の前にheadlessのreview jobで判定し、判定に従って着地・差し戻し・人の判断へ進める。
人に上げるのは、jobが推奨を出せないか確信が低いもの、受け入れ条件の外れか成果を捨てる判断だけにする（[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)）。
決定の元は[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定2と[ADR-0027](../../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)決定1–4。

### 全体の流れ

```text
validationを通ったrun（awaiting_integration。supervisorがleaseとslotを持ち、workerのsessionは開いたまま）
  → 1 開始: review.mdとpromptを書く
  → 2 headless実行: 行き先のproviderで読み取りだけのjob
  → 3 verdict: 読めない・非0の終了は同じ入力で1回だけやり直す
       pass    → 5 衝突の事前判定 → 終了の依頼とclose → 着地の前のe2e → 着地
       revise  → 4 reviseの往復（回に2回まで）→ validationからやり直す
       concern → AIが決めるconcern: land（passと同じ道）/ send_back（4と同じ道）/ ask
       失敗・打ち切り → approve_landingのask → 6 askの回答（land / send_back / cancel）
  supervisorが替わったら 7 引き継ぎ、reviseの待ちの間の質問は 8
```

### 責務と境界

- supervisorはreviewの間もrunのleaseとslotを持ち、runのstatusは`awaiting_integration`のままにする。
  進行はstatusでなく`review_started`・`review_finished`などのeventで読む。
- review jobは読むだけで、worktreeとrun dirのファイルを読み、verdictのJSONをstdoutに出す。
  worktreeは生きているworkerのsessionのものなので、jobは書く道具を持たない。
  Claudeのreviewは予約のwakeupが`claude -p`の終了を妨げるので予約の道具を拒む（一覧と理由は`PRINT_MODE_DENIED_TOOLS`のdoc comment）。
- 直すのはworkerのsessionで、supervisorは依頼を次のturnとしてrun dirの`turns/`に書くだけ（[非対話のworker](headless-worker.md#run-dirのturns)）。
- 着地は`integrate`と同じ処理をsupervisorが別threadで走らせる（[integrate](integrate.md)）。
  review中のrun（supervisorのleaseがある）を人の`integrate`は拒む。
- 資料`review.md`は[`review`](review-command.md#review)が、promptの組み立てと上限は[Prompt](prompt.md#headlessのjobのprompt)が持つ。
- providerの一般の規則は[Agent provider lifecycle](../provider-lifecycle.md)と[Actor model](actor-model.md)が持つ。

### 不変条件

- passでないreviewのrunは、人の`land`の答えか、jobの確かな`land`の推奨なしには着地しない（`domain::concern::lets_land`）。
- 1つのreviewのやり直しは原因（読めないverdict・非0の終了）を合わせて1回まで。
- reviseは回（区切りの後）ごとに2回まで（[ADR-0050](../../adr/0050-revise-count-scope-per-review-round.md)）、回の中の3回目のreviseと`send_back`は人に聞く。
- 依頼（revise・衝突の解消）はeventを記録してから`turns/`に書くので、二重には書かない。
- 1つのrunに開いた`approve_landing`のaskは1つ。
- e2eはhostで同時に1本だけ流れる。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| reviewの始まりと行き先 | `src/application/supervise/landing.rs`の`Supervisor::begin_review`・`prepare_review`・`review_route`、`src/application/supervise/provider.rs`の`review_route`・`review_moves` |
| jobの終わりとやり直し | `src/application/supervise/jobs.rs`の`ReviewWatch`・`ReviewEnd`・`review_end`、`landing.rs`の`retries_review` |
| verdictごとの進め方 | `landing.rs`の`act_on_verdict`・`act_on_concern`・`act_on_agents`・`send_revise`・`precheck` |
| reviseと衝突の待ち | `src/application/supervise/revise.rs`の`ReviseWatch`・`Fix` |
| reviseの回数と衝突の試行 | `src/domain/run/history.rs`の`decide_revise`・`decide_conflict`・`RunHistory::round_revise_attempts` |
| askとその答え | `landing.rs`の`ask_approve_landing`・`apply_landing_answers`・`start_approved_landings`、`src/domain/mod.rs`の`LandingAnswer` |
| 引き継ぎ | `src/application/supervise/adopt.rs`（`adopted_start`・`adopted_concern`・`backfill_sent_back_concern`）、`src/domain/turn.rs`の`adopted_delivery` |
| reviewのprompt | `src/application/prompt/`の`review_prompt`とその定数 |
| reviewのsubagent | `src/application/review.rs`（`snapshot_subagents`・`check_agents`）、`src/domain/review_subagents.rs` |
| 着地の前のe2e | [着地の前のe2e](landing-e2e.md) |

## reviewの流れ

1. **開始**: `review.md`を書き、promptを上限の中で組み立てて書いてから`review_started`を記録する。
   資料かpromptを書けなければ、jobを起動せずに下の「reviewの失敗」にする。
2. **headless実行**: `AgentProvider::review_command`のport（権限の意図は`REVIEW_ACCESS`、[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）でjobをworktreeで起動し、promptはstdinで渡す。
   Claudeのreviewは`--setting-sources ""`とsettingsの`autoMemoryEnabled: false`で、worktreeの`.claude/`・`.mcp.json`・`CLAUDE.md`・auto memoryとuserの設定を読まない（[ADR-t1470-1](../../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)）。
   そのためpromptが、worktreeのrootのinstructionsとそれが名指す文書を読んで判定するよう1文で求める。
   reviewのsettingsは`Stop` hookを持たない（生きているworkerのsessionのidle markerを書かないため）。
   jobはreviewの段のjobの共通の経路（[Headless job processes](headless-job-processes.md#記録)）を`agent`の種類（`JobKind::Agent`、`headless_jobs.kind`は`review`）として通り、起動の記録・timeoutの停止・引き継ぎは`program`の種類のjobと同じである。
   やり直すかどうかはどちらも`JobKind::retries`で決め、`agent`のjobは非0のexitを1回だけやり直す（`program`のjobはやり直さない）。
   時間の上限は種類ごとで、`[review.jobs] agent_timeout_secs`（[Run environment](run-environment.md)）が無ければjobを動かすproviderの`AgentProvider::review_timeout`。
3. **verdict**: verdictが読めれば`review_finished`を記録する。
   読めない（JSONが無い・壊れている・未知の欄）か非0で終わったreviewは、やり直しでなければ同じ入力で1回だけやり直す（`review_retried`）。
   時間の上限で止まったreviewと起動できなかったreviewはやり直さず、reviewの失敗にする。
   やり直しかどうかはsupervisorの状態（`ReviewWatch::retried`）が持ち、待ちを挟んでも保つ。
4. **reviseの往復**: `revise`と、適用する`send_back`のconcernは、回にreviseが残り、sessionが生きていれば、直す依頼を生きているsessionの次のturnに書く（`revise_requested`）。
   文面と手順は`application::prompt::revise_request`が持つ。
   書き込みに失敗したら`revise_unsent`で取り消し、回数から引く。
   sessionがreceiptを依頼より後に書き直し、その後のidle markerが出て、receiptの`commit`がcleanなworktreeのHEADと一致したら、`revise_finished`を記録して[Validation](validation.md#validation)からやり直す。
   receiptがHEADと一致しなければvalidationに渡さず（渡すと`failed`になって作業を失う）、HEADで書き直す依頼を送って待ち直す。
   書き直さないidle・sessionの終わり・`resume_timeout`・依頼の書き込みの失敗は、`approve_landing`のaskで人に聞く。
   `resume_timeout`は依頼か待ちを始め直した入力（答え・控えの解除など）から単調時計で数え、HEADで書き直す依頼では数え直さない。
5. **終了の依頼とclose**: pass・適用する`land`のconcernは着地の直前に、askにするconcernとreviewの失敗はaskを作る前に、workerのsessionの終了を依頼する（run dirの`turns/exit`、[非対話のworker](headless-worker.md#wrapperがturnを止めるとき)）。
   wrapperの`session_exited`の後にbackgroundのwrapperの残りを止める（runはworkspaceを開かない。[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
   - **passの衝突の事前判定**（ADR-0027決定4）: 終了を依頼する前に、passしたheadとlanding branchを`GitRepository::merge_conflicts`で（refを動かさずに）比べる。
     衝突すれば解消の依頼（`ResumeKind::Precheck`）を生きているsessionに送り、4と同じ待ち（`Fix::Conflict`）に入る。
     この依頼は[Needs session](needs-session.md)の「試行の数え方」の衝突だけの試行で、使い切ったrunは引き継ぐretryに回せれば着地へ進み（rebaseが衝突すれば「引き継ぐretry」がtaskを`ready`に戻す）、回せなければaskにする。
     数えるresumeを使い切っていれば送らずにaskにする。
     sessionが居ない・送れない・merge-treeが失敗したときは送らずに着地へ進み、着地のrebaseの衝突は`needs_session`のresumeに回る。
   - **pass**: 衝突しなければ`landing_queued`を記録し、e2eが要るrunは[着地の前のe2e](landing-e2e.md)を通してから、着地スロットを待って`integrate`と同じ着地を走らせる。
   - **concern**（とreviseの打ち切り）: 下の[AIが決めるconcern](#aiが決めるconcern未実装)で決め、askにするものは`approve_landing`のask（`land`・`send_back`・`cancel`）を作ってleaseを外す。
     askを作る前に、そのrunの閉じていない前の`approve_landing`のaskを閉じる。
     残すと同じrun・kindの開いたaskに畳まれて新しい理由が人に届かず、回答済みの古い答えが新しいreviewに適用されうるため。
   - **reviewの失敗**: concernと同じ経路で`approve_landing`のaskを開いてから`review_failed`を記録し、leaseを外す。
     askの問いは失敗したattemptとerrorと、jobが走ったならその出力のpathを名指す。
     askを開けなければ、attentionの`review by hand`から人がreviewして`integrate`を呼ぶ。
6. **askの回答**: supervisorは毎回、回答済みで閉じていない`approve_landing`のask（runが`awaiting_integration`でleaseが無いもの）の答えを、slotや着地スロットの空きに依らずその場で適用してaskを閉じる（`apply_landing_answers`）。
   - `land`は承認を記録してrunを着地の列に並べ、別の段（`start_approved_landings`）がslotと着地スロットの空きを待って承認の古い順に1本ずつ着地させる。
     列はeventから読む（`RunHistory::queued_to_land`）ので、supervisorが替わっても次のsupervisorが拾い、二重に着地しない。
   - `send_back`はrunを`needs_session`にし、[`needs_session`](needs-session.md#needs_session)のresumeがreviewの理由を解消依頼に含める。
     人が`send_back: <理由>`で足した理由は、reviewの指摘より先に載る。
   - `cancel`はrunを`failed`にしてtaskを`cancel`する。
   - 答えの読み方は`domain::LandingAnswer::parse`の1つで、それ以外の答えはinboxが読んで人に返す。
   - runが着地せずに終わると（runが`failed`、またはそのtaskが`completed`・`canceled`。`domain::ended_landing_ask_answer`）、その答えはもう当たらないので、runtimeがそのrunの閉じていない`approve_landing`のaskを閉じる（未回答なら終わり方を答えて`runtime_closed: true`、回答済みなら`ask_closed`）。
     runを`failed`にするかtaskを終える経路（triage・resumeの使い切り・`cancel`の答え・別のrunの着地）がその場で閉じ、閉じ損ねたもの（手での操作など）はsupervisorの定期のsweep（`sweep_ended_runs`）が次の周回で閉じる。
     taskが終わっていない間は、`interrupted`のrunと、`needs_session`など待ちに戻りうるrunのaskは閉じない（resumeの後に待ちに戻ったrunに答えを適用する）。
     閉じた後に`failed`のrunが`decide`の`resume`で戻ったときは、次のreviewが人に聞くなら新しいaskを開く。
7. **引き継ぎ**: review中のsupervisorが死ぬと、leaseがstaleになったrunを他のsupervisorがadoptし、最後の段のeventから続きを決める（`adopt.rs`）。
   - reviseか衝突の依頼の後なら、sessionが生きていればその待ちに戻る。
     記録の後・書く前に止まったときのため、依頼が`turns/`に届いているかを照合し、無ければ1回だけ書く（`domain::turn::adopted_delivery`）。
   - `review_finished`の後ならそのverdictで進み、concernは決め直す。
   - askにするはずの錨の後に、閉じていないsupervisorの`approve_landing`のaskがあれば、開き直さずにそのaskを待つ。
     reviewの失敗のaskを開いて`review_failed`の前に止まったときも同じで、`adopted: true`の`review_failed`を補う。
   - それ以外はreviewを最初からやり直す。
   - sessionもwrapperの終了の記録も無く消えたrunは、sessionが終わったものとして扱い、reviseはaskになる。
   - review・revise・終了の途中のerrorはrunをabandonし、leaseの無い`awaiting_integration`として`review and integrate`のattentionになる。
8. **reviseの間のask**: reviseの待ちは、workerのsessionと同じく`worker_question`の答えを次のturnに届ける（[人の答えを待つrun](waiting.md)の「差し戻しと解消依頼の段」、[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)）。
   - 対象は依頼の送信以後の質問だけで、その閉じていない質問がある間は、書き直さないidleとは見ず`resume_timeout`も数えない。
   - ただし依頼より後にreceiptを書き直してidleになれば、質問の閉じを待たずに判定する（[ADR-t583-1](../../adr/2026-09-28-t583-1-revise-judges-a-rewritten-receipt-past-an-open-question.md)）。
   - 答えを書いた時刻から待ち直し、この待ち自身が届けた答えのcloseは手の配送とみなさない（`ReviseWatch::delivered_closed`）。
   - 質問を待つrunはslotから外れ、戻った時点から`resume_timeout`を数え直す。

## reviewのprovider

- 行き先は`Supervisor::review_route`がreviewを始めるたびに決める。
  `provider`を書かないreviewはClaudeで動き、控えの間は待つ。
- `provider = "codex"`のreviewは、同じ`review.md`とpromptを`codex exec --json`の読み取りだけのjobに渡す（permission profileは[Queue service](../queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)、選び方は[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)）。
  worktreeのprojectを信頼せず、workerが置ける`.codex/config.toml`と`.codex/rules`を読まない（[ADR-t1570-1](../../adr/2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)）。
- `provider`を書いたreviewは、使えないproviderを避けてもう一方で起動し、起動の後に使えないと分かったら控えて（`ProviderHold`）同じpassでもう一方で起動し直す（`domain::actor_model::job_route`）。
  両方が使えなければ`Phase::ReviewHeld`で待ち、毎回行き先を見直す。
- 引数・envの大きさとstdinの一時ファイルによる起動の失敗は、providerの問題でないので控えも切り替えもせずreviewの失敗にする。
- `[provider_fallback] jobs = false`（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）では、使えないことを理由にもう一方を選ばず、失敗したproviderを控えたまま待ち、控えが解けてから同じproviderでreviewし直す。
- `--no-claude`で行き先の無いreviewは待たずに手動review（`approve_landing`）に渡し、askは「no review agent ran」と書く。

## reviewのsubagent<a id="reviewのsubagent"></a>

[ADR-t1453-1](../../adr/2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)の実装。
変えたpathに応じて必須のagentを選び、親のreview jobの中でsubagentとして動かし、親のverdictに集約させる。
[ADR-t1895-1](../../adr/2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)はこれを、programのjob（[ADR-t1895-2](../../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)）と、全体のreviewとagentごとのjobの並列に置き換える決定で、移行が終わるまではこの節の実装が動く。

- **設定**: `dagq.toml`の`[review.subagents.<agent>] paths`（書式は[Run environment](run-environment.md)）と、定義のファイル（`domain::review_subagents::find_definition`、[ADR-t1728-1](../../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)）。
  設定の誤りは`dagq doctor`の`agents`（`application::review::check_agents`）が出す。
- **道具の宣言**: 定義のfrontmatterの`tools`はruntimeの道具の一覧から選び、役割の許可を超えれば誤りにする（`AgentTools`、[ADR-t1728-2](../../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)）。
  宣言は独立のagentのjobの起動に当たり、親のjobの中のsubagentの道具は宣言に依らず固定（`SUBAGENT_TOOLS`）。
- **snapshot**: 必須のagentと定義は、試行ごとにlanding branchの今のcommitのtreeから読む（`snapshot_subagents`）。
  workerがrun branchで設定や定義を変えても、選ばれるagentは変わらない。
- **選び方**: reviewする範囲の変えたpath（renameは旧と新の両方）を各agentのglobに照らし、当たったagentを設定の順に1回だけ選ぶ（`domain::review_subagents::select`）。
  範囲は試行ごとに1回決め、`review.md`も同じ範囲で書くので、途中でmainが進んでも選んだ差分と資料の差分は食い違わない。
- **jobへの受け渡し**: 選ばれたagentの定義と当たったpathを`review-subagents-<attempt>.json`に書き、promptに名指しと指示（`SUBAGENTS_INSTRUCTION`）を足す。
- **Claudeへの渡し方**: `AgentProvider::review_subagents`が`--agents`と`--allowedTools Agent`を足す。
  親の`--disallowedTools`とsettingsのdenyはsubagentにも効き、worktreeの`.claude/agents`は読まない。
  実CLIでの確かめは人が流す[手動スモーク](../manual-smoke.md#reviewのsubagentの実cliの確認)が持つ。
- **Codexとproviderの切り替え**: Codexはsubagentを動かさない（`runs_review_subagents`）。
  必須のagentのあるreviewは、行き先が動かせなければ、もう一方が使えてsubagentも動かせるときだけ切り替える（`subagent_launch_of`、`switch_reason: subagents_unsupported`）。
  これは能力による選択なので控えにせず、`[provider_fallback] jobs = false`でも切り替える。
  もう一方がreviewの役割とsubagentを動かせて控え（queueの控えのask・`ProviderHold`）で今使えないだけなら、起動も失敗もせず`Phase::ReviewHeld`で待ち、控えが解けたpassで起動する（[ADR-t1847-1](../../adr/2026-10-08-t1847-1-review-waits-while-the-provider-that-runs-its-subagents-is-held.md)）。
  待ちの`ReviewHeld`は必須のagentを持ち、routeが待たなくなっても動かせるproviderが控え中のあいだは待ち続ける（`Supervisor::review_held_waits`）。
  動かせるproviderのagentがこのsupervisorに無い（`--no-claude`など）か、そのproviderがreviewの役割を動かせないときだけ、起動せずにreviewの失敗にする。
- **結果のそろいの検査**: 選ばれた全てのagentにちょうど1件の完了した判定が無いverdictは、読めないverdictと同じく1回だけやり直し、なおそろわなければreviewの失敗にする（`domain::review_subagents::incomplete`）。
- **判定ごとの行き先**: 親とagentごとの判定を着地・差し戻し・人の判断に直し、最も重いものを取る（`ReviewVerdict::route`、`domain::review_subagents::destination`）。
  agentが決めた差し戻しは理由を合わせた1回のreviseにし、人の判断なら問いに親が軽かったことを載せる（`act_on_agents`）。
  記録した行き先（`review_finished`の`route`）があれば、着地はそれが`land`のときだけ（`concern::lets_land`）で、引き継いだsupervisorも親のverdictだけでは進めずaskにする。
- **誤り**: landing branchの`dagq.toml`が読めない、選ばれたagentの定義が無い、道具の宣言が誤り、範囲かreceiptが読めないときは、必須の検査が分からないので空にせず、起動せずにreviewの失敗にする。
  `dagq.toml`は全体を解釈するので、`[review.subagents.*]`の外の誤りもreviewを失敗させる。
- **設定の無いrepository**: `[review.subagents.*]`の無いrepositoryのreviewは、subagentに関わる欄・指示・引数を持たない。

## プログラムのreview<a id="プログラムのreview"></a>

[ADR-t1895-2](../../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)の部品。
reviewの段への接続（落ちたらagentを動かさず差し戻す）はまだ無く、runのreviewはprogramを動かさない。

- **snapshot**: 試行ごとにlanding branchの今のcommitのtreeから`[review.programs.<name>]`（書式は[Run environment](run-environment.md)）とその`script`の中身を読み、範囲の変えたpathで選ぶ（`review_programs::snapshot_programs`）。
  worktreeとmain checkoutのファイルは読まず、workerが変えた設定やscriptは着地まで効かない。
  scriptは読んだ中身をworkerが書けない場所に書き出して実行する。
  programは`script`でだけ名指す（理由は`ReviewProgram`）。
  cwdから呼ぶscriptとツールがcwdで読む設定（`rust-toolchain.toml`・`.cargo/config.toml`）はworkerの選んだ版が効きえ、runtimeは検出しない（integrateのverifyも同じ）。
- **流す場所**: reviewのactor（`ReviewJob`）のbackendで`ProgramBackends`が`ReviewProgramBackend`の実装を選ぶ。
  hostは`HostPrograms`、未実装のPodmanは起動の失敗にしhostに戻さない（fail closed）。
  cwdはrunのworktreeで、jobは`start_review_program`が起動する。
- **env**: 起動元のenvを消し、e2eの関門と共通の部品（`passed_env::PassedEnv`）で絞る。
  資格情報の除外は常に効き、例外を持たず、cmuxのsocketのpassword・`CMUX_*`・queue serviceとbrokerに届く`DAGQ_*`は渡らない。
  `PATH`からは空・相対の項目と、runのworktree・run dir・main checkoutとその下を指す項目を除く（`review_programs::narrowed_path`）。
- **結果**: `HeadlessJob::poll_program`が終了のstatusとstdout・stderrの末尾を返す。
  上限（programの`timeout_secs`、無ければ`[review.jobs]`）を超えればprocess groupごと止め、statusは無い。

## 着地の前のe2e

passしたrunのうちe2eが要るものは、着地スロットを待つ段でsupervisorがhostでe2eを流してから着地する。
流し方は[着地の前のe2e](landing-e2e.md)が持つ。

## 文書の照合

runのreviewのpromptは、acceptanceの前に`REVIEW_DOCS_CHECK`の1段落を置く（[ADR-t1428-1](../../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)決定5、[ADR-t1942-2](../../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)決定4）。

- 変えた挙動を説明する文書（task・`summary`が名指すもの、読んで見つけたもの）を差分と`summary`に照らして読む。
  文書の差分だけでは正しいとせず、文書を直さなかった理由も確かめる。
- 両方向: 古い文書と変わった流れ・境界・不変条件・約束の書き漏れを`docs_drift`で指摘し、加えて差分が文書に入れたコードの書き写し（欄・flag・既定値・関数名・test名の列挙）と経緯も指摘する。
  名前が文書に無いことだけでは古いとしない。
- verdictの形と分類コードは変えない。
- このrepositoryでは、探した名前の確かめと書き写し・経緯・予算の指摘をdesign-consistencyの[subagent](#reviewのsubagent)が受け持ち、規則は[documents.md](../../development/documents.md#design)が持つ。
- `review.md`のTaskの節はcontextを持つので、plannerがcontextに書いた関連文書もreviewが読む（[`review`](review-command.md#review)）。

## 差し戻しの分類コード<a id="差し戻しの分類コード未実装"></a>

[ADR-t947-1](../../adr/2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)。
コードの一覧と定義は`domain::review_reason::REVIEW_CODES`が重い順に持ち、promptがそのまま載せる。
コードを変えるのにADRは要らない（決定6）。

- **verdictの形**: `reasons`の各項目は文とコード（先頭が主）で、文だけの項目も読む。
  コードは集計のlabelなので、コードの形のせいでverdictを読めなくはせず、判定と適用にも使わない（決定3）。
  主のコードは最初の項目の主のコードで、promptはverdictを決めた項目を先頭に置かせる。
- **主の選び方**: 1つの項目が2つ以上に当たるときは、直すのに誰の判断が要るかの重い方を主にする（一覧の順）。
  `adr_conflict`・`acceptance_conflict`・`acceptance_infeasible`は、workerが条件を満たすと別の決まりや事実に反すると書いているときで、単に届いていなければ`acceptance_unmet`。
- **人の答えからの補い**（決定4）: `approve_landing`の答えを適用するとき、最後のreviewがreviseかconcernなら、答えとそのコードを`review_outcome`に記録する（`domain::review_reason::answer_outcome`）。
  reviewの失敗のask、passの後の衝突のask、workerに返したreviseには記録しない。
  集計は[Stats](stats.md#差し戻しの分類コードごとの集計)が持つ。

## testからのretryの差し替え

`SuperviseOptions::retry_unreadable_review`は本番では常に有効で、CLI・`dagq.toml`・環境変数から変える経路は無い。
testだけが無効にして、やり直しのjobを省いて最初の失敗から失敗の処理へ進められる（helperの使い分けは`tests/it/runtime_support`が持つ）。

## AIが決めるconcern<a id="aiが決めるconcern未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定3（ADR-0027決定1・2のamends）。

- **verdictの欄**: concernのverdictは推奨（`land` / `send_back`）・確信度・人が要る理由（`scope` / `discard`）を持ち、passとreviseでは捨てる（`ReviewVerdict`）。
  `scope`は着地させると受け入れ条件・ADR・goalの決定からの外れを受け入れるもの、`discard`は成果を捨てる判断。
  知らない理由は`scope`として読み、人が決める側に倒す（`domain::concern`）。
- **決め方**（`domain::concern::decide`）: 推奨が無い・人が要る理由がある・確信度が`high`でないものはaskにする。
  残りの`land`は適用し、`send_back`は回にreviseが残っていれば適用し、残っていなければaskにする。
- **適用**（`Supervisor::act_on_concern`）: `land`はpassと同じ衝突の事前判定・e2e・着地へ進み、`send_back`はreviseと同じ依頼として生きているsessionに返す。
  着地を許す判定（`integrate`の拒否・resumeの数え方・着地の回復）は、適用される`land`のconcernを「passした」と読む（`concern::lets_land`）。
- **ask**: askにするconcernは推奨と確信度（[ask](ask.md#aiの推奨と確信度未実装)）を載せ、問いにaskにした理由を足す。
  reviewの失敗・reviseの打ち切り・衝突のaskは推奨を載せない。
  適用した`send_back`の後でsessionが直さなかったときのaskは、reviseでなく適用したconcernとして聞く（`Fix::Revise`の`concern`）。
- **記録**: concernごとに1回、適用でもaskでも`concern_decided`を記録し、jobが動かなかったreviewは記録しない。
  適用した`send_back`が後からaskに至ったときは`concern_decided`を書き直さず、`concern_send_back_escalated`を足して同じattemptで結ぶ。
- **sessionの終了の依頼とclose**（ADR-0027決定1のamends）: `land`はpassと同じく着地の直前、askはaskを作る前、`send_back`はsessionを開いたまま。
- **引き継ぎ**: concernの後で止まったrunは、adoptした側がverdictから決め直す（`adopted_concern`）。
  `send_back`の依頼を記録した後に`concern_decided`の前で止まったときは、引き継ぎが適用として補う（`backfill_sent_back_concern`）。
- **prompt**: concernを機械的な修正でなく判断の要るfindingとし、推奨と確信度の付け方、人に残す基準（迷えば`low`）、runtimeが確かなものを適用することを書く（`application::prompt`のconcernの定数）。

## taskの再計画（予定・未実装）

原因別診断で継続か再計画を選ぶ経路を足す。
review・e2e・integrateの途中の保留は終了境界とgenerationの柵を守り、古いpassや`approve_landing`で置換先を着地させない。
詳細は[taskの再計画](task-replanning.md)が持ち、今の挙動は上の各節のとおりで、この追加だけではrunを保留しない。
