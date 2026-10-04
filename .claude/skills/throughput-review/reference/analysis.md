# 増減の理由の分析のコマンドと結論の分け方

[SKILL.md](../SKILL.md) の「2. 増減の理由の分析」の、対象の時間の決め方、(a)(b)(トークン) のコマンドと読み方、(c) の結論の分け方。

対象の時間（外れた時、下回りが続いた 3 時間など）を UTC の `FROM`・`TO` にして（JST から 9 時間引く）、次を並べる。比べる相手は同じ手順で出した直前の 6〜24 時間。

(a) その時間に着地した run の中身。task の種類と、claim→着地・作業・検証・着地待ちの時間、人の答え待ち、resume:

```sh
FROM=2026-09-28T13:00:00Z; TO=2026-09-28T14:00:00Z
~/.local/bin/dagq stats --since "$FROM" --until "$TO" --full | jq -r '
  "task\tchange\tareas\tclaim→着地(分)\t人待ち除く\t作業\t検証\t着地待ち\tverify\t人の答え待ち\tresume\ttitle",
  (.runs[] | select(.status == "integrated")
   | def m: ./60 | floor;
     def t: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
     [.task_id, (.change // "-"), ((.areas // []) | join(",") | if . == "" then "-" else . end),
      ((.landed_at|t) - (.claimed_at|t) | m), ((.landed_at|t) - (.claimed_at|t) - .land_phases.ask | m), (.work|m), (.validate|m),
      (.wait_to_land|m), (.land_phases.verify|m), (.land_phases.ask|m), .resumes, .title[0:40]] | @tsv)'
```

- 時間はどれも分。「人の答え待ち」は着地待ちの中の ask で、作業中の答え待ちが疑わしい run は `~/.local/bin/dagq timeline RUN` の `waiting_ask` の区間を足し、除いた値も並べる
- 種類の層: task が宣言した `change`（`dagq.toml` の `[tasks] changes` の値。change の無い task と古いバイナリの記録では `-`）と、`dagq.toml` の `[areas]` から求める `areas`（着地 commit の差分から。`[areas]` が無い間と古いバイナリの記録では `-`）で分ける

(b) claim の保留と resume と ask（同じ `FROM`・`TO`）:

```sh
~/.local/bin/dagq events --since "$FROM" --until "$TO" --kind claim_deferred --limit 10000 --full \
  | jq -c '[.events[].payload.reason] | group_by(.) | map({(.[0]): length}) | add'
~/.local/bin/dagq stats --since "$FROM" --until "$TO" --full | jq -c '.claim_holds'
~/.local/bin/dagq events --since "$FROM" --until "$TO" --kind resume_started --kind ask_opened --limit 10000 --full \
  | jq -r '.events[] | [.created_at, .kind, .task_id, ((.payload.reason // .payload.kind // "") | tostring | .[0:80])] | @tsv'
```

- `claim_deferred`（衝突の多いファイルでの保留。reason は `hot_files`）の数と、`stats` の `claim_holds`（load などによる claim の保留。`by_reason.load_average` など、件数と秒）で、slot が空いていたのに claim されなかった時間を見る
- resume と ask は、作業のやり直しと人待ちで slot が埋まっていたかを見る

(トークン) トークン消費の増減を見るときは、`stats` の `sessions.by_kind.tokens` を使わない（session が閉じた日にまとめて数え、Claude の subagent と Codex の job の分が抜ける）。host の transcript と rollout を数える当面の script で、actor × JST の日と着地 1 件あたりを比べる:

```sh
python3 scripts/token-usage.py --repo ~/ghq/github.com/hisamekms/dagq --dagq ~/.local/bin/dagq --since 2026-09-26 --until 2026-10-03   # --format json も出せる
```

- 日は JST、`--since` / `--until` はその両端を含む。queue の dir の既定は repository で打った `dagq locate` から求める。dagq が起動していない session（人が checkout で開いた session など）は合計に入れず、数だけを出す
- この script と節は、恒久の記録（goal 95 の kpi / stats の Execution の軸、task 1494）が入ったら外す

(c) 結論を分ける。書くのは次のどれか（重なるなら全部）と、その根拠の数字:

- **軽い task の偏り**: 増えた時間の着地が `docs`・`plugin`（作業が短い）に偏り、`runtime` の claim→着地の中央値は前と変わらない → 本当に速くなったのではない
- **本当に速くなった / 遅くなった**: 同じ種類（`runtime` どうし）で claim→着地・作業・検証の中央値が動いた。動いた相では `land_phases`（verify・landing_queue・review など）まで割る
- **入口で詰まった**: slot が空いていた（`slots` の used < parallel）のに `claim_deferred` か `claim_holds`（load）が多い、候補が無い
- **人待ち**: 答え待ちを除くと時間が戻る。ask と attention が溜まっていた
- **着地の直列で詰まった**: 着地待ちの中の `landing_queue` が伸びた、verify が長い
- **一時的な事象**: supervisor の停止・入れ替え（`update_installed` など）、resume の集中
