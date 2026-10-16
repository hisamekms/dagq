---
id: design-supervisor-lifecycle-throughput-review
type: design
title: "スループットの見直し（`throughput-review`）"
status: current
created: 2026-09-29
scope: runtime
related:
  - adr-t1172-1
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - adr-t996-1
  - adr-0047
  - adr-0051
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-actor-model
  - adr-t1063-1
  - adr-t1204-1
---

# スループットの見直し（`throughput-review`）

[ADR-t996-1](../../adr/2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)の実装。
supervisorがtimerで毎時・日次・週次のスループットの見直しを始め、結論をinboxに知らせるだけのattentionで届ける。
手順はpluginのdagq skillの`reference/kpi.md`の「Raising throughput: the weekly review」をそのまま使う（binaryが`include_str!`で持ち、promptはその節を写すだけで手順を二重に持たない）。

## 起動（supervisor）

- supervisorが見直しを始めるかは`SuperviseOptions::throughput_review`が決め、既定は`--help`が持つ。
  `up`が起動するsupervisorは既定で有効で、時間帯は既定でhostのもの（`clock::local_utc_offset`）。
- 各passで走るものを回収し、無ければ期限の来たものを1つ子processで始める（`supervise::throughput_review`）。
  observerと同じくrunの枠を使わず、1度に1つで、始めない間は`Supervisor::start_throughput_review`のdoc commentが持つ。
  どのproviderで始めるか（始めないか）は下の「Codexで動かす」の行き先（`Supervisor::job_start_route`）が決め、`[roles.throughput_review]`にproviderの無い見直しは、queueのhold（loginとusageの壁）のあいだと`--no-claude`では始めない。
- 期限: 時（期間が最も早く過ぎる）→日→週の順に、最後の確定した期間に終わりの記録も走っている始まりも無いものが期限で、期間・ラベル・数え方は`domain::throughput_review`のdoc commentが持つ。
  失敗した期間はやり直さない（例外はCodexが使えなかった見直しで、下の「Codexで動かす」のとおりもう一方のproviderで始め直す。`[provider_fallback] jobs = false`なら、Codexが使えなかった期間と、Claudeを書いた役割でClaudeが使えなかった期間は、控えが解けた後に同じproviderで始め直す（期限の窓に収まる限り））。
  どのsupervisorが記録したものでもよい。
  記録の前に死んだ子processを毎passで起こし直さないように、このprocessが始めた期間も覚える。
- 子processのコマンドは`dagq throughput-review`で、supervisorだけが渡す引数は`--help`に出さない。
  envはobserverと同じくsupervisorのactor（`supervisor:<pid>`）で、agentのroleはコマンドが付ける。
  supervisorが止まるときは子processとその子孫をkillし、記録の無い期間は始まりから`RUNNING_MS`を過ぎたら次のsupervisorが始め直す。
  execの引き継ぎではkillせず、jobは新しいprocessの下で続いて自分の終わりを記録し、その始まりの記録が次のprocessに同じ期間を始めさせない（自動更新の引き継ぎは1時間に何度も来うるので、長い週次のjobが殺されて始め直し続けないように）。
  jobはexecの後も同じpidのsupervisorの子なので、execで引き継いだsupervisorは前のprocessが残したjobを終わりのeventで1度だけ回収し、選び方は`domain::throughput_review::children_finished`のdoc commentが持つ。
  `--once`は走っているものを待つ。

## 毎時の判定（`domain::throughput_review`）

コマンドは直前の確定した1時間までの着地を1時間ごとに数え、`judge_hourly`が規則（直前の時間の平均からの大きなずれ・短い平均が長い平均を下回り続ける低下・着地の無い時間）で判定する。
閾値は人が決めた初期値で、値は`domain::throughput_review`の定数のdoc commentが持つ。
判定は起動を決めない: 毎時の見直しは、どの規則にも当たらない平常の時間も含めて毎時agentを起動する（[ADR-t1172-1](../../adr/2026-10-07-t1172-1-review-every-hour-and-notify-only-hours-that-met-a-rule.md)）。
判定は入力の`hourly`と見直しのeventの`reasons`に残り、当たった規則は次の2つに使う。

- 強調: 当たった時間のpromptは当たった規則を結論の冒頭に挙げて理由を分析させ、平常の時間のpromptは`reference/kpi.md`のCadenceのとおり短く書かせる（`application::throughput_review`の`hourly_cadence`）。
- 知らせ: 当たった時間の結論だけがinbox宛てのattentionになり、平常の時間の結論は`reports/reviews/`とeventに残るだけ。

不変条件: 毎時の期間は規則に関わらず`throughput_review_finished`で終わり、毎時の見直しを飛ばす経路は無い。

## コマンド（`throughput-review`、`src/application/throughput_review.rs`）

本体は`application::throughput_review::review`で、composeがqueueの読み取りとfindingの記録と、hostのファイル・設定・時間帯・agentのprocessを注入する。
supervisorが行き先を渡さないCLIの起動は`compose::throughput_review_launch`が`[roles.throughput_review]`を読む。

1. 期間を決め、着地を読む。
   毎時は判定する（dry runはpromptを返す）。
2. 入力の全体（`input.json`）を集め、部分と範囲は`gather`とその部分の関数のdoc commentが持つ。
   promptにはこの全体ではなく、下の「promptの入力」の要約だけを載せる。
3. `<queue dir>/reports/reviews/<mode>-<period>/`（あれば`-1`…を付ける）を作り、`prompt.md`・`input.json`（入力の全体）を書き、`throughput_review_started`（[Actor model](actor-model.md)）を記録する。
4. agentはsupervisorが行き先として渡したlaunch（無ければ`[roles.throughput_review]`）の`provider`（既定のClaude、または下の「Codexで動かす」のCodex）で、actor executorの`HeadlessProgram::Job`として起動する。
   権限の意図`ACCESS`は`queue_cli`で、Claude Codeは`Bash(dagq:*)`だけに、Codexは読み取りだけのsandbox（queue serviceのjobのprofile）に訳し、MCPを読まない（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
   roleは`DAGQ_ROLE=throughput-review-job`で、stdoutを`output.out`、stderrを`output.err`に分けて書き、結果はproviderの`job_reply`が`output.out`だけから取り出した返答から読み、stderrは混ぜない。
   時間の上限を過ぎたら子孫ごとkillし、model / effortは`[roles.throughput_review]`（無ければproviderの既定。もう一方のproviderに切り替えたときはその既定）。
5. 成功したら返答を`parse_output`で結論・全文・次の一手に分け、全文を`review.md`に、結論・次の一手・読めなかった理由・findingのIDを`review.json`に書く。
6. 週次だけ、次の一手を`propose`（proposalを求める印）付きのfindingにし、記録者は`supervisor`で、根拠は`throughput_review_started`のevent。
   runtimeのplannerの既存の経路（[Finding planners](finding-planners.md)）でproposalになり、`[kpi] max_improvement_proposals`に従う。
   毎時・日次の出力にblockがあっても記録しない。
7. `throughput_review_reported`を記録する。
   これがinbox宛ての知らせるだけのattention（`report the review`）で、平常の毎時（`reasons`が空）のものはattentionにならない。
   規則に当たった毎時のものは`watch --role inbox`を単独では起こさず、inboxが次に別の件で起きたときにその`events`に古い順に載る（daily / weeklyは起こす。[ADR-t1418-1](../../adr/2026-10-03-t1418-1-quiet-notices-do-not-wake-the-inbox-watch.md)、[Events and watch](commands.md)）。
8. `throughput_review_finished`を記録する（`outcome`の値と記録の規則は`application::throughput_review::review`のdoc comment）。
   `outcome`が`error`か`failed`のもの（modeを問わない）はinbox宛ての知らせるだけのattention（`check the failed review`）になり、askにはならず、claimと着地を止めない。
   失敗した期間はやり直さない（上の「期限」）。
   例外は`provider_unusable`を持つ終わり（Codexが認証・利用上限・起動の失敗・実行ファイルが無いことで使えなかった見直し。下の「Codexで動かす」）で、attentionにならず、supervisorがCodexを控えてその期間をもう一方のproviderで始め直す（`--no-claude`では始め直しが理由付きの`error`の終わりになり、それがattentionになる）。
   `succeeded`はattentionではない。
   attentionなので、KPIの`attentions_per_landing`（queueのeventのattentionも数える）にも加わる。

## sessionの区間

jobのsessionは、他のheadlessのjob（review・triage・plan review・goal review・observer）と同じく区間（`session_opened` / `session_closed`、[Actor model](actor-model.md)・[stats](commands.md)）を持つ。
区間はobserverと同じqueueのevent（task・goal・runを持たない）で、kindは`throughput_review`（`domain::sessions::THROUGHPUT_REVIEW`）。
queueのeventの区間はjobごとのkindで探し（`domain::sessions::queue_span_kind`）、observerのeventはobserverの区間だけを、見直しのeventは見直しの区間だけを開け閉めする。

- 開く: `throughput_review_started`が開く（区間の`cwd`とtranscriptの探し方は`domain::sessions::THROUGHPUT_REVIEW`のdoc comment）。
- 閉じる（`job_finished`）: `session_id`か`dir`（区間の`cwd`）の一致する`throughput_review_finished`が閉じる。
  Codexの見直しは`session_closed`に終わりのeventのthreadとmodelを写す。
  Claudeの見直しは閉じるときtranscriptの実際のmodel / effort・turn・active time・tokensが他のjobと同じ経路（書き込みの前に読む）で`session_closed`に入る。
- 重なり: 見直しは同時に複数走りうる。
  execの引き継ぎではjobを殺さないので、前のprocessが残したjob（例: 週次）が走るあいだに次のprocessが別の期間（例: 毎時）を始め、supervisorが複数あれば別のsupervisorのjobも走る（同じ期間は`running`のあいだ始めさせない）。
  そのため始まりは他の区間を閉じず、終わりは自分の`session_id`の区間だけを閉じる。
- 終わりの無い区間（jobが死んだ・supervisorの停止で殺された・記録の前に落ちた）は`inferred`で閉じる。
  規則の1つ目: 同じ`mode`と`period`の`throughput_review_started`が来たら、前の区間を閉じる（`RUNNING_MS`を過ぎて始め直されたので、前のjobは終わっている）。
  規則の2つ目: 見直しのevent（`throughput_review_started`・`throughput_review_finished`）が記録されたとき、開いてから`RUNNING_MS`以上たった区間を閉じ、その時までにjobは終わったか時間切れで殺されている。
  `inferred`の区間はtranscriptの最後の記録で終わる（ADR-0048 決定 7）。

## promptの入力

どのheadless jobもpromptをstdinで受け、引数の上限には当たらない（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)の「promptの渡し方」）。
日次・週次の入力の全体は数MBで、stdinで渡してもagentの文脈に収まらないので、promptに載せる入力を要約して上限を設ける（providerのinterfaceは変えない）。

- 上限: prompt全体と、そのうちの入力（pretty JSON）にそれぞれ上限があり（`application::throughput_review`の定数）、残りは指示・手順・言語の行。
- 要約とそこに載せる`stats`の部分は`prompt_input`のdoc commentが持ち、読めなかった部分の`{"error": ...}`はそのまま載せる。
  kpi.mdの手順のCadenceが日次（外れ値と目標割れ）・週次（手順1〜5）で見る数値は要約の`kpi.latest`と`kpi.targets`と`stats.parts`にあり、層ごとの値・過去の期間の値・runごとの値・timelineはjobが読むコマンドで取りに行く。
- 切り詰め: 要約がなお入力の上限を超えるときは、`DROP_ORDER`の順（大きく、コマンドで読み直しやすいものから）に収まるまで丸ごと落とし、落としたものを名前で残す。
  `period`と`hourly`は落とさない。
- promptは要約だと明かし、入力の全体が見直しのdirの`input.json`にあること（人が読むためのもの）と、細部は読むコマンドで取りに行くことを指示する。
  dry runはdirを作らず、作るはずのdirの`input.json`を名指す。
- jobの権限（`ACCESS`の`queue_cli`）は広げない（理由は`ACCESS`のdoc comment）。
- この節の上限・要約・省いたものを読むコマンドの示し方は、headlessのjobに共通の方針（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）の先行の例で、全jobの渡し方と上限は[Prompt](prompt.md#headlessのjobのprompt)の「headlessのjobのprompt」の節の表が持つ。
- byte数の記録（ADR-t1566-1の決定6）: promptを作った後の見直しのeventとdry runの出力に同じ記録を載せ、欄の意味は`ReviewPrompt::record`のdoc commentが持つ。
  promptを作る前に終わったもの（使えるproviderが無い・入力の準備の失敗）には無い。

## Codexで動かす

`dagq.toml`の`[roles.throughput_review]`に`provider = "codex"`を書くと、見直しのjobはCodexの`codex exec`で動く（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)、[Actor model](actor-model.md)の`CODEX_ROLES`）。
jobは状態を変えず読むだけ（[ADR-t996-1](../../adr/2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)の決定4）なので、権限の意図`ACCESS`（`queue_cli`）はそのままで、Codexの実装が読み取りだけのsandboxに訳す（queue serviceが動いていればgoal reviewと同じjobのpermission profile `dagq_job`。`:read-only`を継ぎ、jobの`dagq`はクライアントモードでserviceに読みに行く。[Agent provider lifecycle](../provider-lifecycle.md)）。
promptは変えず、Claudeのplugin・skill・hookに頼らない。

- 行き先（observerと共有する`Supervisor::job_start_route`）: providerを書かない役割はClaudeで、queueのholdのあいだと`--no-claude`では始めない（[ADR-t1204-1](../../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)の決定2）。
  providerを書いた役割は`job_route`で、そのproviderが使えれば（supervisorにagentがあり控えられていない）そこで、使えなければもう一方のproviderで始め、どちらも使えなければ待つ。
  `[provider_fallback] jobs = false`（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）のときは、使えない理由（実行ファイルが無い・起動できない・認証・利用上限）ではもう一方で始めず、控えのあいだ`job_start_route`は`None`で待つ（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。
  `--no-claude`ではClaudeは使えないので、Codexを設定した見直しはCodexで始まり、Codexも使えなければ子processに理由を渡す。
  子processは期間を決めて毎時の判定をした後、agentを起動せずに`throughput_review_finished`（`outcome: error`、`error`に理由）を記録し、inboxの`check the failed review`になる。
- 子process: supervisorは行き先のlaunchを渡し、コマンドはその`provider`のagentで起動する。
  launchが無い（人が手で打つ）ときは束縛されたcheckoutの`[roles.throughput_review]`（`compose::throughput_review_launch`）。
  Codexのjobはsession idを持たずに始め、終わりにproviderが読んだthreadとrolloutの実際のmodelを`throughput_review_finished`に書く（`JobSession::record`）。
  結論はproviderの返答（Codexは最後の`agent_message`）を`parse_output`で読み、保存・週次のfinding・`throughput_review_reported`はClaudeと同じ。
- Codexが使えなかったとき: 役割がproviderを書いた見直しでCodexのjobが成功しなかったとき、providerの`job_failure`（agentが起動しなかったときは起動のerrorを分類する）が使えないこと（認証・利用上限・起動の失敗・実行ファイルが無い）を示せば、`throughput_review_finished`に`provider_unusable`を書く。
  これはattentionにならず（`domain::event_attention`）、その期間は`reviewed`に数えない。
  supervisorは自分の子のその終わりを見てCodexを控え（利用上限はCodexの出力から再開の時刻を読む）、このprocessが始めた期間の記録から外すので、次のpassでその期間はもう一方のproviderで始まる。
  execの引き継ぎで前のprocessから渡った見直しの終わりも同じく`provider_unusable`を見てCodexを控えるので、引き継ぎの間に止まったCodexの見直しの期間も、もう一度Codexでは始まらない。
  `--no-claude`ではClaudeに行かず、上のとおり理由を記録する。
  控えを書けなかったときは期間をやり直さない（同じCodexをすぐ起動し直さないため）。
  ClaudeのjobはClaudeの壁に当たっても失敗として記録する（queueのhold askには入れない）。
  `[provider_fallback] jobs = false`のとき、Codexを書いた役割は上と同じく`provider_unusable`（`codex`）を記録してCodexを控え、その期間を始めた記録から外すが、次のpassはClaudeで始めず、Codexの控えが解けた後のpassで同じ期間をCodexで始め直す。
  Claudeを書いた役割の扱いは`UnusableHold`のdoc commentが持つ。
- 時間の上限: Claudeと同じく時間の上限を過ぎたらjobと子孫をpidで止める（Codexが別のprocess groupで走らせたコマンドも、killの前に列挙した子孫として止まる）。
- 記録: `stats`と`kpi`のjobのproviderの層でClaudeと分けて読め、jobの始まりと終わりは期間とdirで結ぶ（`domain::stats::jobs`）。

## 権限

`throughput-review-job`は`review-job`などと同じheadlessのjobで、policyは`QueueRead`だけ（[Authorization](../authorization.md)）。
`kpi`・`stats`・`timeline`・`events`などの読むコマンドは打て、状態を変えるコマンドはCLIが拒み、拒否は記録に残る（[Authorization](../authorization.md)）。
`throughput-review`コマンド自体はobserverの`observe`と同じ`observe.run`（supervisorと人とinbox）で、Claudeのsettingsのdenyにも入る。
