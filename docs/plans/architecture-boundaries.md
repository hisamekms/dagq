---
id: plan-architecture-boundaries
type: plan
title: runtime の責務の分割（goal 100、task 1552〜1558・2158・1650・2212・2232）の前後の構造の指標・変えた module の CI の test の時間・変更と衝突の集中
status: active
created: 2026-10-12
owners:
  - hisamekms
tags:
  - architecture
  - testing
  - measurement
related:
  - design-architecture
  - plan-slow-test-waits
  - plan-integration-to-unit-tests
  - plan-landing-it-selection
  - design-measurement
---

# runtime の責務の分割（goal 100、task 1552〜1558・2158・1650・2212・2232）の前後の構造の指標・変えた module の CI の test の時間・変更と衝突の集中

goal 100 の受け入れ条件 (5) の最終測定（task 1559）。
レイヤーとコンテキストの 2 軸で責務を分けた変更群（task 1552〜1558 と、後から goal 100 に加わった 2158・1650・2212・2232）の前後で、(A) 構造の指標、(B) 変えた module の main の CI の test の時間、(C) 変更と衝突の集中を比べた。
数え方は goal 68 の測定（[slow-test-waits](slow-test-waits.md)・[integration-to-unit-tests](integration-to-unit-tests.md)）の形に合わせ、数える script は [scripts/architecture-boundaries.py](../../scripts/architecture-boundaries.py)、(B) の run ごと・module ごとの値は [architecture-boundaries/ci-modules.csv](architecture-boundaries/ci-modules.csv) に置いた。
src/ と tests/ は変えていない。
本番の queue は `dagq stats` で読むだけで、状態を変えていない。

## 要点

- **(A) 構造は狭まった。** `struct Supervisor` の直接の欄は 102→46、`impl Supervisor` を持つ submodule は 39→20、ports の外で `dyn Queue`（`Box<dyn Queue>`・`&dyn Queue`・`impl Queue`）を取る fn の signature は 89→41、ports の外で全体の Queue の opener を取る signature と欄は 7＋7→4＋3。
  `application/ports.rs`（3,497 行）は 6 つの module（合計 3,961・最大 1,725 行）に、`compose.rs`（4,173 行）は 6 つ（合計 3,992・最大 1,188 行）に、`application/queue_reads.rs`（921 行）は 5 つ（合計 1,834・最大 1,137 行）に分かれた。
  application の `Instant::now` は 114→86（test の外は 112→66）、supervise の payload の文字列 key は 272→173、domain/run は 56→43。
  `supervise/mod.rs` は 3,413→4,029 行と増えた（下の表の注）。
- **(B) 変えた module の test の時間は、IT の多くで比べられない（欠測）。** base の前の区間（2026-10-02〜10-04）は main の CI が fixture の template の cache の壊れで赤く、対象の IT の module はほとんどが全ての run で落ちていた。
  比べられたのは unit test の群と一部の IT（lifecycle_down・lifecycle_install・cli_read など）で、どれも test の数が区間で違い、時間の差を分割の効果と断定できない。
- **(C) 期間が 3 日に満たないので結論を書かない。** after（2026-10-11T18:04:37Z）の後は測った時点（2026-10-11T19:01:37Z）までの 57 分（0.040 日）しか無い。
  値は並べるが、測り直しは receipt の follow_up に残した。

## 基準の commit と着地の確認

- base: `8ab60f61aab27e0416664e4a6b3c701dbfdec01f`（2026-10-04T11:15:43+09:00）。
  task 1552〜1558・1548・1549 のうち最初に着地した 1549 の squash commit `076bbb727da3fcbd1c883d8e986c8599eee1408d`（2026-10-04T11:29:49+09:00）の親。
- after: `9ba7994771f82438af2d9440e574b85f7bdcd1b8`（2026-10-12T03:04:37+09:00、この run の base commit の main）。
- 着地の commit（main の `Dagq-Task` trailer で引いた）:

  | task | 着地の commit |
  | --- | --- |
  | 1548 | `a35286332287ac0c1f48f9cc57e1580cdb14daef` |
  | 1549 | `076bbb727da3fcbd1c883d8e986c8599eee1408d` |
  | 1552 | `0f0d3c76315b28cbbb55707b71b088b6fdd7f80c` |
  | 1553 | `2ebafe7694cbca5ab383efc61f7f1a07086263a3` |
  | 1554 | `c4ca3e74676053b1bc679d06e5435ac2331ffc85` |
  | 1555 | `b456c08f1af69ff4e1e27f63f5d539233b41e84e` |
  | 1556 | `37f100884833f14e20efdc0395f0b7de5f37e8ea` |
  | 1557 | `64c8a2d9f238b9db27de53480d1740c2acc3a1a0` |
  | 1558 | `2762e010effeebf65ee7ed608c2dc677ffd3bc98` |
  | 2158 | `cccef3ef5b903754297b8deae97a57af9c5b432a` |
  | 1650 | `2d0dbb617a6936f679309bc8b9471365dd8b2589` |
  | 2212 | `d7a72870bbfdacebc38091baf0f00e80acba5547` |
  | 2232 | `2589bb17504c161ef1cd34111c8e8410decf2afa` |

- 確認のコマンド（13 の commit のどれも、base の後にあり after に含まれる。4 つとも exit 0）:

  ```sh
  git log main --format='%H %cI' --grep='^Dagq-Task: 2232$'   # 他の task も同じ
  for c in cccef3ef5b903754297b8deae97a57af9c5b432a 2d0dbb617a6936f679309bc8b9471365dd8b2589 \
           d7a72870bbfdacebc38091baf0f00e80acba5547 2589bb17504c161ef1cd34111c8e8410decf2afa; do
    git merge-base --is-ancestor "$c" 9ba7994771f82438af2d9440e574b85f7bdcd1b8 && echo "$c ok"
  done
  ```

## (A) 構造の指標

`python3 scripts/architecture-boundaries.py structure <commit>` が commit の `src` を `git archive` で取り出して数える（1 回だけ。commit ごとに決まる値）。

| 指標 | base | after | 差 |
| --- | --- | --- | --- |
| `application/supervise/mod.rs` の行数 | 3,413 | 4,029 | +616 |
| ports の行数（base は `application/ports.rs`、after は `application/ports/` の 6 file）の合計 / 最大 | 3,497 / 3,497 | 3,961 / 1,725 | +464 / −1,772 |
| compose の行数（base は `compose.rs`、after は `compose.rs` と `compose/` の 5 file）の合計 / 最大 | 4,173 / 4,173 | 3,992 / 1,188 | −181 / −2,985 |
| queue_reads の行数（base は `application/queue_reads.rs`、after は `application/queue_reads/` の 5 file）の合計 / 最大 | 921 / 921 | 1,834 / 1,137 | +913 / +216 |
| `application/supervise/inbox_nudge.rs` の行数 | 409 | 222 | −187 |
| `infrastructure/runtime_store/run_log.rs` の行数 | 1,040 | 1,141 | +101 |
| `RunLog` の method の数（自分の / supertrait を含む） | 31 / 31 | 0 / 32 | 注 2 |
| `impl Supervisor` を持つ submodule の数 | 39 | 20 | −19 |
| `struct Supervisor` の直接の欄の数 | 102 | 46 | −56 |
| ports の外で `dyn Queue` を取る fn の signature の数（全て / `#[cfg(test)]` の外） | 89 / 88 | 41 / 41 | −48 / −47 |
| `application/recording.rs` の行数 | 623 | 937 | +314 |
| `application/lifecycle.rs` の行数 | 2,602 | 2,282 | −320 |
| ports の外で全体の Queue の opener を取る signature / 欄 | 7 / 7 | 4 / 3 | −3 / −4 |
| application の `Instant::now` の数（全て / `#[cfg(test)]` の外） | 114 / 112 | 86 / 66 | −28 / −46 |
| supervise の payload の文字列 key の数 | 272 | 173 | −99 |
| domain/run の payload の文字列 key の数 | 56 | 43 | −13 |

注:

1. 分けた後の ports の合計と最大は 2212 が変えた `ports/execution.rs`・`ports/host.rs` を含む（最大の 1,725 行は `ports/execution.rs`）。
   queue_reads の after は test の double の `queue_reads/fakes.rs` を含む。
   `supervise/mod.rs` の増減と、queue_reads と recording.rs の合計の増えは、分割と同じ期間に着地した他の変更（388 の commit）も含むので、どれがどの task の分かを断定しない。
2. `RunLog` は、2212 の着地の直前（`d7a72870^`）の 32 から 2212 の着地（`d7a72870`）で 30 になった（`record_inbox_watcher_change` と `claim_inbox_nudge` の 2 つを host運用 の `InboxWatchLog` へ移した）。
   その後に goal 100 の外の `f3680932`（2026-10-09T17:30:04+09:00）が `RunLog` を `RunReads`（11）と `EventStore`（21）の supertrait にまとめ直したので、after の自分の method は 0、supertrait を含めて 32 になる（`EventStore` は run の外の event の読み書きも持つ）。
   base と after の 31→32 は 2 つの trait の範囲が違うので、2212 の効果は直前と直後の 32→30 で読む。
3. 全体の Queue の opener は 2232 の着地の直前（`2589bb17^`）の signature 5 / 欄 3 から 2232 の着地（`2589bb17`）で 4 / 3 になった。
   `RecordingSessions::over` と `Recorder.queues` が狭い `RecordingQueue` の opener を取るようになり、全体の opener を包む `AsRecordingQueue.0` の欄が 1 つ増えた（`RecordingBackend::over` に使われる）。
   `lifecycle::Ports` の recording のための全体の opener（`recording_queues`）は base と after のどちらにも無い（base の後に足され、2232 の前に task 2160 が外した）。

数え方（after の worktree でそのまま流せる形。script も同じ規則で数える）:

- 行数: `wc -l <file>`。分けた module の合計と最大は、`application/ports/`・`compose.rs` と `compose/`・`application/queue_reads/` の下の `*.rs` の `wc -l` の和と最大。
- `RunLog` の method: `pub trait RunLog` の block の中で行頭が 4 つの空白と `fn ` の行（自分の method）。supertrait（`RunLog: RunReads + EventStore`）があれば、それぞれの trait の同じ数を足す。
- `impl Supervisor` の submodule: `grep -rlE '^impl(<[^>]*>)? Supervisor\b' src/application/supervise | grep -vx 'src/application/supervise/mod.rs' | wc -l`（task 2158 の receipt と同じ式）。
- `struct Supervisor` の欄: `awk '/^struct Supervisor<.a> \{/{s=1;next} s&&/^\}/{exit} s&&/^    [a-z_][a-z_0-9]*:/{n++} END{print n}' src/application/supervise/mod.rs`（task 1552 の receipt の awk の式）。
- `dyn Queue` を取る signature: `src/application/ports.rs` と `src/application/ports/` の下を除いた `src` の `*.rs` で、`fn` の行から行末が `{` か `;` の行までを 1 つの signature とし、その文字列が正規表現 `(Box<\s*dyn\s+Queue|&\s*(mut\s+)?dyn\s+Queue|impl\s+Queue)(?![\w])` に当たる fn を 1 件と数える（`dyn QueueOpener` などは識別子が続くので当たらない）。
  `#[cfg(test)]` の外は、`#[cfg(test)]` の次の `mod x {` から行頭の `}` までの行と、`#[cfg(test)] mod x;` が読み込む file を除く。
- `Instant::now`: `grep -rho 'Instant::now' src/application | wc -l`（comment の行を含む）。`#[cfg(test)]` の外は上と同じ除き方。
- payload の文字列 key: `grep -rhoE 'payload\s*\[\s*"|payload\.get\(\s*"' src/application/supervise | wc -l` と、同じ式の `src/domain/run.rs src/domain/run`。
- 全体の Queue の opener:
  - 候補: `grep -rn 'QueueOpener' src | grep -v '^src/application/ports'`（`src/application/ports.rs` と `src/application/ports/` の下を除く）の全ての行。
    `QueueOpener` は `src/application/ports/host.rs` の `QueueOpener<P: ?Sized = dyn Queue + Send>` で、型引数の無い `dyn QueueOpener` が全体の Queue を開くので、一致の数をそのまま件数にしない。
  - 数える: 型引数の無い `dyn QueueOpener` と、型引数が全体の Queue の `dyn QueueOpener<dyn Queue + Send>`（`crate::application::Queue` などの path で書いたもの、`+ Send` / `+ Sync` の有無を問わない）。
  - 数えない: 狭い port を型引数に取るもの（`dyn LifecycleQueue + Send`・`dyn HostOpsQueue + Send`・`dyn ObservationQueue + Send`・`dyn RecordingQueue + Send`・`dyn RunLog + Send` など、`Queue` 以外の trait の dyn）、型引数が generic の `P` のままのもの（`application/update.rs`・`install.rs` の `QueueOpener<P>`。`P` を全体の Queue に具体化する呼び出し側の signature と欄に全体の opener は現れなかった）、`use` の行（`use` の宣言と、その中括弧の続きの行）、doc comment と comment の行、`impl … QueueOpener … for …` の実装の行、`#[cfg(test)]` の module の中の行（上と同じ除き方。`#[cfg(test)] mod port_fakes;` の `application/port_fakes.rs` も）。
    全体の opener を名付ける型の別名と re-export は base と after のどちらにも無かった（`grep -rn 'type .*QueueOpener\|QueueOpener as' src` が 0 件）。
  - 1 件の単位: signature は、引数か戻り値の型に全体の opener が現れる fn を 1 件（同じ fn の複数の引数でも 1 件）。
    欄は、struct の欄（`pub(super)` なども、tuple struct の `.0` も）か enum の variant の欄（base と after のどちらにも当たる候補は無かった）の型に全体の opener が現れるものを、欄 1 つにつき 1 件（`&dyn Fn(&Path) -> Arc<dyn QueueOpener>` のように全体の opener を返す型の欄も 1 件）。
    型が複数行にまたがる signature・欄は宣言の全体（fn は `fn` の行から行末が `{` か `;` の行まで、欄は行末が `,` の行まで）を読んで判定する。
  - 数えた候補と数えなかった候補と理由は付録 A。

## (B) 変えた module の CI の test の時間

### 取得元と標本

- 取得元: main の CI（`.github/workflows/ci.yml` の push の run）の macOS の job（`fmt / test / clippy / coverage`、`runs-on: macos-14`）の `cargo llvm-cov nextest --locked --workspace --fail-under-lines 80` の step の log を `gh run view <id> --log` で読んだ。
  ci.yml が 2026-09-28 の `a722911f` からこのコマンドで全ての test を流すので、base の前と after の後で同じコマンド（llvm-cov の計装・nextest）と同じ runner の image で流れている。
  区間の間に計測に関わる CI の変更が 4 本ある（表の下）。
- 着地の検証の log（`integrate-<attempt>-verify-N.log`）は使わない。
  task 1927（2026-10-07 着地）から `dagq.toml` の `[landing_verification]` が coverage の関門を `sh scripts/landing-it.sh`（unit 全件と絞った IT、既に落ちている test を除く）に置き換えたので、after の後の log には対象 module が流れないことがあり、旧 coverage の関門の log と並べられないため。
- run の一覧: `gh run list --workflow ci.yml --branch main --event push --limit 1000 --json databaseId,headSha,createdAt,conclusion,status`（2026-09-26T08:16:25Z まで遡った 1,000 本）。
- 区間の条件（`python3 scripts/architecture-boundaries.py ci-runs <base> <runs.json> <4 つの着地 commit>`）:
  - before: head が base の祖先で `a722911f` を含む run。`git merge-base --is-ancestor <head> 8ab60f61aab27e0416664e4a6b3c701dbfdec01f && git merge-base --is-ancestor a722911f <head>`。
    当たった 372 本のうち 261 本は cancelled で、残る 111 本の新しい方から test の step が Summary の行まで出た run を 10 本取った（新しい方の 10 本は全て Summary まで出ていた）。
  - after: head が 2158・1650・2212・2232 の全ての着地 commit を含む run。4 つの commit の全てで `git merge-base --is-ancestor <着地 commit> <head>`。
    当たった 17 本（cancelled は 0 本）の古い方から全て見て、docs だけの差分で Rust の job が skipped の 2 本（38117543362・38130312838。Summary の行が無い）を外した 15 本を取った。
- 取った run の id・head commit・時刻・runner の image は付録 B、run ごと・module ごとの値は [ci-modules.csv](architecture-boundaries/ci-modules.csv)（`python3 scripts/architecture-boundaries.py ci-log <id> <log>` の出力に区間・head・時刻を足したもの）。

### module の群と外し方

- IT の群は `tests/it/<名前>.rs` の module で、description が名指す runtime_session・runtime_stall・runtime_stall_recovery・runtime_adopt・runtime_handoff・lifecycle_handoff_stop・lifecycle_down・runtime_sweep・inbox_nudge と、task 1552〜1558・2158・1650・2212・2232 の receipt の summary に名前のある `tests/it` の module（2158 の plan review・goal review の IT、1553 の runtime_e2e、2212 の inbox_nudge の IT、2232 の receipt の lifecycle_install など）。
  一覧は script の `IT_GROUPS`。
- unit test の群は分割の前の module の path を接頭辞とする群（`application::supervise`・`application::ports`・`compose`・`application::queue_reads`・`application::recording`・`application::lifecycle`、2212 の `application::supervise::inbox_nudge`・`infrastructure::runtime_store::run_log`、receipt に名前のある `application::install`・`application::update`・`application::health`・`application::ci_watch`・`application::kpi`・`application::review`・`application::integrate`・`infrastructure::clock`・`infrastructure::runtime_store`・`domain::run::payload`・`domain::run_phase`）。
  分けた先（`application::ports::execution` など）へ移った test は接頭辞が同じなので同じ群に数え、1 つの test は当たる全ての群に数える（`application::supervise::inbox_nudge` の test は `application::supervise` にも入る）。
  一覧は script の `LIB_GROUPS`。
- run ごとに、群の test の PASS の行（`PASS [ 秒s]`）の秒の和を出し、区間の中央値を取った。
- 群の test に PASS 以外の行（`TRY n FAIL`・`TRY n PASS` の FLAKY・FAIL・SKIP・TIMEOUT・SIGSEGV など）が 1 つでもある run は、その群の標本から外した。
  時間の欄が空の行（`SKIP [         ]`）も同じく外す。
  取った 25 本の log に SKIP の行は無かった（`grep -cE ' SKIP '` が全て 0）。
  外したのは before の 589 件（群×run。588 件が `TRY-FAIL`、1 件が FLAKY と `TRY-FAIL`）と after の 9 件（全て `it::planner_headless`。5 件が FLAKY と `TRY-FAIL`、4 件が `TRY-FAIL`）。
  群ごとの本数は表の「外した」。
- 標本が 0 本の群は欠測にし、差と短縮率を出さない。
  欠測の理由は 2 つで、群ごとに表の本数から読める: before で全ての run で外れた（「外した」が区間の本数と同じ。下の CI の壊れ）と、その区間の run にその module の test が無い（「test の無い」が区間の本数と同じ。base の後に足された module、`runtime_stall`・`runtime_stall_recovery` のように after で無くなった module、unit test を持たない `compose`）。
  log が取れない・期限切れ・`gh` が使えないための欠測は無かった。
- before の区間の CI は、`rust-cache` が戻した target の tmp の空の fixture の template で約 3 割の test が落ちる状態だった（ci.yml の「Remove the target's tmp restored from the cache」の comment、`2d1aa9f5` が直した）。
  Summary の行は出ているが、Summary の壁時計は約 3 時間（after は約 14 分）で、after と並べる値ではない。
  取った 10 本より古い 30 本（2026-09-29T01:06Z まで）も読み、description が名指す IT の module（runtime_session・runtime_stall・runtime_stall_recovery・runtime_adopt・runtime_handoff・runtime_sweep・inbox_nudge・plan_review・goal_review）は 40 本の全てで外れていた。
  CI は同じ条件で流し直せないので、before の欠測は測り直せない。

### 表

列: 区間・run の本数（使った / 外した / その module の test の無い）・期間（区間の run の createdAt の最初と最後。`--since` / `--until` に当たる）・module・test の数（区間の run の最小〜最大）・test 時間の合計の中央値（PASS の秒の和の run ごとの値の中央値）・Summary の壁時計の中央値・runner の image（log の Runner Image の `Image` / `Version`）。
差 = after の中央値 − before の中央値、短縮率 = 1 − after の中央値 / before の中央値（負は伸び）。

| module | 区間 | run の本数（使った / 外した / test の無い） | test の数 | test 時間の合計の中央値（秒） | Summary の壁時計の中央値（秒） | 期間 | runner の image | 差（秒） | 短縮率 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| application::ci_watch | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::ci_watch | after | 15 / 0 / 0 | 1 | 0.024 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| application::health | before | 10 / 0 / 0 | 3 | 0.060 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::health | after | 15 / 0 / 0 | 7 | 0.154 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.093 | -154.5% |
| application::install | before | 10 / 0 / 0 | 1 | 0.020 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::install | after | 15 / 0 / 0 | 4 | 0.087 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.067 | -324.4% |
| application::integrate | before | 10 / 0 / 0 | 30 | 0.663 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::integrate | after | 15 / 0 / 0 | 36 | 0.795 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.132 | -19.8% |
| application::kpi | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::kpi | after | 0 / 0 / 15 | - | 欠測 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| application::lifecycle | before | 3 / 0 / 7 | 1 | 0.024 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::lifecycle | after | 15 / 0 / 0 | 3 | 0.067 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.043 | -179.2% |
| application::ports | before | 10 / 0 / 0 | 1 | 0.019 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::ports | after | 15 / 0 / 0 | 1 | 0.017 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | -0.002 | 12.8% |
| application::queue_reads | before | 10 / 0 / 0 | 3 | 0.058 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::queue_reads | after | 15 / 0 / 0 | 12 | 0.260 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.202 | -344.4% |
| application::recording | before | 10 / 0 / 0 | 3 | 0.048 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::recording | after | 15 / 0 / 0 | 7 | 0.145 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.097 | -205.3% |
| application::review | before | 7 / 0 / 3 | 1 | 0.016 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::review | after | 15 / 0 / 0 | 7〜9 | 0.136 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.120 | -750.0% |
| application::supervise | before | 10 / 0 / 0 | 95〜103 | 2.168 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::supervise | after | 15 / 0 / 0 | 195〜205 | 4.740 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +2.572 | -118.6% |
| application::supervise::inbox_nudge | before | 10 / 0 / 0 | 2 | 0.035 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::supervise::inbox_nudge | after | 15 / 0 / 0 | 1 | 0.022 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | -0.013 | 37.1% |
| application::update | before | 10 / 0 / 0 | 6 | 0.124 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| application::update | after | 15 / 0 / 0 | 12 | 0.235 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.110 | -88.8% |
| compose | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| compose | after | 0 / 0 / 15 | - | 欠測 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| domain::run::payload | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| domain::run::payload | after | 0 / 0 / 15 | - | 欠測 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| domain::run_phase | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| domain::run_phase | after | 15 / 0 / 0 | 12 | 0.209 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| infrastructure::clock | before | 10 / 0 / 0 | 1 | 0.012 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| infrastructure::clock | after | 15 / 0 / 0 | 1 | 0.013 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.001 | -8.3% |
| infrastructure::runtime_store | before | 10 / 0 / 0 | 4〜6 | 0.847 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| infrastructure::runtime_store | after | 15 / 0 / 0 | 13〜15 | 3.713 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +2.867 | -338.6% |
| infrastructure::runtime_store::run_log | before | 3 / 0 / 7 | 1 | 0.141 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| infrastructure::runtime_store::run_log | after | 15 / 0 / 0 | 2 | 0.575 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.434 | -307.8% |
| it::cli_planner_request | before | 3 / 0 / 7 | 4 | 0.736 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::cli_planner_request | after | 15 / 0 / 0 | 7 | 1.480 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.744 | -101.1% |
| it::cli_read | before | 10 / 0 / 0 | 10〜11 | 3.325 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::cli_read | after | 15 / 0 / 0 | 14 | 5.992 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +2.667 | -80.2% |
| it::draft_bundles | before | 0 / 10 / 0 | 3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::draft_bundles | after | 15 / 0 / 0 | 4 | 4.536 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::draft_revisit | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::draft_revisit | after | 15 / 0 / 0 | 3 | 6.828 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::finding_planner | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::finding_planner | after | 15 / 0 / 0 | 5 | 6.053 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::goal_review | before | 0 / 10 / 0 | 12 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::goal_review | after | 15 / 0 / 0 | 12 | 12.262 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::goal_review_codex | before | 0 / 10 / 0 | 7 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::goal_review_codex | after | 15 / 0 / 0 | 4 | 7.882 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::inbox_nudge | before | 0 / 10 / 0 | 7 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::inbox_nudge | after | 15 / 0 / 0 | 6 | 3.939 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::lifecycle_down | before | 10 / 0 / 0 | 7〜9 | 1.277 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::lifecycle_down | after | 15 / 0 / 0 | 9 | 1.356 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.079 | -6.2% |
| it::lifecycle_handoff_stop | before | 9 / 0 / 1 | 2 | 0.059 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::lifecycle_handoff_stop | after | 15 / 0 / 0 | 2 | 0.080 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.021 | -35.6% |
| it::lifecycle_install | before | 10 / 0 / 0 | 26〜27 | 16.990 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::lifecycle_install | after | 15 / 0 / 0 | 27 | 16.605 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | -0.385 | 2.3% |
| it::lifecycle_install_watch | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::lifecycle_install_watch | after | 15 / 0 / 0 | 8 | 10.812 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::lifecycle_replace | before | 0 / 10 / 0 | 17〜18 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::lifecycle_replace | after | 15 / 0 / 0 | 16 | 7.583 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::observer_codex | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::observer_codex | after | 15 / 0 / 0 | 9 | 13.738 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::plan_review | before | 0 / 10 / 0 | 30 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::plan_review | after | 15 / 0 / 0 | 15 | 21.737 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::plan_review_codex | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::plan_review_codex | after | 15 / 0 / 0 | 3 | 5.266 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::plan_review_concern | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::plan_review_concern | after | 15 / 0 / 0 | 1 | 1.151 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::plan_review_expected_files | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::plan_review_expected_files | after | 15 / 0 / 0 | 2 | 1.472 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::plan_review_launch | before | 0 / 4 / 6 | 2 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::plan_review_launch | after | 15 / 0 / 0 | 2 | 1.824 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::plan_review_turns | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::plan_review_turns | after | 15 / 0 / 0 | 2 | 1.562 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::planner_close | before | 0 / 10 / 0 | 2 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::planner_close | after | 15 / 0 / 0 | 2 | 0.731 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::planner_headless | before | 0 / 7 / 3 | 6 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::planner_headless | after | 6 / 9 / 0 | 7 | 16.829 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::planner_headless_stats | before | 0 / 4 / 6 | 1 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::planner_headless_stats | after | 15 / 0 / 0 | 1 | 3.089 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::planner_headless_turns | before | 0 / 4 / 6 | 8 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::planner_headless_turns | after | 15 / 0 / 0 | 5 | 18.373 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::planner_slots | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::planner_slots | after | 15 / 0 / 0 | 10 | 12.505 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::queue_service | before | 0 / 10 / 0 | 7 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::queue_service | after | 15 / 0 / 0 | 8 | 18.315 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::queue_service_reads | before | 10 / 0 / 0 | 3 | 37.300 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::queue_service_reads | after | 15 / 0 / 0 | 6 | 41.752 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +4.452 | -11.9% |
| it::recovery_codex | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::recovery_codex | after | 15 / 0 / 0 | 13 | 34.320 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::request_planner | before | 0 / 7 / 3 | 7 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::request_planner | after | 15 / 0 / 0 | 10 | 8.210 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::review_subagents | before | 0 / 10 / 0 | 17 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::review_subagents | after | 15 / 0 / 0 | 15〜22 | 43.428 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_adopt | before | 0 / 10 / 0 | 19 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_adopt | after | 15 / 0 / 0 | 12 | 31.679 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_ask | before | 0 / 10 / 0 | 9 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_ask | after | 15 / 0 / 0 | 8 | 12.427 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_candidates | before | 0 / 10 / 0 | 1 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_candidates | after | 15 / 0 / 0 | 1 | 2.256 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_ci_watch | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_ci_watch | after | 15 / 0 / 0 | 8 | 16.615 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_claim | before | 0 / 10 / 0 | 25 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_claim | after | 15 / 0 / 0 | 25 | 42.040 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_claim_defer | before | 0 / 10 / 0 | 2〜6 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_claim_defer | after | 15 / 0 / 0 | 9 | 38.284 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_claim_hold | before | 0 / 10 / 0 | 2 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_claim_hold | after | 15 / 0 / 0 | 2 | 4.999 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_claim_spacing | before | 0 / 2 / 8 | 3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_claim_spacing | after | 15 / 0 / 0 | 3 | 20.770 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_cleanup | before | 0 / 10 / 0 | 10〜17 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_cleanup | after | 15 / 0 / 0 | 17 | 103.354 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_codex | before | 0 / 10 / 0 | 16 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_codex | after | 15 / 0 / 0 | 11 | 28.982 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_codex_ask | before | 0 / 10 / 0 | 1〜6 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_codex_ask | after | 15 / 0 / 0 | 1 | 3.480 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_codex_work | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_codex_work | after | 15 / 0 / 0 | 1 | 2.499 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_disk | before | 0 / 10 / 0 | 7 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_disk | after | 15 / 0 / 0 | 4 | 13.597 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_e2e | before | 0 / 10 / 0 | 6 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_e2e | after | 15 / 0 / 0 | 6 | 32.490 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_forecast | before | 0 / 10 / 0 | 2〜3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_forecast | after | 15 / 0 / 0 | 3 | 9.654 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_handoff | before | 0 / 10 / 0 | 21〜24 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_handoff | after | 15 / 0 / 0 | 20 | 79.375 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_headless_jobs | before | 0 / 10 / 0 | 2 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_headless_jobs | after | 15 / 0 / 0 | 5〜6 | 15.244 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_headless_reopen | before | 0 / 10 / 0 | 8 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_headless_reopen | after | 15 / 0 / 0 | 6 | 22.042 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_headless_stall | before | 0 / 10 / 0 | 5〜6 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_headless_stall | after | 15 / 0 / 0 | 9 | 39.461 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_heartbeat | before | 9 / 1 / 0 | 4 | 3.025 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_heartbeat | after | 15 / 0 / 0 | 4 | 3.108 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | +0.083 | -2.7% |
| it::runtime_host_metrics | before | 0 / 10 / 0 | 2 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_host_metrics | after | 15 / 0 / 0 | 2 | 4.416 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_integrate | before | 0 / 10 / 0 | 23 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_integrate | after | 15 / 0 / 0 | 23 | 70.936 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_landing_answers | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_landing_answers | after | 15 / 0 / 0 | 2 | 10.787 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_light_slots | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_light_slots | after | 15 / 0 / 0 | 2 | 15.469 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_observer | before | 0 / 10 / 0 | 15〜16 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_observer | after | 15 / 0 / 0 | 18 | 42.914 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_provider_switch | before | 0 / 10 / 0 | 13 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_provider_switch | after | 15 / 0 / 0 | 9 | 28.279 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_push | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_push | after | 15 / 0 / 0 | 4 | 5.875 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_queue_hold | before | 0 / 10 / 0 | 3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_queue_hold | after | 15 / 0 / 0 | 2 | 5.513 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_queue_hold_detect | before | 0 / 10 / 0 | 3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_queue_hold_detect | after | 15 / 0 / 0 | 2 | 4.325 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_queue_hold_shared | before | 0 / 10 / 0 | 3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_queue_hold_shared | after | 15 / 0 / 0 | 3 | 7.935 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_recheck | before | 0 / 10 / 0 | 4〜12 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_recheck | after | 15 / 0 / 0 | 10〜16 | 54.323 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_release | before | 0 / 10 / 0 | 18 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_release | after | 15 / 0 / 0 | 7〜19 | 5.361 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_report | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_report | after | 15 / 0 / 0 | 4 | 3.144 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_resume | before | 0 / 10 / 0 | 21 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_resume | after | 15 / 0 / 0 | 23 | 109.069 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_review | before | 0 / 10 / 0 | 14 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_review | after | 15 / 0 / 0 | 15 | 47.663 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_review_questions | before | 0 / 10 / 0 | 8 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_review_questions | after | 15 / 0 / 0 | 6 | 22.445 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_sccache | before | 0 / 10 / 0 | 3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_sccache | after | 15 / 0 / 0 | 11 | 22.257 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_session | before | 0 / 10 / 0 | 30 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_session | after | 15 / 0 / 0 | 13 | 18.206 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_slot_limits | before | 0 / 10 / 0 | 3 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_slot_limits | after | 15 / 0 / 0 | 3 | 7.775 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_stall | before | 0 / 10 / 0 | 8 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_stall | after | 0 / 0 / 15 | - | 欠測 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_stall_end | before | 0 / 10 / 0 | 2 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_stall_end | after | 15 / 0 / 0 | 2 | 0.432 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_stall_recovery | before | 0 / 10 / 0 | 11 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_stall_recovery | after | 0 / 0 / 15 | - | 欠測 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_sweep | before | 0 / 10 / 0 | 10 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_sweep | after | 15 / 0 / 0 | 11 | 23.256 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_throughput_review | before | 0 / 10 / 0 | 9 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_throughput_review | after | 15 / 0 / 0 | 9 | 15.174 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_triage | before | 0 / 10 / 0 | 24 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_triage | after | 15 / 0 / 0 | 21 | 47.282 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_waiting | before | 0 / 10 / 0 | 12 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_waiting | after | 15 / 0 / 0 | 9 | 35.579 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::runtime_waiting_stages | before | 0 / 10 / 0 | 4 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::runtime_waiting_stages | after | 15 / 0 / 0 | 4 | 50.532 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::supervisor_without_cmux | before | 0 / 0 / 10 | - | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::supervisor_without_cmux | after | 15 / 0 / 0 | 1 | 0.273 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |
| it::throughput_review_codex | before | 0 / 10 / 0 | 8 | 欠測 | 10930.9 | 2026-10-02T19:54:35Z〜2026-10-04T00:06:19Z | macos-14-arm64/20260831.0302.1 |  |  |
| it::throughput_review_codex | after | 15 / 0 / 0 | 9 | 14.142 | 830.8 | 2026-10-09T10:43:03Z〜2026-10-11T18:04:45Z | macos-14-arm64/20260831.0302.1 | - | - |

- 使った本数が 10 に満たない群は、その module の test が区間の途中で足された（「test の無い」の本数）か、落ちて外れた（「外した」の本数）ため。区間の run の本数はどちらも 10 以上（before 10、after 15）。
- CI では load1 と `NEXTEST_TEST_THREADS` を取れない（log に出ない）。
  nextest は CI で既定の並列度（runner の CPU 数）で流れる。
  代わりに runner の image を併記した（両区間とも `macos-14-arm64/20260831.0302.1` の 1 つ）。
- test の数は多くの群で区間の間に違う（例: `application::supervise` 95〜103→195〜205、`it::runtime_session` 30→13、`it::plan_review` 30→15）。
  分割の task が test を unit へ移したこと、他の goal の着地が test を足し・消したことのどれが効いたかは数からは決められないので、差の原因と断定しない。
- 差と短縮率は負荷や runner の違いを原因と断定せず、観測として並べた。
  比べられた unit test の群の多くは合計が 1 秒未満で、差も 1 秒未満。
- 両区間の間に ci.yml・`.config/nextest.toml` を変えた commit（`git log 8ab60f61..9ba79947 -- .github/workflows/ci.yml .config/nextest.toml`）は 14 本。
  計測方式に関わるもの:
  - `2d1aa9f5`（2026-10-06、ci.yml）: test の前に target の tmp を消す step を足し、before の区間の大量の失敗（上）を止めた。
  - `5e4125b3`（2026-10-07、ci.yml と nextest.toml）: `NEXTEST_PROFILE=ci` にし、nextest.toml に default を継いで JUnit を書くだけの `[profile.ci]` を足した（retries・並列度・slow-timeout は変えていない）。
  - `26184a7b`（2026-10-09、ci.yml）: `crates/` の broker の 3 つの crate と `[workspace]` を外したので、`--workspace` が流す test binary から broker の crate が抜けた（Summary の test の数と壁時計に効く。上の群はどれも dagq の crate の test で、群の値には効かない）。
  - `4120791b`（2026-10-08、ci.yml）: docs だけの差分で Rust の job を skipped にした。test の step のある run の数に効く（after の 2 本が skipped で標本から外れた）が、流れた run の test の範囲と計測方式は変えていない。
  計測方式に関わらないもの（test の step のコマンドと環境を変えていない）:
  - `0c26d7a7`（2026-10-08、ci.yml）: llvm-cov nextest の step の comment を直した。
  - `d374aba8`（2026-10-08、ci.yml）: `scripts/landing-it.sh --self-test` の step を docs と script の検査の job に足した。
  - `29853cb0`（2026-10-07、ci.yml）: ADR の索引の生成の検査の step を足した。
  - `9835a66b`（2026-10-07、ci.yml）: docs の相対 link と frontmatter の検査の step を足した。
  - `bdf0b134`（2026-10-07、ci.yml）: docs/design の予算の検査の step を足した。
  - `6ca5fa78`（2026-10-07、ci.yml）: check script の root の検査の step を足した。
  - `c270da8c`（2026-10-07、ci.yml）: frontmatter の日付の行の検査の step を足した。
  - `8d208e2a`（2026-10-06、nextest.toml）: 夜間の別の workflow が使う `[profile.it-coverage]` を足した（CI の macOS の job は使わない）。
  - `3ccae59c`（2026-10-05、ci.yml）: Linux の job を直した（macOS の job ではない）。
  - `1f9c9ad2`（2026-10-05、ci.yml）: test の step の後に IT の時間の関門の step を足した（test の step は変えていない）。

## (C) 変更と衝突の集中

- 期間: after の後の期間は、after（`9ba79947`、2026-10-11T18:04:37Z）の次の秒から測った時点（2026-10-11T19:01:37Z）までの 0.040 日（3,419 秒）。
  7 日に満たないので base の期間をその長さに合わせ、base（2026-10-04T02:15:43Z）で終わる同じ長さにした。
- commit の数（`python3 scripts/architecture-boundaries.py churn <since> <until> <before|after>` と同じ）:

  ```sh
  git log main --since=2026-10-04T01:18:44Z --until=2026-10-04T02:15:43Z --format=%H -- <対象の path...> | sort -u | wc -l
  git log main --since=2026-10-04T01:18:44Z --until=2026-10-04T02:15:43Z --format=%H -- src | sort -u | wc -l
  git log main --since=2026-10-11T18:04:38Z --until=2026-10-11T19:01:37Z --format=%H -- <対象の path...> | sort -u | wc -l
  git log main --since=2026-10-11T18:04:38Z --until=2026-10-11T19:01:37Z --format=%H -- src | sort -u | wc -l
  ```

  - 対象の path（base の期間）: `src/application/ports.rs`・`src/compose.rs`・`src/application/supervise/mod.rs`・`src/application/queue_reads.rs` と、2212・2232 の変えた `src/application/ports/execution.rs`・`src/application/ports/host.rs`・`src/application/supervise/inbox_nudge.rs`・`src/infrastructure/runtime_store/run_log.rs`・`src/application/recording.rs`・`src/application/lifecycle.rs`（base の期間に無い path は 0 件として同じ path を数える）。
  - 対象の path（after の期間）: 上の全てと、task 1552〜1558・2158・1650 が分けて作った module の全て（`src/application/ports/`・`src/compose/`（2232 の変えた `compose/host.rs` を含む）・`src/application/queue_reads/`・`src/application/supervise/contexts.rs`・`stages.rs`・`handoff_slots.rs`・`src/application/port_fakes.rs`）。
- conflict_hotspots: 固定バイナリで `dagq stats --since <since> --until <until> --full` を読んだ（本番 queue の読み取りだけ）。
  `count` は期間の衝突（`integration_deferred` と `conflict_precheck` の `conflicts`）の件数、files は衝突した file の行。

| 期間 | `--since` | `--until` | 長さ | 対象の file 群を変えた commit | src を変えた commit | 割合 | conflict_hotspots の count / file / alert | うち src の file（衝突） | うち対象の file 群 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| base の前 | 2026-10-04T01:18:44Z | 2026-10-04T02:15:43Z | 0.040 日 | 2 | 2 | 100% | 0 / 0 / 0 | 0 | 0 |
| after の後 | 2026-10-11T18:04:38Z | 2026-10-11T19:01:37Z | 0.040 日 | 1 | 1 | 100% | 0 / 0 / 0 | 0 | 0 |

- 期間が 3 日に満たないので、集中が減ったか増えたかの結論は書かない。
  どちらの期間も src の commit が 1〜2 本で、割合も衝突の件数も比べる標本にならない。
- 測り直しは receipt の follow_up（measurement）に残した: after の後の期間が 7 日（2026-10-18T18:04:38Z 以降）になってから、同じ script と同じ対象の path で、after の期間を 2026-10-11T18:04:38Z から 7 日、base の期間を base で終わる 7 日（2026-09-27T02:15:43Z〜2026-10-04T02:15:43Z）にして数え直す。

## 入力と出力の区分と作り直せる範囲

この測定は 1 回きりなので、区分は [計測](../design/measurement.md) の「SSOTとビュー」に足さず、ここに書く（[文書の規則](../development/documents.md)の「design」、ADR-t2065-1 決定2）。

| 入力・出力 | 区分 | 作り直せる範囲 |
| --- | --- | --- |
| (A)・(C) の git の履歴（base と after の commit の `src`、main の commit と commit 日時） | SSOT | base・after の hash と上のコマンド（`structure`・`churn` と `git log`）から、いつでも同じ値を作り直せる |
| (B) の GitHub Actions の CI の run の log（`gh run view --log`） | 材料 | log の保持期限（この repository の設定は 90 日。`gh api repos/hisamekms/dagq/actions/permissions/artifact-and-log-retention` が `{"days":90,"maximum_allowed_days":90}` を返した）を過ぎると消え、同じ表を log から作り直せない。before の run は 2026-12-31 ごろ、after の run は 2027-01-07 ごろから順に消える |
| (C) の本番 queue の event（`dagq stats` の conflict_hotspots の元の `integration_deferred` と `conflict_precheck`） | SSOT | event が残る限り、同じ `--since` / `--until` の `dagq stats --full` で作り直せる |
| この文書の (A)・(C) の表と付録 A | ビュー | 上の SSOT から作り直せる |
| [ci-modules.csv](architecture-boundaries/ci-modules.csv) と (B) の表・付録 B | ビュー（log が消えた後は残る唯一の写し） | log が残る間は log から作り直せる。log の期限の後は、この CSV に残した run ごとの id・head commit・時刻・runner の image・Summary と、群ごとの test の数・PASS の秒の和・外した理由から、中央値・差・短縮率を計算し直すことだけができる。test ごとの時間の取り直しと、標本（run）の選び直しはできない |

## 付録 A: 全体の Queue の opener の候補

`python3 scripts/architecture-boundaries.py structure <commit>` の出力の CSV。
判定の「数える」が上の表の件数で、「型（宣言）」は signature なら fn の宣言の全体、欄なら欄の型。

### base（8ab60f61） の候補

| file:line | 判定 | 別 | 名前 | 型（宣言） |
| --- | --- | --- | --- | --- |
| `src/compose.rs:27` | use | - | - | `AgentProvider, Generators, LaunchAgent, MainRemote, ProcessControl, QueueOpener,` |
| `src/compose.rs:2182` | 数える | signature | queues | `fn queues(&self, db: &Path) -> Arc<dyn QueueOpener> {` |
| `src/compose.rs:2198` | 数える | signature | lifecycle_ports | `fn lifecycle_ports<'a>( &'a self, cmux: &'a dyn WorkspaceBackend, launchd: &'a dyn LaunchAgent, processes: &'a dyn ProcessControl, queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>, ) -> LifecyclePorts<'a> {` |
| `src/application/install.rs:15` | comment | - | - | `//! is read through [`QueueOpener`]; [`crate::compose`] builds the adapters.` |
| `src/application/install.rs:18` | use | - | - | `Clock, ProcessControl, Queue, QueueOpener, RunFiles,` |
| `src/application/install.rs:386` | 数える | 欄 | Ports.queues | `&'a dyn Fn(&Path) -> Arc<dyn QueueOpener>` |
| `src/application/lifecycle.rs:31` | use | - | - | `QueueOpener, RunFiles, SOCKET_PASSWORD_ENV, SupervisorEnvironment, WorkspaceBackend,` |
| `src/application/lifecycle.rs:277` | 数える | 欄 | Ports.queues | `&'a dyn Fn(&Path) -> Arc<dyn QueueOpener>` |
| `src/application/recording.rs:11` | use | - | - | `use super::{QueueOpener, SupervisorEnvironment, WorkspaceBackend, WorkspaceTags};` |
| `src/application/recording.rs:193` | 数える | 欄 | RecordingBackend.queues | `Arc<dyn QueueOpener>` |
| `src/application/recording.rs:207` | 数える | signature | over | `pub fn over( inner: &'a dyn WorkspaceBackend, queues: Arc<dyn QueueOpener>, token: Option<LeaseToken>, load_average: fn() -> Option<f64>, ) -> Self {` |
| `src/application/update.rs:26` | use | - | - | `Clock, InstalledPlugin, ProcessControl, Queue, QueueOpener, RunCoordination, RunFiles,` |
| `src/application/update.rs:362` | 数える | 欄 | JobPorts.queues | `&'a dyn Fn(&Path) -> Arc<dyn QueueOpener>` |
| `src/application/supervise/cleanup.rs:312` | 数える | 欄 | JobPorts.queues | `Arc<dyn QueueOpener>` |
| `src/application/supervise/deliver.rs:666` | #[cfg(test)] | - | - | `use crate::application::{Queue, QueueOpener};` |
| `src/application/supervise/deliver.rs:798` | #[cfg(test)] | - | - | `impl QueueOpener for NoQueue {` |
| `src/application/supervise/forecast.rs:137` | 数える | signature | snapshot | `fn snapshot( queues: &dyn QueueOpener, processes: &dyn ProcessControl, port: &ForecastPort, token: &LeaseToken, now: i64, pending: &Pending, ) -> Result<(i64, SnapshotOutcome)> {` |
| `src/application/supervise/mod.rs:10` | comment | - | - | `//! queue ([`Queue`], a connection per thread from [`QueueOpener`]), Git` |
| `src/application/supervise/mod.rs:50` | use | - | - | `InputSource, LeasedRun, MainRemote, ProcessControl, Queue, QueueOpener, Repository,` |
| `src/application/supervise/mod.rs:356` | 数える | 欄 | Ports.queues | `Arc<dyn QueueOpener>` |
| `src/application/supervise/mod.rs:535` | 数える | signature | start | `pub fn start(queues: Arc<dyn QueueOpener>, token: LeaseToken, policy: HeartbeatPolicy) -> Self {` |
| `src/application/supervise/mod.rs:574` | 数える | signature | beat | `fn beat( queues: &dyn QueueOpener, token: &LeaseToken, policy: HeartbeatPolicy, stop: &mpsc::Receiver<()>, ) -> Result<()> {` |
| `src/application/supervise/mod.rs:939` | 数える | 欄 | Supervisor.queues | `Arc<dyn QueueOpener>` |
| `src/application/supervise/mod.rs:3270` | 数える | signature | spawn_validation | `fn spawn_validation( queues: Arc<dyn QueueOpener>, repository: Arc<dyn Repository + Send + Sync>, files: Arc<dyn RunFiles>, run: TaskRun, e2e_paths: Vec<String>, ) -> thread::JoinHandle<Result<Validation>> {` |
| `src/infrastructure/runtime_store/mod.rs:488` | impl | - | - | `impl crate::application::QueueOpener for SqliteOpener {` |

### after（9ba79947） の候補

| file:line | 判定 | 別 | 名前 | 型（宣言） |
| --- | --- | --- | --- | --- |
| `src/compose/host.rs:21` | use | - | - | `AgentProvider, CommandSpec, HostOpsQueue, LaunchAgent, ProcessControl, QueueOpener, RunLog,` |
| `src/compose/host.rs:860` | 狭い port | signature | lifecycle_queues | `fn lifecycle_queues(&self, db: &Path) -> Arc<dyn QueueOpener<dyn LifecycleQueue + Send>> {` |
| `src/compose/host.rs:868` | 狭い port | signature | event_queues | `fn event_queues(&self, db: &Path) -> Arc<dyn QueueOpener<dyn RunLog + Send>> {` |
| `src/compose/host.rs:876` | 狭い port | signature | host_queues | `fn host_queues(&self, db: &Path) -> Arc<dyn QueueOpener<dyn HostOpsQueue + Send>> {` |
| `src/compose/host.rs:890` | 狭い port | signature | lifecycle_ports | `fn lifecycle_ports<'a>( &'a self, launchd: &'a dyn LaunchAgent, processes: &'a dyn ProcessControl, queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener<dyn LifecycleQueue + Send>>, ) -> LifecyclePorts<'a> {` |
| `src/application/install.rs:15` | comment | - | - | `//! is read through [`QueueOpener`]; [`crate::compose`] builds the adapters.` |
| `src/application/install.rs:18` | use | - | - | `Clock, HostOpsQueue, ProcessControl, QueueOpener, RunFiles, SupervisorRegistry,` |
| `src/application/install.rs:375` | generic の P | 欄 | Ports.queues | `&'a dyn Fn(&Path) -> Arc<dyn QueueOpener<P>>` |
| `src/application/lifecycle.rs:30` | use | - | - | `AgentProvider, CONFIG_HOME_ENV, Clock, CommandSpec, LaunchAgent, ProcessControl, QueueOpener,` |
| `src/application/lifecycle.rs:282` | 狭い port | 欄 | Ports.queues | `&'a dyn Fn(&Path) -> Arc<dyn QueueOpener<dyn LifecycleQueue + Send>>` |
| `src/application/lifecycle.rs:1729` | 狭い port | 欄 | InboxPorts.queues | `&'a dyn Fn(&Path) -> Arc<dyn QueueOpener<dyn RunLog + Send>>` |
| `src/application/port_fakes.rs:258` | #[cfg(test)] | - | - | `impl QueueOpener<dyn RecordingQueue + Send> for SessionRecords {` |
| `src/application/recording.rs:13` | use | - | - | `use super::{QueueOpener, RecordingQueue, SessionWrappers, WorkspaceBackend, WorkspaceTags};` |
| `src/application/recording.rs:163` | 狭い port | 欄 | Recorder.queues | `Arc<dyn QueueOpener<dyn RecordingQueue + Send>>` |
| `src/application/recording.rs:171` | 数える | 欄 | AsRecordingQueue.0 | `Arc<dyn QueueOpener>` |
| `src/application/recording.rs:173` | impl | - | - | `impl QueueOpener<dyn RecordingQueue + Send> for AsRecordingQueue {` |
| `src/application/recording.rs:311` | 数える | signature | over | `pub fn over( inner: &'a dyn WorkspaceBackend, queues: Arc<dyn QueueOpener>, token: Option<LeaseToken>, load_average: fn() -> Option<f64>, ) -> Self {` |
| `src/application/recording.rs:419` | 狭い port | signature | over | `pub fn over( inner: &'a dyn SessionWrappers, queues: Arc<dyn QueueOpener<dyn RecordingQueue + Send>>, token: Option<LeaseToken>, load_average: fn() -> Option<f64>, ) -> Self {` |
| `src/application/recording.rs:667` | #[cfg(test)] | - | - | `impl<P: ?Sized> QueueOpener<P> for NoQueue {` |
| `src/application/update.rs:26` | use | - | - | `AskStore, Clock, HostOpsQueue, InstalledPlugin, ProcessControl, QueueOpener, RunFiles, RunLog,` |
| `src/application/update.rs:523` | generic の P | 欄 | JobPorts.queues | `&'a dyn Fn(&Path) -> Arc<dyn QueueOpener<P>>` |
| `src/application/supervise/cleanup.rs:756` | 狭い port | 欄 | JobPorts.queues | `Arc<dyn QueueOpener<dyn crate::application::HostOpsQueue + Send>>` |
| `src/application/supervise/contexts.rs:202` | 狭い port | 欄 | ObservationState.queues | `Arc<dyn QueueOpener<dyn ObservationQueue + Send>>` |
| `src/application/supervise/contexts.rs:316` | 狭い port | 欄 | HostOpsState.queues | `Arc<dyn QueueOpener<dyn HostOpsQueue + Send>>` |
| `src/application/supervise/forecast.rs:137` | 狭い port | signature | snapshot | `fn snapshot( queues: &dyn QueueOpener<dyn crate::application::ObservationQueue + Send>, processes: &dyn ProcessControl, port: &ForecastPort, token: &LeaseToken, now: i64, pending: &Pending, ) -> Result<(i64, SnapshotOutcome)> {` |
| `src/application/supervise/mod.rs:10` | comment | - | - | `//! queue ([`Queue`], a connection per thread from [`QueueOpener`]), Git` |
| `src/application/supervise/mod.rs:56` | use | - | - | `IdleHook, LeasedRun, MainRemote, ProcessControl, Queue, QueueOpener, Repository,` |
| `src/application/supervise/mod.rs:432` | 数える | 欄 | Ports.queues | `Arc<dyn QueueOpener>` |
| `src/application/supervise/mod.rs:434` | 狭い port | 欄 | Ports.observation_queues | `Arc<dyn QueueOpener<dyn crate::application::ObservationQueue + Send>>` |
| `src/application/supervise/mod.rs:436` | 狭い port | 欄 | Ports.host_queues | `Arc<dyn QueueOpener<dyn crate::application::HostOpsQueue + Send>>` |
| `src/application/supervise/mod.rs:439` | 狭い port | 欄 | Ports.recording_queues | `Arc<dyn QueueOpener<dyn crate::application::RecordingQueue + Send>>` |
| `src/application/supervise/mod.rs:647` | 数える | signature | start | `pub fn start(queues: Arc<dyn QueueOpener>, token: LeaseToken, policy: HeartbeatPolicy) -> Self {` |
| `src/application/supervise/mod.rs:686` | 数える | signature | beat | `fn beat( queues: &dyn QueueOpener, token: &LeaseToken, policy: HeartbeatPolicy, stop: &mpsc::Receiver<()>, ) -> Result<()> {` |
| `src/application/supervise/mod.rs:1097` | 数える | 欄 | Supervisor.queues | `Arc<dyn QueueOpener>` |
| `src/application/supervise/mod.rs:3741` | 数える | signature | spawn_validation | `fn spawn_validation( queues: Arc<dyn QueueOpener>, repository: Arc<dyn Repository + Send + Sync>, files: Arc<dyn RunFiles>, run: TaskRun, e2e_paths: Vec<String>, ) -> thread::JoinHandle<Result<Validation>> {` |
| `src/infrastructure/runtime_store/mod.rs:545` | comment | - | - | `/// One connection, as [`crate::application::QueueOpener::open`] and` |
| `src/infrastructure/runtime_store/mod.rs:558` | impl | - | - | `impl crate::application::QueueOpener for SqliteOpener {` |
| `src/infrastructure/runtime_store/mod.rs:571` | impl | - | - | `impl<P: ?Sized> crate::application::QueueOpener<P> for SqlitePorts<P> {` |


## 付録 B: (B) で取った run

| 区間 | run の id | head commit | 時刻（createdAt、UTC） | Summary の test の数 / passed | Summary の壁時計（秒） | runner の image |
| --- | --- | --- | --- | --- | --- | --- |
| before | 37163887261 | `d1288f237e2d` | 2026-10-04T00:06:19Z | 2762 / 1946 | 11157.3 | macos-14-arm64/20260831.0302.1 |
| before | 37153165600 | `d69b066caddd` | 2026-10-03T20:53:42Z | 2743 / 1934 | 11162.9 | macos-14-arm64/20260831.0302.1 |
| before | 37141852457 | `a6de115b148b` | 2026-10-03T17:48:18Z | 2714 / 1909 | 11142.5 | macos-14-arm64/20260831.0302.1 |
| before | 37128450407 | `6c1b6760b0ec` | 2026-10-03T14:05:58Z | 2683 / 1882 | 10933.4 | macos-14-arm64/20260831.0302.1 |
| before | 37112989516 | `0045125d37c8` | 2026-10-03T09:25:18Z | 2661 / 1871 | 10928.5 | macos-14-arm64/20260831.0302.1 |
| before | 37109712351 | `3bbd120ee7e7` | 2026-10-03T08:26:35Z | 2660 / 1870 | 10924.9 | macos-14-arm64/20260831.0302.1 |
| before | 37098513544 | `ac1e5eb3fe71` | 2026-10-03T05:02:15Z | 2641 / 1849 | 10912.9 | macos-14-arm64/20260831.0302.1 |
| before | 37088913376 | `3759e2521d5c` | 2026-10-03T02:11:20Z | 2614 / 1835 | 10947.1 | macos-14-arm64/20260831.0302.1 |
| before | 37073568797 | `8327e76a2736` | 2026-10-02T22:38:29Z | 2555 / 1800 | 10830.4 | macos-14-arm64/20260831.0302.1 |
| before | 37057161905 | `a9397a719885` | 2026-10-02T19:54:35Z | 2556 / 1804 | 10641.5 | macos-14-arm64/20260831.0302.1 |
| after | 37919302081 | `2589bb17504c` | 2026-10-09T10:43:03Z | 3150 / 3148 | 888.6 | macos-14-arm64/20260831.0302.1 |
| after | 37922054026 | `4621c279bd71` | 2026-10-09T11:10:03Z | 3158 / 3157 | 840.7 | macos-14-arm64/20260831.0302.1 |
| after | 38119871519 | `f7183e607bc3` | 2026-10-11T06:59:50Z | 3156 / 3156 | 753.9 | macos-14-arm64/20260831.0302.1 |
| after | 38121297351 | `7e647ab70c7f` | 2026-10-11T07:25:07Z | 3179 / 3178 | 806.1 | macos-14-arm64/20260831.0302.1 |
| after | 38125746336 | `f5ab9aefd955` | 2026-10-11T08:42:22Z | 3187 / 3187 | 792.6 | macos-14-arm64/20260831.0302.1 |
| after | 38130159593 | `aae8adfecc72` | 2026-10-11T09:56:33Z | 3187 / 3186 | 813.4 | macos-14-arm64/20260831.0302.1 |
| after | 38139201912 | `fa03abb287c8` | 2026-10-11T12:25:30Z | 3187 / 3187 | 765.2 | macos-14-arm64/20260831.0302.1 |
| after | 38144390569 | `2922ae9de99a` | 2026-10-11T13:44:40Z | 3187 / 3186 | 830.8 | macos-14-arm64/20260831.0302.1 |
| after | 38151706190 | `040cc74c7960` | 2026-10-11T15:32:32Z | 3196 / 3196 | 779.7 | macos-14-arm64/20260831.0302.1 |
| after | 38154477288 | `819eee1bbc90` | 2026-10-11T16:12:21Z | 3198 / 3197 | 852.2 | macos-14-arm64/20260831.0302.1 |
| after | 38156413395 | `fe9143ff18f1` | 2026-10-11T16:40:08Z | 3198 / 3197 | 801.4 | macos-14-arm64/20260831.0302.1 |
| after | 38157438295 | `d14ff3fae4fb` | 2026-10-11T16:55:00Z | 3198 / 3198 | 863.1 | macos-14-arm64/20260831.0302.1 |
| after | 38158240488 | `553d201023dc` | 2026-10-11T17:06:17Z | 3198 / 3197 | 898.1 | macos-14-arm64/20260831.0302.1 |
| after | 38160727969 | `b7248f58f6e2` | 2026-10-11T17:41:55Z | 3212 / 3211 | 904.6 | macos-14-arm64/20260831.0302.1 |
| after | 38162294405 | `9ba7994771f8` | 2026-10-11T18:04:45Z | 3229 / 3229 | 850.2 | macos-14-arm64/20260831.0302.1 |
