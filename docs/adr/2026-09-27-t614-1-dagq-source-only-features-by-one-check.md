---
id: adr-t614-1
type: adr
title: dagqの開発でだけ要る機能を、queueのrepositoryがdagqのソースかの判定1つで有効にし、ソースでないrepositoryではmigrationの振り直し・--fromなしのinstall・source buildの自動更新・cargo専用の計測を動かさない（ADR-0067決定3・ADR-0073決定14・17をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0067 decision 3
  - adr-0073 decision 14
  - adr-0073 decision 17
owners:
  - hisamekms
tags:
  - runtime
  - operations
  - release
related:
  - adr-0067
  - adr-0073
  - adr-t598-1
  - adr-t614-2
  - design-supervisor-lifecycle-source-repository
---

# ADR-t614-1: dagqの開発でだけ要る機能を、queueのrepositoryがdagqのソースかの判定1つで有効にし、ソースでないrepositoryではmigrationの振り直し・--fromなしのinstall・source buildの自動更新・cargo専用の計測を動かさない（ADR-0067決定3・ADR-0073決定14・17をamends）

## Context

dagqはこのrepository自身の開発（dogfooding）でしか使われてこなかったので、dagqの開発でだけ意味を持つ機能が、どのrepositoryのqueueでも動く形で入っている（goal 52の(2)、2026-09-26〜27の調査）。

- [ADR-0067](0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)決定3の`integrate`のmigrationの番号の振り直し。振り直すのはdagqに組み込まれたqueueのschemaのmigrationのためで、他のrepositoryの`migrations/`（そのprojectのDBのもの）は番号の規則も意味も違う。今は`migrations/NNNN_<name>.sql`の形のファイルがあれば、どのrepositoryでも`git mv`してcommitしうる。
- [ADR-0073](0073-kind-additions-are-compatible.md)決定14の`--from`なしの`dagq install`は、queueのrepositoryのmain checkoutを`cargo build --release --locked`してできた`dagq`を固定バイナリにする。dagqのソースでないrepositoryでは、buildが失敗するか、別のバイナリを`dagq`として入れ替えてしまう。
- ADR-0073決定17の`up --auto-update`は、着地のたびにqueueのrepositoryをsource buildして入れ替える。ソースでないrepositoryでは同じく意味を持たない。
- `stats`・KPI・worktimeには、cargoとこのrepositoryの検証の形（`cargo llvm-cov`・全体の`cargo test`・`--test e2e`）を前提にした分類と数、hostの`rustc`の記録がある。他のrepositoryでは0や`unknown`が並び、読み手を誤らせる。

人は2026-09-27に、これらを設定のopt-inにせず、「queueのrepositoryがdagqのソースか」の判定1つで有効にすると決めた。外部のprojectは`cargo install dagq`で導入するのを基本にする。

## Decision

1. **判定は1つにする。** queueが束縛されたrepositoryのroot（main checkout。queueを開く前に動く`--from`なしの`install`では、buildしようとするcheckout）の`Cargo.toml`が`[package]`の表を持ち、その`name`が`"dagq"`であるとき、そのrepositoryを「dagqのソース」とする。`Cargo.toml`が無い・読めない・形が合わない・`[package]`が無い（workspaceだけ）ときはソースではない（機能を止める側に倒す）。判定は機能を使うたびにその時点のファイルで行い、どこにも保存しない。設定・flag・環境変数で判定を上書きする手段は作らない。
2. **dagqの開発でだけ要る機能はこの判定だけで有効にする。** 対象と、ソースでないrepositoryでの振る舞いは次のとおり。dagqのソースのrepositoryでは、どれも今までどおり動く。
   - (a) `integrate`のmigrationの番号の振り直し（ADR-0067決定3）: ソースでなければ動かない。番号を見ず、振り直さず、番号が埋まっていることを理由に`needs_session`にもしない。runの`migrations/`の変更は他のファイルと同じに扱う。
   - (b) `--from`なしの`dagq install`（ADR-0073決定14のmain checkoutからのbuild）: ソースでなければbuildせずにerrorで止め、`cargo install dagq`で入れ替えるか`--from`でバイナリかcheckoutを指すよう案内する。`--from`を付けた`install`と`--rollback`は判定に関係なく今までどおり動く。
   - (c) `up --auto-update`のsource build（ADR-0073決定17）: ソースでなければ`up`はerrorで止め、supervisorを起動も引き継ぎもしない。自動更新の設定を持つsupervisorが、ソースでないrepositoryのqueueでbuildに進むこともしない。外部のprojectの更新の仕組み（新しいリリースの検知とaskでの入れ替え）は別のADRで決める。
   - (d) `stats`・KPI・worktimeのcargo専用の計測（cargo testやllvm-cov・e2eの分類と回数、integrateと重なる検証の数、hostの`rustc`とtoolchainの記録と、それによる分け方）: ソースでなければ記録も出力もしない。projectの構成に合わせた計測は別のgoalで扱う。
3. **ADR-0067決定3とADR-0073決定14・17の適用範囲を、dagqのソースのrepositoryに狭める。** 3つの決定の中身（振り直しの規則、installの手順、自動更新のきっかけとbuildと入れ替え）は変えない。変えるのは、それが動くrepositoryの範囲だけである。

新しくdagqの開発でだけ要る機能を足すときは、この判定を使い、[Source repository](../design/supervisor-lifecycle/source-repository.md)の対象の一覧に足す。対象の一覧・欄名・errorの文言はdesignが持つ。

## Alternatives

- **設定のopt-in（`dagq.toml`の項目や`up`のflag）**: 人が退けた。dagqの開発でしか使わない機能に設定を増やすと、外部の利用者が意味の分からない項目を見ることになり、付け忘れでdagq自身の運用が黙って変わる。
- **`migrations/`や`build.rs`があるか**: 他のprojectにもよくある名前で、区別にならない。
- **originのURLが`hisamekms/dagq`か**: forkやmirror、SSHとHTTPSの綴り、originの無いrepository（goal 52の目標の形）で外れる。
- **root commitのhash**: shallow cloneで読めず、履歴の書き換えで変わる。
- **`Cargo.toml`の`[package] name`**（採用）: 設定が要らず、dagqのソースならfork・clone・worktreeのどれでも同じ結果になり、1ファイルの読みで済む。無関係のcrateが`dagq`という名前を持つことはcrates.ioの名前が一意なので現実的でなく、forkが判定に当たるのは、そのforkのmigrationも同じdagqのschemaのものなので正しい。

## Consequences

- goal 52の目標の(2)（ソースでないrepositoryでは振り直し・`--from`なしのinstall・source buildの自動更新が動かず、dagq自身のrepositoryでは今までどおり動く）が、この判定1つのtestで確かめられる。
- dagqのソースの`Cargo.toml`の`[package] name`を変えると、上の機能がすべて止まる。名前を変えるときはこのADRを置き換える。
- 外部のprojectは`cargo install dagq`で入れ、更新は別のADRの仕組みか`install --from`で行う。
- 実装は別のtaskで行う。実装が入るまでは、上の機能はどのrepositoryでも動く。
