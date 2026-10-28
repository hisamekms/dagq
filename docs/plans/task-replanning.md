---
id: plan-task-replanning
type: plan
title: 長期化した非対話taskの診断・保留・成果保存・置換（一部実装）
status: active
created: 2026-10-05
related:
  - plan-task-hold
  - adr-t1521-1
  - adr-t1521-2
  - adr-t1487-1
  - adr-t1394-1
  - adr-t451-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-goal-review
---

# 長期化した非対話taskの再計画（一部実装）

**下の「実装の状態」に挙げたもののほかは予定の契約であり未実装。**
[ADR-t1521-1](../adr/2026-10-05-t1521-1-diagnose-before-replanning-and-preserve-acceptance.md)と[ADR-t1521-2](../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)を実装する際の正本。
現在のrevise・resume・requestは関連文書が持つ。
この文書を追加しても#1405のrunを停止・編集しない。
wrapperはtask 1440が統一するbackgroundの経路を前提にし、cmux workspaceのcloseで保留や復元を代用しない。
Claude/Codexで同じ状態機械を使う。

## 実装の状態

- 実装済み: 操作の永続（`src/infrastructure/replans.rs`）、状態と遷移・版・保留の判定（`src/domain/task_replan.rs`）、入口のCLI（人とinboxの`replan request`、workerの自分のrunだけの`replan propose`、読む`replans`）と権限、`status`・`doctor`・`list`・`show`での表示。
- 保留の判定（`check_step`）は、操作が保留の柵より後の状態にある間、claimと候補・resume・復旧の回・revise・`ready`・`edit`・`cancel`・依存の変更・着地を、どのroleにも拒む。
  各経路はその工程のtransactionの中で同じ行を読む。
- 古いruntimeを止める柵: 済み（非互換のschema。floorが再計画の表を読まないバイナリを拒む）。
- 保留とsnapshot・復元: 済み（`src/application/supervise/replan_park.rs`、`src/domain/replan_snapshot.rs`）。
  柵の後はturnを送らず（送信が拒む）、wrapperも次の依頼を取らずturnの間で終わる（`turn_withheld`）。
  走っているjobは有界な終了を待ち、そのverdictと柵の後の受理（`replan_result_withheld`）でrevise・ask・着地を始めない。
  wrapperは各turnの開始の前（最初のturnと新sessionでのやり直しを含む）にも柵を見る。
  柵を読めないあいだはturnを始めず（柵の前の依頼も取らない）、`turn_withheld`に読めない理由を残して待つ。
  turnの上限と猶予を過ぎた書き手は、runが記録したhandleとturnだけを止める。
  その後もturnがprocess groupの外に残したprocessがworktreeで猶予を過ぎて動いていれば、保存せず`blocked`にする。
  snapshotは`replans/`の作業dirで作り、manifestと読み戻しで照合してrenameで確定し、headを`refs/dagq/replans/<操作>`で固定する。
  snapshotはrun dirのlogとturns/の依頼・turnの出力も全体を保存し、linkや特殊なfileは辿らずmanifestに名を残す。
  1つのfileか全体の上限を超えるものは切らずに保存の失敗（`blocked`）とする。
  `parked`とsnapshotの参照、runの`needs_session`（`replan_parked`）とleaseの返却は一つのtransactionで確定する。
  slotを持たないrun（人の答え・resume・triageを待つrun）も同じく`needs_session`にし、開いた`approve_landing`のaskを同じtransactionで閉じるので、前の承認やreceiptで復元を飛ばして進まない。
  保留より前の着地の承認（`integration_approved`）とreviewのpassは、継続の後の成果の着地の根拠にしない（新しいvalidationとreviewを通す）。
  保存の失敗は`blocked`で、worktreeに手を付けずslotは返す。
  保留も一つのsupervisorが止めるところから確定まで持つ。
  slotを持たないrunはその前にleaseを取り（ほかのsupervisorが持つ間と、保留が確定した後は取れない）、sessionを止めるか閉じる前ごと、headの固定とsnapshotの確定の前に、leaseと操作の位置が変わっていないことを確かめる。
  そのため、止まっている間に引き継がれたsupervisorは、戻っても新しいsessionを止めない。
  柵が来た時にturnが走らせていなかった回答の依頼（待っていたもの、wrapperが取った直後に柵に会ったもの、resumeしたthreadが無く何も走らなかったturnのもの）は`replan_parked`に残し、継続のresumeの依頼に載せる。
  保留の後にその依頼を走らせたturnがあるまで回答は未消費で、その前にもう一度保留されたときは前の保留の回答を次の保留にも残す。
  reviewの走っているjobとprogramは終わりを待つが、待っているjob（同時の上限・providerの待ち・もう一度の実行）と次のprogram・agentのreviewは柵の後に始めず、reviewを保留して（`replan_result_withheld`）runを保留する。
  reviewを始める側（validationの後、adopt、exec引き継ぎの後の組み直し）も、保護のあいだはprogramもjobも始めず、reviewを待たせたまま保留に渡す。
  継続のresumeの依頼に載せる回答は依頼の中の上限までで、超えた分も含めた全体を`resume-<回>-answers.txt`に書き、依頼がそのfileを名指す。
  保護のあいだ柵に拒まれた回答の配送は失敗とせず、askを開いたまま残して、継続のresumeのsessionが届ける。
  保護中のrunのreviseと着地の順も保留の経路で扱い、slotで待たずaskにも回さない（暫定の扱いを置き換えた）。
  `already_satisfied`の主張のrunも同じく保留する。
  走っている検証は終わりを待つが、その結果では`approve_close`を開かずtaskも閉じない（`replan_result_withheld`）。
  答えを待って休むrunも保留し、開いた`approve_close`を同じtransactionで閉じる。
  closeと再検証のleaseのtransactionは保護のあいだ拒む。
  各段は永続の状態から再開する（再起動・adopt・exec引き継ぎ）。
  untrackedは実行権限も保存・復元・照合する。
  保護のあいだはresumeを使い切ったrunも`failed`にせず、復旧jobにも継承retryにも渡さない。
  継続・撤回で閉じた操作のrunは、resumeの前（使い切ったrunは復旧jobか継承retryに渡す前）にworktreeをsnapshotと照合し、無ければ作り直す（`replan_restored`）。
  照合と作り直しはsnapshotを保存した保留のrunに限り、保留を終えた柵がslotに見つけてsnapshotを作らず休ませたrun（`rest_fenced`）は照合も作り直しもせず、worktreeをそのままresumeする。
  作り直しは隣のscratch worktreeで組み立て（差分はgitの標準入力へ渡す）、snapshotと照合してから元のpathへ移し、途中で止まったscratchは次の試みが捨ててやり直す。
  作り直しはmanifestが記録したheadから始め、固定したrefは到達を保つだけにする。
  復元は一つのsupervisorが始めから結果の記録まで持つ。
  runのleaseを取ってから始め、scratchやworktreeを変える前ごとにleaseがまだ自分のものかを確かめ、結果はleaseと操作の世代が変わっていないときだけ記録してleaseを返す。
  leaseを失ったsupervisorはscratchも操作も変えない。
  manifestが名指すpath（各fileの名前とuntrackedの元のpath）は、空・絶対・`.`・`..`を含まない相対pathであることを確かめてからjoinし、外れたmanifestはsnapshotのfileにもworktreeにも触れずに復元の失敗とする。
  照合か復元の失敗（食い違うworktreeは上書きしない）は`replan_restore_failed`を記録し、操作を`blocked`に戻す（下の「復元や起動の失敗はblockedのまま」）。
  resumeは始まらず、`replans`の`problems`に出る。
  `replan_parked`はresumeの区分・統計・`resolved_head`で一つのparkとして読む。
  containerのcloneのrunは、制御側がcloneでgitを走らせず、executorがturnの終わりに書き出した差分・index・untracked（実行権限を含む）を状態の世代とheadと照合してsnapshotに写す。
  書き出しが無い・古い・照合が合わなければ保存せず`blocked`にし、cloneに手を付けない。
  継続・撤回では、制御側が復元の材料（消えたcloneには作り直したことと、headのbundle）をrun dirに置き、消えたcloneはbaseで作り直す。
  executorはresumeのturnのagentの前に今のcloneをsnapshotと照合し、作り直したcloneにだけ当て、答えをrun dirに書く。
  制御側はresumeのslotで答えを記録し、食い違うcloneは上書きせずagentも動かさず、`replan_restore_failed`で`blocked`に戻してrunをslotから休ませる（providerの壁としては扱わない）。
  失敗を記録した後にcloneを直して操作を継続に戻すと、記録した失敗の数で数える新しい試行になり、前の試行の答えや作り直しを使わずexecutorがcloneを照合し直す。再起動・adoptは同じ試行を続ける。
  resumeを使い切ったrunはturnが続かず、今のcloneを見るexecutorが動かない（保留の前の書き出しは後の変更を映さない）ので、cloneの有無によらず復元の失敗とし、復旧jobにも継承retryにも渡さない。
- 条件の対応表の書き手: 済み（`replans.rs`の`write_mappings`）。同じ操作で同じ項目の削除を2度記録せず、削除した項目を子に割り当てない（複数の子への割り当ては許す）。
- 閉じた操作の依頼を閉じること: 済み（[taskの再計画の依頼](../design/supervisor-lifecycle/plan-planners.md#taskの再計画の依頼)）。
  依頼のplannerが今開いていなければ閉じる移動が閉じ（`replans.rs`の`move_in`）、開いていればそのplannerが決めずに終わったときに閉じる（`plan_requests.rs`の`decline_after_planner`）。
  復元の失敗から再びplannerに渡る段では新しい依頼を作る（`plan_requests.rs`の`renew_declined`）。
- plannerへの依頼とplan review: 済み（`src/domain/replan_plan.rs`、`replans.rs`、`request_planner.rs`）。
  依頼のplannerが立つと操作は`parked`から`planning`へ進み、promptは元条件とその版・snapshotの場所・runの原因別のturnとpassしなかったreviewの指摘・全後続を渡す。
  plannerは継続（`request decline --continue-run`）か、子taskを置換計画とともに`submit --replacement`するか、条件の削除・意図の変更・成果の破棄をscope / discardのaskにする。
  置換計画は元条件の各項目の子への対応（削除は、操作の後に開いた操作のscopeのaskにそのaskが選択肢に出した項目の序数とhashを名指す`delete:<序数>:<hashの先頭12文字>`を人がそのまま選んだものだけが認め、`keep`や別の項目への答え、tokenを含む拒否や説明は認めない。`landed`は着地したtaskを要する）、子ごとの元のverifyと全体のevidenceとpaths、保存した成果の各部分を1つの子に順とともに渡すこと、元の前提依存の引き継ぎと元taskや後続への依存の禁止、全後続が待つ子を持つ。
  置換proposalのreviseのplannerは、保留したtaskへの削除の権限のaskを打ち、その答えを受け取れる（下の「ask・request・再起動・継続と撤回」）。
  submit・`lint`・plan reviewのpassは同じ検査（`check_plan`）を今の版で当て、外れればsubmitは拒み、passとconcernへの人の`ready`はreviseに変えて問題をplannerへ届ける（passはreviseの上限を過ぎていればconcernにする）。
  子は`draft`か`submitted`のtaskに限り、取り消された子に項目・成果・後続の待ちを渡す計画は通らない。passはそのactionを当てた後の姿で検査する（`PlanContext::after_actions`）。
  reviseの後の出し直しのsubmitも計画を要して検査し、plan reviewが人に保留したproposalの再試行だけが前の計画のまま出し直す（`proposals.rs`の`held_retry`）。
  同じgoalのSpikeと再計画の操作の依頼のplannerは、answerによる開き直しとその依頼のproposalのreviseが立てるplannerも含めて1つずつ開く（`plan_requests.rs`の`TARGETS`・`open_request_planner`・`REVISE_WAITS_FOR_GOAL`）。
  plan reviewのjobには計画・元条件・snapshotの場所・後続を渡し、意味上の対応と成果の引き継ぎ方を検査させる。`replans ID`は最新の計画とその今の問題を出す。
  passは計画を承認済みにして子を`submitted`のまま残し（readyは誰の手でも拒む）、concernは操作を`awaiting_answer`に、proposalの取り下げは`planning`に戻し、どれもrunの保留とsnapshotを保つ。
  操作が継続か撤回で閉じると、未適用のproposalの子はdraftに戻る。
  依頼が`--continue-run`の却下で終われば操作は`continuing_after_park`へ、そのほかの却下か`exhausted`、proposalを取り下げたplannerが次を出さずに終わったときは`blocked`へ進む（`settle_replan_requests`）。
- 置換の適用: 済み（`src/domain/replan_apply.rs`、`src/infrastructure/replan_apply.rs`、`src/application/supervise/planning/replan_apply.rs`）。
  supervisorの計画の回が、passで承認された最新の計画を持つ操作を`applying`へ進め、snapshotをmanifestと固定したrefで照合してから、1つのtransactionで全てを検査し直して書く（`apply_problems`）。
  書くのは元taskとrunの`replaced`、置換先と子ごとの引き継ぎ（`carry`）、条件の対応、全後続の辺の付け替え、子の`ready`、旧runの開いたaskの閉じ、`applied`と`replan_applied`。
  後続は計画が挙げたものは計画の子を、挙げないもの（操作が条件を固定した後、柵の前に足された後続）は全ての子を待つ。後続の追加は計画を古くしない（`check_replacement`は版を固定した後続で比べる）。
  承認の時の子の条件（待つtaskとgoal、runの進め方を含み、子を待つtaskと優先度は除く）の版を記録し、適用はそれと今の版を比べ、passの後に子が変われば適用しない。
  問題か途中の失敗は全体をrollbackし、操作を`applying`からの`blocked`にして理由を残し、snapshot・ref・runは保留のまま。循環はgoal依存・goalの所属task・適用済みの置換の辺も辿り、submitとpassの検査も同じグラフを見る。
  `blocked`の後は問題の種類で進む（`ApplyRecovery`）: 書き込みの失敗やleaseのような待てば変わるものは毎回の計画の回が検査し直し（理由が変わったときだけ`replan_apply_rechecked`で残す）、無くなれば適用する。計画が成り立たないもの（版・子・始まった後続・循環）は子をdraftに戻し、操作を`parked`に戻して依頼を新しくし、そのplannerが新しい計画・継続・撤回を選ぶ。snapshotの食い違いは人に残す。
  `applying`で止まった操作は次の回が同じ手順で終える。適用済みの操作への2度目の適用は何も書かない。
  子のclaimは計画が名指した部分だけを渡す: snapshotのheadは固定したrefと照合してpromptで名指し（containerのcloneにはそのrefを渡す）、未commitの変更はmanifestと照合してrun dirの`replan-carry/`へ写す（未追跡の実行可能fileはその権限ごと。`run_inherited`）。
  渡せなければ`replan_carry_failed`を残してprovisioningの失敗とし、snapshotとrefは消さない。holdの解除が今のmainから始め直させた（`retry_fresh`）子のその後のclaimには渡さず、引き継ぎがまだ続いていた子ではその解除が引き継ぎの終わり（`replan_carry_ended`）を記録する。子の成果は子のrunのreceipt・検証・review・e2e・着地だけを通る。
- goal close: 一部（`replaced`は終わったtaskに数え、completedには数えない。置換先を再置換も辿った全てのtaskを、別のgoalへ移ったものも含めて読むので、それが全て終わるまでgoal reviewの起動もcloseも待つ（起動に要る`completed`は所属taskだけで数える）。定期taskの次の登録も同じく待つ。置換先の記録が空・欠落・循環の置換元は解決済みとせず、goal reviewの起動・achievedの適用・closeを拒み、定期taskのkeyを保ち、`status`のattention（`replacement_unresolved`）で人に見せる。goal reviewのjobには置換の系統・元条件・条件の対応を渡し、fingerprintはそれと計画・条件の版を持つので、起動の後に対応が変わればachievedを適用しない）。
- 検知と診断: 済み（`src/domain/replan_detect.rs`、`src/application/supervise/execution/replan_diagnosis.rs`）。
  `dagq.toml`の`[replan]`が`enabled = true`のときだけ動く（keyと既定値は`ReplanConfig`のdoc comment）。
  表が無い・無効なら検知も診断もせず、人とworkerが開いた操作は`candidate`のまま待つ。
  この表を知らない古いバイナリは表を拒むので、queueを動かす固定バイナリが読めるまで`dagq.toml`に足さない。
  schemaは変えず、診断はoperationの`diagnosis`に読んだ窓の終わり・未達の項目数・決めた者を足して持ち、それを知らない読み手は読み飛ばす。
  検知は進行中のtaskの最新runについて、下の「回数・抑制・上限と観測」の窓を数える。
  窓はrunとproviderを跨いでtaskで数え、最後のpassと前の診断が読んだeventの後から始まる。
  閾値に届くか、窓に非passがある上で既存の上限の出口（reviewの`revise_limit`、resumeの使い切り）が来れば、`runtime`の操作を1つ開き、開いた操作のあるtaskには開かない。
  操作は起点を問わず一度に1つずつ、run slotの外で`diagnosing`へ進み、復旧jobの経路で読み取りだけの診断jobが動く（runのturnは止めない）。
  jobはrunのrun dirの下だけで動き、run dirの無いrunには準備中か準備の前に中断したなら待ち、それ以外は始めずに人へのaskにする（main checkoutには書かない）。
  jobの試行とaskはそれぞれ操作を新しい版で`diagnosing`へ取り直してから始めるので、同じ版を読んだ2つのsupervisorの片方だけが取れ、前の版のjobの結果もaskの答えも適用されない。
  askはその取り直しと、askを操作に結ぶ`replan_diagnosed`と同じtransactionで書く。
  作業中のrunのjobはturnの合間（依頼したturnも走るturnも無いとき）にだけ始め、止まっているrunのjobは待たせない。
  合間はturnのeventに加えてsessionの`turns/`の依頼とidle markerで読む（走るturnの間に書いた依頼のeventはそのturnの終わりより前に残るため）。
  jobには条件・原因別のturn・窓の非passのreviewの指摘・衝突やe2eや失敗したturn・前の診断・receiptを渡す。
  verdictはその時の操作の版と状態、taskとrunの今を確かめ直してから`decide`で適用する。
  適用の移動とverdictからのaskは同じtransactionで前提を確かめ直す。jobの開始の後に次のどれかがあれば適用もaskもせず診断し直す: runの状態の変化、review・turn・receiptの到着、taskの条件の変更、着地の開始、人の依頼の合流（人の依頼でないと読んだ判断のとき）。
  runのHEADは移動とaskの直前に、jobの開始時に読んだものと比べる（queueのtransactionの外なので、turnのeventの無いcommitがその間に入る隙は残る）。
  自動の保留は系統の上限も同じtransactionで数え直し、超えるなら人へのaskにする。
  継続は`continuing`、作業単位の原因の再計画は`pause_requested`（その後は上の保留の経路）にする。外因だけの再計画は継続の判断とし、継続と同じ確かめを通す。
  確信度の低いもの・系統の自動の保留の上限・未達の項目数か進展の無い継続・残作業が2回続けて減らない継続・人の依頼への継続・jobの3回の失敗は、taskについての`decide`のask（`continue`か`replan`）にする。
  人の答えはその者の判断として記録して同じ移動をする。
  人の依頼でない操作を診断が柵へ進めると、同じtransactionでplannerへの依頼を記録する。
  着地したrunの操作は閉じ、着地中のrunは待ってから診断し直す。
  taskの[hold](task-hold.md)は診断の開始と結果の適用に先立つ（[ADR-t1879-1](../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)決定6）。hold中のtaskには診断jobを始めずaskも開かず、verdictも人の答えも操作を進めない。
  jobとaskのための取り直しと、verdictと答えの移動とaskの前提はholdを含み、同じtransactionでholdが開いていれば拒む。
  hold中に出たverdictは、jobが読んだもの（版・runの状態・HEAD・読んだeventの終わり）とともに記録に保持し、操作はその版の`diagnosing`のまま残す。人の答えはaskに答えたまま閉じずに残す。
  解除の後の最初のpassは、保持したverdictを同じ前提の再検査を通して適用し、前提が崩れていれば捨てて診断し直す。保持を確定する記録はどの保持かを名指し、前の版の遅れた記録は今の版の保持を無効にしない。答えも解除の後に今の経路で適用する。
  動いていたjobのsupervisorがいなければ（exec後の同じprocessも）次の試行として始め直す。
  `status`・`doctor`の`replans`は、`dagq.toml`が読めなければ設定と窓を出さずにその誤りを出す（supervisorは前に読めた設定で動き続ける）。開いた操作ごとに診断・taskの原因別のturn・窓（閾値は継続の後の上限を含む）・系統の自動の保留の数を出す。
  全体では状態・起点・選択・決めた者・主因ごとの数と、柵を越えた数と置換先を出す。
  `timeline`はrunの原因別のturnと再計画の段を、`stats`は窓の原因別のturn・開いた操作・診断・askの理由・移動を出し、数え方は検知と同じ関数による。
- pluginの手順: 済み（dagq skillの`reference/replan.md`がフローと役割ごとのコマンド・条件の対応・分割の例を、dagq-recover skillの`reference/replan-held.md`が保留の読み方・人の判断・`blocked`の扱いを持つ）。
  `withdrawn`へ進める経路と、適用以外の`blocked`（保存・復元の失敗、継続なしの却下・使い切り・置換proposalの取り下げ）を先へ進めるコマンドはまだ無い。
  pluginはそれらを手順として書かず、操作は`blocked`のまま成果を保持し、人に理由を見せるだけとする。

## 入口と診断

フローは **候補検知 → 診断 → 継続か保留 → snapshot → plannerの置換proposal → plan review → trusted runtimeの適用**。手動の再計画依頼・workerの提案・runtimeの検知は同じ候補記録に合流する。人/inboxは対象runを参照した既存の永続requestを入口にできる。workerは自分のrunに提案と根拠を残すだけで、停止・実行中taskの編集・request登録・DB操作は行えない。権限上の問いだけは従来のworker_questionを使い、単なる分割の推奨をaskにしない。再計画を明示した手動requestは通常のplanner起動より先に候補へ結び、snapshot確定まで起動を待たせる。runtimeは採用した候補を一意の再計画操作に結び、同じrun・同じ診断窓の重複をまとめる。
操作の一意性は実装済み（[SQLite persistence](../design/persistence.md#不変条件)）。

診断は読み取りだけの復旧jobの契約を拡張し、根拠event ID・時刻の窓・task/goalの条件・paths/verify/evidence・receiptとHEAD・原因別のturn/review/resume・前回の診断と進捗を渡す。実行中turnを止めて診断する必要はない。診断結果の選択はcontinue / replan / askで、原因・比較した案・推奨・confidence・次に確かめる条件を持つ。runtimeは診断の対象versionを再確認する。

| 原因 | 診断の根拠 | 通常の次の手 |
| --- | --- | --- |
| 局所的な不具合、test/evidence/docsの欠落 | 指摘と差分が局所で、残作業が減っている | 同じ条件でrevise/resumeを継続 |
| acceptanceの曖昧さ・矛盾・実現不能 | 条件と先例・ADR・観測が食い違う | 解釈できれば記録して継続、意図変更が要ればscopeの判断 |
| 作業単位が過大、独立な条件が混在 | 条件ごとの進捗と成果の境界を分けられる | 全条件を保つ分割・範囲再配分 |
| rebase/precheckの衝突 | pass済みのHEAD、衝突event、mainの進み | 既存の衝突解消や継承retryを先に比較、総turnで分割しない |
| e2e/検証の失敗 | 対象commit・失敗log・test名・環境 | コードの修正かhost/環境の復旧。関門の削除を分割と呼ばない |
| provider/認証/資源、runtimeの障害 | turn failure、hold、復旧log | provider切替・hold・復旧。作業単位を小さくしても直るとは限らない |

原因は複数付けられ、主因と副因を記録する。高い確信度で元条件・成果を保てる推奨はADR-t451-1どおり自律で採用する。scope/discardの既存の権限ある判断で決まらない意図変更・成果破棄、low confidence、上限・復旧不能だけ人へ上げる。人の判断はask/answerのauthority・approvalと元eventを参照し、plannerの推測で代用しない。

## 永続状態と所有権

以下の名前は追加する再計画操作の状態で、既存run statusに暗黙に読み替えない。操作はoperation ID、元task/run、goal、起点（manual/worker/runtime）、診断窓、task/goal/依存のversion、単調増加するgeneration、lease token、戻り先phase、snapshot ID、request/proposal ID、ask ID、結末と理由を永続する。状態と遷移eventは同じDB transactionに書く。

| 状態 | 許される進行と保持 |
| --- | --- |
| candidate / diagnosing | 読み取り診断。元runは通常どおり進む。条件やHEADが変われば再診断 |
| continuing | 保留せず同じ条件で継続した結末。根拠を記録して操作を閉じる |
| pause_requested / quiescing | 次turn・通常receipt処理・review/e2e/landingを封鎖し、書き手の終了を待つ |
| snapshotting | 書き手が無いまま保存。元taskはin_progress、claimを拒む |
| parked / planning | 完全なsnapshotとrequestが永続済み。run slotを返し、plannerの枠で計画 |
| reviewing / awaiting_answer | 子はsubmittedでclaim不可。旧成果・元task・全後続の保護を継続 |
| applying | 永続成果と版を確認する適用の準備。DB確定前なら依存は元のまま |
| applied | 一括確定済み。元task/runはreplaced、子と対応表と依存が可視 |
| continuing_after_park / withdrawn | snapshotを保持し元条件の継続を確定。withdrawnは案の撤回で、task cancelではない |
| blocked | 停止・保存・起動・review・適用の失敗。どの段から修正するかとエラーを保持し、旧成果を残す |

保留のguardはtask/runの最新性とgenerationで判定し、lease行の有無だけでは解除しない。staleなleaseをadoptする側はoperationも同時に引き継ぎ、generationを更新する。旧tokenの書き込み・配送・適用を拒む。leaseは排他制御、versionは診断/承認した内容の一致を保証し、どちらも必要。exec引き継ぎでも同じ記録から状態を再構成する。process内の変数やrun dirだけをSSOTにしない。

## 停止境界と競合

1. trusted runtimeはtransaction内で元task/runのversion、現在phase、lease、未着地を再検査し、pause_requestedとgenerationの柵を確定する。全てのturn配送・askのanswer配送・revise・resume・provider retry・receipt受理・自動recover/triage・landing開始がこの柵を見る。既に渡したrequestも旧generationとして次turnを開始させない。
2. 原則は現在のbackground turnが終わる境界で止め、次turnを起動しない。pauseの待ちは既存のturn上限までとし、超過・緊急停止では記録したwrapper/agentと所属を再確認した子孫をpid/process groupで止める。名前でsignalしない。wrapperのack、turnの終了、書き手processの不在を確認する。孤児helperやworktreeの書き手が残ればparkedにせずblockedで復旧へ渡す。
3. validating/review/triageのjobが動いていれば、その有界な終了を待つか当該jobだけを停止して終了を確認する。verdictは履歴に残すが、古いgenerationのrevise・ask・landingを適用しない。e2e実行中も終了とfixture cleanupを待ち、結果を保存する。e2e待ち/再試行待ちなら新しい試行を止める。失敗したe2eをpass扱いにしない。
4. integratingが先に所有権を得た場合は再計画を適用しない。integrateを途中で止めてsnapshotしない。着地が終われば候補を不要として閉じ、needs_sessionへ戻れば改めて診断する。pauseが先ならbegin_integration、手動integrateを含む全着地経路を拒む。review passやintegration_approvedがあってもguardを迂回しない。
5. Git操作（rebase/cherry-pick等）が途中なら、conflict状態・index・作業treeと操作metadataを保存できるまで保存の失敗として保持する。HEADだけをcleanな成果とみなさない。復元は専用のscratch worktreeで行い、元worktreeを上書きしない。

遅延receiptは受け取ったbytesと時刻・旧generation・拒否理由を診断材料として保存する。pauseの柵より前に受理しても、その後のvalidation/review/landingが柵を再検査する。柵より後のreceiptやworker終了はcompletedへの遷移を起こさない。applied後の旧workerの終了もreplacedをfailed/completedに変えない。書き手不在の後にHEADや保存対象が変わったらsnapshotは無効で、封鎖したまま原因を調べる。

この柵は保留の理由を複数持てるようにし、[taskのhold](task-hold.md)（[ADR-t1879-1](../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)決定7）もここに理由を1つ足す。
holdが共有するのは、次の工程（turn・回答の配送・revise・resume・着地など）を始める前の「次の工程を始めてよいか」の判定だけで、上の2のturnの上限や緊急停止でのprocessの停止、3のjobの停止、receiptの受理の柵は再計画だけのもので、holdは引き継がない（holdは走っている工程を止めない）。

## snapshotと保存の順

snapshotは引き継ぎの材料で、receiptのsucceededやreview passの代わりではない。trusted runtimeがworkerの書けない保存先に作り、内容hashとmanifestを永続記録に結ぶ。読むfileはlink/FIFOを辿らず、通常fileを上限付きで読み、許されないfile・容量超過・I/O失敗を黙って省かずblockedにする。

保存するもの: base/HEADと元runのcommit範囲、Git refで固定したcommit、staged/unstaged差分とindex、untrackedソースと証拠、進行中Git操作metadata、task/goalの元条件・paths/verify/evidenceと全前後依存の写し、receiptの全体とhash、review/検証/e2eのlogと対象commit、turn/request/会話session ID・provider・会話の保存可能な参照、診断eventとask/answer。外部transcriptの参照だけで復元可能とは言わず、必要な引き継ぎ文面と会話の材料を保存する。target等の再生成可能なbuild出力は除外をmanifestに明記してよい。元taskのpaths外の成果も隔離保存し、子の成果として採用できるかはplan reviewで判断する。

順序は **pauseの柵 → 書き手停止確認 → 保存準備のmanifest → ref固定と一時dirへの保存 → hash/復元可能性の照合 → atomic renameでsnapshot確定 → DBでsnapshot参照とparked・永続requestを同時確定 → slot解放 → planner起動**。file/refとSQLiteを跨ぐtransactionとは呼ばない。保存準備のoperation IDを鍵に、再起動後に確定済みdir/refを照合してDB確定を再試行する。未完のdirは成果を捨てず補修する。置換起点でsnapshotが無いrequestのplannerを起動しない。DB確定前に掃除や旧branch/worktreeの削除をしない。

保存されたソース・証拠・refと元worktreeはpinする。適用直後も子の引き継ぎ確認が済むまでpinを外さず、その後は元のsnapshotを履歴の成果として保持する。自動cleanupは再生成可能なbuild出力だけを削除できる。保存成果の削除・採用しない選択は対象と理由、権限あるdiscard判断を別に記録する。容量不足も破棄の承認とみなさない。

## 元条件と置換proposal

plannerはsnapshotを読むだけで、元worktreeで修正しない。修正案のtaskをdraftからsubmitし、一つのoperationには一つの有効な置換proposalだけを結ぶ（reviseは同じproposalのversionを進める）。普通のproposalに子を紛れ込ませても元taskは終了しない。案はcontinue / split / redistribute / withdrawを区別し、破棄は独立のdispositionとして明示する。

元acceptanceを文面とordinal/hashで固定し、重複した同じ文面も別の項目として対応表に載せる。goal acceptanceも同じように参照する。以下は書式の例でありtaskの一覧ではない。

| 元条件 | 現在の根拠と残作業 | 子の条件と成果の所在 | 扱い・判断の根拠 |
| --- | --- | --- | --- |
| A1: APIの振る舞い | snapshot Sのcommit C、残る境界test | 子XのX1、Cの範囲を継承 | split、未達の検証はXで実施 |
| A2: 運用と検証 | 文書差分は保存、統合検証は未達 | 子YのY1、YはXに依存 | redistribute、同じgoalに残す |
| A3: goalの追加要件 | 根拠不足 | 対応する子が無い | 削除を求めるならscopeのauthority/answerを必須にする |

権限ある削除の判断を別に記録した項目を除き、全項目を少なくとも一つの子条件へ割り当て、複数の子で共同で満たすなら全てを載せる。既存mainに着地済みの根拠で充足する項目はcommit・検証の証拠と検査する子を載せる。元runの未着地成果だけで「達成済み」にしない。子のpaths/verify/evidence、依存DAG、成果の採用範囲と採用順・重複適用防止、残るreview指摘・e2e/衝突の対応を含める。各子のgoalは元goalと同じで、goal無しなら同じ目的を保持するgoalの設定をproposalで明示する。スコープ縮小は残りを子に残し、goal acceptanceそのものの削除はauthority・actor・対象旧新値・理由・ask/answer/event IDを要する。

plan reviewは通常の規則に加え、対応の漏れ/削除/重複、成果の所在と安全な採用、子の検証、全後続の対応、循環・claimとの競合、authorityを検査する。機械的な検査はID/version・全項目の被覆・goal一致・依存DAG・guard、意味上の充足はjobが行う。passまたは権限内のhighのready推奨は「このproposal versionを適用可能」にするだけで、この段で子をreadyにしない。reviseは案を直し、low/scope/discardや上限は既存approve_plan/planner_questionへ送る。review中に子や条件が変われば承認は無効。

## trusted runtimeの原子的適用と依存

snapshotの完全性を再確認した後、BEGIN IMMEDIATEの中で次を全部再検査する: operationが適用待ち、lease token/generation、元task/runが最新で保留中、旧書き手不在の停止証明、元task/goal・proposal・全前後依存のversion、reviewが検査したproposal hash、必要なscope/discard承認、後続が未claim、循環なし。停止証明は停止したwrapperが新しいagentを登録できないgenerationの柵と組み合わせる。fileの照合後に内容が変わらないtrusted保存領域を使う。

一つのDB transactionで元task/runを新しい終端の`replaced`にする（completedでも通常canceledでもない）。置換先IDs、条件対応・snapshot参照、全子のready化、全後続依存の付け替え、requestの適用結末、旧runの着地/claim/receipt権限の失効、operation appliedとeventを確定する。goal acceptanceの変更を権限ある判断で認めた案なら、その旧新値とauthorityもこのtransactionで更新・記録する。失敗はrollbackで子も元依存もそのまま。DB確定後の通知・表示・掃除は冪等な後処理にする。再配送にはoperation IDの既存結末を返し、二重に子を作らない。

全ての直接後続について、元taskへのedgeを、その後続に必要な子**全て**へのedgeに置換する。元taskが全部の条件を表したedgeは既定で全子を必要とする。一部だけ必要ならplannerが対応表で理由を示し、plan reviewが条件の漏れが無いと確認した場合だけ絞る。推移的な後続もこのDAGから待つので、全後続の一覧とbefore/afterをproposalと適用記録に残す。子は元taskの前提依存を引き継ぎ、子どうしの依存も検査する。replacedを依存充足と読む逃げ道を作らず、後続のclaimは必要な子全てのcompleted（着地）を要求する。子のcancel・再置換・失敗は自動でedgeを満たさず、再置換なら同じ手続きで次の全子へ展開する。

保留前に後続がclaim済みなら、この操作でそのrunを編集/停止せず、適用を拒んで再診断する。claimと依存編集は同じtransactionでguard/versionを確認するため、確認後の割り込みでも先に確定した側だけが進む。通常edit/cancel/ready/retry/recover/integrateやproposalの普通のready化は、保留中の元taskと置換候補の子・保護されたedgeへの変更をuser/inboxからも拒否する。worker/plannerの既存禁止も維持する。解除・破棄はこのoperationの専用の権限ある結末だけを使う。通常のready-task reviseではin_progressをsubmittedへ戻せない。

## goal reviewとgoal close（予定・未実装）

ADR-t1521-2決定5は、ADR-0047決定43のgoal reviewの起動・achieved適用と、決定8の`goal close --verdict achieved`の拒否条件を改める。所属taskの`completed` / `canceled`に加えて、`replaced`の置換元は**置換先を再帰的に辿った全ての葉が`completed`か`canceled`になった場合だけ**終了条件上の解決済みとして扱う。未終端の葉、再計画で保留中の子が残れば起動もachievedでのcloseも待つ。置換先が空・欠落・循環している記録は解決済みとせず、復旧へ渡す。置換元のstatusは`replaced`のままで、`completed`へ変えず、goal reviewの「completedが1件以上」の件数に加えない。実際の子など所属taskのcompletedだけを数える。cancelされた子も含む終了条件の充足はgoal acceptanceの達成証明ではない。後続のclaimは上の契約どおり必要な子全てのcompletedを要求し、この終了条件を依存充足には使わない。

goal reviewの入力には元条件の対応表、置換の系統とsnapshot参照、子の着地commit・receipt・検証/review/e2eの根拠、cancelや権限ある削除/破棄の理由を載せる。元runの未着地snapshotを達成証拠にせず、元goalのacceptanceを子の成果で照合する。足りなければ通常のgaps/askへ進む。既存のdraft・未完task・open ask・follow-up所属判断・acceptance版などの起動/close条件は維持する（[Goal review](../design/supervisor-lifecycle/goal-review.md)）。

fingerprintには既存の入力に加えて、置換元と再置換先のID・status・version、置換relation/元条件対応のversionを含める。goal reviewのachieved、`approve_goal`のachieved answer、人/inboxによる`goal close --verdict achieved`は、同じtransaction内で置換の系統・全葉の終了・条件対応・所属と既存のclose条件を再検査する。reviewのverdictは起動時のfingerprintとも一致させ、起動後の再置換・子の復帰・条件変更なら適用せず取り直す。再起動でも同じ永続記録を読む。実装責務(a)(c)はこの共通判定、入力/fingerprintとclose経路の配線を含み、再置換が終わるまで待つこと、全子が終われば起動/closeできること、未達をcompletedにしないこと、cancelで達成を捏造しないこと、並行した置換で古いachievedを適用しないことをunit/integrationで検証する。

## ask・request・再起動・継続と撤回

askの行の持ち主（元runかrequest）を付け替えない。operationは関連askと未配送answerを参照し、保留中の旧workerへの配送を止める。worker_questionのanswerは元条件の判断材料としてplannerに参照で渡すが、旧workerへのask_deliveredとは記録しない。requestについてのplanner自身の問い（`--request`のask）はそのrequestのplannerだけが打て、答えもそのrequestのplannerへ届く（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)決定7）。これとは別に、閉じていない操作の置換proposalのreviseのplannerは、保留したtaskに`--task`の`planner_question`で項目の削除を聞ける。その答えは置換proposalのtaskについての答えと同じplanner（reviseの持ち主か、reviseとともに新しく開くplanner）へ届き（`draft_planners::route_of`、[reviseの配送](../design/supervisor-lifecycle/plan-review.md#reviseの配送)）、操作についてのaskとして削除の権限に使える（`replans.rs`の`check_replacement`）。子taskへのaskは操作についてのaskではないので、削除の権限にならない。既存approve_landingのanswerも保留中は着地に使わない。旧計画を対象にしたland/cancelの答えを新計画の承認に読み替えず、版違いを記録し必要なら新しい問いを作る。

継続時は未配送answerと元の指摘を同じrunの新generationのbackground wrapperへ一度だけ送る。置換確定時は適用できなくなった旧runのaskを理由と置換先参照付きでruntimeがanswer/closeし、既存の人の答えは上書きしない。引き継ぐ判断はsnapshot/子promptに参照する。approve_plan/planner_questionのanswerもproposal/request versionと適用の結末を再検査し、古いanswerで適用しない。

requestは既存のopen/proposed/declined/exhaustedを維持する。置換起点のrequestはoperationとsnapshotを持ち、openは計画待ち、submitでproposed。ただしproposedは適用済みを意味しない。operationのapplied/continuing_after_park/withdrawnが実際の結末で、inboxに依頼・診断・保留・proposal・適用か継続を結んで知らせる。declined/exhausted、proposal取り下げ、job失敗は旧成果を保留したblockedへ戻し、黙ってcancel/readyにしない。修正は同じoperation/requestの案をreviseし、別requestが必要な言い直しは旧operationの閉じ方を先に記録する。

継続・案の撤回は、子がまだclaimされていないこととoperation versionを確認して、未適用proposalの子をclaim不可のまま閉じ、元taskの条件/依存を保持したままguardを新generationの継続へ移す。旧background wrapperの終了を確認してから一つだけ新wrapperを起動する。同じprovider会話が使えなければsnapshotから新sessionを立てる。復元や起動の失敗はblockedのまま。新receipt・validation・review・必要なe2e・integrateを通し、古いpassを新HEADに流用しない。継続でも既存のrevise/resumeの数はリセットしない。applied後の撤回は元をreadyに戻す操作ではなく、新たな条件付き再計画とする。

再起動/adoptは永続状態を照合する: quiescingは書き手の終了待ちから、snapshottingはmanifest/ref/dirの照合から、parked以後はsnapshotとrequestの欠けの補修から再開する。planner/jobの孤児とleaseを確認し、同じrequestで二つのplannerを立てない。applyingでDB未確定なら再検査し直し、appliedなら後処理だけを行う。旧receiptや旧終了eventは履歴として取り込んでも状態を逆行させない。再計画を知らない古いruntimeがguardを迂回するのを防ぐ非互換schema/起動の柵は実装taskが設ける。

## Spikeとの共通基盤・実装の責務

goal 96のADR-t1487-1と、task 1395で実装した永続request・runtime planner・plan review・ask/answer・起動回数とadoptを共有する。Spikeのcompletedからの依頼は結果による次の計画で、未達runの保留/置換とはoriginと成果の状態が違う。同じgoalでSpike起点とtask置換起点の再計画planner/適用は一つずつに直列化し、後の案は前の結末とgoal versionを読んで作る。workerはslotを返してplannerを待ち、plannerは案をsubmitして終わり、子の実行を枠を持ったまま待たない。調査の周期/同時数の柵はSpikeの契約を使い、再計画の回数と混ぜない。

後続の実装責務は、(a) queueの状態/version/lease/claim/権限/原子的置換とmigration、(b) background停止・snapshot・adopt/receipt柵・復元、(c) 診断とplanner/request/proposal/plan reviewの配線、(d) CLIの追跡・plugin手順・評価に分ける。各実装taskはその変更に対応するdesignの予定の節を実装済みへ更新する。本taskはruntime/schema/CLI/pluginを実装しない。

適用前のtestは原因別判断をunit、境界をintegrationで確認する。二つのsupervisor、exec/restart、各保存段の失敗、重複request/proposal/answer/適用、遅延receipt・旧worker、孤児process、review/e2e/integrateとclaimの競合、全後続と再置換DAG、通常edit/cancel/readyの拒否、snapshotのdirty/index/untracked/Git操作復元を含める。子の検証・review・e2e/integrateの関門を実際に通ることも確認する。workerはhostの実queueやe2eを操作せず、hostのe2eは通常どおりruntimeの関門が行う。

## 回数・抑制・上限と観測

CLIのtask/run/request/proposal/eventの読み取りにoperation、generation、診断窓と原因、保留時刻、snapshot、元条件対応、全後続のbefore/after、引き継ぎ先、判断actor/authority、結末を結ぶ。statsは総turnと原因別turn、review verdict、revise/resume（衝突/e2e/その他）、診断・保留・適用・継続・撤回・失敗の件数と時間を別に集計する。原因未分類はunlabeledとし、reviewの理由件数とreview回数を混ぜない。一つのturnに原因が複数なら主因ごとの件数は一つ、副因は別の非加算表示にする。

候補検知の初期値は同じ原因の非pass reviewが連続3回か、同じ診断上の残作業が2回続けて減らないこと。総turnは参考に表示するだけで自動停止/分割に使わない。自動診断は同じrunの同じHEAD/条件/根拠event窓で一回だけ、継続を選んだ後は新しい根拠か進捗の変化まで抑制する。手動依頼は抑制を越えても重複操作は作らない。
ここでの「同じ原因の非pass review」は、reviewから来た非pass（concernとrevise）を`primary_code`を問わず数え、passで数え直す。
衝突・e2e・検証・provider/host・receiptの促し・worker質問の答えのturnとその後のreviewの無い区間は、この数を増やさず数え直しもしない。
`primary_code`が同じことを条件にしない（#1405は同じcodeの連続が最長2回で、条件にすると長期化を見逃す。下の「長期化の測定」）。
候補検知が起こすのは読み取りだけの診断で、検知だけでrunを保留しない。
継続を選んだ後に続けてよいのは、reviewから来た非passの2回まで（その修正のturnを含む）で、3回目の非pass reviewを新しい根拠として再診断する。
この再診断も保留を決めるのは診断で、回数で自動に保留しない。
衝突・e2e・検証・provider/hostのturnはこの追加の上限に数えない。
抑制は時間では決めない（reviewの往復は数分から数十分で、人の答え待ちが数時間になりうるので、時間の窓は原因の区別にならない）。

一つの元taskから再置換先までを含む系統に自動の保留・再計画を3回までとする（pauseの柵の確定で一回、保存や起動のretryは同じ回）。適用・撤回・継続でも数を戻さず、再起動でも永続記録で数える。到達したら根拠と今の成果を保持して人のscope/recovery_failed判断へ上げ、上限を超える自動分割をしない。権限ある追加一回の判断は対象系統・理由・回数を記録する。requestのplanner起動は既存の一件3回、plan review/revise/resume・Spikeの上限も独立に守る。抑制/上限の記録から候補数と実際の保留数を分けて評価する。設定の実装ではこれらの値を文書と一致させ、固定バイナリが読めるまでdagq.tomlへ新keyを足さない。

## 歴史的な観測窓

#1405、run `42637110`の例は動き続けるrunの最新総数ではなく、**event 70011（2026-10-03T00:47:54Z、12番目のturn_finished）までを含み、70018より前で締めた窓**である。
task 1522の基準値もこの同じ締めと定義を使い、後の数を混ぜない。
下の数はqueueのeventから再現でき、原因別の内訳と測定時点までの窓は「長期化の測定」が持つ。

| この窓の数 | 定義と根拠 |
| --- | --- |
| turn_started 12 / turn_finished 12（12 turn） | 同runのそれぞれのevent件数。startedは送ったturn、finishedは終わったturn。実行中はstartedだけに数える。終端は70011 |
| review 10回 | review_finishedのevent件数。理由の数でもjob起動数でもない |
| 差し戻し8回 = concern 3 + revise 5 | pass以外のverdict件数。concern: 65320・65818・69064、revise: 65751・68811・68981・69528・69651 |
| pass 2回 | 69789・69907。passの後も衝突やe2e対応はあり、taskの着地と同義ではない |

proposal 548の提出時点は別窓で、event 70025（2026-10-03T00:50:05Z）までならturn_started 13 / turn_finished 12、review_finished 11（非pass 9、pass 2）。追加の非passは70018（11回目のreview_finished、revise）。この別窓の13 turnやreview 11を歴史的基準値に加えない。

この事例を「12 turnなので分割」とは判断しない。concern/reviseの指摘の重なり、条件ごとの残作業、passしたHEADを診断し、衝突の解消は既存のprecheck/resume/継承retryを比較し、e2eの失敗は対象commitのlogでコード修正かhost復旧を分ける。衝突/e2eのturnはreviewの8回に加えず、turnの12回の中で原因を分けて数える（「長期化の測定」の原因別の内訳）。
分割する案でも未達の指摘と必要なe2eを子に引き継ぎ、元成果を保存してから新しい関門を通す。

## 長期化の測定（導入前の基準値）

再計画の導入前に、#1405と比べられる非対話のrunの長期化を原因別に数えた基準値。
読んだのは`dagq events`・`dagq stats`だけで、queueの操作・KPIの印・hostの設定は変えていない。
測定の時点は、eventを読み終えた後に`dagq status`が返したcursor 296163（2026-10-22T21:08:01Z）。
再現は[measure.py](task-replanning/measure.py)で、runごとの値は[out/runs.csv](task-replanning/out/runs.csv)にある（1回きりの測定の出力で、同じ引数で作り直せる。queueのeventは消えないので、窓が同じなら値も同じ）。
検知の初期値・抑制・継続の上限の値は「回数・抑制・上限と観測」だけが持ち、この節はその根拠を持つ。

### 数え方

- turn: runの`turn_started`（送ったturn）と`turn_finished`（終わったturn）のevent件数。
  走っているturnはstartedだけに数え、finishedに数えない。
- review: `review_finished`の件数。
  差し戻し（非pass）はverdictがpass以外（concernとrevise）の件数で、reviewの理由の数やjobの起動数ではない。
- turnの原因: `turn_started`の`what`で分ける。
  `revise request`はreview、`conflict request`は衝突、`answer of ask N`はworker質問の答え、`provider switch`と失敗した`turn_finished`の次のturnはprovider/host。
  `resolution request`（resume）は、その前の最後の契機のeventの原因を取る: 人の`send_back`（`landing_decided`）と非passの`review_finished`はreview、`conflict_precheck`と`rebase_conflict`の`landing_recheck_failed`は衝突、`run_e2e_failed`はe2e、rebase後の検証の失敗は検証、`validation_finished`の`accepted: false`はreceiptの促し、`resume_started`の理由はそれぞれの原因。
  どれにも当たらないturnは`unlabeled`として分け、ほかへ混ぜない（この窓では0件）。
- 人待ち: run の`approve_landing`・`decide`・`worker_question`・`stalled`・`blocked`のaskの`ask_opened`から`ask_answered`まで。
  答えの無いaskは欠測として件数を別に出す（0秒にしない）。
- 費用: `turn_finished`の`cost_usd`の和と`tokens.output`の和。
  値の無いturn（Codexの`cost_usd`、失敗したturn）は欠測として件数を別に出す。
- `dagq stats`の`resume_reasons`は使わない。
  #1405では衝突のresumeを`unknown`、e2eの失敗のresumeを`sent_back`と出し、eventの`resume_started`の理由（衝突1・人の`send_back` 2・e2e 1）と合わない。

### #1405の窓

run `42637110`（2026-10-02T15:24:17Z claim）。
3つの窓を並べ、後の窓の数を前の窓の基準値に混ぜない。

| 窓 | 締め | turn_started / turn_finished | review_finished（非pass = concern + revise / pass） |
| --- | --- | --- | --- |
| 歴史的な観測窓 | event 70011（2026-10-03T00:47:54Z、12番目のturn_finished）まで・70018より前 | 12 / 12 | 10（8 = 3 + 5 / 2） |
| proposal 548の提出時点 | event 70025（2026-10-03T00:50:05Z）まで | 13 / 12（13番目は走っている） | 11（9 = 3 + 6 / 2） |
| 測定時点 | cursor 296163（2026-10-22T21:08:01Z）まで | run 42637110は13 / 13、継承したrun `05efd4f3`は1 / 1、taskの累積は14 / 14 | run 42637110は12（9 = 3 + 6 / 3）、taskの累積は13（9 / 4） |

測定時点では走っているturnは無い。
run 42637110はevent 70528（2026-10-03T01:19:11Z）でtaskがreadyに戻って終わり、継承したrun `05efd4f3`がevent 70792（2026-10-03T01:47:20Z）でtaskをcompletedにした。
task 1405の最後のeventは157547（`attributes_baseline`）。

原因別のturn（根拠はturn_startedのevent ID）:

| 原因 | 歴史的な観測窓（12） | 測定時点のtask累積（14） |
| --- | --- | --- |
| 最初のprompt | 1（64736） | 2（64736、継承したrunの70548） |
| reviewの差し戻しの修正 | 8: reviseの6（65328・65758・68818・68987・69534・69658）と、人の`send_back`の後のresume 2（ask 339の68544、ask 349の69290） | 9（+70025、70018のrevise） |
| 衝突 | 2（67085: 67005の`landing_recheck_failed`、69793: 69791の`conflict_precheck`） | 2。70322の`conflict_precheck`はturnを送らずaskにし、継承したrunの70548のturnが衝突の解消だけを行った（70522の`triage_decided`）ので、原因で数えれば衝突は3 |
| e2e | 1（69970: 69955の`run_e2e_failed`） | 1 |
| receiptの促し | 0（`validation_finished`の拒否は0件） | 0 |
| worker質問の答え | 0 | 0 |
| provider/hostの障害 | 0（失敗した`turn_finished`は0件。68841の`submit_not_started`の止まりは68925で自然に解け、turnは増えていない） | 0 |

人が要った判断（turnとは別に数える）: 歴史的な観測窓は`approve_landing`の2件（ask 339: 2026-10-02T16:45:25Zから22:04:06Zの5時間18分41秒、ask 349: 1分44秒）。
測定時点の累積は5件で、ask 355（`approve_landing`、理由の無い`send_back`）と`decide`の356・357（resumeの上限による`resume_exhausted`の復旧job）が足され、人待ちは合計5時間25分12秒。
復旧jobは歴史的な観測窓で1回（68842、止まりの確認）、累積で4回。
費用は歴史的な観測窓で$56.13・出力token 314,869、taskの累積で$61.08・出力token 369,837。
元条件の達成（taskのcompleted）までは最初のclaimから10時間23分3秒で、うち人待ちが5時間25分12秒。
claimの前のplan reviewの差し戻し2回（607・609。612でready）は計画の段のもので、runの手戻りに数えない。

同じ受け入れ条件への手戻り: reviewのeventは指摘がどの条件に当たるかを持たないので、#1405だけ、`review_finished`の`reasons`の文面を最終の受け入れ条件（event 64101の`to`）の項目に手で対応づけた。
1つの指摘が複数の項目に当たれば両方に数える。

| 項目 | 歴史的な観測窓の非pass review | 測定時点で足されたもの |
| --- | --- | --- |
| A1 workspaceなしで最初のsession・revise・resume・答えまで進む | 0 | 0 |
| A2 supervisorの停止と起動を越えてturnが続きadoptされる（pidと起動時刻の識別） | 4（65320・68811・69528・69651） | 0 |
| A3 止める全ての経路の後にwrapperとturnのprocessが残らない | 4（65320・65751・68811・69528） | 1（70018） |
| A4 wrapperの入口と登録がterminalとworkspace IDを待たない | 0 | 0 |
| A5 設定がworkspaceのときは今までどおり（backgroundとworkspaceのsessionが混ざる場合） | 2（69528・69651） | 1（70018） |
| A6 integration testが確かめる | 2（65320・68811） | 0 |
| 条件の文面の外の設計の規則（run dirのI/O、ADR-t1404-1決定10のstatsの生死判定） | 3（65818・68981・69064） | 0 |
| repositoryの規則（receiptのstressの根拠） | 1（68811） | 0 |

繰り返した指摘はA2とA3に集まり、その中身はどちらもwrapperとturnのprocessの識別（pidと起動時刻、sessionの境界）だった（65320・69528・69651、statsの68981・69064も同じ仕組み）。
A2とA3は同じ識別の仕組みに依存し、残る条件は独立していなかったので、条件ごとに分けても繰り返した指摘は一方の子に残る。
#1405は回数では長期化していたが、分割より、識別の仕組みを1つの作業として直すことが残作業の中身で、衝突2回とe2e 1回のturnはscopeの大きさと関係しない。

### 比べる集団と窓

- 集団: `run_claimed`の`worker_mode`がheadlessで、2026-10-01T00:00Zから2026-10-04T00:00Zの間にclaimしたrun（claimのevent 55037〜79472）。
  人が開いたrun（interactive、49件）は除く。
- 終わり: 2026-10-06T00:00Zより前のevent（最後は106950）で終わりを見た。
  145件の全てが終わっている（着地139、readyに戻った5、cancel 1）。
- provider: Claude 144、Codex 1。
- 短い正常run: 着地し、turnが1つで非passのreviewが無いrun（55件）。
- 下の表の時間・人待ち・費用・出力tokenは、`runs.csv`の`population`がTrueの行の`claim`から`ended_at`まで・`human_wait_secs`・`cost_usd`・`output_tokens`の各群のp50 / p90（`measure.py`の`pct`の最も近い順位）。
- 費用は`cost_missing_turns`が0のrunだけ（集団140件、短い正常run 54件、候補16件）、出力tokenは`tokens_missing_turns`が0のrunだけ（集団141件、短い正常run 55件、候補16件）で取り、欠測のあるrunを下限の値として混ぜない。

runの単位の表（検知の根拠に使う。効果の比較の基準値は下の「taskの累積の基準値」）:

| 値 | 集団145件 | 短い正常run 55件 | 検知の候補18件 | #1405（歴史的な観測窓） |
| --- | --- | --- | --- | --- |
| turn p50 / p90 / 最大 | 2 / 4 / 13 | 1 / 1 / 1 | 5 / 8 / 13 | 12 |
| claimから終わりまで p50 / p90 | 0.68h / 2.62h | 0.51h / 1.11h | 3.88h / 8.03h | 9.39h（窓の締めまで） |
| 人待ち p50 / p90 | 0 / 67分 | 0 / 0 | 117分 / 350分 | 5時間20分 |
| 費用 p50 / p90 | $4.3 / $11.3 | $2.5 / $5.7 | $11.8 / $23.1 | $56.13 |
| 出力token p50 / p90 | 38,261 / 95,569 | 19,513 / 58,260 | 101,487 / 175,282 | 314,869 |

turnの原因別の合計（145件、335 turn）: 最初のprompt 145、reviewの修正140、衝突37、検証4、provider/host 6、e2e 2、worker質問の答え1、receiptの促し0、`unlabeled` 0。
最初のprompt以外のturn 190のうち、衝突・検証・e2e・provider/hostは49（26%）で、35件のrunにある。
総turnだけを見るとこれらがreviewの修正と混ざるので、検知はreviewの非passだけで数える。

| change | run | turn p50 / p90 / 最大 | 検知の候補 |
| --- | --- | --- | --- |
| feature | 40 | 2 / 5 / 13 | 8 |
| fix | 32 | 1 / 2 / 8 | 1 |
| docs | 23 | 2 / 5 / 7 | 4 |
| test | 18 | 1 / 3 / 4 | 1 |
| refactor | 10 | 1 / 4 / 4 | 1 |
| config | 7 | 1 / 2 / 3 | 0 |
| measure | 7 | 2 / 3 / 3 | 0 |
| 無し（changeの導入前のtask） | 8 | 3 / 3 / 7 | 3 |

areaは`dagq stats`がrunの着地した差分から決めるので、1つのrunが複数のareaに入り、着地しなかったrun（#1405のrun 42637110を含む6件）は空になる。
着地しなかったrunは、同じtaskの着地したrunのareaで読む（#1405はdocs・runtime・tests）。

| area | run | turn p50 / p90 / 最大 | 検知の候補 |
| --- | --- | --- | --- |
| docs | 120 | 2 / 4 / 8 | 17 |
| tests | 92 | 2 / 4 / 8 | 12 |
| runtime | 88 | 2 / 4 / 8 | 12 |
| plugin | 28 | 2 / 5 / 8 | 5 |
| migrations | 6 | 1 / 3 / 5 | 1 |
| config | 6 | 1 / 2 / 2 | 0 |
| ci | 5 | 2 / 3 / 3 | 0 |
| other | 1 | 1 / 1 / 1 | 0 |
| 空（着地しなかったrun） | 6 | 2 / 4 / 13 | 1 |

### taskの累積の基準値

効果の比較は導入前も導入後もtaskの累積で行い、集計の範囲を揃える。
上のrunの単位の表は検知の根拠で、効果の比較には使わない（#1405の候補のrunは継承の前のrunだけで、復旧jobも含まないため）。

- 対象のtaskの選び方と集計を分ける。
  対象は、claimの窓（2026-10-01T00:00Zから2026-10-04T00:00Z）にheadlessのrunをclaimしたtask（`tasks.csv`の1行）。
  集計は、そのtaskの全てのrunと、`task_id`がそのtaskの`session_closed`のうち最初のclaimからtaskの終わり（completedかcanceled）までのものを足す。
  全てのrunは、claimの窓より後でも観測の終わり（2026-10-06T00:00Z）より前にclaimしたrun（継承retryと次のrun）と、headless以外のrunを含む。
  runの単位の表（`runs.csv`の`population`がTrue）はclaimの窓のheadlessのrunだけで、それ以外のrunはそのtaskの累積にだけ足す（`runs.csv`の`population`がFalseの行、`tasks.csv`の`runs_outside_population`）。
  この窓では、そのようなrunは無かった（2つのrunを持つ5件のtaskは、どちらのrunもclaimの窓のheadlessのrun）。
  claimの窓を2026-10-02から2026-10-03に狭めて流すと、#1405の継承したrun `05efd4f3`（2026-10-03T01:20:39Zのclaim）は集団の外になるが、taskの累積には入り、2 run・14 turnになる。
- 観測の終わりまでにtaskが終わらなければ、`tasks.csv`の`end`は`open`で、時間は欠測とする（その時点までのrunとsessionは足す）。
- 除くtask: 窓の前にもrunを持つ11件（「欠測」）。
  残る129件はどれも観測の終わりまでに終わった（completed 128、canceled 1）。
- 時間: 最初のclaimからtaskの終わりまで。
- 費用の成分: `session_closed`の`kind`で分ける。
  worker（`worker`・`revise`・`resume`のsession）、reviewのjob（`review`）、復旧job（`triage`）、計画（`runtime_planner`・`plan_review`・`planner`）、その他（導入前の範囲では0）。
  成分ごとにsession数、出力token、`cost_usd`を出し、値の無いsessionの数を別の列に残す。
- 成分ごとの値を持つかどうか（導入前の129件のsessionで数えた）:

| 成分 | session | 出力token | `cost_usd` | 比べ方 |
| --- | --- | --- | --- | --- |
| worker | 302 | 297（5は欠測） | 296（6は欠測） | 出力tokenと費用を、欠測の無いtaskだけで比べる |
| reviewのjob | 306 | 130（176は欠測。Codexのjobはtokenを残さない） | 全て欠測 | session数で比べる。出力tokenは欠測の無いtaskだけの参考値で、費用は比べない |
| 復旧job | 12（8件のtask） | 12 | 全て欠測 | session数と出力tokenで比べる。費用は比べない |
| 計画 | 0 | ― | ― | 下の「効果の指標」の再計画のplannerとplan reviewを参照 |

- taskが始まる前の計画（最初のclaimの前のplannerとplan review。#1405ではproposal 490のplan review 607・609・612）は、導入前も導入後もこの範囲の外で、比べない。
  plan reviewの`session_closed`はproposalの1つのtaskの`task_id`だけを持ち、taskごとに分けられないためでもある。
- 短い正常task: 1つのrunが1つのturnで非passのreviewなく着地したtask（46件）。
- 検知の候補のtask: 候補のrunを持つtask（16件。候補のrun 18件のうち2件は窓の前にもrunを持つtaskで除いた）。

| 値（p50 / p90。runの数の行は件数） | 集団129件 | 短い正常task 46件 | 検知の候補のtask 16件 | #1405 |
| --- | --- | --- | --- | --- |
| runの数が2以上のtask | 5 | 0 | 2 | 2 run |
| 最初のclaimからtaskの終わりまで | 0.71h / 2.73h | 0.61h / 1.11h | 4.26h / 10.38h | 10.38h |
| 最初のclaimから元条件の達成まで（completedのtaskだけ） | 0.71h / 2.73h | 0.61h / 1.11h | 4.26h / 10.38h | 10.38h |
| turn | 2 / 4 | 1 / 1 | 6 / 8 | 14 |
| 人待ち | 0 / 67分 | 0 / 0 | 134分 / 365分 | 325分 |
| workerのsession | 2 / 4 | 1 / 1 | 6 / 8 | 13 |
| workerの費用（欠測の無いtask） | $4.51 / $11.29（124件） | $2.90 / $6.06（45件） | $11.76 / $23.06（13件） | $61.08 |
| workerの出力token（欠測の無いtask） | 40,867 / 101,487（125件） | 22,650 / 58,260（46件） | 101,487 / 149,655（13件） | 369,837 |
| reviewのjobのsession | 2 / 4 | 1 / 1 | 6 / 9 | 13（tokenは全て欠測） |
| reviewのjobの出力token（欠測の無いtask、参考値） | 14,713 / 41,596（52件） | 6,550 / 11,440（25件） | 53,340 / 61,460（6件） | 欠測 |
| 復旧jobのsession | 0 / 0 | 0 / 0 | 0 / 3 | 3（出力token 2,053、費用は欠測） |

#1405の列は測定時点の窓のtaskの累積（run 42637110と継承したrun `05efd4f3`）で、歴史的な観測窓の値（「#1405の窓」）とは別。
復旧jobはsessionの数で、`recovery_requested`の4回のうちsessionを開かずに終わった1回（68842、止まりの確認の`wait`）を含まない。

### 候補検知の初期値の根拠

- reviewの非passの連続の最長（runごと）: 0が61、1が43、2が23、3が9、4が7、5が1、8が1（#1405）。
  連続3回以上は18件（12%）で、これを候補にすると#1405はevent 65818（2026-10-02T16:45:20Z、3回目の非pass）で候補になる。
- 同じ`primary_code`の連続3回以上は6件で、#1405（同じcodeの連続は最長2回）を含まない。
  `primary_code`の一致を条件にしない根拠はこれ。
- 候補18件のうち17件は同じrunで着地し、候補の後の非pass reviewは0回が9件、1回が6件、2回が2件、#1405だけが6回だった。
  候補の時点で保留すると17件は不要な保留（誤検知）になるので、候補は読み取りの診断だけにする。
- 継続の後の上限: 候補の後の非passが2回までなら#1405のほかは全部が収まり、#1405は3回目のevent 69064（2026-10-02T23:16:01Z）で再診断になる。
  そのとき以降の非pass 3回（69528・69651・70018）、衝突とe2eの対応、resumeの上限による継承retryと人の判断2件（ask 356・357）の前に診断が入る。
- 「同じ診断上の残作業が2回続けて減らない」は、残作業を記録するのが診断で、導入前のeventに無いので基準値を出せない（欠測）。
  初期値は設計の値のまま置き、導入後に診断が記録した残作業から、この条件で候補になったrunと非passの連続で候補になったrunを分けて数え直す。
- 総turnを条件にしない根拠: turn 5以上の12件のうち11件は同じrunで着地した。
  #1405を除く11件の最初のprompt以外のturn 59のうち18（31%）は衝突・検証・e2e・provider/hostで（f4b8ea86・6c6542c9・18bcb9c4は衝突が3）、scopeの大きさを表さない。

### 欠測

- 受け入れ条件の項目ごとの手戻りは、reviewのeventが項目を持たないので#1405のほかは数えていない（0ではなく欠測）。
- 費用は5件のrunに`cost_usd`の無いturnがあり（Codexのrun 1件と、Claudeの失敗したturnを持つrun 4件）、出力tokenは4件のrunにtokenの無いturnがある。
  これらのrunの値は下限なので費用と出力tokenのp50 / p90からは除き、欠測のturn数をCSVの別の列に残した。
- taskの累積: 窓の中で2つのrunを持つtaskは5件（1405: 13 + 1、1334: 4 + 4、1424: 2 + 2、1421: 2 + 1、1428: 2 + 1）。
  集団のtaskのうち11件（251・648・655・724・774・793・1087・1095・1115・1144・1188）は窓の前にもrunがあり、taskの累積の基準値から除いた（`tasks.csv`の`excluded_earlier_runs`）。
  claimの窓の後（2026-10-04以降）に集団のtaskのclaimは無い（2026-10-06以降も無いことを`run_claimed`で確かめた）。
- taskの累積の成分の欠測は「taskの累積の基準値」の表のとおりで、reviewのjob（Codex）の出力tokenと全てのjobの`cost_usd`は記録が無い。
  0として足さず、欠測のsession数を`tasks.csv`に残した。
- receiptの促し: `validation_finished`の拒否は7件のrunにあるが、どれも同じrunへの促しのturnにならず、復旧jobか次のrunに渡った（3件はbaseから外れたcommit、1件はcommit無し、3件は失敗したturnの後）。
  そのturnは復旧の原因（provider/hostか次のrun）で数え、receiptの促しは0件と記録した。

### 効果の指標と導入後の比べ方

指標（taskの累積で、置換したtaskは元と、再置換を辿った中間と葉の全ての子を足す）:

- 総turnと原因別のturn（上の数え方）、reviewの非passと、受け入れ条件の項目ごとの手戻り（再計画の診断と対応表が項目を持つので導入後は全件で数える）。
- 総時間（最初のclaimからtaskの終わりまで）と、元条件の達成までの時間を分けて並べる。
  終わりは、置換しないtaskはcompletedかcanceled、置換したtaskはその下の葉が全て終わった時刻。
  元条件の達成は、置換しないtaskはcompleted、置換したtaskは対応表の必要な子（再置換された子はその下の葉）が全てcompletedになった時刻で、cancelで終わったtaskは達成までの時間に入れない（`measure.py`の`achieved_hours`）。
- 人待ち（上の数え方）と、人に上げた判断の件数。
- 費用: 「taskの累積の基準値」と同じ範囲と成分で数える。
  置換したtaskは、元taskと再置換を辿った全ての子task（中間のtaskを含む）の範囲のsessionを足す。
  導入で増える成分は、診断のjobと、再計画の操作のrequestとproposalのplannerとplan reviewで、操作に結んで数える。
  導入前には再計画の操作が無いので、この成分の導入前の値は0で、欠測ではない。
  診断のjobや再計画のplanner・plan reviewのsessionは、`kind`が`triage`や`runtime_planner`・`plan_review`でも、再計画の操作に結んだものは復旧jobと計画の成分から外し、導入で増えた成分にだけ数える（二重に数えない）。
  導入後の合計は単位ごとに分け、費用（USD）はworkerの費用、出力tokenはworkerと復旧jobの和、session数はreviewのjobと復旧jobの和として、導入で増えた成分とは別に並べる。
  両方で欠測の多い成分（reviewのjobの出力tokenと費用、jobの費用）は合計に入れず、session数と欠測の数だけを並べる。
- 誤検知: 候補になって継続を選び、追加の非pass 2回以内に同じrunで着地したもの（保留しなかったので費用は診断だけ）と、保留した後に`continuing_after_park`か`withdrawn`で閉じたもの（保留と復元の費用を払った）を分けて数える。
- 成果の破棄: 保存した成果を採用しなかった件数と、その権限ある判断の有無。
  判断の無い破棄は0件でなければ欠陥として扱う。

比べ方:

1. 再計画の診断と保留が本番の固定バイナリに入った時刻（`dagq marks`の`derived:dagq_version`）の後に、同じ長さ（3日のclaim、2日の観測の猶予）の窓を取る。
   対象のtaskはclaimの窓で選び、そのtaskの観測の終わりまでの全てのrun（猶予の間の継承retry・次のrun・置換した子のrun）を累積する。
   観測の終わりに終わっていないtaskは時間を欠測とし、導入前と同じく件数を並べる。
2. 操作の結末を`dagq replans --all`で読み、置換ごとに元taskと対応表が必要とする子を`--replaced 元:子,子`で`measure.py`に渡す。
   再置換された子は、その子を元とする`--replaced`をもう1つ渡し、引数の順は問わない。
   `measure.py`は置換の関係を木として辿り、根の元taskの行に、中間のtaskと葉の子のrunとsessionを全て足す（子が猶予の間に初めてclaimされても観測の終わりまで集める）。
   木を数えるのは根がclaimの窓で選んだtaskのときだけで、根が猶予の間に初めてclaimされた木は行を足さない。
   根が窓の前にだけrunを持つ木は、窓の中の子を1つの行にまとめ、窓の前にrunを持つtaskと同じく除く。
   置換したtaskの終わりは、その下の葉が全て終わった時刻（最も遅い葉の終わり）で、葉が全てcompletedなら`replaced`、cancelされた葉があれば`replaced_child_canceled`とし、終わっていない葉があれば`open`（時間は欠測）とする。
   置換したtask自身の`replaced`の状態は終わりに数えないので、子を渡し忘れた置換、空の子の並び、runの無い葉は`open`として残り、終わったように見えない。
   置換の関係が循環していれば`replacement_cycle`とし、終わりに数えない。
   2つの元に同じ子を渡した引数と、同じ元を2度渡した引数は拒む。
   この扱いは`test_measure.py`（`python3 -I docs/plans/task-replanning/test_measure.py`）が確かめる。
   木の形（直列・分岐・取消・未完・循環・窓の外の根）は両方の引数の順で、ほかに中間のtaskのsessionの範囲と、拒む引数を確かめる。
3. `measure.py`を同じ引数の形（と2の`--replaced`）で流し、同じ数え方で`runs.csv`と`tasks.csv`を作る。
   runの単位の値は`runs.csv`の`population`がTrueの行だけで出す。
4. changeごとに、taskの累積の基準値と同じ列（turn・時間・人待ち・成分ごとの費用とsession数のp50 / p90、候補の数）を並べる。
   taskのchangeは`runs.csv`のそのtaskのrunのchangeを使い、areaは着地したrunのareaを使う。
   件数の少ないchangeは件数だけを並べ、比率を出さない。
5. 候補の数・診断の数・保留の数・置換の数を分け、候補あたりの保留の割合を出し、候補のtaskの総時間・成分ごとの費用を導入前の検知の候補のtask 16件（runの単位の候補18件ではない）と比べる。
   保留したtaskだけを取り出すときも、導入前の側は候補のtaskと同じ単位で並べ、保留の有無で分けられないことを書く。
6. 衝突・e2e・検証・provider/hostのturnの増減は再計画の効果に数えず、別に並べる（それらを減らすのは再計画の目的ではない）。
7. 導入前と後で、providerの組成とreviewの規則（差し戻しの回数の扱いなど）が変わっていれば、その変化の時刻と件数を並べ、効果をそれと分けて読めないときはそう書く。
