---
id: adr-t1204-1
type: adr
title: Claude を使わない運転を明示し、未対応の役割は手作業で代行する
status: accepted
created: 2026-09-30
updated: 2026-09-30
accepted_on: 2026-09-30
amends:
  - adr-t813-2 decision 2
  - adr-t813-2 decision 6
  - adr-t1063-1 decision 4
  - adr-t1063-1 decision 5
  - adr-t1063-1 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - provider
related:
  - design-provider-lifecycle
---

# ADR-t1204-1: Claude を使わない運転を明示し、未対応の役割は手作業で代行する

## Context

2026-09-30 に人は、Claude を使わず Codex worker と臨時の inbox の手動差配で開発を続け、未対応の役割を順に Codex に移すと決めた。現状の provider の控えは認証・利用上限などの事実によって始まり、時間や人の回答で解ける。この決定を控えに偽装すると、理由の記録を誤らせ、控えが解けたときに Claude が再開する。

## Decision

1. **人の明示的な運転方針として Claude の起動を禁止できるようにする。** この方針は利用不能の一時的な控えと分け、起動前の確認、worker、全ての job、runtime の planner、inbox、および切り替え先に適用する。禁止先へは戻らず、使える worker が無ければ待つ。既存の supervisor や Claude worker を drain してから切り替え、引き継ぎ・更新で方針を失わない。
2. **使える実装の無い役割は手作業へ渡す。** worker は Codex で動かす。受理した成果は review のために lease を持ち続けず、手動の review と既存の integrate に渡す。計画・復旧の未対応分も inbox が見つけられるようにし、runtime の planner・observer・見直しは自動で起動しない。Codex に対応済みの役割は設定に従って動かし続ける。認証や利用上限を偽造せず、禁止と手動待ちの理由を記録する。
3. **段階的な移行の順序を人が選べる。** goal review の後は、着地を自動化する review、計画を進める plan review、復旧を先に移すことを許す。各役割の Codex 対応は別の task とし、権限の意図、provider・model・session の記録を必須とする既存の決定は維持する。inbox・planner の Codex 対応が揃うまでは人が開いた臨時の session で代行する。

ADR-t813-2 の決定 2・6 と ADR-t1063-1 の決定 4・5 の切り替え先・控えの条件に明示的な禁止を加え、ADR-t1063-1 の決定 7 の順序を上記のとおり緩める。通常の運転の既定は変えない。

## Alternatives

- 認証失敗や利用上限を仮に登録する: 実際の障害と運転方針を混同し、再開条件も違う。
- 全ての役割を一度に Codex に実装する: 移行を始めるまで開発が止まり、役割ごとの検証ができない。

## Consequences

- 手動の review・計画・復旧が運転の前提となり、未処理のものは inbox の attention に残る。
- 禁止した provider のコマンドを実行しないことを、起動経路と共通の executor の両方で守る。これは runtime 自身の運転方針であり、利用者が別の terminal で Claude を起動することまで制限するものではない。
- 具体的な flag、待ちの表示とイベントは [Provider lifecycle](../design/provider-lifecycle.md#claude-を使わない運転) に置く。
