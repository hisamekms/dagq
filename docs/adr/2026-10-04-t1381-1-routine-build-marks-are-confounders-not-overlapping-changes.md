---
id: adr-t1381-1
type: adr
title: kpi --compareの「重なった変更」に、parallelを変えないsupervisorの起動・引き継ぎ・停止とbuildだけの導く印（日常の印）を入れず、confoundersに並べるだけにする
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0051 decision 16
owners:
  - hisamekms
tags:
  - runtime
  - operations
related:
  - adr-0051
  - adr-0045
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-marks
  - plan-headless-default-evaluation
---

# ADR-t1381-1: kpi --compareの「重なった変更」に、parallelを変えないsupervisorの起動・引き継ぎ・停止とbuildだけの導く印（日常の印）を入れず、confoundersに並べるだけにする

## Context

`kpi --compare`は印の並びを時刻の順にたどり、前の印からその印までに終わったrunが`min_samples`（5）に満たなければ1つの「重なった変更」にまとめる（[ADR-0051](0051-kpi-time-series-report-and-push.md)決定16）。このrepositoryは`up --auto-update`でruntimeの着地ごとにsupervisorが引き継ぎ（`supervisor_started`、`handoff: true`）、間に終わるrunが5本に満たないことが多いので、人の印が引き継ぎの印と鎖のようにつながる。

2026-10-02T11:30Zに`kpi --compare 61916 --area runtime`を読むと、境の変更は07:47:15Z〜11:29:26Zの10個の印（`supervisor_started` 9と印61916）で`separable: false`、後の窓は最後の引き継ぎ11:29:26Zから始まって`runs` 0だった。範囲の`overlapping`には、89個の`supervisor_started`を含む09-27 01:06Z〜22:39Zのまとまりなど18のまとまりがあり、人の印（`mark_recorded`）はどれも引き継ぎとまとめられていた。自動更新が続くかぎり人の印の後の窓の始まりがずれ続け、「markして`kpi --compare <印>`で効果を確かめる」手順が使えない。task 1368は[headless-default-evaluation](../plans/headless-default-evaluation.md)の3で`--compare A..B,C..D`の窓の明示に頼った。

buildの違いは前後比較の`build=`の層（決定15）がすでに分けて読める。

## Decision

1. **日常の印。** supervisorのbuildを記録するだけの次の3つを「日常の印」と呼ぶ。印のkindとdetailから判定し、新しいeventもmigrationも足さない。
   - (a) 時刻の順で直前の`supervisor_started`と`parallel`が同じ`supervisor_started`（buildだけが変わった引き継ぎ・起動と、何も変わらない起動し直し）。直前の起動が無い最初の起動と、どちらかに`parallel`の記録が無い起動は日常の印にしない。
   - (b) `supervisor_stopped`。
   - (c) `derived:dagq_version`。
2. **日常の印は「重なった変更」をつくらず、つながず、境の変更にも入れない。** 日常の印は今までどおり`confounders`に`position`つきで並べ、buildの違いは`strata`の`build=`の層で読む。それ以外の印（`mark_recorded`、`run_env_changed`、`parallel`を変える`supervisor_started`、`derived:parallel`・`claude_version`・`codex_version`・`toolchain`）はADR-0051決定16の規則でまとめ、そのときの「間に終わったrun」は日常の印を飛ばした前の印からその印までで数える。
3. **日常の印そのものを指した`--compare`。** `--compare`が日常の印（`supervisor_started`のevent IDや、`derived:dagq_version`を読んだclaimのevent ID）を指したときは、その印1つを境に`separable: true`で比べ、他の印（日常の印を含む）は`confounders`に並べる。
4. **導く印を複数出したclaimを指した`--compare`。** 1件のclaimから複数の導く印が同時に出たとき、`--compare <claimのevent ID>`は次のとおりに読む。
   - (i) そのclaimの導く印に日常でない印（`derived:parallel`・`claude_version`・`codex_version`・`toolchain`）が1つでもあれば、claimはその日常でない印を指す。`split`は日常でない印を含む重なった変更のまとまり（決定2のまとまり。同じclaimの日常でない印どうしは間のrunが0なので1つにまとまる）で、同じclaimの`derived:dagq_version`は`split.marks`に入れず、`confounders`に`position: "after"`で並べる（時刻は境と同じだが、新しいbuildは後の窓に効くので`after`とし、buildの違いは`build=`の層で読む）。
   - (ii) そのclaimの導く印が日常の印（`derived:dagq_version`）だけなら、決定3のとおりその印1つを境に`separable: true`で比べる。
5. **変えないもの。** 取り消された印と取り消しの印を区切りに使わないこと、`--compare A..B,C..D`、時刻のcursor（その時刻に効いた日常でない印があればそのまとまり、日常の印だけならその印1つ）、区間を自動では縮めないこと（決定14）は変えない。

## Alternatives

- **人の印を境にするときだけまとめない**: 人の印を指したときだけ効き、引き継ぎの印どうしは日単位のまとまりを作り続ける（`overlapping`の18のまとまりは残る）。導く印や`run_env_changed`を指したときも同じ問題が残るので採らない。
- **自動更新の引き継ぎの印を記録しない・`kpi`から外す**: 引き継ぎの時刻は前後比較の交絡として読みたい（buildの層だけでは、窓の中にいつ引き継いだかが見えない）。ADR-0051決定10の記録する印を減らすことになるので採らない。
- **`min_samples`を下げる・まとめる規則を時間で決める**: 日常の印の間隔は着地の間隔で決まり、値を変えても着地が続けば鎖は残る。決定16の意図（3分差の2つの変更を分けて比べたことにしない）を弱めるので採らない。

## Consequences

- 着地ごとに自動更新する運用でも、人の印の前後に日常の印しか無ければ`kpi --compare <印>`は印1つで`separable: true`になり、後の窓は印の時刻から始まる。headless-default-evaluationの3は`kpi --compare 61916`でも読める（窓の明示のコマンドはそのまま有効）。
- `parallel`を変える起動、`[run.env]`の変化、外部のツールの導く印、人の印は今までどおりまとめるので、決定16の意図は保つ。
- 日常の印の前後で起きたbuildの違いは、境の変更の一部として分けられないと示されなくなる。`confounders`と`build=`の層で読む。
- 今の仕様は[kpi](../design/supervisor-lifecycle/kpi.md)の「前後比較（`compare`）」に置き、実装は`domain::kpi::compare`。
