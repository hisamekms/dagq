---
id: adr-t4005-1
type: adr
title: 実装（worker）とreviewのproviderの組を本番のA/Bで比べる（群はtaskのhashで決定的に割り当て、reviewはrunの群に従い、集計の主は割り当てた群で副は実際に動いた組、止める判定は人かplanner。ADR-t3957-1決定2をamends）
status: accepted
created: 2026-11-02
updated: 2026-11-02
accepted_on: 2026-11-02
amends:
  - adr-t3957-1 decision 2
owners:
  - hisamekms
tags:
  - performance
  - worker
  - review
  - provider
  - measurement
related:
  - adr-t3957-1
  - adr-0079
  - adr-t813-2
  - adr-t1857-1
  - adr-t1063-1
  - adr-t1453-1
  - adr-t2257-1
  - design-supervisor-lifecycle-worker-model
  - design-supervisor-lifecycle-review
  - design-provider-lifecycle
---

# ADR-t4005-1: 実装（worker）とreviewのproviderの組を本番のA/Bで比べる

## Context

reviewの担当をCodexにした後、reviewのpassの率は下がり、Codexが利用上限で控えに入りreviewがClaudeに切り替わった期間は着地が増えた。
ただしCodexの指摘の多くは本物の不具合で、期間ごとにtaskの種類も違うので、providerだけの差とは言えない。
人の判断は、いきなり入れ替えず、実装とreviewのproviderの組をA/Bで比べ、どちらの群も「書いたproviderと見るproviderが違う」を保つこと。

[ADR-t3957-1](2026-11-02-t3957-1-production-ab-of-worker-and-runtime-planner-model-and-effort.md)はworkerとruntimeのplannerのmodel・effortをhashの群で比べるが、決定2で「workerの候補にproviderを入れない」と決めた。
reviewのproviderは役割と各subagentの固定の設定で、runの群に従う仕組みは無い。
記録は、claimの属性の開いた組、sessionの起動の記録（providerと切り替えの理由・元のprovider）、runの実際のprovider、Execution（turnとreviewのjob）のtokenの記録がある。

amendsにするのは、ADR-t3957-1の決定2の「providerを候補に入れない」に、model・effortの試しと別の試しとしてproviderの組を足すところだけが変わり、他の決定（決定1・3〜7）とADR-0079の決定1〜3・5・6は変わらないため（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
model・effortの試しの候補は今どおりproviderを含まず、providerの組はこのADRの別の試しが持つ。

## Decision

### 1. 群は2つで、変えるのはworkerとそのrunのreviewのproviderの組だけ

- A（control）: workerはtaskのprovider（既定はClaude）のまま、reviewは今の設定のまま。
- B（treatment）: workerはCodex、そのrunのreviewはClaude。
- どちらの群も書いたproviderと見るproviderが違う。
- plan review・goal review・復旧のjob・observerなどの役割のproviderは群に依らない（1度に変える項目は1つの組）。

### 2. 群はtaskのhashで決定的に割り当て、taskに固定する

- 群は、providerの試しの名前とtaskのIDを合わせたhashを割合と比べて決める。
  名前はmodel・effortの試しと別にし、2つの試しの割り当てを互いに独立にする。
- 一度群を持ったtaskはresume・revise・retryで群を変えない（ADR-t3957-1決定1と同じ規則）。
- 有効・無効と割合は`dagq.toml`で受け、既定は無効（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。
  keyの名前と既定値は実装の定義のそばのdoc commentが持つ。

### 3. 試しの外のtaskは群nullと理由を記録する

- taskのproviderがrepositoryの既定のproviderでないもの（人がCodexを名指したtaskなど）、headlessでないworkerのclaim、Spikeは試しの外で、群nullとその理由を記録する。
- 限界: 既定のproviderを明示したtaskは既定と見分けられないので、対象に入る。

### 4. model・effortの試しとは重ねて記録し、組み合わせの層は読み手が分ける

- BのclaimはCodexなので、今どおりmodel・effortの試しの外（その試しの群はnull）。
- AのClaudeのclaimにはmodel・effortの試しが重なりうる。
- 両方の試しの群をclaimの属性に記録し、kpiはどちらの属性でも層に分けられるので、組み合わせの層は読み手が分ける。

### 5. reviewはrunの群に従う

- Bのrunのreviewは、全体の判定をClaudeで起動し、subagentのうちproviderがCodexのもの（明示したものと役割の設定を継ぐもの）もClaudeで起動する。
- providerをClaudeと書いたsubagentと、Aのrunのreviewは設定のまま起動する。
- 群でClaudeにしたjobのmodelとeffortはClaudeの側の設定（無ければClaudeの既定）を使い、CodexのmodelをClaudeに渡さない。

### 6. fallbackは今の規則のままで、切り替えはjobごとの予定providerと比べて数える

- workerは今のproviderの切り替えの規則のまま（[ADR-t1857-1](2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
  workerの切り替えを止めたrepositoryでは、BのworkerはCodexの控えが解けるのを待つ。
- reviewのjobは今どおり使えないproviderから切り替える（[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)）。
  AのreviewがClaudeに切り替わるのは今も起きていることで、群の割り当ては変えない。
- 各reviewのjob（全体の判定と各subagent）の基準は、群の代表のreviewのproviderではなく、そのjobに群と設定（役割の設定・そのsubagentの設定・群の上書き）を当てたfallback前の予定providerとする。
- 切り替えは、そのjobの実際のproviderが予定providerと違うときだけで、記録ではlaunchの切り替えの元が予定providerを、切り替えの理由がその理由を持つときに当たる。
  群の上書きと、設定どおりの混在（例: Aのrunで全体の判定はCodex・Claudeと書いたsubagentはClaude）は切り替えでない。
- 予定providerを記録する前のjobは、切り替えの元があればそれ、無ければ実際のproviderを予定providerと読む。
- 記録の持ち主を分ける: 予定providerをjobごとに決めて記録するのはreviewの起動（task 4007）、jobごとの切り替えをrunの行に畳むのは台帳（task 4012）、副の判定はkpi（task 4008）が台帳の値を読むだけにする。

### 7. 集計の主は割り当てた群、副は実際に動いた組

- 主は割り当てた群のまま数える（切り替えも含む実運用の効果、intention-to-treat）。
  人の問い（着地が速くなるか）は実運用の効果で、上限に当たって切り替わること自体がproviderの組の性質だから。
- 副は実際に動いた組で、workerの実際のproviderが割り当てたworkerのproviderのままで、そのrunのどのreviewのjobも決定6の意味で切り替えていないrunだけを数える。
  reviewのjobを群の代表のproviderと比べないので、設定どおりにClaudeとCodexが混在するAのrunも副に入る。
- 群ごとに切り替えたrunの数を添える。

### 8. 見る数値と、Codexのtokenと費用の出どころ

- 一度で通る割合、reviseの回数、着地までの時間（台帳の人の答え待ちを除いた値も、Bのworkerの控えの待ちも別に）、tokenと費用、使用量の上限に当たった回数（控えと切り替え）、着地の後のCIの失敗とrevert（ADR-t3957-1決定5の記録）。
- 着地の後の不具合を一番大事にする（Bのreviewが甘いだけならここに出る）。
- Codexのtokenとeffortは、sessionの終了の記録が持たないので、Execution（workerのturnとreviewのjob）の記録を使い、sessionの終了の記録は変えない。
- Codexの費用は、evalのために`dagq.toml`にある今のCodexの単価を本番の換算にも使う。
  単価を1か所に置き、新しいkeyで旧バイナリが`dagq.toml`を読めなくなる順番の制約を避けるため。
  別のkeyが要る理由が見つかったときだけ、その理由をADRに書いて分ける。

### 9. reviewの厳しさと実装の質の交絡は別のevalで測る

- reviewの正しさは、runのreviewの全体の判定のeval（goal 246）で同じケースを両方のproviderに判定させて別に測り、この試しはそのevalを待たない。

### 10. 止める・広げるのは人かplannerの判定

- Bの着地の後のCIの失敗かrevertの率がAを明らかに超えるか、Bのworkerの控えの待ちが着地を止めるなら、plannerか人が止める判定をする。
  runtimeは自動で止めない（ADR-0079決定6と同じ）。
- 広げる・入れ替える判定も別に人かplannerが行う。

### 11. 一般のA/Bへ移せる形にする

- 群は台帳とkpiに専用の欄を足さず、claimの属性の開いた組とreviewのjobの記録に置き、試しの名前を一緒に残す（ADR-t3957-1決定7と同じ形）。
  一般のA/Bの仕組みが開いたら、その実験の宣言の1つとして移せる。

## Alternatives

- **ADR-t3957-1の候補にproviderを足す**: model・effortの段上げとproviderの切り替えが1つの試しで混ざり、どちらの効果かが分からない。
  別の名前の独立な試しにした。
- **reviewのproviderをworkerと独立に割り当てる**: 書いたproviderと見るproviderが同じrunが生まれ、人の条件（違うproviderが見る）を破る。
- **plan reviewなども群に従わせる**: 1度に変える項目が増え、差を組に帰せない。
- **集計の主を実際に動いた組にする**: 上限で切り替わるrunを除くと、実運用で起きる控えと切り替えの費用が見えず、着地の速さの問いに答えない。
- **切り替えを群の代表のproviderと比べて判じる**: 設定どおりの混在（Claudeと書いたsubagent）を切り替えと誤って数え、Aのrunの多くが副から落ちる。
- **Codexの単価に本番用の別のkeyを置く**: 同じ値が2か所に分かれ、新しいkeyを読めない旧バイナリとの順番の制約も生む。
- **reviewのevalを待つ**: 本番の着地の速さと不具合は本番でしか測れず、evalは別に進む。

## Consequences

- 試しを有効にしたrepositoryでは割合の分のtaskがBで動き、群ごとの数値が`kpi`に主と副で出る。
  有効にしない限りclaimとreviewの起動は変わらない。
- workerの切り替えを止めたrepositoryでは、Codexの控えの間Bのtaskの着地が遅れ、それも主の数値に入る。
- reviewの起動はjobごとに予定providerを決めて記録する必要があり、台帳はjobごとの切り替えをrunに畳む読み取りを持つ。
- Aのrunにmodel・effortの試しが重なるので、組み合わせの層を読むときは群が小さくなる。
- reviewの厳しさの差はこの試しだけでは分けられず、別のevalの結果と合わせて読む。
