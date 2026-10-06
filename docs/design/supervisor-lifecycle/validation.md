---
id: design-supervisor-lifecycle-validation
type: design
title: "Validation"
status: current
created: 2026-09-26
updated: 2026-10-05
last_verified: 2026-10-05
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0029
  - adr-t963-1
  - adr-t1165-1
  - adr-t1233-2
---

# Validation

`validating`のrunに対して、supervisorがセッションと同じleaseの下で、runごとのthreadで次を順に確認する。sessionは開いたまま（[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)の1）のことも、終了済みのこともある。最初に外れた項目が`failed`の理由（`last_error`）になり、以降は確認しない。

1. receiptが存在し、`Receipt`として解釈できる。
2. `run_id`が一致し、`result`が`succeeded`である。`tests`/`e2e`/`subagent_review`は`failed`でなく、`passed`には証跡、`not_applicable`には理由が空でなく書かれている。`commit`は完全なSHAである。
3. worktreeのHEADがrun branch `dagq/<run-id>`を指し、そのcommitがreceiptの`commit`と一致する。
4. commitがbase commitと異なり（commitなしを拒む）、base commitの子孫である。
5. `git status --porcelain --untracked-files=all`が空である。untracked fileもdirtyとみなす。
6. （欠番）taskの`verification_commands`はここでは実行しない。同じcommitに対する実行は`integrate`のrebase後の1回だけ（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定1。[`integrate`](integrate.md#integrate)のステップ5）なので、validatingの所要時間はreceiptとGitの照合だけで、`verification_command`イベントも`<run-dir>/verify-N.log`も作らない。検証コマンドで壊れるcommitもここでは`awaiting_integration`になり、`integrate`で`needs_session`になって自動resumeされる。
7. **宣言パス**（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）: taskの`paths`（`add --paths`）が空でなければ、run branchが今のmainから分かれた点（`GitRepository::merge_base`で`refs/heads/<branch>`とreceiptの`commit`のmerge-base。rebaseしていなければbase commit、resumeしたsessionがrebaseしていればそのmain）からreceiptの`commit`までに変わったパス（他のtaskがmainに着地したパスは数えない。`GitRepository::changed_paths`: `git diff --name-only -z --no-renames`。renameは両側、削除も含む）のうち、どのglobにも合わないもの（`domain::scope::out_of_scope`）を探す。1–5をすべて通ったrunだけをここで見る。見つかればrunを`failed`ではなく`needs_session`にし、`last_error`を`changed paths outside the task's --paths: <paths>`にして、`validation_finished`（`status: needs_session`、`scope_violation`に外れたパスの配列、`allowed_paths`にtaskのglob）の後に`scope_violation`イベント（`paths`、`allowed`、`reason`。`status`は持たない）を記録する。8より先に見るので、両方が欠けていれば先にこちらを直させ、resume後の再validationで8を見る。後の扱い（終了の依頼、close、自動resume）は8と同じで、resumeの依頼は宣言外のパスをrun branchから外す文面になる。`paths`の無いtaskでは7は何もしない。
8. **要求evidence**（ADR-0019の決定5）: taskの`required_evidence`（`add --evidence`）のうちworkerが裏付けるcheck（`domain::required_of`。`e2e`は含めない。Codexのworkerは`subagent_review`も含めない）の各checkについて、receiptのそのcheckの`status`が`passed`で`evidence_or_reason`が空白でないこと（`Receipt::missing_evidence`）。`e2e`はruntimeがreviewのpassの後に流すので（下の[runtimeが流すe2e](#runtimeが流すe2e)）、receiptには求めず、要否だけを決めて記録する。1–5をすべて通ったrunだけをここで見るので、receiptやGitの不整合は従来どおり`failed`になる。要求されたcheckに限り、2の「`failed`でない」と「evidence_or_reasonが空でない」はここに回す（`Receipt::check_requiring`）ので、`failed`・`not_applicable`・空のevidenceはどれも`failed`ではなくここで欠落になる。要求されていないcheckの`failed`は従来どおり2で`failed`になる。欠けていればrunを`failed`ではなく`needs_session`にし、`last_error`を`evidence missing: tests`（複数は`, `区切り、要求の順）にして、`validation_finished`（`status: needs_session`、`evidence_missing`に欠けたcheckの配列）の後に`evidence_missing`イベント（`checks`、`reason`、runがe2eを要るときは`e2e_requirement`。`status`は持たない: `stats`が`validation_finished`の`needs_session`で1回数えるため）を記録する。runのsessionには終了を依頼し、終わったらbackgroundのwrapperの残りを止める（resumeは自分のwrapperをbackgroundで起動する。runはworkspaceを開かない。[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。supervisorはこのrunを[`needs_session`](needs-session.md#needs_session)の手順で自動resumeし、不足しているcheckの実行とreceiptの書き直しを依頼する。要求の無いtaskでは8は何もしない。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容、7で外れたときだけ`scope_violation`と`allowed_paths`、8で欠けたときだけ`evidence_missing`、e2eの要否が分かったときは`e2e_requirement`、`receipt_observed`からこの検証までのload averageの`load_avg_mean` / `load_avg_max`（task 197。[`supervise`](supervise.md)の7））と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、着地まで依存taskを解放しない。

その後: `awaiting_integration`のrunは[Review](review.md#review-supervisor)に進む（leaseとsessionはそのまま）。`integration_approved`のあるrun（`integrate`が呼ばれた後にresumeしたrun）はreviewを待たずに終了の依頼→close→着地する（ADR-0027の決定3）。`needs_session`（宣言外のパス、evidenceの欠落）は終了の依頼→closeしてleaseを外す。`failed`は終了を依頼し、workspaceは調査のため閉じずにleaseを外す。

## runtimeが流すe2e

> **予定（goal 92）**: [ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定3で、実cmuxを要るe2eはinboxを開く`up` / `down`のtestだけになり、他のe2eは対話のstubのworkerをやめてcmuxの無いhostでも流れる形にする。1本ずつ流すことと関門の位置は変えない。e2eのrunはtask 1440からworkspaceを開かずbackgroundのwrapperで動く（[testの制約](../../development/testing.md#e2e)）が、e2eの関門と各ケースは今もcmuxを要る。残りの実装と本数・時間の前後の測定は後続のtask（1443・1444）。

[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)の決定2・3が決めたe2eの要否を、validatingが決めて記録する。e2eを流すのはworkerではなく、reviewのpassの後にruntimeがhostで流す工程で（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)。流し方は[Review](review.md#着地の前のe2e)の「着地の前のe2e」）、workerのreceiptは`e2e`を裏付けない（`domain::required_of`が`e2e`を落とす）。そこで落ちたe2e（上限の内に終わり、流し直しても印で通らない）だけがrunを`needs_session`に戻し、上限切れや始められないe2eは変更のせいとせずruntimeが流し直す。

- **設定**: main checkoutの`dagq.toml`の`[e2e]`の`paths`（globの配列。書式は[Run environment](run-environment.md)）。無ければ空で、差分からは何も求めない（taskの`--evidence e2e`だけで決まる）。supervisorはvalidatingを始めるたびとworkerのpromptを作るたびに読み直す（`Verifier::e2e_paths`）。読めなければ警告を出して空として扱う。
- **判定**（`domain::validation::E2eRequirement`、`ReceiptFacts::e2e_requirement`）: taskの`required_evidence`に`e2e`があれば（`ReceiptFacts::with_task_e2e`）`source: task`で要る。差分は見ない（`paths`の無いtaskは7の差分も読まない）。無く、`[e2e] paths`があれば、7と同じ差分（merge-baseからreceiptの`commit`まで、`GitRepository::changed_paths`）を読み、どれかのglobに合うpathがあれば`source: paths`で要る。無ければ要らない。receiptの`e2e`は、どの場合も要らないcheckとして2で検査する（`failed`か空の理由なら`failed`）。workerは`not_applicable`と理由（runtimeが流す）を書く。
- **記録**: `validation_finished`に`e2e_requirement`（`{required, source?, paths?}`。`source`は`task` / `paths`、`paths`は`source: paths`のとき合った差分のpath。要らないときは`{"required": false}`）を載せる。差分を読む前に外れた（1–5で拒んだ）runで、差分が要否を決めるときは載せない。`evidence_missing`にも、要るときは同じ`e2e_requirement`を載せる。着地の前のe2eは最後の`validation_finished`のこれを読む（`domain::run_e2e::due`）。`stats`もこれで`runs[].e2e`と`e2e`の群を作る（[stats](stats.md)）。
- **resume**: resumeが書き直したreceiptの解決の判定（`ResumeWatch::required_evidence`、衝突だけのresumeを飛ばす`resolved_head`）も`e2e`を含めない（`resume_required`）。解決したresumeは、`integrate`が呼ばれていないrunならもう一度validatingを通り、そこで差分から要否を決め直す。`integration_approved`のあるrun（着地のresume）はvalidatingを通らずに着地へ進み、前のvalidatingの要否で、着地の前のe2eを新しいcommitに流す。
- **integrateの検査**: `integrate`のreceiptの検査（[`integrate`](integrate.md)）も`required_of`のcheckだけを見て、`e2e`を求めない。e2eは`integrate`では流さない（ADR-t963-1決定4）。人が`dagq integrate`で手で着地させるrunにはruntimeのe2eが流れないので、e2eを要るrunを手で着地させる前は、run dirの`e2e-*.log`と`run_e2e_finished`を確かめる。
- **workerに知らせる**: workerのprompt（[Prompt](prompt.md)）は、runがe2eを要りうるとき（taskが`e2e`を要るか、`[e2e] paths`がある）に、e2eを自分で流さないこと、要るならruntimeがreviewのpassの後にhostで流し、落ちればsessionに戻すこと、receiptの`e2e`は理由つきの`not_applicable`にすることを1行で書く（`prompt::e2e_line`）。e2eのコマンド・`[e2e] paths`の一覧・関門の印・Codexのworkerの除外は載せない（ADR-t963-1決定5の除外はADR-t1233-2決定6でなくなった）。`Required evidence:`の行にも`e2e`は出ない。
- **この repository で置く範囲**（task 966が、固定バイナリがtask 965を含むのを確かめてから`dagq.toml`の`[e2e] paths`に理由のコメントつきで書いた）: cmuxのadapter（`src/infrastructure/adapters.rs`）、process（`src/infrastructure/process.rs`・`src/infrastructure/sessions.rs`）、launchd（`src/infrastructure/launchd.rs`）、lifecycle（`src/application/lifecycle.rs`・`src/application/supervise/handoff.rs`）、integrate（`src/application/integrate.rs`）、install（`src/application/install.rs`・`src/infrastructure/binaries.rs`）、update（`src/application/update.rs`・`src/application/supervise/update.rs`・`src/infrastructure/e2e_gate.rs`）、actorの起動（`src/application/actor_executor.rs`・`src/application/session.rs`・`src/application/headless_session.rs`・`src/infrastructure/claude.rs`・`src/infrastructure/codex.rs`）と、e2eそのもの（`tests/e2e.rs`と`tests/e2e/`の下）。`src/application/supervise/`の判断の部分は含めない（ADR-t963-1決定3）。
- **test**: `src/domain/validation.rs`の`a_diff_touching_the_e2e_paths_needs_the_e2e_but_no_evidence`・`a_diff_outside_the_e2e_paths_lands_without_e2e`・`a_task_that_requires_e2e_keeps_it_whatever_the_diff`・`without_e2e_paths_the_diff_requires_nothing`、`src/application/integrate.rs`の`check_receipt_records_the_e2e_a_diff_touching_the_e2e_paths_needs`、`tests/it/runtime_evidence.rs`の`the_e2e_a_run_needs_is_recorded_and_its_receipt_backs_none`。

