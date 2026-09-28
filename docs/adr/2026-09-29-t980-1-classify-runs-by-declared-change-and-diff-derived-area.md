---
id: adr-t980-1
type: adr
title: taskのkindを廃止し、plannerが宣言する変更の種類（change）と着地の差分から読むときに求める変更の対象（area）の2軸でrunを分類する（ADR-t624-1を置き換え、ADR-0051決定5・15をamends）
status: accepted
created: 2026-09-29
updated: 2026-09-29
accepted_on: 2026-09-29
supersedes:
  - adr-t624-1
amends:
  - adr-0051 decision 5
  - adr-0051 decision 15
owners:
  - hisamekms
tags:
  - runtime
  - operations
  - measurement
related:
  - adr-0051
  - adr-0029
  - adr-0069
  - adr-0080
  - adr-t598-1
  - design-domain-model
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-stats
---

# ADR-t980-1: taskのkindを廃止し、plannerが宣言する変更の種類（change）と着地の差分から読むときに求める変更の対象（area）の2軸でrunを分類する（ADR-t624-1を置き換え、ADR-0051決定5・15をamends）

## Context

[ADR-t624-1](2026-09-27-t624-1-task-kind-is-a-free-label.md)で、taskの`kind`はrepositoryが名付ける小文字のlabel 1値になった。dagqのrepositoryでは`docs`・`plugin`・`runtime`・`ci`の4値を使うが、これは検証の重さ（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の`--paths`と`--verify`の組み合わせ）の呼び名を兼ねており、変更の種類（機能・修正・test・測定など）も、変更の対象（runtimeのどの部分か、brokerのcrateか）も区別できない。完了した524件のうちkindが付いているのは154件で、うち121件が`runtime`だった。2026-09-28 22時の着地7件の分析では、「測定のdocs 3本とtestだけの修正2本が短く流れた」ことをtitleを読んで分けるしかなかった。

kindはclaim・検証・pathsの検査には使われず、読むのは`stats`・`kpi`（層・`--kind`・目標の`kind`）・`kpi --compare`の要約・`forecast`の分布・レポートの層だけである。

変更の種類は差分からは分からない（同じ`src/`の差分が機能にも修正にもなる）ので、書いた者が宣言するしかない。一方、変更の対象は着地したcommitの差分が事実として持っており、宣言させると宣言と差分が食い違いうる。着地commitのファイルをgitから読む先例は、衝突の多いファイルの判定（[ADR-0069](0069-do-not-claim-tasks-overlapping-hot-files.md)、[ADR-0080](0080-supervisor-rereads-conflicts-config.md)の`conflict_hotspots`）にある。

## Decision

1. **taskの`kind`を廃止し、CLI・コード・DBの列（`tasks.kind`）まで消す。** 列の削除は非互換のmigrationでよい（ベータのうちは非互換の変更を許すと2026-09-28に人が決めた。installは非互換のmigrationの手順で人の了承を得る）。
2. **runの分類は2つの軸にする。**
   - **change**: plannerがtaskに宣言する変更の種類で、1 taskに1つ。値の集合はrepositoryが`dagq.toml`で決め、決めてあれば`add`と`lint`はchangeを必須にし、集合の外の値を拒む。決めていなければ形（決定6(a)）だけを検査する。
   - **area**: 着地したcommitの差分のファイルを、`dagq.toml`の対応表（areaの名前→globの並び）に通して、読むときに求める変更の対象。1 runに複数付きうる。対応表のglobは重なってよく、1つのファイルが複数のareaに入ってよい（`src/`の下のcomponentのような細かい区分も、対応表に行を足すだけで作れる）。
3. **保存するのは宣言（change）と事実（着地commit）だけにし、areaはeventにも列にも保存しない。** areaは集計のたびに着地commitの差分をgitから読んで求める（`conflict_hotspots`と同じ）。対応表を直すと、過去の全runが同じ基準で分類し直される。
4. **runtimeはchangeとareaの値の集合を持たない。** 名付けるのはrepositoryで（ADR-t624-1の決定1を引き継ぐ）、runtimeは`dagq.toml`に書かれた集合と対応表を使うだけにする。
5. **置き換え（changeとarea）が固定バイナリに入り、repositoryの規則（dagqではAGENTS.mdとpluginのskill）が`--change`に切り替わってから、kindを消す。** 分類できない空白の期間を作らない。
6. **ADR-t624-1の決定のうち変えないものを引き継ぐ。**
   - (a) labelは小文字の短いslugにする。形はnoteとfindingのkindと同じ（小文字の英字・数字・`-`・`_`）で、長さに上限を置く。集計で値の無いものを呼ぶ`unknown`と全体を呼ぶ`all`はlabelにできない。これをchangeの値とareaの名前の両方に当てはめる。
   - (b) runtimeは特定の値に頼る既定を持たない。`kpi --compare`の作業時間の前後比較の要約は、軸の指定が無ければ、比較に現れた値ごとに出す。これをchangeに当てはめ、areaで集計できる出力ではareaにも当てはめる。
   - (c) repositoryが使う値とその意味はrepositoryの規則（dagqではAGENTS.md）にし、runtimeのhelpとerrorの文言にはどのrepositoryの構成も書かない。
7. **ADR-0051の決定5と決定15を、kindの軸からchangeとareaの軸に改める。**
   - 決定5（KPIの種類の軸）: 種類の軸はtaskの`kind`列ではなく、宣言したchangeと着地commitから求めるareaにする。changeが無いtaskと、着地commitの無いrunや対応表のどのareaにも入らない差分は、推さずに`unknown`に数える（changeを差分やpathsから推さず、areaを宣言から推さない）。全体（`all`）も出すことは変えない。
   - 決定15（層別と前後比較の要約）: 層別の軸の`kind`をchangeとareaに置き換える。`parallel`・`load`の帯・`build`の層別は変えない。作業時間の前後比較の要約は決定6(b)に従う。

ADR-0029の検証の選び方（`--paths`と`--verify`）はこの決定では変えない。検証の重さとchange・areaの対応を決める仕組みは作らない。

## Alternatives

- **kindを残し、changeとareaを足す**: 3つ目の軸が検証の重さの呼び名として残り、plannerは同じtaskに重なる2つの宣言を書くことになる。検証の重さは`--paths`と`--verify`がすでに持つので、kindを残す理由が無い。
- **areaもplannerに宣言させる**: 宣言が差分と食い違いうるうえ、宣言の無い過去のtaskを分類できない。差分は事実なので、そこから求める。
- **着地のときにareaを求めてeventに保存する**: 対応表を直したときに過去のrunが古い基準のまま残り、前後比較が基準の変化を混ぜる。読むときに求めれば全runが同じ基準になる。gitを読む費用は`conflict_hotspots`と同じ程度で受け入れる。
- **changeを差分から推す**: 同じファイルの変更が機能にも修正にもなり、推す規則が別の分類になる（ADR-0051決定5が`--paths`から種類を推さない理由と同じ）。

## Consequences

- `stats`・`kpi`・`forecast`・レポートは、kindの代わりにchangeとareaで層に分けて読める。過去の着地runにも、gitの着地commitからareaが付く。
- areaの集計はgitの読み取りを伴うので、着地commitがgitから読めない（履歴を失ったcloneなど）runは`unknown`になる。
- kindの列を消すmigrationは非互換なので、固定バイナリの入れ替えは人の了承を経る。
- 欄名・flagの綴り・`dagq.toml`の書式・`unknown`の扱いと要約の選び方の詳細は、後続のtaskが[domain-model](../design/domain-model.md)・[`kpi`](../design/supervisor-lifecycle/kpi.md)・[`stats`](../design/supervisor-lifecycle/stats.md)に書く。
