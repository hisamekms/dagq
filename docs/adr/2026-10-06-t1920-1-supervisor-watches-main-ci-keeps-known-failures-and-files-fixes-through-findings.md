---
id: adr-t1920-1
type: adr
title: 設定で有効にしたrepositoryでは、supervisorがhostのGitHubのCLIで着地先のbranchのpushのCIの結果を定期的に確かめてeventに記録し、赤になったら落ちたtestの組ごとにfindingを記録してruntimeのfindingのplannerとplan reviewで修正taskにし、既に落ちているtestの一覧を持って着地の検証の除外とworker・reviewの材料に渡す。読む手段が無ければrunの環境が名指すプログラムと同じく止めて知らせ、mainのCIの失敗のissueは人への知らせとして残す
status: accepted
created: 2026-10-06
updated: 2026-10-06
accepted_on: 2026-10-06
amended_by:
  - adr-t2034-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - ci
related:
  - adr-0047
  - adr-0049
  - adr-0051
  - adr-t996-1
  - adr-t598-1
  - adr-t1632-1
  - design-supervisor-lifecycle-ci-watch
  - design-ci-failure-issues
  - design-supervisor-lifecycle-finding-planners
---

# ADR-t1920-1: supervisorがmainのCIを見張り、既に落ちているtestの一覧を持ち、修正taskをfindingから作る

## Context

人は着地の検証を軽くし（fmt・clippy・軽い検査・unit test全件・影響範囲で絞ったIT）、最終関門をCIにすると決めた（request 43、goal 157）。CIで分かる壊れは後から直し、mainの前でCIを通す仕組みとrevertの手順は作らず、事故は許容する。そのためには、mainが赤になったことがqueueに届き、直すtaskが登録され、既に落ちているtestが他人の着地の検証とworkerの判断を巻き込まない必要がある。今のCIの失敗の知らせは、GitHubのworkflowがissueを開くだけで（[CI failure issues](../design/ci-failure-issues.md)）queueに届かず、plannerがissueを読んで登録している。

修正taskの登録の経路は2つを比べた。

- **(A) supervisorがfindingを記録し、runtimeのfindingのplannerがplan reviewを通してtaskにする**（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定18〜20）。同じ問題を1件にまとめる規則（種類・対象・subject）が重複防止にそのまま使え、plannerが既存のtask（同じ壊れを既に直しているrun）を探して重ねず、verify・paths・evidenceをrepositoryの規則で決め、plan reviewの検査（範囲・検証・所属）を外さない。代わりにplannerとplan reviewの分だけ登録が遅れ、改善のproposalの上限と優先度の上限（[ADR-0051](0051-kpi-time-series-report-and-push.md)決定25・26）が掛かる。
- **(B) supervisorが直接taskを足す**。速いが、runtimeが汎用のままrepositoryに固有のverify・paths・evidenceを決められず（定数か設定のひな形になる）、plan reviewを外すか、外さないならdraftとして結局plannerを要する。既存のtaskとの重複もruntimeが判断できない。

## Decision

1. **見張りは設定で明示したrepositoryでだけ有効にする。** 見るworkflow・branch・確かめる間隔は設定に持ち、書かなければ無効で、他のrepositoryは何も変わらない。runtimeは汎用に保ち、どのworkflowがtestを流すかとtestの結果の出し方はrepositoryの設定とCIの定義に置く。
2. **確かめ方: supervisorがhostのGitHubのCLI（gh）で、見るbranchへのpushのCIの実行のうち終わったものを、前に確かめた実行の続きから古い順に読む。** pull requestの実行は見ない。cancelされた実行（と成否の決まらない終わり方）は飛ばし、次に成否の決まった実行が範囲を広げて引き受ける。CIのworkflowのconcurrencyは変えない。runtimeは自分でCIを流さない。
   - **読む手段が無いとき**: ghが見つからないか認証が無いときは、[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定9（runの環境が名指すプログラムの検査）と揃えて止めて知らせる。`up`のpreflightは起動せず理由と対処を出し、supervisorは見張りと新しいclaimを止め、reviewを通ったrunは着地に進めず着地の列で待たせ（走っているrun・review・始まった着地は止めない）、状態が変わったときだけeventを1件記録してinbox宛てのattentionにし、戻れば記録してclaimと着地を再開する。`doctor`が今の状態を出す。claimと着地まで止めるのは、絞った着地の検証がCIを最終関門にしているので、見張りの無いまま着地を重ねると壊れが誰にも見えずに積もるため。通信の失敗のような一時の失敗は止めずに次の間隔でやり直す。workerとsupervisorはhostにツールを入れない。
3. **出来事をqueueのeventに残す。** 新しく確かめた実行の結果（commit・結論・URL・落ちたtestと外したtest）、赤になったこと、緑に戻ったこと、読む手段の有無の変化を記録する。同じ実行を2度記録せず、間隔ごとの空振りは記録しない。eventが既に落ちているtestの一覧と見張りの状態の正本で、他の表に写さない。
4. **赤になったら、その実行で新しく落ちたtestの組ごとにfindingを記録し、(A)の経路で修正taskにする。**
   - supervisorはqueueを対象に、CIの失敗の種類のfindingを、proposalを求める印を付けて記録する。subjectは新しく落ちたtestの集合（testの名前が取れない失敗では落ちたjobとstep）から作る鍵で、同じ鍵のfindingがあれば新しい行を作らず更新する（ADR-0047決定18の1件にまとめる規則。runtimeが印付きのfindingを記録する先例は[ADR-t996-1](2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)決定4）。既に一覧にあるtestは前のfindingが持つので、新しい組に入れない。
   - findingと修正taskは、落ちたtest、最後の緑から最初の赤までのcommitの範囲（cancelで飛ばした分を含む）、CIの実行のURL、今の固定バイナリ（supervisorのbuildが名乗るcommit）がその範囲を含むか（全部・一部・含まない・分からない）を持つ。plannerはそれをtaskの説明に写し、既存のtaskが同じ壊れを直していればtaskを作らず、そのtaskを直す者としてfindingに記録して閉じる。
   - **優先度**: 修正taskは改善のproposalとしてADR-0051決定26に従い`normal`以下で、決定25の上限も掛かる。一覧のtestは着地の検証から外れるので、mainが赤でも着地は止まらず、人の決定（壊れは後から直す）に合う。急ぐときは人かinboxが優先度を上げる。
5. **既に落ちているtestの一覧を持つ。** testは、見るbranchの実行で落ちたときに足し、後の実行で通ったとき（実行が成功したときは全部）に外し、足したことと外したことを3のeventに記録する。名前の取れない失敗（jobとstep）も一覧に並べ、次の成功で外す。読む口は人・inbox・scriptが読む1つの読み取り専用のCLIと、queueの状態の要約で、着地の検証の除外、workerのpromptとrunのreviewの材料はruntimeの中で同じ見え方を使う。
   - **修正taskのrunでは外さない**: runのtaskが、CIの失敗のfindingに紐づいたproposalのtaskか、4で直す者としてfindingに記録したtaskなら、そのfindingのtestを外さない一覧をそのrunに渡す（他のfindingのtestは外す）。taskの側に印を足さず、findingからの紐づきで見分ける。
6. **CIは落ちたtestの名前を機械で読める形で出す。** runtimeはCIの実行の成果物からJUnitのXMLを読み、testの名前と成否を取る。どの成果物を読むかは設定に持ち、成果物を出すことはrepositoryのCIの定義が受け持つ（このrepositoryではnextestのJUnitを成果物にする）。成果物が無い・読めない実行では、落ちたjobとstepの名前だけを使う。
7. **作らないもの**: 自動更新（固定バイナリの入れ替え）にCIの確かめや全testを足さない（自動更新は着地とほぼ同じ頻度で走るので、足すと着地の検証を軽くした意味が無くなる）。CIのworkflowのconcurrencyは変えない。mainの前でCIを通す仕組みとrevertの手順は作らない。既存のe2eの関門と固定バイナリを戻す手順はそのまま。
8. **mainのCIの失敗のissueは残す。** issueはsupervisorが止まっている・見張りが無効なrepositoryでも、GitHubの上で人に届く。見張りを有効にしたrepositoryでは、見張りが止まっている間も含めて、plannerはissueから修正taskを登録せず（見張りが戻れば4の経路が同じ失敗を拾うので重ねない）、issueは人への知らせとして残す。

## Alternatives

- **(B) 直接taskを足す**: Contextのとおり、汎用のruntimeがrepositoryに固有の検証と範囲を決められず、plan reviewと重複の判断を外すので採らない。
- **修正taskを`high`にする（ADR-0051決定25・26の例外）**: このADRは既存のADRを変えない範囲で決める。一覧のtestを外せば着地は止まらないので、まず改善のproposalと同じ扱いで動かし、段4の前後比較（mainが赤だった時間・修正taskの数）で遅すぎると分かれば、そのとき決定25・26をamendsする。
- **失敗の組をtestの集合全体にする**: 赤が続く間に1本増えるたびに、前と重なるfindingが増える。新しく落ちたtestの組で鍵を作る。
- **GitHubのwebhookやActionsからqueueへ書く**: hostのqueueに外から届く経路と秘密が要る。hostのghで読む形なら、既存の認証とsupervisorの周回だけで済む。
- **ghが無くても見張りだけを止めてclaimと着地を続ける**: 最終関門の見えないまま着地が積もる。決定9と同じ止め方（claimと着地の開始を止める）のほうが、運用の見え方も1つで済む。

## Consequences

- このrepositoryで有効にするには、固定バイナリが見張りを持った後に設定を足し（新しい設定は固定バイナリが読めるようになってから足す）、CIがJUnitを成果物にする必要がある。
- 修正taskはplannerとplan reviewの分だけ遅れ、改善の上限で待つことがある。待っている間も一覧が着地を守り、待ちはfindingの一覧に見える。
- 見張りが止まる（ghの認証切れなど）とclaimと着地も止まるので、inboxは決定9のツールの欠けと同じくすぐ人に知らせる必要がある。
- 設定の書式、eventの種類と欄、findingの鍵、一覧の読み方、ghの呼び方は[CI watch](../design/supervisor-lifecycle/ci-watch.md)が持つ。
