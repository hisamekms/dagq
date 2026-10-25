---
id: adr-t1181-1
type: adr
title: inboxのproviderは役割の設定を既定にして`dagq inbox`の起動の引数で上書きし（既定claude）、Codexのinboxの足りない機能は起動の引数のhookと前景のwatchと指示で埋め、状態を変えるdagqはinboxのprincipalでqueue serviceに送ってsandboxを緩めない。Codexが使えなければ黙ってClaudeに戻さず開かずに人に言い、inboxのsessionにもproviderとmodelとthreadを記録する（ADR-t1063-1決定1・5・6をamends）
status: accepted
created: 2026-10-24
updated: 2026-10-25
accepted_on: 2026-10-24
amends:
  - adr-t1063-1 decision 1
  - adr-t1063-1 decision 5
  - adr-t1063-1 decision 6
amended_by:
  - adr-t3331-1
owners:
  - hisamekms
tags:
  - runtime
  - provider
  - inbox
related:
  - adr-t1063-1
  - adr-t813-2
  - adr-t813-3
  - adr-t1233-5
  - adr-t728-3
  - adr-t2159-1
  - adr-t1433-5
  - adr-t2022-1
  - adr-t3102-1
  - adr-t3102-2
  - adr-t3102-3
  - adr-t3105-1
  - adr-t1091-1
  - adr-t1204-1
  - adr-t1394-1
  - plan-codex-inbox-spike
  - design-provider-lifecycle
  - design-queue-service
  - design-supervisor-lifecycle-actor-model
  - design-supervisor-lifecycle-queue-hold
---

# ADR-t1181-1: inboxのproviderを選ぶ場所、Codexのinboxの足りない機能の埋め方、Codexが使えないときの扱い、inboxの記録（ADR-t1063-1決定1・5・6をamends）

## Context

[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)はheadlessのjobのproviderを役割の設定で選ぶとし、Contextで「plannerとinboxは対話のsessionなので対象外」とした。2026-09-30に人はinboxをCodexでも動かし、どのprovider・modelで動いたかを必須で記録すると決めた（goal 77）。前提は[ADR-t2159-1](2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)で、inboxは人が自分のterminalで`dagq inbox`を打って開き（決定2）、`up`はinboxを開かず（決定3）、dagqはinboxの画面を読まず打ち込まない（[ADR-t1433-5](2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)決定1・2）。

測定（[codex-inbox-spike](../plans/codex-inbox-spike.md)、codex-cli 0.160.0、scratchのrepositoryとsocketでの対話のTUI）で分かったこと:

- 項目1: backgroundのcommandの終わりでsessionは起きない。前景の待ちなら同じturnで返りを扱え、待ちの間も人は打てる。外のprocessの`codex queue`はthreadを起こせるが、sandboxの中からは打てない。
- 項目2: 対話で問いと答えを往復できる。選択肢のtoolは開発中のfeatureを有効にしたときだけ出る。
- 項目3: AGENTS.mdとrepositoryの`.agents/skills`は自動で読む。pluginのskillを起動の引数で足す手段は無く、常に効く指示と初期のpromptは起動の引数で渡せる。Claudeのguardrailのsettingsに当たるものは無い。
- 項目4・5: SessionStart（startup・compact・clear・resume）・SessionEnd・Stopのhookを起動の引数で渡せ、stdoutがcontextに入り、`session_id`と`model`が出る。hookはtrustが要り、その起動だけtrustを飛ばす引数がある。`/clear`の前のsessionの終わりはprocessの終わりまで出ない。rolloutにprovider・CLIの版・modelがある。
- 項目7: sandboxからhostのsocketには、networkを開けるかsocketだけのpermission profileで届いた。queue dirのDBへの直接の書き込みは拒まれた。
- 項目8: 実行ファイルが無いことと未ログインは起動の前に見分けられた。

inboxのprincipalは[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定2〜4で決まり、`dagq inbox`が発行するtokenは実装済み（[ADR-t3105-1](2026-10-23-t3105-1-reopen-the-inbox-once-after-the-host-cli-reaches-the-queue-service.md)）。taskの登録の時の前提（serviceはAI actorにだけtokenを出し`answer`などを`no_use_case`で断る）は、tokenの部分はもう当たらない。spikeの時点（項目7「既決・未実装・問い」）では`answer`・`ask close`・`request add`などのユースケースの無い書き込みがDBを直接開いた。今のmainでは計画の書き込み（`add`・`goal add`・`submit`・`request add`など）はserviceのユースケースとして送られる（[hostのCLI](../design/queue-service-host-cli.md)の「計画のユースケース」、[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定3・4）。残っているのは`answer`・`ask close`などinboxが人の答えと代行で打つ書き込みで、まだDBを直接開き、[ADR-t3102-2](2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)決定6の段4がservice経由に移す。

未確認のもの: inboxの見張りを前景で時間単位で続けられるか、`dagq inbox`からCodexのinboxを開いてanswerまで通ること（[手動スモーク](../development/manual-smoke.md)の「Codexのinboxのスモーク」。goal 77のnoteと[結果](../plans/manual-smoke-results.md)にはまだ結果が無い）。PreToolUseのhookでコマンドを拒めるか、socketだけを足すsandboxでinboxの読み取り・作業に足りるか（測った形は読み取りだけを継ぐもの）、tokenのファイルがsandboxの中の`dagq`に届くか、利用上限に当たったときの画面と終わり方、共有のdaemonの経路も確かめていない。TUIの起動が人の`~/.codex/config.toml`を書かないことも確かめきれていない。spikeでは人の設定は変わらなかったが、空の設定では起動の案内の欄が書かれ、`tui.model_availability_nux`は既に欄があったので書かれなかっただけとみられる（spikeの「task 1174の結果と`~/.codex/config.toml`」）。

## Decision

1. **providerとmodelは役割の設定を既定にし、`dagq inbox`の起動の引数でその起動だけ上書きする（ADR-t1063-1決定1をamends）。** 決定1の役割の表にinboxを足す。値と既定（`claude`）・modelをproviderごとの名前として読むこと・実装の無いproviderを設定の誤りにすることは決定1のまま。inboxは人が開くので（ADR-t2159-1決定2）、起動の引数でproviderとmodelを選び直せる。どちらも指定しなければ今のClaudeのinboxと同じに起動し振る舞う。どちらで選んだかを起動の記録に残す（決定5）。決定1の「headlessのjob」の決定2〜4・7はinboxに当てず、inboxの切り替えは決定3が持つ。根拠: spikeの候補1。設定は役割のproviderと並べて比べられ、引数は人がその場で選べる。
2. **Codexのinboxの足りない機能はCodexの起動の引数とinboxの指示で埋め、人のCodexの設定を書かない。** providerの実装が起動の引数だけで次を持ち、人の`~/.codex`の設定・`CODEX_HOME`を書き換えず、共有のdaemonにつながない（spikeの候補8）。
   - 見張り: inboxのturnがinboxの見張り（watch）を上限つきで前景で待ち、返ったら扱って待ち直すことを指示とskillに書く（項目1の(a)。具体的な呼び出しはdesignとskill）。dagqの外のprocessがthreadに次のturnを積む経路は取らない。dagqがinboxに話しかける経路を足すことになり、ADR-t1433-5決定2の趣旨に合うかを決めずに済む前景の待ちで足りるかを、まず手動スモークで確かめる。
   - askの提示とanswer: 選択肢は番号つきの文で見せ、人の文の答えを受ける。開発中のfeatureの選択肢のtoolには頼らない（項目2）。
   - 指示: AGENTS.mdは自動で読み、dagq-inboxのskillは常に効く指示でpluginのSKILL.mdの場所を名指して読ませる。Codexのpluginは人の設定に入れるものなので使わない（項目3）。
   - 復帰・区間・Stop: SessionStart・SessionEnd・Stopのhookを起動の引数で渡し、ClaudeのinboxのhookのSessionStartの`status`、区間の記録、watchの無いturnの終わりのblockと同じ役目を持たせる。trustの確認を省くのは、dagqが起動の引数で渡す記録と復帰のhookだけに限る。有効な全てのhookのtrustを飛ばす形（spike項目4の、その起動だけtrustを飛ばす引数）は使わない。sandboxの外で動きうる、人が確かめていないhookをhostで動かすことになるためである。Codexがdagqのhookだけにtrustの省略を限れないと実装のtaskが確かめたら、起動の時のtrustの確認を残し、人がそのhookを確かめて認める。Codexのworkerとjobにtrustやsandboxを飛ばす引数を使わない決定（ADR-t813-3決定6、ADR-t1063-1決定2）は変えない。sandboxは飛ばさない。`/clear`の前の区間は、新しいSessionStartで閉じたと推定する（[ADR-t2022-1](2026-10-08-t2022-1-supervisor-infers-ended-hook-sessions-without-cmux.md)と同じ考え方。項目4・5）。
   - guardrail（ADR-t2159-1決定5）: Codexの手段はPreToolUseのhookで、roleのdagqのコマンドと身元の環境変数を拒む。hookで拒めないと実装のtaskが確かめたら、guardrailの無いinboxとして起動の記録に残し、designにそう書く。
   - 画面の判定: 持たない。承認やloginの画面で止まったinboxは人が自分のterminalで見ており、watchの不在の後ろ盾（ADR-t1433-5決定1の(3)）が残る（項目6）。
   - sandboxからの状態を変えるdagq: Codexのworkerとjobの経路（[ADR-t1233-5](2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)決定4・5）をinboxに当てはめる。inboxのdagqはクライアントモードでinboxのtokenとともにserviceのsocketに送り、serviceがinboxのprincipalで認可して記録する。sandboxはqueue dirを書かせず、socketへの接続だけを足す（足せなければ届け方を実装のtaskが確かめてdesignに書く。ADR-t1233-5決定4と同じ扱い）。queue dirを書ける範囲に足してDBを直接開かせる案は取らない。ADR-t3102-2決定7の並存の期間には許されるが、DBを直接開く経路を減らしている期間に新しく足すことになり、queue dirを書けることは他のrunのworktreeとqueueの記録を書けることでもあり（ADR-t813-3決定3の理由）、書く範囲をDBの操作に絞れない。
   - 権限と記録: Codexのinboxの権限と`answered_by`・`answer_authority`の記録はClaudeのinboxと同じで、providerで変えない。inboxはuserと同じ権限で、代行はactorの型から記録する（[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)決定1・2、ADR-t3102-1決定3・4）。
3. **Codexが使えなければ黙ってClaudeに戻さず、開かずに人に言う。** 「使えない」は[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定2と同じ。`dagq inbox`はexecの前に実行ファイルとloginを確かめ、使えなければinboxを開かず、理由とClaudeで開く起動の引数を人のterminalに出して0でなく終わる。askは開かない（知らせる先のinboxが無く、人は目の前にいる）。開いた後に使えなくなったら（利用上限など）、人が開き直す。根拠: 項目8と候補6。inboxは人の前で開き、黙って替えると比べる記録が混ざる。
4. **providerの控えはinboxを止めず、Claudeの控えの`queue_hold`の扱いは変えない（ADR-t1063-1決定5をamends）。** runtimeはinboxを起動しないので、控えはinboxを止めず、inboxは決定5の「Claudeでしか動けない役割」に数えない。Claudeの控えで今の`queue_hold`のaskを開く条件と、Codexだけの控えでaskを開かないこと（ADR-t813-2決定6）は変えない。Claudeの控えの間もCodexのinboxはそのaskを人に見せ、answerを書ける。ADR-t813-2の決定は変えない。人がClaudeの起動を禁じたとき（[ADR-t1204-1](2026-09-30-t1204-1-explicit-no-claude-operation.md)決定1）は、`dagq inbox`はClaudeのinboxを開かず、Codexが選べればCodexで開き、選べなければ決定3と同じく開かずに理由を出す。根拠: spikeは控えそのものを測っていない。askを人に見せることは項目2、answerの書き込みが届く経路は項目7のsocketへの到達で確かめた範囲で、`answer`がservice経由で書けること（段4）と、Codexのinboxで`queue_hold`のaskに答えて控えが解けることは未確認で、手動スモークで確かめる。Codexの利用上限の画面も未確認（項目8）なので、Codexの控えの間のinboxの振る舞いは決定3の「開いた後に使えなくなったら人が開き直す」に任せる。
5. **inboxのsessionにもproviderとmodelとsessionのidを記録する（ADR-t1063-1決定6をamends）。** 決定6の記録の対象にinboxのsessionを足す。inboxの起動の記録にproviderとどちらで選んだか（決定1）と要求したmodelを、区間の記録に分かる範囲の実際のmodel（読めなければ要求したmodelと読めなかったこと）とsession（Codexならthread）のidを残し、statsかkpiでClaudeのinboxと分けて読め、inboxの役割に設定されたproviderを決定6のとおり`status`か`doctor`で見られるようにする。必須で、記録の欠けたままCodexのinboxを選べるようにしない。根拠: 項目5。SessionStart・SessionEndのhookが`session_id`と`model`を、rolloutがprovider・CLIの版・実際のmodelを持ち、`/clear`で新しい`session_id`になる（項目4）。区間の始まりは最初のturnまで、`/clear`の前の区間の終わりは`/quit`まで遅れることは測った。hookをdagqのhookだけに限ってtrustを省けない場合に人がtrustしなかったときの記録の欠け方と、自動のcompactionの後の記録は未確認で、実装のtaskが確かめる。

欄名・設定のkey・flag・eventのkind・既定値・Codexの起動の引数の綴りは、後続の実装のtaskが[docs/design/](../design/)（[provider-lifecycle](../design/provider-lifecycle.md)・[actor-model](../design/supervisor-lifecycle/actor-model.md)ほか）に書く。

### 実装の順と分け方の方針

(a) providerとmodelの選択（設定と起動の引数）と起動の記録（決定1・5の起動の部分。Claudeだけで着地でき、既定の振る舞いを変えない）。(b) Codexの起動の経路（決定2の起動の引数・指示・hook・guardrail・sandbox）と使えないときの確認（決定3）。hookのtrustをdagqのhookだけに限れるかと、起動が人の`~/.codex/config.toml`を書くかを最初に確かめる。(c) 区間の記録と集計（決定5の残り）。(d) design・pluginのskill・AGENTS.md。Codexのinboxを選べるようにするのは、(b)(c)と、inboxにまだ残るユースケースの無い書き込み（`answer`・`ask close`など。計画の書き込みは既にservice経由）をservice経由にするADR-t3102-2決定6の段4の着地の後にし、それまでは設定と引数でcodexを選ぶと開かずに理由を出す。最後に人かinboxが手動スモークで確かめる。

## Alternatives

- **選ぶ場所を設定だけ、または起動の引数だけにする**: 設定だけでは人がその場で試せず、引数だけでは役割のproviderと並べて既定を読めない。
- **外のprocessが`codex queue`でinboxを起こす**: 確実に起こせるが、dagqがinboxへ話しかける経路を持つ。前景の待ちで足りないと手動スモークで分かったときに、ADR-t1433-5決定2との関係とともに決め直す。
- **有効な全てのhookのtrustをその起動だけ飛ばす**: 人の設定を書かずに済むが、人が確かめていないhookもsandboxの外でhostで動く。2026-10-25に人がこれを退けた（ask 1184）。
- **hookのtrustを人が起動の時に与える**: Codexが人の設定にtrustを書きうる。dagqのhookだけに省略を限れないときの残す形として採り（決定2）、最初から選ぶ形にはしない。hookを使わず指示だけにすると、区間の記録（必須）が欠けうる。
- **sandboxを緩めてDBを直接開かせる**: 段4までの間だけの案にしかならず（ADR-t3102-2決定1）、決定2に書いた理由で取らない（項目7）。sandboxを外す引数は候補にしない。
- **run dirへの要求をsupervisorが取り込む（[ADR-t813-3](2026-09-28-t813-3-codex-worker-permissions.md)決定3）**: ADR-t1233-5決定5がservice経由にamendsした歴史的な案で、選ぶ候補にしない。
- **Codexが使えなければClaudeで開く**: 人の前で黙って替わり、記録が混ざる。ADR-t813-2の相互フォールバックは人のいない非対話のworkerのためのもの。

## Consequences

- providerを指定しなければ今のClaudeのinboxのまま。Codexのinboxは`answer`などがservice経由になる段4（goal 182）の着地を待つ。実装までの今の姿（inboxはClaudeだけで、この決定は実装前であること）と方針の境界は、この変更で[`up` / `down`](../design/supervisor-lifecycle/up-down.md)の「`dagq inbox`」と[actor-model](../design/supervisor-lifecycle/actor-model.md)の「provider」「記録」に書き、各実装のtaskが同じ変更でそれを今の姿に書き直す。
- trustの省略をdagqのhookだけに限れなければ、Codexのinboxを開くたびか最初の起動で人がtrustを確かめる手間が残り、そのtrustをCodexが人の設定に書きうる。人が自分で認める操作なのでdagqが書くことには当たらないが、どちらになったかとその手順を実装のtaskがdesignと手動スモークの手順に書く。
- TUIの起動が人の`~/.codex/config.toml`の欄を書くか（Contextの未確認）を、実装のtaskが起動の前後の更新時刻と中身で確かめる。書くなら起動の引数で止められるかを調べ、止められなければdesignに書く。
- 前景の待ちはturnを長く開き、待ちを続けるかはmodelと指示に頼る。手動スモークで時間単位で続かなければ、外から起こす案を決め直す。
- 未確認の前提（上のContextの最後）は実装のtaskと手動スモークが確かめ、違えばdesignと実装を直し、決定が変わるならADRで直す。
- plannerに同じ決定が当てはまるかは決めない。plannerはruntimeが立てる非対話のjobで（[ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）、Codexに乗せるならADR-t1063-1のjobの経路の延長になる。spikeでは指示の読み込み（項目3）、rolloutからのproviderとmodelの読み取り（項目5）、sandboxからsocketへの到達（項目7）が持ち込める。登録の書き込み（`add`・`goal add`・`submit`）は今はserviceのユースケースなので、spikeの時点と違い段4を待たずにsocketの経路に乗る。別のgoalで決める。
