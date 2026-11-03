---
id: adr-t3924-1
type: adr
title: CIの見張りは宣言したworkflowの古さを着地先のbranchの最新の成功した実行の時刻で測り、上限の手前でinboxにattentionを出してworkflow_dispatchで1回だけ起動し、新しい成功で閉じ、claim・着地の保留・一覧・赤と緑には効かせない
status: accepted
created: 2026-11-03
updated: 2026-11-03
accepted_on: 2026-11-03
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - ci
related:
  - adr-t1920-1
  - adr-t3807-1
  - adr-t3841-1
  - adr-t2268-1
  - design-supervisor-lifecycle-ci-watch
  - design-supervisor-lifecycle-run-environment
---

# ADR-t3924-1: CIの見張りは宣言したworkflowの古さを最新の成功で測り、上限の手前で知らせて1回だけ作り直させる

## Context

projectには、定時のworkflowが作る成果物を着地の検証が読むものがある（このrepositoryではITの対応表）。
成果物には使える上限の古さがあり、超えると着地の検証が重くなる。
[ADR-t3807-1](2026-10-31-t3807-1-ci-watch-reads-declared-scheduled-workflows-and-files-their-failures-as-findings.md)の定時のworkflowの見張りは実行の成否だけを読み、cancelledの実行を何も決めないものとして扱う。
runnerの不調で定時の実行がcancelledで続くと、成果物が上限を超えても誰も気づかない。
実際にこのrepositoryで約12時間気づかれなかった。
runtimeはprojectのworkflowの名前も成果物の書式も上限も持たない（[ADR-t2268-1](2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)）。

## Decision

1. **projectが宣言したworkflowだけを見張る。** `[ci_watch]`に、古さを見張るworkflowと、その上限と、上限の何時間前に知らせるかを宣言できる。
   宣言の無いprojectは今と何も変わらない。
2. **古さは最新の成功した実行の作られた時刻で測る。** 実行の成否の並びではなく、着地先のbranchの、どのeventでも`success`で終わった最新の実行を読む。
   成果物はその実行の中で作られるので、それより新しく、runtimeは成果物の書式を読まずに済む。
   cancelled・failure・runnerの変更で成功しない実行は古さを延ばすだけなので、成否の種類によらず拾える。
   成功が1つも無いときと、時刻が読めないときは古いとみなす（安全側）。
3. **上限の手前で1回知らせ、1回だけ作り直させる。** 最新の成功が「上限−手前の時間」より古いと、inboxへのattentionを1件出し、そのworkflowを`workflow_dispatch`で1回だけ起動する。
   1回の判断はworkflowと最新の成功の組ごとにqueueで一意の記録にし、記録を書けたsupervisorだけが起動する。
   次の確かめと別のsupervisorは、同じ成功について起動を重ねない。
   起動しても直らなければ（起動の拒否も含めて）attentionが残り、inboxが人に知らせる。
   繰り返しの起動はしない（壊れた実行を周期で積まないため）。
4. **新しい成功で閉じる。** 記録した成功より新しい成功の実行が読めたら、attentionを閉じる記録を書く。
   上限を超えたままなら残る。
   宣言を外したworkflowのattentionは`status`に出さない。
5. **効かせないもの。** 古さの見張りは、claimと着地の保留、既に落ちているtestの一覧、赤と緑の状態と転換、着地の検証を変えない（ADR-t3807-1決定4と同じ扱い）。
   宣言したworkflowを読めない（`gh`の失敗）ときは見張りの一時の失敗に数えず、logに残して次の確かめで読み直す。
6. 変えないもの: ADR-t1920-1とADR-t3807-1の決定。古さの見張りは同じ確かめの周回に足すだけである。

## Alternatives

- **定時の実行のcancelledをfindingにする**: cancelledは成否の決まらない実行で、続けて起きても1回では古さが分からず、runnerの変更や実行が始まらないときを拾えない。
- **成果物（artifact）の時刻を読む**: 成果物の名前と書式をruntimeが知ることになる（ADR-t2268-1）。
- **上限を超えるたびに起動し直す**: 壊れたworkflowを周期で起動し続ける。人に知らせるattentionで十分に止まる。
- **古いときにclaimか着地を止める**: 古い成果物でも検証は重くなるだけで壊れないので、止める理由にならない。

## Consequences

- このrepositoryで使うには、固定バイナリがこの設定を読めるようになってから`dagq.toml`に宣言を足す（別のtask）。
- `workflow_dispatch`の無いworkflowを宣言すると起動は拒まれ、attentionだけが残る。
- keyの名前と既定、eventの種類と欄、attentionの対処は[CI watch](../design/supervisor-lifecycle/ci-watch.md)の「成果物の古さ」から辿るコードのdoc commentが持つ。
- 配線のtestはADR-t3841-1のプロセスの中の層に置き、新しいITを足さない。
