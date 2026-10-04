# 毎時の確認のコマンドと読み方

[SKILL.md](../SKILL.md) の「1. 毎時の確認」の (a)(b) のコマンドと読み方。

(a) 着地数の表。直近 6 時間の完了した時と今の途中の時、3h・6h の移動平均、完了した 24 時間の平均、分析を始める条件の判定を出す:

```sh
since=$(date -u -v-30H +%Y-%m-%dT%H:%M:%SZ)   # GNU date なら: date -u -d '30 hours ago' +%Y-%m-%dT%H:%M:%SZ
~/.local/bin/dagq events --kind run_integrated --since "$since" --limit 10000 | jq -r '
  def hour: floor | . - (. % 3600);
  def jst: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601 + 9*3600;
  def avg(a): if (a|length) == 0 then null else ((a | map(.n) | add) / (a|length) * 10 | round / 10) end;
  (now + 9*3600 | hour) as $cur
  | ([.events[].created_at | jst | hour | tostring] | group_by(.) | map({key: .[0], value: length}) | from_entries) as $c
  | [range(29; -1; -1) | ($cur - . * 3600) as $h | {h: $h, n: ($c[$h|tostring] // 0)}] as $hs
  | ($hs[:-1]) as $done
  | avg($done[-24:]) as $d24
  | [range(3) | (($done | length) - 1 - .) as $i | avg($done[$i-2:$i+1])] as $ma3
  | ($done[-1].n) as $last | avg($done[-7:-1]) as $prev6
  | "時(JST)\t着地\t3h平均\t6h平均",
    ($done | to_entries[-6:][] | .key as $i
      | "\(.value.h | strftime("%m-%d %H時"))\t\(.value.n)\t\(avg($done[$i-2:$i+1]))\t\(avg($done[$i-5:$i+1]))"),
    "\($hs[-1].h | strftime("%m-%d %H時"))（途中）\t\($hs[-1].n)\t-\t-",
    "完了した24時間の平均\t\($d24)\t件/時",
    "判定: 直近の時が6h平均から外れた=\((($last - $prev6) | fabs) >= ([$prev6 * 0.5, 3] | max)) 3h平均が24h平均の7割未満で3時間=\(all($ma3[]; . < $d24 * 0.7)) 着地0件の時=\(any($done[-6:][]; .n == 0))"'
```

- 「6h 平均からのずれ」は、直近の完了した時を、その前の 6 時間（その時を含めない）の平均と比べる
- 表の後に前回の確認（この session の直前の報告）からの増減を 1 行で書く（例: 「3h 平均 4.0 → 5.3」）。前回が無ければ書かない

(b) 今の slot・待ち・ask・attention・claim の保留:

```sh
~/.local/bin/dagq status | jq '{
  slots: [.supervisors[] | .slots | "\(.used)/\(.parallel)"],
  waiting: [.supervisors[] | .waiting | "\(.count)/\(.limit)（戻り待ち \(.returning)）"],
  asks: (.asks | length),
  attention: [.attention[] | .kind // .reason // .] ,
  claim_deferrals: (.claim_deferrals | group_by(.reason) | map({(.[0].reason): length}) | add)}'
```
