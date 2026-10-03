---
id: adr-t1433-3
type: adr
title: workerとruntimeのplannerの非対話のsession wrapperは、supervisorから切り離したbackgroundのprocessだけで動かす。dagq.tomlの切り替えの欄は受け付けて無視してから消し、runのworkspaceの掃除・workspace group・descriptionをやめ、run screenをturnのlogのCLIに置き換え、task 1409の判定を退行の確認として読む（ADR-0018を置き換え、ADR-t1404-1決定7・9・10などをamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
supersedes:
  - adr-0018
amends:
  - adr-t1404-1 decision 7
  - adr-t1404-1 decision 9
  - adr-t1404-1 decision 10
  - adr-t1394-2 decision 2
  - adr-t1394-2 decision 3
  - adr-t813-1 decision 3
  - adr-t813-1 decision 4
  - adr-t813-1 decision 5
  - adr-0026 decision 2
  - adr-0026 decision 3
  - adr-0026 decision 4
  - adr-0028 decision 2
  - adr-0049 decision 3
  - adr-0054 decision 1
  - adr-0054 decision 9
  - adr-0048 decision 6
  - adr-0048 decision 7
  - adr-t1228-1 decision 6
  - adr-t1300-1 decision 2
  - adr-0068 decision 4
  - adr-0039 decision 4
  - adr-0039 decision 5
  - adr-0053 decision 9
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - worker
  - cmux
related:
  - adr-t1404-1
  - adr-t813-1
  - adr-0018
  - adr-0026
  - adr-0028
  - adr-0048
  - adr-0049
  - adr-0054
  - adr-t1228-1
  - adr-t1300-1
  - adr-0039
  - adr-0053
  - adr-0068
  - adr-t1091-1
  - adr-t1433-1
  - adr-t1433-2
  - design-supervisor-lifecycle-headless-worker
  - design-supervisor-lifecycle-run-workspaces
---

# ADR-t1433-3: 非対話のsession wrapperはbackgroundだけで動かす

## Context

[ADR-t1404-1](2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)は、非対話のworkerとruntimeのplannerのsession wrapperを、`dagq.toml`で選べばcmuxのworkspaceなしでsupervisorから切り離したbackgroundのprocessとして動かすとした（決定1〜6・8）。決定7は既定をworkspaceのまま残し、この repositoryだけをbackgroundにして、既定を変えるかを評価（goal 89のtask 1409）の後に別のADRで決めるとした。決定9・10は、workspaceを選んだsessionに元の決定が効く読み分けを持つ。

2026-10-03に人は「cmuxはinboxだけが使う」方針を採り（goal 92、[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）、対話の経路も廃止した（[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)）。workspaceの経路を残すと、runのworkspaceの作成・掃除・workspace group・偽のcmuxを通るtestが残る。runのworkspaceの名前を決める[ADR-0018](0018-run-workspace-named-after-the-task.md)は決定1・2・4がrunのworkspaceだけを対象にし、決定3もすでにADR-0021・0028に上書きされているので、丸ごと置き換える。

## Decision

1. **wrapperはbackgroundだけにする（ADR-t1404-1決定7・9・10、ADR-t813-1決定3・4・5、ADR-t1394-2決定2・3をamends）。** workerとruntimeのplannerの非対話のsession wrapperは、ADR-t1404-1決定1〜6・8の形（切り離したprocess、pidと起動時刻とheartbeatの識別、終了の依頼とsignalの停止、processのenv、run dirのlog）だけで動かす。workspaceの中でwrapperを動かす経路は無くす。
   - ADR-t1404-1決定7の「workspaceとbackgroundを`dagq.toml`で選び、既定はworkspace」は廃止する。決定9の「cmuxは対話のrun・inbox・人が開くplanner・in-cmux modeのsupervisorのbackendとして残る」はinboxだけと読み、決定9・10の「workspaceを選んだsessionには元のまま効く」の読み分けは無くなる。決定10の読み替えはすべてのworkerとruntimeのplannerのsessionに効く。
   - ADR-t813-1決定3の「run のworkspaceのsession wrapperの中で動かす」、決定4の「非対話のrunのterminalは出力を見せるだけ」、決定5の「最後のturnの終わりにworkspaceを閉じる」は、ADR-t1404-1決定1・6・4のbackgroundの読み替えだけが効く。
   - runtimeのplannerについての[ADR-t1394-2](2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定2の「wrapperの置き場所（workspaceの中か、workspaceなしの切り離したprocessか）を同じ設定で選ぶ」と、決定3の「workspaceの中ならworkspaceとwrapper」で生死を決めることは、backgroundとADR-t1404-1決定2の識別だけになる。
2. **切り替えの欄の扱い。** 新しいバイナリは`dagq.toml`の切り替えの欄を受け付けて無視し（値に関わらずbackground）、警告で無視したことを示す。古い固定バイナリが欄の無い`dagq.toml`を読めることを確かめてから、別のtaskでこの repositoryの`dagq.toml`から欄を消す。欄を拒まないのは、走っている本番のsupervisorと固定バイナリの入れ替えの順で`dagq.toml`が読めなくならないため（goal 92のconstraints）。
3. **runのworkspaceの掃除をやめる（ADR-0026決定2・3・4、ADR-0028決定2、ADR-0049決定3、ADR-0054決定1・9、ADR-0048決定6・7、ADR-t1228-1決定6、ADR-t1300-1決定2、ADR-0068決定4、ADR-0039決定4・5、ADR-0053決定9をamends）。** supervisorはrunとplannerのworkspaceを作らないので、終わったrunのworkspaceのsweep・`run close-workspaces`の片付け・queueのworkspace group（`ensure_group`）・runのworkspaceのdescription・resumeのworkspaceのtitleは対象が無く、やめる。代わりの後始末はADR-t1404-1決定3の残ったwrapperの停止だけにする。envはprocessのenvで渡し（ADR-0049決定3、ADR-0026決定2、ADR-0048決定6）、provisionはwrapperの起動、その失敗はwrapperを起動できないこと（ADR-0054決定1・9）、区間を推定で閉じる判定とplannerを閉じた記録の「workspaceが消えた」はwrapperの生死（ADR-0048決定7、ADR-t1300-1決定2）と読む。過去に作られて残ったworkspaceは、人が自分のterminalで閉じる（ADR-t1228-1決定1の表の人自身のterminalの行と同じ扱い）。
   - sessionの終わりを「`/exit`してworkspaceを閉じる」と書く決定は、終了の依頼でwrapperに終わらせ、終わったことを識別の組で確かめ、残っていれば[ADR-t1404-1](2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3の手順で止めることと読む。[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)決定4の「resumeが解決したrunは閉じていない`approve_landing`のaskがあれば、sessionを`/exit`してworkspaceを閉じ、leaseを手放して答えを待つ」は、wrapperをこの手順で終わらせてからleaseを手放すと読む。askを閉じないこと、reviewをやり直さないこと、答えの適用は変えない。
   - [ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)決定4の「slotを`workspace_id`から組み立て直し、`exit_requested`があれば`/exit`を再送しない」は、slotをwrapperの識別の組（ADR-t1404-1決定2）から組み立て直し、終了の依頼を置き直さずに終わりの待ちを数え直すと読む。決定5の「`exited_at`を記録済みのsessionに`/exit`を送らない（cmuxが`send`を拒んでもrunを手放さないため）」は、終わったwrapperに終了の依頼もsignalも送らないと読む。引き継ぎの条件・トランザクション・tokenの付け替え・leaseを失った側が退くこと（決定1〜3・5・7）は変えない。
   - [ADR-0053](0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md)決定9の「走行中のrunのcmux workspaceは旧pathの`runner`と`--db`で動いている」は、走行中のrunのbackgroundのwrapperが旧pathの`runner`と`--db`で動いていると読む。runを走らせたままqueueのディレクトリを動かさないことは変えない。
4. **run screenをturnのlogのCLIに置き換える。** runとplannerの画面を読む操作（ADR-t1228-1決定4、ADR-t1433-2決定5）は、task 1406のturnのlogを読み・追うCLIに置き換える。
5. **task 1409の評価の読み方。** goal 89のtask 1409の1週間の判定は、workspaceに戻すかの判断ではなく、backgroundの経路の退行（残ったprocess・startup・wrapperを起動できない失敗・識別の誤り）の確認として読む。退行が見つかれば、backgroundの経路を直すtaskにする。

ADR-0018の決定のうち引き継ぐものは無い（決定3の名前はADR-0021・ADR-0028が上書き済みで、inboxのtitleだけがADR-0028決定1に残る）。domain / applicationがcmuxの書式を知らないこと（決定4の後半）は[ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md)決定4とADR-t1433-1決定1が持つ。

実装はgoal 92の後続のtaskが行う。欄名・警告の文面・CLIの綴りは[非対話のworker](../design/supervisor-lifecycle/headless-worker.md)と[Run workspaces](../design/supervisor-lifecycle/run-workspaces.md)に書く。

## Alternatives

- **workspaceの経路を設定で残し続ける**: 使う人の居ない経路のために、workspaceの作成・識別・掃除・groupの分岐とそのtest、偽のcmuxがruntimeに残る。戻し先としての価値は、評価を退行の確認として読むことで足りる。
- **評価（task 1409）の後に決める**: 人は戻す選択肢を先に閉じると決めた。評価は退行を見つけるために残る。
- **切り替えの欄をすぐ拒む**: `dagq.toml`に欄を持つ本番のqueueが、新しいバイナリへの入れ替えの直後に起動できなくなる。
- **見るためのworkspace（viewer）を残す**: ADR-t1404-1が退けたとおり、cmuxへの依存と閉じ忘れのworkspaceを戻す。

## Consequences

- runとplannerのためのcmuxの呼び出しが無くなり、run dirとwrapperのprocessだけが残る。
- workspaceの経路にだけある分岐とtest（runのworkspaceのfake、sweep、group、description）を消せる。
- `dagq.toml`の欄は一時的に意味の無い欄として残り、消すのは後のtaskになる。
