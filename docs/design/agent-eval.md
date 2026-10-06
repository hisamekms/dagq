---
id: design-agent-eval
type: design
title: agentのeval（定義とケースの置き場・ケースの欄・patchの共有・CLI・event・費用の上限と既定値・evalの枠・採用の判定・productionのケースと見張り・道具の宣言とproviderごとの変換・本番と共有する起動経路・programのreviewの当て方）
status: draft
created: 2026-10-06
updated: 2026-10-06
last_verified: 2026-10-06
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
---

# agentのeval

> **一部だけ実装（2026-10-06）**: この文書はagentの定義とeval（goal 125）の今の予定で、実装したのは定義のpath（`.dagq/agents/<name>/AGENT.md`、移行の間は旧の`.dagq/review-agents/<agent>.md`にも戻る）と、名指すagentの定義の有無と複数の役割の検査（`dagq doctor`の`agents`。task 1866）と、下の「道具の宣言」の宣言の検査とproviderごとの変換の関数（task 1873。どの起動にもまだ当たらない）だけ。今動いているreviewのagent（親のreview jobの中のsubagent）は[Review](supervisor-lifecycle/review.md#reviewのsubagent)と[Run environment](supervisor-lifecycle/run-environment.md)が持ち、この文書はそれらを変えない。後続のtask（1867〜1874）が実装したら、この注記と各節を今の姿に直す。

決めた理由は[ADR-t1728-1](../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)（定義とケースの置き場・eval）と[ADR-t1728-2](../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)（道具の宣言）、材料は[review-agent-evalのSpike](../plans/review-agent-eval-spike.md)が持つ。ここはpath・欄・コマンド・event・数値・設定のkeyの予定を持つ。名前と数値は実装のtaskが確かめて決め、変えたらここを直す。

## 定義とケースの置き場

- 定義: `.dagq/agents/<name>/AGENT.md`。形は今の`.dagq/review-agents/<agent>.md`と同じ（frontmatterの`description`と本文の検査項目と参照先の文書）に、道具の宣言の`tools`（下の「道具の宣言」）を足したもの。`<name>`は今と同じkebab-case。
- ケース: `.dagq/agents/<name>/evals/`の`dev.json`・`holdout.json`・`production.json`（Spikeの`evals.json`は`dev.json`）。splitはこのファイルで区分し、ケースの欄に持たない。
- patchの共有の置き場: `.dagq/agent-cases/patches/<sha256>.patch`（patchの内容のSHA-256。agentの名前と衝突しないよう`.dagq/agents/`の外に置く）。同じ内容のpatchは1つだけ置き、ケースはhashで参照する。どのケースからも参照されないpatchと、ケースが参照するhashのpatchが無いことは、下の設定の検査が誤りにする。
- どこで走るか: `dagq.toml`の`[review.subagents.<name>] paths`のまま（形は変えない）。定義のpathだけが`.dagq/agents/<name>/AGENT.md`に変わった（task 1866。移行の間は旧の`.dagq/review-agents/<name>.md`にも戻る。[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「設定」）。
- 設定の検査（`dagq.toml`を読むときと、`review_subagents::this_repository_names_only_agents_it_defines`の後継のtest）: `[review.subagents.<name>]`が名指す`<name>`に`.dagq/agents/<name>/AGENT.md`が無いこと、同じ`<name>`を複数の役割の表が名指すこと（今の役割の表は`[review.subagents]`だけ。役割の表が増えたら同じ検査に足す）を誤りにする。この2つは実装済みで、`dagq doctor`の`agents`の`errors`が出す（役割の表の集合は`domain::review_subagents::ROLE_SECTIONS`、移行の間は旧の`.dagq/review-agents/<name>.md`も定義と数える。[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「設定」）。あわせて、ケースのファイルの形と、patchの置き場の参照の過不足（上）を検査する。

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
- `handmade`はdevだけに置く。改善するsessionが中身を読んだケースはholdoutに置かない。
- 起動・形の検査・採点・productionのケースの作り方は役割ごとのharnessが持つ。今作るのはreviewのharnessだけ。

## CLI

- `dagq agent eval <agent> [--split dev|holdout|production] [--k N] [--cases <set>] [--rerun]`: evalの依頼を記録する（queue serviceのユースケース。実行はsupervisor）。`--split`の既定は`dev`。`--k`はケースの`k`を上書きする。`--cases`はproductionの集合（下の`dagq agent cases`が返すid）か、devの一部のケースのidの並び。`--rerun`は同じキーのhold-outの2回目以降の明示の指定で、人と人の言葉を受けたinboxだけが使える。返すのは依頼のid。
- `dagq agent evals [--agent <agent>] [--limit N]`: evalの依頼と周の一覧（状態・split・成績の要約）。
- `dagq agent evals show <id>`: 1つの周の成績（判定と規則コードのrecall・precision・ケースごとの判定・失敗したケースのidとagentの理由・費用と出所・programで止まったケース・complete / incompleteと理由）。
- `dagq agent cases production <agent> [--since <date>]`: 本番のreviewのeventと差分からproductionのケースの集合を作る依頼（ラベルのjobの起動と集合の作成はsupervisor）。作った集合はqueue側に置き、idを返す。
- 認可（[Authorization](authorization.md)のPolicyに足す予定）: `agent eval --split dev`はworker（自分のrunの分）・人・inbox・planner、`--split holdout|production`と`agent cases production`は人・inbox・planner、`--rerun`は人とinbox、`agent evals`の読み取りはworker（自分のrunの依頼の分）・人・inbox・planner・observer。ケースのラベルの変更と`disputed`の解除はコマンドを持たず、人とinbox・plannerがtaskで行う。

## event

| event | いつ | 主な欄 |
|---|---|---|
| `agent_eval_requested` | 依頼を記録した | `eval_id`・`agent`・`split`・`k`・`cases`（集合のidかケースのid）・`rerun`・`requested_by`（actor）・`run_id`（workerの依頼と着地の前のdev） |
| `agent_eval_refused` | 周を起動しなかった | `eval_id`・`reason`（`over_run_limit` / `over_cost_limit` / `cost_unknown` / `holdout_used` / `unauthorized`）・`estimate`（見積もりがあれば） |
| `agent_eval_started` | 周を起動した | `eval_id`・`agent`・`split`・`provider`・`definition_commit`（landing branchのcommit）・`definition_digest`・`case_set_digest`・`planned_runs`・`estimate`（`per_run_usd`・`per_run_source`（`recent_max` / `provider_default`）・`total_usd`）・`reserved_usd` |
| `agent_eval_finished` | 周が終わった | `eval_id`・`outcome`（`complete` / `incomplete`）・`incomplete_reason`（`cost_limit` / `program_failed` / `job_failed`）・`scores`（`verdict_recall`・`verdict_precision`・`codes_recall`・`codes_precision`・`runs`・`errors`）・`passed`（4つとも閾値以上で、`complete`のときだけ`true`）・`failed_cases`（idとagentの理由）・`program_stopped`（`count`と`case_ids`）・`cost`（`spent_usd`と実行ごとの`source`（`actual` / `converted` / `estimated`）の内訳）・`definition_digest`・`case_set_digest` |
| `agent_eval_watch_due` | 見張りの時期が来た（finding） | `agent`・`definition_digest`・`new_reviews`・`since`（前回の見張り） |

- hold-outの1回の制限は、`agent_eval_started`の（`agent`・`definition_digest`・`case_set_digest`）と`split: holdout`の組がすでにあるかで判定する（別の状態を持たない）。`case_set_digest`は流すケースのidの並びと、参照するpatchの内容のhashから作る。
- 閾値を下回った`holdout` / `production`の`agent_eval_finished`はobserverのfindingにする。`agent_eval_watch_due`のfindingは[Finding planners](supervisor-lifecycle/finding-planners.md)の経路でruntimeのplannerを開き、plannerが`agent cases production`と`agent eval --split holdout --cases <set>`を依頼する。
- 版ごとの成績はこのeventだけが持ち、repositoryに置かない。

## 費用の上限と既定値

`dagq.toml`の`[eval]`と`[eval.providers.<provider>]`（予定のkey。旧バイナリは知らない表で全体を読めなくなるので、このrepositoryに足すのは固定バイナリが対応した後）。

| key | 既定 | 意味 |
|---|---|---|
| `[eval] max_runs` | 120 | 1周の実行（ケース × k）の回数の上限 |
| `[eval] max_cost_usd` | 30 | 1周の金額の上限（USD） |
| `[eval] concurrency` | 4 | 1周の中のproviderのprocessの同時数（Spikeの並列4） |
| `[eval] threshold` | 0.9 | 判定と規則コードのrecall・precisionの閾値（4つとも） |
| `[eval] recent_runs` | 20 | 1回の見込みに使う、同じagentとproviderの直近の実績の回数（その最大を使う） |
| `[eval.watch] interval_days` | 7 | 見張りの間隔（前回の見張りから） |
| `[eval.watch] new_reviews` | 10 | 見張りの時期にする、前回からの新しい本番のreviewの件数（agentごと。間隔とどちらか先に来たほう） |
| `[eval.providers.<provider>] default_run_usd` | claude: 0.40、codex: なし | 実績の無いときの1回の見込み（ClaudeはSpikeの実測の1回$0.20の2倍） |
| `[eval.providers.<provider>] input_usd_per_mtok`・`cached_input_usd_per_mtok`・`output_usd_per_mtok` | なし | 金額を返さないproviderのtokenの単価（入力・cache・出力。100万tokenあたりのUSD） |

- 見積もり: 予定の実行の数（ケースの`k`の和）× 1回の見込み。1回の見込みは直近の`recent_runs`回の実績の最大、無ければ`default_run_usd`。どちらも無ければ`cost_unknown`。
- 使った額: Claudeは結果の`total_cost_usd`（`actual`）、Codexはtokenに単価を掛けた額（`converted`）。単価が無いproviderは周を起動しない（`cost_unknown`）。金額もtokenも返さずに終わった実行は、その実行の見込みを数える（`estimated`）。
- 実行中: 次の実行を起動する前に、`spent + 実行中の見込み + 次の1回の見込み > max_cost_usd`なら起動せず、実行中のものを待って`incomplete`（`cost_limit`）で閉じる。

## evalの枠

- evalはrunのslotを使わない。claim・着地の順・superviseの回ごとのrunの本数は変わらず、本番のreviewはrunのslotの中で動いてevalを待たない。
- queueごとに同時に1周だけ流す。1周の中の同時数は`[eval] concurrency`。
- 待っている周の順: 着地の前のdev（下）を先に、依頼は`agent_eval_requested`の古い順。流れている周は止めない。

## 採用の判定（着地の前のdev）

- 対象: runの差分が`.dagq/agents/<name>/AGENT.md`を変えるrun（差分の取り方はreviewのagentの選び方と同じ`<base>...<head>`）。変えたagentごとに1周。
- reviewのpassの後、着地の前（[Review](supervisor-lifecycle/review.md)の「着地の前のe2e」と同じ位置）に、supervisorがlanding branchのcommitの`.dagq/agents/<name>/evals/dev.json`とpatchで、runのcommitの定義を測る。run branchのケースの追加・変更は使わない。
- 待つ間と流す間、runは自分のslotとleaseを持ったまま待つ（e2eと同じ）。
- `passed`が`false`（閾値を下回った、`incomplete`）なら着地させず、理由（成績・失敗したケースのidとagentの理由）をworkerに差し戻す。差し戻しはreviewの`revise`と同じ経路で送り、reviseの上限に1回と数える（上限を超えれば人の判断）。`refused`（`cost_unknown`・上限超え）はworkerが直せないので人の判断（`approve_landing`のask）にする。

## productionのケースと見張り

- `dagq agent cases production <agent>`は、`review_started`・`review_finished`のeventとrunの差分（`git diff <base>...<head>`）から、そのagentが選ばれたreviewをケースにする。ラベルは改善する側（Claude）と別のprovider（今はCodex）のjobが規則の本文だけで付け、本番の判定と食い違うものは`disputed`にして人に回す（inboxのask）。
- 作った集合はqueue側に置き、hold-outとして1回流した後、repositoryの`production.json`に足すのはtaskで行う。
- 見張り: supervisorは、採用した定義（landing branchの定義のdigest）ごとに、前回の見張り（`agent_eval_watch_due`）の後に`new_reviews`件の本番のreviewがたまったか`interval_days`が過ぎたら、`agent_eval_watch_due`のfindingを記録するだけで、自分では依頼しない。

## 道具の宣言

実装済み（task 1873）。今の姿の詳細は[Review](supervisor-lifecycle/review.md#reviewのsubagent)の「道具の宣言」が持つ。

- frontmatterの`tools`: runtimeが持つ道具の一覧の名前のリスト（`tools: [read, grep]`か、1行に1つの`- read`）。無ければ役割の既定（reviewは`read`・`grep`・`glob`）。`tools: []`は道具を持たない宣言。
- runtimeの道具の一覧（`domain::review_subagents::AgentTool::ALL`）: `read`（fileを読む）・`grep`（中身を探す）・`glob`（pathを探す）・`shell`（コマンド）・`edit`・`write`。reviewの役割が許すのは`read`・`grep`・`glob`だけで、`shell`・`edit`・`write`と一覧に無い名前とリストでない`tools`は定義の誤りにする（`AgentTools::declared`）。誤りの定義を選んだreviewはsnapshotが誤りにしてADR-t1453-1決定6の経路でpassにせず、`dagq doctor`の`agents`の`errors`にも出す。
- providerごとの変換（ADR-t1895-1の独立のagentのjobの起動の設定を作る関数。evalのagentのjobにも本番のrunのreviewのagentのjobにも使える）:
  - Claude（`infrastructure::adapters::claude_agent_job_tool_args`）: jobの起動の引数の`--allowedTools`に宣言した道具（`read`→`Read`、`grep`→`Grep`、`glob`→`Glob`、`shell`→`Bash`、`edit`→`Edit`・`NotebookEdit`、`write`→`Write`）を並べ、`--disallowedTools`に一覧の残りを並べる。reviewの既定は今のreviewと同じ`Read,Grep,Glob`と`Bash,Edit,Write,NotebookEdit`。`--setting-sources ""`とreviewの`--settings`（ADR-t1470-1）はjobの起動の側が持ち、変換は触らない。
  - Codex（`infrastructure::codex::agent_job_tools_config`）: Codexはfileを読む・探す・pathを探すのもshellのコマンドで行うので、`read`・`grep`・`glob`・`shell`のどれかを宣言したjobには何も足さず（read-onlyのsandboxがコマンドを読み取りに留める）、どれも宣言しないjobには`-c features.shell_tool=false`と`-c features.unified_exec=false`（codex-cli 0.160.0の`[features]`）を足してshellを外す。`--sandbox read-only`・jobのpermission profile・worktreeのprojectの`untrusted`（ADR-t1570-1）は変えず、狭めるだけ。このためCodexでは`read`だけの宣言と`read`・`grep`・`glob`の宣言は同じ設定になる。
- 当てないもの: 親のjobの中のsubagentの渡し方（Claudeの`--agents`のJSONの`tools`は`SUBAGENT_TOOLS`のまま、Codexの`-c agents.<name>.*`）と`subagents_unsupported`。全体のreviewのjobは定義を持たないので対象にしない。

## evalが本番と共有する起動経路

- evalのケースごとの1回は、本番のrunのreviewのagentのjobと同じ経路で、測るagentの1本のjobだけを起動する: task 1896の共通のjobの経路（起動・時間の上限・記録）、task 1903のagentのjobのpromptの組み立て（snapshotが返した定義をそのまま持たせる）、task 1873の道具の変換、行き先のproviderは`[roles.review]`。起動の引数・prompt・時間の上限の詳細は[Review](supervisor-lifecycle/review.md)が持ち、ここに写さない。
- ケースのtreeは`base_commit`にpatchを当てた使い捨てのworktree（queueのdata dirの下）で、jobのcwdとreviewの差分の範囲（`<base_commit>...<patchを当てたcommit>`）にする。
- 使わないもの: 全体のreviewのjob、親のjobの中のsubagent（`--agents`・Codexの`runs_review_subagents`・能力による切り替え）、eval専用の起動の組み立て。

## programのreviewの当て方

- evalは各ケースに本番のreviewと同じ段を当てる。landing branchのcommitのprogramの一覧（[ADR-t1895-2](../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)。一覧の設定・実行・envとbackendは[Review](supervisor-lifecycle/review.md)と[Run environment](supervisor-lifecycle/run-environment.md)がtask 1896・1897の実装と一緒に持ち、ここに写さない）を、ケースのtreeに対して先に流す。
- programが落ちたケースはagentに渡さず、agentの成績の分母から外し、`agent_eval_finished`の`program_stopped`に数とidを記録する。
- programの起動の失敗・時間切れのケースがあれば、周を`incomplete`（`program_failed`）にし、`passed`を`false`にする。
- programが受け持った規則コードをagentの`expected`から外す整理は、定義を移す後続のtask（goal 153の1901とgoal 125の定義の取り込み）が行う。
