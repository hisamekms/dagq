---
id: design-supervisor-lifecycle-plan-review
type: design
title: "Plan review (supervisor)"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1985-1
  - adr-t1704-1
  - adr-t1566-1
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-0041
  - adr-0044
  - adr-0079
  - design-supervisor-lifecycle-actor-model
  - adr-t947-1
  - adr-t1300-1
  - adr-t1504-2
  - adr-t1981-1
  - adr-t1971-1
  - adr-t1975-1
---

# Plan review (supervisor)

execの[引き継ぎ](handoff.md)を待つあいだに終わったjobと、`prepare_handoff`の時点で終わっていたjobは、捨てずに回収してverdictを適用する（新しいjobは始めず、verdictでreadyになったtaskも待ちのあいだはclaimしない）。
走っていて止めたjobは、引き継いだsupervisorが最初のpassに入る前に自分のtokenの未完了行を`interrupted`で閉じる。
候補が無くても閉じ、既存のclose経路で区間の`session_closed`を記録する。
他のtokenの行は触らない。
候補が残れば以後のpassでreviewをやり直し、interruptedはattemptに数えない。

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定10〜15・17。
submitされたproposalを、supervisorが起動するheadlessのplan review jobが検査し、そのverdictと人のanswerをruntimeが適用する。
use caseは`src/application/supervise/plan_review.rs`、storeは`src/infrastructure/plan_reviews.rs`（port `PlanReviewStore`）、verdictの型と順序は`src/domain/plan_review.rs`。
run slotは使わず、observerと同じくloopの各passで進める。
`--once`のloopは走っているplan reviewを待ち、passでverdictかanswerを適用したらもう1 pass回ってreadyになったtaskをclaimしてから終わる（`--once`で立てたplannerは終わらせない）。
drain中（`stop`、provisioningの失敗）は新しいplan reviewを始めず、reviseの配送もしない（走っているjobとanswerの適用は続ける）。

1. **記録**: jobは`plan_reviews`の1行で、欄の意味はmigrationのコメントが持つ。
   未完了の行は部分unique indexで1つまでなので、queue全体で同時に1つ。
   proposalは保留（plan reviewが自分では取り直さない）・plannerに渡す指摘・reviseの配送と時間切れの記録を持ち（`migrations/0027_plan_review.sql`のコメント）、`submit`（出し直しを含む）はこれらをすべて消す。
   eventはrun_eventsがtaskかgoalを要るので、proposalの最初のtask（ID最小）に付けて記録する。
2. **順序**（`next_to_review`）: 対象は`submitted`で保留が無く、`submitted`のtaskを持つproposal。
   `interrupt`の優先度のsubmittedのtaskを持つものが先、次にsubmitの古い順、次にID順。
   gateは飛ばさない。
3. **開始**（`begin_plan_review`、`BEGIN IMMEDIATE`）: 未完了の行があり、そのsupervisorの登録が生きていれば始めない。
   そうでない行は`interrupted`で閉じる（supervisorが死んだjobの引き継ぎ）。
   対象であることを再検査し、行を作ってjobのdirを決め、`plan_review_started`を記録する。
   起動のmodel / effortとその出どころは[Actor model](actor-model.md)の役割の表から決め、区間の`session_opened`にも写る。
   proposalの特徴は書き込みのロックの前に読み、読めなければ無しとして記録する（規則は[kpi](commands.md)）。
   promptをjobのdirに書き、`AgentProvider::headless_command`（権限の意図`PLAN_REVIEW_ACCESS`）をrepositoryのcheckoutで起動する。
   promptはargvでなくstdinで渡す（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)の「promptの渡し方」）。
   jobは`plan-review-job`の役割で、queue serviceのクライアントとして読むコマンドだけを打つ（[Queue service](../queue-service.md#クライアントモード)）。
   timeoutは`AgentProvider::review_timeout`。
4. **prompt**（`plan_review_prompt`）: 最初にrepositoryの指示（AGENTS.md / CLAUDE.md）と、それが名指す文書と規則（とくにplan review向けの記述）、taskが名指す文書を読んで反映せよと指示する。
   verificationの規則や、ADRの索引・IDの形などrepositoryの記録の規則をruntimeに埋め込まない（dagqのrepositoryではAGENTS.mdの「plan review」の節から[taskの登録](../../development/task-registration.md)の「plan reviewが当てはめる規則」へ辿る）。
   AGENTS.mdが無いrepositoryでは、taskのverify・paths・evidenceをCLAUDE.md → README・CIの設定・buildの設定の順で判断し、どれでも決まらなければ人が要るので`concern`にさせる（[Prompt](prompt.md#repositoryの規則を読む順)）。
   矛盾の検査は「ADR」ではなく「repositoryが記録した決定（指示とそれが名指す決定の記録）」との矛盾と書く。
   jobは`dagq`の読むコマンドを打て、書き込むコマンドはCLIが役割で拒む（reviewと同じ形）。
   promptはそれと、記録を読むCLI（`prompt::RECORD_READING`、ADR-0044の決定22）を示す。
   promptはdagqのADR番号を含まない。
   材料は、proposalのtask（全field、依存、goalへの依存）、関係するgoal（constraintsがtaskのdescriptionより優先）、`dagq lint`の結果、他の`submitted` / `revising`のproposalとそのtask（このproposalより前に出されたか後か）、`ready` / `in_progress`のtaskの要約、proposalの各taskの予想するファイル、人が答えたask（先例の候補）、衝突の多いファイル、proposalの各taskの重複と実装済みの候補。
   出どころが`follow_up`のtaskには、登録時の材料（workerの所属の提案を含む。[follow_upsの所属の提案](receipt-and-session-exit.md#follow_upsの所属の提案)）と所属の判断の全ての行、最新の判断の版が今の版と一致するかを足す（[ADR-t1504-2](../../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定7）。
   関係するgoalは、taskの今のgoalとproposalのgoalに、follow_upの元goal（登録時の材料か判断の行が名指すもの）を足したもの。
   description・acceptance・contextなどの全文は、重複の候補に載ったtaskと、proposalのtaskと同じhotspotを触ると予想されるtaskにだけ別の節に付け、要約に全文が下にあることを示す。
   衝突の多いファイルはsupervisorが`[conflicts]`の設定と`Repository::main_history`で数え、renameされたものは今の名前で載せ、各ファイルに触ると予想されるproposalのtaskと`ready` / `in_progress`のtaskを並べる。
   重複と実装済みの候補は、`dagq related`と同じ関連の強いtask（状態を問わない）と、`dagq search`と同じ検索でtaskのtitleの語を引いたtaskと着地したcommitで、proposal自身のtaskとそのcommitは除く。
   searchのqueryは`domain::search::any_word_query`がtitleの語をORに組み、空白で区切らない日本語もtrigramで引く（ひらがなだけのtrigramは助詞や語尾なので除く）。
   `dagq search`のCLIのqueryの解釈は変えない。
   重さの予測（ADR-0079の決定2）として、verdictに関わらずproposalの`submitted`のtaskごとに予測を出せと指示し、対象のtaskのIDと目安（スパイクの`scripts/spikes/predictor-replay/predict.py`のもの）を書く。
   予測のために別のmodelは呼ばない。
   **予想するファイル**は[claimを控える](claim-defer.md#判定の入力)と同じ規則と関数（ADR-0080の決定1・2、ADR-t1981-1）で求める。
   proposalのtaskは`paths`が変わりうるので読み直し、hotspotに当たるかの`touches`はglobのまま。
   意味の検査として、次を挙げる。
   - 重複と実装済み: 候補を手がかりに、completed・canceledのものも見てLLMが判断する。
     明らかな重複だけを`cancel_duplicate`にし、疑わしいもの・実装済みに見えるものは`concern`。
   - repositoryが記録した決定・constraintsとの矛盾。
   - **acceptanceとdescription・兄弟taskのacceptanceの食い違い**（型を変える変更なのに「tests/cli.rsは変えない」など）。
   - 依存: 中身の前提のときだけ。
     同じファイル・hotspotを触ることだけでは足さず[claimの控え](claim-defer.md)に任せる。
     優先度の高いgoalのtaskが低いgoalのtaskを衝突回避だけで待つか理由が無ければ`revise`（ADR-t1985-1）。
   - `ready` / `in_progress`のtaskと一部だけ重なるtask（片方の範囲を削れば済むもの）は`pass`にせず`revise`にし、どの範囲を削るか・どちらに寄せるかをreasonに書く（ADR-0044の決定11）。
   - 先に出されたproposalとの食い違い（後に出されたものとの食い違いならこちらは通す）。
   - 変える必要のあるreadyのtask（`reopen`）と、`in_progress`のtaskは直さず着地後に直すtaskを求める`revise`。
   - follow_upの所属: plannerの対応づけを起点に元goalのacceptanceに照らして確かめ、全面の調査を独立にやり直すことは求めないが形式だけでpassにしない。
     理由がacceptanceの項目を名指さない、証拠がreceiptや差分と食い違う、所属先が無関係な巨大なgoal、acceptanceを弱めて範囲外にした、版が一致しない、のように疑わしいものは周辺の証拠も読んでから決める。
     plannerが直せる対応づけの誤りは`revise`、follow-upを外すためのacceptanceの弱めは人の意図が要るので`concern`。
   - lintの指摘。
   同じ種類の指摘に人が答えたaskがあれば、そのIDを`precedents`に入れ、reasonに人の答えを書く。
   promptの渡し方・節ごとと全体の上限・選ぶ順・省いたことの明示・byte数の記録の方針は[Prompt](prompt.md#headlessのjobのprompt)の「headlessのjobのprompt」の節が持つ（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）。
   **上限と選ぶ順**: `plan_review_prompt`は`PlanReviewPrompt`（promptの文と`PromptBytes`）を返し、promptの節を必須の節と省いてよい節に分けて上限の中に収める。
   上限と件数の値と理由は各定数のdoc commentと[Prompt](prompt.md#plan-reviewの上限)が持つ。
   promptは由来を各taskとgoalの行から読ませ、人の出どころの優先度と人間由来（`unknown`を含む）のtask・goalは優先度と所属を変えず疑いは`concern`（`scope`）、AI由来のtaskは新しいgoalの優先度と既存のgoalへの所属（受け入れ条件に要るか）を見て`revise`にさせる。
   - **必須の節**（指示と検査とverdictの形、proposalのtask、その予想するファイル、goal、lint、最後の言語の指示）は省かない。
     合わせて必須の節の上限を超えるときだけ、黙って切らずに、大きいものから順に、読むコマンドを添えた1行の代わりに替えて収める。
     代わりの行だけでも収まらなければ、task・予想するファイル・goalの節をそれぞれ1行の案内に替え、重さの予測の対象のIDの並びも案内にする。
     替えたことと理由はtaskの節の頭に書き、`PromptBytes`に残す。
     読む経路はplan reviewのjobの今の権限で打てる読むだけの`dagq`のコマンドに限り（ADR-t1566-1決定3）、jobのdirのファイルには退避しない（Claudeの`Read`がrepositoryの外のjobのdirを読めるかに頼らない）。
   - **省いてよい節**（CLIで読み直せるもの）は、残す順に衝突の多いファイル、重複と実装済みの候補、他のproposal、人が答えたask、全文、要約。
     どれも自分の上限と、全体の上限から必須の節と前の節が使った残りの小さい方に収まる行だけを順に載せ、収まらない行は飛ばして次の行を試す（1つの巨大な行が残りを塞がない）。
     省いてよい節の注記の分を全体の上限から先に空けておくので、promptは全体の上限を超えない。
   - **全文の選ぶ順**: 全文の候補を、proposalのtaskが依存するもの → proposalと共有するhotspotの数の多い順 → proposalのtaskの予想するファイルと重なる数の多い順（`claim_defer::touches`をどちら向きにも当てる） → 重複の候補の中の位置の良い順 → IDの新しい順に並べ、上限まで載せる。
     節もこの順に並ぶ。
   - **要約の選ぶ順**: 全文の候補を上の順に先に、残りをIDの新しい順に選び、載せる行はIDの新しい順に並べる。
     全文が下にある印は全文の節に実際に載ったものだけに付く。
   - **省いたことの書き方**（ADR-t1566-1決定5）: promptの冒頭で、上限で切った一覧はその件数と読むコマンドを書くと伝え、各節の末尾に省いた件数と読む方法を書く。
     読む方法の`dagq`のコマンドは`prompt::PLAN_REVIEW_READS`に並べ、plan reviewのjobの役割がコマンドラインでもqueue serviceのクライアントとしても打てる読み取りであることをtestが確かめる。
   - **byte数の記録**（ADR-t1566-1決定6）: jobを終える`plan_review_finished`と`plan_review_failed`に`PromptBytes`を記録する。
     promptを組み立てる前に失敗したとき（jobのdirを作れない・材料を読めない・行き先が人になる）は記録しない。
5. **verdict**（`PlanReviewVerdict`、全体か最外の`{...}`、未知のフィールドは拒否）: `pass` / `revise` / `concern`と、理由（[差し戻しの分類コード](#差し戻しの分類コード)）・actions・`reopen`・先例・重さの予測、`concern`の推奨（[AIが決めるconcern](#aiが決めるconcern未実装)）を持つ。
   形は`PlanReviewVerdict`の定義が持つ。
   予測は未知のフィールドの拒否の対象外で、どんな形でもverdictの解釈を失敗させない（生のJSONのまま受け、6で検査する。`domain::prediction::parse_predictions`）。
   actionsは依存の追加・優先度の引き下げ・重複のcancelだけで、`pass`のときだけ適用する。
   優先度の引き下げとpassでの優先度は各taskの由来と優先度の出どころで扱いが分かれる（6の`pass`）。
6. **適用**（`finish_plan_review`、1トランザクション）: 行がまだ自分の未完了の行でなければ何もしない。
   proposalがもう`submitted`でないか保留されていれば、行を`interrupted`で閉じ`plan_review_discarded`だけを記録する。
   jobの開始より後にproposalのいずれかのtaskに`task_edited`があれば（[ADR-0041](../../adr/0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)の決定9。eventのIDで前後を比べる）、verdictはpass / revise / concernのどれも古い内容へのものなので適用しない。
   行を`interrupted`で閉じて（verdictは残す）、`plan_review_discarded`を記録する。
   proposalは`submitted`で保留もないままなので、次のpassで編集後の内容をplan reviewにかけ直す（`interrupted`の行は`attempt`に数えない）。
   検査中でないsubmittedのtaskの編集と、他のproposalのtaskの編集は、verdictの適用を妨げない（次のplan reviewが新しい内容を読む）。
   runtimeは`revise`で差し戻しの回数が`MAX_PLAN_REVISES`（runのreviewとは別）以上なら`concern`にする。
   先例のaskは人の答えを添えた行にして、reviseの指摘の後ろとconcernのaskのquestionに足す。
   - `pass`: 由来は文面でもproposalでもなく、各taskの作成時の記録（`origin`）と優先度の出どころ（個別の優先度はその設定者、無ければgoalのもの）で決める（ADR-t1975-1の決定7。記録は[Plan planners](plan-planners.md)の「inboxからの計画の依頼」）。
     優先度の引き下げは、`origin`が`human`か`unknown`のtaskと、効いている優先度の出どころが`human`のtaskには適用せず、verdictも失敗にしない。
     残りのactionを先に検査し（proposalの`submitted`のtaskであること、依存先が存在し自分でないこと、優先度は下げるだけ、重複先が`cancel --duplicate-of`と同じ`check_duplicate`を通り、同じverdictで重複としてcancelされるものでもないこと）、外れればverdict全体を失敗（7）にする。
     通れば依存を足し、AI由来のtaskの優先度の引き下げは値を置かず個別の指定を外し（`set-priority --inherit`と同じ経路）、重複をcancelする（`cancel --duplicate-of`と同じ記録）。
     続けて、findingとactionの有無に関わらず、`origin`が`ai`の`submitted`のtaskの個別の優先度のうち出どころが`ai`のものを同じ経路で外す。
     その後proposalを`accepted`、submittedのtaskを`ready`にし、draftのgoalを開く。
   - `revise`: proposalを`revising`（taskは`draft`、差し戻しの回数+1）にし、指摘を置いて配送を待たせる（下の9）。
   - `concern`: 確信度が高く理由の分類の無い`ready` / `send_back`の推奨は、上の`pass` / `revise`として適用する（下の[AIが決めるconcern](#aiが決めるconcern未実装)）。
     それ以外はproposalを`concern`で保留し、`approve_plan`のask（proposalの最初のtask、runなし、questionにsummary・上書きの理由・reasons・先例・promptのpath）を同じトランザクションで作る。
   - `reopen`（verdictに関わらず）: proposalに無く、`submitted` / `revising`の他のproposalにも属さない`ready`のtaskを`submitted`に戻し（`TaskAction::Reopen`。claimされない）、そのtaskだけの新しい`revising`のproposal（持ち主はruntime）に入れ、理由を指摘として置き、`task_reopened`を記録する。
     `ready`でないtask（`in_progress`など）と、検査中・差し戻し中のproposalのtaskは変えず、`plan_review_finished`に理由を残す。
     出し直し（`submit --proposal`）はproposalの`submitted`のtaskもそのまま持って出す。
   - 最後に行を閉じ、`plan_review_finished`を記録する。
     `concern`のverdictなら続けて`plan_concern_decided`を記録する（[AIが決めるconcern](#aiが決めるconcern未実装)）。
   - **重さの予測**（ADR-0079の決定2）: `pass` / `revise` / `concern`のどれでも、適用の前にproposalの`submitted`だったtaskに対して予測を検査する（`domain::prediction`）。
     予測が無いか形や値が読めない、taskの過不足や重なりのどれかがあれば予測は1件も記録せず、理由を`plan_review_finished`に残す。
     verdictの適用はそのまま行う。
     通れば`plan_review_finished`の後に、taskごとに`task_weight_predicted`を記録する。
     そのmodel / effortは`plan_review_finished`が閉じたjobのsessionの`session_closed`のもの。
     出し直しや`reopen`でかけ直したtaskには新しい予測を上書きせず追記し、読む側は最後のものを使う（[stats](commands.md)）。
     検査中の編集で捨てたverdictと失敗したjobの予測は記録しない。
     claimは予測を作らず、workerの起動のmodel / effortも変えない。
7. **失敗**（`fail_plan_review`）: 行がまだ自分の未完了の行でなければ何もせず、proposalがもう`submitted`でないか保留されていれば行を`interrupted`で閉じ、agentが起動していれば6と同じeventを記録する。
   jobの開始より後にproposalのいずれかのtaskに`task_edited`があれば（6と同じ判定。ADR-0041の決定9）、編集後の内容はまだ検査されていないので人に渡さない。
   行を`interrupted`で閉じて保留を付けず、`plan_review_failed`の代わりに`plan_review_discarded`を記録する。
   proposalは`submitted`のままで、次のpassで編集後の内容をかけ直す。
   検査中でない編集（jobの開始の前）と他のproposalのtaskの編集では、下のとおり`failed`にする。
   役割の表に`provider`を書いた役割で、jobのproviderが使えなかった（起動できない・認証・利用上限）ときは、行を`interrupted`で閉じ、保留もattentionも付けず、次のpassでもう一方のproviderでかけ直す（下の[Codexで動かす](#codexで動かす)の「使えないprovider」）。
   jobの入力かsupervisorの環境による起動の失敗はproviderが使えないことに含めない（[Agent provider lifecycle](../provider-lifecycle.md)）。
   `--no-claude`ではClaudeに切り替えないので、次のpassで行き先が無くjobを起動せずに下のとおり`failed`で保留する。
   それ以外で、起動できない、非0終了、timeout、stdoutにverdictが無い、verdictを適用できない（許されないaction、proposal内のtaskの`reopen`）は、行を`failed`で閉じ、proposalを`failed`で保留して`plan_review_failed`を記録する。
   proposalは`submitted`のまま動かさず、supervisorは同じproposalを自分では取り直さない。
   `plan_review_failed`はattention（`plan review by hand`）で、`status`の`attention`にも保留が解けるまで出る（`concern`で保留したのに`approve_plan`のaskが自由記述の回答で閉じられたproposalも同じattentionになる）。
   人の指示で、inboxがtaskをbypassで`ready`にするか、plannerかinboxが`dagq submit --proposal N`を打つ。
   保留中のproposalはtaskとgoalを足さずにそのまま出し直せ（`proposal_resubmitted`、submitの時刻が新しくなる）、plan reviewにかけ直される。
8. **片付け**（`settle_proposals`、各pass）: `submitted`なのに`submitted`のtaskが1つも無くなったproposal（人がbypassで`ready`にした、cancelした）は、jobが走っていなければ、cancelされていないtaskが残れば`accepted`、無ければ`canceled`にし、保留を消して`proposal_settled`を記録する。
   goalとtaskが他のproposalに入れるようになり、attentionも消える。
9. **reviseの配送**（`tend_planners`、各pass）: plannerが終わったかは`planner_view`で決め、wrapperで判定する（sessionが見つからないだけでは証拠にせず、workerと同じ）。
   cmuxのworkspaceに立てた行と人が開いたplannerの行の扱いは[`plan` / `planners`](plan-planners.md)が持ち、runtimeはそれらにcmuxを呼ばず何も送らない（ADR-t1433-2決定3・5）。
   まず差し戻し中のproposalを見て、渡した相手のplannerが終わっていれば`plan_revise_lost`を記録して未配送に戻す。
   終わっていないのに`--planner-timeout`を過ぎても出し直されていなければ`planner_unresponsive`を記録する。
   まだ誰にも渡せていないreviseも、差し戻した時刻から期限を過ぎれば`planner_unresponsive`にする。
   このeventは待っている理由として、上限を埋めているruntimeのplanner（生きているもの）とそれぞれを終わらせない理由を載せる（`PlanningState::planner_holds`）。
   終わらせない理由は`busy_reasons`が決め、空ならidleで終わる番のもの。
   どちらも1回だけで、attentionは`check the planner`、workspaceは閉じない。
   これとは別に、runtimeのplannerは期限で見ず、wrapperがturnを`[stall]`の上限で止めたときに、plannerごとに1回`planner_unresponsive`になる（`tell_of_stopped_planner_turns`、ADR-t1394-2決定3）。
   配送を取ったまま記録の無いもの（supervisorが間で死んだ）は、supervisorの生存の判定と同じ時間の後に未配送に戻す。
   次に未配送のreviseをproposalのID順に配る。
   持ち主のplannerが生きていれば、idleになるのを待ち（Claudeを待つplannerには、続きのturnが終わるまで置かない）、`claim_revise`で取ってから（2つのsupervisorが同じreviseを二重に配らない）指摘を`send_to_planner`で届け、`plan_revise_sent`を記録する。
   届ける依頼は、指摘・先例・`dagq submit --proposal N`で出し直すこと・意図の変わる修正は人に聞くことを持つ。
   runtimeのplannerには打ち込まず、次のturnの依頼として置く。
   非対話でない行には何も打たず、誤りにする。
   生きているsessionのmodel / effortは切り替えない（[Actor model](actor-model.md#差し戻しで開き直すplannerの段上げ)）。
   届けるのが失敗すれば取った配送を戻す。
   持ち主が居ないか終わっていれば、runtimeが立てたplannerで生きているものが`runtime_planners`未満のときだけ（run slotとは別。人が開いたplannerは数えない）、配送を取って`open_runtime_planner`で新しい非対話のplannerを立て、初期promptに指摘を載せる。
   そのplannerのeffortは1段上げる（ADR-0079の決定7の(c)、[Actor model](actor-model.md#差し戻しで開き直すplannerの段上げ)）。
   上限に達していれば待つ。
   上限で待つ持ち主の居ないreviseが期限を過ぎたら、そのpassの終わりに枠を1つ空ける（`release_runtime_planner`）。
   対象は、runtimeのplannerで生きていてidle、終わらせない理由があり（無ければ10が終わらせる）、人の答え待ち・届いていない答え・Claudeを待つことのどれも含まず、最後の動きが期限より古いものの1つ。
   Claudeを待つplannerを解くと、待ちが決めずに終わった回数に入るので外す。
   それに終了の依頼（`turns/exit`）を置き（`/exit`は打ち込まない）、`planner_released`を記録する。
   runtimeのplannerに終了の依頼を置いてまだ終わっていないものがあれば、それが枠を空けるので送らない。
   終わったplannerは10のとおり閉じ、次のpassでreviseにruntimeのplannerが立つ（自分宛てのreviseを持っていたplannerなら、そのreviseは`plan_revise_lost`で未配送に戻り、同じ上限を並んで待つ）。
   上限の外でplannerを立てる形は取らない: idleで何も待たないplannerは仕事をしておらず、残すと上限を埋め続けるため。
   持ち主のsessionが見つからないのにwrapperが生きているときも待つ（期限でinboxに届く）。
   reviseを配った後、同じ上限の残りで、人がinboxに頼んだ計画の依頼のplanner（[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）、runtimeやjobが作ったdraftのplanner（[Draft planners (supervisor)](draft-planners.md#draft-planners-supervisor)）の順に立てる。
   持ち主が人の答え待ちで終わる途中（10）ならその行が閉じるのを待つ。
   持ち主が居なければ、proposalのtaskに未回答の`planner_question`があるあいだは立てず知らせず、answerは指摘と同じ新しいplannerに載せて閉じる（行き先は`planner_answer_route`。ADR-t1704-1決定4）。
   人の答えだけを待つplannerは10で終わって枠を空けるので、ここでは解かない。
10. **runtimeが立てたplannerの終わり**（`end_runtime_planners`）: runtimeが立てたplannerのうち、終わったもの（9のとおり）はbackgroundのwrapperがまだ動いていれば止め、行を閉じて`planner_closed`を記録する（`end_planner`。理由と記録は[`plan` / `planners`](plan-planners.md)の「plannerの終わりの記録」、ADR-t1300-1）。
    runtimeはruntimeのplannerのcmuxのworkspaceを閉じない。
    idleで終わらせない理由の無いものと、人の答えだけを待つ非対話のもの（[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)の「人の答えだけを待つplannerの終わり」）には1回だけ終了の依頼（`turns/exit`）を置き、wrapperがturnを終えてagentの終了が記録された後のpassで上のとおり閉じる。
    終了の依頼から`SessionWrappers::exit_timeout`を過ぎても終わらなければbackgroundのwrapperを止めて行を閉じる。
    `planner_question`を待っているかは`question_wait`で決める（[Draft planners (supervisor)](draft-planners.md#draft-planners-supervisor)の5。reviseのplannerはproposalのtaskのask）。
    答えは`claim_planner_answer`と`ask_delivered`の後にplannerの次のturnの依頼として置き、打ち込まない。
    書けなければ`ask_delivery_failed`を残し人の答え待ちと同じく終わらせて新しいplannerに運ぶ。
    届けた答えを読み終えたかはidle markerと配送を取った時刻の比較では決めず、そのanswerの依頼を取ったturnが壁（Claudeのログイン切れ・利用上限・起動の失敗）で失敗せずに終わったかで決める（判断は`src/domain/turn.rs`の`request_read`）。
    壁で失敗したturnだけでは読み終えたとせず、wrapperが次に取るそのanswerを載せた再試行のturnが壁で失敗せずに終わるまで追う（壁で失敗したturnの後にplannerを終わらせると、answerが読まれずに失われるため）。
    依頼の記録が無ければ配送を取った時刻とplannerのidleの時刻を秒で比べ、配送を取らずに新しいplannerの初期promptに載せた答えは、plannerを立てた時刻と比べる。
    askの`closed_at`は届けた後に書くので比べない。
    人が開いたplanner（廃止前から開いている行）は終了を送らず、同じpassの初めに閉じた扱いにし、そのworkspaceにはcmuxを呼ばない（[`plan` / `planners`](plan-planners.md)の「人のplannerの終わり」、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5）。
    持っていたproposalのreviseは、持ち主のplannerが閉じているときと同じく新しい非対話のplannerへ行く。
11. **answer**（`decide_plan`、1トランザクション）: 回答済み・未closeの`approve_plan`のaskのうち、回答が`PLAN_OPTIONS`のどれか（理由付きの`send_back`を含む）で、askのtaskのproposalがまだ`submitted`で`concern`で保留されているものに適用する。
    `ready`はproposalを`accepted`にしてtaskを`ready`、`send_back`は`revising`にして人の理由と直近のverdictのreasonsを指摘にし（配送は9）、`cancel`はproposalを`canceled`にしてsubmittedのtaskをcancelする。
    askを閉じ、proposalの直近のverdictのある行がconcernなら`plan_review_outcome`（[差し戻しの分類コード](#差し戻しの分類コード)）を、続けて`plan_decided`を記録する。
    proposalが動いていればaskを閉じ、`ask_closed`だけを記録する。
    それ以外の回答は適用せず、runtimeが運ばない答えとしてattentionになる。
    適用できる回答は`status`でruntimeが適用中と出る。
12. **設定**: runtimeが同時に立てるplannerの上限`runtime_planners`（優先順と読み直しは[Run environment](run-environment.md)の`[supervisor]`）と、reviseを待つ期限、runtimeが立てるplannerが読むpluginは`supervise`の`--help`が持つ。
    `up`は自分のplugin dirをsupervisorにも渡す。
13. **test**: 判断は各moduleの単体test、境界は`tests/it/plan_review.rs`ほかの`tests/it/`のtest。
    境界のtestはstubのheadless provider（台本のverdictを印字する）と、runtimeのplannerのwrapperをbackgroundに止めて置くbackend（plannerはtestが演じ、cmuxのworkspaceは開かない）を使う。

## Codexで動かす

goal reviewの仕組み（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1〜6、[Actor model](actor-model.md#provider)）でplan reviewも動き、`--no-claude`との関係はrunのreviewの形（[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)）に揃う。
jobは状態を変えず読むだけ。

- **設定**: main checkoutの`dagq.toml`の`[roles.plan_review]`に`provider = "codex"`（`model` / `effort`も書ける）。
  `provider`を書かない表と表の無いqueueはClaudeで動く（`domain::actor_model::CODEX_ROLES`が`plan_review`を含む）。
  この repositoryの設定は`dagq.toml`が持つ。
- **行き先**（`plan_review_route`）: `provider`を書かない役割はClaudeで起動し、queueの控えのaskが開いている間は待ち、`--no-claude`ではjobを起動せずに`plan_review_failed`でplan review by handに渡す。
  `provider`を書いた役割は`job_route`で、そのproviderが使えればそこで、使えなければもう一方で起動し、両方使えなければ待つ。
  切り替えを止める設定（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）のときは、使えない理由ではもう一方で起動せず、控えが解けるまで待って同じproviderで起動する（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。
  Claudeの控えのaskが開いていても、Codexで動けるplan reviewはCodexで起動する（plan reviewのpassは控えでjobの開始を止めず、行き先が決める。runtimeのplannerへのreviseの配送は控えの間止める）。
  `--no-claude`ではClaudeに切り替えず、Codexも使えなければ`plan_review_failed`でplan review by handに渡す。
- **起動**: goal reviewと同じ`HeadlessProgram::Job`で、promptはstdin（argvに載せないので、promptの大きさで起動が`E2BIG`にならない）。
  権限の意図はCodexではqueue serviceのsocketだけを許す読み取りのsandboxで表す（[Agent provider lifecycle](../provider-lifecycle.md#codexのheadless-job)、[Queue service](../queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)）。
  sandboxの中の`dagq`はjobの環境でクライアントモードになり、serviceがplan-review-jobのpolicyで読むコマンドだけを通す（Claudeのjobと同じ）。
  promptはClaudeと同じで、Claudeのplugin・skill・hookに頼らない。
  時間の上限は`HeadlessJob`の上限で、過ぎればjobのprocessとその子孫をpidで止める（Codexが別のprocess groupで走らせたコマンドも含む）。
- **verdict**: Codexの最後のagentの返答の文（`AgentProvider::job_reply`）から、Claudeと同じ`PlanReviewVerdict::parse`で読み、上の6の適用にそのまま入る。
- **記録**: Codexはthreadを自分で名付けるので、開始のeventにsession IDは無く、終わりのeventと区間の`session_closed`がthreadのidと実際のmodelを持つ（`JobSession::record`）。
  `stats`と`kpi`はplan reviewのjobをproviderとmodelの層で並べて読める。
  重さの予測のmodelは、Codexのjobでは終わりのeventのもの。
- **使えないprovider**: 起動できない・認証・利用上限で失敗したjob（`JobFailure::switch_reason`）は、`provider`を書いた役割ならそのproviderを控え（Claudeの認証と利用上限は控えのaskも開く）、`plan_review_failed`に理由を付けて行を`interrupted`で閉じ、proposalを保留しない。
  次のpassでもう一方のproviderで同じattemptとしてやり直す（goal reviewと同じ）。
  切り替えを止める設定なら失敗の記録と控えはこのままで、次のpassは控えが解けるまで待ち、解けた後に同じproviderでやり直す。
  それ以外の失敗（非0の終了、時間切れ、読めないverdict、jobの入力か環境による起動の失敗）は保留してplan review by handに渡し、Claudeに戻さない。
- **test**: `tests/it/plan_review_codex.rs`（行き先の場合分けは`plan_review_route`の単体test）。

## 差し戻しの分類コード<a id="差し戻しの分類コード未実装"></a>

[ADR-t947-1](../../adr/2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)の決定。
一覧と定義と主の選び方（重い順）は`domain::review_reason::PLAN_REVIEW_CODES`が持ち、plan reviewのpromptがそのまま載せる。

- **verdictの形と記録**: runの[Review](review.md#差し戻しの分類コード)と同じく、`reasons`の各項目は文とコード（先頭が主）で、文字列だけの項目も読み、コードの形はverdictを読めなくしない。
  `PlanReviewVerdict`は文とコードを分けて持ち、`plan_review_finished`と`plan_reviews`の行のverdictもコードを持つ（文を読む所は文だけを読む）。
  一覧に無い値はそのまま記録し、コードはverdictの判定と適用に使わない。
  コードより前の記録は書き換えない。
- **runのreviewと共通のコード**: 両方の一覧にあるコードは[Review](review.md#差し戻しの分類コード)と同じ定義で使う（plan reviewで止められなかったものが後でreviewのconcernやworkerの問いになった割合を追うため）。
  `task_overlap`はworkerの問い（[ask](ask.md#worker_questionの分類コード)）と同じ定義。
- **人の答えからの補い**（ADR-t947-1決定4）: 11の`approve_plan`の答えを適用するとき（`decide_plan`の同じトランザクション、`plan_decided`の前）、proposalの直近のverdictのある`plan_reviews`の行の`outcome`が`concern`なら、それを指す`plan_review_outcome`をproposalのanchorのtaskに記録する（`record_plan_outcome`）。
  答えから記録する結果は`domain::review_reason::answer_outcome`が決める。

## AIが決めるconcern（ADR-t451-1）<a id="aiが決めるconcern未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定4（ADR-0047決定11をamends）。
判定は`PlanReviewVerdict::decide_concern`（`src/domain/plan_review.rs`）、適用は`finish_plan_review`。

- **verdictの欄**: `concern`のverdictは推奨（`ready` / `send_back` / `cancel`）、確信度（askの`AskConfidence`と同じ値）、理由の分類（`scope` / `discard` / 無し）を持てる（どれも省ける。`concern`以外のverdictでは読まない。知らない値はverdict全体を失敗にする）。
  `scope`はADR・goalのconstraints・人の先例と矛盾するまま通す判断、`discard`はcancel（`cancel`の推奨は理由の分類に関わらず`discard`に数える）。
- **判定**: 推奨か確信度が無い、cancelか`discard`、`scope`、確信度が低い、上限まで差し戻した`send_back`のどれかに当たるものは人に残し、当たった理由を記録する（順は`decide_concern`）。
  どれにも当たらない（確信度が高く理由の分類が無い`ready` / `send_back`）ものはruntimeが適用する。
- **適用**: `ready`はpassと同じに適用する（上の6の`pass`。actionsの検査を含み、外れればverdict全体が失敗（7）になる。由来による優先度の扱いもpassと同じ）。
  `send_back`はreviseとして持ち主のplannerに返す（6の`revise`。差し戻しの回数に数える。指摘は`reasons`と先例）。
  どちらも行の`outcome`と記録の決定は適用した`pass` / `revise`で、verdictは`concern`のまま残る。
  askを作らないので`plan_review_outcome`（上の11）は記録しない。
- **人に残すもの**: 人に残したものは`approve_plan`のaskにする（6の`concern`）。
  askにjobの推奨と確信度を載せ、理由の分類はverdictが`discard`か推奨が`cancel`なら（推奨か確信度の欠けた形でも）`discard`、ほかは`scope`にする。
  questionに推奨と確信度と人に残した理由の行を足す。
  jobが自分でしてよい修正（actionsと`reopen`）は変えない。
  reviseの上限を超えた`revise`（6）は`concern`のverdictではないので判定にかけない。
- **記録**: `concern`のverdictを適用したとき、`plan_review_finished`の後に`plan_concern_decided`をproposalのanchorのtaskに記録する。
  検査中の編集で捨てたverdictと失敗したjobには記録しない。
  `stats`はruntimeが適用したものを`approve_plan`のaskにせず決めた数に数える（[Stats](commands.md)）。
- **prompt**: plan reviewのpromptは`concern`に推奨と確信度を付けさせ、`high`は記録（queue・repositoryの文書と決定・答えられたask）で決まるとき、`low`は決まらないか迷うとき、`scope` / `discard`の付け方と、何が人に残るかを示す。
- **test**: 境界は`tests/it/plan_review_concern.rs`、判定の順と数え方は各moduleの単体test。
