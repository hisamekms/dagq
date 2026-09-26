---
id: adr-t616-1
type: adr
title: runtimeが出す固定の文字列（prompt・ask・error・event・contextの見出し・commitの定型部分・pushのメッセージ）は英語にする
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - language
related:
  - adr-t598-1
  - adr-t616-2
  - design-supervisor-lifecycle-language
---

# ADR-t616-1: runtimeが出す固定の文字列（prompt・ask・error・event・contextの見出し・commitの定型部分・pushのメッセージ）は英語にする

## Context

dagqはこのrepository自身の開発でしか使われてこず、ここで人は日本語を使っている（goal 52）。runtimeの文字列はほとんど英語で書かれているが、固定の文字列に日本語が混ざる箇所が残っている。

- runtimeやjobが作るdraftの`context`の見出し: `integrate`がfollow_upのdraftに付けるcontext、runtimeのplannerのpromptが`--context`の冒頭に書かせるfollow-up draft・goal gap draft・findingの見出し。
- KPIの目標割れと日次のまとめのpushのメッセージ（区切りや見出しの語）。

他のrepositoryの人は日本語を読めるとは限らない。一方で、AIが人に向けて書く文の言語は人が選べるようにする（[ADR-t616-2](2026-09-27-t616-2-language-of-text-ai-writes-for-people-is-configurable.md)）。runtimeの固定の文字列までその設定で訳すと、同じ文字列を複数の言語で持ち、testと文書とplugin（skillが文言を引用する）を言語ごとに保つことになる。

人は2026-09-27に「システムの出力は英語、AIがユーザーに返す言語は指定できる」と決めた。

## Decision

1. **runtimeが自分で組み立てる固定の文字列は英語だけで持つ。** 対象は、sessionとjobに渡すprompt（worker・resume・revise・review・復旧job・plan review・planner・inbox・observer）、askのquestionとoptionの定型部分、CLIの出力とerror、eventのpayloadの文言、runtimeやjobが作るtaskの`context`の見出し、runtimeが組み立てる着地のcommitのメッセージの定型部分、attentionの`next`、KPIのレポートとpushのメッセージ。言語の設定（ADR-t616-2）では変えず、翻訳の仕組みも持たない。
2. **人やAIが書いた文はそのまま運ぶ。** taskのtitle・description・context、receiptのsummary、askのquestion、note、findingの本文など、人かAIが書いた文は、runtimeが中に埋め込んでも訳さない（その言語はADR-t616-2が決める）。runtimeが人の文を読む処理（searchやrelatedの語の区切り）は、どの言語の入力も受け付けるままにする。
3. **既に残った文字列は書き換えない。** queueに記録済みのcontext・event・noteと、着地済みのcommitのメッセージは英語に直さない。変わるのは実装が入った後に作る文字列だけである。

今の日本語の箇所と、英語に直す実装の状況は[Language](../design/supervisor-lifecycle/language.md)が持つ。

## Alternatives

- **固定の文字列も言語の設定に従って訳す（i18n）**: 言語ごとに文字列とtestを持ち、pluginのskillが引用する文言も言語ごとに要る。人が決めた範囲（AIが書く文だけを選べる）を超える。
- **日本語の箇所をそのまま残す**: 他のrepositoryの人が読めない文字列がqueueとpromptに入る（goal 52の(6)）。
- **記録済みの文字列も英語に直す**: 過去の記録を書き換えることになり、eventを追記だけで扱う前提に反する。

## Consequences

- 日本語を使う人も、runtimeの定型部分は英語で読む。その前後にAIが書く文は、ADR-t616-2の設定で日本語にできる。
- pluginのskillと文書がruntimeの文言を引用している箇所（follow_upのcontextなど）は、実装に合わせて英語に直す。
- 実装は別のtaskで行う。
