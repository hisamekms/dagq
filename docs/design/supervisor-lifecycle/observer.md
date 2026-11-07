---
id: design-supervisor-lifecycle-observer
type: design
title: "Observer"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-0044
  - adr-0051
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-finding-planners
  - design-domain-model
  - adr-0070
  - adr-t649-1
  - adr-t1222-1
  - adr-t1063-1
  - adr-t1204-1
  - design-supervisor-lifecycle-actor-model
  - adr-t3745-1
---

# Observer

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定4・18・23。
observerはdagqが回っているかを観察し、継続的改善の材料をfindingに残すjobで、個々の詰まりは解消しない。
note・goal・taskを書かず、それらとrunの状態を変えず、見立てが人を要るfindingにだけ`blocked`のaskを上げる（下の「人が要る見立てだけをblockedにする」）。
workspaceを持たず、MCPを読まないheadlessのagentとして動く。

ユースケースの入口は`src/application/observer/mod.rs`、queueと観察の入力・hostの組み立ては`src/compose/observation.rs`。
観測のファイルとagentのprocessは`src/infrastructure/observer.rs`が受け持つ。

## 観測の入口と窓

1回の観測の入口は`application::observer::observe`で、入力の収集から終了の記録までの契約はそのdoc commentが持つ。
変化もalertの増加も無ければagentを起動せずskippedを記録する（[ADR-t649-1](../../adr/2026-09-28-t649-1-new-time-driven-alerts-wake-the-observer.md)）。
時間だけで閾値を越えるalertも起動の理由になるため、eventの有無だけではskipを決めない。
起動を省ける条件は`skip`、alertの同一性は`alert_keys`、数えないeventは`SqliteQueue::events_besides`が持つ。
KPIの記帳だけではobserverを起こさない（[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)決定24）。

入力の組み立てとpromptが残す順は`observer_input`、役割・閾値・flakyなtest・KPIの読み方は`observer_prompt`が正本。
workerの作業中だけで落ちたtestを継続的改善のfindingにせず、着地と自動更新の関門での失敗を読む。
失敗を繰り返すe2eの読み方は[ADR-t1165-1](../../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)決定4、記録の入口は[自動更新](auto-update.md)。
閾値を見直すfindingから改善を提案できるが、observer自身は閾値を変えない。

失敗した観測の窓は次の観測が読み直す。
保存するcursorはstatsのpageの続きなので、終わったrunを飛ばさない。
期間集計はpageによらず窓の終わりまで数えるため、間隔のうちにpageを越えるrunが終わると次の集計が前の窓と重なる。
観測ごとの期間集計を足し合わせず、1回の値をその窓の値として読む（`domain::stats`のmodule doc）。
連続する失敗の通知は`domain::OBSERVER_FAILURES_TOLD`とattentionの判定が持ち、askにせずclaimと着地を止めず、statusにも残さない。

履歴を知りたいときの入口は`application::observer::history`、観測時点の全入力を読みたいときは`infrastructure::observer::read_input`。
終了した観測の出力をruntimeが読み直して分類することはない。
失敗の分類を知りたいときの入口は`application::observer::read_failure`。

## 権限と出力の境界

providerによらずjobのdagqはDBを開かず、[queue serviceのクライアントモード](../queue-service.md#クライアントモード)で書く（[ADR-t1222-1](../../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)）。
認可はserviceがobserverのprincipalで行い、通せる操作は[Authorization](../authorization.md)の表が持つ。
書き手とactorの記録がCLIと同じになる約束は[Queue serviceのユースケース](../queue-service.md#ユースケース)、起動ごとのtokenの発行と失効は[principalとtoken](../queue-service.md#principalとtoken)が持つ。
書かれなかったfindingとaskは観測の終了の件数にもidにも入らない。
走っている間にserviceが落ちた場合の観測の成否は`observe`のdoc commentが持つ。

promptは省いた入力をファイルのpathではなくqueueの読むコマンドで読ませる。
Claudeのobserverはdagqだけを打ててファイルを読めず、Codexも同じpromptを使う。
CodexにClaudeの道具の許可の代わりのhookやrulesを作らない。
読み取りだけのsandboxがファイルの書き込みと他のprocessへのsignalを拒み、queueへの書き込みはserviceの認可が通す（ADR-t1222-1決定5）。

findingの読み手は人とplannerで、影響の大きい順に読み、手当てしないと決めたものはdismissする。
proposalを求める印からruntimeのplannerが改善を作る境界は[Finding planners](finding-planners.md)。
supervisorの見張りだけが書けるfindingは[agentのeval](../agent-eval.md#採用した定義の見張り)と[CI watch](ci-watch.md#定時のworkflow)が持ち、observerの記録では代用しない。
findingに紐づくaskのoptionと人の答えの適用は`NewAsk::offered_options`と[ask](ask.md)。
blockedのaskはinboxのstatusに現れる。
runの生死はbackgroundのwrapperで測る（[Stats](commands.md)）。
認証切れや利用上限で止まったときはholdのaskをinboxのwatchが人に知らせる（[人への通知](ask-push.md)）。

## timer

supervisorが観測を起動する入口は`src/application/supervise/observation/observer.rs`。
期日の判定は`due_observation`、子プロセスの起動は`ObservationState::start_observer`、終了と引き継ぎでの停止は`ObservationState::stop_observer`が持つ。
同時に走るobserverは1つで、run slotを使わず、queue serviceが居るときだけ起動する。
期日はqueueの記録で判定し、別のsupervisorや手で起動した観測も数える。
記録前に落ちた子を毎passで起動しないため、process内でも起動の間隔を守る。
正常な停止は観測の終了を待つが、supervisorのerrorとexecの引き継ぎでは子孫ごと止め、次のprocessが期日に起動し直す。
強制停止した観測は終了の記録を書かない。
agentの時間切れも子孫をpidで止めるため、Codexが別のprocess groupで起動したものも残らない（`infrastructure::observer::run_agent`）。

## Codexで動かす

[ADR-t1222-1](../../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)と[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1・4・5。
providerの設定を読む入口は`infrastructure::observer::observer_launch`。
promptと打たせるコマンドと出力の形はproviderによらず、Claudeのplugin・skill・hookに頼らない。
設定は観測ごとに読み直すので、main checkoutに反映されればsupervisorの再起動は要らない。

権限の意図は`application::observer::ACCESS`、Codexのsandboxへの翻訳は[Agent provider lifecycle](../provider-lifecycle.md#codexのheadless-job)。
serviceのsocketへの到達だけを足し、書けるrootやnetworkを足さずsandboxを外さない。
行き先を決める入口は`domain::actor_model::job_start_route`（スループットの見直しと共有）。
使えないproviderを控えた後の再起動は`ObservationState::timer_job_due_again`で、切り替えの可否は[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)が持つ。
Codexの失敗をClaudeのqueueのhold askに入れない。
切り替えを止めたjobは控えが解けた後に同じproviderで観測し直す（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
Claudeを使わない運転の約束は[ADR-t1204-1](../../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)決定2。
起動する実行ファイルの解決はCLIのobserveの分岐、条件の説明は`ObserveOptions::launch`が持つ。
Codexが名付けるthreadを終了時に記録する入口は`domain::headless_job::JobSession::record`で、jobの集計をproviderとmodelで分けて読める。

## promptの入力の上限と選ぶ順

[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)の決定2〜6。
入力をpromptに収める入口は`src/application/observer/input.rs`の`fit`で、上限・節の優先順・要約の契約はそのmoduleと定義のdoc commentが持つ。
LLMには入力を選ばせず、省いたものと続きを読む方法を必ず示す。
観測時点の全入力を読むコマンドと今の状態を読むコマンドを区別し、省略から問題が無いと判断させない。
指示と見出しは切らず、それだけで上限を越えるときは資料を全て省いて読む方法を示す。
promptの渡し方とbyte数の記録の共通の約束は[Prompt](prompt.md#headlessのjobのprompt)。

## KPIの目標割れと改善の上限

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定24〜26（決定26はADR-t1971-1がamends）。
KPIの観測入力は`OneShot::observer_kpi`、目標割れをfindingに対応させる入口は`domain::kpi::observe`。
数字はkpiの出力をそのまま写し、observer自身では作らず、目標割れをblockedのaskにしない。
目標割れの記録・解消・改善の提案の読み方は`observer_prompt`が持つ。

改善のproposalが同時の上限に達している間は、印の付いたfindingはopenのまま待ち、1つ終われば印の古い順にplannerが立つ（[Finding planners](finding-planners.md)の改善の上限）。
改善のtaskの優先度はgoalから継ぎ、normal以下はgoalと所属で守る（同文書の優先度の制約）。

## 完了見込みの誤差

[ADR-0070](../../adr/0070-forecast-snapshots-and-scoring.md)の決定5。
偏りが続くという判定はforecastのKPIの目標割れだけで、observer自身が見込みの数字から偏りを判定する別の経路は作らない。
findingの対応は`domain::kpi::observe::subject`で、根拠と解消・改善の上限と優先度はKPIの目標割れと同じ。
人へのaskにせず、snapshotの記帳だけでobserverを起こさない。

## 人が要る見立てだけをblockedにする<a id="人が要る見立てだけをblockedにする未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定2（ADR-0047決定4・23をamends）。
observerは見立てが人を要るfindingにだけblockedのaskを上げ、待てば解ける・leave itという見立てはfindingに残す。
人の判断かruntimeと復旧jobが行えない手の操作を要ることが条件で、認証と費用はqueueのholdのaskが持つ（[ask](ask.md)の「人が要る理由」）。
見立ての指示は`observer_prompt`、CLIとserviceの推奨の検査は`NewAsk::check_asker`が持つ。
推奨がwaitやleave itに当たるかはCLIでは判定しない。

findingごとにopenなblockedのaskは1件のままで、askの無いfindingは普通の状態である。
人はfindingsでその見立てを読む。
askにしなかったfindingの数え方は`SqliteQueue::written_by`、窓の集計は`domain::stats::recommendations`。
