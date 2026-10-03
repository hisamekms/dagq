---
id: adr-t1228-1
type: adr
title: inboxとplannerはcmuxを直接打たず、plannerへの依頼・plannerを閉じること・runとplannerのsessionの画面を読むこと（行数の上限つき）と送ること（決めたキーの集合と答えられたaskの答えだけ）・supervisorが居ないあいだに残った終わったrunのworkspaceの片付けをdagqのCLIで行い、宛先をIDで指し、actorのpolicyで判定してactor付きのeventを残す
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amended_by:
  - adr-t1394-2
  - adr-t1433-1
  - adr-t1433-2
  - adr-t1433-3
owners:
  - hisamekms
tags:
  - runtime
  - security
  - cmux
  - inbox
  - planner
related:
  - adr-t728-1
  - adr-t728-3
  - adr-t1228-2
  - adr-t609-1
  - adr-0026
  - adr-0031
  - adr-0044
  - adr-0047
  - adr-t598-1
  - adr-t1091-1
  - design-authorization
  - design-security
  - design-supervisor-lifecycle-session-send
  - design-supervisor-lifecycle-plan-planners
  - design-supervisor-lifecycle-cleanup-and-recovery
  - design-supervisor-lifecycle-run-workspaces
---

# ADR-t1228-1: inboxとplannerはcmuxを直接打たず、sessionへの操作をdagqのCLIで行う

## Context

dagqの認可（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)、[Authorization](../design/authorization.md)）はdagqのCLIにしか効かず、`cmux send`・`send-key`・`read-screen`・`workspace close`は判定も記録も通らない。workerとjobはコンテナ化（goal 38、goal 82）で塞がるが、inboxとplannerはhostの制御側に残るので、この穴はコンテナ化では閉じない。しかもworkerのreceipt・ask・follow_upを読むのはinboxとplannerで、prompt injectionの入り口になる。2026-10-01には、supervisorが居ないあいだに残った終わったrunのworkspace 69個を、inboxがcmuxでUUIDを引き直して手で閉じた。人は同じ日に、これらをdagqのCLIに移し、宛先をworkspaceのUUIDでなくIDで指し、actorのpolicyで判定し、actor付きのeventを残すと決めた（goal 81）。

## Decision

1. **今の直接の使用の行き先。** inboxとplannerがcmuxを直接打つ手順は次のとおりに移す。runtime自身（supervisor・session wrapper・askの`cmux notify`）のcmuxの使い方は変えない。

   | 今の使用 | 行き先 |
   | --- | --- |
   | inboxがplannerに依頼を打ち込む`cmux send`（依頼のファイルを指す文）と、届いたかを見る`read-screen` | CLI: plannerへの依頼（決定2）と画面を読む操作（決定4） |
   | `dagq-recover`の`reference/session.md`・`stuck-exit.md`・`stalled.md`の`read-screen`・`send`・`send-key`（`stuck_exit`・`answer_prompt`・`stalled`、`send the answer of ask <id>`、`recover by hand`） | CLI: 画面を読む操作と送る操作（決定4・5） |
   | `session.md`の`cmux workspace close`（`triage by hand`のrun、supervisorが居ないとき） | CLI: 終わったrunのworkspaceの片付け（決定6） |
   | `doctor.md`の`cmux workspace list`でworkspaceを探して`/exit`を打つこと（`recover`の前） | CLI: `show`の`workspace_id`は読むだけにし、`/exit`は送る操作でrunのIDに送る |
   | `up-down.md`の`workspace-action --action unpin`と`workspace close`（pinされたinboxを閉じる）、`up`が忘れた退役roleのworkspaceの`workspace close` | 人自身のterminal（`DAGQ_ROLE`なし）に残す。queueがもうIDを持たないか、inbox自身を閉じる操作で、まれ |
   | AGENTS.mdの使い捨てqueueのスモークの`cmux workspace-group delete`と、[manual-smoke](../design/manual-smoke.md)の`send-key`・`workspace close` | 人自身のterminalに残す。本番queueのIDで指せない別のqueueの後始末か、人の手動スモークの手順 |
   | AGENTS.mdと`up-down.md`の`cmux workspace env`（inboxのworkspaceのenvの確認） | 人自身のterminalに残す。runの操作ではない読むだけの確認で、まれ |
   | `dagq-inbox`の`cmux notify`の記述 | runtimeの通知の説明で、inboxが打つものではない。変えない |

2. **plannerへの依頼。** 依頼はplannerのIDを指すCLIで渡す。CLIは依頼のファイルをそのplannerのディレクトリの下に写し、写した先を指す決まった文だけを、supervisorが生きているsessionに送るのと同じ送信と確認の経路で送る。依頼の本文はキー列として打たない。宛先は生きている対話のplannerだけで、閉じた・`lost`のplannerへの依頼は拒む。依頼ごとに新しいruntimeのplannerを立てる経路（task 454が予約したADR-0065。まだmainに無い）が入れば、新しい依頼の既定はそちらにし、このCLIは開いているplannerへの続きの依頼に使う。両者はファイルを写して指す同じ受け渡しの形にそろえる。

3. **plannerを閉じる。** task 695が足す`planner close [ID]`を、inboxが他のplannerのIDに使えるようにする。plannerはIDを省いた自分自身だけを閉じられ、他のplannerは閉じられない。

4. **画面を読む。** runとplannerのsessionの画面を、runのIDかplannerのIDで指して読むCLIを足す。読む行数には上限を置き、上限を超える指定は上限に切る。非対話のrun（[非対話のworker](../design/supervisor-lifecycle/headless-worker.md)）は画面を持たないので、画面の代わりに画面が無いことを返す（turnの出力はrun dirの`turns/`を読む）。読むことも決定7のeventを残すので、この操作は状態を変えるコマンドとしてqueueを書き込みで開き、本番queueでは固定バイナリで打つ（読み取りだけのコマンドの扱い（ADR-0073決定5・7・18）には入らない）。

5. **sessionに送る。** 送る操作は自由な文やキー列を通さない。送れるのは、(a) 決めたキーの集合（ダイアログの選択と確定、入力欄に残った文の送信、`/exit`のような決まった1語）と、(b) そのrunかplannerに紐づく、答えられたaskの答えの文（askのIDで指し、CLIがqueueから答えを読み、workerへの答えはsupervisorが答えを届けるときと同じ形で打つ）だけ。(b)は`stuck_exit`・`answer_prompt`・`stalled`の答えと`send the answer of ask <id>`を置き換える。`stalled`の`intervene`のように画面を読んでから人が書く指示も、先に指示の文を答えにしたaskとして記録してから(b)で送る（同じaskに答え直すか、そのrunに新しいaskを開いて答えるかは実装のtaskが決める）。runtimeの文が失われたとき（`input_not_ready`・`send_unconfirmed`）の打ち直しはruntimeの送り直しに任せ、人が打つならその文を答えにしたaskを経る。送る前と後の画面の確認、入力欄に残ったときのEnterの送り直しは、supervisorの送信（[sessionへの送信と確認](../design/supervisor-lifecycle/session-send.md)）と同じ規則で行う。非対話のrunには送れない（答えは今までどおり`answer`でsupervisorが次のturnとして送る）。

6. **終わったrunのworkspaceの片付け。** supervisorの終わったrunのworkspaceの掃除（[Run workspaces](../design/supervisor-lifecycle/run-workspaces.md)）と同じ条件で、終わったrunの開いたworkspaceのうちcmuxがまだlistしているものを閉じるCLIを足す。supervisorが居なくても動く。runのIDを指せば、そのrunだけを、sweepが除くtriageの対象（`triage by hand`）でも、生きているsession・wrapper・leaseが無いときに閉じる。IDを指さなければsweepの対象を全部閉じる。生きているrunのworkspaceは閉じない。

7. **宛先・判定・記録。** 決定2〜6の操作はどれもrun・planner・askのIDで宛先を指し、workspaceのUUIDを引数に取らない（UUIDはqueueの記録から引く。[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。どれもapplicationの境界でADR-t728-1のdefault denyのpolicyに通し、拒めば`authorization_denied`を残す。成功した操作は、画面を読むことも含めてactor（roleとid）付きのeventを残す。eventに画面の中身は載せない。許すactorは次のとおり（supervisorは自分の経路を持ち、これらのCLIを使わない）。

   | 操作 | user | inbox | planner | ほかのrole |
   | --- | --- | --- | --- | --- |
   | plannerへの依頼 | 許す | 許す | 拒む | 拒む |
   | plannerを閉じる | 全て | 全て | 自分だけ | 拒む |
   | 画面を読む（runとplanner） | 許す | 許す | 拒む | 拒む |
   | sessionに送る | 許す | 許す | 拒む | 拒む |
   | 終わったrunのworkspaceの片付け | 許す | 許す | 拒む | 拒む |

   inboxをuserと同じにするのは[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)の人の言葉での代行で、区別は記録のactorが持つ。plannerを拒むのは、今のpolicyでplannerがrunに対してnoteしか書けないこととそろえるため。

8. **名前。** 隠しサブコマンド`dagq session`（runのsession wrapper）と衝突させないため、新しいCLIは`session`を名前に使わず、宛先のIDの種類（runかplanner）の名詞の下に置く（plannerはtask 695の`planner close`と同じ群）。綴り・キーの集合・行数の上限・eventのkind・capabilityの名前は実装のtaskが[docs/design/](../design/)に書く。

ADR-t728-1の決定は変えない。決定5はpolicyの表の中身をdesignに置き、決定2〜6の操作はその表に新しいcapabilityを足すだけで、今の運用の権限を狭めない（cmuxを打てなくすることは[ADR-t1228-2](2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)で、dagqのpolicyの外のguardrailである）。

## Alternatives

- **cmuxをそのまま使い、判定と記録をあきらめる**: prompt injectionされたinboxやplannerが任意のsessionに任意の文を打てる。ふさぐのが目的。
- **cmuxの汎用の中継（`dagq cmux ...`）**: 判定と記録は付くが、自由な文とキー列が通り、宛先もUUIDのままになる。操作の種類と送れるものを絞ることにならない。
- **送る操作に自由な文を許す**: 人の答えをaskに記録せずに打てる抜け道になる。答えはまず`answer`でaskに残し、その答えだけを送る。
- **plannerにも画面を読む・送る操作を許す**: plannerはrunの状態を変えない役割で、人がplannerから復旧を頼む場合も、その操作はinboxか人が行えば足りる。
- **`dagq session screen` / `send`の綴り**: 既存の隠しサブコマンドと衝突し、parseと認可の写しが曖昧になる。
- **終わったrunの片付けを`up`だけに任せる**: supervisorを起こせない（起こしたくない）ときに片付けられず、2026-10-01のような手作業が残る。
- **残す使用もCLIにする（inboxのunpin、使い捨てqueueのgroup）**: queueがIDを持たない対象で宛先をIDで指せず、まれな人の手作業のためにUUIDを取る口を足すことになる。

## Consequences

- `dagq-recover`・`dagq-inbox`・`dagq-planner`のskillとAGENTS.mdの手順はCLIを使うように書き換える（goal 81の後続のtask）。人自身のterminalに残した使用は、そのterminalで打つと手順に書く。
- 人が答えをaskに残さずにworkerへ文を打つ手順は無くなる。runtimeの文が失われたときに打ち直す手順も、runtimeの送り直しか、その文を答えにしたaskに置き換わる。
- 画面を読むことにもeventが残るので、誰がどのsessionを見たかが追える。
- host実行の判定は助言的（ADR-t728-1決定6）のままで、これらのCLIは記録と誤操作の防止の境界であり、sandboxではない。
