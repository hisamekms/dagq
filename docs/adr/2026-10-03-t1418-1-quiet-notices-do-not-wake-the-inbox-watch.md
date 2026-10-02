---
id: adr-t1418-1
type: adr
title: 知らせるだけのattentionのうちupdate_installedと毎時のスループットの見直しはinboxのwatchを起こさず、次に起きたときにまとめて届ける（ADR-t996-1決定3をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amends:
  - adr-t996-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - inbox
  - operations
related:
  - adr-t996-1
  - adr-0073
  - adr-0016
  - design-supervisor-lifecycle-events-watch
  - design-supervisor-lifecycle-throughput-review
  - design-supervisor-lifecycle-auto-update
---

# ADR-t1418-1: 知らせるだけのattentionのうちupdate_installedと毎時のスループットの見直しはinboxのwatchを起こさず、次に起きたときにまとめて届ける（ADR-t996-1決定3をamends）

## Context

常駐のinboxはwatchが返るたびに会話の全体を読み直す1 turnを使うので、tokenの消費は「起きる回数 × contextの長さ」で決まる。2026-10-02 JST 0時からのinbox宛てのattention 122件のうち、自動更新の成功（`update_installed`）が47件、ほぼ毎時のスループットの見直しの知らせが12件で、どちらも人に知らせるだけで操作を求めない。`update_installed`は人が報告不要と決め、inboxはwatchを回し直すだけにしている。2026-10-02に人が「inboxの常駐のtoken消費が大きい」と言い、plannerが出した回避法のうち「知らせだけではwatchを起こさない」を、対象をこの2つに絞って選んだ。

## Decision

1. **知らせるだけのattentionのうち、自動更新の成功と毎時のスループットの見直しの結論は、単独ではinboxのwatchを起こさない。** これらが届いても、inboxのwatchは他のattentionかsupervisorの健全性の変化が来るまで待ち続ける。
2. **起こさない知らせも消さず、次に起きたときにまとめて届ける。** watchが返るときは、起こさなかった知らせも含めてcursorより後のinbox宛てのattentionを古い順に全部返す。時間切れで返るときは何も返さず、cursorを進めない。
3. **日次・週次の見直しの結論と、頻度を問わない見直しの失敗は今までどおりinboxを起こす。** 人の目が要る頻度の低い知らせと、手当てを要する知らせだからである。
4. **何がattentionかは変えない。** `events`・`status`・plannerのwatch・roleを付けないwatchに見えるもの、`next`、KPIの数え方はそのままで、変わるのはinboxのwatchが起きる条件だけである。

ADR-t996-1の決定3（結果をinbox宛ての知らせるだけのattentionで届ける）のうち、毎時の見直しの結論の届け方を「inboxを起こさず、次に起きたときにまとめて届ける」に改める。知らせるだけのattentionであること、askにしないこと、日次・週次の届け方は変えない。対象のeventのkindと頻度の値、watchの返し方は[events-watch](../design/supervisor-lifecycle/events-watch.md)が持つ。

## Alternatives

- **知らせるだけのattentionを全て起こさない**: 日次・週次の見直しは人が読むために作ったもので、他の件が来るまで届かないと半日以上遅れうる。見直しの失敗は手当てが要る。
- **知らせをattentionから外す**: `status`の`next`と`events`から消え、KPI（`attentions_per_landing`など）の数え方も変わる。人がまとめて読む経路も無くなる。
- **inboxのmodelやeffortを軽くする**: 起きる回数は減らない。別案として残す。

## Consequences

- inboxが起きる回数は、2026-10-02の割合で約半分（122件のうち59件）減る見込み。
- 自動更新の成功と毎時の見直しの結論は、次の別の件と一緒に届くまで遅れる。どちらも操作を求めないので、遅れによって止まるものは無い。
