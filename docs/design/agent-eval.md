---
id: design-agent-eval
type: design
title: agentのeval（定義とケースの置き場・ケースの欄・採点・漏れの検査・依頼と認可・記録・費用の上限・hold-outの1回の制限・evalの枠・本番と共有する起動経路・道具の宣言・採用の判定・productionのケースと見張り・programのreview）
status: current
created: 2026-10-06
scope: runtime
related:
  - adr-t1728-1
  - adr-t2259-1
  - design-supervisor-lifecycle-review-builtin
  - adr-t1728-2
  - adr-t1453-1
  - adr-t1895-1
  - adr-t1895-2
  - adr-t1570-1
  - adr-t1470-1
  - plan-review-agent-eval-spike
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-run-environment
  - design-queue-service
  - design-authorization
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle-headless-job-processes
  - adr-t1869-1
  - adr-t2599-1
  - adr-t2229-1
---

# agentのeval

reviewのagentが正しく判定しているかを、規則コードごとの陽性・陰性のケースと閾値で測る仕組み。
誰か（人・inbox・plannerか、自分のrunのdevだけはworker）がqueueに依頼を記録し、supervisorがlanding branchの定義とケースで1周を実行して、成績をeventに記録する。
workerもjobもLLMのCLIを直接打たない。
1周の中のケースごとの1回は、本番のrunのreviewのagentのjobと同じ起動経路で、測るagentの1本のjobだけを起動する。

> 欄・flag・既定値の意味は定義のそばのdoc commentが持つ。

決めた理由は[ADR-t1728-1](../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)（定義とケースの置き場・eval）と[ADR-t1728-2](../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)（道具の宣言）、定義を切らない例外は[ADR-t1869-1](../adr/2026-10-09-t1869-1-agent-jobs-carry-the-whole-definition-and-do-not-start-over-the-limit.md)、材料は[review-agent-evalのSpike](../plans/review-agent-eval-spike.md)が持つ。

## 定義とケースの置き場

- 定義: `.dagq/agents/<name>/AGENT.md`。形は[ADR-t1453-1](../adr/2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)の定義と同じ（frontmatterの`description`と本文の検査項目と参照先の文書）に、道具の宣言の`tools`（下の「道具の宣言」）を足したもの。`<name>`はkebab-case。
- ケース: `.dagq/agents/<name>/evals/`の`dev.json`・`holdout.json`・`production.json`。
  splitはこのファイルで区分し、ケースの欄に持たない。
- patchの共有の置き場: `.dagq/agent-cases/patches/<sha256>.patch`（patchの内容のSHA-256。agentの名前と衝突しないよう`.dagq/agents/`の外に置く）。同じ内容のpatchは1つだけ置き、ケースはhashで参照する。どのケースからも参照されないpatchと、ケースが参照するhashのpatchが無いことは、下の設定の検査が誤りにする。
- どこで走るか: `dagq.toml`の`[review.subagents.<name>] paths`のまま（形は変えない）。定義は`.dagq/agents/<name>/AGENT.md`だけから読み、そこに無ければ定義が無い扱いで、reviewはpassにしない（[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「設定」）。
- 設定の検査（`dagq.toml`を読むとき）: `[review.subagents.<name>]`が名指す`<name>`に`.dagq/agents/<name>/AGENT.md`が無いこと、同じ`<name>`を複数の役割の表が名指すこと（今の役割の表は`[review.subagents]`だけ。役割の表が増えたら同じ検査に足す）を誤りにする。
  この2つは実装済みで、`dagq doctor`の`agents`の`errors`が出す（[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「設定」）。
- ケースのファイルの形とpatchの置き場の参照の過不足（上）は、ケースの一覧の読み手と、このrepositoryの全ての一覧をそれで読むtestが検査する。
  `dagq doctor`と設定の読み込みにはまだ足していない。
- built-in（security・tests）の定義とケースはバイナリが持つ（`domain::review_builtins::evals`のmodule doc）。
  `builtin:acceptance`はverdictがagentのjobの形と違うので対象にしない。

## ケースの欄

ファイルの形と欄の意味は`domain::agent_eval`の`CaseFile`と`Case`のdoc commentが持つ。

- `handmade`はdevだけに置く。改善するsessionが中身を読んだケースはholdoutに置かない。
- baseは`base_commit`か、built-inの同梱のケースだけの`baseline`の一方。
- 読み手は形の誤りを、最初の1つでなく全て返す。
- 起動・形の検査・採点・productionのケースの作り方は役割ごとのharnessが持つ。今作るのはreviewのharnessだけ。
  役割が増えたら、その役割のharnessを足す。

## 採点

reviewのharnessは、1周の実行（ケース × kの1回）を単位に数える。

- 判定: `revise`と`concern`を違反あり、`pass`を違反なしとする。
  期待が`violation`で違反あり＝TP、`violation`で違反なし＝FN、`clean`で違反あり＝FP、`clean`で違反なし＝TN。
- 規則コード: 違反ありの結果の理由が挙げた規則コードを、コードごとに期待のコードと比べる。
  期待のコードを挙げた回＝TP、挙げなかった回＝FN、期待にも`acceptable_codes`にも無いコードを挙げた回＝FP（`acceptable_codes`は挙げても誤検出に数えない）。
  agentの値はコードごとの和から出す。
- recall＝TP /（TP＋FN）、precision＝TP /（TP＋FP）。
  分母が0なら値なし。
- 判定の無い実行（完了しない・結果が読めない）はerrorで、判定にも規則コードにも数えない。
- `disputed`のケースとその実行は主の指標から外し、含めた値は別に返す。
- 閾値: 判定と規則コードのrecall・precisionの4つがすべて閾値以上で、errorが無いときだけ通る。
  値なしは下回ったと数える。
  閾値の値は、pjのagentは`[eval]`の設定（`EvalConfig`のdoc comment）、built-inは同梱の値（`review_builtins::evals::THRESHOLD`のdoc comment）。
  規則コードごとの値も返すが、採用の判断はagentの値で行う。

## 漏れの検査

定義がケースに合わせて書かれていないかを見る。
誤りにはせず、見つけたケースと語を返す。

- ケース固有の語: ケースのpatchが変えたpathとファイルの名前、変わった行のADRのIDとbacktickで囲んだ名前のうち、普通の語でない固有の名前。
- 規則の語: 定義がlinkする規則の文書（landing branchのcommitのもの）とそのpathに現れる語。
  ケース固有の語でも規則の語なら漏れにしない。
- 漏れ: 規則の語でないケース固有の語が、1つの語として定義に現れたもの。

## 依頼と認可

- 入口: `dagq agent eval <agent>`が依頼を記録し、`dagq agent results`と`dagq agent result <id>`が周を読む。
  どれもqueue serviceのユースケースで（[Queue service](queue-service.md)）、client modeのworkerも使える。
  use caseは`application::agent_eval`の`AgentEvals`。
- 依頼は記録するだけで、実行はsupervisorが行う。
  supervisorは依頼を実行するだけで、自分では依頼しない（見張りは時期をfindingにするだけ。下の「採用した定義の見張り」）。
- 依頼の認可と実行を分ける: devはworker（自分のrunの分）・人・inbox・planner、hold-outとproductionは人・inbox・planner、同じキーの再実行は人とinboxだけができる。
  読み取りはworkerには自分のrunの分だけを見せる。
  capabilityの表は[Authorization](authorization.md)が持ち、要るcapabilityは`domain::authorization::eval_request_needs`が決める。
- hold-outとproductionの周は集合の全体だけを流し、`--cases`は人の再実行の指定のときだけ受ける（[ADR-t2229-1](../adr/2026-10-21-t2229-1-held-out-rounds-run-their-case-set-whole.md)）。
  記録に残った依頼も周の開始で拒む。
- queue側のproductionの集合を選ぶ周のケースの集合のdigestは、選んだ集合の全体のもの（作り直しで飛ばしたケースがあればその残り）になる。
  周は開始の時の集合（ラベルと人の決定）で採点し、引き取ったsupervisorも同じ時点までのeventで集合を読み直す（`record::Started::set_through`）ので、開始の後の人の決定は引き継ぎの有無に依らずその周を変えず、次の周から効く。
- productionの集合の作成の依頼（`agent cases`）と人の決定（`agent decide`）は人・inbox・plannerの`eval.cases`で、workerとsupervisorは`authorization_denied`になる。
  supervisorは依頼された集合を作るだけで、自分では依頼しない。
- repositoryの一覧のケースのラベルの変更と`disputed`の解除はコマンドを持たず、人とinbox・plannerがtaskで行う。
- `builtin:<名前>`の認可はpjのagentと同じで、周は汎用の品質を測るためpjの規則のfileとprogramを当てない。

## 記録と成績の作り直し

- 依頼・周の開始と拒否・providerを待つこと・実行ごとの開始と終わり・成績は、全てqueueのeventで、周の状態はeventだけから作り直す（`domain::agent_eval::record`）。
  別の表を持たず、supervisorが入れ替わっても同じ状態を読む。
- incompleteの周は閾値を超えてもpassに数えない。
- 版ごとの成績はeventだけが持ち、repositoryに置かない。

## 費用の上限

- 入口: `domain::agent_eval::round`、設定は`dagq.toml`の`[eval]`と`[eval.providers.<provider>]`（keyと既定値は`EvalConfig`・`ProviderCost`のdoc comment）。
- 不変条件: 周は起動の前に見積もり（予定の実行の数 × 1回の見込み）、回数か見積もりが上限を超えれば起動せず理由を記録する。
  起動した周は見積もりを予約として記録し、次の実行の前に、使った額と実行中の見込みと次の1回の見込みの和が上限を超えるなら新しい実行を起動せず、incompleteで閉じる。
- 使った額は実行ごとに確定する: providerが返す金額、返さないproviderはtokenに単価を掛けた額、どちらも無く終わった実行はその見込み。
- 落とし穴: 金額を返さずtokenの単価も無いproviderの周は、既定の見込みがあっても使った額を確定できないので起動しない。
  1回の見込みが無い周も起動しない。
- 上限なしで流す経路は持たない。

## hold-outの1回の制限

- キーはagent・定義のdigest・ケースの集合のdigest（`round::case_set_digest`）で、hold-outとproductionの周に当てる（`holdout_refusal`）。
- 判定はeventに記録した開始した周のキーから読み、別の状態を持たない。

## evalの枠

- evalはrunのslotを使わず、claim・着地の順・superviseの回ごとのrunの本数を変えない。
  本番のreviewはrunのslotの中で動き、evalを待たない。
- queueごとに同時に1周だけ流す（流れている周は開始があって終わりの無いもので、eventから読む）。
  周の持ち主はそれを始めたか引き取ったsupervisorで（そのtokenをeventに書く）、同じqueueのほかのsupervisorは持ち主が生きている間は触らない。
  周の開始と引き取りはそれぞれ1つの書き込みのtransactionで、2つのsupervisorが同じ周を始めることも引き取ることもない（`EvalRounds`）。
  1周の中のproviderのprocessの同時数は設定の上限まで。
- 待っている周は、着地の前のdevを先に、依頼の古い順に流し（`round::next_round`）、流れている周は止めない。
- evalのjobはrunのreviewのjobの同時実行の上限（`ReviewJobs::concurrency`）に数えず、runのreviewのjobはevalの枠に数えない。

## 採用の判定（着地の前のdev）

入口は`application::supervise::execution::eval_gate`、対象と行き先の判定は`domain::agent_eval::before_landing`。
built-inの定義の変更には広げない（定義がdagqのsourceにあり、見るpathがdagqの開発の事情になるため）。

- 対象: runの差分が`.dagq/agents/<name>/AGENT.md`を変えるrun（差分の取り方はreviewのagentの選び方と同じ`<base>...<head>`）。変えたagentごとに1周。
  landing branchのcommitに`evals/dev.json`が無いagent（ケースがまだ無い）と、runが定義を消したagentは流さず、理由を`run_agent_eval_requested`の`skipped`に残す。
  定義を変えないrunは何も記録しない。
- reviewのpassの後、衝突の事前判定の前に、supervisorが依頼を記録し（`before_landing`、定義のcommitはrunのhead）、周はlanding branchのcommitの`evals/dev.json`とpatchとprogramでrunの定義を測る。
  run branchのケースの追加・変更は使わない（`agent_eval_started`の`cases_commit`）。
  依頼はheadごとに1回で、同じheadを引き継いだsupervisorは同じ周を待つ。
- 待つ間と流す間、runはsessionを開いたまま自分のslotとleaseを持つ（e2eと同じくslotを放さない。差し戻しを生きているsessionに送るため、sessionを閉じる前に待つ）。
  待ちの理由は`run_agent_eval_waiting`で、`status`の`progress`の`agent_eval_waiting`・`agent_eval`に出る。
- 行き先は全部の周の終わりで決め、`run_agent_eval_finished`に記録する。
  全部が`passed`なら衝突の事前判定へ進む。
  完了した周が閾値を下回れば、成績と失敗したケースのidとagentの理由をreviewの`revise`と同じ経路でsessionに差し戻し、runを通した自動の手直しの上限に1回と数える。
  上限を使い切った後の差し戻しは理由のコードを持たないので[Review](supervisor-lifecycle/review.md#reviewの流れ)の4aで着地せず、`approve_landing`のaskになる（[ADR-t2599-1](../adr/2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)決定1・3・6）。
- workerが直せないもの（`refused`の`cost_unknown`・上限超えなど、`incomplete`、判定の無い実行、`--no-claude`かagentが無くて使えないprovider）と、止まるsupervisor（引き継ぎでないもの）は、理由を載せた`approve_landing`のaskにする。
  控え中のproviderは待つ。

## productionのケースと見張り

本番のreviewからケースの集合を作り、queue側に置いてhold-outとして流す。
入口は依頼と読み取りが`application::agent_cases`の`AgentCaseSets`、作るのが`application::supervise::execution::agent_cases`、eventから集合を畳むのが`domain::agent_eval::production`（eventの種類と欄はそのdoc comment）。

- 1集合ずつ、runのslotの外で、古い依頼から作る。
  集合を作るsupervisorは1つで、取るのも居なくなった持ち主から引き取るのも1つの書き込みのtransaction（`EvalRounds::take_case_set`、`agent_cases_taken`）。
  ほかのsupervisorは持ち主が生きている間その集合に触らないので、2つが同じケースにjobを起動したりtreeを共有したりしない。
  引き取ったsupervisorは、前の持ち主のjobが引き継ぎで止められた後、ラベルの無いケースを付け直す。
- 準備: landing branchのcommitの定義とケースの一覧から、規則コード・`k`・規則の文書のpathと、規則コードごとの規則の場所の対応を決め（`production::frame`）、期間の`review_started`（`subagents`がそのagentを選んだもの）と同じrunとattemptの`review_finished`（そのagentの判定）から、`<base>...<head>`の差分を内容のhashで集合のpatchに書く。
  baseかheadのcommitがrepositoryのどのrefからも辿れないreviewは、objectが残っていても（branchを消した後、GCの前）理由つきで飛ばす（`Repository::reachable_commit`）。
  差分が読めないreviewも理由つきで飛ばし、1つのreviewが集合と後の依頼を止めない。
  reviewが多すぎる依頼は作らず理由を記録し、人・inbox・plannerが期間を狭めて依頼し直す（上限は`application::supervise::execution::agent_cases`のmodule doc）。
- ラベル: ケースごとに、`[roles.review]`と別のprovider（reviewがClaudeならCodex）のagentのjobを1本起動する。
  起動経路はevalのagentのjobと同じ（下の「evalが本番と共有する起動経路」）で、ケースのtreeからは全てのagentの定義とevalのケースを消す。
  jobの「定義」は規則の文書のpathと、規則コードごとの規則の場所だけ（`production::label_definition`）で、定義の検査項目の文は渡さない。
  コードと場所の対応はケースを作る側がケースの一覧に書き（`CaseFile::rules`）、一覧の対応は改善する定義から作らない。
  一覧に無いコードだけ、定義の行のうちそのコードを名指す行のlinkの先（文書と節）を使い、定義の検査項目の文は使わない。
  対応はlanding branchのcommitから読むが、jobが読む規則の本文はケースのtree（reviewのbaseにその差分を当てたもの）の文書で、landing branchの文書ではない。
  このため本文はそのreviewの時の規則で、差分が規則の文書を変えていれば変えた後の本文になる。
  providerが使えない間はjobを起動せず待ち、壁で止まったjobはproviderを控えてそのケースをやり直す。
- 組み立て: 全てのケースのラベルのjobが終わったら（判定の無いjobと起動できなかったjobはラベルの無いケースとして理由を記録する）、ラベルの付いたケースだけで集合のファイルを作り、digestを`agent_cases_built`に記録する。
  ラベルの無いケースと作り直せないケースは集合に入れず、`agent_cases_built`に理由を残す。
  ラベルと本番の判定が食い違うケースは`disputed`の候補で、1つの`decide`のask（最初の候補のreviewのtaskに付く）が人の決定（`agent decide`）を求める。
  決定はラベルになり`disputed`を外す。
- 集合のファイル（queueのdirの`agent-cases/<set id>/`の`cases.json`と`patches/`）はeventとgitから作り直せるビューで（`application::agent_cases::view`）、`agent eval --set`の周はこれを作り直してから読む。
  作り直しは、まずbaseかheadがrefから辿れないケースを、patchのファイルが残っていても理由つきで飛ばす（ケースのtreeはbaseから作り、ビューはファイルの有無で変わらない）。
  残りのケースは内容のhashが合うpatchのファイルを使い、無ければ差分から書き直し、読めない・内容が変わった・UTF-8の文でない差分のケースは理由つきで飛ばす。
- 集合をhold-outとして1回流した後、repositoryの`production.json`に足すのはtaskで行う。
- built-inの集合はそれを選んだreviewから作り、ラベルのjobには同梱の定義の本文を規則として渡す。

### 採用した定義の見張り

landing branchの定義を新しい本番のreviewで測り直す時期をsupervisorが判断し、findingにする。
判断は`domain::agent_eval::watch`、記録は`application::supervise::observation::agent_watch`（間隔と値は`[eval]`の`watch_new_reviews`・`watch_interval_days`、`EvalConfig`のdoc comment）。

- supervisorは依頼しない: 集合の作成もhold-outも、findingで開かれたruntimeのplannerが依頼者として依頼し（[Finding planners](supervisor-lifecycle/finding-planners.md)）、実行は今までどおりsupervisorが行う。
  plannerの手順は見張りのfindingのplannerの初期prompt（[Finding planners](supervisor-lifecycle/finding-planners.md)の4）とpluginの`dagq` skillの`reference/observer.md`が持つ。
- 対象と時期: 働いているsupervisorが一定の間隔で、landing branchのcommitに`evals/`のファイルを持つagentごとに、新しい期間にそのagentを選んで判定の付いた本番のreview（`production::candidates`と同じく開始の時刻で数える）を数える。
  間隔は最後の見張り（`due`のfinding・productionの周の開始・新しいreviewの無かった周の記録）から数え、期間は前の期間の終わり（`due`の`until`、集合の周はその集合の`--until`、周の無かった記録の`through`）から始まる。
  期間の終わりは今か、まだ終わっていないそのagentのreviewのうち最も早い開始の前（上限の時間を過ぎたものは数えない）で、見張りの間に走っていたreviewは終わった後に次の期間で数える（`watch::period_end`）。
  件数が設定に達したか、間隔が過ぎて1件以上あれば`due`のfindingを記録する。
  間隔が過ぎて1件も無ければfindingを作らず`agent_eval_watch_skipped`に理由と期間の終わりを記録する。
  同じagentの見張りのfindingが開いている間は何も記録しない。
- findingの段: どれも種類`agent_eval_watch_due`・対象queue・`--propose`付きで、subjectが段と対象を名指し、同じsubjectは状態に依らず1回だけ記録する（`record_finding_once`。2つのsupervisorでも1つ）。
  `due`は期間と依頼する`agent cases`を、`holdout`はplannerが依頼した集合が作られて周が無いことと依頼する`agent eval --set`を、`refused`はplannerの集合の周が始まらなかった理由を持つ。
  `refused`はplannerが理由を書いてdismissする（再実行はplannerの権限に無い）。
- 成績のfinding: hold-outかproductionの周（着地の前のdevでなく、定義はlanding branchのもの）が全ての実行を終えて閾値を下回れば、依頼者に依らず種類`agent_eval_below_threshold`のfindingを記録し、成績・下回った値・失敗したケースのidとagentの理由を持たせる。
  findingにするのは周の定義のdigestがlanding branchのheadの`.dagq/agents/<name>/AGENT.md`の今のdigestと等しい周だけで、定義が変わった後の周や定義の無いagentの周は記録しない（採用していない定義の改善をplannerに頼まない）。
  `AGENT.md`が読めなければfindingを作らず、理由をsupervisorのlogに残して次の見張りで判定し直す。
  runtimeのplannerが定義の改善のtaskにし、ラベルの食い違いは集合の`disputed`と同じく人に回る。

## 道具の宣言

今の姿の詳細は[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「道具の宣言」が持つ。

- frontmatterの`tools`: runtimeが持つ道具の一覧の名前のリストで、無ければ役割の既定（`AgentTools::declared`）。
  `tools: []`は道具を持たない宣言。
- runtimeの道具の一覧は`domain::review_subagents::AgentTool::ALL`で、役割が許さない道具と一覧に無い名前とリストでない`tools`は定義の誤りにする（`AgentTools::declared`）。
  誤りの定義を選んだreviewはsnapshotが誤りにしてADR-t1453-1決定6の経路でpassにせず、`dagq doctor`の`agents`の`errors`にも出す。
- providerごとの変換（ADR-t1895-1、`AgentProvider::agent_job_command`が使う）:
  - Claude: jobの起動の引数で宣言した道具を許し、一覧の残りを拒む。
  - Codex: read-onlyのsandbox・jobのpermission profile・worktreeのprojectの`untrusted`（ADR-t1570-1）は変えず、狭めるだけ。
- 当てないもの: 全体のreviewのjobは定義を持たないので対象にしない。

## evalが本番と共有する起動経路

- ケースごとの1回は、本番のrunのreviewのagentのjobと同じ経路で、測るagentの1本のjobだけを起動する。
  経路はheadlessのjobの共通の経路（[Headless job processes](supervisor-lifecycle/headless-job-processes.md)）、1本のagentのjobの組み立て（`application::agent_job::build`、[Prompt](supervisor-lifecycle/prompt.md#agentのjobの上限)）、上の「道具の宣言」の変換、行き先のproviderは`[roles.review]`。
  eval専用の起動の組み立ては作らない。
- 本番のrunのreviewのagentのjobも同じ組み立て・起動のコマンド・1つのverdictの読み取りを使う。
  ケースのjobは組み立ての入力の読める場所の欄（`AgentReads`）を持ち、本番のrunのreviewのagentのjobはその欄を空にする。
  同じ定義・provider・道具の宣言なら、argvの差はその欄からproviderが作る読み取りの制限の引数だけで、stdinのpromptの差は材料の行だけである。
- 実行の入口は`application::supervise::execution::agent_eval`。
- ケースのtreeは、queueのdata dirの`agent-evals/`の下に周ごとに置く、tree自身のGitのrepositoryである。
  `base_commit`のケースは、supervisorが`base_commit`にpatchを当てたcommitをrepositoryの中に作り、材料のファイル（commitの一覧と全文の差分`<base_commit>...<patchを当てたcommit>`）をそこから作って、ケースのdirの材料だけのdirに書く。
  `baseline`のケースは、treeの場所に独立のrepositoryをbaselineとpatchから作り、対象のrepositoryを読まず変えない。
  treeはbaseとそのcommitのファイルからevalのケースの一覧とpatchの置き場を除いたもので、それぞれを1つのcommitにし、baseのcommitをlanding branchの名前に、そのcommitをdetachedの`HEAD`に置く（ほかのrefは無い）。
  landing branchと比べるprogramのreviewには、runのworktreeと同じくケースの差分が見える。
  treeのGitはrepositoryとobject・ref・Git directoryを共有しないので、treeの中から辿れるどのcommitにもケースの一覧・patchの置き場・期待は無い。
  採点の期待はsupervisorがtreeの外で持つ。
- jobのcwdはそのtreeで、Codexのjobが読めるのはtree・ケースの材料のdir・commandの実行に要るsystemの場所だけである。
  repositoryのcheckoutとGit directory、queueのdata dirのほか（ほかのケースと実行の出力、productionのケースの集合）は読めない。
  Claudeのjobは読める場所の欄を使わず、`--add-dir`でtree・材料のdir・自分のjobのdirを足す今の起動のままである（shellを宣言した定義のcommandは限らない）。
  Codexのjobは、`--sandbox read-only`の代わりに、読める場所の欄から作るjobのpermission profileの読み取りの設定で限る（`infrastructure::codex::agent_reads_config`）。
  書き込みとnetworkはread-onlyのsandbox・jobのpermission profile（ADR-t1233-5）・`untrusted`（ADR-t1570-1）のまま変えない。
  ケースのdirは番号で名付け、promptにもpathにもケースのidと期待を出さない。
- providerのloginか使用量の上限で止まった実行は、providerを控えて同じ実行を待たせ、やり直しに数えない。
- 起動を止めたsupervisor（drain・stop・serviceの停止）は、流れている実行が終われば周を手放し、次に動くsupervisorがeventから読み直す。
- providerを切り替えない: 成績・1回の見込み・tokenの換算がproviderごとで、切り替えると別のproviderの成績が混ざるため。
  `[roles.review]`のproviderが控え中か`--no-claude`で使えない間は、新しい実行を起動せず理由を記録して待ち、流れている実行は止めない。
- supervisorの再起動: 持ち主が居なくなった周だけを引き取り、前のsupervisorのevalのjobは引き継ぎで子孫ごと止めて記録し、周をeventから読み直して、終わっていない実行をその見込みで使った額に数えてから費用の上限の判定を通して起動し直す。
  走っているprocessを引き継いで続ける形にはしない。
- 使わないもの: 全体のreviewのjob。

## programのreviewの当て方

- evalは各ケースに本番のreviewと同じ段を当てる（判断は`domain::agent_eval::programs`）。
  周の定義とケースを読むlanding branchのcommitから、programの一覧とscriptを読み（[ADR-t1895-2](../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)。一覧の設定・実行・envとbackendは[プログラムのreview](supervisor-lifecycle/review-programs.md)と[Run environment](supervisor-lifecycle/run-environment.md)が持ち、ここに写さない）、pathがケースの差分に当たるものを、ケースのtreeをcwdに設定の順に流す。
  ケースのpatchがscriptや設定を変えても、流れるのはcommitの中身である。
- 選んだprogramは、先にケースの`base_commit`のtree（ケースのtreeと同じものを隠す）で設定の順に流し、0以外で終わったものはそのケースのtreeで流さない（古いbaseは今の規則に既に違反しうるが、本番のreviewのbaseのlanding branchはCIが通すため。[ADR-t2300-1](../adr/2026-10-21-t2300-1-eval-leaves-out-programs-a-cases-base-fails.md)）。
  baseで0のものだけをケースのtreeで流すので、ケースを止めるのは差分が足した違反だけである。
  除いたprogramはケースごとに`agent_eval_case_checked`に名前を残し、同じ周の同じbaseでの結果は使い回す。
- built-inのprogramはケースに当てない（runの変更が足す行を着地の前に見るもので、ケースのpatchはそれに当たらない）。
- programはケースの1回の枠の中でagentのjobの前に1本ずつ流し、周の同時数の上限を超えない。
  providerの費用には数えない。
  全部exit 0ならagentのjobを起動する。
- ケースのtreeで1本でも0以外で終わったケースは、残りのprogramとagentを起動せず、agentの成績の分母から外し、`agent_eval_finished`に記録する。
- programの起動の失敗（reviewのactorのbackendがcontainerのときを含む）か時間切れのケースがあれば（baseのtreeでも同じ）、新しい実行を起動せず、周を`incomplete`で閉じ、`passed`を`false`にする。
- ケースごとのprogramの結果は`agent_eval_case_checked`に記録し、周を引き取ったsupervisorは止まったケースのagentを起動しない。
- programが受け持つ規則コードはagentの定義から外すか狭め、ケースの`expected`は書き換えない（programが止めたケースは分母から外れ、ラベルの変更は人・inbox・plannerに限る。[ADR-t1728-1](../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)の決定6・13）。
