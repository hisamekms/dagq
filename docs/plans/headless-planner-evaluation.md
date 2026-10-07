---
id: plan-headless-planner-evaluation
type: plan
title: runtime の planner の対話の期間の基準値と、非対話に切り替えた後の評価のコマンドと、対話に戻す基準の案
status: active
created: 2026-10-03
owners:
  - hisamekms
tags:
  - measurement
  - planner
  - headless
related:
  - adr-t1394-1
  - adr-t1394-2
  - adr-t1404-1
  - adr-t1340-1
  - adr-0048
  - adr-0051
  - plan-headless-default-evaluation
  - plan-headless-worker-measurement
  - plan-headless-background-evaluation
  - design-supervisor-lifecycle-plan-planners
---

# runtime の planner の対話の期間の基準値と、非対話に切り替えた後の評価

goal 87 の task 1401 の測定。[ADR-t1394-2](../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md) は、runtime の planner の経路（対話・非対話）を `dagq.toml` の `[roles.runtime_planner]` で選べるようにし、まずこの repository だけを非対話にして印を打ち、1 週間後の評価で既定を変えるかを決めるとした（決定 1）。2026-10-02 に人は「非対話にして 1 週間測り、問題なければ非対話を続ける」と決めた。この文書はその判定の物差しを切り替えの前に決めておくもので、次の 4 つを書く。

1. 基準値: 切り替えの前の 7 日の runtime の planner（revise・draft・finding）を、種類ごとに分けた値。人が開いた planner は別の列にする
2. 切り替えた後に同じ値を読む評価のコマンドと、読むのに要る標本の数（min_samples）
3. 対話に戻す基準の案（決めるのは人）
4. 重なる変更（confounders）の読み方

測って書くだけで、設定も queue の状態も変えていない。形は worker の評価（[headless-default-evaluation](headless-default-evaluation.md)、task 1368）に合わせた。

## 読んだ範囲と方法

- 読んだ時点: 2026-10-02T15:55Z（UTC）ごろ。固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+dfdf4456`）の状態を変えないコマンド（`events`・`stats`・`kpi`）だけを使った（worker の session のクライアントモードで queue service から読んだ。`dagq planners` は service のユースケースに無いので使っていない）
- 窓: `2026-09-25T15:00:00Z` 以上 `2026-10-02T15:00:00Z` 未満（7 日）に記録された event（56,685 件、event ID 7651〜64335）。`stats` と `kpi` も同じ `--since` / `--until` で読んだ
- planner の種類: event から planner の ID ごとに決めた。`draft_planner_opened` の planner が **draft**、`finding_planner_opened` が **finding**、`plan_revise_sent` の `opened: true`（revise のために runtime が新しく立てた）が **revise**。人が開いた planner は `session_opened` の `kind: planner` の planner。窓の planner は全て対話（非対話の経路がまだ無い）
- 記録が窓の途中から始まるもの: 次の event は新しい runtime から記録されるので、基準値はその時刻からの値になる。後の窓では全て窓の全体にある
  - runtime の planner の区間（`session_opened` / `session_closed` の `kind: runtime_planner`）: 2026-09-27T12:18Z から
  - `draft_planner_settled`（draft の決着）: 2026-09-28T15:01Z から
  - `planner_closed`: 2026-10-02T11:11Z から（[ADR-t1300-1](../adr/2026-10-02-t1300-1-runtime-closes-exited-person-planners-after-a-grace.md) の実装の後）
  - event の `actor`: 2026-09-26T23:11Z までの event には無い（`planner_question` の ask 20 件は聞いた planner が分からない）

```sh
# 窓の event（1 回の応答が 16 MiB を超えると queue service の答えが切れるので 6 時間ずつ読む）
python3 - 2026-09-25T15:00:00Z 2026-10-02T15:00:00Z > ev.json <<'EOF'
import json, subprocess, sys, datetime as D
f = D.datetime.fromisoformat(sys.argv[1].replace('Z', '+00:00')); t = D.datetime.fromisoformat(sys.argv[2].replace('Z', '+00:00'))
out, s = [], f
while s < t:
    u = min(s + D.timedelta(hours=6), t); z = lambda x: x.strftime('%Y-%m-%dT%H:%M:%SZ')
    r = json.loads(subprocess.run(['dagq', 'events', '--all', '--full', '--since', z(s), '--until', z(u), '--limit', '200000'],
                                  capture_output=True, text=True, check=True).stdout)
    out += r['events']; s = u
json.dump(out, sys.stdout)
EOF
dagq stats --full --since 2026-09-25T15:00:00Z --until 2026-10-02T15:00:00Z > stats.json
dagq kpi --since 2026-09-25T15:00:00Z --until 2026-10-02T15:00:00Z > kpi.json
python3 planner.py ev.json        # 下の「評価のコマンド」の planner.py
```

## 1. 基準値（対話、2026-09-25T15:00Z〜2026-10-02T15:00Z）

時間は秒。「中央値 / p90 / 最大（件数）」の形。

### planner の数と種類

| 種類 | planner | 補足 |
|---|---|---|
| draft | 590 | draft 727 件に対して立った（bundle ごとに 1 つ。`stats` の `draft_flow.bundles` 590 と一致）。`draft_planner_opened` の `attempt` は 1 が 727、2 が 13、3 が 1 |
| revise | 86 | `plan_revise_sent` 95 件のうち、runtime が新しく立てたもの 86。生きている runtime の planner に送ったもの 1、人の planner に送ったもの 8 |
| finding | 8 | 印の付いた finding ごとに 1 つ |
| runtime の planner の合計 | 684 | 1 日あたり約 98 |
| 人が開いた planner（参考） | 29 | `session_opened` の `kind: planner`。ADR-t1394-1 で廃止するので、後の窓では 0 に近づく |

日ごとの `draft_planner_opened` は 19（09-25 の 9 時間）・201・111・135・99・21（09-30、Claude の無効の期間）・74・81。

### revise の依頼から submit --proposal まで

`plan_revise_sent` から、同じ proposal の次の出し直しの記録（`submit --proposal` が記録する `task_submitted`。`proposal_resubmitted` が先に来ればそれ）まで。出し直しの後の plan review の起動待ち（`plan_review_started` まで）は planner の外の時間なので含めず、別の列に並べる（例: proposal 4 は依頼 2026-09-25T23:02:40Z、`task_submitted` 23:03:14Z、`plan_review_started` 23:29:25Z で、出し直しまでは 35 秒）。`proposal_withdrawn` と `plan_revise_lost` で終わったものは時間に入れず、件数を並べる。

| 宛先 | 依頼 | 出し直しまで | 同（人の答え待ちを除く） | 参考: 出し直しから plan review の起動まで | 出し直しの記録が無い・出し直さずに終わった |
|---|---|---|---|---|---|
| runtime の planner | 87 | 54 / 110 / 197（66） | 54 / 110 / 197（66） | 2 / 75 / 1,571（66） | 記録なし 15、取り下げ 6 |
| 人の planner（参考） | 8 | 45 / 59 / 67（6） | 45 / 59 / 67（6） | 32 / 95 / 131（6） | `plan_revise_lost` 2 |

**出し直しの記録が無い 15 件**: plan review が ready の task を submitted に戻した reopen の revise（15 件のうち 13 件は直前に `task_reopened`、例 proposal 474）。task が submitted のままなので、planner は task を直すだけで `task_submitted` が記録されず、planner が閉じた後に次の `plan_review_started` が来る。終点が無いので「出し直しまで」の標本から外し、参考に依頼から planner の最後の編集（`task_edited`・`dependency_added`・`dependency_removed`）までを数えると 46 / 61 / 99（14。1 件は編集の記録も無い）。後の窓も同じ扱いにする（件数を並べ、時間は記録のある標本だけで比べる）

人の答え待ちは、区間に重なる `planner_question` の ask（ask の task が proposal の task のもの）の開いてから答えまでの秒を引いた。基準の窓では revise の区間に重なる ask が無かった（revise の planner が聞いた ask 3 件は proposal の task に紐づかず、区間の外）。

### draft と finding の決着まで

| 種類 | 決着の内訳 | 時間 | 同（人の答え待ちを除く） |
|---|---|---|---|
| draft（最初の `draft_planner_opened` → `draft_planner_settled`。2026-09-28T15:01Z から） | `submitted` 171・`canceled` 87・`duplicate` 70・`undecided` 11（決めずに終わった率 3.2%） | 139 / 3,080 / 126,420（339） | 123 / 405 / 126,420（339。34 件は答え待ちを含む） |
| finding（`finding_planner_opened` → planner による最初の `finding_status_changed`） | `proposed` 7・`dismissed` 1 | 124 / 224 / 257（8） | 同じ（答え待ちなし） |

`kpi` の `plan.follow_up_draft_secs`（draft の作成から決着まで。planner の枠の空き待ちを含む）は 430 / 35,154 / 157,240（730）。

### planner_question

70 件、全て答えられた（`answered_by` は窓の後半から `inbox` の代行 50 件、前半の 20 件は記録なし）。runtime の planner 1 つあたり 0.10 件。

| 聞いた planner | 開いてから答えまで |
|---|---|
| draft | 231 / 18,420 / 23,768（47） |
| revise | 486 / 486 / 12,072（3） |
| 不明（actor の記録の前） | 102 / 12,311 / 18,857（20） |
| 全て | 191 / 13,814（70）、合計 278,046 |

答えまでの時間は人の答え待ちそのもので経路に依らないので、比べるのは件数（planner 1 つあたり）と、上の「人の答え待ちを除く」時間にする。

### 止まり方

| event | 件数 | 中身 |
|---|---|---|
| `planner_unresponsive` | 7 | revise を取る planner が 1 時間現れない（`reason` が `... waited N seconds for a planner to take it`）5、planner が 1 時間何も見せない（`subject: planner`、`no input, no idle marker, no idle screen`）2（planner 548・670、どちらも draft） |
| `planner_closed` | 30（10-02T11:11Z から） | `origin: runtime` の `runtime_exited` 28、`origin: person` の `abandoned` 2 |
| exhausted の attention（`draft_planner_exhausted`・`finding_planner_exhausted`） | 0 | — |
| `plan_revise_lost` | 2 | どちらも人の planner |

### 区間と token（`stats.sessions.by_kind`）

`stats` の値（2026-09-27T12:18Z から）。`kpi` の `session_open.runtime_planner` / `session_active.runtime_planner` はこの合計（324,178 / 29,039）、`session_active_ratio.runtime_planner` は 0.09。

| kind | 区間 | open（中央値 / p90 / 合計） | active（中央値 / p90 / 合計） | active の割合 | token total | output |
|---|---|---|---|---|---|---|
| `runtime_planner` | 425 | 54 / 218 / 324,178 | 48 / 130 / 29,039 | 0.09 | 3 億 6,156 万 | 184.99 万 |
| `planner`（人、参考） | 29 | 245 / 4,317 / 52,832 | 173 / 465 / 37,029 | 0.701 | 7,023 万 | 44.25 万 |

runtime の planner の区間を planner の種類に分けたもの（`planner.py` の `sessions`。区間 1 つが planner 1 つ）:

| 種類 | 区間 | open | active | token total（1 区間） | output（1 区間） |
|---|---|---|---|---|---|
| draft | 370 | 51 / 211 | 45 / 128 | 60.8 万 / 161.5 万 | 2,898 / 8,877 |
| revise | 47 | 66 / 192 | 63 / 108 | 85.8 万 / 168.7 万 | 5,041 / 9,328 |
| finding | 8 | 164 / 420 | 120 / 227 | 187.9 万 / 256.9 万 | 8,460 / 15,101 |

active の割合が 0.09 と低いのは、閉じるのが遅れた少数の区間（open の最大 124,783 秒）が合計を押し上げるため。中央値では open と active はほぼ同じ。対話の区間の token は transcript から数え、`cost_usd` は記録されない（非対話は turn の `result.usage` から数え、cost も持つ。数え方の違いは [非対話の worker の測定](headless-worker-measurement.md) の「読み方」）。

### kpi の plan.*（窓の全体、層は proposal の出どころ）

| KPI | all | `origin=follow_up` | `origin=person` | `origin=runtime` | `origin=observer` |
|---|---|---|---|---|---|
| `plan.revise_rate` | 0.147（565） | 0.07（286） | 0.38（71） | 0.182（22） | 0.0（8） |
| `plan.task_rework_rate` | 0.226（557） | 0.168（244） | 0.337（104） | 0.387（31） | 0.375（8） |
| `plan.duplicate_cancels_after_ready` | 5（424） | 1 | 0 | 0 | 0 |
| `plan.follow_up_canceled_after_adoption` | 7（405） | 2 | — | — | — |
| `plan.follow_up_adoption_rate` | 0.573（730） | | | | |
| `plan.follow_up_duplicate_rate` | 0.163（730） | | | | |

ほかに `drafts_per_landing` 1.19、`draft_backlog` 3、`finding_resolve_time` 19,273 / 157,887（52）、plan review の verdict は pass 0.846・revise 0.145・concern 0.009（565）。

## 2. 評価のコマンド

### 窓

切り替えの task が `dagq.toml` を非対話にして着地し、main checkout に反映された時刻に印（`dagq mark '<label>' --at <効いた時刻>`）を打つ。その印を `M`、時刻を `TM` とする。

- 基準: この文書の窓（`2026-09-25T15:00:00Z..2026-10-02T15:00:00Z`）を固定の基準にし、加えて印の前の 7 日（`TM` − 7 日〜`TM`）も同じコマンドで読む（区間・`draft_planner_settled`・`planner_closed` が窓の全体にある対話の値になる）
- 後: `TM`〜`TM` + 7 日以上（下の min_samples を満たすまで延ばす）。印の時点で生きていた対話の planner は対話のまま終わるので、planner ごとの経路で分ける

```sh
B=<TM − 7 日>..<TM>; A=<TM>..<TM + 7 日>
# 区間と plan.* を経路で（session_* の route=interactive / route=headless の層は --by によらず常に出る。task 1398）
dagq kpi --compare "$B,$A" > kpi-route.json
dagq kpi --compare M > kpi-mark.json                      # separable と overlapping、confounders を確かめる
for k in session_open.runtime_planner session_active.runtime_planner session_active_ratio.runtime_planner \
         plan.revise_rate plan.task_rework_rate plan.follow_up_draft_secs plan.follow_up_adoption_rate \
         plan.follow_up_duplicate_rate plan.duplicate_cancels_after_ready draft_backlog; do
  jq -c --arg k "$k" '{k: $k, s: .compare.strata[$k]}' kpi-route.json
done
jq -c '.compare.confounders[] | select(.kind == "mark_recorded") | {at, label, position}' kpi-mark.json
# 区間の経路ごとの値と planner の経路ごとの様子（task 1398 の stats）
dagq stats --full --since <A の始まり> --until <A の終わり> | jq -c '{sessions: .sessions.by_route.runtime_planner, planners: .planner_routes}'
```

`kpi` の自動更新の引き継ぎは着地ごとに起きるので、印での比較は重なった変更にまとまりうる（[headless-default-evaluation](headless-default-evaluation.md) の 3）。数字は時刻で明示した `"$B,$A"` を主にする。

### planner ごとの集計（1 の表の全ての値）

上の event の集め方で後の窓の `ev.json` を作り、次を `planner.py` として保存して流す。経路は `session_opened` の `route`（ADR-t1394-2 決定 4、task 1398。非対話の planner の区間は最初の turn が `route: headless` で開く）で、無ければ対話に数える。非対話の planner の turn は run の無い `turn_finished` の `planner_id` で数える（[plan-planners](../design/supervisor-lifecycle/plan-planners.md) の「runtimeのplannerの経路」の「区間」。同じ数え方を `stats` の `planner_routes` が持つ）。

```python
# 使い方: python3 planner.py ev.json   （ev.json は窓の event の配列）
import json, sys, datetime, statistics as st, collections as C
ev = json.load(open(sys.argv[1]))
def ts(s): return datetime.datetime.fromisoformat(s.replace('Z', '+00:00')).timestamp()
def med(xs):
    xs = sorted(x for x in xs if x is not None)
    return {'n': len(xs), 'median': round(st.median(xs)), 'p90': round(xs[int(0.9 * (len(xs) - 1))]), 'max': round(xs[-1])} if xs else {'n': 0}
P = lambda e: e.get('payload') or {}
# planner の種類（runtime: draft / finding / revise）と、人が開いた planner
kind, route = {}, {}
for e in ev:
    p = P(e)
    if e['kind'] == 'draft_planner_opened': kind.setdefault(p['planner_id'], 'draft')
    elif e['kind'] == 'finding_planner_opened': kind.setdefault(p['planner_id'], 'finding')
    elif e['kind'] == 'plan_revise_sent' and p.get('opened'): kind.setdefault(p['planner_id'], 'revise')
    if e['kind'] == 'session_opened' and p.get('kind') == 'runtime_planner':
        route[p.get('planner_id')] = p.get('route') or 'interactive'   # 非対話は route: headless（task 1398）
person = {P(e).get('planner_id') for e in ev if e['kind'] == 'session_opened' and P(e).get('kind') == 'planner'}
print('planners', dict(C.Counter(kind.values())), 'person', len(person), 'route', dict(C.Counter(route.values())))
# planner_question（開いた→答え。人の答え待ち）
qo, qa, qt, qk = {}, {}, {}, {}
for e in ev:
    p = P(e)
    if e['kind'] == 'ask_opened' and p.get('kind') == 'planner_question':
        qo[p['ask_id']] = ts(e['created_at']); qt[p['ask_id']] = e['task_id']
        a = (e.get('actor') or {}).get('id') or ''
        pid = int(a.split(':')[1]) if a.startswith('planner:') else None
        qk[p['ask_id']] = 'person' if pid in person else kind.get(pid, 'unknown')
    if e['kind'] == 'ask_answered' and p.get('kind') == 'planner_question': qa[p['ask_id']] = ts(e['created_at'])
print('planner_question', {k: med([qa[a] - qo[a] for a in qo if a in qa and qk[a] == k]) for k in sorted(set(qk.values()))},
      'unanswered', len([a for a in qo if a not in qa]))
def human(tasks, s, t):   # [s, t) に重なる、tasks に紐づく planner_question の答え待ちの秒
    return sum(max(0, min(qa[a], t) - max(o, s)) for a, o in qo.items() if a in qa and qt[a] in tasks)
# revise: plan_revise_sent → 同じ proposal の次の出し直しの記録（task_submitted / proposal_resubmitted）か取り下げ。
# 記録が無く plan_review_started が先に来るもの（reopen の revise）は no_submit_record として件数だけ数え、参考に最後の編集までを出す
ptasks = C.defaultdict(set)
for e in ev:
    if P(e).get('proposal_id') is not None and e.get('task_id') is not None: ptasks[P(e)['proposal_id']].add(e['task_id'])
rv, rvh, rest, edit, ends = C.defaultdict(list), C.defaultdict(list), C.defaultdict(list), C.defaultdict(list), C.Counter()
for i, e in enumerate(ev):
    if e['kind'] != 'plan_revise_sent': continue
    p = P(e); pid = p['proposal_id']; t0 = ts(e['created_at'])
    k = 'person' if p.get('planner_id') in person else 'runtime'
    end = next((x for x in ev[i + 1:] if P(x).get('proposal_id') == pid and x['kind'] in
                ('task_submitted', 'proposal_resubmitted', 'plan_review_started', 'proposal_withdrawn', 'plan_revise_lost')), None)
    ek = end['kind'] if end else 'open'
    if ek in ('task_submitted', 'proposal_resubmitted'):
        ek = 'resubmitted'; d = ts(end['created_at']) - t0
        rv[k].append(d); rvh[k].append(d - human(ptasks[pid], t0, t0 + d))
        nxt = next((x for x in ev[i + 1:] if P(x).get('proposal_id') == pid and x['kind'] == 'plan_review_started'), None)
        if nxt: rest[k].append(ts(nxt['created_at']) - ts(end['created_at']))
    elif ek == 'plan_review_started':
        ek = 'no_submit_record'
        eds = [x for x in ev[i + 1:ev.index(end)] if x.get('task_id') in ptasks[pid] and x['kind'] in ('task_edited', 'dependency_added', 'dependency_removed')]
        if eds: edit[k].append(ts(eds[-1]['created_at']) - t0)
    ends[f'{k}/{ek}'] += 1
print('revise', {k: med(v) for k, v in rv.items()}, 'without human', {k: med(v) for k, v in rvh.items()}, dict(ends))
print('revise: submit→plan_review_started', {k: med(v) for k, v in rest.items()}, 'no_submit_record: →last edit', {k: med(v) for k, v in edit.items()})
# draft: draft の最初の draft_planner_opened → draft_planner_settled
op, dd, ddh, outc = {}, [], [], C.Counter()
for e in ev:
    if e['kind'] == 'draft_planner_opened': op.setdefault(e['task_id'], ts(e['created_at']))
    if e['kind'] == 'draft_planner_settled':
        outc[P(e)['outcome']] += 1
        if e['task_id'] in op:
            t1 = ts(e['created_at']); t0 = op[e['task_id']]
            dd.append(t1 - t0); ddh.append(t1 - t0 - human({e['task_id']}, t0, t1))
print('draft', 'opened', len(op), dict(outc), med(dd), 'without human', med(ddh))
# finding: finding_planner_opened → planner による最初の finding_status_changed
fo, fd, fout = {}, {}, C.Counter()
for e in ev:
    p = P(e)
    if e['kind'] == 'finding_planner_opened': fo.setdefault(p['finding_id'], ts(e['created_at']))
    if e['kind'] == 'finding_status_changed' and p.get('by') == 'planner' and p['finding_id'] in fo and p['finding_id'] not in fd:
        fd[p['finding_id']] = ts(e['created_at']) - fo[p['finding_id']]; fout[p['to']] += 1
print('finding', 'opened', len(fo), dict(fout), med(list(fd.values())))
# 区間（session_closed の active_secs と tokens）を planner の種類 × 経路で
opened = {e['id']: e for e in ev if e['kind'] == 'session_opened' and P(e).get('kind') == 'runtime_planner'}
S = C.defaultdict(lambda: C.defaultdict(list))
for e in ev:
    p = P(e)
    if e['kind'] == 'session_closed' and p.get('kind') == 'runtime_planner' and p.get('opened_event_id') in opened:
        o = opened[p['opened_event_id']]; pid = P(o).get('planner_id')
        s = S[f"{kind.get(pid, 'unknown')}/{route.get(pid)}"]; tk = p.get('tokens') or {}
        s['open'].append(ts(e['created_at']) - ts(o['created_at'])); s['active'].append(p.get('active_secs'))
        s['total'].append(sum(tk.get(x, 0) for x in ('input', 'output', 'cache_creation', 'cache_read'))); s['output'].append(tk.get('output'))
for k, s in sorted(S.items()): print('sessions', k, {m: med(v) for m, v in s.items()})
# 止まり方
print('planner_unresponsive', dict(C.Counter('no_taker' if 'for a planner to take it' in P(e)['reason'] else 'silent' for e in ev if e['kind'] == 'planner_unresponsive')))
print('planner_closed', dict(C.Counter(f"{P(e).get('origin')}/{P(e).get('code')}" for e in ev if e['kind'] == 'planner_closed')))
print('exhausted', dict(C.Counter(e['kind'] for e in ev if e['kind'] in ('draft_planner_exhausted', 'finding_planner_exhausted'))),
      'draft attempts', dict(C.Counter(P(e)['attempt'] for e in ev if e['kind'] == 'draft_planner_opened')))
print('turns', dict(C.Counter(f"{P(e).get('outcome')}/{P(e).get('failure')}" for e in ev if e['kind'] == 'turn_finished' and P(e).get('planner_id'))))
```

後の窓では、上の値を planner の経路（`route`）でも分ける: revise・draft・finding の時間と決着の内訳と `planner_question` の件数を、planner の ID から `route` を引いて層にする（スクリプトの `kind` の代わりに `f"{kind}/{route}"` で数える）。ADR-t1394-1 の inbox からの依頼の planner（実装の後の新しい種類）は、上の 3 種類と分けて数える（`kind.get(pid, 'unknown')` が `unknown` になる。依頼の event の kind が決まったら種類に足す）。

### min_samples

後の窓は `TM` から 7 日以上で、次を満たすまで延ばす。満たさない層の率と中央値は判断に使わず、件数だけを並べる（`kpi` の層は `[kpi] min_samples` の既定 5 で判定しない層を出す）。

| 項目 | min_samples | 理由 |
|---|---|---|
| 非対話の draft の planner | 100 | 基準は 7 日で 590（1 日約 98）。日に 20〜200 と揺れるので、少ない日が続いても 1 週間で届く数にした |
| 決着した draft（`draft_planner_settled`） | 100 | 基準は 4.0 日（2026-09-28T15:01Z から）で 339。`undecided` の率（基準 3.2%）を数件の差で動かさないため |
| revise の出し直し（runtime の planner 宛て、出し直しの記録のあるもの） | 30 | 基準は依頼 87 のうち記録のある 66。依頼は日に 1〜33 と揺れる |
| finding の planner | 5（`kpi` と同じ） | 基準は 7 日で 8。5 に届かなければ件数と中身だけを書く |
| `planner_question` | 15 | 基準は 70。planner あたりの率を比べる |
| 非対話の planner の turn | 100 | turn の失敗の率を比べるため。draft の planner はふつう 1〜2 turn で終わる |

## 3. 対話に戻す基準の案

決めるのは人。数値は 1 の基準値から置いた。戻すとは、この repository の `dagq.toml` の `[roles.runtime_planner]` の経路を対話に戻すこと（着地して main checkout に反映されてから、新しく立つ planner から効く）。既定を変えるかは ADR-t1394-2 決定 1 のとおり別の ADR で決める。

| 項目 | 層 | 基準値（対話） | 戻すことを検討する目安（非対話） | 理由 |
|---|---|---|---|---|
| 非対話に固有の止まり方（planner の turn の `succeeded` 以外、wrapper を失った planner、turn の上限で止めた planner） | 非対話の planner の全て | —（対話は画面の推定の `planner_unresponsive` の `silent` 2 / 684） | 利用上限・認証の控えを除いて、planner の 2% 以上、かつ 5 件以上 | 対話の止まり方の率（0.3%）の数倍。少ない件数で動かないよう下限を置く。turn が動いているのに止まったものは原因を読んで経路に依るかを分ける |
| 決めずに終わった draft（`undecided`）と exhausted の attention | draft | `undecided` 3.2%（11 / 339）、exhausted 0 | `undecided` が 8% を超える、または exhausted が 3 件以上 | 決めずに終わると立て直し（3 回まで）と inbox への attention で人の手が増える。2 倍を超える幅 |
| draft の決着まで（人の答え待ちを除く） | draft | 123 / 405 | 中央値が 250、または p90 が 810 を超える | 基準の約 2 倍。非対話は画面の起動と入力の確認を省くので縮むはずで、伸びたら turn の駆動の待ちを疑う |
| revise の依頼から出し直しまで（人の答え待ちを除く） | revise | 54 / 110（出し直しの記録のある 66 件） | 中央値が 110、または p90 が 220 を超える | 同上。依頼が今の turn の後に届く（ADR-t1394-2 決定 7）ため、turn の長い planner で待ちが伸びうる |
| finding の決着まで | finding | 124 / 224（8） | min_samples を満たし、中央値が 250 を超える | 同上。標本が少ないので件数を満たしたときだけ |
| `planner_question` の件数 | runtime の planner の全て | 0.10 / planner | 0.20 / planner を超える | 非対話の planner は画面で人に聞けず、迷ったら ask にするしかない。ask が倍になると inbox と人の手が増えて切り替えの得を打ち消す。中身（`--topic` は無いので問いの文）を読み、推奨が出せる問い（[ADR-t451-1](../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)）が増えたなら prompt を直す task にする |
| plan review の差し戻し | `plan.revise_rate` の `origin=follow_up` | 0.07（286） | 0.12 を超える（plan review が Codex の期間どうしで比べる。4 の confounders） | draft の planner の proposal の質の目安。0.05 の上げ幅は件数 300 で 15 件程度の差 |
| 着地の後の手戻り | `plan.task_rework_rate` の `origin=follow_up` | 0.168（244） | 0.25 を超える | 計画の甘さが worker の手戻りに出る遅い指標。worker 側の変更（非対話の既定）も効くので、超えたら run の review の理由を読む |
| 重複の取り漏れ | `plan.follow_up_duplicate_rate`、`plan.duplicate_cancels_after_ready` | 0.163、5 | `duplicate_cancels_after_ready` が 10 を超える | planner が `search` / `related` を省くと ready の後の重複の cancel が増える |
| token | 種類ごとの 1 planner の token total の中央値 | draft 60.8 万、revise 85.8 万、finding 187.9 万 | 対話の基準の 1.5 倍を超える | worker では非対話で token が減った。増えたら turn の中の繰り返しか、依頼ごとに文脈を読み直しているしるし。数え方が違う（transcript と `result.usage`）ので、1.5 倍の幅を置く |
| `planner_unresponsive` の `no_taker` | revise | 5 | 目安にしない（件数を記録する） | 上限（`[supervisor] runtime_planners`）と planner の枠の埋まり方で起き、経路に依らない。非対話で planner が早く終われば減るはず |

判断の手順の案:

1. 目安に当たっても、すぐには戻さない。当たった planner の event（`turn_finished` の出力、`planner_unresponsive`、`planner_closed` の `code`、`draft_planner_settled` の `undecided`）と planner のディレクトリの `turns/` を読み、経路に固有の原因（turn の失敗・wrapper の喪失・依頼の配送の遅れ・画面で聞けないための ask）か、経路に依らない原因（plan review の provider、Claude の利用上限、host の load、planner の上限）かを分ける
2. 経路に固有の原因が 1 つの不具合か prompt に帰せるなら、戻さずに直す task にする
3. 経路に固有の原因が複数あるか直し方が見えないときに、人に「対話に戻す」を提案する。戻すのは `dagq.toml` の 1 行で、ADR-t1394-2 の決定の範囲の中（既定は変わらない）

目安に当たらず min_samples を満たしたら、非対話を続け、既定を変えるかを決める ADR（ADR-t1394-2 決定 1）の材料にする。

## 4. 重なる変更（confounders）

- **plan review の provider**: plan review は 2026-10-02T06:13Z（最初の `launch.provider: codex` の `plan_review_started`）から Codex（`gpt-6-sol`・`gpt-6.1-sol`）で動いている（窓の `plan_review_started` 614 回のうち 46 回、`plan_review_finished` 565 回のうち model が `gpt-6*` のもの 35 回）。基準の窓と印の前 7 日の大半は Claude の plan review で、後の窓は Codex になる。`plan.revise_rate` と revise の件数は reviewer が変わると動くので、plan review が Codex の期間どうしで比べる: 基準を `2026-10-02T06:13Z..TM`（対話の planner × Codex の plan review）に縮めて読み、固定の基準の窓の値は参考にする。縮めた基準が min_samples に届かなければ、`plan.revise_rate` は判断に使わず件数だけを並べる。`kpi` の `job.verdict.plan_review.*` を `--by provider` で並べ、plan review の revise の率そのものが変わっていないかを先に見る
- **人の planner の廃止と inbox からの依頼（ADR-t1394-1）**: 同じ goal の実装が後の窓に入ると、人の planner が無くなり、revise の宛先は全て runtime の planner になる（基準では 8 件が人の planner 宛て）。依頼の planner は新しい種類として分けて数え、draft・revise・finding の値に混ぜない。`plan.revise_rate` の `origin=person` は比べない
- **background の wrapper（ADR-t1404-1）**: 非対話の planner の wrapper の置き場所は worker と同じ設定で選ぶ（決定 8）。planner の非対話化と background への切り替えが同じ窓に入ると、止まり方の原因がどちらか分からなくなる。印が別なら、planner の turn の失敗を wrapper の置き場所ごとにも分ける（[headless-background-evaluation](headless-background-evaluation.md)）
- **worker の既定の非対話化（印 61916、2026-10-02T10:15Z 記録）**: worker の receipt の follow_up の数と中身が変わると、draft の数と決着の内訳が動く。draft の数は経路の評価に使わず、1 planner あたりの値で比べる
- **窓の中の人の印**: 基準の窓に 7 件（34143 worker stress 20→5 周、45923 goal review を Codex に、49333 Claude 無効・臨時 Codex inbox、51928・51978 Codex の既定の model、60152 run review を Codex に、61916 Claude worker の既定を非対話に）。後の窓の印は `kpi --compare M` の `confounders` の `position` で読む
- **Claude の無効の期間（2026-09-30）**: その日は draft の planner が 21 と少ない。`--no-claude` では runtime の planner が立たない（ADR-t1394-2 決定 5）ので、後の窓に同じことがあれば、その期間の draft の決着までの時間は控えの待ちを含む。控え（`queue_hold`）の区間を除いて読む
- **planner の上限**: `[supervisor] runtime_planners = 2`。変われば `no_taker` の `planner_unresponsive` と draft の決着までの時間（枠の空き待ち）が動く。上限を変えたら印を打つ
- **記録の始まり**: 基準値の区間・`draft_planner_settled`・`planner_closed` は窓の途中から（「読んだ範囲と方法」）。印の前 7 日をもう一度読めば、全ての値が窓の全体にある対話の基準になる
- **自動更新**: 着地ごとに supervisor が入れ替わる。印の前後に引き継ぎが続くと `kpi` は印を重なった変更にまとめるので、時刻で窓を明示する

## 続け方

- 切り替えは task 1402 が `dagq.toml` の `[roles.runtime_planner]` に `route = "headless"` を書いて行う。worker は印を打てないので、着地して main checkout に反映された後に planner か inbox が固定バイナリで `dagq mark 'runtime planner headless' --at <効いた時刻>` を打つ。打ったら、その印の ID と時刻をこの文書に足す
- 印から 7 日以上（min_samples を満たすまで）経ったら、2 のコマンドで後の窓と印の前 7 日を読み、1 の表に列を足し、3 の目安に当たったかを表にする（goal 87 の受け入れ条件 (4) の 1 週間後の評価）
