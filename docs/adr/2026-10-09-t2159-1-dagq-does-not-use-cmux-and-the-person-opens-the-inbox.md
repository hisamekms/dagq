---
id: adr-t2159-1
type: adr
title: dagqはcmuxを呼ばず前提にしない。inboxは人が自分のterminalで`dagq inbox`で開き、`up`はinboxを開かず、askの通知は`[push]`にし、inboxとplannerのsettingsから`Bash(cmux:*)`を外す（ADR-t1433-1・ADR-t2105-1・ADR-0031・ADR-t1228-2・ADR-0026・ADR-0028を置き換え、ADR-0052決定3・4・7・8などをamends）
status: accepted
created: 2026-10-09
updated: 2026-10-09
accepted_on: 2026-10-09
supersedes:
  - adr-t1433-1
  - adr-t2105-1
  - adr-0031
  - adr-t1228-2
  - adr-0026
  - adr-0028
amends:
  - adr-0052 decision 3
  - adr-0052 decision 4
  - adr-0052 decision 7
  - adr-0052 decision 8
  - adr-0010 decision 5
  - adr-0022 decision 4
  - adr-0022 decision 5
  - adr-t1433-4 decision 1
  - adr-t1433-4 decision 3
  - adr-t963-1 decision 3
  - adr-t1233-2 decision 4
  - adr-t2125-1 decision 1
  - adr-t2125-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - cmux
  - inbox
  - testing
  - operations
related:
  - adr-t1433-1
  - adr-t1433-4
  - adr-t1433-5
  - adr-t2105-1
  - adr-t2125-1
  - adr-t1228-1
  - adr-t1228-2
  - adr-0010
  - adr-0016
  - adr-0022
  - adr-0026
  - adr-0028
  - adr-0031
  - adr-0047
  - adr-0052
  - adr-t963-1
  - adr-t1233-1
  - adr-t1233-2
  - adr-t1091-1
  - design-supervisor-lifecycle-up-down
  - design-supervisor-lifecycle-notification-route
---

# ADR-t2159-1: dagqはcmuxを呼ばず前提にしない。inboxは人が`dagq inbox`で開き、askの通知は`[push]`にする

## Context

[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)はcmuxをinboxだけが使うとし、worker・runtimeのplanner・supervisor・queue service・observer・jobはcmuxを呼ばなくなった。残るのは`up`のcmuxのpreflightとinboxのworkspaceの作成・再利用・色・status pill・pin・group、`down`のinboxと登録済みのin-cmuxのsupervisorのworkspaceのclose、`watch --role inbox`の`ask_opened`の`cmux notify`、e2eの関門のcmuxの確認と後始末（[ADR-t2105-1](2026-10-08-t2105-1-e2e-gate-skips-cmux-e2e-only-when-cmux-does-not-answer.md)）と、それを守る`Bash(cmux:*)`のdeny（[ADR-t1228-2](2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)）である。これがあるかぎりdagqはcmuxの無いhost（Linux・コンテナ・他のterminal）で全部は動かず、testとe2eにcmuxのfakeと実cmuxが残る。

2026-10-08に人は残りのcmuxへの依存をinboxも含めて全部取り除くと決めた（request 84）。runtimeは既にinboxのterminalに打ち込まず（[ADR-t1433-5](2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)決定2）、inboxへの知らせはpullの`watch --role inbox`が運び、watchの不在の後ろ盾は`host.toml`の`[push]`を使う。inboxがcmuxのworkspaceである理由は、`up`が開くことと`cmux notify`の宛先であることだけになっている。本番のsupervisorは2026-10-08時点でlaunchd modeで、in-cmuxの登録は無い。

## Decision

1. **dagqはcmuxを呼ばず、実行の前提にしない。** runtime・CLI・test・e2e・pluginはcmuxを呼ばない。新しくcmuxを呼ぶ処理を足さない。workspaceのport（`WorkspaceBackend`）は無くす。[ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md)決定3・4はこの決定に改まり、domain / applicationが外部のツールをportを介して使う原則は他のportについてそのまま。ADR-0052決定7の表の環境変数の例から`DAGQ_E2E_CMUX`を除き、cmuxのworkspaceのtitleと識別を送る注記は対象を失う。決定8の「cmuxとGit worktreeで動く」は「Git worktreeで動く」と読む。[ADR-0010](0010-maintainer-and-resident-supervisor.md)決定5のworkspace名は対象を失う。[ADR-0016](0016-maintainer-notification-and-compact-output.md)決定4・[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定34・42・[ADR-t1228-1](2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)決定1・[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定1でADR-t1433-1が改めた部分の今の決定は、この決定1と決定4が持つ。
2. **inboxは人が自分のterminalで`dagq inbox`を打って開く対話のsessionにする（ADR-0022決定4をamends）。** terminalの種類（cmux・Terminal.app・iTerm・IDEなど）を問わない。`dagq inbox`はqueueをcwdから解決し、providerの`AgentProvider::inbox_command`が作るcommand（guardrailのsettings・`--plugin-dir`・inboxのprompt）を`DAGQ_ROLE=inbox`と`DAGQ_QUEUE`を付けて今のterminalの前面で起動し（exec）、`inbox_opened`（guardrailの有無とsettings）を記録する。`DAGQ_ROLE=inbox`の中で打てば二重に開かずに拒む。inboxのworkspaceのUUIDは無く、`status`・`doctor`の`inbox_guardrail`は最新の`inbox_opened`とwatchの生存の記録で判定する。
   引き継ぐもの: roleとqueueは`DAGQ_ROLE`と`DAGQ_QUEUE`の環境変数でsessionに渡す（[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)決定2の趣旨）。`DAGQ_ROLE`の値は`SessionRole`に合わせる（[ADR-0028](0028-workspace-titles-are-repo-and-role.md)決定3）。
3. **`up`はsupervisorを起動・引き継ぐだけで、inboxを開かない（ADR-0022決定4、[ADR-t1433-4](2026-10-03-t1433-4-supervisor-resides-without-cmux.md)決定1・3をamends）。** `up`はcmuxのpreflightをしない。結果のinboxの欄は、開かなかったことと`dagq inbox`の案内にする。dagqはworkspaceのtitle・色・status pill・pinを当てず、閉じる前のunpinも要らない。`down`はcmuxのworkspaceを閉じない。登録済みのin-cmuxのsupervisorの引き継ぎ（exec）と`down`のsignalは今までどおり受け付け、そのworkspaceは人が閉じる。ADR-t1433-4決定1の「`up`がinboxのworkspaceを開くためのcmuxの確認は残る」と、決定3の「移った後の`down`だけがin-cmuxのworkspaceを閉じ」る部分はこの決定に改まる。
4. **askの人への通知は`[push]`で送る（ADR-0022決定5をamends）。** `watch --role inbox`が`ask_opened`を見たとき、`host.toml`の`[push]`があればそのcommandで送り、無ければ出さない。supervisorとqueue serviceは通知を出さない（ADR-t1433-1決定2から引き継ぐ）。通知はaskのときだけでattention全般には出さないこと、通知がsessionを起こさないこと、認証とコストのaskを1件にまとめるときの通知は最初の1回だけ（ADR-0047決定42）は変えない。watchの不在の後ろ盾（ADR-t1433-5決定1の(3)）は変えない。
5. **inboxとplannerのsettingsのdenyから`Bash(cmux:*)`を外す。** dagqがcmuxのsessionを持たないので、守る対象が無い。
   ADR-t1228-2から引き継ぐもの: inboxとplannerのsettingsの`permissions.deny`に、roleのdagqのコマンドの拒否と身元の環境変数の拒否を持つ。inboxのsettingsは`dagq inbox`がqueueのディレクトリの下に書いて起動のcommandに渡し、中身は`permissions.deny`だけにする（Stop hook・idle marker・サジェストは入れない）。記録したinboxがguardrailつきで開かれたかを`status`か`doctor`が出し、打ち直しのsessionは防がない（guardrailの限界として受け入れる）。これはguardrailでenforcementではない（ADR-t1228-2決定5のまま）。Codexのinboxは同じ趣旨（dagqのCLIを通す）をCodexの手段で持ち、手段が無ければdesignにそう書く。workerとjobはこのdenyの対象にせず、人自身のterminal（`DAGQ_ROLE`なし）はsettingsを持たない。
6. **移行。** 登録済みのsupervisorのargvとlaunchdのplistの`--cmux`はhiddenの引数として受け付けて無視し、`up`が書く新しいplistには書かない。古いバイナリが残した`session_workspaces`のinbox・supervisorの行と、`mode`が`in_cmux`の登録は、cmuxを呼ばずに読み、閉じた扱いにする（workspaceは人が閉じる）。記録を消すmigrationは足さない。
7. **testとe2e（[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定3、[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定4、[ADR-t2125-1](2026-10-08-t2125-1-e2e-gate-checks-only-cmux-after-the-broker-removal.md)決定1・2をamends）。** cmuxのfakeと実cmuxを要るtestを無くし、e2eは全部cmuxの無いhostで流れる。e2eの関門はcmuxをpingせず、cmuxによるe2eのskipとその記録、fixtureのworkspaceとgroupの後始末を次の関門に残すこと、人の端末の`CMUX_SOCKET_PASSWORD`のe2eへの受け渡しをやめ、全部のe2eを流して判定する。関門は「繋がらないので流さない」e2eを持たない（ADR-t2125-1決定1の「cmuxのほかに」は除く）。ADR-t2105-1がamendsしていたADR-t963-1決定1とADR-t1233-2決定2・3はその元の形に戻る: 関門は全部のe2eを流し、流せなければ通さない（ADR-t963-1決定1）。着地の前のe2eを変更のせいでなく流せないrunは、workerに返さず待たせて後で流し直す（ADR-t1233-2決定3）。e2eがcmuxを要らないので、両決定が流せない例に挙げるcmuxは対象を失う。ADR-t2125-1決定2がADR-t2105-1決定1〜4を「そのまま」とした部分は対象を失う。ADR-t963-1決定3のe2eの範囲の境目から「実cmux」を除き、実プロセス・launchd・lifecycle・integrate・install・update・actorの起動の境目は変えない。ADR-t1233-2決定4の同時に1本の理由から「実cmuxのworkspace」を除き、1本のlockは変えない。

実装と`docs/design/`の書き直しは後続のtaskが行う。`inbox_opened`の欄・`up`の結果の欄・通知の文面は[docs/design/](../design/)に書く。

### 置き換えたADRの決定の行き先

| ADR | 決定 | 行き先 |
| --- | --- | --- |
| ADR-t1433-1 | 1・3 | 対象を失う（決定1・7が持つ） |
| ADR-t1433-1 | 2 | 「supervisorとqueue serviceは通知を出さず、askの通知はinboxのwatchが出す」を決定4に引き継ぐ。`cmux notify`の部分は対象を失う |
| ADR-t2105-1 | 1〜4 | 対象を失う（決定7） |
| ADR-0031 | 1〜6 | 対象を失う（dagqがcmuxのworkspaceを作らず閉じない。決定3） |
| ADR-t1228-2 | 1 | 一度きりの時期で失効 |
| ADR-t1228-2 | 2・3・4・6・7 | 対象を失う（`Bash(cmux:*)`・`up`が作るinboxのsettings・reusedのworkspace・Codexのcmuxのguardrail・範囲）。roleのdagqのコマンドと身元の環境変数の拒否、inboxのsettingsの中身、guardrailの有無の表示、Codexの趣旨、workerとjobと人のterminalの扱いは決定5に書き直して引き継ぐ |
| ADR-t1228-2 | 5 | 決定5に引き継ぐ |
| ADR-0026 | 1・3・4・6 | 対象を失う（UUID・description・group・title） |
| ADR-0026 | 5 | groupのexternal IDに使うことは対象を失う。queue hash（repositoryのqueueはqueueディレクトリ名、`--db`のqueueは隣に`repository`ファイルを持つ`queue.db`ならディレクトリの名前、それ以外はcanonical pathのhash）の定義そのものはlaunchdのlabelなどが使うので、そのまま有効なものとして引き継ぐ |
| ADR-0026 | 2 | 「roleとqueueを`DAGQ_ROLE`と`DAGQ_QUEUE`でsessionに渡す」を決定2に引き継ぐ |
| ADR-0028 | 1・2・4 | 対象を失う（title・resumeのtitle・旧名の互換） |
| ADR-0028 | 3 | 「`DAGQ_ROLE`の値を`SessionRole`に合わせる」を決定2に引き継ぐ |

## Alternatives

- **inboxをtmuxなど別のmultiplexerで`up`が開く**: multiplexerへの依存が残り、人がinboxを開くterminalを選べない。
- **inboxをheadlessのjobにする**: inboxは人と対話するsessionなので非対話にできない。
- **通知をmacOSの`osascript`で出す**: platformに固有の依存が増える。既にある`[push]`で足りる。

## Consequences

- dagqはcmuxの無いhostで全部動き、testとe2eからcmuxのfakeと実cmuxが無くなる。e2eの関門はcmuxの状態でskipしない。
- 人は`up`の後に自分のterminalで`dagq inbox`を打ってinboxを開く。inboxのterminalを閉じるのも人である。
- `[push]`の無いhostではaskの通知が出ない。inboxのsessionはwatchの返りで起きるので、inboxを開いていればaskは届く。
- 古いバイナリが開いたcmuxのworkspace（inbox・in-cmuxのsupervisor）は人が閉じる。
