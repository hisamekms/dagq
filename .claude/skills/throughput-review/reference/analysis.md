# 増減の理由の分析のコマンドと結論の分け方

[SKILL.md](../SKILL.md) の「2. 増減の理由の分析」の、対象の時間の決め方、(a)(b)(トークン) のコマンドと読み方、(c) の結論の分け方。

> `stats`・`kpi`・`report` は台帳から読む（[SKILL.md](../SKILL.md) の共通の決まり）。待ちの内訳は `stats` の `breakdown` の blocker・holds の割合で読み、`unattributed` の割合を添える。

対象の時間（外れた時、下回りが続いた 3 時間など）を UTC の `FROM`・`TO` にして（JST から 9 時間引く）、次を並べる。比べる相手は同じ手順で出した直前の 6〜24 時間。

(a) その時間に着地した run の中身。task の種類と、claim→着地・作業・検証・着地待ちの時間、人の答え待ち、resume:

```sh
FROM=2026-09-28T13:00:00Z; TO=2026-09-28T14:00:00Z
~/.local/bin/dagq stats --since "$FROM" --until "$TO" --full | jq -r '
  "run\ttask\tchange\tareas\tclaim→着地(分)\t作業\t検証\t着地待ち\tverify\tresume\ttitle",
  (.runs[] | select(.status == "integrated")
   | def m: ./60 | floor;
     def t: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
     [.run_id, .task_id, (.change // "-"), ((.areas // []) | join(",") | if . == "" then "-" else . end),
      ((.landed_at|t) - (.claimed_at|t) | m), (.work|m), (.validate|m),
      (.wait_to_land|m), (.land_phases.verify|m), .resumes, .title[0:40]] | @tsv)'
```

人の答え待ち（分）は run ごとに `timeline` の gap から足す（上の表の `run` の値を `RUN` に）:

```sh
~/.local/bin/dagq timeline "$RUN" --gap 1 | jq -r '
  [.gaps[] | select(.blocker == "human")] as $h
  | "人の答え待ち \([$h[] | select(.reason != "push_pending") | .secs] | add // 0 | ./60 | floor) 分（push_pending を除く）\t\($h | group_by(.reason) | map("\(.[0].reason)=\([.[].secs] | add / 60 | floor)") | join(" "))\tlegacy \([.gaps[] | select(.reason == "legacy")] | length)"'
```

- 時間はどれも分。「人の答え待ち」は [SKILL.md](../SKILL.md) の共通の決まりの除く分で、run ごとには上の `timeline` の `blocker` が `human` の gap（`waiting`・`landing_answer`・`claim_answer`・`push_pending`・`held`）だけから一度だけ数え、`land_phases.ask` を別に引かない（`landing_answer` の gap と重なる）。claim→着地から引くのは着地より前の gap で、着地の後の `push_pending` は上の jq の合計から除く（工程ごとの値には出る）。`--gap` の既定 300 秒では短い gap が出ない。gap の `reason` が `legacy` の run（工程の記録の無い run）は区間を作り直さないので答え待ちを除けず、含む値だけを出してそう書く。時間の範囲の全体で引くなら `stats` の `breakdown.overall.duration.by_blocker.human.secs // 0`（範囲は claim から push の終わりまでで `push_pending` を含むので、claim→着地の和ではなく `duration.secs` から引く）
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

(トークン) トークン消費の増減は、`kpi` の `tokens` と `tokens_per_landing`（`actor=`・`provider=`・`model=` の層）を前の期間と並べ、印や時刻の前後は `--compare` で比べる。対象の時間の内訳は `stats` の `execution_tokens`（`by_actor`・`by_provider`・`by_model`）で読む:

```sh
~/.local/bin/dagq kpi --period day --last 7 | jq '.periods[] | {label, tokens: .kpis.tokens, per_landing: .kpis.tokens_per_landing, unavailable}'   # 週は --period week
~/.local/bin/dagq kpi --compare 'A..B,C..D'                  # 2 つの窓（印や時刻の前後も）
~/.local/bin/dagq stats --since "$FROM" --until "$TO" | jq '.execution_tokens'
```

- 欄の意味と日への振り分けは plugin の dagq skill の [reference/kpi.md](../../../../plugins/claude-dagq/skills/dagq/reference/kpi.md) の Tokens と [reference/inspect.md](../../../../plugins/claude-dagq/skills/dagq/reference/inspect.md) の `execution_tokens` が持つ
- 記録の始まりと欠けは `execution_tokens` の `recorded_from` と `coverage`（`--compare` では両側の `token_coverage`）で分かる。`kpi` の値が null（`not_recorded` / `partly_recorded`）の期間は記録の無い（途中から始まる）期間として比べない。`actor=` の層はその actor の記録の始まりで null になるので、`all` などが値を持っても actor ごとの `recorded_from` を確かめる

(c) 結論を分ける。書くのは次のどれか（重なるなら全部）と、その根拠の数字:

- **軽い task の偏り**: 増えた時間の着地が `docs`・`plugin`（作業が短い）に偏り、`runtime` の claim→着地の中央値は前と変わらない → 本当に速くなったのではない
- **本当に速くなった / 遅くなった**: 同じ種類（`runtime` どうし）で claim→着地・作業・検証の中央値が動いた。動いた相では `land_phases`（verify・landing_queue・review など）まで割る
- **入口で詰まった**: slot が空いていた（`slots` の used < parallel）のに `claim_deferred` か `claim_holds`（load）が多い、候補が無い
- **人待ち**: 答え待ちを除くと時間が戻る。ask と attention が溜まっていた
- **着地の直列で詰まった**: 着地待ちの中の `landing_queue` が伸びた、verify が長い
- **一時的な事象**: supervisor の停止・入れ替え（`update_installed` など）、resume の集中
