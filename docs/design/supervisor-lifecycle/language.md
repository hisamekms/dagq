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
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-prompt
  - design-plugin-integration
  - adr-t616-1
  - adr-t616-2
---

# Language

runtimeが出す固定の文字列は英語だけで持ち（[ADR-t616-1](../../adr/2026-09-27-t616-1-runtime-fixed-strings-are-english.md)）、AIが人に向けて書く文の言語は`[language]`の設定で指定できる（[ADR-t616-2](../../adr/2026-09-27-t616-2-language-of-text-ai-writes-for-people-is-configurable.md)）。

設定を読むのは`src/infrastructure/language.rs`、指示の文面と足し方は`src/domain/language.rs`。

## runtimeの固定の文字列

runtimeが自分で組み立てる文字列（prompt、askのquestionとoptionの定型部分、CLIの出力とerror、eventのpayloadの文言、runtimeやjobが作るtaskの`context`の見出し、runtimeが組み立てる着地のcommitのメッセージの定型部分、attentionの`next`、KPIのレポートとpushのメッセージ）は英語で書く。
`[language]`では変えない。
人やAIが書いた文（title・description・context・summary・question・note・finding）は埋め込んでも訳さない。
記録済みのqueueの行とcommitは書き換えない。
testの中の入力の例と、人の文を読む`search` / `related`の語の区切り（`、`・`と`・`タスク`など）は対象外。

## `[language]`の欄

同じ形の表を2か所に書ける。

| 置き場所 | 読むもの | 優先 |
| --- | --- | --- |
| repositoryの`dagq.toml`（main checkoutの作業ファイル。[Run environment](run-environment.md)と同じ） | そのqueueのsupervisor・`up`・`doctor`・`status` | 1 |
| 利用者ごとの`config.toml`（pathは`infrastructure::language`） | その利用者の環境で動くdagq。`up`が起動するlaunchd modeのsupervisorは`up`のpreflightと同じファイルを読む（[up / down](up-down.md)のplist） | 2 |

```toml
[language]
tag = "ja"
```

- 解決は表ごとではなく`tag`ごとに見る（表だけあって`tag`が無ければ次へ。`resolve_language`）。
- 書式の誤りはerrorにする（`infrastructure::language`）。
- `config.toml`はほかの表をerrorにする（KPI・レポート・pushは[`host.toml`](push.md)）。
- 解決は使うたびに行い、queueにもsupervisorの登録にも保存しない。
  変えたら次に立つsessionとjobから効き、走っているsessionのpromptは変わらない（workerの`prompt.txt`はclaim時点のスナップショット。resumeとreviseの依頼文には依頼の時点の指示が入る）。

## promptへの渡し方

設定が解決できたら、runtimeは次のpromptの末尾に英語の指示を1段落足す（`with_instruction`）。
契約の前ではなく末尾に置くのは、どのpromptにも同じ関数で足せ、各promptの組み立てを変えずに済むため。

- worker: claimの`prompt`、resumeの依頼とその派生、reviewの差し戻し（差し戻しの前提が崩れたときの依頼を含む）
- review job、復旧job、plan review jobとそのreviseの依頼、goal review job（goalの判定の理由も人が読む）
- planner: runtimeが立てるplannerのすべてのprompt（どれも`PlannerLaunch`の`language`を`launch_planner`が足す）
- inbox: `dagq inbox`が開くinboxのprompt（`inbox_session_prompt`）
- observer: observerのprompt（`src/application/observer/mod.rs`）

supervisorは`Verifier::language`で、promptを組み立てるたびに解決する。
停滞の催促やaskの答えの配送など、立ったsessionに送る短い定型文には足さない（最初のpromptが指示を持つ）。
指示の文面は`domain::language`が持つ。

compactionと`/clear`で初期promptが失われるので、inboxとplannerについてはその役割の`status`の出力に言語を足し、pluginの`SessionStart` hook（`session-start.sh`、[plugin-integration](../plugin-integration.md#起き直しhookadr-0016)）が出す起き直しの出力に同じ指示が入るようにする。
hookは先頭の1行で「statusの`language.instruction`があればそれに従って人に向けて書く」と指し、その後のstatusのJSONが指示の文面を持つ（shで JSONから文面を抜き出さない）。
pluginのskillには、promptか起き直しの出力に言語の指示があればその範囲の文をその言語で書き、無ければ会話とrepositoryの規則に従うと書く（範囲と置き場所は`dagq` skillの「Language」が持ち、ほかのskillはそこを指す）。
workerには起き直しのhookが無い（`session-start.sh`はinboxとplannerだけを扱う）ので、workerは初期promptと、resume・差し戻しの依頼文に載る指示に頼る。

## `up`と`doctor`

- `up`のpreflight: `dagq.toml`の`[language]`と`config.toml`を読み、書式の誤りならsupervisorを起動せず、ファイルのpathと誤りを挙げたerrorで止まる（どちらかで決まっても両方を読むので、使われない側の誤りも止める）。
  `up`の出力に解決した言語を付ける。
- `dagq.toml`の他の表を読む処理は、`[language]`を既知の表として受け付けるだけで、中のkeyと値も表の重複も検査しない。
  `[language]`の書式の誤りでprovisioningや着地を失敗させないためで、誤りを見せるのは`up`・`doctor`・promptの組み立てだけにする。
- supervisor: promptを組み立てる時点で`[language]`か`config.toml`が読めなければ指示を足さずに進み、`tracing`のwarnを出す。
  runもplannerも止めない。
- `doctor`（既定の出力にも出す）: 解決した言語と書式の誤りを出す（`LanguageReport`、[doctor](commands.md)）。
  このbinaryが読めない（migrateが要る・拒まれる）queueの`doctor`には出ない。
