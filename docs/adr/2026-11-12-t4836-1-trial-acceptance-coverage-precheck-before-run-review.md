---
id: adr-t4836-1
type: adr
title: runのreviewの前に受け入れ条件の被覆だけを見る軽い確認のjobを試行する
status: accepted
created: 2026-11-12
updated: 2026-11-12
accepted_on: 2026-11-12
amends:
  - adr-t1895-1 decision 2
  - adr-t1895-1 decision 3
  - adr-t2259-1 decision 1
  - adr-t2259-1 decision 4
  - adr-t2259-1 decision 5
  - adr-t2259-1 decision 6
  - adr-t2599-1 decision 1
  - adr-t2599-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - review
  - trial
related:
  - adr-t2389-1
  - adr-t3033-1
  - adr-t4005-1
  - adr-t4833-1
  - plan-acceptance-precheck-spike
---

# ADR-t4836-1: runのreviewの前に受け入れ条件の被覆だけを見る軽い確認のjobを試行する

## Context

[Spikeの推奨](../plans/acceptance-precheck-spike.md#推奨)は、receiptの機械の検査を先に入れ、意味の判断を要する形だけのtest・未接続・項目の抜けを軽いjobで見る試行を勧めた。
[ADR-t4833-1](2026-11-12-t4833-1-acceptance-items-in-receipts-and-deferral-gate-before-review.md)の検査で答えの無い先送りは戻せるが、満たしたという申告の根拠が形だけかは判定できない。
Spikeでは確認1回を全体のreviewの0.05倍にしても、a〜eを対象にした見積もりでトークンは47M増え、a・c・dと機械の検査の組では54M増える。
トークン削減を約束せず、被覆不足をreviewの前に戻せることと通過率の改善を測って残すかを決める。

## Decision

1. **確認の入力と観点を被覆に限る。**
   プログラムのreviewが全て通った後、全体のreviewとsubagentのjobを起動する前に、受け入れ条件の各項目に実装とtestの根拠があるかを確認するagentのjobを1本置く。
   読む資料はtaskの受け入れ条件、runの差分、receipt（ADR-t4833-1の項目ごとの状態と根拠を含む）だけで、差分の外のコードや文書を探さずtestも実行しない。
   見るのはSpikeの(a)形だけのtest（新しいコード同士の比較など条件が求める基準を検証しないもの）、(c)未接続（必要な入口へつながる根拠が無いもの）、(d)項目の抜けだけとする。
   差分の外を読まないと判じられないものは欠けと断定せず、通常のreviewへ渡す。
   code_defect・docs_drift・securityや一般のtest品質を検査しない。
   ADR-t4833-1の機械の検査とworkerの対応づけはそのまま保ち、答えの存在を先送りの許可や達成とは扱わない。

2. **任意のbuilt-inとして軽く動かす。**
   名前を`builtin:acceptance-precheck`とし、landing branchの設定でon/offを選び、既定はoff、このrepositoryでは試行の開始時に明示してonにする。
   通常のsubagentと並列に動かす一覧には入れず、onなら全ての差分でこの専用の段に動かし、pathでは絞らない。
   provider・model・effortはこのjob専用の設定とし、effort low相当の軽い設定で始める。
   [ADR-t4005-1](2026-11-02-t4005-1-production-ab-of-worker-and-review-provider-pairs.md)を保ち、確認のproviderと設定はworker・reviewのA/Bの群に依らず同じとし、通常のreviewのproviderの組を変えない。
   起動の隔離と読み取りだけの許可は通常のreviewのagentと同じに保つ。
   出す理由は受け入れ条件の被覆不足だけで、項目番号と差分・receiptの根拠を示す。
   projectの規則のfileを別に読み足すこともせず、汎用の被覆の確認に留める。

3. **欠けはreviewを起動せずworkerへ戻し、回数は別に限る。**
   被覆不足があれば全体のreviewとsubagentのjobを1本も起動せず、項目と理由をworkerへ渡して修正させる。
   この返却はADR-t2599-1の通常のreviewの自動の手直しには数えず、runを通した専用の回数と有限の上限を持ち、resumeやsupervisorの再起動で数え直さない。
   上限に達した後は確認による返却を増やさず、残る指摘を資料にして通常のreviewへ進む。
   確認だけを理由にapprove_landingを増やさず、上限に達したことも着地の承認とは扱わない。
   [ADR-t2599-1](2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)決定1・2の全部の手直しを数える範囲にこの例外を足すだけで、通常のreviewの数え方と上限後の判定は保つ。
   通常の上限の置き場はamended_byを辿った[ADR-t3033-1](2026-10-22-t3033-1-run-review-revise-limit-from-the-landing-branchs-dagq-toml.md)のままとする。

4. **markで区切る試行とし、期限と残す条件を決める。**
   実装の着地と自動更新後、inboxか人が有効化のmarkを残し、機械の検査だけが動く直前の期間と、確認も動く期間を`kpi --compare`で比べ、窓・母数・設定の版・結果と判断をqueueのnoteに残す。
   比較はこのrepositoryの運用規則に従ってruntimeの層で行い、切り分け不能と出た結果を採用せず、併発した変更を交絡として記す。
   初回の判定は有効化から14日後、前後の同じ長さの窓を用い、それぞれ100 run未満なら28日後まで一度だけ延ばし、同じ長さの窓で比べ直す。
   first_pass_rate・revise_rate、reviewの差し戻しのacceptance_unmetの件数とreview回数に対する割合、reviewのトークンの総量とrunあたりを記録する。
   確認のjobのトークンも加えた総量と、確認による返却の件数・型・上限到達・失敗の件数を別に添え、通常のreviewから確認へ戻し先を移しただけの改善を区別する。
   A/Bの各群と全体を示し、群の構成や切り替えの偏りを確認し、群で異なる設定を入れて効果を作らない。
   残す条件は、形だけのtestが通常のreviewのjobを起動せず戻ることをtestかfakeと試行の記録で確認でき、被覆以外の理由で戻しておらず、first_pass_rateが上がり、revise_rateとacceptance_unmetの割合が下がることとする。
   判定の基準を緩めた通過や、確認が原因の着地後の不具合があるなら残さない。
   28日後にも母数が足りない、改善が揃わない、交絡で切り分け不能、または返却の移動だけで効きが確認できないときはoffに戻す。
   トークンが増えたことだけでは外さず、増分と改善の釣り合いを示して人かplannerが継続を決める。
   目標のfirst_pass_rate ≥0.65・revise_rate ≤0.1の達成も併記するが、この試行だけで達成するとは約束しない。

5. **確認の失敗・時間切れは通常のreviewへ進める。**
   起動できない、判定が読めない、jobが失敗した、時間切れの場合は、その事実を記録して全体のreviewとsubagentのjobへ進む。
   確認がoffのときや専用の返却上限後も同じで、確認が無い現在のreviewと同じ基準を全て適用する。
   確認の通過・失敗・省略を通常のreviewのpassと見なさず、通常のreviewのjobの欠けや失敗を許す例外にも広げない。

## Amendsの範囲

[ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)決定2・3の固定の段に確認を足すが、programの順次実行と、その後の通常のagentの並列実行・機械による判定は保つ。
[ADR-t2259-1](2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)決定1・4・5・6にはbuilt-inの名前と専用の段・設定・既定off・入力の例外を足す。
そのamended_byを辿った[ADR-t2389-1](2026-10-12-t2389-1-docs-check-only-in-the-overall-review.md)の文書の照合を全体のreviewだけに置く決定と、ADR-t2391-1の境界の検査の移管は変えない。
ADR-t4005-1の集計はamended_byを辿ったADR-t4118-1・ADR-t4163-1の主・副・切り替えと環境の層を保ち、確認のために群の定義を変えない。
全体のreviewであるbuiltin:acceptanceをoffにできないことも保つ。
いずれも複数ある決定の一部だけを変えるため、置き換えでなくamendsとする。

## Alternatives

- 機械の検査だけにする: 形だけのtestと虚偽の達成の申告を見分けられない。
- 全体のreviewを先に直列にする: 通常のagentの並列を崩し、別のagentの指摘が次の回へずれる費用の符号も決まらない。
- 確認の返却を通常の手直しに数える: 通常のreviewの修正機会を減らしてapprove_landingを増やす。
- 確認の失敗や上限だけで着地を止める: 補助の確認を必須のreviewに変え、軽い試行の境界を越える。

## Consequences

実装は後続のtask 4837が行い、このADRではdocs/design/を変えない。
実装時にdesignの入口と変わる流れ・不変条件の行を足し、prompt・設定値・専用の返却上限の値・分岐の細部は定義のそばのdoc commentが持つ。
通常のreviewの基準と必須のjobを保ちながら、被覆不足の返却を早める一方、通るrunにも直列の時間とトークンが増える。
workerに全体のtest・e2eや新しい重い検査を足さず、着地後の測定とmarkの操作はinboxか人が担う。
