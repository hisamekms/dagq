---
id: adr-t1975-1
type: adr
title: requestに人が指定した優先度の欄を持たせてruntimeがgoalに人の出どころの優先度として付け、goal・taskの由来を作成時にruntimeが記録し、人の出どころの優先度はAIのactorが値の変更でも所属の変更でも変えられないようruntimeが守らせる（ADR-t1971-1決定1〜4をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1971-1 decision 1
  - adr-t1971-1 decision 2
  - adr-t1971-1 decision 3
  - adr-t1971-1 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - plan-review
  - priority
  - goal
related:
  - adr-t1971-1
  - adr-t1639-1
  - adr-t1639-2
  - adr-t1504-1
  - adr-t1504-2
  - adr-0047
  - adr-0051
  - adr-t451-1
  - adr-t1091-1
  - adr-t1453-2
  - adr-t598-1
  - development-task-registration
---

# ADR-t1975-1: requestの優先度の欄を人の出どころの優先度としてruntimeがgoalに付け、goal・taskの由来を記録し、人の出どころの優先度をAIのactorが変えられないようにする（ADR-t1971-1決定1〜4をamends）

## Context

[ADR-t1971-1](2026-10-07-t1971-1-plan-review-keeps-human-origin-priority-and-membership-and-ai-tasks-inherit-goal-priority.md)は、plan reviewが由来（人かAIか）で優先度と所属の扱いを分けることを決めたが、由来はproposal単位で、requestとproposalの結び付けと持ち主から判定する。人の優先度の指定にも構造化した記録が無い。人がinterruptを指示したrequest 44では、plannerがrequestの自由文から読んだ値をgoalに書き写し、proposalの取り下げと別のplannerの出し直しで結び付けが切れ、plan reviewのpromptに人の指示が載っていたのにtaskに個別の`normal`が置かれた（人が戻した）。原因は3つで、人の指定が記録でなくplannerの書き写しであること、由来がproposalの結び付けという切れうる記録に頼ること、守る手段がpromptの指示だけであること。

2026-10-06に人がrequest 49で恒久対応の方針を決めた: `request add`に優先度の欄を足し、runtimeがその値を人の指定としてgoalに付ける。runtimeがgoal・taskの作成時に由来を記録する。人が指定した値はAIのactorが変えられず、promptは補助にする。このADRはそれを決定にする。CLIの欄の綴りと出どころの値の名前はこのADRで決めてよいと人が許した。

## Decision

1. **人の優先度の指定はrequestの欄で受ける。** `dagq request add`に`--priority`（5段）を足す。inboxと人は、人の言葉にあった指定だけを入れ、言葉に無ければ付けない（inboxやplannerが段の目安で補わない）。runtimeはrequestに値を記録し、requestの表示に出す。欄は1つの値で、人の言葉が部分ごとに違う優先度を指すときは、inboxが欄に入れずに、plannerの作ったgoalへ人の言葉どおり後から付ける（決定3で人の出どころになる）。後回しなどの別の指定は、要るときに同じ形で欄を足す（今は足さない）。
2. **requestの値はruntimeが人の出どころの優先度としてgoalとtaskに付け、plannerは書き写さない。** 優先度の値にはそれを誰の指定で置いたかの出どころを持たせ、出どころの値は`human`（人の指定）とそれ以外とする。今の優先度の出どころの表示（taskの個別・goal・既定のどれから効いているか）とは別の軸として持ち、goalの印にはしない（どの段から効いているかと誰が決めたかは独立で、一方に混ぜると他方が読めなくなるため）。値が`human`であることは`show`・`list`・`goal show`の表示に出す。優先度を持つrequestの由来（決定5）で作られたgoalには、runtimeがそのrequestの値を`human`で付ける（requestのplannerが作ったgoalと、requestを持たないreviseのplannerがそのrequestに結ばれたproposalに足したgoal）。plannerが`goal add`で値を渡さなければrequestの値を付け、同じ値なら受け、別の値なら拒否してrequestの値を名指す。同じ由来のtaskを既存のgoalに足したとき、goalの優先度がrequestの値と違うか、同じ値でも出どころが`human`でなければ、runtimeがtaskにrequestの値を`human`の個別の優先度として付ける。goalの無い単独のtaskにも同じくrequestの値を`human`の個別の優先度として付ける。これらのtaskは人間由来で、plannerがtaskに個別の優先度を付けない運用（ADR-t1971-1決定4）の例外としてruntimeが付ける。requestに優先度が無ければ、新しいgoalの優先度は今どおりplannerが段の目安で付け、出どころは`human`でない。
3. **人の出どころの優先度は、人とinboxだけが変える。** 人（actorが人）とinbox（人の言葉による操作）が置いた優先度の出どころは`human`で、両者は`human`の値も変えられ、変えた後も`human`のままにする。AIのactor（planner、plan review、runtimeの自動の修正。passでの個別の指定の外しとfindingの改善のproposalの自動の引き下げを含む）は、taskの効いている優先度かgoalの優先度が`human`のとき、それを変えられない。CLIの`set-priority`と`goal edit`の優先度の変更はactorの役割で拒み、拒否は人の指定の値を名指す。plan reviewのactionはADR-t1971-1決定3の「適用せず記録し、verdict全体は失敗させない」に乗せ、疑いは`concern`で人に上げる。AIのactorが置いた値は今どおりAIのactorが変えてよい。promptの指示は補助で、守るのはruntimeの検査とする。
4. **AIのactorの所属の変更でも人の出どころの値と保護を残す。** 個別の指定の無いtaskは所属のgoalから継ぐ（[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)）ので、`set-goal`、follow_upの`out_of_scope`の移動などgoalを変える経路でtaskを動かすだけで値が変わる。AIのactorがtaskのgoalを変え（goalを外すことを含み、そのときは`normal`を継ぐ）、そのtaskの効いている優先度が`human`なら、runtimeは移動の前の値を`human`の個別の優先度としてtaskに付けてから移動を通す。taskがすでに`human`の個別の優先度を持てば移動で変えない。値を付けずに移動先を継がせてよいのは、移動先のgoalの優先度が`human`で同じ値のときだけとする。移動先が同じ値でも`human`でなければ値と出どころをtaskに付ける（移動の後は移動先を継ぐので、AIが移動先のgoalの優先度を変えるか再び移すだけで下げられるため）。効いている優先度が`human`でないtaskの所属の変更は今どおり許す。人とinboxの移動（`correct_goal`の答えの適用を含む）は今どおり移動先を継ぐ。移動を拒まず値を残すのは、follow_upの所属の判断（[ADR-t1504-1](2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)）や重複の整理を止めずに、値だけを守れるため。
5. **由来はgoal・taskの作成時にruntimeが記録し、後から変えない。** 由来の値は`human`・`ai`・`unknown`とする。`human`は、requestのplannerが作ったもの（requestの番号も記録する）、actorが人の直接の`add`・`goal add`、inboxが人の言葉で作ったもの。`ai`は、finding・follow_up・goal_gap・reopened・draftのplannerが作ったもの。requestを持たないreviseのplannerがproposalに足したtaskは、そのproposalが結ばれたrequest（ADR-t1971-1決定2の出し直しの結び付けを含む）があればそのrequestの`human`、無ければ`ai`を継ぐ。記録はproposalの取り下げ・出し直し・別のplannerのsubmitで変わらない。taskとgoalのcontext・descriptionの文面は使わない。
6. **既存の行はmigrationで記録から埋め、決めきれないものはAIが変えない側に倒す。** 由来は、requestとproposalの結び付け、draftの出どころの記録、findingの紐づき、作成のeventのactorから判定できるものを埋め、判定できないものを`unknown`にする。既存の優先度の出どころは、最後に置いたeventのactorが人かinboxなら`human`とし、由来が`human`か`unknown`の行の値も`human`とする（request 44のように、人の指定がplannerの書き写しで入った値を守るため）。由来が`ai`で、値を置いたactorがAIだと記録から言える値だけを`human`でないとし、置いたactorが分からない値（eventの無い値を含む）は`human`とする。値と所属はmigrationで変えない。plan reviewは由来が`unknown`の行を人間由来と同じく扱い、優先度と所属を変えない。
7. **ADR-t1971-1との関係。** ADR-t1971-1決定1の判定の元を、proposal（requestの結び付け・持ち主・findingの紐づき）からgoal・taskの由来の記録（決定5）に移す。plan reviewはproposalの各task・goalの記録で人間由来かAI由来かを見て、proposal単位の判定は記録の無い行の補いにも残さない（既存の行は決定6のmigrationで埋まり、埋まらない行は`unknown`で足りるため）。ADR-t1971-1決定1の「記録から人間由来と言えなければAI由来」は`unknown`の扱い（決定6）に改める。ADR-t1971-1決定2のうち「taskとgoalに由来の欄を足す案は採らない」を改めて由来をgoal・taskに持たせ、出し直しのproposalを元のrequestに結ぶことは報告と追跡のために残す。ADR-t1971-1決定3の「人間由来では優先度を変えない」を、由来を問わず`human`の優先度（taskが継ぐgoalの`human`の値を含む）に広げる（決定3）。ADR-t1971-1決定4に、このADRの決定2のruntimeが付ける`human`の個別の優先度と、決定3・4の保護を足す。ADR-t1971-1の決定5・6・7は変えない。ADR-t1971-1の7つの決定のうち4つの一部を変え、残りの3つと各決定の大半はそのまま有効なので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)の選び方でamendsにする。ADR-t1639-1（goalの優先度が正本で、taskは個別の指定が無ければ継ぐ）は変えず`related`とし、その上に出どころの軸を足す。

## Alternatives

- **人の指定をrequestの自由文のまま、plannerがgoalに書き写す（今の形）**: 書き写しの誤りと、後のAIの変更を防げない。
- **由来をproposal単位の判定のまま、結び付けの穴を塞ぐ（ADR-t1971-1決定2）**: 結び付けは報告と追跡には足りるが、変更の検査の元にするには経路が多く、新しい経路のたびに穴が開く。
- **優先度の出どころの表示に人の値を足す（taskの個別・goal・既定に並べる）**: 人の指定がtaskの個別かgoalかが読めなくなる。
- **人の出どころの値を持つtaskのAIのactorによる所属の変更を拒否する**: follow_upの所属の判断や重複の整理が止まる。値を残せば移動を通しても守れる。
- **移動先が同じ値なら何もしない**: 移動先が`human`でなければ、後のgoalの変更か再びの移動で下がる。
- **既存の不明の行をAI由来として扱う**: 人の指定が書き写しで入った既存の値が守られず、request 44と同じ事故が残る。
- **promptの指示だけで守る**: request 44で守れなかった。

## Consequences

- 人が言葉で指定した優先度は、plannerの書き写しを経ずにgoalに入り、planner・plan review・runtimeの自動の修正で値の変更でも所属の変更でも下がらない。疑いは`concern`で人に届く。
- 由来はproposalの取り下げと出し直しを経ても変わらず、plan reviewの判定が切れうる結び付けに頼らない。
- `human`の個別の優先度を持つtaskが増え、goalの優先度を人が変えてもそのtaskには効かない。人とinboxが`set-priority --inherit`で外せる。
- runtime（requestの欄と記録、goal・taskの由来と優先度の出どころの記録とmigration、AIのactorの変更の拒否と所属の変更での値の保持、plan reviewの判定の元の切り替え）、`docs/design/`の今の姿、pluginのinbox・planner・registerの手順は後続のtaskが合わせる。このADRと同じ変更では`docs/development/task-registration.md`だけを合わせる。
