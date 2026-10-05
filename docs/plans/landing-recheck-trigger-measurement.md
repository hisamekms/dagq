---
id: plan-landing-recheck-trigger-measurement
type: plan
title: landing recheckのきっかけを広げた（ADR-t1310-1）前後の本番のrecheckの件数・時間・commandの回数と、hostの負荷への影響
status: completed
created: 2026-10-05
updated: 2026-10-05
owners:
  - hisamekms
tags:
  - performance
  - measurement
  - landing
related:
  - adr-t1310-1
  - adr-0068
  - design-supervisor-lifecycle-landing-recheck
  - plan-landing-lane-cpu-share
---

# landing recheckのきっかけを広げた（ADR-t1310-1）前後の本番のrecheckの件数・時間・commandの回数と、hostの負荷への影響

task 1514の測定（task 1310のfollow_up）。[ADR-t1310-1](../adr/2026-10-03-t1310-1-recheck-whenever-main-moves-past-the-last-recheck.md)で、landing recheckがsupervisorの着地に加えて、mainが最後の`landing_recheck_finished`のmainと違うとき（直接の`integrate`、handoff・再起動の間の着地、dagqを通さないpush）と、対象が無かったmainに後から着地待ちのrunが出たときにも走るようになった。recheckのcommand（`cargo check --locked --all-targets`）の回数が増えてhostの負荷に効いていないかを、本番queueの記録で前後に比べた。

- 記録: 固定バイナリ（`dagq 0.4.0-dev+c7558779`）の読み取り専用のコマンド（`events --all --full`・`stats --full`）だけで数えた。queueの状態を変えるコマンドは打っていない。
- 変更はせず、決定もしない。

## 要点

- **recheckの回数は65回から80回に増えた（+23%。着地1件あたりでは0.45回から0.53回。前の窓の始めの約12.3時間はsupervisorが動いていなかったので、着地1件あたりの方が比べやすい）が、確かめたrunの数は451から193に、commandの回数（推定）は435回から149回に減った。recheckの合計の時間も10,439秒から6,309秒に減った（-40%）。**（2節）
- **ADR-t1310-1のきっかけと分かったのは、supervisorのexec（自動更新のhandoff）・再起動の後のrecheck 20回で、後の窓の80回の25%に当たる（新しいきっかけの下限。後から着地待ちのrunが出たきっかけは「着地の直後」と分けられない）。** dagqを通さないmainの動き（`landed_run_id`がnull）は0回、直接の`integrate`の着地は0回だった。（3節）
- **hostの負荷に効いているとは言えない。** recheckのcommandの回数と合計の時間が減り、`cargo`のCPUの平均も下がった。着地の順番待ちも短くなった。loadのp90とrunのworkの中央値は上がったが、recheckの時間が減っているのでrecheckでは説明できない。（4節）
- 打ち手は要らない。command 1回ずつの時間は判断に要らなかったので、計装のtaskも出さない。

## 1. 比較の境界Tと窓

### 境界T

T = **2026-10-03T01:28:15.711Z**（`supervisor_started` event 70602）。

- その版は`dagq_version`が`0.4.0-dev+33cc04102ae9ad370e5cb30a93f2b0aa6bf81cb8`で、同じsupervisor（pid 82072、token `f5b8bc25`）が2秒後に`update_installed`（event 70612、2026-10-03T01:28:17.742Z、`commit` 33cc0410、`previous_version` e994a99f）を記録した。
- 1310の着地（e850ad3a、2026-10-02T23:54:50Z）の直後の`supervisor_started`（event 69467、2026-10-02T23:54:59.754Z）と`update_installed`（event 69479、23:55:02.125Z）は、版がe994a99fでe850ad3aを含まない。その後、70602までに`supervisor_started`と`update_installed`は無い。
- 確かめ方:

  ```sh
  dagq events --all --full --kind update_installed --kind supervisor_started \
    --since 2026-10-02T23:00:00Z --until 2026-10-04T00:00:00Z --limit 100
  git merge-base --is-ancestor e850ad3a e994a99f750b3f07aee1e3e4eb26d4dec3bc0afc  # exit 1（含まない）
  git merge-base --is-ancestor e850ad3a 33cc04102ae9ad370e5cb30a93f2b0aa6bf81cb8  # exit 0（含む）
  ```

  `git log --oneline e994a99f..33cc0410`の8件の中にe850ad3aがある。

### 窓

測った時点（2026-10-05T03:18Z）でTの後に取れるのは約2日2時間だったので、両窓とも48時間にした。

| 窓 | UTCの範囲 | 長さ |
|---|---|---|
| 前 | 2026-10-01T01:28:15.711Z 〜 2026-10-03T01:28:15.711Z | 48時間 |
| 後 | 2026-10-03T01:28:15.711Z 〜 2026-10-05T01:28:15.711Z | 48時間 |

前の窓の始めの約12.3時間（2026-10-01T01:28Z〜13:49Z）は本番のsupervisorが動いていなかった（窓の中の最初の`supervisor_started`はevent 53457、2026-10-01T13:49:35Z、`handoff` false。最初の着地は14:01Z）。後の窓にもprocessの入れ替わり（pid 82072→3515→4323→50952→53391）の間の短い空きがある。そのため、件数と合計の時間の窓どうしの比は、着地1件あたりの比より前の窓に不利（前の窓が小さく出る）である。前の窓の最後の約1.5時間（23:54Z〜T）はe994a99fの版で、1310の変更はまだ動いていない。後の窓の最後の約4時間（2026-10-04T21:29Z以降）は、task 1311（ADR-t1311-1、recheckのcleanの記録）を含む版が動いていた。

```sh
dagq events --all --full --kind landing_recheck_finished --since <窓の始め> --until <窓の終わり> --limit 10000
dagq stats --full --since <窓の始め> --until <窓の終わり>
```

## 2. recheckの件数と時間

`stats`の`landing_rechecks`と、`landing_recheck_finished`のpayloadの合計を並べる。`stats`の`conflicts`・`check_failures`はrunごとの初めての発見だけを数え（`repeat`のものは数えない）、`resumed`はslotに持たれたrunが着地のときにparkされた分も数える（`src/domain/stats.rs`の`landing_rechecks`）。payloadの合計は、同じrunが別のrecheckでまた見つかった分も数える。

| | 前 | 後 |
|---|---:|---:|
| 着地（`run_integrated`） | 143 | 150 |
| `rechecks`（`landing_recheck_finished`の数） | 65 | 80 |
| 着地1件あたりのrecheck | 0.45 | 0.53 |
| `runs_checked`（payloadの`checked`の合計） | 451 | 193 |
| recheck 1回あたりの`checked` | 6.9 | 2.4 |
| `conflicts`（stats） / payloadの合計 | 16 / 16 | 36 / 44 |
| `check_failures`（stats） / payloadの`check_failed`の合計 | 1 / 1 | 0 / 0 |
| `resumed`（stats） / payloadの合計 | 16 / 15 | 34 / 32 |
| payloadの`held`の合計 | 2 | 4 |
| payloadの`errors`の合計 | 0 | 0 |
| `duration_secs`の中央値 | 140秒 | 54.5秒 |
| `duration_secs`の合計 | 10,439秒（窓の6.0%） | 6,309秒（窓の3.7%） |

- `duration_secs`は1回のrecheckの全体（対象の複数のrunの`git merge-tree`・mergeのcommit・scratchのcheckout・command・結果の適用まで）の時間で、command 1回ずつの時間ではない（`src/application/supervise/recheck.rs`の`apply_recheck`）。
- 後の窓の80回のうち15回は1秒で、どれもmerge-treeの衝突だけでcommandが走らなかったrecheck。
- `checked`が減ったのは、1310が同じ変更で、headがmainを含むrunと、同じmainとheadで判定済みのrun（`conflict_precheck`・`integration_deferred`・`landing_recheck_failed`）を対象から外したことと、approveを待つ着地待ちのrunが減ったこと（`stats`の`land_phases.landing_queue_via`の`approve`が41件から14件）が重なったためと見られる。eventからはこの2つを分けられない。

### commandの回数の推定

commandはmerge-treeがcleanでcommandの設定があったrunでだけ走る。1回のrecheckのcommandの回数は`clean + check_failed`を下限、それに`errors`を足した数を上限とした（`errors`はどの段で失敗したかがeventから分からないため）。

| | 前 | 後 |
|---|---:|---:|
| `clean`の合計 | 434 | 149 |
| `check_failed`の合計 | 1 | 0 |
| `errors`の合計 | 0 | 0 |
| commandの回数の下限（clean + check_failed） | 435 | 149 |
| commandの回数の上限（+ errors） | 435 | 149 |

両窓とも`errors`が0なので下限と上限は同じ。command 1回ずつの実行時間と、`errors`がどの段で起きたかの内訳はeventに記録が無く、測っていない。

## 3. 後の窓のrecheckのきっかけ

`landed_run_id`は、recheckを始めるときのmainが最新の`run_integrated`の`result_commit`（か`commit`）のときにその着地を名指し、そうでなければnullになる（`recheck.rs`の`landing_at`）。

| 分け方 | 後の窓 | 前の窓 |
|---|---:|---:|
| `landed_run_id`がnull（dagqの着地でないmainの動き） | 0 | 0 |
| `landed_run_id`あり | 80 | 65 |

`landed_run_id`ありの80回を、名指された着地の`run_integrated`と`supervisor_started`で次のとおりさらに分けた。

| きっかけ | 後の窓 | checked | clean | conflicts | `duration_secs`の合計 | 前の窓 |
|---|---:|---:|---:|---:|---:|---:|
| 同じsupervisorの着地の直後（従来のきっかけと、後から着地待ちのrunが出たきっかけ） | 60 | 147 | 113 | 34 | 5,071秒 | 64 |
| 着地とrecheckの間に同じprocessのexec（自動更新のhandoff）があった | 19 | 44 | 34 | 10 | 1,184秒 | 0 |
| 着地を頼んだsupervisorと別のprocess（`up`での再起動の後）が確かめた | 1 | 2 | 2 | 0 | 54秒 | 0 |
| 同じmainの2回目以降のrecheck | 0 | | | | | 1 |
| 着地を頼んだのが`supervisor`でない（直接の`integrate`） | 0 | | | | | 0 |

- 分け方: `run_integrated`の`actor.requested_by`が`supervisor:`で始まらなければ直接の`integrate`、recheckの`actor.id`（`supervisor:<pid>`）と違えば別のprocess、同じで`run_integrated`とrecheckの間に`supervisor_started`があればexecの後、無ければ着地の直後とした。両窓とも着地150件・143件は全て`requested_by`が`supervisor:`だった。
- execの後と再起動の後の20回は、ADR-t1310-1が足した「handoff・再起動の間の着地」のきっかけに当たる。recheckの予定（`Rechecks`）は新しいprocessで空から始まるので（`src/application/supervise/mod.rs`の`recheck::Rechecks::default()`）、execの後に走ったrecheckは「mainが最後のrecheckのmainと違う」から走ったと分かる。前の窓では着地の後にexecが挟まったrecheckは0回だった。
- 分けられない分: 「着地の直後」の60回のうち、着地の予定（`due`）から走ったものと、後から着地待ちになったrunを見直した（`idle`）ものはeventでは分けられない。`idle`の見直しは、そのmainで対象が無かった試み（`landing_recheck_finished`を記録しない）の後に走るので、記録の上ではそのmainの最初のrecheckになり、`landed_run_id`は同じ着地を名指す。60回はADR-t1310-1のこのきっかけを含みうる。
- 前の窓の「同じmainの2回目」の1回（event 61222、task 968の着地のmain、checked 1）は1310より前の版で、きっかけはeventから分からない。

増えた15回（65→80）は、execと再起動の後の20回でおおむね説明できる。着地が143件から150件に増えたことと、前の窓の始めにsupervisorが動いていなかったことも効いている。新しいきっかけの回数は20回が下限で、`idle`の分は上に書いたとおり数えられない。

## 4. hostの負荷への影響

判断: **増えたrecheckはhostの負荷に効いているとは言えない。** 根拠は次の数（`stats --full`の`host`・`overall`、同じ窓）。

| | 前 | 後 | 読み |
|---|---:|---:|---|
| recheckの合計の時間 | 10,439秒 | 6,309秒 | -40% |
| commandの回数（推定） | 435 | 149 | -66% |
| 着地1件あたりのrecheckの時間 / commandの回数 | 73秒 / 3.0回 | 42秒 / 1.0回 | 前の窓のsupervisorの止まっていた時間に依らない比でも減った |
| execと再起動の後のrecheck（増えた分）の時間 | 0秒 | 1,238秒 | 後の窓の合計の20% |
| `cpu_cargo`の平均 / 中央値（%） | 56.56 / 16.0 | 51.68 / 18.8 | 平均は下がった |
| `cpu_total`の平均 / p90（%） | 336.41 / 553.1 | 322.26 / 534.7 | 下がった |
| `load1`の中央値 / 平均 / p90 | 13.34 / 15.81 / 26.36 | 11.43 / 16.14 / 32.43 | 中央値は下がり、p90は上がった |
| 着地の順番待ち（`land_phases.landing_queue`）の中央値 / 合計 | 8秒 / 97,151秒 | 2秒 / 31,968秒 | 短くなった |
| runの`work`の中央値 / 合計 | 681秒 / 168,234秒 | 1,150秒 / 229,038秒 | 長くなった |
| `work_breakdown`の`build`の中央値 | 92秒 | 112秒 | 少し長くなった |

- recheckは1回ずつ（queueの`recheck/lock`の下）走り、commandはその中だけで走る。増えた20回を足しても、recheckの合計の時間とcommandの回数は前の窓より大きく減った。recheckが作るCPUの量は前の窓より少ない。
- `load1`のp90と`work`の中央値は上がったが、recheckの時間とcommandの回数が減っているので、recheckの増加では説明できない。`work`の増えは`idle`（中央値150秒→225秒）・`model`（362秒→410秒）・`test`（252秒→293秒）にも広がっていて、hostのCPUに依らない待ちも含む。hostの記録が埋まっている時間（`host.cpu_secs.covered_secs`）も前100,252秒・後145,041秒と違うので、loadの分布は両窓で同じ条件の標本ではない。
- 着地の順番待ちは短くなり、recheckが着地を遅らせた跡は見えない（recheckはrunが`integrating`の間は始まらない）。

効いていないので打ち手のfollow_upは無い。command 1回ずつの時間は、recheckの合計の時間とcommandの回数がどちらも減ったことで判断できたので、計装のtaskも要らない。

## 5. 数え方の再現

```sh
B0=2026-10-01T01:28:15.711Z; T=2026-10-03T01:28:15.711Z; A1=2026-10-05T01:28:15.711Z
dagq events --all --full --kind landing_recheck_finished --since $B0 --until $T  --limit 10000 > before.json
dagq events --all --full --kind landing_recheck_finished --since $T  --until $A1 --limit 10000 > after.json
jq '[.events[].payload] as $p | {n: ($p|length), checked: ([$p[].checked]|add),
    clean: ([$p[].clean]|add), check_failed: ([$p[].check_failed]|add), errors: ([$p[].errors]|add),
    conflicts: ([$p[].conflicts]|add), resumed: ([$p[].resumed]|add), dur: ([$p[].duration_secs]|add),
    null_landed: ([$p[]|select(.landed_run_id==null)]|length)}' after.json
dagq stats --full --since $T --until $A1 | jq '{landing_rechecks: (.landing_rechecks|del(.runs)), host: .host.metrics.load1}'
# きっかけの分け方には、同じ範囲の run_integrated と supervisor_started を --kind で取り、
# landed_run_id の run_integrated の actor.requested_by と、その間の supervisor_started を突き合わせる
```
