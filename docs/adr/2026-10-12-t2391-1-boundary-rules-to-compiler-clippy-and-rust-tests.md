---
id: adr-t2391-1
type: adr
title: runtimeの境界の規則（L1〜L9・C1〜C8・X1〜X3）を規則ごとにclippy・型・可視性・crateの分割・Rustのtest・全体のreviewへ移し、reviewのsubagent architecture-boundariesを先に退役させ、2本の検査のscriptは移し先が着地した規則から削って最後に消す（ADR-t1545-1決定4、ADR-t2287-1決定1・2・3、ADR-t2259-1決定12をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t1545-1 decision 4
  - adr-t2287-1 decision 1
  - adr-t2287-1 decision 2
  - adr-t2287-1 decision 3
  - adr-t2259-1 decision 12
amended_by:
  - adr-t2397-1
  - adr-t2796-1
owners:
  - hisamekms
tags:
  - architecture
  - review
  - testing
related:
  - adr-0013
  - adr-t1545-1
  - adr-t2287-1
  - adr-t2259-1
  - adr-t2389-1
  - adr-t1942-2
  - adr-t1453-1
  - adr-t1410-1
  - adr-t1091-1
  - design-architecture
---

# ADR-t2391-1: runtimeの境界の規則をclippy・型・可視性・crateの分割・Rustのtest・全体のreviewへ移し、architecture-boundariesを先に、検査のscriptを最後に退役させる

## Context

[architecture.md](../design/architecture.md)の境界の規則（L1〜L9・C1〜C8・X1〜X3）は、字句でRustを読み直すshellのscript（`scripts/check-layer-deps.sh`・`scripts/check-context-boundaries.sh`、合わせて約2,000行）と、LLMのreviewのsubagent `architecture-boundaries`が見ている（[ADR-t1545-1](2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)決定4、[ADR-t2287-1](2026-10-12-t2287-1-context-boundary-records-in-code-and-config-lists.md)）。
人の問題意識は「TSならeslintで済む話をshellとLLMで再実装している」こと（request 107）。

本番queueの2026-10-05〜10-12の根拠:

- `architecture-boundaries`の指摘128件のうち実害（A）は2件で、このagentにしか出せなかったAはほぼ0。Codexの時期に104 job・差し戻し23回・計1.9hで、`src/**`のrunすべてで走る。
- scriptはtraitを名前だけで索引し、同名の重複で一覧に無い`pub trait`を見逃した。
- 規則別の指摘はC4 12・C3 11・C5 10・C7 10・C6 9・L7 8・L5 7。

制約（goal 195）: 振る舞いを変えない。境界の検査をshellのscriptやLLMのagentに新しく足さない。dylintのようにhostかtoolchainに新しい道具を要る仕組みは入れない。古いeventの読み取りは保つ。crateの分割はgoal 121（domainのcrateの切り出しの測定）の結論を受けてから決める。

単一のcrateの可視性で守れる範囲は次のとおり確かめた。

- `src/main.rs`とintegration testは別のcrateで、`dagq::application`・`dagq::infrastructure`を600か所あまりで名指すので、レイヤーとcontextのmoduleは`pub`のまま要る。
- contextの内部のport（C5）は、そのcontextのapplicationのmoduleと、traitを実装するinfrastructureのmoduleの両方から見える必要があり、両者に共通の祖先はcrateの根なので、`pub(in path)`は`pub(crate)`と同じ範囲になる。`src/application/ports/`のmoduleどうし（C8）・`src/compose/`のcontextのmoduleどうし（L9の前半）は同じ親の下の兄弟で、privateでも互いに見える。可視性ではC5・C8とL9の前半は守れない。
- `src/compose/`のcontextのmoduleは既にprivateの`mod`で根が再公開するので、`src/compose/`の外から`crate::compose::host`のように名指すこと（L9の後半）は既にコンパイラが落とす。
- `application::supervise`の分けた状態の欄は`pub(super)`で`supervise`の全てのsubmoduleから変えられる。状態と、それを変えるsubmoduleをcontextごとのmoduleの下にまとめ、欄の可視性をそのmoduleに絞れば、他のcontextのsubmoduleが欄を変えること（C3）はコンパイラが落とせる。

## Decision

### 1. 規則ごとの移し先

| 規則 | 移し先 | 理由 |
| --- | --- | --- |
| L1 | crateの分割 | moduleの参照の向きはclippyの`disallowed_*`（itemのpathの禁止）で表せず、上の可視性も`pub`が要るので効かない。domainのcrateが他に依存しなければコンパイラが落とす |
| L2 | clippy | 時計・乱数のID・DB・ファイル・process・network・`anyhow`はpathの決まったitemなので`disallowed_methods`・`disallowed_types`で表せる（決定2）。domainのcrateの分割の後は、DBと`anyhow`の部分はcrateの依存にも現れる |
| L3 | crateの分割 | L1と同じ。共有の部品（`build_id`・`migration_numbers`）の例外はcrateの置き場で表す |
| L4 | clippy | L2と同じ形でapplicationの本番のコードに当てる |
| L5 | 時計の読み取りはclippy、判断の中で読むかは全体のreview | `Instant::now`・`SystemTime::now`の位置は機械で見られる。読むのが判断の中か呼ぶ側の薄い処理かは意味の判断 |
| L6 | crateの分割 | L1と同じ。infrastructureのcrateは起動部分とレイヤーの外のmoduleに依存できない |
| L7 | 時計の読み取りはclippy、判断とeventのpayloadの組み立ては全体のreview | L5と同じ分け方 |
| L8 | 参照されないことはcrateの分割、新しいmoduleを外に足さないことは全体のreview | 外のmoduleを外側のcrateに置けば内側から依存できない。置き場の判断は意味の判断 |
| L9 | `src/compose/`の外からの参照は可視性（既に成り立つ）、contextのmoduleどうしはcrateの分割の判断に委ねる | 前者はprivateの`mod`が落とす。兄弟のmoduleどうしは可視性で守れない |
| C1 | Rustのtest | SQLの書き込み先のtableは型やclippyに現れない（決定3） |
| C2 | 型（portの読み取りと書き込みのtraitの分割） | 観測と分析に読み取りのtraitだけを渡せば、書き込みのmethodを呼ぶコードはコンパイルできない |
| C3 | 可視性（状態の欄をcontextのmoduleに絞る）、残りは全体のreview | 上のContextの確かめ。他のcontextの関数を呼ぶべきところかの判断はreviewに残る |
| C4 | 型（portの読み取りと書き込みのtraitの分割）、use caseの切り方は全体のreview | 要るportだけを取る形はtraitが分かれて初めてsignatureで表せる。どこで切るかは意味の判断 |
| C5 | crateの分割 | 上のContextの確かめのとおり単一のcrateの可視性では守れない。portをcontextのcrateに置き、公開するものだけを`pub`にすれば落ちる |
| C6 | 型（eventのkindを`domain::event_kind::EventKind`で読む） | kindを文字列で比べず型で名指せば、綴りの誤りと一覧に無いkindはコンパイラが落とす。古いeventは知らないkindを読む逃げ道（未知の値）を型に持って保つ |
| C7 | Rustのtest | 所有の記録とコードの食い違いはC1と同じtestが見る |
| C8 | crateの分割、例外の中身（観測と分析が読むためだけか・`shared`と`host`の組）は全体のreview | `ports/`のmoduleどうしは兄弟で可視性では守れない。portをcontextのcrateに置けば名指せる範囲はcrateの依存で決まる。例外が許す用途かは意味の判断 |
| X1 | Rustのtest | C1と同じtestで、複数のcontextのtableを書く関数が所有の記録の`transaction`に名指されているかを見る |
| X2 | 全体のreview | transactionを分けるか・述語を外へ出すかは意味の判断 |
| X3 | 全体のreview | 他のcontextの公開する関数を通すべきかは意味の判断 |

「全体のreview」は全体のreviewのjob（`builtin:acceptance`）で、このrepositoryのpjの規則の`.dagq/review/builtin-acceptance.md`がarchitecture.mdの規則のIDを名指す（[ADR-t2389-1](2026-10-12-t2389-1-docs-check-only-in-the-overall-review.md)と同じ置き場）。
規則の本文はarchitecture.mdの1か所に置き、clippyの設定・test・pjの規則のfileは規則のIDを名指して本文を写さない（ADR-t1545-1決定4の前半は保つ）。

### 2. clippyの形

- 禁止するitem（時計・乱数のID・DB・ファイル・process・network・`anyhow`）の一覧は`clippy.toml`の`disallowed_methods`・`disallowed_types`に置き、各項目の理由に規則のIDを書く。
- lintはpackageの全体では切り、規則の当たるmodule（domain・application・起動部分）の入口で、testでないビルドのときだけ有効にする（`cfg_attr(not(test), ...)`の形）。infrastructure・レイヤーの外のmodule・`src/main.rs`・integration testには当たらない。`--all-targets`のtestのビルドではlintが切れたままなので、`#[cfg(test)]`の中のfixtureのファイル操作（L4）は数えない。
- 今ある違反と許す場所は、その場の理由付きの`#[expect]`（lintが鳴らなくなると落ちる）か`#[allow]`にだけ置き、`.config/layer-deps-allow.txt`の項目はそこへ移す。applicationの`Instant::now`（約85か所）も同じで、減るにつれ`#[expect]`が消える。
- DB・ファイル・processの型まで入れる。crateの分割はgoal 121の後になり、それまでscriptを残さずにL2・L4を機械で見るにはclippyが最も安い。分割の後にcrateの依存で表せる部分は、そのADRがclippyの項目を外すかを決める。

### 3. C1・C7・X1のRustのtest

- 所有の記録（pathから導けないtableとstoreのmoduleのcontext、portとmethodの公開先、境界をまたぐtransactionの関数とID・書き手・理由）は、testの側の1か所の型付きの表に置き、`.config/context-boundaries.txt`と`.config/context-boundaries-allow.txt`は消す。runtimeは所有を実行時に読まないので、本番のコードとbinaryに持たせない。今ある違反は同じ表に理由と行き先付きで置き、行き先のtaskがまだ無いものは未登録と理由を書いてよい（ADR-t2287-1決定2の緩めを保つ）。clippyの`#[expect]`/`#[allow]`の理由も同じ扱いにする。
- testはsrcのRustを字句でなく構文として読み（dev-dependencyのparser。hostの道具は要らない）、storeのmoduleとtransactionの関数の中のSQLの書き込み先を所有の表と照合する。traitを名前でなくpathで識別し、同名の重複で見逃さない。
- `cargo test`のunitの範囲に置き、着地の検証で毎回流れる。
- dylint（独自のlint）は入れない。nightlyとcargo-dylintがhostとtoolchainに要り、workerはhostに道具を入れられず、CIと着地の検証の費用も増える（goal 195の制約）。

### 4. crateの分割はgoal 121の後の別のADR

L1・L3・L6・L8の参照されないこと・L9のcontextのmoduleどうし・C5・C8は、crateの分割で守る。
具体の切り方（crateの数と線、portとcompositionの置き場、共有の部品の置き場、testの置き場）は、goal 121（task 1721・1722）の測定と結論を受けた別のADRのtaskが決める。
ADR-0013とADR-t1545-1決定4の「レイヤーを分けるためだけのcrateの分割はしない」を変えるかもそのADRが決め、この決定は変えない。
それまで、これらの規則はscriptが見続ける（決定5）。
そのADRがcrateに分けない、または一部の規則をcrateで表さないと決めるときは、その規則の別の移し先（clippy・Rustのtest・全体のreview）も同じADRが決め、scriptを消すのはその移し先の着地の後にする。
goal 121が測るのはdomainのcrateだけなので、application・infrastructureの線（L3・L6）を引くのに測定が足りなければ、そのADRのtaskが測るtaskを先に足す。

### 5. 退役の順

1. **`architecture-boundaries`を先に退役させる。** 実害の指摘がほぼ無く、費用は`src/**`の全てのrunで掛かる。同じ変更で`dagq.toml`のこのagentの有効化を消し、意味の判断の規則（L5・L7の残り・L8の置き場・C3の残り・C4のuse caseの切り方・C8の例外の中身・X2・X3）は`.dagq/review/builtin-acceptance.md`が規則のIDで名指す。移し先がまだ着地していないC2・C6と、C5のscriptの見ない部分（一部公開のportのmethodの単位の参照など）は、型とcrateの分割が着地するまで同じfileが名指し、着地した規則から外す。
2. **scriptは移し先が着地した規則から削る。** clippy・型・可視性・Rustのtestの着地ごとに、その規則をscriptと許可の一覧から外す。L9の`src/compose/`の外からの参照はコンパイラが既に落とすので、最初に削ってよい。
3. **最後にscriptを丸ごと消す。** crateの分割（決定4）が着地して残りの規則が移った後に、2本のscriptと`.config/`の一覧、CI・verifyの案内・`dagq.toml`の`[review.programs]`からの参照を消す。

### 6. amendsの範囲

- **ADR-t1545-1決定4**: 「reviewのsubagentと機械的な検査は規則の節への参照と検査項目だけを持つ」の検査の置き場を、コンパイラ・clippy・Rustのtest・全体のreviewのpjの規則に変える。「今ある違反は許可の一覧にだけ置く」を、clippyは理由付きの`#[expect]`/`#[allow]`、C1・C7・X1はtestの表に変える。規則の本文を1か所に置くこと、crateやdirectoryの分割を目的にしないことは保つ。
- **ADR-t2287-1決定1**: pathから導けない所有の記録の置き場を、scriptが読む`.config/`の一覧からtestの側の型付きの表に変える。設計文書に置かないこと、pathから導けるものはコードが持つことは保つ。
- **ADR-t2287-1決定2**: 食い違いとC1・C5・C7・X1を落とすものを、scriptからRustのtest（C1・C7・X1）とcrateの分割（C5）に変える。reviewだけで見る規則の今の違反を文書に記録しないことは保つ。
- **ADR-t2287-1のDecisionの末尾の文**（境界の規則と一覧を`scripts/`・`.config/`・`dagq.toml`・`.dagq/`が持つ）: 決定1・2に従う部分として変え、lintの一覧は`clippy.toml`、許す場所はコードの属性、所有の表はtestが持つ。runtimeの実行時の判断に境界の規則を持たせないことは保つ。
- **ADR-t2287-1決定3**: scriptが見ない部分と意味の判断の置き場を、`architecture-boundaries`から全体のreviewに変える。runのreviewが境界の記録の漏れを文書の欠けとして指摘しないことは保つ。決定4は変えない。
- **ADR-t2259-1決定12**: このrepositoryに残すpjのagentから`architecture-boundaries`を除く。

変える決定はADR-t2287-1の4つのうち3つだが、各決定で変えるのは検査の置き場の句だけで、設計文書に記録を置かないこと・reviewが記録の漏れを指摘しないこと・規則のIDを1か所に置くことは生きている。ADR-t2287-1がADR-t1545-1に対して取ったのと同じ理由で、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)の丸ごとの置き換えでなくamendsにする。

amendsしないADR:

- [ADR-t1942-2](2026-10-07-t1942-2-document-check-and-review-in-both-directions.md): 文書の照合を決め、境界の検査の置き場を決めていない（その決定4の境界の記録の部分はADR-t2287-1決定3がamendsし、今回はADR-t2287-1の側を変える）。
- [ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md): pathで有効にするreviewのsubagentの仕組みで、`architecture-boundaries`もscriptも名指さない。
- [ADR-0013](0013-layered-architecture-and-type-function-style.md): 依存の向きをreviewと棚卸しで見ることはAlternativesの理由で、決定ではない。crateを1つに保つことは決定4のとおり分割のADRが扱う。

## Alternatives

- **dylintで独自のlintを書く**: moduleの参照の向きやSQLの書き込み先も1つの仕組みで見られるが、nightlyとcargo-dylintがhostとtoolchainに要る（決定3）。
- **scriptを残してagentだけを退役させる**: 安いが、字句で読む誤り（同名のtraitの見逃し）と2,000行の保守が残り、人の問題意識を解かない。
- **crateの分割を先に決める**: L1〜L9とC5・C8が一度に片付くが、goal 121の測定の前に線を引くことになり、制約に反する。
- **所有の記録を本番のコードの定義のそばに置く**: 読み手には近いが、tableとtransactionはコードの1か所に定義が無く（ADR-t2287-1のAlternatives）、runtimeが読まない記録をbinaryに持たせることになる。
- **clippyのlintをpackageの全体で有効にし、infrastructureと外側を`#[allow]`で外す**: 許す場所の方が多く、integration testのcrateごとに外す記述が要る。

## Consequences

- 段の実装（`architecture-boundaries`の退役、clippy、`EventKind`、portの分割、Rustのtest、C3の可視性、crateの分割のADR）は後続のtaskが行い、architecture.mdの各規則の「検査」の行は実装の着地ごとに直す。着地までは今の検査が動く。
- clippyの設定・testの表・pjの規則のfileが規則のIDを名指すので、規則のIDを変えない規則（architecture.md）は保つ。
- 全体のreviewのpjの規則のfileは、移行のあいだ境界の規則の分だけ長くなり、移し先の着地ごとに短くなる。
- `#[expect]`の数がapplicationの`Instant::now`の残りを示し、L5の違反の減り方をコードで数えられる。
