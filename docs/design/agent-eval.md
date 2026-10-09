---
id: design-agent-eval
type: design
title: agentのeval（定義とケースの置き場・ケースの欄・採点・漏れの検査・依頼と認可・記録・費用の上限・hold-outの1回の制限・evalの枠・本番と共有する起動経路・道具の宣言・採用の判定・productionのケース・programのreview）
status: current
created: 2026-10-06
scope: runtime
related:
  - adr-t1728-1
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
---

# agentのeval

reviewのagentが正しく判定しているかを、規則コードごとの陽性・陰性のケースと閾値で測る仕組み。
誰か（人・inbox・plannerか、自分のrunのdevだけはworker）がqueueに依頼を記録し、supervisorがlanding branchの定義とケースで1周を実行して、成績をeventに記録する。
workerもjobもLLMのCLIを直接打たない。
1周の中のケースごとの1回は、本番のrunのreviewのagentのjobと同じ起動経路で、測るagentの1本のjobだけを起動する。

> **一部は予定**: 「採用の判定」「productionのケースと見張り」の節はまだ実装していない予定で、名前と数値は実装が決める。
> ほかの節は今の姿で、欄・flag・既定値の意味は定義のそばのdoc commentが持つ。

決めた理由は[ADR-t1728-1](../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)（定義とケースの置き場・eval）と[ADR-t1728-2](../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)（道具の宣言）、定義を切らない例外は[ADR-t1869-1](../adr/2026-10-09-t1869-1-agent-jobs-carry-the-whole-definition-and-do-not-start-over-the-limit.md)、材料は[review-agent-evalのSpike](../plans/review-agent-eval-spike.md)が持つ。

## 定義とケースの置き場

- 定義: `.dagq/agents/<name>/AGENT.md`。形は[ADR-t1453-1](../adr/2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)の定義と同じ（frontmatterの`description`と本文の検査項目と参照先の文書）に、道具の宣言の`tools`（下の「道具の宣言」）を足したもの。`<name>`はkebab-case。
- ケース: `.dagq/agents/<name>/evals/`の`dev.json`・`holdout.json`・`production.json`（Spikeの`evals.json`は`dev.json`）。splitはこのファイルで区分し、ケースの欄に持たない。
- patchの共有の置き場: `.dagq/agent-cases/patches/<sha256>.patch`（patchの内容のSHA-256。agentの名前と衝突しないよう`.dagq/agents/`の外に置く）。同じ内容のpatchは1つだけ置き、ケースはhashで参照する。どのケースからも参照されないpatchと、ケースが参照するhashのpatchが無いことは、下の設定の検査が誤りにする。
- どこで走るか: `dagq.toml`の`[review.subagents.<name>] paths`のまま（形は変えない）。定義は`.dagq/agents/<name>/AGENT.md`だけから読み、そこに無ければ定義が無い扱いで、reviewはpassにしない（[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「設定」）。
- 設定の検査（`dagq.toml`を読むときと、`review_subagents::this_repository_names_only_agents_it_defines`の後継のtest）: `[review.subagents.<name>]`が名指す`<name>`に`.dagq/agents/<name>/AGENT.md`が無いこと、同じ`<name>`を複数の役割の表が名指すこと（今の役割の表は`[review.subagents]`だけ。役割の表が増えたら同じ検査に足す）を誤りにする。
  この2つは実装済みで、`dagq doctor`の`agents`の`errors`が出す（役割の表の集合は`domain::review_subagents::ROLE_SECTIONS`。[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「設定」）。
- ケースのファイルの形とpatchの置き場の参照の過不足（上）は、ケースの一覧の読み手と、このrepositoryの全ての一覧をそれで読むtestが検査する。
  `dagq doctor`と設定の読み込みにはまだ足していない。

## ケースの欄

1つのファイルは次の形のJSONにする（Spikeの`evals.json`の形から、`split`と役割に依る欄を分けた）。

```json
{
  "agent": "adr-rules",
  "role": "review",
  "codes": ["A-150", "A-152"],
  "k": 3,
  "cases": [
    {
      "id": "gen-amend-accepted-adr",
      "source": "generated",
      "made_by": "codex-cli 0.160.0 (rules only, verified by a second call)",
      "base_commit": "f46a7963cf708d621bf529eb855ecfb552f78b33",
      "patch": "<sha256>",
      "adjudicated": null,
      "disputed": null,
      "k": 3,
      "review": {
        "input": {},
        "expected": { "verdict": "violation", "codes": ["A-150"], "acceptable_codes": ["A-152"], "note": "..." }
      }
    }
  ]
}
```

- 共通の欄（役割に依らない）: `id`（ファイルの中で一意）・`source`（`generated` / `handmade` / `production`）・`made_by`（作った道具と版、productionはラベルを付けたproviderと版）・`base_commit`（patchを当てるcommit）・`patch`（共有の置き場のhash）・`adjudicated`（人の決定。`null`か、決めた人・日・内容）・`disputed`（争いの印。`null`か理由。立っているケースは主の指標から外す）・`k`（任意。無ければファイルの`k`）。
- 役割ごとの欄: 役割の名前の欄（今は`review`）の下に`input`と`expected`を置く。reviewの`input`は今は空（差分はpatch、規則は定義が持つ）。reviewの`expected`は`verdict`（`violation`＝`revise`か`concern`、`clean`＝`pass`）・`codes`（求める規則コード）・`acceptable_codes`（挙げても誤検出に数えないコード）・`note`（期待の理由。`acceptable_codes`を使うときは許す理由）。
- 読み手が知らない欄はファイルに残してよく、読まない。
  Spikeから取り込んだケースは、由来（元の区分・本番のreviewの結果など）を記録する追加の欄を共通の欄にも役割の欄の下にも持ちうる。
  それらは任意で、無いケースもある。
  読み手はそれらを誤りにせず、読んだケースの型にも持たない（ファイルにだけ残る）。
  採点（判定と規則コードのrecall・precision）が使うのは`expected`の`verdict`・`codes`・`acceptable_codes`と`disputed`だけで、追加の欄も`note`のような読む任意の欄も成績を変えない。
- `handmade`はdevだけに置く。改善するsessionが中身を読んだケースはholdoutに置かない。
- 読み手は形の誤り（欄の欠けと型の違い・知らないファイル・置き場に無いpatchの参照・ファイルの中のidの重複など）を、最初の1つでなく全て返す。
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
  閾値の値は`[eval]`の設定（`EvalConfig`のdoc comment）。
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
  use caseは`application::agent_eval`の`AgentEvals`、flagの意味はCLIの定義（`AgentCommand`）が持つ。
- 依頼は記録するだけで、実行はsupervisorが行う。
  supervisorは依頼を実行するだけで、自分では依頼しない。
- 依頼の認可と実行を分ける: devはworker（自分のrunの分）・人・inbox・planner、hold-outとproductionは人・inbox・planner、同じキーの再実行は人とinboxだけができる。
  読み取りはworkerには自分のrunの分だけを見せる。
  capabilityの表は[Authorization](authorization.md)が持ち、要るcapabilityは`domain::authorization::eval_request_needs`が決める。
- ケースのラベルの変更と`disputed`の解除はコマンドを持たず、人とinbox・plannerがtaskで行う。

## 記録と成績の作り直し

- 依頼・周の開始と拒否・providerを待つこと・実行ごとの開始と終わり・成績は、全てqueueのeventで、周の状態はeventだけから作り直す（`domain::agent_eval::record`）。
  別の表を持たず、supervisorが入れ替わっても同じ状態を読む。
- 欄の意味はそのmoduleのdoc commentと、成績を作る`application::agent_eval::finished`のdoc commentが持つ。
- 成績は判定と規則コードのrecall・precision、閾値との比較、失敗したケースとagentの理由、回数、費用とその出所、定義とケースの集合のdigest、completeかincompleteかを持つ。
  incompleteの周は閾値を超えてもpassに数えない。
- 版ごとの成績はeventだけが持ち、repositoryに置かない。

## 費用の上限

- 入口: `domain::agent_eval::round`の`estimate`・`may_start_next`・`run_cost`、設定は`dagq.toml`の`[eval]`と`[eval.providers.<provider>]`（keyと既定値は`EvalConfig`・`ProviderCost`のdoc comment）。
- 不変条件: 周は起動の前に見積もり（予定の実行の数 × 1回の見込み）、回数か見積もりが上限を超えれば起動せず理由を記録する。
  起動した周は見積もりを予約として記録し、次の実行の前に、使った額と実行中の見込みと次の1回の見込みの和が上限を超えるなら新しい実行を起動せず、incompleteで閉じる。
- 使った額は実行ごとに確定する: providerが返す金額、返さないproviderはtokenに単価を掛けた額、どちらも無く終わった実行はその見込み。
- 落とし穴: 金額を返さずtokenの単価も無いproviderの周は、既定の見込みがあっても使った額を確定できないので起動しない。
  1回の見込みが無い周も起動しない。
- 上限なしで流す経路は持たない。

## hold-outの1回の制限

- キーはagent・定義のdigest・ケースの集合のdigest（流すケースのidと参照するpatchの内容から作る）で、hold-outとproductionの周に当てる（`holdout_refusal`・`case_set_digest`）。
- 判定はeventに記録した開始した周のキーから読み、別の状態を持たない。
- 2回目以降は人の明示の再実行（`--rerun`）だけで流せ、同じ定義でもケースの集合が違えば初回になる。

## evalの枠

- evalはrunのslotを使わず、claim・着地の順・superviseの回ごとのrunの本数を変えない。
  本番のreviewはrunのslotの中で動き、evalを待たない。
- queueごとに同時に1周だけ流す（流れている周は開始があって終わりの無いもので、eventから読む）。
  周の持ち主はそれを始めたか引き取ったsupervisorで（そのtokenをeventに書く）、同じqueueのほかのsupervisorは持ち主が生きている間は触らない。
  周の開始と引き取りはそれぞれ1つの書き込みのtransactionで、2つのsupervisorが同じ周を始めることも引き取ることもない（`EvalRounds`）。
  1周の中のproviderのprocessの同時数は設定の上限まで。
- 待っている周は、着地の前のdevを先に、依頼の古い順に流し（`round::next_round`）、流れている周は止めない。
- evalのjobはrunのreviewのjobの同時実行の上限に数えない。

## 採用の判定（着地の前のdev）

- 対象: runの差分が`.dagq/agents/<name>/AGENT.md`を変えるrun（差分の取り方はreviewのagentの選び方と同じ`<base>...<head>`）。変えたagentごとに1周。
- reviewのpassの後、着地の前（[着地の前のe2e](supervisor-lifecycle/landing-e2e.md)と同じ位置）に、supervisorがlanding branchのcommitの`.dagq/agents/<name>/evals/dev.json`とpatchで、runのcommitの定義を測る。run branchのケースの追加・変更は使わない。
- 待つ間と流す間、runは自分のslotとleaseを持ったまま待つ（e2eと同じ）。
- `passed`が`false`（閾値を下回った、`incomplete`）なら着地させず、理由（成績・失敗したケースのidとagentの理由）をworkerに差し戻す。差し戻しはreviewの`revise`と同じ経路で送り、reviseの上限に1回と数える（上限を超えれば人の判断）。`refused`（`cost_unknown`・上限超え）はworkerが直せないので人の判断（`approve_landing`のask）にする。

## productionのケースと見張り

- `dagq agent cases production <agent>`は、`review_started`・`review_finished`のeventとrunの差分（`git diff <base>...<head>`）から、そのagentが選ばれたreviewをケースにする。ラベルは改善する側（Claude）と別のprovider（今はCodex）のjobが規則の本文だけで付け、本番の判定と食い違うものは`disputed`にして人に回す（inboxのask）。
- 作った集合はqueue側に置き、hold-outとして1回流した後、repositoryの`production.json`に足すのはtaskで行う。
- 見張り: supervisorは、採用した定義（landing branchの定義のdigest）ごとに、前回の見張り（`agent_eval_watch_due`）の後に`new_reviews`件の本番のreviewがたまったか`interval_days`が過ぎたら、`agent_eval_watch_due`のfindingを記録するだけで、自分では依頼しない。

## 道具の宣言

今の姿の詳細は[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「道具の宣言」が持つ。

- frontmatterの`tools`: runtimeが持つ道具の一覧の名前のリスト（`tools: [read, grep]`か、1行に1つの`- read`）。無ければ役割の既定（reviewは`read`・`grep`・`glob`）。`tools: []`は道具を持たない宣言。
- runtimeの道具の一覧（`domain::review_subagents::AgentTool::ALL`）: `read`（fileを読む）・`grep`（中身を探す）・`glob`（pathを探す）・`shell`（コマンド）・`edit`・`write`。reviewの役割が許すのは`read`・`grep`・`glob`だけで、`shell`・`edit`・`write`と一覧に無い名前とリストでない`tools`は定義の誤りにする（`AgentTools::declared`）。誤りの定義を選んだreviewはsnapshotが誤りにしてADR-t1453-1決定6の経路でpassにせず、`dagq doctor`の`agents`の`errors`にも出す。
- providerごとの変換（ADR-t1895-1の独立のagentのjobの起動の設定を作る関数。agentのjobの起動、`AgentProvider::agent_job_command`が使う）:
  - Claude（`infrastructure::adapters::claude_agent_job_tool_args`）: jobの起動の引数の`--allowedTools`に宣言した道具（`read`→`Read`、`grep`→`Grep`、`glob`→`Glob`、`shell`→`Bash`、`edit`→`Edit`・`NotebookEdit`、`write`→`Write`）を並べ、`--disallowedTools`に一覧の残りを並べる。reviewの既定は今のreviewと同じ`Read,Grep,Glob`と`Bash,Edit,Write,NotebookEdit`。予約の道具の拒否（`PRINT_MODE_DENIED_TOOLS`）、`--setting-sources ""`とreviewの`--settings`（ADR-t1470-1）はjobの起動の側が持ち、変換は触らない。
  - Codex（`infrastructure::codex::agent_job_tools_config`）: Codexはfileを読む・探す・pathを探すのもshellのコマンドで行うので、`read`・`grep`・`glob`・`shell`のどれかを宣言したjobには何も足さず（read-onlyのsandboxがコマンドを読み取りに留める）、どれも宣言しないjobには`-c features.shell_tool=false`と`-c features.unified_exec=false`（codex-cli 0.160.0の`[features]`）を足してshellを外す。`--sandbox read-only`・jobのpermission profile・worktreeのprojectの`untrusted`（ADR-t1570-1）は変えず、狭めるだけ。このためCodexでは`read`だけの宣言と`read`・`grep`・`glob`の宣言は同じ設定になる。
- 当てないもの: 親のjobの中のsubagentの渡し方（Claudeの`--agents`のJSONの`tools`は`SUBAGENT_TOOLS`のまま、Codexの`-c agents.<name>.*`）と`subagents_unsupported`。全体のreviewのjobは定義を持たないので対象にしない。

## evalが本番と共有する起動経路

- ケースごとの1回は、本番のrunのreviewのagentのjobと同じ経路で、測るagentの1本のjobだけを起動する。
  経路はheadlessのjobの共通の経路（起動・時間の上限・やり直し・記録・引き継ぎ。[Headless job processes](supervisor-lifecycle/headless-job-processes.md)）、1本のagentのjobの組み立て（`application::agent_job::build`、[Prompt](supervisor-lifecycle/prompt.md#agentのjobの上限)）、上の「道具の宣言」の変換、行き先のproviderは`[roles.review]`。
  eval専用の起動の組み立ては作らない。
- 実行の入口は`application::supervise::agent_eval`。
- ケースのtreeは`base_commit`にpatchを当ててcommitした使い捨てのworktreeで、queueのdata dirの`agent-evals/`の下に周ごとに置く。
  jobのcwdはそのtreeで、材料のファイル（commitの一覧と全文の差分`<base_commit>...<patchを当てたcommit>`）はケースのdirの材料だけのdirに書く。
  Claudeのjobが読めるのはtreeとそのdirだけで（`--add-dir`）、同じケースのほかの実行の出力は読めない。
  Codexのjobはread-onlyのsandboxのshellで読むので、treeの外（ほかの実行の出力やcommitに残るケースの一覧）も読めてしまう: 道具の宣言で読み取りを狭めても、読める場所はsandboxが決める。
  ケースのdirは番号で名付け、promptにもpathにもケースのidと期待を出さない。
  treeの作業ファイルからはevalのケースの一覧とpatchの置き場を消す（commitには残る）ので、Claudeのjobは自分の期待を読めない。
- providerのloginか使用量の上限で止まった実行は、providerを控えて同じ実行を待たせ、やり直しに数えない。
- 起動を止めたsupervisor（drain・stop・serviceの停止）は、流れている実行が終われば周を手放し、次に動くsupervisorがeventから読み直す。
- providerを切り替えない: 成績・1回の見込み・tokenの換算がproviderごとで、切り替えると別のproviderの成績が混ざるため。
  `[roles.review]`のproviderが控え中か`--no-claude`で使えない間は、新しい実行を起動せず理由を記録して待ち、流れている実行は止めない。
- supervisorの再起動: 持ち主が居なくなった周だけを引き取り、前のsupervisorのevalのjobは引き継ぎで子孫ごと止めて記録し、周をeventから読み直して、終わっていない実行をその見込みで使った額に数えてから費用の上限の判定を通して起動し直す。
  走っているprocessを引き継いで続ける形にはしない。
- 使わないもの: 全体のreviewのjob、親のjobの中のsubagent（`--agents`・Codexの`runs_review_subagents`・能力による切り替え）。

## programのreviewの当て方

- evalは各ケースに本番のreviewと同じ段を当てる（判断は`domain::agent_eval::programs`）。
  周の定義とケースを読むlanding branchのcommitから、programの一覧とscriptを読み（[ADR-t1895-2](../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)。一覧の設定・実行・envとbackendは[プログラムのreview](supervisor-lifecycle/review-programs.md)と[Run environment](supervisor-lifecycle/run-environment.md)が持ち、ここに写さない）、pathがケースの差分に当たるものを、ケースのtreeをcwdに設定の順に流す。
  ケースのpatchがscriptや設定を変えても、流れるのはcommitの中身である。
- programはケースの1回の枠の中でagentのjobの前に1本ずつ流し、周の同時数の上限を超えない。
  providerの費用には数えない。
  全部exit 0ならagentのjobを起動する。
- 1本でも0以外で終わったケースは、残りのprogramとagentを起動せず、agentの成績の分母から外し、`agent_eval_finished`の`program_stopped`に数とケースのidと落ちたprogramを記録する。
- programの起動の失敗（reviewのactorのbackendがPodmanのときを含む）か時間切れのケースがあれば、新しい実行を起動せず、周を`incomplete`（`program_failed`）で閉じ、`passed`を`false`にする。
- ケースごとのprogramの結果は`agent_eval_case_checked`に記録し、周を引き取ったsupervisorは止まったケースのagentを起動しない。
- programが受け持った規則コードをagentの`expected`から外す整理は、定義を移す後続のtaskが行う。
