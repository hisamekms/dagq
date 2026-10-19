---
id: design-architecture
type: design
title: レイヤーとコンテキストの境界（2つの軸・共有の部品・contextの目的・検査できる規則と検査の範囲）
status: current
created: 2026-10-04
scope: system
related:
  - adr-t1545-1
  - adr-0013
  - design-measurement
  - adr-0032
  - adr-0054
  - adr-t1410-1
  - adr-t1453-1
  - design-overview
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle-supervise
  - adr-t2391-1
  - adr-t2397-1
  - adr-t2796-1
---

# レイヤーとコンテキストの境界

## 概念

### 目的

runtime（src/）の責務を、レイヤーとコンテキスト（context）の2軸で分ける今の境界と、それを守る規則の正本。
決めた理由は[ADR-t1545-1](../adr/2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)決定1をamends）が持つ。
moduleの説明は[overview](overview.md)、集約は[Domain model](domain-model.md)、tableは[Persistence](persistence.md)、supervisorのループは[`supervise`](supervisor-lifecycle/supervise.md)が持つ。
全体のreviewのpjの規則と検査のscriptは、この文書の規則のID（下の「検査できる規則」）を参照し、本文を写さない（ADR-t1545-1決定4）。
境界の記録（tableとstoreのmoduleのcontext・portの公開・境界をまたぐtransaction・今の違反）はこの文書に置かず、コードと、Rustのtestの所有の表と、検査のscriptが読む`.config/`の一覧が持つ（[ADR-t2287-1](../adr/2026-10-12-t2287-1-context-boundary-records-in-code-and-config-lists.md)、[ADR-t2391-1](../adr/2026-10-12-t2391-1-boundary-rules-to-compiler-clippy-and-rust-tests.md)決定3、下の「[検査の範囲](#検査の範囲)」）。

### 全体の流れ

```text
起動部分（src/main.rs → src/compose.rs）: adapterを作り、use caseに注入する
   │
   ▼
infrastructure（src/infrastructure/）: portの実装（SQLite・Git・agent・process・file）
   │ implements
   ▼
application（src/application/）: use caseとport（trait）。時刻・I/Oはportを通す
   │ calls
   ▼
domain（src/domain/）: 集約・値・判断。I/Oを持たない

context: 計画管理 ──T1 claim──▶ 実行と着地 ──T2 着地 / T7 follow_ups──▶ 計画管理
         観測と分析は全てを読む（書かない）
         host運用が全体を起動・停止し、hostの資源を持つ
```

### 責務と境界

- **レイヤー**（ADR-0013決定1）: `domain`・`application`・`infrastructure`・起動部分（`src/compose.rs`と`src/main.rs`）。
  依存は外から内へだけ向く（起動部分 → infrastructure → application → domain）。
- **context**（ADR-t1545-1決定1）: 計画管理・実行と着地・観測と分析・host運用の4つと、どれにも属さない「[共有の部品](#共有の部品)」。
- 1つのmoduleは1つのレイヤーと、多くとも1つのcontextに属する。
  contextはmoduleのpathか所有の一覧（下の「[検査の範囲](#検査の範囲)」）で決まり、決まらないmodule（ループ・起動部分・domainなど）はreviewが見る。

### 不変条件

- domainはI/O・時計・乱数を持たず、applicationはそれらをportで受ける（L1〜L5）。
- 各contextは自分の状態（table・file・eventの種類・`Supervisor`の欄）だけを書き、他のcontextの状態は公開したportと所有の一覧に名指したtransactionでだけ変える（C1・C5・X1）。
- 観測と分析は読むだけ（C2）。

## 2つの軸

- レイヤーの入口は`src/lib.rs`のdoc comment（各レイヤーとレイヤーの外のmoduleの役割）。
- eventの種類の所有するcontextは`src/domain/event_kind.rs`のmoduleのdoc comment、`Supervisor`の欄の所有するcontextは`src/application/supervise/execution.rs`の`Supervisor`のdoc commentが持つ。

## 共有の部品

どのcontextにも属さず、全てのcontextが使ってよいもの。
業務の判断を持たせない（ADR-t1545-1決定1）。

- IDと値: `domain::ids`・`domain::error`・`domain::reason`・`domain::views`・`domain::input`。
- build識別子: `src/build_id.rs`（`build_id`）で、`build.rs`と共有する。
  domainと同じ扱いで、domainとapplicationが参照してよい（L1・L3の例外）。
- 時刻とID生成: `application::ports`の`Clock`（壁時計と単調時計）・`IdGenerator`、実装は`infrastructure::clock`。
- eventの記録: `EventStore`の`record_runtime_event`・`record_queue_event`と種類の`domain::event_kind::EventKind`。
- 人への問い合わせ: `asks`表と`AskStore`（`infrastructure::asks`）。
  読み取りは`queue_reads::shared`。
  askを開くのと、answerを自分の状態に適用するのは、そのaskの`kind`を持つcontextが行う（例: `worker_question`は実行と着地、`plan_review`は計画管理、`update`はhost運用）。
- actorと認可: `domain::actor`・`domain::actor_model`（headlessのjobの行き先の値を含む）・`domain::authorization`（[Authorization](authorization.md)）。
- 名前・出力・共有の規則: `application::naming`、`tracing`のマクロ、`domain::write_rules`（表をまたぐ書き込みの規則）、`domain::language`。
- `src/migration_numbers.rs`: migrationのファイル名の規則で`build.rs`と共有するI/Oの無い関数。
  domainと同じ扱いで、applicationが参照してよい（L3の例外）。

## 計画管理

goal・task・proposalと、その検査と採否（plan review・goal review・runtimeのplanner・follow_upとfindingのproposal）。
taskの再計画の保護とhold（`task_holds`、`domain::task_hold`）も計画管理が持ち、実行と着地はclaim・resume・復旧jobの開始と結果の適用の確定の中で同じ判定（`task_replan::check_step`）を読むだけで書かない。

## 実行と着地

taskをrunにして動かし、検証し、mainへ着地させること（claim・run・session・review・integrate・resume・triageと復旧・e2e）。

## 観測と分析

起きたことを読み、数え、予測し、知らせること（events・watch・stats・KPI・forecast・印・observer・スループットの見直し・CIの見張り）。
他のcontextの状態とeventを読むだけで書かず、計測のportと台帳の係は[計測](measurement.md)が持つ（ADR-t1662-2決定6）。

## host運用

runtime自身をhostで動かし続けること（up・down・install・自動更新・queue service・compileの共有・diskの後始末・hostの計測・inbox）。

## 検査できる規則

規則のIDは変えない。
規則を変えるときはIDを足すか、古いIDを「廃止」と書いて残す。
各規則の「検査」は、script（機械）で見るか全体のreviewで見るかを書く。

### レイヤーの規則

- **L1** `src/domain`のコードはapplication・infrastructure・起動部分とレイヤーの外のmoduleを参照しない。
  `#[cfg(test)]`の中も同じ。
  例外は共有の部品の`crate::build_id`（I/Oを持たない規則と`named_commit`）だけ。
  検査: Rustのtest（`src/context_boundaries/modules.rs`）。
- **L2** `src/domain`の本番のコード（`test=false`のビルドに残りうるコード）はDB・ファイル・process・network・時計・乱数のID・`anyhow`を参照しない（ADR-0013のAlternativesの機械化）。
  検査: 時計・乱数のID・DB・ファイル・外のcommandはclippy（`clippy.toml`の`disallowed_methods`・`disallowed_types`・`disallowed_macros`）、`anyhow`・`std::process`（`std::process::id`を含む）・`std::net`はRustのtest（`src/context_boundaries/modules.rs`、下の「[検査の範囲](#検査の範囲)」）。
- **L3** `src/application`のコードはinfrastructure・起動部分とレイヤーの外のmoduleを参照しない。
  `#[cfg(test)]`の中も同じ（testはapplicationのtest doubleを使う）。
  例外は共有の部品の`crate::migration_numbers`と`crate::build_id`だけ。
  検査: Rustのtest（`src/context_boundaries/modules.rs`）。
- **L4** `src/application`の本番のコードはDB・ファイル・外のcommand・壁時計・乱数のIDを直接使わず、portを通す。
  `#[cfg(test)]`の中でfixtureを作るファイル操作はよい。
  検査: clippy（`disallowed_methods`・`disallowed_types`・`disallowed_macros`）。
- **L5** `src/application`の状態の判断（遷移・回数と上限・送るか・待つか）は、時刻を`Clock`か値の引数で受け、判断の中で時計を読まない（[ADR-t1410-1](../adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)）。
  待ちの判断は、今の時刻と観測を引数に取り次の操作を値で返す副作用のない関数にし、境界の時刻（ちょうど閾値・その1 ms前）をunit testで確かめる。
  時計を読んで結果をportで実行するのは呼ぶ側の薄い処理にする（例: `supervise::execution::session`・`reopen`の待ちの判断）。
  検査: 時計の読み取りはclippy（`disallowed_methods`の`Instant::now`・`Instant::elapsed`）で、時刻は`Clock::monotonic`、経った時間は同じ時計の`Clock::since`で読む。
  呼ぶ側の薄い処理が直接読む所だけ、そのitemに理由付きの`#[expect(clippy::disallowed_methods, reason = "...")]`を置く。
  判断の中で読むかは全体のreview。
- **L6** `src/infrastructure`のコードは`crate::compose`とレイヤーの外のmoduleを参照しない。
  検査: Rustのtest（`src/context_boundaries/modules.rs`）。
- **L7** 起動部分（`src/compose.rs`とその下のmodule）はadapterを作ってuse caseに注入する配線だけを持ち、判断・時刻の読み取り・eventのpayloadの組み立てを持たない。
  検査: 時刻の読み取りはclippy（`disallowed_methods`、`#[expect]`を置かない）、残りは全体のreview。
- **L8** レイヤーの外のmodule（`view`）は起動部分と同じ外側に置き、domain・applicationを使ってよいが、domain・application・infrastructureから参照されない（L1・L3・L6）。
  `build.rs`と共有する`build_id`・`migration_numbers`はレイヤーの外のmoduleに数えず、共有の部品としてL1・L3が名指す例外の範囲で参照してよい。
  新しいmoduleをレイヤーの外に足さない。
  検査: 参照されないことはRustのtest（`src/context_boundaries/modules.rs`）、新しいmoduleを外に足さないことは全体のreview。
- **L9** `src/compose/`の`supervisor`（ループ）以外のmodule（contextごと）は互いと`supervisor`を参照せず、`use super::*`も書かない。
  共有の配線は`src/compose.rs`に置き、外からは`crate::compose`の再公開だけを参照する。
  検査: contextのmoduleどうしと`supervisor`への参照と`use super::*`はRustのtest（`src/context_boundaries/modules.rs`）、`src/compose/`の外からの参照はコンパイラ（可視性。その下のmoduleがprivateであることは同じtestが確かめる）、共有の配線の置き場は全体のreview。

### コンテキストの規則

- **C1** あるcontextの`src/infrastructure`のstoreのmoduleは、自分のcontextのtable（所有の一覧の`table`の項目）と共有の部品のtableにだけ`INSERT`・`UPDATE`・`DELETE`を書く。
  他のcontextのtableを書くのは、所有の一覧の`transaction`の項目の関数の中だけ。
  検査: Rustのtest（`src/context_boundaries.rs`）。
- **C2** 観測と分析のコードは、他のcontextの状態を変えるportのmethod（`TaskStore`・`RunCoordination`・`SupervisorRegistry`・`DraftPlannerStore`の書き込み、`RunTransitions`・`RunRecovery`・`PlanReviewStore`・`GoalReviewStore`）を呼ばない。
  観測と分析が読む他のcontextのportは、読み取りのtrait（`…Reads`）と、それをsupertraitに持つ書き込みのtraitに分かれる。
  検査: 型（観測と分析のuse caseと`ObservationQueue`・`ObservationEnv`は読み取りのtraitだけを取り、書き込みのmethodを呼ぶコードはコンパイルできない）。
  `application::health`のattentionが読む計画管理の`DraftPlannerStore`・`PlanReviewStore`・`GoalReviewStore`と、それを渡すstatusのこれらのportは、計画管理が読み取りのportを公開するまで型で守られず、全体のreviewが見る。
- **C3** `application::supervise`のsubmoduleは、自分のcontextの`Supervisor`の欄か分けた状態だけを変える。
  他のcontextの欄は読むか、そのcontextの関数を呼ぶ。
  `application::supervise`はcontextごとのmodule（ループと実行と着地の`execution`、`planning`・`observation`・`host_ops`）に分かれ、各contextの分けた状態（host運用の登録`Registration`を含む）と`Supervisor`の欄はそのmoduleの中からだけ見え、ループは他のcontextの状態をそのmethodで読み変える。
  検査: 他のcontextの欄を変えることはコンパイラ（可視性）、他のcontextの関数を呼ぶべきところかとsubmoduleを置くcontextは全体のreview。
- **C4** 新しく足す・変えるuse caseは`Box<dyn Queue>`・`&mut dyn Queue`・全体の`QueueOpener`を取らず、要るportだけを取る（ADR-0013決定1の「portは原則applicationに定義する」のまま、幅を狭める）。
  検査: 観測と分析のuse caseは型（C2の読み取りのtraitだけを取る）、ほかのuse caseの切り方は全体のreview。
- **C5** 他のcontextの公開していないport（所有の一覧で内部としたもの、一部公開のportは公開していないmethod）を使わない。
  検査: portの名前の参照はscript（`scripts/check-context-boundaries.sh`）、一部公開のportのmethodの単位の参照とcontextの決まらないmoduleは全体のreview。
- **C6** 他のcontextのeventを読むときは、kindを`domain::event_kind`か所有するdomainのmoduleの定数で名指し、状態の判断に使うpayloadは文字列のkeyではなく型付きの復元の値で読む。
  古いeventの読み取りは保つ。
  検査: 全体のreview。
- **C7** 新しいport・table・storeのmoduleとtransactionの関数は、pathから導けないもの（所有するcontextと公開先）を所有の一覧に足してから置く。
  一覧はコードに無い項目を持たない。
  検査: table・storeのmodule・transactionの関数はRustのtest（`src/context_boundaries.rs`）、portと`src/application/`のmoduleの記録はC5のscriptが一覧を読むときに照合する（C5とともにRustのtestへ移す、ADR-t2397-1決定4）。
- **C8** `src/application/ports/`のmoduleは自分と`shared`のportだけを、moduleのpath（`super::shared::Clock`）で名指す。
  例外は他のcontextを読む観測と分析、`Queue`・`StateStore`の`shared`、`RunLog`を組に名指す`host`。
  検査: script（例外の中身と`crate::application::X`を通す名指しは全体のreview）。

### transactionの規則

- **X1** 複数のcontextの状態を変える1つのtransactionは、所有の一覧の`transaction`の項目にID・書き手・理由を持つものだけ。
  一覧に足す・外す変更は同じ変更で一覧を直す。
  検査: Rustのtest（`src/context_boundaries.rs`、C1とともに）。
- **X2** 境界の規則のためにtransactionを分けない。
  所有の一覧のtransactionの述語（`WHERE status IN (...)`・`renew_lease`・`BEGIN IMMEDIATE`）を外へ出さない（ADR-0013決定9）。
  検査: 全体のreview。
- **X3** 所有の一覧のtransactionの中で、他のcontextの状態を変えるのは、そのcontextのinfrastructureが公開する関数（`infrastructure::sqlite::transition_task`など）を通す。
  SQLの文を書き写さない。
  検査: 全体のreview。

### 検査の範囲

- 各規則の検査は、scriptから、コンパイラ（可視性・型）・clippy・Rustのtest・全体のreviewへ移している途中で、規則ごとの移し先と退役の順は[ADR-t2391-1](../adr/2026-10-12-t2391-1-boundary-rules-to-compiler-clippy-and-rust-tests.md)が持つ。
  crateは1つのままで、crateの分割に委ねた規則（L1・L3・L6・L8・L9のcontextのmoduleどうし・C5・C8）と、L2のうちdomainだけが禁じる`anyhow`・`std::process`・`std::net`はmoduleの参照を構文で読むRustのtestへ移す（[ADR-t2397-1](../adr/2026-10-18-t2397-1-crate-delegated-boundary-rules-kept-in-one-crate-by-a-rust-module-reference-test.md)、[ADR-t2796-1](../adr/2026-10-20-t2796-1-domain-only-l2-items-to-the-module-reference-test.md)）。
  下の記述と各規則の「検査」の行は今動いている検査で、移し先の着地ごとに直す。
- clippyの一覧は`clippy.toml`で、lintはpackageでは切り（`Cargo.toml`の`[lints]`）、`src/lib.rs`がdomain・application・起動部分のmoduleの宣言でtestでないビルドにだけ有効にする（ADR-t2391-1決定2）。
  3つのlintはdomainとapplicationに、`disallowed_methods`は起動部分にも有効にするので、起動部分にはL7の時計に加えて同じ一覧のDB・ファイル・外のcommandの関数も当たる。
  lintの一覧はpackageに1つなので、有効にした全てのmoduleが禁じるものだけを置き、domainだけが禁じるもの（applicationが使う`anyhow`と`std::process::id`を含む`std::process`の残り、`std::net`）はmoduleの参照のRustのtestが見る。
  infrastructure・レイヤーの外のmodule・`src/main.rs`・`tests/`・`#[cfg(test)]`の中にはlintが当たらないので、例外を置かない。
  許す場所はapplicationの個々のitemの理由付きの`#[expect]`だけで、moduleの宣言・crateの全体・domain・起動部分には置かない。
- L1・L3・L6・L8・L9と、L2の`anyhow`・`std::process`・`std::net`は、`cargo test`が流すRustのtest（`src/context_boundaries/modules.rs`）が当てる。
  testは`src/lib.rs`から辿るmoduleのRustを構文で読み、`use`とpathをcrateの根からのmoduleのpathか外のcrateのpathに解き、再公開を定義のmoduleまで辿る（L2は`#[cfg(test)]`の中を数えず、ほかは数える）。
  共有の部品を参照してよいレイヤー、`src/compose/`のループのmodule、今の違反は所有の表に置く。
- `scripts/check-layer-deps.sh`は、`src/application/ports/`のcontextのmoduleのC8を当てる。
  数えるのは参照のpathで、コメントと文字列は数えない（細目はscriptの先頭のコメント）。
  許可の一覧は`.config/layer-deps-allow.txt`。
- 所有の一覧は2か所に分かれ、pathから導けない所有だけを持つ。
  Rustのtestの側の所有の表`src/context_boundaries/ownership.rs`は、tableの所有、storeのmoduleのcontext、境界をまたぐtransactionの関数、C1とL1・L2・L3・L6・L8・L9の今の違反を持つ。
  `.config/context-boundaries.txt`は、`src/application/`のmoduleのcontext、portとそのmethodの公開先、`ports/`の外のportを持つ。
- C1・C7のtableとstoreのmoduleとtransaction・X1は、`cargo test`が流すRustのtest（`src/context_boundaries.rs`）が当てる。
  testは`src/infrastructure/`のRustを構文で読み、test codeの外の文字列の中のSQLの書き込み先を所有の表と照合する。
  どのmigrationも作らないtableの行、migrationが残すのに行の無いtable、コードに無いstoreのmoduleとtransactionの関数でも落ちる。
  共有の部品のtableは、どのcontextが書いても違反に数えない。
  migrationのtriggerが書く`search_index`・`landed_commits`は数えない。
- `scripts/check-context-boundaries.sh`は、C5と、C5が読む`.config/context-boundaries.txt`とコードの食い違い（一覧に無いport、コードに無い一覧の項目）を当てる。
  storeのmoduleのcontextは、所有の表の`store`の行から読む。
  共有の部品のportは、どのcontextが使っても違反に数えない。
  `src/application/ports/<context>.rs`のportの所有と、`ports/`・`queue_reads/`・`compose/`の`<context>.rs`のcontextは、scriptがpathから導く。
  許可の一覧は`.config/context-boundaries-allow.txt`。
- 所有の表の書式と落ちる条件はtestのdoc comment、`.config/`の一覧の書式と落ちる条件は各一覧の先頭のコメント、scriptの数え方の細目はscriptの先頭のコメントが持つ。
- 今ある違反は、理由と行き先のtask（行き先が未登録なら、所有の表では`Fix::Unregistered`、`.config/context-boundaries-allow.txt`では`none`と理由）を持つ許可の一覧にだけ置く（ADR-t1545-1決定4、ADR-t2287-1、ADR-t2391-1決定3）。
  C1とL1・L2・L3・L6・L8・L9の許可は所有の表に、C5・C8の許可は`.config/`の許可の一覧に置く。
  許可の一覧に無い違反と、違反が消えた古い項目で落ちるので、違反を直す変更は同じ変更で許可の一覧の項目を消す。
  reviewだけで見る規則の今の違反は文書に記録せず、行き先はqueueのtaskが持つ。
- 2つのscriptは、CIと`src/`を変えるtaskのverifyが流す。
  `scripts/check-context-boundaries.sh`は、runのreviewのプログラム（[プログラムのreview](supervisor-lifecycle/review-programs.md)、どのscriptを流すかは`dagq.toml`の`[review.programs]`）としてもLLMのagentより前に流れる。
- 「検査」が全体のreviewを名指す部分と、scriptとtestが見ない部分（C5の一部公開のportのmethodの単位の参照、contextの決まらないmodule、実行時に組み立てるSQLなど。全体はscriptの先頭のコメントとtestのdoc comment）は、全体のreview（[Review](supervisor-lifecycle/review.md#reviewの流れ)）が、規則のIDを名指すこのrepositoryのpjの規則`.dagq/review/builtin-acceptance.md`に従って、差分が足したものだけを見る。
  scriptとtestの規則と、所有の一覧への記録の漏れは見ない。
