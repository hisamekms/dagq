---
id: plan-launchd-pathstate-spike
type: plan
title: スパイク：自分の plist の PathState の KeepAlive・直の SIGTERM・plist の無い agent の bootout が macOS 27.2.1 の実 launchd で成り立つかを使い捨ての agent で確かめる
status: completed
created: 2026-11-15
owners:
  - hisamekms
tags:
  - planning
  - spike
  - launchd
related:
  - design-supervisor-lifecycle-install
---

# スパイク：launchd の PathState と停止の振る舞い

## 問いと全体の判定

問いは、本番の supervisor の LaunchAgent の定義（`src/application/lifecycle.rs` の `LaunchAgentSpec::xml` の owner の無い形）の KeepAlive を「その定義自身の plist の絶対 path の PathState が true」に替えた agent で、O1〜O8 が 4891 の想定どおりになるか。

**判定: holds。** O1〜O6 はすべて想定どおりで、O7・O8 も想定（起こす・起こし直される）と同じ値だった。根拠は「観測ごとの結果」の表。後続は「後続の計画への推奨」。

## 環境

| 項目 | 値 |
| --- | --- |
| 対象の commit | 3753724d4e9fa6c11bec77b6843a373c1b442372（この run の base。`LaunchAgentSpec::xml` の形を写した） |
| OS | macOS、`sw_vers` の ProductVersion 27.2.1、BuildVersion 26B131 |
| launchctl version | Darwin Bootstrapper Version 7.0.0: Sat Oct  3 17:00:42 PDT 2026; root:libxpc_executables-3298.40.28~144/launchd/RELEASE_ARM64E |
| host | MacBook-Air（arm64、uid 501、domain `gui/501`）。通しの 1 回目の途中で電池駆動の Maintenance Sleep に入った（`pmset -g log` の 21:19:46 JST の「Entering Sleep state due to 'Maintenance Sleep'」）。load average は 8〜14 |
| worker の provider と run の経路 | Claude（Opus 5.5）の headless の worker。Codex の worker が usage limit で止まり、run が Claude に移った session の Bash tool から host の `launchctl` を直に打った。bootstrap・bootout・kill はどれも拒まれなかった |

PathState は path の場所に依らない条件なので、plist は `~/Library/LaunchAgents` でなく run の `$TMPDIR` の `mktemp -d` の directory に置いた。launchd は bootstrap に渡した path から定義を読み、PathState はその path の有無を見張るだけで、どの directory かは条件に入らない。

## 方法

script は [pathstate.sh](launchd-pathstate-spike/pathstate.sh) と job の [job.sh](launchd-pathstate-spike/job.sh)。label は `com.dagq.spike-pathstate.<乱数 8 桁>.<a〜e>`、本番の `com.dagq.<queue の id>` とその他の `com.dagq.*` には触れていない。job は起きた時刻と pid を `<名>.count` に足し、`<名>.cmd` が現れたらその中身を終了コードにして終わり、SIGTERM は `<名>.termhold` があれば記録して 90 秒走ってから自分で終わる。

| agent | 使った観測 | KeepAlive |
| --- | --- | --- |
| a | O1・O2・O3・O4（終了コード 0）・O7 | PathState（自分の plist）だけ |
| b | O4（終了コード 1）・O6 | PathState だけ |
| c | O5 の直の kill -TERM・O6 | PathState だけ |
| d | O5 の対照の bootout | PathState だけ |
| e | O8 | PathState と SuccessfulExit=false |

打った command（順に）:

```sh
launchctl bootstrap gui/501 $D/<名>.plist          # a・b・e、後で c・d
echo 0 > $D/a.cmd                                  # O1（ThrottleInterval の 10 秒を越えてから）
echo 1 > $D/a.cmd                                  # O2
kill -KILL <a の pid>                               # O3
rm -f $D/a.plist $D/b.plist $D/e.plist           # O4・O8
echo 0 > $D/a.cmd; echo 1 > $D/b.cmd; echo 1 > $D/e.cmd   # 65 秒見る
(plist を a の同じ path に書き戻す)                  # O7
launchctl bootout gui/501/<b の label>              # O6（plist 無し）
touch $D/c.termhold; rm -f $D/c.plist; kill -TERM <c の pid>   # O5 直
launchctl bootout gui/501/<c の label>              # O6（plist 無し）
touch $D/d.termhold; launchctl bootout gui/501/<d の label>     # O5 対照（plist あり）
launchctl print gui/501/<label>                     # 各観測の後
log show --info --debug --style compact --start <通しの始め> --predicate 'eventMessage CONTAINS "<label の前置き>"'   # 通しの後の読み直し（通しの中の subsystem の条件では 0 行。下の注）
```

通しは 2 回流した。1 回目（12:07:06Z〜、label の前置き `com.dagq.spike-pathstate.33f3486d`）は O1〜O4・O6・O7・O8 と O5 の直までを得たが、O5 の直の間に host が sleep し、O5 の対照の前で止まった。2 回目（13:13:09Z〜、前置き `com.dagq.spike-pathstate.00519172`）は `ONLY=o5` で O5 の直と対照と O6 だけを `caffeinate -i` の下で流した。

使った plist の全文（a。`$D` は 1 回目の `/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/3b1be4eb-52e5-4b60-ae30-132797e2e1a2/tmp/pathstate.gwJpWT`）:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.dagq.spike-pathstate.33f3486d.a</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/3b1be4eb-52e5-4b60-ae30-132797e2e1a2/tmp/pathstate.gwJpWT/job.sh</string>
		<string>/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/3b1be4eb-52e5-4b60-ae30-132797e2e1a2/tmp/pathstate.gwJpWT</string>
		<string>a</string>
	</array>
	<key>WorkingDirectory</key>
	<string>/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/3b1be4eb-52e5-4b60-ae30-132797e2e1a2/tmp/pathstate.gwJpWT</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
	<key>KeepAlive</key>
	<dict>
		<key>PathState</key>
		<dict>
			<key>/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/3b1be4eb-52e5-4b60-ae30-132797e2e1a2/tmp/pathstate.gwJpWT/a.plist</key>
			<true/>
		</dict>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>ExitTimeOut</key>
	<integer>86400</integer>
	<key>StandardOutPath</key>
	<string>/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/3b1be4eb-52e5-4b60-ae30-132797e2e1a2/tmp/pathstate.gwJpWT/a.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/3b1be4eb-52e5-4b60-ae30-132797e2e1a2/tmp/pathstate.gwJpWT/a.log</string>
</dict>
</plist>
```

e は label・引数・path の名だけが違い、KeepAlive の dict の PathState の後に次の 2 行が足してある。

```xml
		<key>SuccessfulExit</key>
		<false/>
```

## 観測ごとの結果

時刻は UTC。「起きた回数」は job の `.count` の start の行の数、print は `launchctl print` の state・runs・last exit code（または last terminating signal）。

| 観測 | 期待 | 観測した値 | log show の launchd の行（JST） | 判定 |
| --- | --- | --- | --- | --- |
| O1 plist あり・終了コード 0 | 起こし直す | 12:07:21 に exit 0、1 秒後に回数 1→2（pid 45893→57425）。print: state = running、runs = 2、last exit code = 0 | `21:07:21.890 … [gui/501 [100019]:] service inactive: com.dagq.spike-pathstate.33f3486d.a`。起こし直しの行は出ない | 想定どおり |
| O2 plist あり・終了コード 1 | 起こし直す | 12:07:33 に exit 1、1 秒後に回数 2→3（pid 65317）。print: state = running、runs = 3、last exit code = 1 | `21:07:33.064 … service inactive: com.dagq.spike-pathstate.33f3486d.a` | 想定どおり |
| O3 plist あり・外から kill -KILL | 起こし直す | 12:07:45 に kill -KILL 65317、1 秒後に回数 3→4（pid 66042）。print: state = running、runs = 4、last terminating signal = Killed: 9 | `21:07:45.137 … service inactive: com.dagq.spike-pathstate.33f3486d.a` | 想定どおり |
| O4 plist を消す（load のまま） | 走る job は止められず、その後の終了コード 0・1 で起こし直さない。label は load のまま | 12:07:57 に a・b の plist を消し、5 秒後も pid 66042・46924 は生きて state = running。12:08:03 に a は exit 0、b は exit 1。65 秒後（12:09:07）も a は回数 4→4、b は 1→1。print: a は state = not running、runs = 4、last exit code = 0、b は state = not running、runs = 1、last exit code = 1。どちらも print で label が見えた | `21:08:03.353 … service inactive: com.dagq.spike-pathstate.33f3486d.a` と同時刻の `….b`。その後 21:09:07 の書き戻しまで a・b の行は無い | 想定どおり |
| O5 直の kill -TERM（plist 無し・load のまま） | 60 秒で SIGKILL されず、job は 90 秒後に自分で終わる | 2 回目: 13:13:21 に kill -TERM 79128、job は term-held を記録し、90 秒後の 13:14:51 に end-after-term を書いて自分で終わった（pid が消えたのも 90 秒後）。print: state = not running、last exit code = 0。15 秒後も起こし直されない。1 回目でも同じ形で、host の sleep を挟んで 610 秒後に end-after-term で自分で終わり、SIGKILL は届かなかった | 2 回目: `22:14:51.084 … service inactive: com.dagq.spike-pathstate.00519172.c`（kill -TERM の 90 秒後）。1 回目: `21:19:30.082 … service inactive: …33f3486d.c`。どちらも SIGKILL の行は無い | 想定どおり |
| O5 対照の bootout（plist あり） | 60 秒前後で SIGKILL | 2 回目: 13:15:18 に `launchctl bootout`（すぐ rc=0 で戻る）。job は term-held を記録したが end-after-term を書かず、ちょうど 60 秒後の 13:16:18 に pid が消えた。trap できない signal で殺されたことを示し、`exit timeout = 60`（print）と合う | `22:16:18.831 … service inactive: com.dagq.spike-pathstate.00519172.d` と同時刻の `removing service: …00519172.d`（bootout の 60 秒後）。SIGKILL の行は出ない（下の注） | 想定どおり |
| O6 plist の無い load の agent の bootout | 外せて、print で見えなくなる | b（12:09:08）・c（13:15:06）とも `launchctl bootout gui/501/<label>` が rc=0、直後の print は「Bad request.」（見つからない） | `21:09:08.674 … removing service: com.dagq.spike-pathstate.33f3486d.b`、`22:15:06.674 … removing service: com.dagq.spike-pathstate.00519172.c` | 想定どおり |
| O7 plist の無い load の agent（job 止まり）に同じ path へ書き戻す | 4891 の想定は「起こす」 | 12:09:07 に書き戻し、1 秒後に回数 4→5（pid 68529）。print: state = running、runs = 5、last exit code = 0（前の終わりの値） | 21:09:07〜21:09:08 に a の行は無い（起動の行はこの経路の log に出ない） | 起こす（想定と同じ） |
| O8 SuccessfulExit=false を足し plist を消して終了コード 1 | 4891 の想定は「起こし直される」 | 12:07:57 に e の plist を消し、12:08:02 に exit 1、回数 1→2（pid 66809）。print: state = running、runs = 2、last exit code = 1 | `21:08:02.422 … service inactive: com.dagq.spike-pathstate.33f3486d.e` | 起こし直される（想定と同じ） |

`log show` の launchd の行: script の中の `subsystem == "com.apple.xpc.launchd"` で絞った `log show` は 2 回とも 0 行だった。review の後に `log show --info --debug --style compact --start <通しの窓> --predicate 'eventMessage CONTAINS "spike-pathstate"'` で読み直すと、上の列の行が出た（launchd が domain の行として `[gui/501 [100019]:]` の前置きで出すもので、subsystem の条件に当たらなかった）。この経路で出るのは「service inactive」（job の終わり）と「removing service」（bootout）だけで、起動・終了コード・SIGKILL の service ごとの行（mdworker では `exited due to SIGKILL` の形で出る）は spike の label には出なかった。そのため O5 の対照の SIGKILL は log の行でなく、SIGTERM を trap した job が 90 秒の後の end-after-term を書かず、bootout のちょうど 60 秒後に「service inactive」になった（print の `exit timeout = 60` と合う）ことから判定した。O5 の直は SIGKILL の行が無いことに加え、job が自分の end-after-term を書いてから「service inactive」になったので、SIGKILL を受けていない。

## 後片付けの確かめ

2 回目の終わりの記録（script の cleanup）:

```
13:16:25Z gone com.dagq.spike-pathstate.00519172.a: Bad request.
13:16:26Z gone com.dagq.spike-pathstate.00519172.b: Bad request.
13:16:26Z gone com.dagq.spike-pathstate.00519172.c: Bad request.
13:16:26Z gone com.dagq.spike-pathstate.00519172.d: Bad request.
13:16:26Z gone com.dagq.spike-pathstate.00519172.e: Bad request.
13:16:26Z removed .../tmp/pathstate.zsh42A: yes
labels left in launchctl list: 0
```

1 回目は Bash tool の時間の上限で script が止められ、cleanup は bootout まで進んだが一時の directory の削除の前で切れた。後から 5 つの label を `launchctl print gui/501/com.dagq.spike-pathstate.33f3486d.<a〜e>` で確かめてすべて「Bad request.」、`launchctl list` にも残らず、各 job の pid も消えていたので、一時の directory を手で消した。

## 後続の計画への推奨

holds なので、4891 を PathState の案のまま再提出する。この文書を根拠に 4891 の context を直し、e2e の case (8) は回帰の条件として残す。4891 の各部分への意味:

- (a) KeepAlive の形: 自分の plist の PathState だけで、plist のある間は終了コード 0・1・SIGKILL のどれでも起こし直し（O1〜O3）、旧 binary の終了コード 0 の終わりとの互換が保てる。SuccessfulExit=false を足すと plist を消した後も終了コード 1 で起こし直す（O8）ので、足さない理由はそのまま書ける。
- (b)(c) 停止の順: plist を消してから job に直の SIGTERM を送れば、launchd は job を止めず、60 秒の SIGKILL も来ない（O4・O5）。plist のある bootout は今も 60 秒で SIGKILL する（O5 の対照）ので、drain に bootout を使わない前提は正しい。drain の後の bootout は plist の無いまま行える（O6）。
- (d) plist を書く順: plist の無い load の agent に同じ path へ plist を書くと、古い定義のまま 1 秒で job が起きる（O7）ので、「plist は bootout の後に書く」を守る理由が実機で裏づけられた。

条件の注記: この run の経路の unified log には spike の label の service ごとの行（起動・終了コード・SIGKILL）が出ず、domain の「service inactive」「removing service」だけが出た。4891 の e2e や後の確認で SIGKILL を log の行で見たい場合は、人か inbox が host の terminal で同じ script を流し、`exited due to SIGKILL` や「Sending SIGKILL」の行が出るかを確かめる。
