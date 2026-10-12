---
id: design-supervisor-lifecycle-validation
type: design
title: "Validation"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0029
  - adr-t963-1
  - adr-t1165-1
  - adr-t1233-2
  - adr-t2125-1
  - adr-t1925-1
  - adr-t1487-1
  - design-supervisor-lifecycle-review-programs
  - adr-t2268-1
  - design-toolchain-profiles
---

# Validation

`validating`のrunに対して、supervisorがセッションと同じleaseの下で、runごとのthreadで次を順に確認する。sessionは開いたまま（[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)の1）のことも、終了済みのこともある。最初に外れた項目が`failed`の理由（`last_error`）になり、以降は確認しない。

1. receiptが存在し、`Receipt`として解釈できる。
2. `run_id`が一致し、`result`が`succeeded`か`already_satisfied`である（`already_satisfied`は`base_checks`も要る。[Receipt and session exit](receipt-and-session-exit.md#baseがすでに満たすときalready_satisfied)）。
   `tests`/`e2e`/`subagent_review`は`failed`でなく、`passed`には証跡、`not_applicable`には理由が空でなく書かれている。
   `commit`は完全なSHAである。
3. worktreeのHEADがrun branch `dagq/<run-id>`を指し、そのcommitがreceiptの`commit`と一致する。
4. commitがbase commitと異なり（commitなしを拒む）、base commitの子孫である。
   `already_satisfied`では逆にcommitがbase commitであることを求め、5と8だけを見て7を読まずに下の「already_satisfiedの検証」へ進む。
5. `git status --porcelain --untracked-files=all`が空である。untracked fileもdirtyとみなす。
6. （欠番）taskの`verification_commands`はここでは実行しない。同じcommitに対する実行は`integrate`のrebase後の1回だけ（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定1。[`integrate`](integrate.md#integrate)のステップ5）なので、validatingの所要時間はreceiptとGitの照合だけで、`verification_command`イベントも`<run-dir>/verify-N.log`も作らない。検証コマンドで壊れるcommitもここでは`awaiting_integration`になり、`integrate`で`needs_session`になって自動resumeされる。
   `already_satisfied`のrunは着地しないので、検証は下の「already_satisfiedの検証」がmainのsnapshotで流す。
7. **宣言パス**（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）: taskの`paths`（`add --paths`）が空でなければ、run branchが今のmainから分かれた点（`GitRepository::merge_base`で`refs/heads/<branch>`とreceiptの`commit`のmerge-base。rebaseしていなければbase commit、resumeしたsessionがrebaseしていればそのmain）からreceiptの`commit`までに変わったパス（他のtaskがmainに着地したパスは数えない。`GitRepository::changed_paths`: `git diff --name-only -z --no-renames`。renameは両側、削除も含む）のうち、どのglobにも合わないもの（`domain::scope::out_of_scope`）を探す。1–5をすべて通ったrunだけをここで見る。見つかればrunを`failed`ではなく`needs_session`にし、`last_error`を`changed paths outside the task's --paths: <paths>`にして、`validation_finished`（`status: needs_session`、`scope_violation`に外れたパスの配列、`allowed_paths`にtaskのglob）の後に`scope_violation`イベント（`paths`、`allowed`、`reason`。`status`は持たない）を記録する。8より先に見るので、両方が欠けていれば先にこちらを直させ、resume後の再validationで8を見る。後の扱い（終了の依頼、close、自動resume）は8と同じで、resumeの依頼は宣言外のパスをrun branchから外す文面になる。`paths`の無いtaskでは7は何もしない。
8. **要求evidence**（ADR-0019の決定5）: taskの`required_evidence`（`add --evidence`）のうちworkerが裏付けるcheck（`domain::required_of`。`e2e`は含めない。Codexのworkerは`subagent_review`も含めない）の各checkについて、receiptのそのcheckの`status`が`passed`で`evidence_or_reason`が空白でないこと（`Receipt::missing_evidence`）。`e2e`はruntimeがreviewのpassの後に流すので（下の[runtimeが流すe2e](#runtimeが流すe2e)）、receiptには求めず、要否だけを決めて記録する。1–5をすべて通ったrunだけをここで見るので、receiptやGitの不整合は従来どおり`failed`になる。要求されたcheckに限り、2の「`failed`でない」と「evidence_or_reasonが空でない」はここに回す（`Receipt::check_requiring`）ので、`failed`・`not_applicable`・空のevidenceはどれも`failed`ではなくここで欠落になる。要求されていないcheckの`failed`は従来どおり2で`failed`になる。欠けていればrunを`failed`ではなく`needs_session`にし、`last_error`を`evidence missing: tests`（複数は`, `区切り、要求の順）にして、`validation_finished`（`status: needs_session`、`evidence_missing`に欠けたcheckの配列）の後に`evidence_missing`イベント（`checks`、`reason`、runがe2eを要るときは`e2e_requirement`。`status`は持たない: `stats`が`validation_finished`の`needs_session`で1回数えるため）を記録する。runのsessionには終了を依頼し、終わったらbackgroundのwrapperの残りを止める（resumeは自分のwrapperをbackgroundで起動する。runはworkspaceを開かない。[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。supervisorはこのrunを[`needs_session`](needs-session.md#needs_session)の手順で自動resumeし、不足しているcheckの実行とreceiptの書き直しを依頼する。要求の無いtaskでは8は何もしない。
9. **Spikeの結果**（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定3）: 実行区分が`spike`のtaskのrunだけ、receiptの`spike_result`がそろっていること（[Receipt and session exit](receipt-and-session-exit.md#spikeの結果)）。
   否定的な結果と未解決は欠けではなく、中身が判定を支えるかは見ない。
   欠けは8と同じく`needs_session`にして自動resumeし、欠けた欄の書き足しを依頼する（codeは`spike_result_missing`）。
   `integrate`も着地の前に読み直したreceiptを同じく検査する。
   実装の区分のrunでは9は何もしない。

containerで動くrunでは、3〜5の事実をcloneでgitを走らせずに得る（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定2〜4、[Container runtime](../container-runtime.md#git)）。
3のbranchと5のstatusはrunの状態のファイル、3〜4と7のheadはrun branchのbundleを制御側のrun branchへ取り込んだcommitで、祖先・差分・pathsは今と同じく制御側のobjectsで判定する。
bundleか状態のファイルが無い・壊れている・symlinkである・前提のcommitが無い、bundleにrun branch以外のrefがある、状態のファイルのHEADがbundleのheadと食い違うときは、`commit_mismatch`でsessionに返す。
検証は前の着地用のworktreeを先に消し、着地は検証が受け入れたcommitで行う。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容、7で外れたときだけ`scope_violation`と`allowed_paths`、8で欠けたときだけ`evidence_missing`、e2eの要否が分かったときは`e2e_requirement`、`receipt_observed`からこの検証までのload averageの`load_avg_mean` / `load_avg_max`（task 197。[`supervise`](supervise.md)の7））と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、着地まで依存taskを解放しない。

その後: `awaiting_integration`のrunは[Review](review.md#review-supervisor)に進む（leaseとsessionはそのまま）。`integration_approved`のあるrun（`integrate`が呼ばれた後にresumeしたrun）はreviewを待たずに終了の依頼→close→着地する（ADR-0027の決定3）。`needs_session`（宣言外のパス、evidenceかSpikeの結果の欠落）は終了の依頼→closeしてleaseを外す。`failed`は終了を依頼し、workspaceは調査のため閉じずにleaseを外す。

reviewの段の順は、先頭の[プログラムのreview](review-programs.md)、その後のagentのreview、passの後の[着地の前のe2e](landing-e2e.md)である。
プログラムのreviewはtestを含まない速い形式の検査で、validatingと同じくtaskのverifyのコマンドを流さない。
着地するrunのtestを含む検証は下の着地の検証（`integrate`のrebase後の1回）だけで、プログラムのreviewとは二重にしない。
着地しない`already_satisfied`のrunは、下の「already_satisfiedの検証」でmainのsnapshotに流す。

## already_satisfiedの検証

[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)の、workerが「baseがすでに受け入れ条件を満たす」と書いたrunの経路。
verifyが通れば人のaskに上げ、人の答えで閉じる。

- **validating**: 受理してもrunは`awaiting_integration`にせず`validating`のまま残し、`result_commit`を持たない（`AfterValidation::Satisfied`）。
  sessionに終了を依頼してworkspaceを閉じる。
- **snapshot**: 検証の時点のmainのheadを1つ決めて`run_satisfied_snapshot`に残し、そのheadでtaskの`verification_commands`を登録のまま全部流す（`integrate::verify_snapshot`）。
  coverageの関門も流し、[着地の検証](#着地の検証)の置き換えはしない。
  流す場所はrun directoryのscratch worktreeで、runのworktreeはreceiptの名指すbaseのままにする。
  rebase・commit・push・e2eは無い。
  結果は同じheadとともに`run_satisfied_verified`に残す（欄の意味は`event_kind`のdoc comment）。
  流す間はslotとleaseを持ち、handoffはこれを待つ。
  verificationのruntimeがcontainerで使えない間は、hostで流さずに検証を始めず、runは受理のまま休み、使えるようになった後のpassが始める（[container runtime](../container-runtime.md#fail-closedの境界)、`satisfied::awaits_verification`）。
- **ask**: verifyの後に`approve_close`のask（理由は`scope`）を開き、questionにsnapshotのheadを載せ、leaseを外してrunを`validating`のまま休ませる。
  verifyが落ちていればoptionsから`close`を外し、questionに落ちたことを書く。
  休む間は開いたask（検証の前ならcontainerの知らせ）がattentionで、supervisorの死んだrunの回収も、人の`recover`と`doctor`も、このrunを取らない（`satisfied::blocks_recovery`、[recover](recover.md)）。
- **答えの適用**（`apply_satisfied_answers`、passごと）: `retry`はrunを`failed`にしてtaskを`ready`に、`cancel`はrunを`failed`にしてtaskを`canceled`にする。
  `close`は今のmainのheadを最後の`run_satisfied_verified`と比べる（`satisfied::close_step`）。
  同じheadで通っていれば、runを`succeeded`（commitなし）、taskを`completed`にし、依存するtaskがclaimの候補になる（`run_satisfied_closed`）。
  進んでいれば古い結果では閉じず、空いたslotでrunをleaseし直して新しいheadをsnapshotにverifyを1回流し直す。
  通ればその間にmainがさらに進んでも閉じ、落ちれば閉じずに答えたaskを閉じて、落ちたことを書いたaskを開き直す。
  人の答えは受け入れ条件の意味の判断なので、通った流し直しの後に聞き直さない。
- **再起動**: snapshotと結果はeventから読むので、askの後にsupervisorが替わっても、答えの前にmainが進んでも同じ比較になる。
  流し直しの途中で死んだrunは、adopterの再validationが開いたaskを見て休ませ、次のpassの答えの適用がもう一度流し直す。
  検証のコマンドは自分のprocess groupで動き、supervisorが死んでも残りうる。
  そこでscratch worktreeを使い直す前に、そこで動く前の検証のprocessを止め（`auto_repaired`の`satisfied_verification_stopped`）、止まらないか一覧が読めなければ検証せず、askに落ちたことを書く。
- **集計**: 閉じたrunは着地（`run_integrated`）に数えず、`stats`の`already_satisfied`で別に数える（[stats](stats.md)）。

## 着地の検証

[ADR-t1925-1](../../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)で、着地の検証を軽くし、最終関門をCIにする。
置き換えと材料の受け渡しはruntimeにあり、repositoryの`dagq.toml`が設定を持つときだけ働く。
validatingは今と同じく検証のコマンドを流さず、置き換えも`integrate`のrebase後の1回の検証の中で起きる（[`integrate`](integrate.md)の手順5）。

- **置き換えの流れ**: 設定があるときだけ、runtimeはtaskの検証のコマンドのうち設定が名指すもの（coverageの関門）を、設定の着地の検証のコマンドに置き換えて流す。
  当たるコマンドがいくつあっても置き換えたコマンドは1回だけ流し、他のコマンドは登録の順のまま流す。
  当たるコマンドを持たないtaskでは何も置き換わらない。
  登録済みのtaskの検証のコマンドは書き換えず、着地のたびにmain checkoutの設定から決め直すので、設定を外せば登録のコマンドに戻る。
  置き換えたコマンドの`verification_command`のeventと検証のlogの先頭に、元のコマンドが残る。
  hostの失敗のやり直しと不安定なtestの着地のやり直し（[`integrate`](integrate.md)）は、置き換えたコマンドにも同じく効く。
- **失敗の読み方**: 置き換えたコマンドも登録のままのコマンドも、失敗の言語・道具に固有の分類と落ちたtestの名前とflakyのやり直しのenvは、repositoryが`dagq.toml`で宣言したtoolchainのprofileが読む（[Toolchain profiles](../toolchain-profiles.md)）。
  容量の不足・signal・時間切れの終了コード・コマンド全体の上限はruntimeの共通の分類が宣言に依らず読むので、hostの失敗のやり直しはどのrepositoryでも効く。
  宣言したprofileがどれも読まない出力は、共通の分類に当たらなければ`unknown`になり、不安定なtestの着地のやり直しは起きない。
- **分担**: 置き換えと、着地の検証のコマンドに渡す材料（base commit、既に落ちているtestの一覧、CIの修正taskのrunか）はruntimeの汎用の仕組みである。
  何を置き換えるか、ITの選び方、対応表の取り方、閾値・共通のファイル・古さの上限の値は、repositoryの設定とscriptが持つ。
  runtimeはrepositoryのtestの構成と対応表の形を知らない。
  材料は置き換えたコマンドにだけenvで渡し、登録のまま流すコマンドには渡さない。
- **コードの入口**: 設定の型・置き換えの判断（`plan`）・envとファイルの名前は`domain::landing_verification`にあり、設定の書式と渡すenvの意味はそこのdoc commentが持つ。
  設定は`infrastructure::run_env`が読み（`Verifier::landing_verification`）、材料は`application::integrate`の`landing_env`が用意する。
- **中身**: このrepositoryの着地の検証は、fmt・clippy・レイヤーの依存の検査・taskに固有の軽い検査・unit test全件・影響範囲で絞ったITで、行カバレッジの関門はCIだけが見る。
- **絞ったITに必ず含めるもの**: 差分で足した・変えたtestのファイルが定めるITと、対応表に無いIT（表の生成の後に他の着地が足したか名前が変わったtest）。
  表は差分の前のmainから作られているので、足したtestと他の着地が足したtestを知らない。
- **ITを全部流す条件**: 絞ったITの見込みの時間が上限を超えるとき、共通のファイルに触れたとき、表が取れないか古すぎるとき。
  値と根拠は[着地のITの絞り込みの測定](../../plans/landing-it-selection.md)の「決めたこと」にある。
- **既に落ちているtest**: [CI watch](ci-watch.md)の一覧のtestは着地の検証から外す。
  CIの修正taskのrunでは、そのfindingのtestを外さない（他のfindingのtestは外す）。
  runtimeはrunのtaskについて読んだ一覧（修正taskのrunではそのfindingの項目を外す対象から除いたもの）を渡し、外す適用は設定のコマンドとscriptが受け持つ。
- **最終関門**: 着地の検証が見逃した壊れはmainのCIが拾い、CIの見張りが修正taskにする。
  mainの前でCIを通す仕組みと着地のrevertは無く、自動更新にもCIの確かめや全testを足さない。
  全部のe2eの関門（下の節と[Auto-update](auto-update.md)）と固定バイナリを前のものに戻す手順は変わらない。

## runtimeが流すe2e

実cmuxを要るe2eは無く、e2eの本文はcmuxを使わない（`up`はinboxを開かない。[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定7、[testの制約](../../development/testing.md#e2e)）。
切り分け: cmuxの要らないe2eは、cmuxの無いhostでも名前で絞って手で流せる（`cargo test --locked --test e2e -- --ignored --exact <名前>`）。
関門（自動更新と`install`の関門。[Auto-update](auto-update.md)の「e2eの関門」。[着地の前のe2e](landing-e2e.md)）は、e2eの前に`cmux ping`を打ち、cmuxが答えれば全部のe2eを流す。
答えないとき（cmuxが無い、socketが拒む）だけ、実cmuxを要るe2e（`application::install::CMUX_E2E`の`--skip`のfilter）を流さず、残りで判定する（[ADR-t2105-1](../../adr/2026-10-08-t2105-1-e2e-gate-skips-cmux-e2e-only-when-cmux-does-not-answer.md)）。
流さなかったtestと理由は`E2eSkip`で関門の結果とeventに残り、`update_installed`の知らせでinboxに届く。
関門が確かめるのはcmuxだけで、podmanの無さで流さないe2eは無い（[ADR-t2125-1](../../adr/2026-10-08-t2125-1-e2e-gate-checks-only-cmux-after-the-broker-removal.md)）。
cmuxが答えないことは`unavailable`の理由にならない。
そのときの後始末（`e2e_gate::clean_up_without_cmux`）はcmuxを呼ばず、関門のdirectoryをqueueのhashとともに残し、次にcmuxが答える関門がそのgroupとworkspaceを閉じて消す。
launchdの`up` / `down`のe2eは、cmuxが答える関門で追加の環境変数なしに本文まで流れ、使い捨てのHOMEとLaunchAgentのlabelで動き、launchdを使えないhostでは落ちる（黙ってskipもpassもしない。[`up` / `down`](up-down.md)）。

[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)の決定2・3が決めたe2eの要否を、validatingが決めて記録する。e2eを流すのはworkerではなく、reviewのpassの後にruntimeがhostで流す工程で（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)。流し方は[着地の前のe2e](landing-e2e.md)）、workerのreceiptは`e2e`を裏付けない（`domain::required_of`が`e2e`を落とす）。そこで落ちたe2e（上限の内に終わり、流し直しても印で通らない）だけがrunを`needs_session`に戻し、上限切れや始められないe2eは変更のせいとせずruntimeが流し直す。

- **設定**: main checkoutの`dagq.toml`の`[e2e]`の`paths`（globの配列。書式は[Run environment](run-environment.md)）。無ければ空で、差分からは何も求めない（taskの`--evidence e2e`だけで決まる）。supervisorはvalidatingを始めるたびとworkerのpromptを作るたびに読み直す（`Verifier::e2e_paths`）。読めなければ警告を出して空として扱う。
- **判定**（`domain::validation::E2eRequirement`、`ReceiptFacts::e2e_requirement`）: taskの`required_evidence`に`e2e`があれば（`ReceiptFacts::with_task_e2e`）`source: task`で要る。差分は見ない（`paths`の無いtaskは7の差分も読まない）。無く、`[e2e] paths`があれば、7と同じ差分（merge-baseからreceiptの`commit`まで、`GitRepository::changed_paths`）を読み、どれかのglobに合うpathがあれば`source: paths`で要る。無ければ要らない。receiptの`e2e`は、どの場合も要らないcheckとして2で検査する（`failed`か空の理由なら`failed`）。workerは`not_applicable`と理由（runtimeが流す）を書く。
- **記録**: `validation_finished`に`e2e_requirement`（`{required, source?, paths?}`。`source`は`task` / `paths`、`paths`は`source: paths`のとき合った差分のpath。要らないときは`{"required": false}`）を載せる。差分を読む前に外れた（1–5で拒んだ）runで、差分が要否を決めるときは載せない。`evidence_missing`にも、要るときは同じ`e2e_requirement`を載せる。着地の前のe2eは最後の`validation_finished`のこれを読む（`domain::run_e2e::due`）。`stats`もこれで`runs[].e2e`と`e2e`の群を作る（[stats](stats.md)）。
- **resume**: resumeが書き直したreceiptの解決の判定（`ResumeWatch::required_evidence`、衝突だけのresumeを飛ばす`resolved_head`）も`e2e`を含めない（`resume_required`）。解決したresumeは、`integrate`が呼ばれていないrunならもう一度validatingを通り、そこで差分から要否を決め直す。`integration_approved`のあるrun（着地のresume）はvalidatingを通らずに着地へ進み、前のvalidatingの要否で、着地の前のe2eを新しいcommitに流す。
- **integrateの検査**: `integrate`のreceiptの検査（[`integrate`](integrate.md)）も`required_of`のcheckだけを見て、`e2e`を求めない。e2eは`integrate`では流さない（ADR-t963-1決定4）。人が`dagq integrate`で手で着地させるrunにはruntimeのe2eが流れないので、e2eを要るrunを手で着地させる前は、run dirの`e2e-*.log`と`run_e2e_finished`を確かめる。
- **workerに知らせる**: workerのprompt（[Prompt](prompt.md)）は、runがe2eを要りうるとき（taskが`e2e`を要るか、`[e2e] paths`がある）に、e2eを自分で流さないこと、要るならruntimeがreviewのpassの後にhostで流し、落ちればsessionに戻すこと、receiptの`e2e`は理由つきの`not_applicable`にすることを1行で書く（`prompt::e2e_line`）。e2eのコマンド・`[e2e] paths`の一覧・関門の印・Codexのworkerの除外は載せない（ADR-t963-1決定5の除外はADR-t1233-2決定6でなくなった）。`Required evidence:`の行にも`e2e`は出ない。
- **この repository で置く範囲**（task 966が、固定バイナリがtask 965を含むのを確かめてから`dagq.toml`の`[e2e] paths`に理由のコメントつきで書いた）: adapter（`src/infrastructure/adapters.rs`）、process（`src/infrastructure/process.rs`・`src/infrastructure/sessions.rs`）、launchd（`src/infrastructure/launchd.rs`）、lifecycle（`src/application/lifecycle.rs`・`src/application/supervise/handoff.rs`・`handoff_slots.rs`）、integrate（`src/application/integrate.rs`）、install（`src/application/install.rs`・`src/infrastructure/binaries.rs`）、update（`src/application/update.rs`・`src/application/supervise/update.rs`・`src/infrastructure/e2e_gate.rs`）、actorの起動（`src/application/actor_executor.rs`・`src/application/session.rs`・`src/application/headless_session.rs`・`src/infrastructure/claude.rs`・`src/infrastructure/codex.rs`）と、e2eそのもの（`tests/e2e.rs`と`tests/e2e/`の下）。
  `src/application/supervise/`の判断の部分は含めない（ADR-t963-1決定3）。
- **test**: `src/domain/validation.rs`の`a_diff_touching_the_e2e_paths_needs_the_e2e_but_no_evidence`・`a_diff_outside_the_e2e_paths_lands_without_e2e`・`a_task_that_requires_e2e_keeps_it_whatever_the_diff`・`without_e2e_paths_the_diff_requires_nothing`、`src/application/integrate.rs`の`check_receipt_records_the_e2e_a_diff_touching_the_e2e_paths_needs`、`tests/it/runtime_evidence.rs`の`the_e2e_a_run_needs_is_recorded_and_its_receipt_backs_none`。

