---
id: adr-t1394-1
type: adr
title: 人が開くplanner（dagq plan）を廃止し、計画の入口を、inboxが人の言葉を計画の依頼として記録しsupervisorが依頼ごとにruntimeのplannerを立てる移譲に一本化する（ADR-0047決定1・5・6・12・13・16・43とADR-t1300-1決定1をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amended_by:
  - adr-t1433-2
  - adr-t1487-1
  - adr-t1540-1
  - adr-t1704-1
amends:
  - adr-0047 decision 1
  - adr-0047 decision 5
  - adr-0047 decision 6
  - adr-0047 decision 12
  - adr-0047 decision 13
  - adr-0047 decision 16
  - adr-0047 decision 43
  - adr-t1300-1 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - inbox
related:
  - adr-0047
  - adr-t451-1
  - adr-t728-1
  - adr-t728-3
  - adr-t808-1
  - adr-t1228-1
  - adr-t1300-1
  - adr-t1394-2
  - adr-t1404-1
  - adr-t1091-1
  - adr-t598-1
  - design-supervisor-lifecycle-plan-planners
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-finding-planners
---

# ADR-t1394-1: 人が開くplannerを廃止し、計画の入口をinboxからruntimeのplannerへの移譲に一本化する

## Context

plannerは2種類ある。人が`dagq plan`で開き画面で人と会話するもの（origin `person`）と、supervisorがrevise・draft・findingのために立てるruntimeのplanner（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定12・16・19）。人に届くものは全てinboxに集まる（決定17）のに、計画を頼むときだけ人がplannerを開いて張り付く必要があり、inboxは開いた人のplannerに依頼のファイルを打ち込む手順（[ADR-t1228-1](2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)決定2）で代わりを務めてきた。runtimeのplannerは[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)で推奨が出せる判断を自分で決め、決めきれないものだけを`planner_question`で上げるので、人が画面に居る必要は無い。2026-10-02に人は、人が開くplannerを廃止し、計画の入口をinboxに一本化すると決めた（goal 87。goal 43の構想を今の形で取り込む）。

## Decision

1. **人が開くplannerを廃止する（ADR-0047決定1・6をamends）。** plannerはruntimeが立てるものだけにする。`dagq plan`は新しいplannerを開かず、「計画はinboxに頼む」案内を付けて拒む。`up`がsupervisorとinboxだけを開くことは変えない。廃止の時点で開いている人のplannerの行とworkspaceはそのまま動かし、その後の扱い（reviseの配送、終わったら閉じること）は今までどおり（このADRの決定9）。`up` / `down` / 固定バイナリの更新は、人がinboxか`DAGQ_ROLE`の無い自分のterminalから打つ（runtimeのplannerには打たせない）。
2. **計画の依頼を記録する（goal 87の(a)）。** 人がinboxに計画を頼むと、inboxは依頼を1件の記録にする。依頼は、人の言葉（inboxの要約でなく人の書いた文。inboxが補うなら区別して添える）、元の通知の参照（ask・task・run・event・findingのID。任意で複数）、依頼したactor、状態、結び付いたproposalを持つ。状態は`open`（plannerを待つか、plannerが作業中）・`proposed`（plannerが依頼からproposalをsubmitした）・`declined`（plannerが手当てしないと理由付きで決めた）・`exhausted`（決めずに終わったplannerが上限に達した）。依頼の中身は記録の後に変えず、言い直しは新しい依頼にする。
3. **CLI（(b)）。** 依頼の記録と一覧（1件の詳細を含む）のCLIを足す。記録できるのはinboxと人（`DAGQ_ROLE`の無いuser）だけで、planner・worker・observer・全てのjobは拒む（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)のdefault denyに新しいcapabilityを足す）。inboxが記録した依頼はactorが`inbox`で、人の言葉の代行（[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)）として残る。一覧は読むだけで、plannerを含む読み取りのroleに許す。
4. **依頼ごとにruntimeのplannerを立てる（(c)、ADR-0047決定12をamends）。** supervisorは`open`の依頼1件ごとにruntimeのplannerを1つ立てる。runtimeのplannerの同時の数の上限・二重起動の防止（立てる直前に同じトランザクションで対象の条件を確かめ直す）・決めずに終わったplannerの後に立て直すこと・1件あたり3回で`exhausted`にしてinboxにattentionを出すことは、draftのplannerとfindingのplannerと同じ規則で、上限の枠も共有する。決定12の「人が開いたplannerは数えない」は、廃止前から開いている人のplannerにだけ残る。
5. **初期prompt（(d)）。** 依頼のplannerの初期promptには、人の言葉、参照先の中身（askの問いと答え、taskとそのrunのreceiptの要約、eventの中身、findingの見立て）、関係するgoal（参照から引けるもの。無ければgoalの一覧を読む手順）、`search` / `related`で既存のtaskと実装済みを確かめる手順、ADR-t451-1の上げる基準（推奨が出せる判断は自分で決め、人が要る理由に当たり決めきれないものだけを`planner_question`にする）、repositoryの規則（AGENTS.md）を読む順を載せる。
6. **結末（(e)）。** plannerがsubmitしたら依頼は`proposed`になり、submitしたproposalを依頼に結ぶ（1件の依頼から複数のproposalを出してよい）。その後はproposalの通常の流れ（plan review、revise、concernの`approve_plan`）で進み、依頼の状態はproposalの結末で変えない。手当てしない（既に実装済み、重複、依頼が成り立たない）と決めたら、plannerは理由を付けて`declined`にし、inboxに知らせる。`proposed`もinboxに知らせ、inboxは依頼した人に結末を伝える。`declined`と`exhausted`は、人が言い直すか取り下げるかを決めるのでattentionにする。
7. **`planner_question`の行き先（(f)、ADR-0047決定13をamends）。** 依頼のplannerが作る`planner_question`は依頼に結び付ける。answerは、その依頼の生きているplannerに届け、居なければ、依頼がまだ`open`ならanswerを持った新しいplannerを依頼に立て、`open`でなければ（submit・却下・上限の後）届けずにaskを閉じる（draftのplannerのanswerの経路と同じ形で、配送の取り合いの防止と3回の上限も同じ）。決定13の「人が開いたplannerはそのworkspaceで人に聞く」は廃止前から開いている人のplannerにだけ残り、新しい計画で人に聞く経路は`planner_question`だけになる。
8. **人が直接計画を書く経路と、人のplannerを前提にした行き先（(g)、ADR-0047決定5・16・43をamends）。** 人が`DAGQ_ROLE`の無い自分のterminalで`add` / `goal add` / `submit`を打つことは今までどおり許す（plannerを開かずに書く逃げ道で、そのproposalのreviseは持ち主が居ないものとしてruntimeのplannerが受ける）。このsubmitは決定16の「人が開いたplannerからのsubmit」と同じく人の判断を経たものとして扱う。依頼のplannerはruntimeのplannerなので、ADR-t808-1の自動で採用しない上限は今までどおり効き、上限のdraftは`planner_question`の`adopt`を経る。決定5・16・43で「人が開いたplannerで人と決める」としていたもの（observerの古いdraft goal、`keep_draft`で残したdraft、draftとfindingの`exhausted`、goal closeの人の判断）は、人がinboxに頼んだ依頼（参照にそのdraft・finding・goalを付ける）か、上の人のterminalで決める。
9. **開いている人のplanner（ADR-t1300-1決定1をamends）。** ADR-t1300-1決定1（agentの終了から猶予の後に人のplannerを閉じる）は、廃止前から開いている人のplannerにだけ効き、それが無くなれば対象が無くなる。決定2（originに依らずplannerを閉じたことをeventに残す）はそのまま効くので、ADR-t1300-1はdeprecatedにせず残す。
10. **ADR-t1228-1との関係（(h)）。** ADR-t1228-1決定2が「依頼ごとに新しいruntimeのplannerを立てる経路（task 454が予約したADR-0065）」と呼んだものはこのADRである。ADR-0065は書かれず、task 453・454は取り消し済みで、goal 43のtask 453の中身はこのADRが引き継ぐ。決定2のとおり、新しい依頼の既定はこの経路にし、IDで指すplannerへの依頼のCLI（task 1229）は開いているruntimeのplannerへの続きの依頼に使う。両者は依頼のファイルをplannerのディレクトリに写して指す同じ受け渡しにそろえ、依頼の本文をキー列として打たない。人の画面の窓口（goal 48のdesk、ADR-0074は未着地）は依頼の記録を人が直接書く画面になりうるが、deskの決定はgoal 48に任せる。

依頼の表と欄名、状態の遷移のevent、attentionのkind、CLIの綴り、promptの文面、上限の数値は[`plan` / `planners`](../design/supervisor-lifecycle/plan-planners.md)に書く。

## Alternatives

- **人のplannerを残し、inboxからの依頼を足すだけにする**: 入口が2つ残り、人が画面に張り付く経路が既定の顔のまま残る。人の判断はADR-t451-1の`planner_question`で足りる。
- **inboxが自分でgoalとtaskを書く**: inboxは自分では判断しない窓口（AGENTS.md）で、計画の権限を足すと人に届くものの窓口と計画が混ざる。plan reviewとrevise・上限の規則もruntimeのplannerにそろっている。
- **依頼を記録せず、inboxがplannerの初期promptに直接書き込む**: 人の言葉と結末がqueueに残らず、二重起動の防止と3回の上限、`planner_question`の行き先を依頼に結べない。
- **人のterminalからの`add` / `submit`も拒む**: plannerもinboxも動かないときに計画を書く手段が無くなる。policyは今もuserに許しており、狭める理由が無い。
- **ADR-t1300-1をdeprecatedにする**: 決定2のplannerを閉じたことの記録はruntimeのplannerに今後も要る。

## Consequences

- 人は計画をinboxに頼み、inboxは依頼を記録して結末を伝えるだけになる。人が要る判断は`planner_question`のaskでinboxに来る。
- `dagq-planner` skillの人と話す手順、`dagq-inbox` skillのplannerを開いて打ち込む手順、AGENTS.mdの`dagq plan`の記述は後続のtaskが書き換える。
- `[roles.planner]`は新しいplannerに使われなくなる（廃止前から開いているplannerの記録には残る）。
- supervisorがClaudeのplannerを立てられないあいだ（`--no-claude`など）、依頼は`open`のまま待ち、inboxは`status`の依頼の一覧でそれを人に見せる。
- 依頼ごとの結末が数えられ、依頼から着地までの時間や`declined`の理由を読める。
