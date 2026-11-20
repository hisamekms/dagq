---
id: adr-t5267-1
type: adr
title: 夜のITの30秒の天井の許可の一覧を5秒の時間の関門から独立させる（ADR-t5039-1決定3をamends）
status: accepted
created: 2026-11-18
updated: 2026-11-18
accepted_on: 2026-11-18
amends:
  - adr-t5039-1 decision 3
owners:
  - hisamekms
tags:
  - testing
  - performance
  - ci
related:
  - adr-t1707-1
  - development-testing
  - development-task-registration
---

# ADR-t5267-1: 夜のITの30秒の天井の許可の一覧を5秒の時間の関門から独立させる

## Context

[ADR-t5039-1](2026-11-18-t5039-1-integration-tests-are-bound-by-a-time-budget-and-boundary-limits.md)決定3は、全てのITに夜の30秒の天井を置き、その例外を5秒の時間の関門と同じ許可の一覧に求めた。
task 5040が着地した`scripts/check-it-budget.sh`は、夜の天井専用の一覧を読み、5秒の一覧への包含を確かめない。
5秒の関門は足した・本文を変えたITを対象にし、夜の天井は差分で変わらないITも対象にするため、両方の例外を別々に判断できる形を決定に反映する。

## Decision

1. **ADR-t5039-1決定3の30秒の天井の許可の一覧を、夜の天井専用にする。**
   超えてよいITは、`scripts/check-it-budget.sh`が標準で読む`.config/it-ceiling-allow.toml`と`.config/it-ceiling-allow.d/*.toml`を合わせた理由つきの一覧に載るものだけとする。
   新しい項目は足すtaskの`.config/it-ceiling-allow.d/t<task ID>.toml`に書く。
   天井と等しい秒は超過でない。

2. **夜の一覧に載るtestが、5秒の時間の関門の一覧にも載ることは求めない。**
   `.config/it-slow-allow.toml`と`.config/it-slow-allow.d/*.toml`は5秒の関門の許可だけを表し、そこへの掲載だけでは夜の天井の例外にならない。
   夜の一覧への掲載も5秒の関門の例外にはならず、足した・本文を変えたITにはその関門を独立に当てる。
   差分の増え直しを止める許可と、全件の夜の測定で30秒を超えることの許可は対象と判断の機会が違うため、包含や自動の引き継ぎを求めず、必要な側でそれぞれ理由を審査する。

3. **許可の理由はADR-t5039-1決定3の2つ目の箇条と、[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)決定2の今の読み方を保つ。**
   守る境界、または移す予定のtaskかgoalを示し、配線の既存項目は移し替えまでの扱いとする。
   判断のcaseを並べただけでは許さない。
   項目は登録したtaskを持ち、testを直すか移すtaskは同じ変更で自分の項目を外すか直す。
   ADR-t5039-1決定3の目標・5秒の差分の関門・夜の測定の対象と配置は保ち、他の決定も変えない。

## Alternatives

- **両方の関門で5秒の一覧を共有する。** 5秒の例外がそのまま30秒の例外になり、夜の天井を超える必要性の審査を分けられないため採らない。
- **夜の一覧を5秒の一覧の部分集合にする。** 夜の測定で見つかった既存のITにも5秒の例外の登録を必須にするが、足した・変えたITを対象とする関門の許可とは役割が違う。包含の維持を条件にせず、それぞれの関門で判断する。

## Consequences

- 夜の天井の例外は専用の一覧で審査し、5秒の許可だけで全件の30秒の見張りを免れない。
- 両方の例外が要るtestはそれぞれの一覧を維持する必要がある。包含の検査で補うのでなく、両方に同じ許可理由の条件を当てる。
- 今の開発規則は[testing.mdの「CI」](../development/testing.md#ci)と[task-registration.mdの「推奨の組み合わせ」](../development/task-registration.md#推奨の組み合わせ)が持ち、両者の専用の一覧の記述を保つ。
