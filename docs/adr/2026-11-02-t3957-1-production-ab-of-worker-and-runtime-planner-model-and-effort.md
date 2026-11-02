---
id: adr-t3957-1
type: adr
title: workerとruntimeのplannerのmodel・effortを本番のA/Bで比べる（群はtaskかrequestのhashで決定的に割り当て、割合と候補の組は設定で受け、守る指標を超えたら人かplannerが止める。ADR-0079決定4・決定7をamends）
status: accepted
created: 2026-11-02
updated: 2026-11-02
accepted_on: 2026-11-02
amends:
  - adr-0079 decision 4
  - adr-0079 decision 7
amended_by:
  - adr-t4005-1
owners:
  - hisamekms
tags:
  - performance
  - worker
  - planning
  - measurement
related:
  - adr-0079
  - adr-t1879-1
  - adr-t1063-1
  - adr-t2257-1
  - design-supervisor-lifecycle-worker-model
  - design-supervisor-lifecycle-actor-model
  - design-measurement-commands
---

# ADR-t3957-1: workerとruntimeのplannerのmodel・effortを本番のA/Bで比べる

## Context

[ADR-0079](0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定4の限定の試しは、plan reviewの予測がmechanicalかつ下位3分の1のtaskだけを、最初のclaimの順にcontrol（Opus 5.5・medium）とtreatment（Sonnet 5・medium）へ交互に割り当てる。
割合も候補の組も選べず、既定で無効で、有効にしたrepositoryも無い。
決定7はworker以外のアクターに役割ごとの設定と差し戻しの1段上げを入れたが、群の割り当ては持たない。
使用量の上限でjobが止まるか切り替わる日があり、作業をする役割（workerとruntimeのplanner）を軽い設定で動かせるかを測りたい。
作業をする役割は評価のケースの組が作りにくいので、本番の一部を軽い群に回して比べる。
`kpi`はすでに群・model・effortの層と、runの属性の組の任意のキーの層と、人の答え待ちを除いた所要時間を持つ。
一般のA/B（実験の宣言・指示の上書き・switchback）は別の大きな計画で、まだ開いていない。

amendsにするのは、ADR-0079の決定のうち割り当て（決定4）と、worker以外のアクターの進め方（決定7）に群を足すところだけが変わり、決定1（速さが軸で、taskに由来する手戻りの率を守る指標にすること）・決定2（予測）・決定3（既定と記録）・決定5（段上げの規則と段の並び）・決定6（判定は人とplanner）は変わらないため（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。

## Decision

### 1. workerの群はtaskのhashで決定的に割り当て、taskに固定する

- 群は、試しの名前とtaskのIDを合わせたhashを割合と比べて決める（ADR-0079決定4の交互の割り当てを置き換える）。
  claimの順や並行するclaimに依らず、同じ設定なら同じtaskは同じ群になる。
- taskが一度群を持ったら、そのtaskのresume・revise・retryでは群を変えない（割合や候補を後で変えても、予測が後で変わっても）。
  hashで引くのは群をまだ持たないtaskだけで、試しの名前を変えても群を持ったtaskは群を保ち、名前はまだ群を持たないtaskの引きだけを変える。
- 対象をmechanicalかつ下位3分の1のtaskに限るかは設定で選ぶ。
  既定は今の限定のままで、限らないときも予測の無いtaskとCodexで動くclaimは試しの外にする。

### 2. workerの候補は段の上のmodelとeffortの組で、段上げは群の段から次の段へ

- controlとtreatmentの候補は、それぞれmodelとeffortの組で書く。
  providerは候補に入れない（workerの試しはClaudeだけで、Codexで動くclaimは今どおり試しの外で群を持たない）。
- 受け付ける組は、ADR-0079決定5の段（Sonnet 5 medium・Opus 5.5 medium・high・xhigh）のどれかだけで、それ以外の組は設定の検査が断る。
  段の外の組は段上げで上がらず、決定5（taskに由来する失敗の後は1段上げる）が群の中で効かなくなるため。
- taskに由来する失敗の段上げは、群の候補の段から段の並びの次の段へ上げ、群は保つ。
  上げた段はretryを含むそのtaskの後のrunに引き継ぐ（今の引き継ぎの規則のまま）。
- holdの解除がmodelを選んだtask（[ADR-t1879-1](2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)）は、今どおりその後のrunを群なしで起動する。
- 段に新しいmodelを足すのは決定5の段の変更で、このADRでは行わない。
  新しいmodelを候補にしたくなったら、先に段を変えるADRを書く。

### 3. runtimeのplannerの群はrequest単位で、requestに結び付かないplannerは試しの外

- plannerの群は、試しの名前とrequestのIDのhashで決め、同じrequestから立つplanner（最初の計画と、そのproposalの差し戻しで開き直すplanner）は同じ群になる。
- plannerが群を引くrequestは、plannerの行が持つrequest、無ければ開き直すproposalに結ばれたrequest（結ばれたものが複数あれば最小のID）で、計画の出どころ（creation origin）がreviseのplannerのrequestを読む規則と同じ順に当てる。
- requestに結び付かないplanner（draft・findingのためのplanner、requestの無いproposalの開き直し）は試しの外で、今の役割の設定で起動し、群なしとその理由を記録する。
- 候補はmodelとeffortの組で、providerは役割の設定のまま（候補に入れない）、effortは役割の設定が受け付けるeffortのどれか。
- 候補のmodelは役割のprovider（役割の設定のprovider、無ければClaude）の名前で書き、設定の検査は役割の設定の検査と同じ規則で、Codexの役割にClaudeのmodelの候補を断る。
  effortだけを変える候補はどちらのproviderでも成り立つ。
- providerが使えず設定と別のproviderで起動するplanner（[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の切り替え）は試しの外で、候補を渡さず群なしとその理由を記録する。
  modelはproviderごとの名前で、切り替え先に候補を渡せないため。
- 試しは役割のproviderの決定（runtimeのplannerをCodexにすることなど）を覆わない。
  providerを変えたら、候補はそのproviderの名前で書き直す。
- 差し戻しで開き直すplannerの段上げ（ADR-0079決定7の(c)）は群の候補のeffortの上に重ね、modelは群のまま。

### 4. 割合と候補の組はdagq.tomlで受け、既定は無効

- workerとruntimeのplannerのそれぞれについて、有効・無効、試しの名前、軽い群（treatment）の割合、controlとtreatmentの候補の組を`dagq.toml`で受ける。
- 既定は無効で、有効にしたrepositoryだけで働く（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)。dagqは汎用）。
  割合は小さく始め、広げるのは決定6の判定の後に人かplannerが決める。
- keyの名前と既定値は実装の定義のそばのdoc commentが持つ。

### 5. 見る数値は群ごとに台帳の共通の読み取りで出す

- worker: 一度で通る割合、reviseの回数、runの所要時間（台帳の区間の人の答え待ちを除いた値を並べる）、token、着地の後のCIの失敗とrevert。
- runtimeのplanner: proposalのplan reviewの差し戻しの率と、その計画から生まれたtaskのrunの手戻り（ADR-0079決定7の計画の品質の指標を群で層別する）。
- revertの証拠は、Gitを読む記録の側（mainの履歴の記録）がeventに残し、統計は台帳の共通の読み取りだけを使う（統計の側がGitを読まない）。
- 群・model・effortと試しの名前は、workerはrunの属性の組に、plannerはplannerのsessionとproposalの属性の組に記録し、`kpi`の層で読む。

### 6. 守る指標を大きく超えたら人かplannerが止める

- 守る指標はADR-0079決定1のtaskに由来する手戻りの率で、treatment − controlの差が決定1の+5ポイントを大きく超えたら、試しを止める判定をplannerか人が行う。
- runtimeは自動で止めない（ADR-0079決定6と同じ）。
  止めるのは設定を無効に戻すことで、群を広げる・全面にする判定も別に人かplannerが行う。

### 7. 一般のA/Bへ移せる形にする

- 群は台帳とkpiに専用の欄を足さず、決定5の属性の開いた組（workerはclaimのrun、plannerはsessionとproposal）に置く。
- 一般のA/Bの仕組みが開いたら、この試しはその実験の宣言の1つとして移せる。
  移すときに群の記録を読み替えずに済むよう、試しの名前を属性に一緒に残す。

## Alternatives

- **交互の割り当てを保つ**: 割合を選べず、claimの順で群が決まるので並行の度合いや再実行で割り当てが揺れる。
  hashなら割合を任意にでき、同じ設定で再現できる。
- **群をclaimごとに引く**: resumeやretryで群が変わると、taskの手戻りをどちらの群に数えるかが決まらない。
- **候補に段の外の組を許す**: 段上げが効かず、決定5の「taskに由来する失敗の後は上げる」が群の中だけ壊れる。
  段を先に変える。
- **候補にproviderを入れる**: providerの決定と試しが混ざり、使用量や控えの都合で起動先が変わったときの比較が濁る。
  providerは役割の設定のままにした。
- **plannerをplanner単位で割り当てる**: 同じrequestの差し戻しのplannerが別の群になり、計画の品質（差し戻しの率）を群に結べない。
- **requestの無いplannerにも群を振る**: draft・findingのplannerは出どころと量が違い、同じ割合で混ぜると計画の品質の比較に別の層が混ざる。
- **守る指標の悪化で自動で止める**: 群が小さい間は差の揺れが大きく（ADR-0079決定6）、誤って止めやすい。
- **一般のA/Bを待つ**: 実験の宣言・指示の上書き・switchbackまで含む大きな計画で、今ある層と記録で小さく始められる試しを遅らせる理由にならない。

## Consequences

- 試しを有効にしたrepositoryでは、割合の分のtaskとrequestが軽い設定で動き、群ごとの数値が`kpi`に出る。
  有効にしない限り挙動は変わらない。
- 交互の割り当てで群を持ったtaskはその群を保つので、切り替えの前後で群の数え方は途切れない。
- 候補は段の上の組に限られるので、新しいmodel（例: Sonnet 5.5）を試すには先に段を変える判断が要る。
- runtimeのplannerの試しは役割のproviderに従うので、providerをCodexに変えるとClaudeのmodelの候補は設定の誤りになり、候補を書き直すまでplannerの試しは働かない（設定の誤りの間の扱いは役割の設定の誤りと同じ）。
- 着地の後のCIの失敗とrevertをrunに結ぶ記録が新しく要り、その記録はmainの履歴を読む側が持つ。
- 判定の材料は揃うが、判定と止める・広げるは人とplannerの仕事のまま残る。
