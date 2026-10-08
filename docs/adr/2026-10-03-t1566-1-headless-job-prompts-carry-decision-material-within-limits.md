---
id: adr-t1566-1
type: adr
title: headlessのjobのpromptは大きさに関係なくファイルかstdinで渡し、判断の材料だけを節ごとと全体の上限の中に決まった順で載せ、一覧と全文はIDと要約にして中身はjobの権限で読めるCLIで取りに行かせ、省いた件数と読む方法を書き、promptのbyte数を記録して上限をtestで確かめる（ADR-0047決定10・39・43をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amends:
  - adr-0047 decision 10
  - adr-0047 decision 39
  - adr-0047 decision 43
amended_by:
  - adr-t2072-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - observer
  - planner
  - operations
related:
  - adr-0047
  - adr-0051
  - adr-t649-1
  - adr-t996-1
  - adr-t1063-1
  - adr-t1222-1
  - adr-t598-1
  - design-supervisor-lifecycle-prompt
---

# ADR-t1566-1: headlessのjobのpromptは大きさに関係なくファイルかstdinで渡し、判断の材料だけを節ごとと全体の上限の中に決まった順で載せ、一覧と全文はIDと要約にして中身はjobの権限で読めるCLIで取りに行かせ、省いた件数と読む方法を書き、promptのbyte数を記録して上限をtestで確かめる（ADR-0047決定10・39・43をamends）

## Context

2026-10-03の本番で、plan reviewのpromptが約1.1MBになり、macOSの`ARG_MAX`（約1MB）を超えて`Argument list too long (os error 7)`で起動できず、起動の失敗によるproviderの止めで全てのheadlessのjobが止まった。observerは2026-10-01T13:54Zから約38時間、約1.2MBのpromptで毎回起動できず、findingとKPIの見張りが止まっていた（attentionにも出なかった）。plan reviewのpromptの内訳は`ready`・`in_progress`のtaskの全文が72%、要約が12%、人が答えたaskが8%。goal reviewは150〜220KB、runのreviewは約10KB。

jobごとに材料の集め方と上限の有無がばらばらで、prompt全体の上限を持つのはスループットの見直し（task 1099）だけで、plan review（task 591）とobserverは一部の節の件数の上限しか持たなかった。渡し方の直接の修正はtask 1560が行い、このADRを待たない。このADRは人が2026-10-03に承認した共通の方針を決める。具体の上限の値・欄名・関数名は`docs/design/`に書く（ADR-t598-1決定2・3）。

## Decision

1. **渡し方: headlessのjobのpromptは、大きさに関係なくファイルかstdinで渡す。** コマンドの引数で渡さず、引数の上限で起動が落ちることを無くす。上限（決定4）に収まるpromptも同じ渡し方にし、大きさで経路を分けない。
2. **載せるもの: promptには判断の材料だけを入れる。** 一覧や全文の大量のデータは、IDと要約にする。中身はagentが状態を変えない読むだけのqueueのCLI（taskの詳細・event・集計など）で、必要なものだけを取りに行かせる（押し込まず、取りに行かせる）。
3. **取りに行く経路と読む方法は、そのjobの今の権限で実際に読める経路に限る。** jobの権限の意図と許す道具（例: observerはqueueのCLIだけ、plan reviewは読むだけのqueueのCLIとファイルの読み取り、runのreviewと復旧jobは意図としてファイルの読み取りだけ）で読めない場所への退避（jobが読めないファイルに全体を書くなど）を読む方法としない。読ませるために権限を広げることは、この方針では決めない。
4. **上限: 節ごとの件数かbyteの上限と、prompt全体の上限を持つ。** 渡し方が引数でなくなっても上限は持つ（大きな入力は費用と判断の質を損なう）。上限を超えたときにどれを残すかの順（関連の強さ、新しさ）は決まった規則で決め、LLMに選ばせない。
5. **省いたことを書く: 上限で省いたら、省いた件数とそれを読む方法（決定3の経路）をpromptに書く。** 黙って落とさない。
6. **記録とtest: jobごとにpromptのbyte数をeventに記録し、上限をtestで確かめる。** 上限を超える入力でpromptが上限に収まり、省いた件数と読む方法が載ることをtestにする。
7. **適用の範囲は、supervisorが起動するheadlessのjobとruntimeのplannerの全て**（plan review・observer・goal review・スループットの見直し・runのreview・復旧job・runtimeのplanner）。jobごとの渡し方・節・上限の値は[Prompt](../design/supervisor-lifecycle/prompt.md)の「headlessのjobのprompt」の表が持つ。今の値が決まっていないjobの上限は、そのjobの実装のtaskが決めてそこに書く。

### ADR-0047との関係

ADR-0047の決定10（plan reviewのpromptに渡すもの）・39（復旧jobに渡すもの）・43（goal reviewの入力）は、promptに載せる材料を列挙し、上限を持たない。このADRはそれらを次のように改める: 列挙した材料は判断の材料の種類として残すが、一覧・全文（決定10の「既にreadyのtaskの一覧」、決定39のrun dirのlog、決定43の所属taskのdescription・acceptanceとreceiptなど）は決定2〜5に従い、上限の中ではIDと要約に、上限を超えた分は省いた件数と読む方法に替えてよい。各決定のその他（jobの検査の内容、verdict、権限）は変えない。ADR-0047決定21（observerの起動と入力に足すもの）、ADR-0051決定24（observerがKPIを読む）、ADR-t649-1、ADR-t1222-1（Codexのobserverの書き込みの経路）、ADR-t996-1（スループットの見直し）、ADR-t1063-1（権限の意図）の決定とは食い違わない: どれも何を読むかを決め、promptにどう載せるかの上限を決めておらず、読む経路はこのADRの決定3のとおり各jobの今の権限に従う。

## Alternatives

- **引数のまま、入力を減らすだけにする**: 入力の伸び（taskやeventの数）で同じ失敗が繰り返す。上限を`ARG_MAX`に合わせて決めることになり、hostごとに値が変わる。渡し方（決定1）と上限（決定4）を分けた。
- **jobごとに別々の規則にする**: 今の形で、上限を持たないjobが残り、observerの停止に誰も気づかなかった。共通の方針を1つにし、値だけをjobごとにした。
- **上限なしで、取りに行かせるだけにする**: promptの組み立てに上限が無ければ、要約の一覧そのものが育って同じ問題になる。取りに行かせる方針と上限を両方持つ。
- **全体をファイルに書き、agentにファイルを読ませる**: queue CLIだけのjob（observer・スループットの見直し）にファイルの読み取りを許すことになり、queueの外も読めるようになる。読むコマンドで同じものが得られるので、権限を広げない（決定3）。
- **上限を超えたらLLMに要約・選別させる**: 呼び出しと費用が増え、何が落ちたかが決まった規則で再現できない。
- **上限を超えたらjobを起動しない**: 見張りやreviewが止まる。今回のobserverの停止と同じ結果になる。

## Consequences

- 入力が増えてもheadlessのjobが起動できなくなることは無くなり、promptの大きさがeventで見える。
- jobは要約から足りないものをCLIで取りに行くので、1回のjobのturnとtoolの呼び出しが増えうる。runのreviewと復旧jobは権限の意図がファイルの読み取りだけ（queueのCLIを持たない）なので、省くものはそのjobが読めるファイルにあるものに限られる。
- 実装はjobごとの後続のtaskが行う（渡し方はtask 1560、plan reviewの上限はtask 1561、observerの上限はその実装のtask）。workerのpromptの渡し方はこのADRの範囲に含めない。
