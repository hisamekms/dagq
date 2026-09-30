---
id: design-supervisor-lifecycle-validation
type: design
title: "Validation"
status: current
created: 2026-09-26
updated: 2026-09-30
last_verified: 2026-09-30
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0029
  - adr-t963-1
  - adr-t1165-1
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
8. **要求evidence**（ADR-0019の決定5）: taskの`required_evidence`（`add --evidence`）と、runの差分が`dagq.toml`の`[e2e] paths`に触れたときの`e2e`（下の[差分から決めるe2eのevidence](#差分から決めるe2eのevidence)）の各checkについて、receiptのそのcheckの`status`が`passed`で`evidence_or_reason`が空白でないこと（`Receipt::missing_evidence`）。1–5をすべて通ったrunだけをここで見るので、receiptやGitの不整合は従来どおり`failed`になる。要求されたcheckに限り、2の「`failed`でない」と「evidence_or_reasonが空でない」はここに回す（`Receipt::check_requiring`）ので、`failed`・`not_applicable`・空のevidenceはどれも`failed`ではなくここで欠落になる。要求されていないcheckの`failed`は従来どおり2で`failed`になる。欠けていればrunを`failed`ではなく`needs_session`にし、`last_error`を`evidence missing: e2e`（複数は`, `区切り、要求の順）にして、`validation_finished`（`status: needs_session`、`evidence_missing`に欠けたcheckの配列）の後に`evidence_missing`イベント（`checks`、`reason`、`e2e`を求めたときは`e2e_requirement`。`status`は持たない: `stats`が`validation_finished`の`needs_session`で1回数えるため）を記録する。runのsessionには`/exit`を送り、終わったらworkspaceを閉じる（resumeは自分のworkspaceを開く）。supervisorはこのrunを[`needs_session`](needs-session.md#needs_session)の手順で自動resumeし、不足しているcheckの実行とreceiptの書き直しを依頼する。要求の無いtaskでは8は何もしない。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容、7で外れたときだけ`scope_violation`と`allowed_paths`、8で欠けたときだけ`evidence_missing`、`e2e`の要否が分かったときは`e2e_requirement`、`receipt_observed`からこの検証までのload averageの`load_avg_mean` / `load_avg_max`（task 197。[`supervise`](supervise.md)の7））と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、着地まで依存taskを解放しない。

その後: `awaiting_integration`のrunは[Review](review.md#review-supervisor)に進む（leaseとsessionはそのまま）。`integration_approved`のあるrun（`integrate`が呼ばれた後にresumeしたrun）はreviewを待たずに`/exit`→close→着地する（ADR-0027の決定3）。`needs_session`（宣言外のパス、evidenceの欠落）は`/exit`→closeしてleaseを外す。`failed`は`/exit`を送り、workspaceは調査のため閉じずにleaseを外す。

## 差分から決めるe2eのevidence

[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)の決定2・3（task 965）。8で求めるcheckは、taskの`required_evidence`と、runの差分が`dagq.toml`の`[e2e] paths`に触れたときの`e2e`の和。

- **設定**: main checkoutの`dagq.toml`の`[e2e]`の`paths`（globの配列。書式は[Run environment](run-environment.md)）。無ければ空で、差分からは何も求めない（今までのtaskの`--evidence`だけの動き）。supervisorはvalidatingを始めるたびとworkerのpromptを作るたびに読み直す（`Verifier::e2e_paths`）。読めなければ警告を出して空として扱う。
- **判定**（`domain::validation::E2eRequirement`、`ReceiptFacts::e2e_requirement`）: taskの要るevidence（`required_of`で絞ったもの）に`e2e`があれば`source: task`で必須、差分は見ない（`paths`の無いtaskは7の差分も読まない）。無く、`[e2e] paths`があれば、7と同じ差分（merge-baseからreceiptの`commit`まで、`GitRepository::changed_paths`）を読み、どれかのglobに合うpathがあれば`source: paths`で必須、無ければ求めない。差分が決まるまでは、2の検査でreceiptの`e2e`を要るcheckと同じに扱い（`failed`や空のevidenceで`failed`にしない）、8で要否が決まってから、求めない`e2e`をもう一度要らないcheckとして検査する（`failed`か空の理由なら、2と同じく`failed`。`result_commit`は残す）。求めた`e2e`が欠けていれば今までの8と同じく`needs_session`（`evidence_missing`）とresume。範囲の外のrunは、receiptの`e2e`が`not_applicable`（理由つき）でも着地できる。
- **記録**: `validation_finished`に`e2e_requirement`（`{required, source?, paths?}`。`source`は`task` / `paths`、`paths`は`source: paths`のとき合った差分のpath。求めないときは`{"required": false}`）を載せる。差分を読む前に外れた（1–5で拒んだ）runで、差分が要否を決めるときは載せない。`evidence_missing`にも、`e2e`を求めたときは同じ`e2e_requirement`を載せる。`stats`はこれを記録した最後の`validation_finished`で`runs[].e2e`と`e2e`の群を作る（[stats](stats.md)）。
- **resume**: 差分で`e2e`を求めて止めたrunのresumeは、最後の`validation_finished`の`e2e_requirement.required`を見て、書き直されたreceiptの解決の判定（`ResumeWatch::required_evidence`と、衝突だけのresumeを飛ばす`resolved_head`）にも`e2e`を含める。解決したresumeは、`integrate`が呼ばれていないrunならもう一度validatingを通り、そこで差分から決め直す。`integration_approved`のあるrun（着地のresume）はvalidatingを通らずに着地へ進む。その解決の判定は前のvalidatingが差分で求めた`e2e`を含むが、`integrate`のreceiptの検査はtaskの要るevidenceだけを見るので、着地のresumeでの衝突の解消が新しく`[e2e] paths`に触れても、そこでは`e2e`を求め直さない（本番を守るのは入れ替え前のe2eの関門）。
- **integrateの検査**: `integrate`のreceiptの検査（[`integrate`](integrate.md)）はtaskの要るevidenceだけを見る。差分で求めた`e2e`はvalidatingが確かめ済みで、resumeで書き直したreceiptもvalidatingを通る。e2eは`integrate`では流さない（ADR-t963-1決定4）。
- **workerに知らせる**: workerのprompt（[Prompt](prompt.md)）に、`[e2e] paths`のglobと、差分がそれに触れたら`e2e`が必須であること、taskの`paths`から読んだ見込みを1行で書く。taskが`e2e`を要るなら`Required evidence:`の行だけ。runのworktreeの`.config/e2e-quarantine.toml`に効いている印があれば、promptとresumeの依頼に印のtestの名前と、落ちたe2eの1回の流し直しと、印で通したときの`e2e`の書き方（`passed`とevidenceに印で通したtestと流し直しの結果）と印を効かせない条件（差分がそのtestを変える、taskがその印の直すtask）を載せる（[ADR-t1165-1](../../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)決定6、task 1167。[Prompt](prompt.md)）。validatingは印を読まない。通常はreceiptの`e2e`が`passed`でevidenceがあれば通り、Codex worker の必須 e2e は次項の evidence 書式も確かめる。
- **Codex worker の除外**（ADR-t963-1決定5）: promptとresumeに、workspace-write sandbox の外にある Podman machine と supervisor の生存確認・signal に依存する e2e の完全な名前・理由・`--skip` を含むコマンドを示す。`src/domain/validation.rs` の `CODEX_WORKER_E2E_EXCLUSIONS` が承認名を持つ。差分による要件は変えず、Codex の必須 e2e の receipt は `e2e.evidence_or_reason` に JSON 文字列 `{"command":"cargo test --locked --test e2e -- --ignored --skip <name>...","result":"passed","excluded":[{"name":"<name>","reason":"<承認理由>"}]}` を入れる。validating はコマンドの `--skip` と除外名・理由の一致を確かめる。全件を流したときは `--skip` 無し、`excluded: []`。印で通した test があれば `marked` に名前、流し直しの結果、印の理由を加える。test 自身の条件で早く戻った場合は任意の `note` にその事実を記す。Claude worker にはこの書式と除外を要求せず、`install` と auto-update の関門は全件を流す。
- **この repository で置く範囲**（task 966が、固定バイナリがtask 965を含むのを確かめてから`dagq.toml`の`[e2e] paths`に理由のコメントつきで書いた。AGENTS.mdの規則もあわせて、runtimeのtaskに一律の`--evidence e2e`を付けない形に改めた）: cmuxのadapter（`src/infrastructure/adapters.rs`）、process（`src/infrastructure/process.rs`・`src/infrastructure/sessions.rs`）、launchd（`src/infrastructure/launchd.rs`）、lifecycle（`src/application/lifecycle.rs`・`src/application/supervise/handoff.rs`）、integrate（`src/application/integrate.rs`）、install（`src/application/install.rs`・`src/infrastructure/binaries.rs`）、update（`src/application/update.rs`・`src/application/supervise/update.rs`・`src/infrastructure/e2e_gate.rs`）、actorの起動（`src/application/actor_executor.rs`・`src/application/session.rs`・`src/application/headless_session.rs`・`src/infrastructure/claude.rs`・`src/infrastructure/codex.rs`）と、e2eそのもの（`tests/e2e.rs`）。`src/application/supervise/`の判断の部分は含めない（ADR-t963-1決定3）。
- **test**: `src/domain/validation.rs`の`a_diff_touching_the_e2e_paths_requires_e2e`・`a_diff_outside_the_e2e_paths_lands_without_e2e`・`a_task_that_requires_e2e_keeps_it_whatever_the_diff`・`without_e2e_paths_the_diff_requires_nothing`、`tests/it/runtime_evidence.rs`の`a_diff_touching_the_e2e_paths_parks_the_run_without_e2e`・`a_diff_outside_the_e2e_paths_lands_without_e2e_unless_the_task_requires_it`。
