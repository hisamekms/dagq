---
id: adr-t1091-1
type: adr
title: ADRをamendsで直すか丸ごと置き換えるかを、IDの形でなく元のADRの決定の数と変える範囲で決め、決定を複数持つ新しい形のADRもamendsで直せるようにする（ADR-t598-1決定5をamends）
status: accepted
created: 2026-09-30
updated: 2026-09-30
accepted_on: 2026-09-30
amends:
  - adr-t598-1 decision 5
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - adr-t598-1
  - adr-t828-1
  - adr-t827-1
  - adr-t1063-1
  - adr-t813-2
  - docs-frontmatter
  - adr-index
---

# ADR-t1091-1: ADRをamendsで直すか丸ごと置き換えるかを、IDの形でなく元のADRの決定の数と変える範囲で決め、決定を複数持つ新しい形のADRもamendsで直せるようにする（ADR-t598-1決定5をamends）

## Context

[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定5は、ADRの決定を変えるときは丸ごと置き換え、amendsで直してよいのは既存の4桁のADRのうち決定が多く丸ごとの書き直しが1 taskに収まらないもの（ADR-0047・0044・0073など）だけとした。frontmatter仕様・ADRの索引・template・AGENTS.mdも「新しい形の小さなADRはamendsせず丸ごと置き換える」と書いている。

実際には、決定を複数持つ新しい形のADRがamendsで直されている。

- [ADR-t828-1](2026-09-28-t828-1-coverage-gate-covers-the-workspace-with-workspace-flag.md)が[ADR-t827-1](2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)の決定3をamendsした。
- [ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)が[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)（決定7つ）の決定6をamendsした。これはgoal 73のacceptance (1)で、人が「ADR-t813-2の『worker以外の役割はClaudeだけ』をamendsするADR」と決めたものである。

4桁の側でも、0047・0044・0073ほど大きくないADR（0049・0071・0076・0067など）がamendsで直されており、規則の文面だけが実際から外れている。

決定5が凍結した大きなADRにamendsを認めた理由は、丸ごとの書き直しが重いことと、未完了のtaskや文書が決定番号で引いていることだった。この理由は決定を複数持つ新しい形のADRにもそのまま当てはまる。丸ごと置き換えると変えない決定まで書き直すうえ、決定番号で引いているもの（goal 57のtaskが引くADR-t813-2の決定2・3など）が置き換えられたADRを指すことになる。規則を直さないと、同じfollow_upとplan reviewのreviseが繰り返される（goal 57・73の後の段でADR-t813-2・ADR-t1063-1をさらに直す見込みがある）。

## Decision

amendsで直すか丸ごと置き換えるかを、ADRのIDの形（4桁か新しい形か）でなく、元のADRの決定の数と変える範囲で決める。

- 番号付きの決定を複数持つADRの一部の決定を変えるときは、4桁でも新しい形でも、小さな新しいADRのamendsに変える決定（ADRのIDと決定番号）を書き、元のADRにamended_byを足し、同じ変更で`docs/design/`を今の姿に直す。手順はADR-t598-1決定5が凍結した大きなADRに定めたものと同じ。
- 決定が1つのADRを変えるときと、決定の大半を変えるときは、今までどおり生きている決定を引き継ぐ新しいADRで丸ごと置き換える。
- どちらにするかは、ADRを書くtaskのplannerがdescriptionに書き、plan reviewが見る。

ADR-t598-1の決定2（新しく書くADRは1 ADRに決定1つ、密に結びついた数個まで）と決定9（append-only）は変えない。ADR-t828-1とADR-t1063-1はこの決定で規則どおりになり、書き換えない。

ADR-t598-1自身も決定を12持つ新しい形のADRで、このADRがその決定5だけをamendsすることが新しい規則の最初の適用になる。

## Alternatives

- **例外として個別に認める**: 規則と実際のずれが残り、同じfollow_upとreviseが繰り返される。
- **決定を複数持つ新しい形のADRも丸ごと置き換え続ける**: 変えない決定まで書き直し、決定番号で引く未完了のtaskと文書が置き換えられたADRを指す。人がgoal 73で求めたamendsとも合わない。
- **決定の数の閾値（例: 決定5つ以上）で機械的に分ける**: 変える範囲が大半かどうかは数だけで決まらず、判断はplannerとplan reviewに任せる方が素直。

## Consequences

- 決定を複数持つADRは、4桁でも新しい形でも`amended_by`を持ちうる。読み手は`amended_by`から後のADRを辿るか、designで今の姿を読む（ADR-t598-1決定6のまま）。
- ADR-t598-1決定2で新しいADRは小さく保つので、amendsの連鎖が長くなる対象は主に決定を複数持つADRに限られる。今の姿はdesignが持つので、連鎖は読む側に残らない。
- frontmatter仕様・ADRの索引・template・AGENTS.mdの文書のルールは同じ変更でこの規則に合わせる。
