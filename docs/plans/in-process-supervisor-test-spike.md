---
id: plan-in-process-supervisor-test-spike
type: plan
title: スパイク：port の fake で supervisor の 1 周を src の #[cfg(test)] のプロセスの中で回せるか、どの形でどれだけ速く「supervisor の配線」の IT を置き換えられるか
status: completed
created: 2026-11-01
owners:
  - hisamekms
tags:
  - testing
  - performance
  - measurement
related:
  - plan-it-reduction
  - development-testing
  - adr-t1410-1
  - adr-t3802-1
---

# スパイク：supervisor の 1 周をプロセスの中の test で回す形

問いは「supervisor の 1 周（ready の task の claim → worker の session の起動と receipt → review → 着地の記録の、少なくとも 1 つの機能の配線）を、外部プロセス・実 Git・SQLite のファイルを使わず `src` の `#[cfg(test)]` で回せるか。成り立つのはどの形か」（goal 239 の「プロセスの中の層」の前提。goal の文書は[IT の削減の計画](it-reduction.md)）。
(a) `Queue` の手書きのメモリの fake と、(b) 実の `SqliteQueue` をメモリの SQLite で開いて他の port を stub にする形を試作し、`runtime_already_satisfied::a_verified_claim_is_closed_without_a_commit_by_the_answer` の前半と同じ配線を確かめた。

## 要点

- **判定は holds。** 形 (b) で、claim → worker の session（wrapper の本体 `application::session::run_session` を thread で）→ turn → receipt → 検証（`validation_finished` の `already_satisfied`）→ main の snapshot の検証（`run_satisfied_verified`、`commands: 2`・`passed: true`）→ claim の判定の review の job → `approve_close` の ask → lease の解放 までが、外部プロセス・Git・SQLite のファイルなしにプロセスの中で通った。IT がこの前半で確かめる event と ask は同じものが出た。
- **形 (a) は配線の層にならない。** 1 本の IT の 2 回の supervise が queue の service に 120 種・2,147 回の操作を送る。`Queue` の合成の必須の method は 350 本で、stub だけで 1,096 行になり、claim・lease・遷移の意味を書けば SQLite の queue をもう 1 つ実装することになる。
- **メモリの SQLite は `:memory:` ではなく memdb の VFS（`file:/<名前>?vfs=memdb`）で開く。** `:memory:` は接続ごとに別の DB になり、service が要求ごとに開き直す queue では空に見える（確かめた）。memdb の URI は今の `SqliteQueue::open` がそのまま開け（bundled の SQLite が URI を読む）、複数の接続と thread で 1 つの DB を共有し、書き込みの待ちは busy の待ちになる。
- **速さは同じ前半で IT の 2〜3 倍（中央値どうしで 2.6 倍）、review の job の見張りを 50 ms にすると 3.6〜4.7 倍。** プロセスの中で 2.5〜3.0 秒（見張りを 50 ms にすると 1.6〜1.7 秒）、IT の同じ前半は 6.2〜7.6 秒。残る秒の大半は production の実時間の待ち（review の job の見張り 1 秒・wrapper の起動の待ち 100 ms・各 tick）で、Git とプロセスではない。
- **この層は testing.md の「unit test」には当たらない。** 回すのは production の loop・heartbeat・wrapper で、実時間の待ちと thread を持つ。test の行は sleep も外部プロセスも使わないが、規則（ADR-t1410-1 決定 5）の unit test とは別の種類の test として規則を置く必要がある（下の「規則の読み」）。

## 条件

- 対象の commit: `be44e3e00dcb66094588e490bca3d42ebac92e4c`（試作はこの上の作業 tree で、commit していない）
- 道具: cargo 1.99.1、rustc 1.99.1（2026-10-13）、cargo-nextest 0.9.146、rusqlite 0.40（`bundled`）・libsqlite3-sys 0.38.2、queue の schema の `user_version` 92
- 環境: macOS 27.2（26B123）、Apple M1・8 core・16 GiB、`NEXTEST_TEST_THREADS=6`、他の run が並ぶ host（load average 11〜18 の下で測った）
- provider と経路: runtime の headless の worker（Claude Code、dagq の run 619c4cd3-8502-4f22-ba33-6779f0e7f83b）の session で、試作の test は `cargo test --lib`、IT は `cargo nextest run --test it` で流した
- 使った時間: 開始 2026-11-01 01:38 JST から文書を書き終えるまで約 1 時間 20 分（上限 3 時間）。試作は (a) 1 回・(b) 2 回（上限は各形 2 回）

## 試した形と測った値

| 形 | 試作 | 1 本の秒 | 書いた行数 | 要った port |
|---|---|---|---|---|
| (a) 手書きの `Queue` の fake | 1 回目: `Queue` の合成の全 trait の stub を生成して compile だけ（意味は書かず、走らせていない） | 測れず（走らない） | stub だけで 1,096 行（必須の method 350 本、23 trait） | queue の全部。supervisor は queue を service（`ServiceCaller`）越しに使うので、sign in と op の名前の振り分けも fake が要る |
| (b) memdb の SQLite と port の stub | 1 回目: 空の queue で `supervise()` の 1 周 | 0.75 秒（うち queue の初期化 0.23〜0.26 秒） | 下の 2 回目に含む | 下の 2 回目と同じ |
| (b) | 2 回目: ready の task 1 本の claim → session → receipt → snapshot の検証 → review → `approve_close` の ask | 5 回で 2.52・2.59・2.57・2.68・3.03 秒。review の見張りを 50 ms にして 3 回で 1.60・1.66・1.69 秒 | harness 267 行・stub 284 行（生成した 99 method のうち振る舞いを入れたのは 56、残り 43 は通らない `unreachable!`）・production への足し 28 行 | `Ports` の 55 欄のうち `Option` 以外の 34 欄を埋め、trait 19 個（と `Spawned`）を stub にした（下） |

比べた IT（同じ host・同じ時間帯、nextest の `--stress-count 3`）:

| test | 1 本の秒 |
|---|---|
| `runtime_already_satisfied::a_verified_claim_is_closed_without_a_commit_by_the_answer`（前半と後半） | 18.3・20.4・19.4、別の時刻に 18.5・16.1・15.0 |
| 同じ test の前半だけ（`claimed()` と前半の assert を一時の test にして測り、消した） | 6.86・7.60・6.15 |
| `runtime_already_satisfied::a_claim_without_its_base_checks_is_an_invalid_receipt` | 5.90・6.89・6.85 |

その他の測った値:

- 1 本の IT の 2 回の supervise が service に送った操作: 2,147 回・120 種（`ServiceRoute::InProcessThrough` の一時の数える layer で数えた。多いのは `runs_with_status` 143、`asks` 120、`latest_queue_event` 112）。
- メモリの queue: `SqliteQueue::init(":memory:")` は 1 回 0.21〜0.26 秒（debug build、全 migration）。初期化した DB を backup の API で別のメモリの DB に写すのは 1 回 約 0.1 ms。`:memory:` を 2 つ目の接続で開くと「queue is not initialized」になる。memdb の URI では 2 つ目の接続から同じ queue が開け、1 つ目の接続が書き込みの lock を持つ間の 2 つ目の書き込みは busy の待ちの後に通った。journal_mode は `memory`（WAL ではない）。

### 形 (b) の試作の要点

置き場は `src/compose`（application と infrastructure の両方が見える composition root）の `#[cfg(test)]` の module で、`compose::supervisor::supervise_with_reviewer` の代わりに `Ports` を直接組み、`application::supervise::supervise` を呼んだ。

- queue: 本物の `InProcessCaller`（service の use case を同じプロセスで答える）を memdb の URI の queue に向けた。`SupervisorQueues` と `SupervisorPorts` は production のまま。
- session: `SessionWrappers::launch_background` の stub が、`dagq session --background` の代わりに `application::session::run_session` を thread で動かす（wrapper の queue は同じ memdb の URI の `SqliteQueue`）。handle は `background:<pid>:<start>` の形にし、`ProcessControl` の stub がその pid を生きている・その start と答える。
- agent: `AgentProvider` の stub の `turn_command` と `review_command` は名前だけの `CommandSpec` を返し、`Spawner` の stub が、worker の turn なら receipt と Claude の結果の行を、review なら verdict を `MemoryFiles` に書いて、終わった `Spawned` を返す。turn の出力を読むのは本物の Claude の adapter の `turn_reader` と `idle_hook`。
- Git: `Repository`・`RepositoryReads`・`MainRemote` の stub が、固定の head、run の worktree の名前から作る branch、scratch の worktree の成功を返す。
- 検証: `Verifier::run_to_log` の stub が command を記録して成功を返す。
- その他: `RunFiles` は既存の `application::memory_files::MemoryFiles`、container・token・telemetry・held records は空を返す stub、時計は止まった `Clock`。

stub にした trait（19）: `Repository`・`RepositoryReads`・`MainRemote`・`Verifier`・`ContainerRuntime`・`RunContainers`・`VerificationTokens`・`VerificationContainers`・`SessionWrappers`・`AgentProvider`・`AgentSignals`・`Transcripts`・`Spawner`（と `Spawned`）・`ReviewProgramBackend`・`ServiceAccess`・`TelemetryTokens`・`TelemetrySinks`・`HeldRecords`・`ProcessControl`。

production に足した口（試作だけで、commit していない）:

1. `InProcessCaller` を、token のファイルの置き場（`queue_dir`）を db の path と別に渡して作る constructor。今は `queue_dir` を db の path の親から求めるので、memdb の URI では `file:` という相対の dir に token を書いてしまう。試作は一時の dir を渡した（層を作るときはメモリの token の置き場にする）。
2. `MemoryFiles` を `compose` の test から見えるようにする再公開。

足さずに済んだ・気づいたこと:

- application の supervise・integrate・review は std の fs と外部プロセスを直接使わず、全部 port を通る（`std::fs` と `Command::new` の直接の呼び出しは 0）。
- queue は run の dir を db の path から求める（memdb の URI では `file:/runs/<run>`）。layout の `runs_dir` をそれに合わせれば `MemoryFiles` の上では困らない。
- Claude の adapter の `turn_command` は設定のファイルを直接書くので、stub の `turn_command` は本物に委ねられない。

### 形 (a) の試作の要点

`Queue` の合成（`StateStore`・`EventStore`・`QueueRecords`・`InboxWatchLog`・`EvalRounds`・`ObservationQueue`・`HostOpsQueue`・`RecordingQueue` と、その下の `RunTransitions`・`AskStore`・`RunRecovery`・`DraftPlannerStore` ほか）の必須の method の stub を生成し、`Box<dyn Queue + Send>` に入ることを compile で確かめた。
1 本の配線が 120 種の操作を使い、その多く（claim と lease、run の phase の遷移、ask の開閉、snapshot の記録）は SQLite の transaction の中の条件と組で意味を持つ。手書きの fake は、それを SQL ではなくメモリの構造でもう一度書くことになり、production の queue と食い違っても気づけない。そのため 2 回目の試作はせず、`application::port_fakes` が今している狭い use case の unit test の fake に留めるのがよいと読んだ。

## 規則の読み

- **`:memory:` と memdb の SQLite と、unit test の規則。** testing.md の「判断と境界のtest」が unit test に禁じるのは「SQLiteのファイル」で、メモリの SQLite はファイルを作らないので字面には当たらない。ただし ADR-t1410-1 決定 2 は SQLite（schema・transaction・migration）を境界とし、決定 5 は境界を要るものを integration test に置く。メモリの SQLite の層の test は、SQLite そのもの（ファイルの lock・WAL・migration・別のプロセスとの競合）を確かめるものではなく、判断の結果が event・ask・status に届く配線を確かめるものとして置く。SQLite の境界の代表は IT に残す。
- **この層は unit test ではない。** test の行は sleep・実時間の時計・外部プロセス・Git を使わないが、回す production の loop と wrapper は実時間で待ち（heartbeat の thread、wrapper の起動の待ち、review の job の見張り、tick）、thread を起こす。試作も detached の wrapper の thread と process-global の static を持ち、待ちの上限（testing.md の「待ちの上限」）を満たさないので commit しなかった。層を作るときは、unit test とも IT とも別の種類の test として、外部プロセス・Git・SQLite のファイルを使わないこと、test の行に sleep を置かないこと、production の待ちを上限つきの 1 つの期限（IT の `common::within` に当たるもの）で包むこと、起こした thread を test の終わりまでに join することを規則にする。
- **ADR-t1410-1 との関係。** 決定 2 は「supervisorの配線（判断の結果がevent・ask・statusに届くこと）」を IT の境界に数える。配線をこの層へ移すなら、決定 2 の配線を「プロセスの中の層が確かめ、IT には SQLite・Git・プロセス・復旧と adopt の代表だけが残る」と改める ADR が要る（決定 5 の unit test の規則は変えず、層を別の種類として足す）。

## 判定

holds。形 (b)（memdb の SQLite の上の本物の queue と service、他の port の stub）で、問いの「claim → worker の session の起動と receipt → review の配線」が外部プロセス・実 Git・SQLite のファイルなしに `src` の `#[cfg(test)]` で回り、IT と同じ event と ask を確かめられた。形 (a) は成り立たない（意味を持つ fake の量が queue の再実装になる）。

根拠の限り: 後半（`close` の答えでの close、依存する task の claim と着地）は試していない。着地は `Repository` の rebase・`advance_main`・`MainRemote` の push と `Verifier` の着地の検証を stub にする必要があり、2 回の試作の上限の外とした。review の差し戻し（revise）を生きている worker の session に送る経路も試していない。速さは前半で 2〜3 倍（見張りを 50 ms にして 3.6〜4.7 倍）で、production の実時間の待ちを test の設定で縮める口を足さなければ、それ以上は縮まない。

## 後続の計画への推奨

- **形と置き場。** 形 (b)。置き場は `src/compose` の `#[cfg(test)]` の module（例えば `src/compose/wiring_tests/`）。queue は process ごとに 1 回だけ初期化した memdb の雛形を backup で test ごとの名前の memdb に写し（0.1 ms）、test ごとに別の URI にして並列に流す。fake は生成した `unreachable!` の stub でなく、状態を持つ少数の型にまとめる: Git（`Repository`・`RepositoryReads`・`MainRemote`。commit・branch・main の head をメモリに持つ）、session（`SessionWrappers`。`run_session` を join できる thread で動かす）、agent（`AgentProvider`・`AgentSignals`・`Transcripts`・`Spawner`。worker の turn と review の job の出力を test が台本で渡す）、`Verifier`、`ProcessControl`、空の container と token と telemetry。
- **production に足す口。** `InProcessCaller` を token の置き場ごと渡して作る口（メモリの token の置き場を含む）、`MemoryFiles` の `compose` の test への公開、速さが要るなら wrapper の起動の待ちと review の job の見張りの間隔を test の設定から渡す口。production の event・payload・閾値の意味は変えない。
- **task の分け方と順。**
  1. 規則の task: この層を unit test と IT の間の種類として ADR と testing.md に書き、ADR-t1410-1 決定 2 の配線の置き場を改める。登録簿から境界の種類「supervisorの配線」を退くなら、ADR-t3802-1 と `scripts/check-it-added-tests.sh` の境界の種類の値も同じ task で改める（層の実装の前か同時）。
  2. 層を作る task: harness と状態を持つ fake を作り、最初の test に `runtime_already_satisfied` の前半（この試作と同じ配線）を置く。
  3. 着地まで広げる task: Git の fake に rebase・`advance_main`・push を足し、`runtime_already_satisfied` の後半（close と依存する task の着地）を層に移す。
  4. module ごとの移し替えの task（1 module 1 task、並行の task が同じファイルに触れない）: goal 237 の次の候補の順に `runtime_triage`・`runtime_already_satisfied`・`runtime_task_release`・`replan_planning`・`runtime_review`・`runtime_landing_answers`・`runtime_review_revise_limit`。各 task は移した test の中身の行き先（層の test・unit test・残す IT）を対応づけ、時間の関門の許可の一覧と登録簿の項目を同じ変更で外す。
- **登録簿の「supervisorの配線」。** 登録簿では境界の種類（`kind`）で、今その種類の id は `review-subagent-judgments-applied-to-the-live-session` 1 つだけ。配線（判断の結果が event・ask・status に届くこと）は層で確かめられるので、種類ごと退けると読む。IT に残すのは SQLite のファイルと transaction の競合、実 Git（worktree・rebase・push）、実プロセス（wrapper の起動・signal・孤児）、復旧と adopt の代表だけになる。ただし今の 1 つの id は生きている session への revise を確かめるもので、この Spike は試していない。その id は層で revise の経路を確かめるまで残し、種類を退けるのは module ごとの移し替えとその id の移し替えが済んでからにする。移す前の IT と許可の一覧の「supervisor の配線」の理由は今のまま残す。
