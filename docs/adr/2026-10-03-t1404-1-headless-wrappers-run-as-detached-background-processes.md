---
id: adr-t1404-1
type: adr
title: 非対話の worker と runtime の planner の session wrapper を、設定で選べば cmux の workspace なしで supervisor から切り離した background の process として動かし、pid と起動時刻と heartbeat で識別し、signal で止め、出力を run dir の log と CLI で読む（ADR-t813-1 決定 3・4・5 と、workspace を前提にした ADR-0052・0054・0049・0047・0026・0048・0022 の決定を amends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amends:
  - adr-t813-1 decision 3
  - adr-t813-1 decision 5
  - adr-0052 decision 3
  - adr-0054 decision 1
  - adr-0054 decision 9
  - adr-0049 decision 3
  - adr-0047 decision 1
  - adr-0047 decision 6
  - adr-0047 decision 7
  - adr-0047 decision 12
  - adr-0047 decision 13
  - adr-0047 decision 16
  - adr-0047 decision 34
  - adr-0026 decision 2
  - adr-0048 decision 6
  - adr-0048 decision 7
  - adr-0049 decision 2
  - adr-0049 decision 6
  - adr-0022 decision 2
  - adr-t813-1 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - worker
  - cmux
related:
  - adr-t813-1
  - adr-t1340-1
  - adr-t1091-1
  - adr-t598-1
  - adr-0022
  - adr-0026
  - adr-0039
  - adr-0047
  - adr-0048
  - adr-0049
  - adr-0052
  - adr-0054
  - adr-0073
  - plan-zero-based-headless-readiness
---

# ADR-t1404-1: 非対話の worker と runtime の planner の session wrapper を、設定で選べば cmux の workspace なしで supervisor から切り離した background の process として動かし、pid と起動時刻と heartbeat で識別し、signal で止め、出力を run dir の log と CLI で読む（ADR-t813-1 決定 3・4・5 と、workspace を前提にした ADR-0052・0054・0049・0047・0026・0048・0022 の決定を amends）

## Context

[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md) 決定 3 は、非対話の turn を run の cmux の workspace の session wrapper の中で動かすとした。理由は (a) supervisor の再起動・crash・exec の引き継ぎを越えて turn を生かす、(b) 人が出力を見られる、(c) wrapper の adopt の規則を使える、で、Alternatives が退けたのは process を supervisor の子にする案だけだった。supervisor から切り離した process は検討していない。

[ADR-t1340-1](2026-10-02-t1340-1-claude-worker-defaults-to-headless.md) で Claude の worker の既定が非対話になり、画面も入力欄も無い run が run ごとに workspace を作っている。[zero-based-headless-readiness](../plans/zero-based-headless-readiness.md) の E1・E3 は、待ちの間も run ごとに wrapper と workspace を抱えること、閉じた記録の無い workspace（ある窓で 85 個）を数えた。負荷の下では cmux の呼び出しが時間切れになり（capture・`backend_call_failed`）、cmux への依存が Linux（goal 83）・他の repository（goal 52）・コンテナ（goal 82）を妨げる。

2026-10-02 に人が「worker headless で workspace が新しく作られているが意味ある？ supervisor が background で起動したらいいような」と提起し、planner と workspace をやめる方針で合意した（goal 89）。(a)(c) は切り離した process でも満たせ、(b) は `turns/` と `dagq timeline` に残る。

## Decision

1. **起動（ADR-t813-1 決定 3 を amends）。** background を選んだ非対話の session（worker の最初の session・`needs_session` の resume・待ちの最中に失った session の開き直し・対話の run から provider を切り替えて始める非対話の session（[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md) 決定 5）、非対話の runtime の planner）では、supervisor は workspace を作らず、session wrapper を自分の子でない切り離した process（新しい session と process group を持ち、親は 1 になる）として起動する。wrapper は supervisor の再起動・crash・exec の引き継ぎより長く生き、ADR-t813-1 決定 3 の理由 (a)(c) を workspace なしで満たす。launchd mode と in-cmux mode のどちらでも同じ形にし、supervisor の mode で分けない（launchd の job の停止や in-cmux の supervisor の workspace の close が wrapper を巻き込まないことは実装が確かめる）。wrapper は TTY を持たず、stdin を閉じて起動する。revise は今までどおり生きている session に送り、新しい wrapper を起動しない。turn の process group を分けて止める決定 3 の最後の文と、turn の駆動（`turns/` の依頼・idle marker・`Turns`）は変えない。
2. **識別と生死。** background の wrapper は、pid・OS が記録した起動時刻（pid の再利用に備える）・今の wrapper の heartbeat で識別する。生きているとは、その pid の process が記録した起動時刻のまま居ることとし、heartbeat の新しさは今の「黙った wrapper」の判定に使う。adopt・引き継ぎ・開き直し・掃除は workspace の UUID でなくこの組で行う（[ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md) の「生きている wrapper の lease を引き継ぐ」はそのまま効く）。起動時刻が合わない pid は別の process とみなし、signal を送らない。
3. **停止。** 今 workspace の close（hangup）で wrapper を止めている経路（review の後、`stalled` の `stop`、cancel、復旧 job の `stop_processes`、run の後始末と掃除、開き直しの前）は、background の run では、まず今の終了の依頼で wrapper に自分で終わらせ、終わらなければ識別した wrapper への signal にする。まず wrapper の pid に終了の signal を送り、wrapper は今の終了の signal の扱い（自分が起動した turn の group を止める）を保つ。猶予の後も残っていれば、wrapper の process group と、wrapper が起動を記録した turn の process group に強制の signal を送る（turn は wrapper と別の group で走るので、wrapper の group だけでは turn が残る）。終わった run に wrapper が残っていれば、掃除が同じ手順で止める。heartbeat が切れたが生きている wrapper（黙った wrapper）は、今と同じく runtime から kill せず、上の経路のどれかに入ったときだけ止める。名前やパターンで選ばない。
4. **ADR-t813-1 決定 5 の読み替え（amends）。** 決定 5 の「最後の turn の終わりに workspace の close だけを同じ時点で行う」は、background の run では「最後の turn の終わりに wrapper の process が終了の依頼で終わり、何も残らない」と読む。runtime は同じ時点で workspace を閉じる代わりに、wrapper が終わったことを識別の組で確かめ、残っていれば決定 3 で止める。workspace を選んだ run では決定 3・5 のまま。
5. **env。** workspace の `--env` で渡していた `DAGQ_*` と、`dagq.toml` の `[run.env]` は、wrapper の process の env で渡す（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md) 決定 3 の「workspace の `--env` で渡す」を、background の run では process の env と読む。amends）。agent の env を wrapper が組み立てる規則（queue service の socket と token、`DAGQ_QUEUE` を外す）は変えない。
6. **出力。** wrapper が terminal に出していた `[dagq]` の要約は、run dir の log に書き、人は dagq の CLI でその log を読み、追う（綴りは design）。runtime は background の wrapper に画面を付ける手段（見るための workspace を開く）を持たない。画面で見たい人は、自分の terminal でその CLI を打つか、その task に対話の経路（`--interactive`）を選ぶ。
7. **切り替え。** workspace と background は `dagq.toml` の設定で選び、runtime の既定は評価が済むまで workspace のままにする。まずこの repository の `dagq.toml` だけを background に切り替え、既定を変えるかは評価の後に別の ADR で決める。設定は session の wrapper を起動する時点で読み、動いている wrapper は動かさない。
8. **非対話の runtime の planner も同じ形。** goal 87 の非対話の runtime の planner（ADR-t1394-2）も、同じ設定で background を選べば、決定 1〜6 と同じ起動・識別・停止・env・出力で動かす。非対話の runtime の planner そのものは ADR-t1394-2 が決め、この ADR は wrapper の置き場所だけを決める。runtime の planner の workspace を前提にした決定の読み替えは決定 10 に書く。
9. **cmux と lifecycle の ADR との関係（ADR-0052 決定 3、ADR-0054 決定 1・9 を amends）。** cmux は対話の run・inbox・人が開く planner・in-cmux mode の supervisor の workspace backend として残るが、background を選んだ非対話の session は cmux の外で動く（ADR-0052 決定 3 の「その中で agent session を動かす」から外れる）。supervisor の provision は background の run では workspace の作成の代わりに wrapper の起動を行い、後始末は workspace の close の代わりに決定 3・4 の確認と停止を行う（ADR-0054 決定 1）。決定 9 の provisioning の失敗には、background の wrapper を起動できない（process を作れない）ことを含める。
10. **workspace を前提にした他の決定の読み替え（amends）。** 次の決定は、worker の run か runtime の planner の session が workspace を持つことを前提に書かれている。background の session では、workspace を決定 2 で識別する wrapper に置き換えて読む。workspace を選んだ session、対話の session、inbox、人が開く planner には元のまま効く。
   - **定義と起動**: 「run ごとに supervisor が開くオンデマンドの workspace」（worker）と「オンデマンドの workspace」（planner）は、run か proposal ごとに supervisor が起動するオンデマンドの session（workspace の中か、background の wrapper）と読み、「新しい planner の workspace を立てる」は新しい background の wrapper を決定 1 で起動することと読む（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) 決定 1・12）。
   - **識別と生死**: 「workspace の UUID で識別する・proposal の持ち主に結び付ける」「workspace が生きている・閉じている・`cmux workspace list` に居る」は、wrapper の識別の組と、その wrapper が生きているか（決定 2）と読む。ADR-0047 決定 6（runtime が立てた planner の識別と title）・決定 7（proposal の持ち主）・決定 12（持ち主の planner が生きているか）・決定 13 と決定 16（planner が居なければ新しい planner を立てる）・決定 34（`workspace_mismatch` は background の run では workspace の一覧でなく wrapper の生死で見る）、[ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md) 決定 7（`runtime_planner` の区間を推定で閉じる判定）。
   - **送る**: 「workspace や terminal に answer を送る」は、同じ session の次の turn の依頼にすることと読む（ADR-t813-1 決定 2 と同じ）。ADR-0047 決定 13・16、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md) 決定 2。
   - **閉じる・残す**: 「終わった workspace を閉じる」は wrapper を決定 3・4 で終わらせることと、「review の間 worker の session と workspace を閉じずに残す」は wrapper を止めずに残すことと読む。ADR-0047 決定 13、ADR-0049 決定 2。
   - **env**: 「workspace の `--env` に置く」「worker の workspace に渡る」は、wrapper の process の env で渡すことと読む（決定 5）。[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md) 決定 2、ADR-0048 決定 6、ADR-0049 決定 6。
   - **出力**: 「非対話の run の terminal は出力を見せるだけ」は、background の run には terminal が無く、出力は決定 6 の log で読むことと読む。人が worker に打ち込めず ask と answer で伝えることは変わらない（ADR-t813-1 決定 4）。

対話の経路（`--interactive` の worker、inbox、人が開く planner）は workspace のまま変えない。設定の欄名・既定値・event の kind と欄・log の file 名・CLI の綴りは [docs/design/](../design/) に書く。

## Alternatives

- **process を supervisor の子にする**（ADR-t813-1 が退けた案）: supervisor の再起動と crash で turn が死ぬ。切り離せば (a) を失わない。
- **workspace のまま、作る時点だけを遅らせる・待ちの間だけ閉じる**: E1 は減るが、cmux の時間切れ・閉じ忘れ・cmux への依存は残る。
- **launchd の job（run ごとの LaunchAgent）や systemd の unit にする**: OS ごとに別の仕組みが要り、Linux とコンテナの段で同じ形にならない。切り離した process は両方で同じ。
- **人が見たいときに viewer の workspace を開く手段を runtime に残す**: cmux への依存を非対話の経路に残し、閉じ忘れの workspace を作る道を戻す。log と CLI で足り、要るときは対話の経路を選べる。
- **既定をすぐ background にする**: 本番での切り替え後の比較（cmux の呼び出しの失敗・残った workspace・startup）がまだ無い。

## Consequences

- background の run は cmux の workspace を作らず、待ちの間に抱えるのは wrapper の process だけになる。閉じ忘れの workspace と、その run の cmux の呼び出しが無くなる。
- 人は background の run の出力を画面でなく CLI で読む。`run close-workspaces` と workspace の掃除は background の run には対象が無く、代わりに残った wrapper の process を止める掃除が要る。
- runtime は wrapper の置き場所を 2 通り（workspace と background）持ち、識別・停止・adopt の分岐が増える。
- `dagq.toml` の新しい欄を知らない古い固定バイナリは起動できないので、この repository の `dagq.toml` に欄を足すのは固定バイナリが対応してからにする。
- amends に挙げた決定（ADR-t813-1 決定 3・4・5、ADR-0052 決定 3、ADR-0054 決定 1・9、ADR-0049 決定 2・3・6、ADR-0047 決定 1・6・7・12・13・16・34、ADR-0026 決定 2、ADR-0048 決定 6・7、ADR-0022 決定 2）は、workspace を選んだ session と対話の session にはそのまま、background の session にはこの ADR の読み替えで効く。
