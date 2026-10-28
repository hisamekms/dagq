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
  - adr-t1570-1
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0027
  - adr-0050
  - adr-t2256-1
  - adr-t2599-1
  - adr-t803-1
  - adr-t947-1
  - adr-t1233-2
  - adr-t1728-1
  - design-agent-eval
  - adr-t1582-1
  - adr-t1165-1
  - adr-t1453-1
  - adr-t1984-1
  - adr-t1895-1
  - adr-t1895-2
  - adr-t1428-1
  - adr-t2259-1
  - adr-t2352-1
  - adr-t2389-1
  - design-supervisor-lifecycle-review-builtin
  - design-supervisor-lifecycle-review-programs
  - adr-t3592-1
---

# Review (supervisor)

## 概念

### 目的

validationを通ったrunを、着地の前にheadlessのreview jobで判定し、判定に従って着地・差し戻し・人の判断へ進める。
人に上げるのは、jobが推奨を出せないか確信が低いもの、受け入れ条件の外れか成果を捨てる判断だけにする（[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)）。
決定の元は[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定2と[ADR-0027](../../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)決定1–4。

### 全体の流れ

```text
validationを通ったrun（awaiting_integration）
  → 0 プログラムのreview: 落ちたら4（agentは起動しない）
  → 1 開始: review.mdと各jobのpromptを書く
  → 2 headless実行: 全体のreviewとagentごとの読み取りだけのjobを並列に
  → 3 判定: 全jobがそろってから
       pass    → 5a 定義を変えるrunはagentのeval → 5 衝突の事前判定 → 終了の依頼とclose → 着地の前のe2e → 着地
       revise  → 4 reviseの往復（runを通した上限まで）→ validationからやり直す
       concern → AIが決めるconcern: land（passと同じ道）/ send_back（4と同じ道）/ ask
       上限の後の差し戻し → 4a コードで着地かask
       失敗・打ち切り → approve_landingのask → 6 askの回答（land / send_back / cancel / retry_review）
  supervisorが替わったら 7 引き継ぎ、reviseの待ちの間の質問は 8
```

### 責務と境界

- supervisorはreviewの間もrunのleaseとslotを持ち、runのstatusは`awaiting_integration`のままにし、進行はeventで読む。
- review jobは読むだけで、worktreeは生きているworkerのsessionのものなので書く道具を持たない。
- 直すのはworkerのsessionで、supervisorは依頼を次のturnとして`turns/`に書くだけ（[非対話のworker](headless-worker.md#run-dirのturns)）。
- 着地は`integrate`と同じ処理をsupervisorが走らせ（[integrate](integrate.md)）、review中のrunを人の`integrate`は拒む。
- 資料`review.md`は[`review`](commands.md)、promptは[Prompt](prompt.md#headlessのjobのprompt)、providerは[Agent provider lifecycle](../provider-lifecycle.md)。

### 不変条件

- passでないreviewのrunは、人の`land`の答えか、jobの確かな`land`の推奨か、4aの収束なしには着地しない。
- 全jobが終わるまで判定せず、答えの無いjobはそのjobだけ1回やり直し、残れば`pass`にしない（プログラムのreviewをやり直したreviewのjobはやり直さない）。
- AIのjobが他のjobの判定をまとめることはない。
- verdictの`follow_ups`は判定と行き先を変えず、それだけでrunを差し戻さない。
- reviseはrunを通した上限まで返し、resumeで数え直さず、上限の後は4aで決める（[ADR-t2599-1](../../adr/2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)）。
  上限は試行ごとにlanding branchの`dagq.toml`から読む（[ADR-t3033-1](../../adr/2026-10-22-t3033-1-run-review-revise-limit-from-the-landing-branchs-dagq-toml.md)、`domain::ReviseLimit`）。
- 依頼（revise・衝突の解消）はeventを記録してから`turns/`に書くので、二重には書かない。
- 1つのrunに開いた`approve_landing`のaskは1つ。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| reviewの始まりと行き先 | `src/application/supervise/execution/`（以下同じ）の`landing.rs`の`Supervisor::begin_review`・`review_route`、`provider.rs` |
| jobの起動・やり直し・判定 | `landing.rs`の`Supervisor::tend_review`、`jobs.rs`、`src/domain/review_jobs.rs` |
| verdictごとの進め方 | `landing.rs`の`act_on_verdict`・`act_on_concern`、`src/domain/concern.rs` |
| reviseと衝突の待ち | `revise.rs`の`ReviseWatch`、回数は`src/domain/run/history.rs` |
| askとその答え | `landing.rs`の`ask_approve_landing`・`apply_landing_answers`、`src/domain/mod.rs`の`LandingAnswer` |
| 引き継ぎ | `adopt.rs` |

## reviewの流れ

番号は上の図の段で、各段の手順と記録するeventは入口のdoc commentが持つ。

2. **headless実行**: 全体のreviewはinstructionsを読ませず、pjの規則はlanding branchの`.dagq/review/builtin-acceptance.md`だけ（ADR-t2259-1決定6）。
   jobの起動・停止・引き継ぎは[Headless job processes](headless-job-processes.md#記録)。
3. **判定**: 全てのjobが答えたときだけ、判定ごとに最も重い行き先を機械で取る（`review_jobs`）。
4. **reviseの往復**: 書き直したreceiptがcleanなworktreeのHEADと一致したときだけ[Validation](validation.md#validation)からやり直す。
   落とし穴: 一致しないreceiptを渡すと`failed`で作業を失うので、HEADで書き直す依頼を送って待ち直す。
   書き直さずに終われば`approve_landing`のaskで人に聞く。
4a. **上限の後の判定**: コードの指摘が無ければ理由をfollow_upのdraftにして着地し、あればaskで示す（`Finding::is_code_finding`・`RunHistory::revise_limit`）。
5. **終了の依頼とclose**: 着地かaskの前にworkerのsessionの終了を依頼する（[非対話のworker](headless-worker.md#wrapperがturnを止めるとき)）。
   衝突の事前判定（ADR-0027決定4）で衝突すれば解消を生きたsessionに依頼して4と同じ待ちに入り、衝突だけの試行に数える（[Needs session](needs-session.md)の「試行の数え方」）。
   落とし穴: askの前に前の`approve_landing`を閉じないと、開いたaskに畳まれて新しい理由が届かず古い答えが適用されうる（`ask_approve_landing`）。
6. **askの回答**: 答えはslotの空きに依らずその場で適用し、`retry_review`はresumeせず同じreceiptを再reviewする（[ADR-t2521-1](../../adr/2026-10-17-t2521-1-retry-review-answer-reviews-a-failed-review-again-on-the-same-run.md)、`apply_landing_answers`）。
   `land`は承認の古い順に着地し、着地せずに終わったrunの閉じていない`approve_landing`はruntimeが閉じる。
7. **引き継ぎ**: 他のsupervisorがadoptして最後の段のeventから続きを決め、途中のerrorはrunをabandonして`review and integrate`のattentionにする。
8. **reviseの間のask**: `worker_question`の答えを次のturnに届け、待つrunはslotから外れる（[人の答えを待つrun](waiting.md)の「差し戻しと解消依頼の段」）。

## reviewのprovider

- 控えるのは実際に失敗したproviderだけで、起動できないjobは他のjobの答えを保ったまま待つ（[Queue hold](queue-hold.md)）。

## reviewのsubagent<a id="reviewのsubagent"></a>

[ADR-t1453-1](../../adr/2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)と[ADR-t1895-1](../../adr/2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)の実装。
変えたpathに応じて必須のagentを選び、全体のreviewのjob（`builtin:acceptance`）と並べて、agentごとに独立のjobとして動かす。

- **設定**: 書式は[Run environment](run-environment.md)、誤りは`dagq doctor`が出し、built-inの規則は[`setup-review`](setup-review.md)が作る。
- **道具の宣言**: 定義の`tools`はruntimeの一覧から選び、役割の許可を超えれば誤りにする（`AgentTools`、[ADR-t1728-2](../../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)）。
- 必須のagentとpjの定義は試行ごとにlanding branchから読み（built-inの定義はバイナリから）、workerがrun branchで変えても選ばれるagentは変わらない（`snapshot_subagents`）。
- **agentのjobのprovider**<a id="agentのjobのprovider"></a>: `provider`を書いたjobには上の「reviewのprovider」の規則をそのjobだけに当てる（`Supervisor::review_job_route`）。
- **誤り**: 必須の検査が分からないときは空にせず、起動せずにreviewの失敗にする。

## プログラムのreview<a id="プログラムのreview"></a>

段の先頭で、pathが差分に当たるprogramをagentのjobの前に流す（[ADR-t1895-2](../../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)、[プログラムのreview](review-programs.md)）。
落とし穴: scriptはworkerが書けない場所に書き出して流す（`review_programs::program_scratch`）。

## 着地の前のe2e

[integrate](integrate.md#着地の前のe2e)が持つ。

## 着地の前のagentのeval

差分が`.dagq/agents/<name>/AGENT.md`を変え、landing branchにそのagentの`evals/dev.json`があるrunは、passの後・衝突の事前判定の前にslotのままdevの周を待つ（[ADR-t1728-1](../../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)決定10、[agentのeval](../agent-eval.md)の「採用の判定（着地の前のdev）」、`eval_gate`）。

## 文書の照合

文書の照合は全体のreview（`builtin:acceptance`）だけが行う（[ADR-t2352-1](../../adr/2026-10-16-t2352-1-review-sends-back-only-documents-the-diff-touched-and-follows-up-the-rest.md)、[ADR-t2389-1](../../adr/2026-10-12-t2389-1-docs-check-only-in-the-overall-review.md)、[built-in](review-builtin.md)）。

差し戻すのは差分が変えた文書と差分の食い違いと、差分が文書の不変条件・約束・境界を破ることだけで、触っていない文書の古い記述は`follow_ups`で返し`docs_drift`のdraftにする（[Draft planners](draft-planners.md)）。

## 差し戻しの分類コード<a id="差し戻しの分類コード未実装"></a>

[ADR-t947-1](../../adr/2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)。
コードはADRなしに変えてよいが収束に数えるコードはADRで変え、コードの形でverdictを読めなくはしない（`domain::review_reason`）。

## AIが決めるconcern<a id="aiが決めるconcern未実装"></a>

ADR-t451-1決定3。

- **verdictの欄**: 推奨・確信度・人が要る理由はconcernのverdictだけが持つ（`ReviewVerdict`）。
  例外は下の`already_satisfied`の判定だけ（`domain::satisfied_judgment`）。
- 推奨が無い・人が要る理由がある・確信度が`high`でないconcernはaskにし、知らない理由は人が決める側に倒す（`domain::concern::decide`）。
- 次の回のreviewは全体のreviewもagentのjobも、そのrunの`approve_landing`に人が書いた答えを資料とpromptに持ち、答えが受け入れたconcernは答えの後の差分が範囲を広げない限り`scope`で聞き直さず、広げた部分だけをconcernにする（[ADR-t3592-1](../../adr/2026-10-29-t3592-1-run-review-reads-the-persons-landing-answers-and-does-not-ask-again.md)、`review::landing_answers`）。
- 引き継いだsupervisorは記録されたconcernを生きたものと同じに決める（`adopt.rs`）。
  ただし上限の前の`send_back`は依頼の記録が無いので、送れなかったものとして人に聞く（`adopted_concern`）。
- セキュリティのagentは[ADR-t1906-1](../../adr/2026-10-14-t1906-1-security-review-agent-findings-follow-the-concern-rules.md)。

## already_satisfiedの主張の判定

[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)決定4の判定の段で、前後と閉じ方は[Validation](validation.md#already_satisfiedの検証)、入口は`satisfied_review`のmodule doc。

- 約束: jobはsnapshotのheadで作り直したworkerが書けない木で判定し、mainが進んでいれば`high`のpassでも閉じない。
- 判定から開くaskと閉じは判定元のjobの依頼として記録する（[roles](roles.md#eventのactor)）。

## 再計画の操作が保護するrun

保護中のrunとholdの開いたtaskのrunは、走っているreviewとe2eと`already_satisfied`の判定のjobの終了を待ち、その結果でrevise・ask・着地を始めずに保留する（[taskの再計画](../../plans/task-replanning.md#実装の状態)、`replan_park.rs`のmodule doc）。
