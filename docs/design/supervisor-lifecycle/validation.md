---
id: design-supervisor-lifecycle-validation
type: design
title: "Validation"
status: current
created: 2026-09-26
updated: 2026-09-29
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0029
  - adr-t963-1
---

# Validation

`validating`のrunに対して、supervisorがセッションと同じleaseの下で、runごとのthreadで次を順に確認する。sessionは開いたまま（[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)の1）のことも、終了済みのこともある。最初に外れた項目が`failed`の理由（`last_error`）になり、以降は確認しない。

1. receiptが存在し、`Receipt`として解釈できる。
2. `run_id`が一致し、`result`が`succeeded`である。`tests`/`e2e`/`subagent_review`は`failed`でなく、`passed`には証跡、`not_applicable`には理由が空でなく書かれている。`commit`は完全なSHAである。
3. worktreeのHEADがrun branch `dagq/<run-id>`を指し、そのcommitがreceiptの`commit`と一致する。
4. commitがbase commitと異なり（commitなしを拒む）、base commitの子孫である。
5. `git status --porcelain --untracked-files=all`が空である。untracked fileもdirtyとみなす。
6. （欠番）taskの`verification_commands`はここでは実行しない。同じcommitに対する実行は`integrate`のrebase後の1回だけ（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定1。[`integrate`](integrate.md#integrate)のステップ5）なので、validatingの所要時間はreceiptとGitの照合だけで、`verification_command`イベントも`<run-dir>/verify-N.log`も作らない。検証コマンドで壊れるcommitもここでは`awaiting_integration`になり、`integrate`で`needs_session`になって自動resumeされる。
7. **宣言パス**（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）: taskの`paths`（`add --paths`）が空でなければ、run branchが今のmainから分かれた点（`GitRepository::merge_base`で`refs/heads/<branch>`とreceiptの`commit`のmerge-base。rebaseしていなければbase commit、resumeしたsessionがrebaseしていればそのmain）からreceiptの`commit`までに変わったパス（他のtaskがmainに着地したパスは数えない。`GitRepository::changed_paths`: `git diff --name-only -z --no-renames`。renameは両側、削除も含む）のうち、どのglobにも合わないもの（`domain::scope::out_of_scope`）を探す。1–5をすべて通ったrunだけをここで見る。見つかればrunを`failed`ではなく`needs_session`にし、`last_error`を`changed paths outside the task's --paths: <paths>`にして、`validation_finished`（`status: needs_session`、`scope_violation`に外れたパスの配列、`allowed_paths`にtaskのglob）の後に`scope_violation`イベント（`paths`、`allowed`、`reason`。`status`は持たない）を記録する。8より先に見るので、両方が欠けていれば先にこちらを直させ、resume後の再validationで8を見る。後の扱い（`/exit`、close、自動resume）は8と同じで、resumeの依頼は宣言外のパスをrun branchから外す文面になる。`paths`の無いtaskでは7は何もしない。
8. **要求evidence**（ADR-0019の決定5）: taskの`required_evidence`（`add --evidence`）の各checkについて、receiptのそのcheckの`status`が`passed`で`evidence_or_reason`が空白でないこと（`Receipt::missing_evidence`）。1–5をすべて通ったrunだけをここで見るので、receiptやGitの不整合は従来どおり`failed`になる。要求されたcheckに限り、2の「`failed`でない」と「evidence_or_reasonが空でない」はここに回す（`Receipt::check_requiring`）ので、`failed`・`not_applicable`・空のevidenceはどれも`failed`ではなくここで欠落になる。要求されていないcheckの`failed`は従来どおり2で`failed`になる。欠けていればrunを`failed`ではなく`needs_session`にし、`last_error`を`evidence missing: e2e`（複数は`, `区切り、要求の順）にして、`validation_finished`（`status: needs_session`、`evidence_missing`に欠けたcheckの配列）の後に`evidence_missing`イベント（`checks`、`reason`。`status`は持たない: `stats`が`validation_finished`の`needs_session`で1回数えるため）を記録する。runのsessionには`/exit`を送り、終わったらworkspaceを閉じる（resumeは自分のworkspaceを開く）。supervisorはこのrunを[`needs_session`](needs-session.md#needs_session)の手順で自動resumeし、不足しているcheckの実行とreceiptの書き直しを依頼する。要求の無いtaskでは8は何もしない。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容、7で外れたときだけ`scope_violation`と`allowed_paths`、8で欠けたときだけ`evidence_missing`、`receipt_observed`からこの検証までのload averageの`load_avg_mean` / `load_avg_max`（task 197。[`supervise`](supervise.md)の7））と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、着地まで依存taskを解放しない。

その後: `awaiting_integration`のrunは[Review](review.md#review-supervisor)に進む（leaseとsessionはそのまま）。`integration_approved`のあるrun（`integrate`が呼ばれた後にresumeしたrun）はreviewを待たずに`/exit`→close→着地する（ADR-0027の決定3）。`needs_session`（宣言外のパス、evidenceの欠落）は`/exit`→closeしてleaseを外す。`failed`は`/exit`を送り、workspaceは調査のため閉じずにleaseを外す。

## 今後: 差分から決めるe2eのevidence（未実装）

[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)の決定2・3。**この節はまだ実装されていない**（goal 66の後続のtaskが、[Auto-update](auto-update.md)の入れ替えの前のe2eの関門を着地させた後に実装する）。今は8の要求はtaskの`required_evidence`だけで決まる。

- **設定**: main checkoutの`dagq.toml`の`[e2e]`の`paths`（globの配列。書式は`add --paths`と同じで、repository rootから、`*`は1階層、`**`は任意の深さ）。無ければ空で、差分からは何も要求しない。runtimeが読むのは`[run.env]`と同じくmain checkoutの作業ファイル。`[e2e]`を知らない古いバイナリは`dagq.toml`を読めなくなるので、固定バイナリが対応してから足す。
- **判定**: 8で、要求するcheckを「taskの`required_evidence`」と「7と同じrunの差分（merge-baseからreceiptの`commit`まで、`GitRepository::changed_paths`）のどれかが`[e2e].paths`のどれかに合えば`e2e`」の和にする。以降の扱い（`needs_session`、`evidence_missing`、resume）は今の8と同じ。`validation_finished`と`evidence_missing`に、`e2e`を要求した出どころ（`task` / `paths`）と、`paths`のときは合ったpathを載せる。範囲の外のrunは、receiptの`e2e`が`not_applicable`（理由つき）でも着地できる（要求されていないcheckの扱い、2）。
- **workerに知らせる**: workerのprompt（[Prompt](prompt.md)）に、`[e2e].paths`があればそのglobと、差分がそれに触れたらe2eが必須であることを書く。taskに`--evidence e2e`を明示したときは今までどおり必須と書く。
- **この repository で置く範囲**（実装のtaskがそのときのファイルの配置で確かめて`dagq.toml`に書く候補）: cmuxのadapter（`src/infrastructure/adapters.rs`）、process（`src/infrastructure/process.rs`・`src/infrastructure/sessions.rs`）、launchd（`src/infrastructure/launchd.rs`）、lifecycle（`src/application/lifecycle.rs`・`src/application/supervise/handoff.rs`）、integrate（`src/application/integrate.rs`）、install（`src/application/install.rs`・`src/infrastructure/binaries.rs`）、update（`src/application/update.rs`・`src/application/supervise/update.rs`）、actorの起動（`src/application/actor_executor.rs`・`src/application/session.rs`・`src/application/headless_session.rs`・`src/infrastructure/claude.rs`・`src/infrastructure/codex.rs`）と、e2eそのもの（`tests/e2e.rs`）。`src/application/supervise/`の判断の部分は含めない（runtimeのcommitの多くが触るので狭める効果が消え、fakeのcmuxのin-processのtest（`tests/it/runtime_*`）が確かめ、実物との組み合わせは入れ替えの前の関門が確かめる。ADR-t963-1決定3）。
- **test**: `domain::scope`の判定のunit testと、`tests/it/runtime_*`に、`[e2e].paths`に触れるrunが`e2e`の`not_applicable`で`evidence_missing`になり、触れないrunは着地し、`--evidence e2e`のtaskは触れなくても必須のままであることを足す。
