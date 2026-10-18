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
  - adr-t1582-1
  - adr-t1165-1
  - adr-t1453-1
  - adr-t1984-1
  - adr-t1895-1
  - adr-t1895-2
  - adr-t1428-1
  - adr-t2259-1
  - adr-t2352-1
  - design-supervisor-lifecycle-review-builtin
  - design-supervisor-lifecycle-review-programs
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
  → 0 プログラムのreview: 落ちたら4（agentは起動しない）、起動の失敗・時間切れは失敗
  → 1 開始: review.mdと各jobのpromptを書く
  → 2 headless実行: 全体のreviewとagentごとの読み取りだけのjobを上限の中で並列に
  → 3 判定: 全jobがそろってから。答えの無いjobはそのjobだけ1回やり直す
       pass    → 5 衝突の事前判定 → 終了の依頼とclose → 着地の前のe2e → 着地
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
- 資料`review.md`は[`review`](commands.md)、promptは[Prompt](prompt.md#headlessのjobのprompt)、providerの一般の規則は[Agent provider lifecycle](../provider-lifecycle.md)が持つ。

### 不変条件

- passでないreviewのrunは、人の`land`の答えか、jobの確かな`land`の推奨か、4aの収束なしには着地しない。
- やり直しはjobごとに1回まで、プログラムのreviewをやり直したreviewのjobはやり直さない。
- 全jobが終わるまで判定せず、答えの無いjobが残れば`pass`にしない。
- AIのjobが他のjobの判定をまとめることはない。
- verdictの`follow_ups`は判定と行き先を変えず、それだけでrunを差し戻さない。
- reviseはrunを通した上限まで返し、resumeで数え直さず、上限の後は4aで決める（[ADR-t2599-1](../../adr/2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)）。
- 依頼（revise・衝突の解消）はeventを記録してから`turns/`に書くので、二重には書かない。
- 1つのrunに開いた`approve_landing`のaskは1つ。
- e2eはhostで同時に1本だけ流れる。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| reviewの始まりと行き先 | `src/application/supervise/landing.rs`の`Supervisor::begin_review`・`review_route`、`supervise/provider.rs` |
| jobの起動・やり直し・判定 | `landing.rs`の`Supervisor::tend_review`、`supervise/jobs.rs`、`src/domain/review_jobs.rs` |
| verdictごとの進め方 | `landing.rs`の`act_on_verdict`・`act_on_concern`、`src/domain/concern.rs` |
| reviseと衝突の待ち | `supervise/revise.rs`の`ReviseWatch`、回数は`src/domain/run/history.rs` |
| 理由のコードと上限の後の判定 | `src/domain/review_reason.rs` |
| askとその答え | `landing.rs`の`ask_approve_landing`・`apply_landing_answers`、`src/domain/mod.rs`の`LandingAnswer` |
| 引き継ぎ | `supervise/adopt.rs` |
| reviewのprompt | `src/application/prompt/`の`review_prompt` |
| reviewのsubagent | `src/application/review.rs`、`src/domain/review_subagents.rs` |

## reviewの流れ

番号は上の図の段で、各段の手順と記録するeventは入口のdoc commentが持つ。

1. **開始**: 資料かpromptを書けなければ、jobを起動せずにreviewの失敗にする。
2. **headless実行**: 全体のreviewはinstructionsを読ませず、pjの規則はlanding branchの`.dagq/review/builtin-acceptance.md`だけ（ADR-t2259-1決定6）。
   jobの起動・停止・引き継ぎは[Headless job processes](headless-job-processes.md#記録)が持つ。
3. **判定**: 全てのjobが答えたときだけ、判定ごとに最も重い行き先を機械で取る。
4. **reviseの往復**: receiptが依頼より後に書き直されcleanなworktreeのHEADと一致したときだけ[Validation](validation.md#validation)からやり直す。
   落とし穴: HEADと一致しないreceiptを渡すと`failed`になって作業を失うので、HEADで書き直す依頼を送って待ち直す。
   書き直さずに終われば`approve_landing`のaskで人に聞く。
4a. **上限の後の判定**: 理由にコードの指摘が無ければ理由をfollow_upのdraftにしてpassの道で着地し、あれば`approve_landing`のaskで示す。
5. **終了の依頼とclose**: 着地かaskの前にworkerのsessionの終了を依頼する（[非対話のworker](headless-worker.md#wrapperがturnを止めるとき)）。
   - **passの衝突の事前判定**（ADR-0027決定4）: 衝突すれば解消を生きているsessionに依頼して4と同じ待ちに入り、[Needs session](needs-session.md)の「試行の数え方」の衝突だけの試行に数える。
   - **pass**: e2eが要るrunは[着地の前のe2e](landing-e2e.md)を通してから着地する。
   - **ask**: askを作る前にそのrunの閉じていない前の`approve_landing`を閉じる。
     落とし穴: 残すと開いたaskに畳まれて新しい理由が人に届かず、古い答えが新しいreviewに適用されうる。
6. **askの回答**: 答えはslotの空きに依らずその場で適用してaskを閉じ、`land`は承認の古い順に1本ずつ着地する。
   `retry_review`はresumeせず同じreceiptを再reviewする（[ADR-t2521-1](../../adr/2026-10-17-t2521-1-retry-review-answer-reviews-a-failed-review-again-on-the-same-run.md)）。
   runが着地せずに終われば、runtimeがそのrunの閉じていない`approve_landing`を閉じる。
7. **引き継ぎ**: 他のsupervisorがadoptして最後の段のeventから続きを決め、途中のerrorはrunをabandonして`review and integrate`のattentionにする。
8. **reviseの間のask**: `worker_question`の答えを次のturnに届け、待つrunはslotから外れる（[人の答えを待つrun](waiting.md)の「差し戻しと解消依頼の段」）。

## reviewのprovider

- 行き先はreviewの開始とjobの起動のたびにjobごとに決める（入口は`landing.rs`の`Supervisor::review_route`と`supervise/provider.rs`の`review_route`）。
- Codexのreviewはworktreeのprojectを信頼しない（[ADR-t1570-1](../../adr/2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)）。
- 控えるのは実際に失敗したproviderだけで、起動できないjobは他のjobの答えを保ったまま待つ（[Queue hold](queue-hold.md)）。
- 落とし穴: 引数・envの大きさによる起動の失敗はproviderの問題でないので、控えも切り替えもしない。

## reviewのsubagent<a id="reviewのsubagent"></a>

[ADR-t1453-1](../../adr/2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)と[ADR-t1895-1](../../adr/2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)の実装。
変えたpathに応じて必須のagentを選び、全体のreviewのjob（`builtin:acceptance`）と並べて、agentごとに独立のjobとして動かす。

- **設定**: 書式は[Run environment](run-environment.md)、誤りは`dagq doctor`が出し、built-inの規則は[`setup-review`](setup-review.md)が作る。
- **道具の宣言**: 定義の`tools`はruntimeの一覧から選び、役割の許可を超えれば誤りにする（`AgentTools`、[ADR-t1728-2](../../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)）。
- 必須のagentとpjの定義は試行ごとにlanding branchから読み（built-inの定義はバイナリから）、workerがrun branchで変えても選ばれるagentは変わらない（`snapshot_subagents`）。
- 範囲は試行ごとに1回決め、`review.md`も同じ範囲で書くので、mainが進んでも差分と資料は食い違わない。
- **agentのjobのprovider**<a id="agentのjobのprovider"></a>: `provider`を書いたjobには上の「reviewのprovider」の規則をそのjobだけに当てる（`Supervisor::review_job_route`）。
- **誤り**: 必須の検査が分からないときは空にせず、起動せずにreviewの失敗にする。

## プログラムのreview<a id="プログラムのreview"></a>

段の先頭で、pathが差分に当たるprogramをagentのjobの前に流す（[ADR-t1895-2](../../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)、[プログラムのreview](review-programs.md)）。

## 着地の前のe2e

e2eが要るpassしたrunは、着地スロットを待つ段でhostでe2eを流してから着地する（[着地の前のe2e](landing-e2e.md)）。

## 文書の照合

promptは`REVIEW_DOCS_CHECK`で文書の照合を求める（[ADR-t1942-2](../../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)決定4、[ADR-t2352-1](../../adr/2026-10-16-t2352-1-review-sends-back-only-documents-the-diff-touched-and-follows-up-the-rest.md)、[built-in](review-builtin.md)）。

- 差し戻すのは、差分が変えた文書と差分の食い違いと、差分が文書の不変条件・約束・境界を破ることだけ。
- 差分が触っていない文書の古い記述は`follow_ups`で返し、supervisorが`docs_drift`のdraftにする（[Draft planners](draft-planners.md)）。

## 差し戻しの分類コード<a id="差し戻しの分類コード未実装"></a>

[ADR-t947-1](../../adr/2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)。
コードはADRなしに変えてよいが収束に数えるコードはADRで変え、コードの形でverdictを読めなくはしない（`domain::review_reason`）。

## AIが決めるconcern<a id="aiが決めるconcern未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定3。

- **verdictの欄**: 推奨・確信度・人が要る理由はconcernのverdictだけが持つ（`ReviewVerdict`）。
- 推奨が無い・人が要る理由がある・確信度が`high`でないconcernはaskにし、知らない理由は人が決める側に倒す（`domain::concern::decide`）。
- 着地を許す判定は、適用される`land`のconcernを「passした」と読む（`concern::lets_land`）。
- セキュリティのagentは[ADR-t1906-1](../../adr/2026-10-14-t1906-1-security-review-agent-findings-follow-the-concern-rules.md)。

## 再計画の操作が保護するrun

保護中のrunとholdの開いたtaskのrunは、走っているreviewとe2eの終了を待ち、そのverdictでrevise・ask・着地を始めない。
runは[保留](headless-worker.md#再計画の保留)されてslotを返し、承認の列と`integrate --next`は飛ばす（[taskの再計画](../../plans/task-replanning.md)）。
