---
id: adr-t3988-1
type: adr
title: チームの版の着地は選んだマシンの報告を照合して制御側がGitHub Appでfast-forwardし、宣言した関門で人の承認を待ち、承認は同じpatch-idだけ持ち越す
status: accepted
created: 2026-11-04
updated: 2026-11-04
accepted_on: 2026-11-04
amends:
  - adr-0022 decision 3
  - adr-t451-1 decision 3
  - adr-t728-2 decision 1
owners:
  - hisamekms
tags:
  - architecture
  - integration
  - security
related:
  - adr-t3981-1
  - adr-t3985-1
  - adr-t3987-1
  - adr-t2599-1
  - adr-t3592-1
  - plan-team-version-landing
---

# ADR-t3988-1: チームの版の着地（検証の照合・mainの更新・人の関門・直列化とmerge train）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)決定6・7は、選んだマシンの検証と制御側のmainの更新を分け、PRを使わず宣言した関門で人の承認を待つと決めた。
このADRはその決定を前提に、マシンの選択と報告の照合、更新の失敗、承認の持ち越し、直列化を見直す測り方を決める。
[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定3と[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定3は、reviewが通ったrunや確信度の高い着地の推奨を人を待たずに進める。
[ADR-t728-2](2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)決定1は検証もmainの更新も制御側のIntegratorに置くので、チームの版の配置に限ってこれらの一部を改める。
具体案は[着地の設計資料](../plans/team-version-landing.md)に分け、実装時に定義のそばのdoc commentへ移す。

## Decision

ADR-t3981-1の全決定、とくに決定6・7を保つ。
このamendsはチームの版の配置にだけ効き、同ADR決定3のローカルの既定（mainへ直接着地）を変えない。
ADR-t728-2決定2・3（依頼と着地の実行を分け、Integratorが自ら検査する）、[ADR-t2599-1](2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)決定3〜6（上限の後の収束の判定と行き先）、[ADR-t3592-1](2026-10-29-t3592-1-run-review-reads-the-persons-landing-answers-and-does-not-ask-again.md)（受け入れたconcernを聞き直さない）も保つ。
関門はそれらの判定が着地へ進めた後の必要条件であり、reviewの行き先を弱めない。
承認できる人とinboxの代行を拒む境界は[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)に従い、このADRはADR-t728-3をamendsしない。
マシンの登録・lease・資格情報・imageは[ADR-t3987-1](2026-11-03-t3987-1-team-version-runners-pull-under-fenced-run-leases-ai-jobs-on-runners-pinned-images-two-github-apps-and-attributed-claude-credentials.md)に従う。

1. **検証は適格な登録マシンから優先順に選ぶ。** 常設と登録したチームのマシンを先にし、その中では必要な能力を満たす空きに割り当てる。
   使えないときは控えのself-hostedのrunner、最後にGitHub Actionsを選ぶ。
   能力・共有の登録・固定したimageを使えることを満たす候補だけを選び、単なる能力の申告は権限にしない。
   選べなければ待ち、失敗した検証を成功するマシン探しで通さない。
   設備を優先して待ちと費用を抑えつつ、控えで継続できることを選ぶ理由とし、控えを使った理由も追跡できるようにする。
2. **検証の報告を照合してからmainの更新を許す（ADR-t728-2決定1のamends）。** 選ばれたマシンが、受け入れたrunのheadをmainへrebaseし、paths・migration・reviewのプログラム・着地の検証を確かめ、squashを作る。
   報告するSは着地候補のsquash commit、Mは検証の基点のmain、treeのhashは検証した最終の内容、imageのdigestは実際に検証に使ったimageを指す。
   制御側は自ら固定した依頼・runのhead・leaseの世代・mainのM・imageと報告を照合し、GitHubからSの実在・親がMだけであること・treeの一致・候補の到達性を確かめる。
   必須の関門の成功と同じ候補への承認も照合し、欠けた報告、古い世代、内容やimageの不一致はmainを進めず拒む。
   mainの前進だけなら新しいMで検証をやり直し、不整合ならその試行を失敗として原因を調べ、単に照合を緩めない。
   制御側はrepositoryのコードもgitも動かさない。
   報告の整合性の照合は遠隔の実行そのものの暗号学的な証明ではなく、マシンの持ち主の悪意まで排除しないというADR-t3985-1の信頼の境界を保つ。
3. **mainを進めるのは制御側のGitHub Appだけにする。** 制御側は照合したSへのrefの更新を、fast-forwardのときだけ成功する操作で行い、強制の更新を使わない。
   main用のAppの鍵を検証役に渡さず、rulesetの確認とrunner用のAppとの分離も保つ。
   更新の直前にもmainと関門を照合し、Mが変われば候補を破棄して再検証する。
   更新が拒まれればmainを読み直して原因を分け、前進なら新しい基点からやり直す。
   応答が失われたときはSがmainに入ったかを先に照合し、入っていれば同じ着地として記録を回復し、二重に着地させない。
   権限やrulesetの不備は待って運用に返し、強制やPRへの迂回で解かない。
4. **関門をrepositoryの設定で宣言し、該当するrunは人を待つ（ADR-0022決定3・ADR-t451-1決定3のamends）。** 条件の種類は変更のpath・reviewの確信度low・goalの印とし、いずれかに当たればapprove_landingで待つ。
   reviewのpass、highのland、収束後の着地の判断も関門を省かない。
   既存のlowやscopeなどの人に聞く理由は、設定の省略で解除されない。
   条件は信頼するmainの設定から取り、run自身の変更で自分の関門を外せない。
   askには不変のcommit同士のGitHubの比較の画面のURLとreviewの要約、当たった条件を示す。
   承認はOIDCで識別した人だけができ、owner本人でよく、inbox・runner・AIの代行を拒む。
   two-person ruleは設定で足せ、その場合はownerと異なる承認資格のある人も必要にする。
   宣言を一か所に置くことで、人が見るべき範囲をreviewの推奨の揺れに依存させない。
5. **承認は同じpatch-idの差分にだけ持ち越す。** rebaseの後も基点から候補への差分のpatch-idを計算し直し、一致していて承認者の資格と関門の条件が今も満たされるときだけ持ち越す。
   衝突の解消・migrationの振り直しなどでpatch-idが変わるか、計算できなければ新しい比較で承認を取り直す。
   一致は必要条件であり、reviewの対象の範囲が広がったときや新しい関門が増えたときの承認の代わりにはしない。
   持ち越しでも新しいMとSの検証を省かず、ADR-t3592-1のconcernへの既存の判断は資料として保つが、変更後の差分への承認とは区別する。
   不変条件として、承認した人と承認対象のcommitをeventとsquash commitのmessageの双方から必ず追跡でき、持ち越し元から実際に着地したcommitまで辿れるようにする。
   記録を欠いた候補は着地させない。
6. **当面は着地を直列にし、検証のslotの占有と待ちを測って拡張を判断する。** 測るのは使用可能な検証のslot時間に対する占有時間と、着地待ち・再検証・失敗の内訳である。
   ADR-t3981-1決定6の利用率の基準を継続して超え始め、待ちの原因が承認やマシンの不在ではなく直列の検証にあるときに、merge trainと関係のないrunの並列の着地へ進む検討を始める。
   自動で切り替えず、測定で効果を見込めることと、各候補の検証した基点と実際のmainの一致を保てることを移行の条件にする。
   merge trainでは先行候補を含む基点で後続を検証し、先行が失敗・変更されたら後続の検証を無効にする。
   関係のないrunは依存と変更範囲の非干渉を確認したものだけ検証を並列にできるが、同じmainへのrefの更新は順に行い、基点の変化を理由に再検証を省かない。
   初めは状態と失敗時の回復を少なくし、観測された詰まりに対してだけ複雑さを増やすためにこの条件を選ぶ。

## Alternatives

- **(a) GitHub Actionsを最初に選ぶ**: マシンの管理は減るが、常設の空きを使えず費用と起動待ちが増えるので控えにする。
- **(a) 制御側で全部を再実行する／報告の成功だけを信じる**: 前者はrepositoryのコードと鍵の境界を崩し、後者は別の候補やimageを着地させうるので、固定した依頼とGitHubのobjectを照合する。
- **(b) 検証役にmainの鍵を渡す／更新をforceする**: 前者は信頼しきらない実行側に着地の権限を渡し、後者は別の着地を失わせるので退ける。
- **(c) 全runを毎回承認する／highの推奨で関門を越える**: 前者は不要な待ちを増やし、後者は宣言した人の判断を消すので、宣言した範囲と既存の人に聞く理由で待つ。
- **(c) hashが変われば必ず聞き直す／同じrunなら常に持ち越す**: 前者はrebaseだけでも承認が無駄になり、後者は変更後の中身を未承認で通すので、patch-idと資格・範囲の再照合を選ぶ。
- **(d) 最初からmerge train／pathが違えば検証なしで並列に更新する**: 前者は効果の裏付けなく回復を複雑にし、後者は基点が違う組合せを未検証で通すので退ける。

## Consequences

- PRを着地の経路にせず、CIはmainの後の最終関門のままにする。
- マシンの不在・照合の不一致・承認待ちで着地は止まり、控えにも必要な登録とimageが要る。
- patch-idは差分の同一性の条件であり、変わったmainでの意味や実行結果を保証しないので再検証が必要になる。
- この決定は未実装で、今の姿を書くdocs/designは実装の着地と同じ変更で直す。
