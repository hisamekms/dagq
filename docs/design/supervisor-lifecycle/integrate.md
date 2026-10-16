---
id: design-supervisor-lifecycle-integrate
type: design
title: "`integrate`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t728-2
  - adr-t639-1
  - adr-t768-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-landing-branch
  - adr-0008
  - adr-0067
  - adr-0013
  - adr-0017
  - adr-0029
  - adr-0040
  - adr-0068
  - adr-t1925-1
  - adr-t2268-1
  - design-toolchain-profiles
---

# `integrate`

`dagq integrate ID`（taskの`awaiting_integration`または`needs_session`のrun）と`dagq integrate --next`（`awaiting_integration`のrunを検証完了の古い順に1件）は、検証済みのrunをruntimeが`main`へ着地させる操作で、repository内で実行する（[ADR-0008](../../adr/0008-merge-queue-squash-landing.md)）。
reviewがpassしたrunはsupervisorが同じ`Integrator`に依頼して着地させるので、人が呼ぶのは`review_failed`のrunと、review以前のsupervisorが残した`awaiting_integration`のrunだけ（[Review](review.md#review-supervisor)）。
repositoryは`GitRepository::inspect`で開き、common directoryが`queue_repository.git_common_dir`と一致することを要求する。

ユースケースの本体はapplication層の`src/application/integrate.rs`にある（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)の方針1と8）。
着地とpushはこのmoduleの`Integrator`だけが行い、supervisorと人の`integrate`は着地の依頼を渡す（[ADR-t728-2](../../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)。依頼者の検査・lease・承認かreviewのpassの確認と、eventの依頼者とIntegratorの記録は[Authorization](../authorization.md#着地とpushintegrator)）。
着地とpushの処理は承認の段（束縛の確認、runの選択、supervisorのleaseの確認、人が打つ`integrate`の空き容量の確認（[空き容量](disk-space.md#人が打つintegrate)）、`integration_approved`、tokenの生成）と着地の段（下の2〜10）に分かれ、どちらもportの束の`Integration`だけを受け取り、queueとGitのinfrastructureの型とrusqliteを参照しない。
portは`src/application/ports/`にあり、infrastructureの型が実装する。
2つの段はmoduleの外から呼べず、`Integrator`の承認のmethod（承認の段を呼ぶ）と着地のmethod（検査の後に着地の段を呼ぶ）が入口になる。
`runtime::integrate`はqueueとrepositoryを開き、portの実装を組み立てて、このプロセスのactor（userかinbox）を依頼者に承認のmethodを呼び、leaseのheartbeat threadを立ててから着地のmethodを呼ぶ入口だけで、supervisorの着地threadも自分のtokenでslotを取ったrunの着地の依頼（依頼者は`supervisor:<pid>`）を着地のmethodに渡す。
validationの`check_receipt`（[Validation](validation.md#validation)）も同じファイルにあり、Gitとreceiptをそれぞれのport越しに読む。
run directoryの検証logの一覧も`RunFiles::read_dir`越しで、applicationはファイルI/Oを直接しない。
副作用の無い判断は関数に分かれ、同じファイルのunit testが確かめ、`tests/it/runtime_integrate.rs`はSQLite・Git・supervisorの配線の境界を確かめる。

1. **スロット**: runを`integrating`にし、このプロセスのtokenで`run_leases`の行を作る。
   同時に`integrating`のrunは1件で、別のrunが着地中ならerror。
   leaseは`supervise`と同じthreadでheartbeatし、`status`/`doctor`に`integrating`のrunとして並ぶ。
   `--next`の順序は`validation_finished`イベントのid順で、`needs_session`のrunは取らない。
   スロットを取ったら、まず**着地済みか**を確かめる: runの`base_commit`..main headのfirst-parentの履歴に、trailer `Dagq-Run: <run id>`を持つcommitがあれば、前の着地がmainを進めた後、完了を記録する前に止まった（supervisorの死など）ので、rebase・検証・commitをせずにそのcommitを着地commitとして`auto_repaired`を記録し、7〜10（完了・後始末・push・follow_upsの登録）へ進む。
   同じrunを二重に着地させない。
2. **worktreeの前処理**: worktreeが存在し、run branch `dagq/<run-id>`をcheckoutしていること。worktreeのpathはqueueの今の`runs/`から解決したもので、queueディレクトリを移した後でも見つかる。repository側の管理情報が旧pathを指したままだと後始末の`worktree remove`が失敗するので、主working treeで`git worktree repair <worktree>`を実行する（移していなければno-op。[ADR-0017](../../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。途中のrebase（`rebase-merge` / `rebase-apply`）が残っていれば`git rebase --abort`する（`integration_rebase_aborted`）。
3. **receiptの検査**: `<run-dir>/receipt.json`がparseでき、`result`が`succeeded`で（`failed`か`already_satisfied`ならrunを`failed`にして終わる。下記と[Validation](validation.md#already_satisfiedの検証)）、`Receipt::check`を通り、`commit`がworktreeの現在のHEADに一致する。
   ただし、このrunの前の着地のrebaseと振り直しの記録を古い順にたどってreceiptの`commit`から今のHEADへ至るなら、前の着地がrebaseの後に止まって残したheadなので一致とみなす（`RunHistory::landing_rewrote`）。
   衝突なしのrunではHEAD = `result_commit`なので検証済みのreceiptがそのまま通る。
   `needs_session`から戻るrunでは、セッションが新しいheadでreceiptを書き直したことの検出になる。
   worktreeはcleanであること。
   `Receipt::check`の後、taskの`required_evidence`かSpikeの結果が欠けていれば（validationの8・9と同じ判定。resumeしたsessionが欠けたまま書き直した場合）着地せず`needs_session`にし、`integration_deferred`のpayloadに欠けたものを書く。
   これらを通った時点で、読んだreceiptの全体を`integration_receipt`イベントに記録する。
   HEAD不一致・衝突・再検証の失敗で着地しなかった場合も記録は残り、`integrate`を繰り返せばその回数だけ並ぶ。
   `validation_finished`は検証時のreceiptしか持たないので、セッションが書き直したreceiptをDBが持つのはこのイベントだけ。
4. **rebase**: `git rebase --no-autostash --no-verify <main head>`（main headは着地開始時に読んだ`refs/heads/<branch>`）。
   すでにmainの上にあればno-op。
   衝突したら`git diff --name-only --diff-filter=U`とGitの出力を取り、`rebase --abort`でworktreeを検証済みheadに戻して`needs_session`にする。
   成功したら`integration_rebased`を記録する。
5. **再検証と検証コマンド**: rebase後のHEADがmain headと異なり（同じなら「commitが残らない」として`needs_session`。変更が不要ならセッションが`failed` receiptを書く）、main headの子孫であること。
   `git status --porcelain --untracked-files=all`が空であること。
   runが足したmigration（main head..rebase後のHEADで追加された`migrations/NNNN_<name>.sql`）の番号を、rebase後のHEADの`migrations/`でrunが足していない別のファイル（mainから来たもの）が使っていれば、振り直す（runが既存のmigrationを改名・置き換えただけなら重ならない）（[ADR-0067](../../adr/0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)の決定3。振り直すかどうかと理由は`application::integrate::plan_renumber`がGitのport越しに読んで決め、振り直しの関数がその計画どおりにcommitして記録する）。
   runが足したmigrationが1つで、その番号の4桁をrunが変えた他のファイル（main head..HEADの変更パスからmigration自身を除いたもの。`git grep -F`）のどれも含まなければ、worktreeで`git mv`して、runが足していないmigrationの最大の番号+1へ移し、run branchにcommitして`migration_renumbered`を記録し、以降はそのcommitをrebase後のHEADとして扱う。
   Gitがそのcommitを拒んだとき（`commit.gpgsign`の署名の失敗、user identityが無い、`--no-verify`でも走る`prepare-commit-msg`のhookの失敗など。時間切れで終わらなかったcommitも同じ）は、`git reset --hard HEAD`で`git mv`を戻し（振り直しの前にworktreeはcleanなので、戻すのはrenameだけで、HEADはrebase後のHEADのまま）、`migration_renumbered`は記録せず、下の振り直せないときと同じ`migration_number_taken`の`needs_session`にする（`Repository::rename_and_commit`が`Ok(Err(<gitの出力>))`を返す）。
   payloadと`last_error`にGitが拒んだ理由を書くので、sessionはresumeで自分で振り直せる。
   戻す操作自体が失敗したときはintegrateのエラーのままで、そのメッセージにcommitの失敗と戻せなかったことを書く。
   runが足したmigrationが2つ以上か、番号を含むファイルがあれば、何も書き換えず検証コマンドも実行せずに`needs_session`にし、`integration_deferred`の`code`を`migration_number_taken`にする（`last_error`は理由と次の空き番号と直し方）。
   taskの`paths`が空でなければ、squashしてmainに載せる範囲（main head..rebase後のHEAD）の変更パスがすべてどれかのglobに合うこと（validationの7と同じ判定。validationの後にsessionが足したcommitやrebaseで変わった差分もここで止まる。[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の決定4）。
   外れれば検証コマンドを実行せずmainも動かさず、`last_error`に外れたパスを書いて`needs_session`にし、`integration_deferred`のpayloadに外れたパスとtaskのglobを書く。
   taskの`verification_commands`を順に`/bin/sh -c`でworktree内で、[Run environment](run-environment.md#run-environment)の`[run.env]`をenvに足して実行し、出力を`<run-dir>/integrate-<attempt>-verify-N.log`、終了コードと末尾を`verification_command`イベントに残す。
   `attempt`はrun directoryにある`integrate-*-verify-*.log`の最大の試行番号+1（無ければ1）で、同じrunの2回目の`integrate`が1回目のlogを上書きしない。
   非0で終わったコマンドは、終了コード・signal・logから失敗を分類し（`domain::verify_failure::classify`）、`verification_command`に記録する。
   `class`の印は、言語に依らない印をruntimeの共通の分類が、言語・道具に固有の印をrepositoryが`dagq.toml`で宣言したtoolchainのprofileが、projectに固有の印を`dagq.toml`の`[verify_marks.<name>]`が持ち、出どころ・宣言の無いときの扱いは[Toolchain profiles](../toolchain-profiles.md)、当てる順とその理由（hostの原因を先に見る、流し直しで通ったtestを時間切れより先に見る）は`classify`のdoc commentが持つ。
   コマンド全体の上限（`VERIFICATION_TIMEOUT`）を過ぎたコマンドは、止めてから着地処理のエラーにせず、`timeout`の失敗として同じく記録する（verifierは時間切れを表すerrorを返し、integrateがそれを見分ける）。
   検証コマンドは自分のprocess groupで起動し、上限を過ぎるとgroup全体と子孫が移った別のprocess groupを止め、processが残っていないことを確かめてから時間切れとして返す（手順と、同じworktreeでのやり直しと競合させない理由は`infrastructure::adapters::run_shell_to_log`のdoc comment）。
   親が先に終わってから自分でprocess groupを分けた子孫は辿れず止まらない。
   logはUTF-8として不正なbyteを置き換えて読む（killやディスク満杯で途中で切れたlogでも印を拾うため）。
   分類による扱いは下の「hostの失敗のやり直し」。
   `class`が`test_failure`・`timeout`・`flaky`なら、logから落ちたtestの名前を読み（`domain::verify_failure::failed_tests`。宣言したprofileが落ちたと読むtestと、projectの印の行が名指すtestを、重ねず出た順に上限まで）、`verification_command`に書く。
   あわせて、落ちたtestのうち、宣言したprofileが道具の流し直しで通ったと読むもの（`domain::verify_failure::flaky_tests`）を書く（`flaky`でない失敗でも、同じコマンドの中の不安定なtestには付く）。
   コードの分類（hostの分類でないもの）で非0なら`needs_session`で（`flaky`は下の着地のやり直しを先に1回使う）、`integration_deferred`のpayloadにも同じ失敗と落ちたtestを書き、`last_error`に分類と当たった行を足す。
   supervisorが自動resumeする。
   hostの分類（`disk_full`・`killed`・`timeout`。`FailureClass::is_environmental`）の失敗は、次の「hostの失敗のやり直し」に従う。
   validatingは検証コマンドを実行しないので、これがそのcommitに対する唯一の実行で（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定1）、rebaseがno-opでも、headがrunの`result_commit`のままでも必ず実行する。
   `integration_verification_skipped`イベントは記録しない（kindは過去のeventを読むために残す）。
   verificationがcontainerなら、使えなければ検証コマンドの前に止め（[Container runtime](../container-runtime.md#fail-closedの境界)）、使えればコマンドを1本ずつrebase後のcommitのcloneのcontainerで流し、記録と分類とやり直しはこの手順のままにする（[Container runtime](../container-runtime.md#verificationの実行)）。
6. **着地**: `git commit-tree <HEAD>^{tree} -p <main head>`で1 commitを作る。messageはtaskのtitle、receiptの`summary`（空なら省略）、trailer `Dagq-Task: <task id>` / `Dagq-Run: <run id>`。`refs/dagq/runs/<run-id>`をrebase後のHEADに向けてから、mainをcheckoutしているworktree（`git worktree list --porcelain`）があればそこで`git merge --ff-only <commit>`、なければ`git update-ref refs/heads/<branch> <commit> <main head>`でmainを進める。
7. **完了**: 1トランザクションでrunを`integrated`、`result_commit`を着地commit、`last_error`をnull、Taskを`completed`にし、lease行を消して`run_integrated`、`lease_released`、`task_status_changed`を記録する（`finish_integration`）。
8. **後始末**: そのrunのcloseされていない`approve_landing`のaskを閉じる。
   supervisorの着地でも手の`integrate`でも同じ着地の段が行い、未回答なら`ask_answered`、回答済みなら`ask_closed`を記録する。
   mainはもう進んでいるので、閉じられなくてもwarnのlogだけで着地は変わらない。
   同じ後始末が、そのrunか、runを名指さないそのtaskの`blocked`のask（observerが開いたもの）も同じく閉じる（着地した後に残ると`ask_unanswered`のalertを出し続けるため）。
   `ask_answered` / `ask_closed`はaskの`ask_opened`と同じrun・taskに記録する（task だけのaskはrunを持たない）。
   askに紐づくfindingは変えない。
   続いて`git worktree remove --force <worktree>`と`git branch -D dagq/<run-id>`（`worktree_removed`）。
   失敗は`cleanup_failed`イベントと`last_error`に残し、statusは変えない。
   同じtaskの他のrunの`approve_landing`も閉じる（[Review](review.md#review-supervisor)の6）。
9. **push**（ADR-0019の決定3、ADR-t615-1）: 着地したbranchを`dagq.toml`の`[repository] remote`の同じ名前のbranchへpushする。
   `push = false`ならremoteを見ずにpushしない。
   Gitはapplication層のremoteのportを通して呼び（pushの操作はIntegratorだけが作れる許可を取る）、pushの先・結果・eventは`decide_push`が決める。
   成功は`push_finished`、`--no-push`・`push = false`・`remote`を書かずに`origin`が無いときは`push_skipped`になる。
   Gitのpushが失敗しても、`MainRemote::contains_landed_commit`でremoteのbranch先端を照会し、着地したcommitがその祖先（先端自身を含む）なら、すでに届いているとして`push_finished`を記録する。
   照会が失敗するか、remoteが着地commitを含まないとき、および明示した`remote`が無い・`[repository]`が読めないときは`push_failed`を着地したrunに記録する。
   どの場合もrunは`integrated`、taskは`completed`のままで、`integrate`は成功（exit 0）で終わる。
   `integrate`の間のrunの工程の記録は[計測](../measurement.md#区間とタグ)が持つ。
   `push_failed`はattention（`push main`）になり、人がinboxから原因を直して`git push <remote> <branch>`を打つ。
10. **follow_upsの登録**（ADR-0019の決定4）: 着地したreceipt（ステップ3で読んだもの）の`follow_ups`の未登録項目を、1つの`BEGIN IMMEDIATE`トランザクションでまとめてdraft taskとして登録する（`application::integrate::register_follow_ups`）。
    titleとdescriptionはfollow_upのもの、acceptance・`verification_commands`・dependenciesは空、goalは登録のtransactionで読み直した元のtaskと同じ（無いか閉じていればgoalなし）にする。
    登録ごとに着地したrunへ`follow_up_registered`を記録する。
    workerの所属の提案は項目に書いたまま（形も値も検査しない。`domain::follow_up_membership_proposal`、[follow_upsの所属の提案](receipt-and-session-exit.md#follow_upsの所属の提案)）記録する。
    種類の`category`は項目の`category`をworkerが書いたまま（前後の空白は落とし、一覧に無い値もそのまま）、空でない文字列でなければ`unlabeled`にする（`domain::follow_up_category`、[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)、一覧は[follow_upsの分類コード](receipt-and-session-exit.md#follow_upsの分類コード)）。
    goalが閉じていればgoalなしで登録する。
    同じrunの`follow_up_registered`が既に持つ配列内の位置は登録しないので、同じrunについて2回呼んでも二重に登録しない。
    `title`が空でない文字列でないか`description`が文字列でない項目は登録せず、そのことを`follow_up_registered`に残す。
    各draftのtask作成・深さ・出どころと、登録した項目・skippedの項目の`follow_up_registered`を同じトランザクションで書く。
    途中で書き込みに失敗したらその呼び出しの未登録項目は全件rollbackされ、次の`integrate`のやり直しでまとめて登録できる。
    supervisorのpassには、同じreceiptの新しいdraftが全件見えるか1件も見えないかのどちらかになる。
    登録の失敗はstderrに出すだけで、着地とexit codeは変えない。
    draftのfollow_upの深さは元のtaskの値+1にし、出どころをfollow_upとして記録する。
    draftなのでsupervisorのclaimは拾わず、補ってsubmitするか、cancelするか、人に聞くかは、supervisorが同じrunのfollow_upの束ごとに立てるplannerが決める（plannerのpromptはdraftの`category`と所属の提案を見せる）（[Draft planners (supervisor)](draft-planners.md#draft-planners-supervisor)）。
    元goalと登録時の状態は登録のtransactionで読み、材料と登録eventに残す。
    所属を移しても変えず、登録時に元goalが無い・閉じていたfollow_upは人のadoptなしでruntimeのplannerがsubmitできない（[所属の判断](../follow-up-membership.md)）。
    `follow_up_registered`はattentionにしない。

**着地の検証の置き換え**（[ADR-t1925-1](../../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)）: 手順5は、`dagq.toml`が着地の検証を持てば、taskの検証のコマンドのうちそれが名指すもの（coverageの関門）を1つのそのコマンドに置き換え、他は登録のまま流す（`domain::landing_verification::plan`。登録は変えない）。
置き換えたコマンドにだけ、base commit（載せたmain）、runのtaskの既に落ちているtestの一覧のファイル、修正taskのrunかをenvで渡す（`landing_env`）。
その`verification_command`のeventとlogの先頭に元のコマンドが残る。
下のやり直しも同じく効く。
選び方と値はrepositoryの設定とscript（根拠は[測定](../../plans/landing-it-selection.md)）、流れと境界は[Validation](validation.md#着地の検証)にある。

**hostの失敗のやり直し**（[ADR-t639-1](../../adr/2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)）: 検証コマンドの失敗の`class`が`disk_full`・`killed`・`timeout`なら、コードではなくhostの原因として、workerのsessionをresumeせず、同じ試行（同じ`attempt`）の中でそのコマンドを1回だけやり直す。

- やり直しのlogは`<run-dir>/integrate-<attempt>-verify-N-retry.log`で、最初の失敗のlogを上書きしない
- やり直しも`verification_command`として記録する。
  落ちたtestの名前はやり直しにも同じ規則で付く
- `disk_full`なら、やり直す前に空き容量を着地の閾値（[空き容量](disk-space.md)の`DiskConfig::needs`の着地の側）でそのときに読んで確かめ、足りなければやり直さずに下のholdにする。
  閾値が決まらない・空きが読めないときは確かめずにやり直す
- やり直しが通れば次のコマンドへ進み、全部通れば着地する
- やり直しがコードの分類で落ちたら、そのやり直しの結果で上と同じ`verification_failed`の`needs_session`にする（`last_error`はやり直しのlogを指す）
- やり直しもhostの分類で落ちたら（と`disk_full`で容量が足りないとき）、runをresumeせずに`awaiting_integration`に戻す（`hold_integration`）。
  `integration_held`イベント（code `verification_environment`）と`lease_released`を記録してleaseとslotを返す。
  `last_error`はhostで落ちたコマンドとlogとhostを直す手がかりを示し、`dagq integrate <task>`で着地させるよう書き、sessionをresumeしないことを書く。
  `needs_session`にしないのでresumeの回数（`resume_started`）は使わない
- holdしたrunはleaseの無い`awaiting_integration`なので、inboxのattentionに1件出る（`domain::run_attention`）。
  人がhostを直して（空き容量を空ける、負荷を下げる）`dagq integrate <task>`を打てば、新しい試行で検証からやり直す。
  supervisorはleaseの無い`awaiting_integration`のrunを自分では着地させない（ただし着地の後のlanding recheckはholdしたrunも確かめ、衝突かコードの分類の失敗なら`needs_session`、hostの分類の失敗なら控えたまま。[Landing recheck](landing-recheck.md)）
- 分類の集合（`disk_full`・`killed`・`timeout`）は人の判断なしに広げない（ADR-t639-1）

**言語・道具の失敗の印**: Rustのprofileが読む印の一覧とnextestの失敗のstatusの集合は、その定義のそばのdoc comment（`domain::toolchain::rust`）が持つ。
同じstatusの集合を`scripts/stress-recent-tests.sh`・`scripts/slow-tests.sh`・`.github/workflows/ci.yml`のLinuxのjobも持ち、nextestの版を上げて形が変わったらそろえて直す（その約束もdoc commentにある）。

**不安定なtestの着地のやり直し**（[ADR-t768-1](../../adr/2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)）: `.config/nextest.toml`の`[profile.default]`で、nextestは落ちたtestを1回だけ流し直し、流し直して通ったtestもFLAKYと出して失敗のままにし、最初の失敗で止まらず全部のtestを流しきる（設定と理由はそのファイルのコメント）。
nextestの設定に置くので`integrate`だけでなく、CI・人の手元・workerのstressにも同じく効く。
`cargo test`（nextestでないもの）の検証には効かず、`flaky`にもならない。
流しきるので、`flaky`の判定（落ちたtestが全て流し直しで通った）はその回の全部のtestの結果に対して行われ、本当の失敗を見落として着地のやり直しに回すことはない。

- 検証コマンドの失敗の`class`が`flaky`（落ちたtestが全て流し直しで通ったと読め、名前が上限を超えて切れていない）で、この着地の試行でまだflakyのやり直しをしていなければ、`needs_session`にせず`integration_retried`（code `verification_flaky`）を記録し、同じ着地の中で検証をもう1回やり直す。
  receiptの照合とrebaseはやり直さない（rebaseがrun branchをreceiptの`commit`の先へ動かしているので、照合し直すと`commit_mismatch`になる）。
  runは`integrating`のまま、同じslotとleaseで、rebase後の同じheadに対して続けるので、supervisorの着地でも人の`integrate`でも同じに動き、resumeの回数（`resume_started`）もsessionも使わない。
  main headは着地の開始時のまま（slotを持つ間はmainは動かない）
- やり直しの検証は新しいlog番号（`attempt`が1つ進み、logは`integrate-<attempt+1>-verify-N.log`）で、検証コマンドを最初から全部流す（`land`の検証のloop）
- やり直しは着地の試行（1回の`integrate`のrebase後のhead）ごとに1回まで（[ADR-t1039-1](../../adr/2026-09-30-t1039-1-retry-flaky-verification-per-landing-and-record-passing-flakes.md)）。
  やり直しの検証コマンド全てに、flakyと読んだprofileのやり直しのenvを`[run.env]`の同名の値より優先して渡す（`ToolchainProfile::flaky_retry_env`）。
  1回目の検証のコマンド（integrateと着地のrecheck、`run_shell_to_log`）は、組み込みのprofileのやり直しのenvの変数をプロセスが受け継いでいても外し、runtimeが渡したものだけを渡す（Rustのprofileの変数と外す理由はそのdoc comment）。
  やり直しはFLAKYだけなら成功し、coverageを含む全コマンドが通れば着地する。
  resume・復旧後の新しい着地でもまた1回やり直せる。
  過去の`integration_retried`は制限に使わない。
  logの`attempt`はやり直しにも新しい番号を使う
- 成功した`verification_command`にもFLAKYの名前を残す。
  成功とFLAKYの名前、その後の`run_integrated`で、FLAKYを記録して着地したことが読める。
  1回目とやり直しで別のtestがFLAKYでも両方をstatsに数える
- やり直しが`test_failure`・`build_error`などで落ちたら、`verification_failed`の`needs_session`にする。
  hostの分類は上のhostの失敗のやり直しに従う。
  1回目からflaky以外のコードの失敗なら直ちに`needs_session`にする
- `integration_retried`はattentionにしない（人の判断は要らない）。
  inboxとobserverは`integration_retried`のeventと、[`stats`](commands.md)の検証の失敗と落ちたtestのflakyの数と候補で読む

結果は`IntegrationOutcome`（各outcomeの中身はそのdoc comment）で、`integrated`、`needs_session`、`failed`、`held`（上の「hostの失敗のやり直し」のhold）、`--next`で対象がなければ`no_run_awaiting`のどれかになる。

supervisorが着地させたrunが`integrated`で終わるたびに、着地待ちのrunをその後のmainに対して`git merge-tree`と`[recheck] command`で確かめ、着地しなくなったrunは次の`integrate`を待たずに`needs_session`にする（[Landing recheck](landing-recheck.md)、[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)）。人が打った`integrate`の着地の後には走らない。

上の手順5のmigrationの番号の振り直しは、queueのrepositoryがdagqのソースのときだけ動く（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。
ソースでないrepositoryでは番号を見ず、`migrations/`の変更を他のファイルと同じに扱う。
判定と対象の一覧は[Source repository](source-repository.md)にある。
`plan_renumber`（`src/application/integrate.rs`）は、Gitのportの判定でrepositoryがdagqのソースでなければ振り直さないと決め、`migration_renumbered`も`migration_number_taken`も記録しない。

手順4・6の`refs/heads/<branch>`と手順9の`origin`は、`dagq.toml`の`[repository]`で指定でき、指定が無ければ推定するbranchとremoteに変わる（[ADR-t615-1](../../adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)）。
`push = false`は`push_skipped`、明示したremoteが無いのは`push_failed`になる。
解決の規則は[Landing branch](landing-branch.md)にある。
`refs/heads/<branch>`は着地の開始時に1度だけ解決した着地先を指し（手順4〜9で同じbranchを使い、着地するcommitが`[repository] branch`を書き換えても、そのpushは開始時のbranchへ行く）、手順9のremoteは`[repository] remote`を指す。

containerで動くrunの手順の`worktree`は制御側だけの着地用のworktreeで、作れないかgitのhookとdriverを止められなければ`held`になる（[Container runtime](../container-runtime.md#git)）。
