---
id: plan-scratchpad-cleanup-measurement
type: plan
title: "task 1100: 本番 scratchpad 掃除後の容量と disk の閾値"
status: completed
created: 2026-09-30
updated: 2026-09-30
---

# 本番 scratchpad 掃除後の測定（task 1117 / goal 54）

task 1100 の掃除は本番で動いた。最初の掃除で 569 ディレクトリ、1,538,609,152 bytes（1.433 GiB）を除去し、観測期間全体では 632 件、2,148,245,504 bytes（2.001 GiB）を除去した。本番 queue の通常の run 名で残っているのは、task が `ready` の 1 件、172 KiB だけだった。削除記録の全 632 件で、削除より前の最後の task 状態は `completed` / `canceled` と一致した。

親全体は基準値の 3.8G から 2.5G、`df -h /` の空きは 40Gi から 43Gi になった。空きの差を全てこの掃除の効果とはできない。更新後にも disk による claim の控えが 1 回、190 秒あり、その回は scratchpad の単発の最大値を加えたため旧閾値を超えた。旧 path 名の 0 KiB の directory は、完了済み task に対応するものが別に 36 件残っていた。現在の空きは新閾値も十分上回る。直ちに設定を変える根拠は弱いが、標本数を短くする案を判断用 follow-up に残す。

## 0. 前提、時刻、読み方

全時刻は UTC。queue は `77067154921b9014`。取得は 2026-09-30 06:57〜07:05、ファイル容量は **06:58:19〜06:58:21** の観測で、同時の原子的 snapshot ではない。event の集計終端は **2026-09-30T06:57:35Z** に固定した。

| 確認 | 結果 |
| --- | --- |
| `which dagq` | `/Users/shinnosukeooyama/.local/bin/dagq` |
| `dagq status`（06:57:35） | alive supervisor の `binary_version`: `0.4.0-dev+fddd507508fb4e7862a1931d98720384c742c57a` |
| 上記 version の `update_installed` | event 49415、2026-09-30T05:27:31.861Z |
| task 1100 を含む最初の更新 | event 43819、**2026-09-29T15:06:32.221Z**、`0.4.0-dev+f1a3724a14dd9e9fd3a436135ba0dedd45734263` |
| 更新直前 | `0.4.0-dev+338d7a53f236352f119dc2da4871f71c520f7558` |
| commit の包含 | `git merge-base --is-ancestor 966b1dd f1a3724a14dd9e9fd3a436135ba0dedd45734263` と、末尾を `fddd507508fb4e7862a1931d98720384c742c57a` にしたコマンドはともに exit 0 |
| 最初の掃除の記録 | `scratchpad_removed` 43822〜44390、2026-09-29T15:06:43.103Z〜15:06:45.557Z |
| 更新後に終わった run | `run_integrated` 44528、2026-09-29T16:18:49.964Z、task 1060 / `e3683bec-bbad-42b5-9fe0-c7866cc8fe19`。集計終端まで 63 件 |

更新 event の取得には `dagq events --full --kind update_installed --since 2026-09-29T15:03:00Z --limit 100` を使った（取得件数は上限未満）。以下の event 取得も全て上限未満で、ページ切れは無い。

以上から測定の前提を満たす。最初の掃除は更新後の最初の着地より前に、過去の run を拾っている。

比較窓はそれぞれ **57,062.779 秒（15時間51分2.779秒）**。

- 前: `[2026-09-28T23:15:29.442Z, 2026-09-29T15:06:32.221Z)`
- 後: `[2026-09-29T15:06:32.221Z, 2026-09-30T06:57:35Z)`

`events` は `--since` 以上・`--until` 未満。`stats` は since より後・until 以下だが、境界に控えの event は無く、この集計の件数は変わらない。stats の秒は窓の中で始まった控えの継続時間で、任意の稼働時間を窓で切って足したものではない（[stats](../design/supervisor-lifecycle/stats.md)）。

## 1. 残存容量と分類

06:58:19 のコマンド:

```sh
date -u '+%Y-%m-%dT%H:%M:%SZ'
du -sh /private/tmp/claude-501
df -h /
du -sk /private/tmp/claude-501/*
```

直下を Python の `Path.iterdir()` / `is_dir()` でも確認した。隠し entry は 0。directory は 239、その他のファイルは 59。`du` の割当量（KiB）で分類し、ファイル内容は読んでいない。

| 分類 | directory 数 | KiB |
| --- | ---: | ---: |
| 本番 queue の `<run UUID>-worktree` の正確な名前 | 1 | 172 |
| 旧 `cmux-taskq/e0f87c3b33a10073` の `<run UUID>-worktree` 名 | 36 | 0 |
| その他の cwd 名・手作業の directory | 202 | 2,602,656 |
| 直下の通常ファイル（directory 数に含めない） | 59 | 2,048 |
| 全 entry 合計 | 239 directories + 59 files | 2,604,876 |

本番 run の上位は 1 件だけ（3章）。旧 queue の 36 件は全て 0 KiB で同順位。例: `033dea4b-b8b7-4e7c-8723-441b01a0b00f`、`07a58ad0-eab0-46ec-86ae-068bcf4c3e4e`、`0ca49644-bd75-4827-8e8d-b548cabbe9c2`。これらは本番 queue の worktree path から作る名前ではない。

その他の directory の容量上位（いずれも `/private/tmp/claude-501/` 直下）:

| 名前 | KiB |
| --- | ---: |
| `-Users-shinnosukeooyama-ghq-github-com-hisamekms-dagq` | 2,540,716 |
| `revtest` | 58,600 |
| `adrchk` | 1,324 |
| `sp878` | 644 |
| `sp` | 604 |
| `bash-edit-diff` | 444 |
| `rv` | 252 |

main checkout の cwd 名が大半（2.423 GiB）で、人の session・planner・inbox 等が同じ cwd を使いうる。名前だけでは役割ごとの内訳を識別できない。task 1100 が消す対象ではない。

基準値（task context、2026-09-29T15:03:00Z）の 3.8G と比べ、表示上約 1.3G 減った。空きは表示上約 3Gi 増えた。基準値は丸めた表示だけなので、byte 単位の差は算出しない。途中の新規書き込み・他の掃除・APFS の共有容量もあり、2章の削除 bytes と空きの増加は同一の量ではない。

## 2. 削除 event と失敗、空き容量のための掃除

06:57〜07:00 に取得、対象は上記「後」の窓。再取得するコマンド:

```sh
dagq events --full --kind scratchpad_removed --since 2026-09-29T15:06:32.221Z --until 2026-09-30T06:57:35Z --limit 100000
dagq events --full --kind cleanup_failed --kind auto_repaired --since 2026-09-29T15:06:32.221Z --until 2026-09-30T06:57:35Z --limit 100000
```

| 項目 | 値 |
| --- | ---: |
| `scratchpad_removed` 件数 / paths 数 | 632 / 632（各 event 1 path） |
| bytes 合計 | 2,148,245,504 |
| bytes 最大 | 471,572,480（0.439 GiB） |
| bytes > 0 / bytes = 0 | 365 / 267 |
| `reason: task_completed` | 629 件、2,148,245,504 bytes |
| `reason: task_canceled` | 3 件、0 bytes |
| `by` | 全件 `supervisor` |
| scratchpad の `cleanup_failed` | 0 件（path / message に `scratchpad` または `claude-501` を含むもの） |
| `auto_repaired` の `repair: disk_cleanup` | 0 件 |

最大は event 45596（2026-09-29T19:14:35.807Z）、task 1048 / run `ba661aa3-ce08-4818-be17-1da6e030bd05`。最初のまとまった掃除 569 件の合計は 1,538,609,152 bytes。event 時刻は job の結果を loop が記録した時刻で、各ファイルの削除時刻ではない。

空き容量のための掃除の `auto_repaired.bytes` に scratchpad が含まれるかは、該当 event が **0 件のため本番では未測定**。設計では worktree・build・scratchpad の合計だが、通常の掃除の 632 件をその実証の代わりにはしない。[disk-space](../design/supervisor-lifecycle/disk-space.md) / [run-worktrees](../design/supervisor-lifecycle/run-worktrees.md) を参照。

## 3. 消す条件と残存 run

06:59〜07:01 に、`dagq events --full --kind run_claimed --limit 100000` で run と task を対応させ、残存する本番 run について task description の指定どおり `dagq show 1017` を読んだ（担当 task の list / show は実行していない）。

| 残存名の run | task | task の状態 | run の状態 | KiB |
| --- | ---: | --- | --- | ---: |
| `fb6ffc2c-2914-49c6-8251-dc7bfe08ec40` | 1017 | `ready` | `failed` | 172 |

`show` の `runs[].worktree_path` を ASCII 英数字以外 `-` に変換した名前と残存名は一致する。run が失敗していても task は終わっておらず、scratchpad を残す条件どおりだった。本番 queue の通常名に `completed` / `canceled` の消し残しは **0 件**。

削除側も `dagq events --full --kind task_status_changed --limit 100000` と照合した。各削除 event の task について、それより ID が小さい最後の状態遷移の `to` を取り、629 件が `completed`、3 件が `canceled`、不明・不一致は 0 件。削除された 632 paths の存在確認も全て不在だった（07:00 頃）。

**判定: 現在 DB にある worktree path から求める対象では task 1100 の受け入れ条件に適合。ただし、旧 path 名の終わった run の directory は 36 件残る。** task が続く run の保存例と、終わった task だけが削除されたことを確認した。測定時の生きた run は Codex で、Claude scratchpad を持つ生きた run が必ず保存されることの全ケースをこの snapshot で実証したわけではない。削除時点の lease / slot の同時状態までは再構成していない。

次は自動掃除の対象と区別する:

- 旧 `cmux-taskq/e0f87c3b33a10073` path 名の 36 directories は全て 0 KiB。追加の照合（07:04〜07:05）で **全 36 run が本番 queue に存在し、対応する 34 task は全て `completed`** と確認した。run は integrated 33・interrupted 2・failed 1。現在の `runs[].worktree_path` は全 36 件とも `/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/runs/<UUID>/worktree` で、残存する旧 path 名とは異なる。task 1100 の設計は現在の DB の path から名前を求め、移設前の別名を探索しない。そのため「親の下の run 名は全て未完了 task のもの」という広い判定は **成り立たない**。容量回復を妨げてはいないが、旧名の残骸として明記し、必要なら人 / inbox が片付ける ops follow-up にする。
  - 使用コマンドは各 task の `dagq show ID`、過去の run も必要な task 6・15・17 は `dagq show --full ID`。task ID: 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 21, 22, 23, 24, 27, 28, 29, 30, 31, 32, 41, 42, 43, 45, 46, 48。例: run `033dea4b-b8b7-4e7c-8723-441b01a0b00f` は task 4、`0ca49644-bd75-4827-8e8d-b548cabbe9c2` は task 17。
- 本番 run ID を含むが正確な worktree 名ではない directory が 10 件ある。`...-runs-7cf22aa5-...-spike-wt` が 8 KiB、`-private-tmp-claude-501-...-scratchpad-...` という、scratchpad 内の別 cwd から作られた名前が 9 件で合計 28 KiB。task 1100 の名前の導出では対象にならず、1章の「その他」に含めた。run ID の部分一致で消えるべき対象と判断しない。

## 4. 閾値の変化

06:58〜07:01 に、全履歴から各切断時刻より前の event を ID 順に並べ、kind ごとに最後の最大 20 件を選んだ。期間内だけの 20 件ではない。更新前の build 標本は全履歴でも **8 件**しか無い。scratchpad 標本は更新前 **0 件**。

```sh
dagq events --full --kind build_outputs_removed --kind scratchpad_removed --limit 100000
```

`B = max(build の直近20件の bytes, 無ければ0)`、`S = max(scratchpad の直近20件の bytes, 無ければ0)` とし、claim は `(B+S)*2`、着地は `(B+S)*1.5`。[disk] は worktree の `dagq.toml` に無く既定値で計算した。09-29 の実際の控え event の閾値とも一致する。supervisor は main checkout の設定を起動時に読むため、worktree の確認だけで全期間の設定不変を証明するものではない。

| 切断時点 (UTC) | B (bytes) | S (bytes) | claim bytes (GiB) | 着地 bytes (GiB) |
| --- | ---: | ---: | ---: | ---: |
| 更新直前 09-29 15:06:32.221 | 8,889,061,376 | 0 | 17,778,122,752 (16.557) | 13,333,592,064 (12.418) |
| 最初の掃除直後 09-29 15:06:46 | 8,889,061,376 | 339,968 | 17,778,802,688 (16.558) | 13,334,102,016 (12.418) |
| 再発直前 09-29 22:07:49.485 | 8,889,061,376 | 471,572,480 | 18,721,267,712 (17.436) | 14,040,950,784 (13.077) |
| 集計終端 09-30 06:57:35 | 8,889,061,376 | 115,728,384 | 18,009,579,520 (16.773) | 13,507,184,640 (12.580) |

build 最大は全行とも event 35529（09-28 11:13:32.977、task 941）。更新前の build 標本の範囲は event 9490〜40376（09-26 00:20:52.282〜09-28 22:45:03.941）。終端の build 20 件は 10286〜50044（09-26 02:20:02.412〜09-30 06:57:05.081）、scratchpad 20 件は 48094〜49384（09-30 00:33:03.367〜05:22:59.878）。終端の scratchpad 最大は 49384、task 1174。最初の掃除直後の S は最初の 569 件全体の最大ではなく、最後の 20 件（44371〜44390）の最大である。

容量取得時の `os.statvfs('.').f_bavail * f_frsize` は **45,662,945,280 bytes（42.527 GiB）**。同じファイルシステム上の worktree で取得した。終端の claim 閾値より 27,653,365,760 bytes、着地閾値より 32,155,760,640 bytes 多い。`status` の supervisor に現在の `claim_hold` / `landing_hold` は無かった。

## 5. 控えと ask の再発、設定の判断

06:59 頃に取得したコマンド:

```sh
dagq stats --full --since 2026-09-28T23:15:29.442Z --until 2026-09-29T15:06:32.221Z
dagq stats --full --since 2026-09-29T15:06:32.221Z --until 2026-09-30T06:57:35Z
dagq events --full --all --since 2026-09-28T23:15:29.442Z --until 2026-09-30T06:57:35Z --limit 100000
```

| 指標 | 前 | 後 |
| --- | ---: | ---: |
| `claim_holds.by_reason.disk_space.count` | 9 | 1 |
| 同 `.secs` | 2,104 | 190 |
| `landing_holds.count`（全件 disk） | 2 | 0 |
| 同 `.secs` | 193 | 0 |
| `queue_hold` ask の開設（cost 全体） | 9 | 4 |
| 直後の disk の控え event と照合できた ask | 9 | 1 |

前の ask は 203〜205・207・209〜213（列挙すると **9 件**であり、task context の「8 回」と異なる）。全て直後に `reason: disk_space` の `claim_held` がある。後は ask 232〜235。232 は event 46554（22:07:49.325）と disk の `claim_held` 46555（22:07:49.485）が対応し、46565（22:10:59.923）の `claim_resumed` まで 190 秒。空き 18,588,209,152 bytes に対し閾値 18,721,267,712 bytes、差は 133,058,560 bytes（0.124 GiB）だった。

ask 233（22:19:32.343）・234（22:20:27.930）・235（22:23:23.481）も runtime が閉じているが、取得した `ask_opened` は `subject` と question を持たず、`reason_category: cost` だけでは disk と利用上限を区別できない。対応する disk の控え event も無く、**この 3 件の disk 該当性は未測定**。従って「disk ask は後に少なくとも 1 件、cost 全体は 4 件」と報告し、4 件全てを disk と断定しない。stats の claim 控え 1 件を ask 4 件へ読み替えない。

再発時は旧 claim 閾値なら通過した。S の直近 20 件では event 45596 の 471,572,480 bytes が突出し、残りは最大 245,760 bytes。この値が新閾値を約 0.878 GiB 上げた。ただし、それだけで当時の実際の空きが新 run に十分だったと断定はできない。安全余裕を大きく取る設計どおりの停止でもある。

**見直し判定:** 現在は変更不要。単発の大きな標本により 190 秒停止した実例があるので、再発時の判断用に次の案を残す（この task では適用しない）。

```toml
[disk]
claim_factor = 2.0
integrate_factor = 1.5
sample_runs = 10
```

同じ再発直前の履歴で 10 件にすると B は同じ、S は 245,760 bytes、claim は 17,778,614,272 bytes となり、その瞬間の空きは上回る。係数は保ち、古い単発値の保持期間を短くする案である。一方、build の標本も 10 件になり、大きい run が繰り返す際に必要量を過小評価するリスクがある。190 秒の改善だけのために直ちに変更するのは勧めない。planner / 人が現状維持か、継続観測してこの案を試すかを判断する follow-up（decision）とする。着地の控えは後の窓で 0 件なので、`integrate_factor` を下げる根拠は無い。

削除された量、task 状態、再発時の閾値は観測事実だが、前後の差全体を task 1100 の因果効果とはしない。期間中には多数の更新、別の build 出力の掃除、worker provider の変化がある。過去の同時 lease、disk_cleanup の bytes 内訳、3 件の cost ask の subject は未測定として残す。旧 path 名の残骸は状態を照合済みで、現在の path 名だけを掃除する設計の限界として区別する。

## 操作範囲

本番に実行したのは読み取りの `status`・`events`・残存 task の `show`・`stats`（および CLI help）だけ。queue の状態を変えるコマンド、DB への直接アクセス、scratchpad の削除は行っていない。変更はこの文書だけで、`dagq.toml` と `src/` は変更していない。GiB は 2^30 bytes、KiB は 2^10 bytes。event の bytes は割当ブロック量であり、ファイルの論理サイズではない。
