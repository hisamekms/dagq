---
id: adr-t624-1
type: adr
title: taskのkindをdagqのrepositoryの構成の4値から、repositoryが自分で名付ける小文字の自由なlabelにし、特定のkindに頼る既定をruntimeに持たない（ADR-0051決定5・15をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0051 decision 5
  - adr-0051 decision 15
owners:
  - hisamekms
tags:
  - runtime
  - operations
related:
  - adr-0051
  - adr-0029
  - adr-t598-1
  - design-domain-model
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-stats
---

# ADR-t624-1: taskのkindをdagqのrepositoryの構成の4値から、repositoryが自分で名付ける小文字の自由なlabelにし、特定のkindに頼る既定をruntimeに持たない（ADR-0051決定5・15をamends）

## Context

taskの`kind`（`add --kind` / `edit --kind`）は、値の集合が`docs`・`plugin`・`runtime`・`ci`に固定されていた。これはdagq自身のrepositoryの構成（文書、Claude Codeのplugin、Rustのcrate、scriptsとCI）と、[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の決定6がAGENTS.mdに置いた`--paths`と`--verify`の組み合わせの種類そのものである。[ADR-0051](0051-kpi-time-series-report-and-push.md)の決定5はこの集合をKPIの種類の軸にし、決定15は作業時間の前後比較の要約を既定で`runtime`の種類だけで行うと決めた。

kindはverificationもscopeも決めず、使われるのは`show` / `list`の表示と、`stats`・`kpi`・`forecast`の集計の軸だけである。goal 52（dagqを他のrepositoryで使えるようにする）で見ると、他のrepositoryにはpluginもcrateも無いことが多く、4値のどれを選んでも意味が通らない。一方でそのrepositoryにも、集計を分けたい変更の種類（frontendとbackend、docsとinfraなど）はある。

## Decision

1. **taskのkindは、そのtaskを登録するrepositoryが自分で名付ける小文字の短いlabelにする。** runtimeは値の集合を持たず、labelの形だけを検査する。形はnoteとfindingのkindと同じslug（小文字の英字・数字・`-`・`_`）で、長さに上限を置く。集計でkindの無いtaskを呼ぶ名前（`unknown`）と全体を呼ぶ名前（`all`）はlabelにできない。既存の4値はこの形に合うので、登録済みのtaskはそのまま読め、今までどおり同じ層に集計される。
2. **`stats`・`kpi`・`forecast`はkindを文字列のまま集計の軸にする。** `kpi --kind`と`[kpi.targets]`の`kind`も任意のlabel（と`unknown`）を受ける。ADR-0051の決定5のうち「値の集合はADR-0029の変更の種類」を、これで改める。kindを1つの根拠にし、nullのtaskを推さずに`unknown`に数え、`all`も出すことは変えない。
3. **runtimeは特定のkindの値に頼る既定を持たない。** ADR-0051の決定15の「作業時間の前後比較の要約は、既定では`runtime`の種類だけで行う」は、「`--kind`が無ければ、比較に現れたkindごとに要約する」に改める。要約をkindごとに分ける理由（種類で桁が違う）は変えず、どのkindが重いかはrepositoryによって違うので、runtimeに1つの値を埋め込まない。見たいkindは`--kind`で選ぶ。設定の項目は足さない（kindごとの要約で、既定の値を設定に置く必要が無くなるため）。決定15のその他（`all`と`kind`・`parallel`・`load`・`build`の層別）は変えない。
4. **dagq自身のrepositoryで使う4値とその意味は、AGENTS.mdの規則にする。** runtimeの`add`のhelpとerrorの文言には、どのrepositoryの構成も書かない。

ADR-0051の決定11（`[run.env]`のhashの印）はkindの値に頼らないので変えない。ADR-0029の決定6はdagqのrepositoryの`--paths`と`--verify`の運用で、kindの値の集合を定めていないので変えない（その種類とkindのlabelの対応はAGENTS.mdが持つ）。

## Alternatives

- **4値を残し、repositoryごとに値の集合を`dagq.toml`で宣言させる**: 宣言の無いrepositoryで`add --kind`が使えず、宣言を変えると過去のtaskの値が集合から外れる。labelの形だけを検査すれば、同じ集計の目的をより少ない仕組みで満たせる。
- **既定の要約のkindを`dagq.toml`の設定にする**: 設定の無いrepositoryで既定が何も要約しないか、存在しないkindを指すことになる。kindごとに要約すれば、設定なしでどのrepositoryでも意味のある出力になる。
- **labelを大文字や空白も許す自由な文字列にする**: `kind=<label>`の層の名前、`--kind`のcomma区切り、目標の表のキーと衝突しうる。noteとfindingのkindと同じslugに揃える。

## Consequences

- 他のrepositoryは自分の構成に合うkindで`stats`と`kpi`を分けて読める。dagqのrepositoryの集計は今までどおり。
- 綴りの揺れ（`doc`と`docs`など）は別のkindとして数えられる。揃えるのはrepositoryの規則（dagqではAGENTS.md）とplan reviewの仕事になる。
- DBの列は元から`CHECK`の無い`TEXT`なので、migrationは要らない。labelの形、検査の関数、`unknown`の扱い、要約の選び方の詳細は[domain-model](../design/domain-model.md)・[`kpi`](../design/supervisor-lifecycle/kpi.md)・[`stats`](../design/supervisor-lifecycle/stats.md)が持つ。
