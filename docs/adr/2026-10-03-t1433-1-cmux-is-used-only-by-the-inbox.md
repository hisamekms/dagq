---
id: adr-t1433-1
type: adr
title: cmuxはinboxだけが使う。cmuxを呼ぶのはupがinboxのworkspaceを開く・確かめる・閉じることとinboxのsessionの中の操作だけにし、WorkspaceBackendをinboxのためのportに縮め、askの通知をinboxのwatchから出し、cmuxのfakeと実cmuxを要るtestをinboxを開くup / downだけにする（ADR-0052決定3・4などをamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amends:
  - adr-0052 decision 3
  - adr-0052 decision 4
  - adr-0010 decision 5
  - adr-0016 decision 4
  - adr-0022 decision 5
  - adr-0028 decision 1
  - adr-0047 decision 34
  - adr-0047 decision 42
  - adr-t1228-1 decision 1
  - adr-t1228-2 decision 7
  - adr-t1233-1 decision 1
  - adr-t963-1 decision 3
  - adr-t1233-2 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - cmux
  - inbox
  - testing
related:
  - adr-0052
  - adr-0016
  - adr-0022
  - adr-0026
  - adr-0028
  - adr-0031
  - adr-t1091-1
  - adr-t1228-1
  - adr-t1228-2
  - adr-t1233-1
  - adr-t1233-2
  - adr-t963-1
  - adr-t1433-2
  - adr-t1433-3
  - adr-t1433-4
  - adr-t1433-5
  - design-overview
  - design-supervisor-lifecycle-cmux-notify
---

# ADR-t1433-1: cmuxはinboxだけが使う（ADR-0052決定3・4などをamends）

## Context

[ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md)の決定3はcmuxを必須のworkspace backendにし、runtimeがworkspaceの中でworktreeとagent sessionを動かすとした。今はworker（対話の画面・打ち込み）、runtimeのplanner、非対話のrunのsession wrapperのworkspace、supervisorの起動時のpreflightとworkspace group、runのworkspaceの掃除、askの`cmux notify`（supervisorとqueue serviceが呼ぶ）、in-cmux modeのsupervisor、`stats`の`workspace_mismatch`がcmuxを呼ぶ。

2026-10-03の計測（goal 92のdescription）では、着地の関門のtestの時間の合計2,278秒のうち、対話に固有のtest（約230件）が約644秒、偽のcmux（`TestWorkspace`など）を経路として通るだけのtest（約400件）が約853秒を占め、e2eの15本はすべて実cmuxを要る。cmuxへの依存はLinux（goal 83）・他のrepository（goal 52）・コンテナ（goal 82）も妨げる。同じ日に人はplannerのsessionで「cmuxはinboxだけが使う」方針を採った（goal 92）。対話の経路の廃止は[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)、wrapperをbackgroundだけにすることは[ADR-t1433-3](2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、supervisorをcmuxなしで常駐させることは[ADR-t1433-4](2026-10-03-t1433-4-supervisor-resides-without-cmux.md)、inboxへの打ち込みをやめることは[ADR-t1433-5](2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)が決め、このADRはその全体の境界を決める。

## Decision

1. **cmuxを呼ぶのはinboxのためだけ（(a)。ADR-0052決定3・4、ADR-0010決定5、ADR-0028決定1、ADR-0047決定34、ADR-t1228-1決定1、ADR-t1228-2決定7、ADR-t1233-1決定1をamends）。** cmuxを呼んでよいのは、`up`がinboxのworkspaceを開く・確かめる・閉じること（`down`を含む）と、inboxのsessionの中の操作（[ADR-t1228-1](2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)のdagqのCLIと、inboxのsessionの子として動く`watch --role inbox`）だけにする。worker・runtimeのplanner・supervisor・queue service・observer・headlessのjob・session wrapperはcmuxを呼ばない。読み取りだけの診断（`stats`の`workspace_mismatch`、`doctor`のworkspaceの一覧など）もsupervisorとqueue serviceからは呼ばない。新しくcmuxを呼ぶ処理は足さない。
   - ADR-0052決定3の「runtimeはcmuxでworkspaceを作り、その中でGit worktreeとagent sessionを動かす」は「cmuxはinboxのworkspaceのbackendで、worktreeとagent sessionはcmuxの外で動く」と改める。cmuxが実行環境の前提になるのは、inboxを開く`up`と人がinboxで作業するときだけになる。
   - ADR-0052決定4の`WorkspaceBackend`のportは、inboxのworkspaceを開く・確かめる・閉じる・inboxへの通知のためのportに縮める。domain / applicationがcmuxを直接参照しないこと（portを介すこと）は変えない。
   - workspaceのtitle（ADR-0028決定1、ADR-0010決定5の名前）は、inboxの`[<repo>]inbox`だけが残る。supervisor・worker・plannerのtitleは対象が無くなる。
   - ADR-t1233-1決定1の「cmuxは制御側に置き、runner sessionのcmuxのTTY・キー送信・capture・Stop hookはそのまま動く」は、cmuxは制御側のうちinboxにだけ置くと改める。
2. **askの通知はinboxのwatchが出す（(b)。ADR-0016決定4、ADR-0022決定5、ADR-0047決定42をamends）。** supervisorとqueue serviceは`cmux notify`を呼ばない。`ask_opened`の人への通知は、inboxのsessionの中で動く`watch --role inbox`が`ask_opened`を見たときに出す。watchはcmuxのterminalの子なのでsocketのpasswordが要らず、宛先はwatchが動いているinbox自身になる。通知がsessionを起こさないこと、通知はaskのときだけでattention全般には出さないこと（ADR-0022決定5のまま）は変えない。認証とコストのaskを1件にまとめるときの「通知は最初の1回だけ」（ADR-0047決定42）も、その1回をinboxのwatchが出すと読む。watchが居ないあいだは通知も出ないので、その後ろ盾はADR-t1433-5が決める。
3. **testの方針（(c)。ADR-t963-1決定3、ADR-t1233-2決定4をamends）。** cmuxのfake（`WorkspaceBackend`のtestの実装）と実cmuxを要るのは、inboxを開く`up` / `down`のlifecycleのtestだけにする。ほかのintegration testとe2eは、cmuxの無いhostで、非対話でbackgroundの経路で流れる形にする。決定2のinboxのwatchの通知も例外にしない: `ask_opened`を見て通知を1回出すという判断はcmuxを呼ばないunit testで確かめ、cmuxへの通知の呼び出しそのものは、inboxを開く`up` / `down`のtestが使うのと同じinboxのportの実装に置いて、そのtestの範囲で確かめる。ADR-t963-1決定3の「e2eの範囲のactor（worker・planner・inbox・jobのsession）の起動」は、cmuxを要るものはinboxの起動だけと読み、ADR-t1233-2決定4の「実cmuxのworkspaceと実プロセスを使う」は、inboxを開く`up`のe2eだけが実cmuxを使うと読む。e2eを1本ずつ流すことと関門の位置は変えない。

残すもの: inboxとplannerにcmuxを直接打たせない[ADR-t1228-2](2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)のguardrail（cmuxを使う決定ではなく拒む決定）、inboxのpinとunpinの[ADR-0031](0031-color-pill-and-pin-for-inbox-and-planner-and-unpin-before-close.md)（`up`だけが行う）、人が自分のterminalで打つcmuxの手作業（ADR-t1228-1決定1の表の「人自身のterminalに残す」行。runtimeの使用ではない）、`up`のpreflightのcmuxの確認（[`up` / `down`](../design/supervisor-lifecycle/up-down.md)。`up`はinboxを開くので前提が要る）。e2eとLinuxのtestについての、[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1と[ADR-t1162-1](2026-09-30-t1162-1-e2e-gate-skips-podman-e2e-only-when-podman-is-unreachable.md)決定3（cmuxが使えずe2eを流せないときは関門を通さない）、[ADR-t1233-3](2026-10-02-t1233-3-this-repository-passes-on-linux-before-containers.md)決定1・3（実cmuxに依るtestはLinuxで流さないか別に確かめ、e2eはLinuxの対象に含めない）、[ADR-0078](0078-one-integration-test-binary.md)決定2（cmuxが要るe2eは別のbinary）も残す。どれも決定3の後もinboxを開く`up`のe2eが実cmuxを要ることと両立し、残りのe2eをLinuxに含めるかはgoal 83の段で決める。移行の間だけの例外として、新しいバイナリが登録済みのin-cmuxのsupervisorを引き継ぎ、`down`がそのworkspaceを閉じること（[ADR-t1433-4](2026-10-03-t1433-4-supervisor-resides-without-cmux.md)決定3）も残す。人がlaunchdに移せば無くなり、新しく起動するsupervisorには無い。人が開くplannerを`up`が開くこと（ADR-0022決定4、ADR-0047決定6）は[ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)が人のplannerを廃止したことで対象が無い。ADR-t1394-1が廃止の時点で開いていた人のplannerに残した扱い（workspaceを動かし続け、reviseを配送し、終わったらruntimeが閉じる。決定1・7・9）は、[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5がamendsし、runtimeはそのworkspaceにcmuxを呼ばない。

実装はgoal 92の後続のtaskが行う。portの形・通知の文面・testの置き場所は[docs/design/](../design/)に書く。

## Alternatives

- **対話の経路を残し、cmuxのfakeを使うtestだけを減らす**: fixtureの既定を非対話にすればtestの時間は一部減るが、対話の画面・ダイアログ・打ち込みの処理とそのtest、run・supervisorのcmuxの呼び出しが残り、cmuxの無いhost（Linux・コンテナ）では動かないままになる。
- **`WorkspaceBackend`を汎用のportのまま残し、tmuxなど別のbackendを足す**: 非対話の経路は画面を要らないので、別のbackendを足しても守るものが無い。inboxは人が見るsessionなので、cmuxの1つで足りる。
- **askの通知をsupervisorに残す**: launchdのsupervisorがcmuxを呼ぶためにsocket passwordとpreflightが要り続け（ADR-0011）、supervisorをcmuxから外せない。
- **通知をやめる**: 人がinboxを見ていないときにaskに気付く手段が無くなる。inboxのwatchが出せば、cmuxを呼ぶのはinboxの中だけで済む。

## Consequences

- supervisor・queue service・worker・planner・jobはcmuxの無いhostで動き、in-cmux modeとrunのworkspaceの掃除が無くなる（ADR-t1433-3・ADR-t1433-4）。
- runtimeのintegration testは偽のcmuxを経路として通らなくなり、e2eはinboxを開く`up`のtest以外がcmuxなしで流れる。testの時間の前後はgoal 92の測定のtaskがdocs/plansに残す。
- inboxのwatchが動いていないあいだは`cmux notify`も出ない。watchの生存の保証はADR-t1433-5が持つ。
- amendsに挙げた決定は、inboxについては元のまま、それ以外についてはこのADRのとおり読む。
