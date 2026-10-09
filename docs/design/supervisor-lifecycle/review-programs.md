---
id: design-supervisor-lifecycle-review-programs
type: design
title: "プログラムのreview"
status: current
created: 2026-10-09
scope: runtime
related:
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-run-environment
  - adr-t1895-1
  - adr-t1895-2
---

# プログラムのreview

[Review](review.md)の段の先頭で、agentのjobより前に流すprogramのjob（[ADR-t1895-1](../../adr/2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)・[ADR-t1895-2](../../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)）。
形式の違反があるうちにagentを動かすと、時間とtokenを使った上で同じ差し戻しになるので、落ちたらagentを起動せずにworkerへ返す（fail-fast）。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| 段の流れと記録 | `src/application/supervise/landing.rs`の`start_review_programs`・`go_on_with_programs`・`review_program_ended`・`review_programs_failed` |
| 次の一手の判断と差し戻しの理由 | `src/domain/review_programs.rs`の`step`・`after_failure`・`rejection_reason` |
| 一覧とscriptの読み込み | `src/application/review_programs.rs`の`snapshot_programs` |
| jobの起動と終わり | `src/application/supervise/jobs.rs`の`start_review_program`・`HeadlessJob::poll_program`・`ProgramsWatch` |
| eventの欄 | `src/domain/event_kind.rs`の`REVIEW_PROGRAMS_STARTED`・`REVIEW_PROGRAM_FINISHED`・`REVIEW_PROGRAMS_FINISHED`のdoc comment |
| 送れなかった差し戻しの引き継ぎ | `src/application/supervise/adopt.rs`の`programs_rejected_after` |

## 範囲

- 流すのはtestを含まない速い形式の検査（書式・構造・参照を読むだけのもの）で、build・unit test・integration test・e2eは流さない（ADR-t1895-2決定1）。
- testを含む検証は`integrate`のrebase後の1回のまま（[Validation](validation.md)、[integrate](integrate.md)）で、プログラムのreviewはその前に形式の誤りを安く返すためのものである。
  一覧にtaskのverifyのtestやcoverageのコマンドを挙げず、integrateの検証と二重にしない。
  プログラムのreviewが通ってもverifyは省かれない。

## 段の流れ

1. **選び方**: reviewを最初から始めるたび（validationの後、adoptがやり直すときなど）に、landing branchの今のcommitから`[review.programs.<name>]`とscriptを読み、receiptの範囲の変えたpathで選ぶ。
   選ばれたものが無い（設定が無い、どのpathも当たらない）reviewは何も記録せず、今までどおりagentのreviewに進む。
   agentのjobのやり直し（読めないverdict・非0の終了・providerの移り）と控えの後の再開はagentのjobだけを始め直し、プログラムのreviewを流し直さない。
2. **流し方**: `review_programs_started`を記録し、設定の順に1本ずつprogramのjobとして流し、各々の終わりに`review_program_finished`を記録する。
   supervisorのphaseは`Phase::ReviewPrograms`で、記録するphaseとrunの`progress`は`review`である。
   providerを使わないので、queueの控え（[認証と利用上限のaskの待ち](queue-hold.md)）の間も流れ、`Phase::ReviewHeld`で待つのは全部が通った後のagentのreviewだけである。
3. **全部が通る**: 全部がexit 0なら`review_programs_finished`の後、agentのreview（`review_started`）に進む。
4. **1本が落ちる**: 非0で終わったら、後のprogramもagentのjobも起動せず、`review_programs_finished`の後に、落ちたprogramの名前と出力の末尾を理由にreviseと同じ依頼をworkerに送る（`send_revise`。ADR-t1895-2決定3）。
   回のreviseに1回と数え、上限を超えればreviseと同じく人の`approve_landing`のaskになる。
   直したreceiptは[Validation](validation.md)からやり直し、プログラムのreviewも最初から流れる。
5. **起動の失敗と時間切れ**: 一覧かscriptが読めない、起動できない（未実装のbackendも）、時間の上限を超えたときは、workerの変更のせいとせずreviewの失敗にする（決定4）。
   最初のprogramから1回だけ流し直し、なお失敗すればagentのjobを起動せずに`review_failed`と`approve_landing`のaskにする。
   この流し直しはreviewのやり直しの1回に数えるので、その後のagentのreviewは読めないverdictでもやり直さない（[Review](review.md)の不変条件）。

eventはrunのeventとして`events --run`と`timeline`（`--full`で欄）に出る。

## 引き継ぎ

- handoffはprogramのjobを止め、次のprocessはreviewを最初からやり直す。
  死んだsupervisorのjobは`headless_jobs`の引き継ぎが止める（[Headless job processes](headless-job-processes.md)）。
- 差し戻しの依頼（`revise_requested`）の後はagentのreviseと同じ待ちに戻る。
  依頼を送れなかった（`revise_unsent`）runのaskは、その前の`review_finished`でなく、後にあるprogramの差し戻しの理由で聞く。

## 読む元・流す場所・env・結果

- **snapshot**: 試行ごとにlanding branchの今のcommitのtreeから`[review.programs.<name>]`（書式は[Run environment](run-environment.md)）とその`script`の中身を読む（`review_programs::snapshot_programs`）。
  worktreeとmain checkoutのファイルは読まず、workerが変えた設定やscriptは着地まで効かない。
  scriptは読んだ中身をqueueの`review-programs/<run id>/`（workerが書けない場所）に書き出して実行し、出力はrun dirに書く（ファイルの名前は`start_review_program`）。
  programは`script`でだけ名指す（理由は`ReviewProgram`）。
  cwdから呼ぶscriptとツールがcwdで読む設定（`rust-toolchain.toml`・`.cargo/config.toml`）はworkerの選んだ版が効きえ、runtimeは検出しない（integrateのverifyも同じ）。
- **流す場所**: reviewのactor（`ReviewJob`）のbackendで`ProgramBackends`が`ReviewProgramBackend`の実装を選ぶ。
  hostは`HostPrograms`、未実装のPodmanは起動の失敗にしhostに戻さない（fail closed）。
  cwdはrunのworktreeである。
- **env**: 起動元のenvを消し、e2eの関門と共通の部品（`passed_env::PassedEnv`）で絞る。
  資格情報の除外は常に効き、例外を持たず、cmuxのsocketのpassword・`CMUX_*`・queue serviceとbrokerに届く`DAGQ_*`は渡らない。
  `PATH`からは空・相対の項目と、runのworktree・run dir・main checkoutとその下を指す項目を除く（`review_programs::narrowed_path`）。
- **結果**: `HeadlessJob::poll_program`が終了のstatusとstdout・stderrの末尾を返す。
  上限（programの`timeout_secs`、無ければ`[review.jobs]`）を超えればprocess groupごと止め、statusは無い。
