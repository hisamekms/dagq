---
id: adr-t616-2
type: adr
title: AIが人に向けて書く文の言語を、利用者ごとの設定を既定にrepositoryのdagq.tomlで上書きして指定でき、runtimeがすべてのsessionとjobのpromptに指示を足す
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - language
  - plugin
related:
  - adr-t598-1
  - adr-t616-1
  - design-supervisor-lifecycle-language
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-doctor
---

# ADR-t616-2: AIが人に向けて書く文の言語を、利用者ごとの設定を既定にrepositoryのdagq.tomlで上書きして指定でき、runtimeがすべてのsessionとjobのpromptに指示を足す

## Context

dagqのsession（worker・planner・inbox）とjob（review・復旧job・plan review・observer）はAIが動かし、人が読む文を書く: 会話、askのquestion、taskとgoalの本文、note、receiptのsummaryとfollow_ups、そこから作られる着地のcommitのメッセージ。今はそれを何語で書くかをdagqは指示せず、AIは会話とrepositoryの規則（このrepositoryではAGENTS.mdと人の日本語）から推し量っている。runtimeが立てるheadlessのjobには人との会話が無く、promptは英語なので（[ADR-t616-1](2026-09-27-t616-1-runtime-fixed-strings-are-english.md)）、人が日本語で読みたくても英語で書かれうる。

人は2026-09-27に次を決めた。

- 範囲は会話（inbox・planner）、askのquestionと選択肢の説明、taskとgoalの本文、note、receiptのsummaryとfollow_ups、着地のcommitのメッセージまでで、queueとgitに残る文も含む。コード・識別子・CLIのflagは対象外。
- 置き場所は利用者ごとの設定を既定にし、repositoryの`dagq.toml`で上書きする。

## Decision

1. **言語は1つの値で指定する。** 値はBCP 47の言語タグ（`ja`・`en`・`pt-BR`など）で、runtimeは形だけを検査する。言語名の自由な文は受け付けない（`doctor`で見せ、比べられるようにするため）。
2. **置き場所と優先順。** repositoryの`dagq.toml`（main checkoutの作業ファイル）の指定を最優先し、無ければ利用者ごとの設定ファイル（XDGの設定のdirectoryの下のdagqのファイル。repositoryの外にありcommitしない）の指定を使う。repositoryの指定は、そのrepositoryの記録（task・commit）を読む人たちの言語を揃えるためにある。利用者ごとのファイルは、その利用者が動かすdagq（`up`・`plan`・supervisor）が読む。
3. **未設定なら指示しない。** どちらにも指定が無ければ、runtimeはpromptに言語の指示を足さず、AIは今までどおり会話とrepositoryの規則に従う。既存のqueueとrepositoryは設定なしで今までと同じに動く。
4. **範囲。** 上の人の決定のとおりで、AIが書いて人が読む文すべて（review・復旧job・plan reviewのverdictの理由とquestion、observerのfindingの本文を含む）。コード、識別子、CLIのflag、askのoptionの値（runtimeが解釈する`land`・`retry`など）、runtimeの出力の引用は訳さない。runtimeの固定の文字列はADR-t616-1のとおり英語のままにする。
5. **渡し方。** runtimeは、sessionとjobを立てるたびにその時点の設定を解決し、promptに言語の指示を1つ足す。対象はworker（claimとresumeとrevise）・review・復旧job・plan review・planner（人が開くものとruntimeが立てるもの）・inbox・observerのすべてのprompt。初期promptがcompactionと`/clear`で失われうる常駐・対話のsession（inboxとplanner）は、pluginの`SessionStart` hookが出す起き直しの出力にも同じ指示を載せる。workerは初期promptと、resume・差し戻しの依頼文に載る指示に頼る。pluginのskillには、promptの言語の指示に従い、その範囲の文をその言語で書くと書く。
6. **見せ方。** `doctor`は解決した言語とその出どころ（repository・利用者・未設定）を出す。設定のファイルの書式の誤りは`up`のpreflightで止め、`doctor`でerrorとして見せる。それ以外の場面（claimのprovisioning、`integrate`、promptの組み立て）では言語の設定の誤りでrunも着地も止めず、指示を足さずに進める（言語の設定の誤りでqueueを止めない）。

欄の名前・書式・promptの指示の文面・`doctor`の欄は[Language](../design/supervisor-lifecycle/language.md)が持つ。

## Alternatives

- **言語名を自由な文で書く**（`"Japanese"`・`"日本語"`）: AIは読めるが、`doctor`での表示と比較、書式の検査ができない。
- **repositoryの`dagq.toml`だけに置く**: 1人で複数のrepositoryを使う人が、repositoryごとに同じ指定を書くことになる。人が利用者ごとの既定を決めた。
- **既存の`host.toml`に置く**: `host.toml`はhostの事情（KPIの目標・レポート・push）の設定で、queueごとの層も持つ。言語は利用者の好みで、queueごとに変える理由が無い。
- **未設定なら英語を指示する**: 今まで日本語で書かれていた運用が、設定を書くまで英語に変わる。
- **skillだけに書き、promptには足さない**: headlessのjobはskillを読まない経路があり、設定の値をskillに渡す手段も無い。

## Consequences

- 人は1か所に書くだけで、headlessのjobが書くaskやverdictの理由も読める言語になる。
- `dagq.toml`に新しい表が増えるので、それを知らない旧バイナリは未知の表として拒む。repositoryの`dagq.toml`に足すのは、固定バイナリを入れ替えた後にする。
- 実装は別のtaskで行う。実装が入るまでは今までどおり指示しない。
