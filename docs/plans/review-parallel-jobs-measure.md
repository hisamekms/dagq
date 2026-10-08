---
id: plan-review-parallel-jobs-measure
type: plan
title: 直近7日のrunのreviewの時間・トークン・選ばれたsubagentの数とslotの使い方の測定と、agentのreviewを並列のjobにしたときの同時実行の上限の既定値
status: active
created: 2026-10-08
owners:
  - hisamekms
tags:
  - measurement
  - review
related:
  - adr-t1453-1
  - plan-receipt-evidence-review-study
  - plan-review-subagents-spike
  - development-local-checks
---

# 直近7日のrunのreviewの時間・トークン・選ばれたsubagentの数とslotの使い方の測定と、agentのreviewを並列のjobにしたときの同時実行の上限の既定値

goal 154でrunのreviewのsubagentを1本ずつのagent jobにする前に、今の親のreview jobの時間・トークン・選ばれたagentの数と、同時に走っていたreviewの数を測り、全体のreviewのjob 1本＋agentごとのjobにしたときの同時のjobの数と、上限Lの先着順の模擬の待ちを出す。
結論は「[5. 推奨](#5-推奨)」: reviewのjobの同時実行の上限の既定値は**10**。

## 0. 冒頭の注意: 窓の全てがgoal 153の前で、切れ目は無い

- 窓の全てのreviewは、goal 153のプログラムのreviewが入る前のもの。
  この測定はgoal 153の完了を待たずに始まったので、窓にgoal 153の前のreviewが混じる（ここでは全てが前）。
- **切れ目は無い**。
  切れ目は、task 1900の着地のcommit Cを含むbuildが固定バイナリに入った最初の`update_installed`の時刻と決めていた。
  (a) `dagq events --full --task 1900 --kind run_integrated`は空で（読んだ時点の`cursor`は138737）、task 1900は着手日（2026-10-08）までに着地していない。
  task 1900の最後のeventは2026-10-06T02:43:03Zの`task_weight_predicted`（id 109743）。
  Cが無いので、(b)〜(c)の祖先判定（`git merge-base --is-ancestor C B`）に当てる`update_installed`は無く、どのeventも切れ目にしていない。
  参考に、窓の始まりから窓の終わり+2時間までの`update_installed`は230件（`dagq events --full --kind update_installed --since 2026-10-01 --until 2026-10-08T02:00:00Z --limit 1000`、1ページで全件）で、`plugin_only`がtrueのものは0件。
  全てがCを含むと確かめる対象が無いという理由で除いた（個々のidとcommitは上のコマンドで再現できる）。
- 切れ目が無いので、時間とトークンは前の値だけを出し、並列のjobにした後の上限側の見積もりとして使う。
  後の区間のreviewは0回（20回に満たない）。
- slotの量（同時のreviewの数と選ばれたagentの数）はgoal 153で変わらない量なので、窓全体で出す。

## 1. 窓・読んだコマンド・区間の作り方

- **窓**: 2026-10-01T00:00:00Z〜2026-10-08T00:00:00Z（UTC、着手日2026-10-08の前日までの7日）。
- **eventsを読んだ範囲**: 窓の始まり−2時間〜窓の終わり+2時間。
- **queueの止まっていた時間**: 2026-09-30T13:53:31Z（id 53021、`supervisor_stopped`）から2026-10-01T13:22:29Z（id 53022）まで本番queueにeventが無い。
  窓の最初の約13.4時間はreviewが走りえなかったので、「窓の全時間」の母集団（168時間）はこの時間を含む。
- **回数**: 過去の記録を1回読んだ。
  同じコマンドを繰り返しても値は変わらないので、繰り返さない。
- **読んだコマンド**（本番queueを固定バイナリ`~/.local/bin/dagq`の読み取りだけのコマンドで、2026-10-08T13:13Z頃に読んだ）。
  前の出力の`cursor`を`--after`に渡し、`events`が空になるまで読んだ（11ページで10,702件、12ページ目が空）。

  ```sh
  # sh で流す（$K を語に分ける）
  K='--kind review_started --kind review_finished --kind review_failed --kind review_retried --kind session_opened --kind session_closed'
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 0   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 60400   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 66632   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 73220   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 79174   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 91375   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 100653   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 107169   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 114390   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 120888   # 1000件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 127496   # 702件
  dagq events --full $K --since 2026-09-30T22:00:00Z --until 2026-10-08T02:00:00Z --limit 1000 --after 138737   # 空
  ```

- **集計の道具**: [review-parallel-jobs-measure/analyze.py](review-parallel-jobs-measure/analyze.py)。
  各ページの出力を`p<n>.json`に置き、`jq -s '[.[].events[]]' p*.json > all.json`でまとめ、`python3 -I analyze.py all.json attempts.csv`で試みごとの表・runごとの表（同じdirectoryの`runs.csv`）と集計（JSONを標準出力に）を出す。
  1回きりの測定の出力で、区分は材料（他の仕組みは読まない）。
  上のコマンドが同じ窓を読む限り作り直せる。
- **`kpi`・`stats`とgoal 95のExecutionの記録**: reviewの試みごとの時間・トークン・agentは上のeventsに全てあるので使っていない。

### 区間（試み）の作り方

- 区間はrunのreviewの1回の試み（`run_id`と`attempt`の組）。
  `run_id`の無いreview（plan review・goal review）は対象外。
- 始まりは`review_started`（payloadの`attempt`）。
  終わりは同じ`run_id`の同じ`attempt`の`review_finished`・`review_failed`・`review_retried`のうち、始まりより後で最初のもの。
- **`review_retried`の`attempt`の意味**: やり直した（失敗した）試みの番号。
  `src/application/supervise/landing.rs`の`retry_review`と`move_review`は、失敗した試みの`attempt`をそのまま`review_retried`に書き、その後に（`retry_review`は`resume_review`を経て）始まる`begin_review`が次の試みの番号を`RunHistory::review_attempts()`（`review_started`の数）+1で決める。
  なので`review_retried`は同じ番号の`review_started`の終わりとして組にし、次の番号は別の試みにした。
- 結果は`review_finished`の`verdict`（pass・revise・concern）、`review_failed`ならfailed、`review_retried`ならretried、終わりが無ければ「終わりなし」。
- **母集団**: 始まりが窓の中の試みは1073（`(run_id, attempt)`の重複は0）。
  窓の前に始まり窓の中で終わった試みは0（queueが止まっていたため）。

### 時間の秒とprocessの終わりの時刻

- **時間の統計の秒**: `review_finished`と`review_failed`はどちらもpayloadの`duration_secs`（jobの起動からjobの終わりの検出までの経過）を使う。
  `review_failed`のeventの時刻と始まりの差は使わない。
  `review_failed`はworkerのsessionの終了を待った後に記録される（`src/application/supervise/mod.rs`の`AfterExit::ReviewFailed`。`duration_secs`はjobの終わりの検出の時に取り、eventはその後の`Phase::Exiting`の終わりで書く）ため。
  例: run f1d2111a-c855-4595-9095-a83b8fcda92a attempt 2はevent 102814（`review_started`）から102882（`review_failed`）まで86.295秒だが、`duration_secs`は83で、reviewのsessionは102875（`session_closed`、`reason = job_finished`）で閉じている。
  `review_retried`は`duration_secs`を持たないので、`review_retried`の時刻と始まりの差を使う。
- **processの終わりの時刻**（同時の数と模擬に使う）: 同じ試みのreviewのsessionの`session_closed`（payloadの`kind = review`・`reason = job_finished`で、`opened_event_id`が指す`session_opened`の`session_id`が`review_started`の`session_id`と同じで、`run_id`も一致するもの）の時刻。
  無いときは、`review_finished`・`review_retried`はそのeventの時刻（jobの終わりの検出の直後に記録される）を「記録の時刻」、`review_failed`は始まり+`duration_secs`を「推定」として使う。
  どれも取れない試みは「欠測」として同時の数と模擬から外す。
- **workerの終了待ちはprocessの時間に入れない**: `review_failed`の時刻と`session_closed`の時刻の差は、reviewのprocessが終わった後のworkerのsessionの終了待ちなので、同時の数と模擬に数えない。
  参考に、`session_closed`のある`review_failed`の14件で、終了待ちの中央値は2.4秒、最大は4.1秒。
- **終わりなしの試み**: 時間の統計から外す。
  同時の数と模擬では、同じrunの次の`review_started`（無ければ窓の終わり）で閉じたとみなす。
  終わりなしの75試みに`job_finished`の`session_closed`は無く、全てに`reason = inferred`の`session_closed`がある。
  `inferred`は引き継いだsupervisorが後から推した閉じで、processの終わりの時刻とは限らないので使わず、規則どおり次の`review_started`で閉じた。
  そのうち11試みは`inferred`の閉じが次の`review_started`より60秒以上前（6試みは300秒以上、最大982秒の8a86a175-6a6e-4e69-af67-ba52a052008c attempt 1）で、合わせて約1.3 process時間を同時の数と模擬に多く数えている（見積もりは上限側に寄る）。

### 終わりの時刻の出所ごとの件数

| 出所 | 件数 |
| --- | ---: |
| `session_closed`（`job_finished`） | 996 |
| 記録の時刻（`review_finished`。Codexのreviewで、sessionの`session_closed`が無い2件） | 2 |
| 推定（`review_failed`の始まり+`duration_secs`） | 0 |
| 欠測 | 0 |
| 終わりなし（次の`review_started`か窓の終わりで閉じたとみなす） | 75 |
| 計 | 1073 |

**終わりなしの75試み**は、どれも同じrunの次の`review_started`が先に来たもので、例のrun 00364554-d18c-4735-8a91-565a7f6fd240 attempt 2は`supervisor_handed_off`（id 78414）の直後に次の試み（attempt 3）が始まり、attempt 2のsessionは`reason = inferred`で閉じている（supervisorの引き継ぎで捨てられた試み）。
`run_id#attempt`の一覧:

- `00364554-d18c-4735-8a91-565a7f6fd240#2`, `01881c1c-8ec9-4f95-b904-b947e9989020#3`, `0389f222-415f-4c00-96f9-fb05998f7a3c#1`, `0ce0d8a2-a1d4-49e1-a173-3fc90086c562#4`, `13fbe0f9-7441-4c6e-8581-b27dd168370d#1`
- `1b30d72b-2835-4414-8dad-2a7dcdda4aa4#1`, `1b74f1cc-1f57-42ac-9818-156024d011a4#1`, `1df032d1-460d-418b-adb4-0ae58c9f1419#1`, `21291c4f-c263-4b5f-aa52-08399eac5863#1`, `22254e6a-db9d-4a25-abcb-75d3c04e0feb#1`
- `2a145e51-0f6e-4f74-875a-bf9ec712a4af#3`, `2a145e51-0f6e-4f74-875a-bf9ec712a4af#4`, `31ae076a-affc-4cf0-b2e7-5e7eaea69ded#3`, `3397cb54-8f35-4a11-922d-d22002ceedad#3`, `3397cb54-8f35-4a11-922d-d22002ceedad#5`
- `3397cb54-8f35-4a11-922d-d22002ceedad#7`, `34d90fa8-3556-4390-a85f-3524973ed700#1`, `3be848c0-6af6-4d0b-94d9-65f37e594fdf#1`, `3d21d841-867f-4a87-873e-0e95cd5a83e9#2`, `3d21d841-867f-4a87-873e-0e95cd5a83e9#3`
- `4af52b96-4075-4dad-bf45-d1048cfdb8d5#3`, `4f69bfe8-dc03-4cb3-84e5-c47f0547a1bb#1`, `53c73bbd-f13e-4b99-94da-18791fb97ad1#1`, `55ba7223-3054-4933-baf3-ef87bbdaf10c#2`, `56dfaeaa-897d-47c2-b6b8-6907f23c7f0d#2`
- `59f842a5-d062-465a-8648-fd00deba193e#4`, `5ae955d3-0448-416f-8993-3e5ce40c05b9#1`, `5b0f0eb3-59d8-4834-80ff-30f638519c90#2`, `5ed87780-c191-4a6b-b02e-dc7cb963a8d0#1`, `6000103e-2245-4dcf-aa11-756ffd8b7536#1`
- `659ddb93-98bc-44ea-a892-f1315c81d143#1`, `69566233-72dc-4e0f-b334-aff8404a25f6#2`, `6f7ea894-b95f-4f36-93ba-97d6d1ce3dc9#2`, `710ace0e-4fa5-419a-b72e-54667dc5c83d#2`, `76c5dd97-fde3-4a96-8784-4f4e3ad22fb0#2`
- `853c5ae1-26d2-40da-af5d-fc158c669810#4`, `8a86a175-6a6e-4e69-af67-ba52a052008c#1`, `8cf5ae79-9fde-4b98-a4fe-d38360ae379c#3`, `9102561c-0e3c-475e-81b6-1ed97a81d01b#3`, `93222d22-2a7e-48ca-9bc1-a6de8fe6442e#1`
- `947e89a1-6638-478b-acd4-06ef6168f701#1`, `98a2d261-d70a-4e54-86cd-af805d3bb20b#1`, `98b5d449-822c-409e-8964-af74901b3aac#3`, `a0155fb0-445e-4b9a-8313-01cc261e7e21#2`, `a943da00-7565-4dc4-98ab-0e568c9bc31d#1`
- `ae1fdf72-6ec2-462f-8eaa-e80583371076#3`, `b06a1265-90b6-414b-8c33-3062d26915a7#2`, `b2174d09-5dfc-4351-83b3-5140b32e39fe#1`, `b2375b92-a2a7-4525-bb24-497c801f157c#4`, `b7cd626c-de03-4ac4-aabb-fefa0fe11458#4`
- `bbd11ba3-0056-4621-96a4-fac3d4058ca6#5`, `bc4a1494-3b67-422f-9760-020c8e761a13#1`, `c1e7b209-9c1d-46fa-80d0-e5620c73dd1c#1`, `c85be6af-3b45-4dc8-bbe5-03a6dff59bd8#2`, `cb3ad75e-056b-4b40-a7cc-056f9d5d3ff6#2`
- `d4773faa-1ed4-4511-9f65-3127f7fd03e1#2`, `d8b9c5e3-7c85-4a39-ae47-711f51665c9f#2`, `d97216b8-eb3a-415f-8716-b087c588f086#2`, `d9e44747-aff3-44d1-bcb3-17dec4b56ebb#3`, `d9e44747-aff3-44d1-bcb3-17dec4b56ebb#6`
- `dc686e6a-17f8-4429-9e20-de109a64bc83#3`, `e4d2bae0-198c-4ea0-b22f-7d5065806d41#1`, `e66aa82f-ddfd-4f82-a020-a921143866e0#1`, `e6cfc445-d78c-461b-8d61-d35247c081db#7`, `e8bda067-5b08-4061-bce0-148528c1ac9f#1`
- `e94f55e2-3ba4-430a-8862-64028f1eaaa2#1`, `eb79b198-07aa-4098-8fc5-cacb1cff784e#1`, `ebe22e74-1983-4eb9-99b1-d9e31ade3418#1`, `f1d2111a-c855-4595-9095-a83b8fcda92a#7`, `f42d4c2b-4369-4e9d-82b7-8a05d15058d6#2`
- `f4b8ea86-dff0-4086-a2c8-5eb760569be9#1`, `f68cc82a-1bc6-4f40-a5a5-ea5300ca726c#2`, `f7d0d744-913d-44fa-9cad-32ea792d10a2#1`, `f8f9ae0d-3592-4589-ac6c-e2602e941f48#3`, `fbb0ee8c-e5c7-465b-b3b0-08db5d354438#1`

## 2. 試みごとの表

試みごとの表は[review-parallel-jobs-measure/attempts.csv](review-parallel-jobs-measure/attempts.csv)（1073行）に置いた。

- 列: `run_id`・`attempt`・`started_at`・`start_event`（`review_started`のid）・`end_kind`（終わりのeventのkind、終わりなしは空）・`end_event`・`secs`（時間の統計の秒）・`result`（pass・revise・concern・failed・retried・open）・`n_agents`と`agents`（`review_started`の`subagents.agents`の数と名前）・`provider`と`switched_from`（`review_started`の`launch`）・`input`・`output`・`cache_creation`・`cache_read`（reviewのsessionの`session_closed`の`tokens`）・`usage_input_total`・`usage_output`（`review_finished`の`tokens`。subagentの分を含む。窓の中には記録が無い）・`end_source`（終わりの時刻の出所。`session_closed`・`recorded`・`estimated`・`missing`・`open_closed_at_next_or_window_end`）・`proc_end`（processの終わりの時刻）・`side`（切れ目の前か後か。全て前）。
- runごとの表は[review-parallel-jobs-measure/runs.csv](review-parallel-jobs-measure/runs.csv)（497 run）で、列は`run_id`・`attempts`（試みの数）・`secs_sum`（試みの秒の和、終わりなしは0）・`jobs_sum`（試みごとの1+agentの数の和）・`results`（試みの結果を順に）・`output_sum`（親のoutputのトークンの和）。
  runごとの試みの数は中央値2・p90 4・最大12、試みの秒の和は中央値148・p90 607・最大3,128秒。
- **subagentの分のトークン**: 窓の中の`session_closed`の`tokens`は親のsessionの分だけで、subagentの分を含まない。
  subagentを含む`review_finished`の`tokens`（`tokens_source = model_usage`）は窓の後（2026-10-08T01:17Z以降）の8試みにしか無い。
  その8試み（窓の外、参考）では、出力のトークンは`model_usage`が`session_closed`の2.8〜4.6倍（中央値で約3.2倍）、入力（input+cache）は1.6〜4.2倍で、subagentの分が出力の約7割を占める。
  例: event 132004（run e87a4568-b953-4352-b496-7f7011543c21 attempt 5、agent 6）は`model_usage`の出力57,122・入力7,008,024に対し、`session_closed`は出力12,289・入力2,287,338。

## 3. 集計

### 3.1 reviewの時間（秒、最近順位法、切れ目の前）

| 結果 | 件数 | 中央値 | p90 |
| --- | ---: | ---: | ---: |
| 全体（終わりのある試み） | 998 | 111 | 204 |
| pass | 508 | 94 | 161 |
| revise | 364 | 133 | 229 |
| concern | 108 | 124 | 222 |
| failed | 14 | 600 | 600 |
| retried | 4 | 234 | 891 |

- 後の区間は0試み（切れ目が無い）なので、後の列は出さず、前の値を上限側の見積もりとして使う。
- failedの14件のうち11件はreviewの時間切れ（600秒、`duration_secs`は600か606）。
- 人の答え待ちの時間はreviewの区間（`review_started`から終わりまで）に入らないので引かない。

### 3.2 トークン（reviewのsessionの`session_closed`、親の分、切れ目の前）

`tokens`のあるClaudeの試みは864（`session_closed`の無いものと、Codexの208試みは`tokens`を持たない）。

| 指標 | 中央値 | p90 | 計 |
| --- | ---: | ---: | ---: |
| input | 32 | 48 | 27,458 |
| output | 8,037 | 12,478 | 6,995,484 |
| cache_creation | 72,213 | 112,113 | 63,646,917 |
| cache_read | 783,134 | 1,620,335 | 785,896,313 |

agentの数ごとの親のトークン（中央値）:

| agentの数 | 試み | output | 入力（input+cache） |
| ---: | ---: | ---: | ---: |
| 0 | 92 | 3,339 | 628,010 |
| 1 | 10 | 3,120 | 229,038 |
| 2 | 41 | 4,594 | 342,972 |
| 3 | 117 | 5,862 | 513,319 |
| 4 | 58 | 6,566 | 572,082 |
| 5 | 129 | 7,991 | 777,995 |
| 6 | 190 | 9,033 | 1,037,763 |
| 7 | 132 | 10,342 | 1,253,662 |
| 8 | 95 | 11,400 | 1,400,861 |

- 親の分はagentの数にほぼ比例して増える（subagentの結果を読み込むため）。
- subagentの分は上の表に入らない（2の「subagentの分のトークン」）。
  窓の後の8試みの比から、subagentを含めたreview 1回のトークンは親の分の約3倍が上限側の見積もり。
- 並列のjobにすると、各agentのjobは今のsubagentと同じく自分のcontextで動き、全体のreviewのjobはsubagentの結果を読み込まなくなる。
  一方でjobごとにprompt・指示・差分を読み直すので、jobごとのcache_creationが増える。
  トークンの総量は今の`model_usage`の水準（親の約3倍）から大きくは変わらないと見積もる（上限側）。

### 3.3 選ばれたsubagentの数とprovider

| agentの数 | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 試み | 300 | 10 | 41 | 118 | 58 | 129 | 190 | 132 | 95 |

- 0の300試みは、subagentを選ぶ仕組みが動く前（最初に`subagents`を持つ`review_started`は2026-10-03T04:15:16Z）の298試み（Claude 92・Codex 206）と、その後のCodexの2試み。
  仕組みの後の775試みのうち773はagentを持つ。
- agentごとの選ばれた回数: receipt-evidence 773・design-consistency 712・adr-rules 696・test-rules 584・migration-rules 512・architecture-boundaries 383・plugin-generic 297・config-rules 190。
- provider: Claude 92・Codex 208・Codexから切り替えたClaude 773（`switched_from = codex`、`switch_reason = subagents_unsupported`）。

### 3.4 同時に走っていたreviewの数と、並列のjobにしたときの同時のjobの数

階段関数（窓の中の時刻ごとの開いた区間の数）。
中央値とp90は、その数が続いた時間で重みをつけた最近順位。

| 量 | 母集団 | 最大 | 中央値 | p90 |
| --- | --- | ---: | ---: | ---: |
| 同時のreview | 窓の全時間（168.0時間） | 3 | 0 | 1 |
| 同時のreview | 1本以上が開いていた時間（34.2時間） | 3 | 1 | 2 |
| 同時のjob（1+agentの数） | 窓の全時間（168.0時間） | 25 | 0 | 7 |
| 同時のjob（1+agentの数） | 1本以上が開いていた時間（34.2時間） | 25 | 7 | 11 |

参考（subagentの仕組みが動いた後、2026-10-03T04:15:16Z〜窓の終わり）:

| 量 | 母集団 | 最大 | 中央値 | p90 |
| --- | --- | ---: | ---: | ---: |
| 同時のreview | 全時間（115.7時間） | 3 | 0 | 1 |
| 同時のreview | 1本以上が開いていた時間（26.1時間） | 3 | 1 | 2 |
| 同時のjob | 全時間（115.7時間） | 25 | 0 | 8 |
| 同時のjob | 1本以上が開いていた時間（26.1時間） | 25 | 7 | 12 |

- 同時のreviewの最大3は、`[supervisor] parallel = 3`（同時のrunの上限）と一致する。
- 今のreviewはrunごとに1本の親のprocessで、subagentはその中で動く。
  並列のjobにすると、processの数は最大25、reviewが開いている時間の中央値で7、p90で11〜12になる。
- 同時のreviewの数ごとの時間: 0本133.8時間・1本29.6時間・2本4.4時間・3本0.3時間。

## 4. 上限Lのときの待ちの模擬

窓の中に始まった1073試み（欠測は0）を`review_started`の時刻の順に並べ、各試みの1+N本（Nは選ばれたagentの数）のjobを始まりの時刻に同時に要求し、空きがL本の先着順（要求の順に、最も早く空く枠へ）で起動した。
jobの起動の待ちは「起動の時刻−要求の時刻」、reviewの終わりの遅れは「最後のjobの終わり−今の終わり」で、各jobの時間が同じなので試みの中のjobの待ちの最大に等しい。

| L | jobの待ち 中央値 | p90 | 最大 | 待ったjob（5,220本中） | reviewの遅れ 中央値 | p90 | 最大 | 遅れたreview（1,073中） |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 3,246 | 12,773 | 27,678 | 4,835 | 1,967 | 12,603 | 27,678 | 779 |
| 4 | 209 | 1,439 | 4,275 | 3,862 | 162 | 1,265 | 4,275 | 725 |
| 6 | 0 | 362 | 1,803 | 2,465 | 48 | 362 | 1,803 | 570 |
| 8 | 0 | 135 | 1,803 | 1,407 | 0 | 172 | 1,803 | 306 |
| 10 | 0 | 60 | 363 | 873 | 0 | 78 | 363 | 204 |
| 12 | 0 | 0 | 362 | 485 | 0 | 24 | 362 | 123 |
| 25（今の同時のjobの最大） | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |

（秒）
subagentの仕組みが動いた後の775試みだけで回すと、L=10の遅れの中央値0・p90 94・最大363秒、L=12は0・64・362秒、L=8は0・225・1,803秒。

仮定:

- 各agentのjobと全体のreviewのjobの時間を、今の親のjobの区間（始まりからprocessの終わりまで）の秒と同じとみなす。
  今の親の中のsubagentは並列に動き、親の時間はsubagentの最も長いものと親自身の仕事を含むので、1本ずつのjobの時間としては長めで、待ちと遅れは上限側の見積もり。
- 0本のagentの試み（仕組みの前とCodex）は1本のjobとして数えた。
  goal 154の後はCodexのreviewもagentのjobを持つので、そのぶん今の窓全体の値はjobの数を少なめに数えている（上の「仕組みが動いた後」の値が近い）。
- jobの起動にかかる時間・providerの控え・時間切れのやり直しは模擬に入れていない。
- 人の答え待ちの時間はreviewの区間に入らないので引かない。

## 5. 推奨

**runのreviewのjobの同時実行の上限の既定値は10にする。**

- 1回のreviewのjobは最大9本（全体の1本+agent 8本）で、上限が9以上なら、他のreviewが走っていないときに1回のreviewが自分のjobを待たない。
  8以下ではagentの多いreviewが自分のjobの終わりを待つ（L=8の遅れの最大1,803秒）。
- L=10で、reviewの終わりの遅れの中央値は0、p90は78秒（仕組みの後だけで94秒）、最大は363秒。
  reviewの時間の中央値111秒・p90 204秒に対して、p90の遅れは1回分の時間より短い。
- L=12にしても遅れのp90は24秒（仕組みの後だけで64秒）までしか縮まず、最大は362秒で変わらない。
  一方で同時のprocessが2本増え、hostの`[supervisor] parallel = 3`のworkerとintegrateのcargo（8コア/16GBを前提に並列度を決めている）と同時に走る。
- 今の親のprocessは同時に最大3本で、L=10はreviewのprocessを最大で今の3倍強にする。
  トークンの総量は今の`model_usage`の水準から大きく変わらない見積もり（3.2）なので、上限で抑えるのはprocessの数とproviderへの同時の要求の数。
- 並列のjobにした後に、同じ窓の作り方（このscript）でjobの待ちとreviewの時間を測り直し、遅れのp90がreviewの時間の中央値を超えるなら12へ上げることを検討する。
