---
id: adr-0072
type: adr
title: dagq.tomlの間隔でruntimeが定期draftを登録し、runtimeのplannerが採否を決める
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - planner
related:
  - adr-0049
  - adr-0047
  - adr-t451-1
  - adr-t808-1
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-draft-planners
---

# ADR-0072: dagq.tomlの間隔でruntimeが定期draftを登録し、runtimeのplannerが採否を決める

## Context

2026-09-26に人は、このrepositoryのRustを完全に固定し、MSRVも同じ版に揃え、stableのreleaseごと（6週ごと）に引き上げ続けると決めた（goal 46）。hostのmiseはrust-toolchain.tomlに従い、未導入の版はrustupが入れるのでhostの設定変更は要らない。今の遅れを取り戻す引き上げと、以後の定期の引き上げを分け、後者を人の手を要さずdagqの中で回す必要がある。

runtimeやjobが作ったdraftをruntimeのplannerが検討する入口はすでにある（[Draft planners](../design/supervisor-lifecycle/draft-planners.md)、task 282・418）。時刻が来たことと、作業が必要なことは別である。Rust以外の定期の点検にも使える入口を作り、必要性の判断はplannerに任せる。

## Decision

1. **repositoryが定期の提案と間隔をdagq.tomlに宣言する。** 識別用のkey、日数の間隔、draftのtitle・description・acceptance・verification・paths・evidence・任意のgoalを持つ表を置く。runtimeが読むのはmain checkoutの作業ファイルで、runのworktreeのものではない（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定3）。設定は提案の材料であり、実行や採用の命令ではない。表の書式と検査は[Run environment](../design/supervisor-lifecycle/run-environment.md#予定-定期draft未実装)に置く。
2. **supervisorが期限の来たkeyごとにdraftを1件登録し、出どころをqueueに残す。** 出どころはrecurringとし、keyと前回の登録をdraft_originsに記録する。同じkeyの前のdraftまたは採用されたtaskがcompletedかcanceledになるまで次を登録しない。submitted・ready・実行中・判断待ち・保留中も重複を止める。間隔は前回の登録から数え、完了やcancelの時刻からは数えない。登録履歴の無いkeyは最初に読んだときに1件登録する。停止中の期限は次の起動で1件だけ取り戻し、逃した周期の数だけ登録しない。その登録時刻を次の間隔の起点にする。登録と出どころ・履歴の保存、未完了の再検査は同じtransactionで行い、引き継ぎ中にsupervisorが重なっても二重登録しない。設定から消えたkeyの履歴は残し、同じkeyの再追加や内容の編集で間隔や重複防止をリセットしない。
3. **recurringのdraftは既存のdraft plannerに渡し、採用もplan reviewを通す。** plannerは新しいstableの有無など必要性を確かめ、必要ならeditで具体的な版・受け入れ条件・検証を補ってsubmit、不要ならcancelとnoteで理由を残す。登録だけでreadyにせず、submit後は通常のplan reviewを経る。各周期は設定に基づく独立した起点なのでfollow_up_depthは0にし、周期を重ねても増やさない。そこから生じるfollow_upは元taskの深さ+1で数え、[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)の自動採用の上限（深さ3以上、および元goalや現在のgoalが無いか閉じたfollow_upには人のadoptが必要）を変えない。recurring自体をfollow_upとして扱って毎回adoptを求めることはしない。plannerの同時数・再試行の上限も既存のものを共有する。
4. **採否を人に毎回聞かず、決めきれないときだけplanner_questionにする。** [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定41と[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定1・5に従う。推奨を出せる判断はplannerが理由と根拠をcontextかnoteに残して進める。人が要る理由の分類を必須にし、範囲や方針の変更など材料で決めきれないこと、低い確信度、既存の柵が人の判断を要することだけを問う。単に間隔が来たこと、新しいstableが無いこと、定期draftの採否はaskの理由にしない。理由の分類に当てはまらない問いをaskにしない。
5. **最初の利用はRust toolchainの引き上げを42日ごとに提案する。** plannerがその時点の公式stableを確かめ、今の固定版と比べ、更新があればrust-toolchain.tomlを完全に固定した版にし、Cargo.tomlのrust-version（MSRV）も同じRustの版に揃えるtaskにする。fmt・clippy・test・CI・releaseの確認を受け入れ条件に含める。新しいstableが無ければ理由を残してcancelし、次の42日の提案を待つ。値と提案の材料は実装対応後にrepositoryのdagq.tomlに置く。goal 46を閉じても継続できるよう、恒久設定はそのgoalへの所属を必須にせず、採用するplannerが必要な開いたgoalを選ぶ。

## Alternatives

- **RenovateなどGitHubの外部bot**: dagqのintegrateを通らないPRを作るので採らない（goal 46のconstraints）。
- **時刻だけでtaskを自動採用する**: releaseの遅れや既に更新済みの場合にも不要な作業を流す。draftを登録し、必要性はplannerが確かめる。
- **止まっていた周期を全件取り戻す**: 同じ点検を何度も提案する。次の起動で1件にまとめる。
- **人が毎回登録・採用する**: 継続を人の注意に依存させる。既存のplannerとplan reviewに任せ、決めきれない問いだけを人に残す。

## Consequences

- 定期の登録と採否をdagqの記録で追え、同じkeyの未完了taskは重ならない。間隔はreleaseの検知そのものではなく、必要性を点検する頻度になる。
- queueに登録の履歴とrecurringの出どころを持たせ、設定の読み手とdraft plannerの材料を拡張する必要がある。実装と設定の配置は後続の作業で、このADRの追加だけでは登録は動かない。
- 綴り・材料・期限の計算は[Run environment](../design/supervisor-lifecycle/run-environment.md#予定-定期draft未実装)と[Draft planners](../design/supervisor-lifecycle/draft-planners.md#予定-recurringのdraft未実装)に予定として置く。
