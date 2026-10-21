---
id: adr-t2478-1
type: adr
title: cancelの理由の分類コードの一覧・定義・相手を要る理由・flagの綴りはdagq cancelの--helpとコードの定義のそばのdoc commentが持つ（ADR-t947-4決定5をamends）
status: accepted
created: 2026-10-21
updated: 2026-10-21
accepted_on: 2026-10-21
amends:
  - adr-t947-4 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - planning
  - docs
related:
  - adr-t947-4
  - adr-t876-1
  - adr-t598-1
  - design-domain-model
---

# ADR-t2478-1: cancelの理由の分類コードの一覧・定義・相手を要る理由・flagの綴りは--helpと定義のそばのdoc commentが持つ（ADR-t947-4決定5をamends）

## Context

[ADR-t947-4](2026-09-28-t947-4-cancel-carries-a-reason-code.md)決定5は、cancelの理由の分類コードの一覧・定義・相手を要る理由・flagの綴りを`docs/design/`のDomain modelが持つとした。決めた時点では実装が無く、仕様を置く場所がdesignしかなかった。

実装が入り、`dagq cancel`のclapの定義の--helpがflagの綴りと必須の組み合わせを、分類コードの定数と判断（`domain::cancel_reason`）のdoc commentが各コードの意味・補い・読み方を持つようになった。designに同じ一覧を置くと二重になり、コードとずれる。flagの綴り・名前の一覧はコードの定義のそばに書くという文書の規則（[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定2・3）にも合わない。

## Decision

1. **置き場をコードに移す。** 分類コードの一覧・各コードの定義・`--duplicate-of`の相手を要る理由・flagの綴りと必須の組み合わせ・補いと過去のcancelの読み方は、`dagq cancel`の--help（clapのhelpかlong_help）と、分類コードと判断の定義のそばのdoc commentが持つ。`docs/design/`は判断の入口を指すだけにし、一覧を写さない。
2. **一覧の足し引きにADRは要らない**（ADR-t947-4決定5のまま）。コードはlabelとして記録し、知らない値も読める（[ADR-t876-1](2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)決定3）。ADR-0047決定41の`discard`は変えない。ADR-t947-4の決定1〜4は変えない。

## Alternatives

- **designに一覧を残す**: --helpとdoc commentと同じ中身が二重になり、コードを変えるたびに文書の照合が要る。
- **ADR-t947-4を丸ごと置き換える**: 変えるのは置き場だけで、決定1〜4はそのまま有効である。

## Consequences

- コードを足し引きする変更は、--helpとdoc commentを同じ変更で直す。plannerやskillが写す一覧は、それぞれの文書の照合の対象になる。
