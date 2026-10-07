---
id: design-supervisor-lifecycle-language
type: design
title: "Language"
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-doctor
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle-session-prompts
  - design-plugin-integration
  - adr-t616-1
  - adr-t616-2
---

# Language

runtimeが出す固定の文字列は英語だけで持ち（[ADR-t616-1](../../adr/2026-09-27-t616-1-runtime-fixed-strings-are-english.md)）、AIが人に向けて書く文の言語は`[language]`の設定で指定できる（[ADR-t616-2](../../adr/2026-09-27-t616-2-language-of-text-ai-writes-for-people-is-configurable.md)）。

**実装状況**: ADR-t616-1は実装した（task 625。下の「日本語が残っていた固定の文字列」をすべて英語にした）。ADR-t616-2も実装した（task 626。読むのは`src/infrastructure/language.rs`、指示の文面と足し方は`src/domain/language.rs`）。

## runtimeの固定の文字列

runtimeが自分で組み立てる文字列（prompt、askのquestionとoptionの定型部分、CLIの出力とerror、eventのpayloadの文言、runtimeやjobが作るtaskの`context`の見出し、runtimeが組み立てる着地のcommitのメッセージの定型部分、attentionの`next`、KPIのレポートとpushのメッセージ）は英語で書く。`[language]`では変えない。人やAIが書いた文（title・description・context・summary・question・note・finding）は埋め込んでも訳さない。記録済みのqueueの行とcommitは書き換えない。

### 日本語が残っていた固定の文字列

ADR-t616-1の時点（2026-09-27）で英語に直す対象だったもので、task 625でどれも右の英語にした。これから記録される`context`と着地のcommitとpushのメッセージが英語になり、記録済みの行は書き換えない（testの中の入力の例と、人の文を読む`search` / `related`の語の区切り（`、`・`と`・`タスク`など）は対象外）。

| 場所 | 文字列（今） | 用途 |
| --- | --- | --- |
| `src/application/integrate.rs`の`register_follow_ups` | `follow_up proposed by the receipt of run {run_id} of task {id} ({title})` | follow_upのdraftの`context` |
| `src/application/prompt.rs`の`draft_planner_prompt` | `follow-up draft (proposed by the receipt of run {} of task {})`、`goal gap draft (proposed by the judgment of goal {})` | runtimeのplannerが`--context`の冒頭に書く見出し |
| `src/application/prompt.rs`の`finding_planner_prompt` | `from finding {id} ({kind})` | 同上（findingから作るtask） |
| `src/domain/kpi/push.rs` | `target breach`、`landings`、`breaches`、`missed N period(s) in a row (since …)`、`target`、`and`、`Breaches:`、`Missed (1 period):`、`Resolved:`、`open asks: {open_asks}`、`report: {html}`、区切りの`, ` | KPIの目標割れと日次のまとめのpushのメッセージ |

pluginのskill（`dagq-recover`の`reference/review-by-hand.md`、`dagq`の`reference/goal-close.md`）はfollow_upのcontextの文言を引用しているので、合わせて英語の文言にした（古い日本語の文言の記録が残ることも書いた）。`tests/it/related.rs`は古い文言の形のcontextを入力の例に使うが、読むのはtaskの番号だけなので変えていない。

同じtask 625で、promptとerror・commitの文言からdagqのADR番号（`ADR-0044 decision 22`など）を除いた（[Prompt](prompt.md#repositoryの規則を読む順)）。

## `[language]`の欄

同じ形の表を2か所に書ける。

| 置き場所 | 読むもの | 優先 |
| --- | --- | --- |
| repositoryの`dagq.toml`（main checkoutの作業ファイル。[Run environment](run-environment.md)と同じ） | そのqueueのsupervisor・`up`・`doctor`・`status` | 1 |
| 利用者ごとの`$XDG_CONFIG_HOME/dagq/config.toml`（`XDG_CONFIG_HOME`が空か無ければ`~/.config/dagq/config.toml`） | その利用者の環境で動くdagq。supervisorは自分のprocessの`XDG_CONFIG_HOME` / `HOME`で読む。in-cmux modeでは`up`を打ったshellのenvを引き継ぎ、launchd modeでは`up`がshellの`XDG_CONFIG_HOME`（exportされていて空でなければ）をplistの環境に入れるので、どちらのmodeでも`up`のpreflightと同じファイルを読む（[up / down](up-down.md)のplist。`host.toml`も同じ）。pathはCLI（`main.rs`）が環境変数から決めて`SuperviseOptions`・`ObserveOptions`・`UpEnvironment`・`OneShot`の`user_config`に渡し、そこが`None`なら読まない（testは明示したpathだけを読み、testを走らせる人の設定に左右されない） | 2 |

| key | 型 | 既定 | 意味 |
| --- | --- | --- | --- |
| `tag` | 文字列 | 無し（指示しない） | AIが人に向けて書く文の言語のBCP 47の言語タグ（例`"ja"`、`"en"`、`"pt-BR"`） |

```toml
[language]
tag = "ja"
```

- 解決は`dagq.toml`の`tag`、無ければ`config.toml`の`tag`、どちらも無ければ未設定。表ごとではなく`tag`ごとに見る（表だけあって`tag`が無ければ次へ）。
- 書式の誤りはerrorにする: 表の中の未知のkey、文字列でない値、空の文字列、BCP 47の形（`-`で区切った英数字の部分タグで、先頭は2〜3文字か5〜8文字の英字の言語）に合わないタグ。登録簿にあるかは見ない。大文字小文字は書いたまま渡す。
- `config.toml`は今は`[language]`だけを持ち、ほかの表は未知の表としてerrorにする（KPI・レポート・pushは[`host.toml`](push.md)のまま）。`config.toml`が無ければ未設定。
- 解決は使うたびに行い、queueにもsupervisorの登録にも保存しない。変えたら次に立つsessionとjobから効き、走っているsessionのpromptは変わらない（workerの`prompt.txt`はclaim時点のスナップショット。resumeとreviseの依頼文には依頼の時点の指示が入る）。
- `dagq.toml`の他の表と同じく、`[language]`を知らない旧バイナリは表ごと拒むので、repositoryに足すのは固定バイナリを入れ替えた後にする。このrepositoryは入れ替えの後に`dagq.toml`へ`[language] tag = "ja"`を足す（人の運用が日本語のため）。

## promptへの渡し方

設定が解決できたら、runtimeは次のpromptの末尾に英語の指示を1段落足す（`with_instruction`）。未設定なら何も足さない。契約の前ではなく末尾に置くのは、どのpromptにも同じ関数で足せ、各promptの組み立てを変えずに済むため。

- worker: claimの`prompt`、resumeの依頼（`resume_request`とその派生）、reviewの差し戻し（`revise_request`と、差し戻しの前提が崩れたときの`revise_mismatch_request`）
- review job（`review_prompt`）、復旧job（`recovery_prompt`）、plan review job（`plan_review_prompt`）とそのreviseの依頼（`plan_revise_request`）、goal review job（`goal_review_prompt`。goalの判定の理由も人が読む）
- planner: runtimeが立てる`runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`・`request_planner_prompt`（どれも`PlannerLaunch`の`language`を`launch_planner`が足す。人が開くplannerの`planner_prompt`は`dagq plan`の廃止（ADR-t1394-1、task 1399）で消した）
- inbox: `inbox_prompt`（`up`の`inbox_session_prompt`）
- observer: `observer_prompt`（`src/application/observer/mod.rs`）

supervisorは`Verifier::language`（`ShellVerifier`がmain checkoutの`dagq.toml`と`user_config`を読む）で、promptを組み立てるたびに解決する。停滞の催促やaskの答えの配送など、立ったsessionに送る短い定型文には足さない（最初のpromptが指示を持つ）。

指示の文面（`[language]`が`ja`のとき）:

```text
Language: write everything you address to people in the language with BCP 47 tag `ja` — your replies in this conversation, ask questions and option descriptions, task and goal titles and descriptions, context, notes, findings, verdict reasons, and receipt summaries and follow_ups (the landing commit message is built from the task title and the receipt summary). Keep code, identifiers, CLI flags, ask option values and quoted runtime output as they are.
```

inboxとplannerの初期promptは5行以内（[Session prompts](session-prompts.md)）だが、この1行は数えない。compactionと`/clear`で初期promptが失われるので、inboxとplannerについては`status --role inbox|planner`の出力に`language`（`tag`・`source`・`user_config`・`instruction`、誤りなら`error`。未設定なら`tag`と`instruction`はnull）を足し、pluginの`SessionStart` hook（`session-start.sh`、[plugin-integration](../plugin-integration.md#起き直しhookadr-0016)）が出す起き直しの出力に同じ指示が入るようにする。hookは先頭の1行で「statusの`language.instruction`があればそれに従って人に向けて書く」と指し、その後のstatusのJSONが指示の文面を持つ（shで JSONから文面を抜き出さない）。pluginのskill（`dagq`・`dagq-planner`・`dagq-inbox`・`dagq-recover`）には「promptか起き直しの出力に言語の指示があれば、その範囲の文をその言語で書く。無ければ会話とrepositoryの規則に従う」と書く（範囲と置き場所は`dagq` skillのsection 5「Language」が持ち、ほかの3つはそこを指す1文を持つ）。workerには起き直しのhookが無い（`session-start.sh`はinboxとplannerだけを扱う）ので、workerは初期promptと、resume・差し戻しの依頼文に載る指示に頼る。

## `up`と`doctor`

- `up`のpreflight: `dagq.toml`の`[language]`と`config.toml`を読み、書式の誤りならsupervisorを起動せず、ファイルのpathと誤りを挙げたerrorで止まる（どちらかで決まっても両方を読むので、使われない側の誤りも止める）。`up`の出力に解決した`language`（`{"tag", "source"}`か、未設定ならnull）を付け、inboxの初期promptに指示を足す。
- `dagq.toml`の他の表を読む処理（claimのprovisioningの`[run.env]`、`integrate`の検証、supervisorの`[stall]`・`[disk]`など）は、`[language]`を既知の表として受け付けるだけで、中のkeyと値も表の重複も検査しない。`[language]`の書式の誤りでprovisioningや着地を失敗させないためで、誤りを見せるのは`up`・`doctor`・promptの組み立てだけにする。
- supervisor: promptを組み立てる時点で`[language]`か`config.toml`が読めなければ指示を足さずに進み、`tracing`のwarnを出す。runもplannerも止めない。
- `doctor`（既定の出力にも出す）: `language`の欄に`tag`（未設定ならnull）、`source`（`repository` / `user` / `unset`）、読んだ`user_config`のpath、promptに足す`instruction`、書式の誤りがあれば`error`を出す（誤りのときは`source`が`unset`）（[doctor](doctor.md)）。このbinaryが読めない（migrateが要る・拒まれる）queueの`doctor`には出ない。

testは`src/domain/language.rs`と`src/infrastructure/language.rs`のunit test（タグの形・優先順・誤り）、`tests/it/language.rs`（設定なし・利用者ごと・`dagq.toml`の上書きの3通りでworkerとreviewのpromptと`doctor`・`status --role inbox`）、`tests/it/lifecycle_up.rs`（preflightの誤りとinboxのprompt）、`tests/it/lifecycle_plan.rs`（runtimeのplanner）、`tests/it/runtime_observer.rs`（observer）、`tests/plugin.rs`（hookの出力）。
