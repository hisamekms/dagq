#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Render the dated measurement from derived CSVs (no queue access)."""
import csv
import json
from pathlib import Path

HERE=Path(__file__).resolve().parent
OUT=HERE/'out'

def read(name):
    with (OUT/(name+'.csv')).open() as stream:
        return list(csv.DictReader(stream))

def value(x):
    if x in ('','None',None):
        return '—'
    try:
        return ('%.3f'%float(x)).rstrip('0').rstrip('.')
    except (ValueError,TypeError):
        return str(x)

def table(headers,rows):
    return '\n'.join(['| '+' | '.join(headers)+' |','| '+' | '.join(['---']*len(headers))+' |']+['| '+' | '.join(str(x).replace('|','/') for x in r)+' |' for r in rows])+'\n'

def timing(r,prefix):
    return ' / '.join(value(r[prefix+suffix]) for suffix in ('_n','_median','_sum'))

def main():
    rows=read('runs'); totals=read('table'); changes=read('definition-changes'); anomalies=read('anomalies')
    meta=json.loads((OUT/'meta.json').read_text())
    labels={r['tree']:'D'+str(i) for i,r in enumerate(changes)}
    text='''---
id: plan-docs-candidate-search
type: plan
title: 変えた名前で文書の候補を探す指示の前後測定
status: completed
created: 2026-10-05
related:
  - plan-acceptance-check
  - adr-t1688-1
  - design-measurement
---

# 変えた名前で文書の候補を探す指示の前後測定

初回 review の docs_drift の差し戻しは前後とも **14/40（35%）**。主理由だけなら **10/40（25%）→7/40（17.5%）** だが、文書指摘全体の減少は示していない。worker の作業時間の中央値は **889.5→1267.5 秒（+378 秒）**。探索だけの追加時間は記録されず、仕事の内容・負荷・並行した prompt と review の変更が混ざるので、この差を探索の費用や因果効果と断定できない。

## 区間と境界

締切 C = **2026-10-05T09:05:36.831Z**（この測定 run f387f7b2-4cf0-444d-98a4-27a5577db53c の run_claimed、event 103389、build b4f75bac20660c042b72799001e373ce980602d4）。全 event は created_at < C。選択は stats の着地時刻ではなく、各 run の最初の run_claimed の build と attempt=1 の review_finished の存在で決める。claim の古い順（同時刻は event ID 順）に並べ、前は直近最大40件、後は最初の最大40件を採る。最初の review を C までに終えていない run は含めない。区切りを時刻だけで推定せず、acceptance-check の fetch.holds / version_commit と compute.find_t（--t-task の仕組み）を再利用し、claim に記録された build に着地 commit が含まれるか git merge-base --is-ancestor で run ごとにも検査した。

前の条件は task 1429 と subagent 導入 9384cb25 の両方を含み、1688 を含まない build。後は1688を含む build。境界の着地と最初の該当 build の記録は以下（UTC）。完全な SHA と event ID は [meta.json](docs-candidate-search/out/meta.json)、各 claim の build は [runs.csv](docs-candidate-search/out/runs.csv)。

'''
    text+=table(['境界 task','着地 commit','着地 event / 時刻','最初の含む build / 起動 event / 時刻'],[
        [p['task'],p['landing']['commit'],str(p['landing']['id'])+' / '+p['landing']['created_at'],p['first_binary']['version']+' / '+str(p['first_binary']['id'])+' / '+p['first_binary']['created_at']] for p in meta['t']['parts']])
    text+='\n'
    text+=table(['区間','件数','選んだ claim の最初〜最後','attempt 1 review_finished の最初〜最後'],[
        [group,len(rs),rs[0]['claimed_at']+'〜'+rs[-1]['claimed_at'],min(r['review_at'] for r in rs)+'〜'+max(r['review_at'] for r in rs)]
        for group in ('before','after') for rs in [[r for r in rows if r['interval']==group]]])
    text+='''
後は40件で20件の条件を満たす。標本不足を理由とする再測の時期と measurement follow_up は不要。これは固定された C での測定であり、以後の run は結果に足さない。

## 指標の取得元と式

- **初回 docs_drift 率**: dagq events --full の attempt=1 の review_finished。分母 N は区間の run 数（1 run 1 event）。payload.route があれば destination=send_back のみを差し戻しとする（verdict=concern も含む）。route が無ければ verdict=revise、または concern で同じ run の次の review_started より前かつ C より前に revise_requested があるものを数える。ask、route 無し concern で revise_requested が無いものは「人の判断へ」。送れなかった差し戻し・取り消されたものも率には含む。(a) 分子は差し戻しで reason_codes の配列の配列のどこかに docs_drift、(b) 分子は同じ差し戻し判定で primary_code=docs_drift。全選択で (b)⊆(a) を assert した。参考は差し戻しを問わず reason_codes に docs_drift の attempt 1 の数。task 251/event 78650 と 655/event 78994 のような concern→send_back を verdict だけで落とさない。
- **照合の記録なし**: compute.mapping() と共有する validation_receipt_before() で取った最後の pre-review validation_finished の payload.receipt.summary。receipt の dict を持つ validation_finished を ID 順で探す。summary が無い・空、またはこの validation が無いものは欠測として分母の外へ出す。分母 V は summary が取得できた run。分子は summary に文書 path または不要理由の句が無い run。率=分子/V。receipt_observed と integration_receipt は使わない。取得用 CLI は dagq events --full --all --task ID --kind validation_finished --until C（実際の fetch は --task ID --all で全 kind を取得し、その中を抽出）。
- **summary の判定規則**: compute.py の PATH は docs/...、AGENTS.md、CLAUDE.md、plugins/...、単体の *.md を認める。UNNEEDED は英語の docs/documents と no change/update、not needed/required、unchanged 等、日本語の文書と不要/変更なし/更新なし、照合/更新と不要を認める。path の有無を数える規則で、照合の正しさは判定しない。受け入れ条件の根拠や作成した ADR の path だけでも陽性になるため、記録の充足を過大に見積もりうる。節名だけ（「役割の表を見た」）、代名詞だけ、.rst など docs/ の外の別の拡張子、別の語による不要理由は見落としうる。採用された最初の手がかりは runs.csv の check_evidence。後の「探した名前」は同じ summary の SEARCH（names searched、searched names/terms/for、search terms、探した名前、検索した名前/語、検索語の後に非空文字）で数える。検索の実行や語と差分の対応を保証せず、「Docs search: grepped ...」だけ等の違う言い方は見落とす。
- **最初の1往復（秒）**: (a) の集合（(b) の部分集合も別列）。attempt 1 review_finished 後、次の review_started 前の最初の取り消されていない revise_requested を始まりとする。revise=次の review_started.created_at−revise_requested.created_at。再 review=その started と同 attempt の最初の review_finished.payload.duration_secs。和は両側が取得できた run のみ。revise_unsent は同 attempt の依頼を取り消し、後に再送された revise_requested があればそれから測る。中央値は数値のソートの中央（偶数は中央2値の平均）、合計は取得値の和。
- **時間の除外**: 取り消しだけなら両側無し、未送信（次の started 前に request 無し）も両側無し。revise 未完了（request の後に started 無し）は両側無し。再 review 未完了/失敗（started の後に同 attempt の finish 無し、または review_failed）は revise のみ。duration_secs が無い/非数・非有限・bool、時刻が読めない場合は取得不能側のみ除く。C 後を読まない。2往復目以降は、再 review が差し戻された後の同じ計算（理由 code 不問）を追加往復として別に出す。途中に上の除外がある run は追加往復数・合計から外す。最初の取り消し後の再送が完了した run は最初の時間を出すが、追加往復の参考値からは外す。
- **worker 作業時間（秒）**: dagq stats --full --since 2026-10-01T00:00:00Z --until C の runs[].work（[stats](../design/supervisor-lifecycle/stats.md) の run_claimed→最初の receipt_observed）。land_phases は使わない。待ちを引いた work_excl_wait は acceptance-check compute.run_rows / overlap を再利用し、work の区間と run_waiting_started→次の run_waiting_ended の組の重なりだけを引く（元の計算どおりミリ秒から整数秒へ切り捨て）。receipt より前に開いた未終了の待ちは unfinished とし、中央値から除く。claim/receipt が無ければ両側欠測、stats.work の未取得も除く。run_waiting_deferred が work の区間内にある run は別に数える（待ちに入らなかった ask は引かれない）。今回 work は各 run の event の区間と一致。閉じた待ちの重なりを引いた run は前の task 1583（4643→1978 秒、2665 秒を除外）と後の task 1660（10760→1699 秒、9061 秒を除外）の2件。全体の中央値はどちらの区間も work と excl が同じだが、個々の値は異なる。
- **層**: change は fetch.task_at_claim（今の show --full の change/acceptance から claim 以後の task_edited を巻き戻す）。area は compute.areas_of の着地 commit の file と締切前の main の [areas]。複数 area の run は各行に重複して入り、area 行は足さない。未着地は unknown。重さは compute.terciles / bins の着地 commit の added+deleted の三分位で、前の選択40件から閾値を決め後にも固定して適用する。未着地/未取得は重さの別行で、三分位には入れない。これは完成後の変更量であって着手時の難しさの推定ではない。

## 件数・率・worker の時間（層ごと）

a/N と b/N は同じ分母・同じ差し戻し判定。参考は軽い指摘を含む。照合なし/V、探した名前/V は同じ初回 receipt。work と excl は取得件数/中央値（秒）。欠測列は summary / work / unfinished wait / waiting_deferred の件数。空の時間は — で、0秒とは区別する。各率の数値と時間の取得件数も [table.csv](docs-candidate-search/out/table.csv) にある。

'''
    text+='重さの閾値: '+str(meta['line_terciles'])+' 行。\n\n'
    text+=table(['区間','層','値','N','a/N','b/N','参考','照合なし/V','探した名前/V','人へ','route 無し','欠測等','work n/中央値','excl n/中央値'],[
        [r['interval'],r['layer'],labels.get(r['value'],r['value']),r['n'],r['drift_any']+'/'+r['n'],r['drift_primary']+'/'+r['n'],r['drift_reference'],r['check_absent']+'/'+r['summary_n'],r['searched']+'/'+r['summary_n'],r['human'],r['route_absent'],'/'.join(r[k] for k in ('summary_missing','work_missing','wait_unfinished','waiting_deferred')),r['work_n']+'/'+value(r['work_median']),r['work_excl_wait_n']+'/'+value(r['work_excl_wait_median'])] for r in totals])
    text+='''
## 最初の1往復の時間（層ごと）

各セルは **取得件数 / 中央値 / 合計**（秒）。a は reason_codes の docs_drift の差し戻し全体、b は primary_code の部分集合。合計の取得件数は revise や再 review 単独の件数と異なり、両側が取れた run だけ。

'''
    text+=table(['区間','層','値','a revise','b revise','a 再 review','b 再 review','a 和','b 和'],[
        [r['interval'],r['layer'],labels.get(r['value'],r['value']),timing(r,'revise'),timing(r,'primary_revise'),timing(r,'rereview'),timing(r,'primary_rereview'),timing(r,'total'),timing(r,'primary_total')] for r in totals])
    text+='''
## 打ち切り・欠測・追加の往復

初回1往復の (i) 取り消し（取り消した依頼の件数）、(ii) 未送信、(iii) revise 未完了、(iv) 再 review 未完了/失敗、(v) revise 時刻/再 review duration 欠測を別に示す。率の分子は変えない。0件の ID 一覧は空（—）。追加往復の除外は first の異常も含む。詳細は [anomalies.csv](docs-candidate-search/out/anomalies.csv)。

'''
    kinds=['cancelled','not_sent','revise_unfinished','rereview_unfinished_or_failed','revise_missing_time','rereview_missing_duration','human','route_absent','summary_missing','work_missing','wait_unfinished','waiting_deferred']
    text+=table(['区間','場合（first または run）','件数','run ID'],[
        [g,k,len(rs),'; '.join(r['run_id'] for r in rs) or '—']
        for g in ('before','after') for k in kinds for rs in [[r for r in anomalies if r['interval']==g and r['kind']==k and r['stage'] in ('first','run')]]])
    text+='\n再 review 未完了/失敗の4件は、次の started の同 attempt が C までに review_finished/review_failed を持たないもの。revise は残し、再 review と和は除いた。\n\n'
    text+=table(['区間','追加往復から除く場合','件数','run ID'],[
        [g,k,len(rs),'; '.join(r['run_id'] for r in rs) or '—'] for g in ('before','after') for k in ['excluded']+kinds[:6] for rs in [[r for r in anomalies if r['interval']==g and r['kind']==k and r['stage']=='additional']]])
    text+='\n対象 run ごとの最初の時間と追加往復（追加0回も表示）。b=1は主理由の部分集合。\n\n'
    text+=table(['区間','task','run ID','b','revise','再 review','和','追加往復数','追加合計秒'],[
        [r['interval'],r['task_id'],r['run_id'],r['drift_primary']]+[value(r[k]) for k in ('revise','rereview','total','extra_cycles','extra_seconds')] for r in rows if r['drift_any']=='1'])
    text+='''
## 検出条件と並行した変更

REVIEW_DOCS_CHECK の定数は、対象を含む build 70個で同じ SHA256（3247d97974206c0d9c189b9e050aedddf478b9e6ca919f3d99fe09da145b64da、const 宣言全体）だった。一方、.dagq/review-agents/ は以下の変更があるので同一条件と呼べない。review_started.payload.subagents.commit が示す、実際に定義を読んだ repository の commit から全定義 directory の tree を取って D0〜D4 に分け、上の detection 行に全指標を並べた。実際に path に選ばれた agent 名と digest は [detection.csv](docs-candidate-search/out/detection.csv)。選択対象の差分によって agent の組は違う。定義が同じでも、対象の仕事内容や review の provider が同じという意味ではない。

'''
    text+=table(['条件','定義を変えた commit','着地 UTC','tree','内容'],[[labels[r['tree']],r['commit'],r['landing_at'],r['tree'],r['title']] for r in changes])
    text+='''
前の40件も D0=26、D1=5、D2=7、D3=2 に分かれる。1688を含まない build で claim された最後の2件は review の時点では1688の定義 D3 を読んでいた（境界を claim と review の両方で記録する理由）。後は D3=9、D4=31。task 1688 は worker の探索と design-consistency の確かめ方の強化を同時に着地した。後の値はその和であり、探索だけを受けた対照群が無いので分けられない。D3 の前後2対9件だけでも対象と標本が小さく、worker の探索の効果を断定できない。

以下は最も早い選択 claim から C までの git log --first-parent HEAD で、src/application/prompt.rs、src/application/review.rs、src/application/supervise/jobs.rs、src/domain/review_subagents.rs、src/domain/review_reason.rs、.dagq/review-agents/、dagq.toml に触る着地を列挙したもの。これは広く候補を拾う一覧で、復旧・planner の変更だけの行も含む。元の commit/時刻/event は [concurrent-changes.csv](docs-candidate-search/out/concurrent-changes.csv)。

'''
    text+=table(['commit','着地 UTC','task','変更'],[[r['commit'][:8],r['landing_at'],r['task_id'],r['title']] for r in read('concurrent-changes')])
    text+='''
特に cd4d7def の background worker への切替、dc93fcfa の review 資料の上限、d751a0c9 の対話経路の廃止、00493c30/64ef873d/cfe2fd07 の subagent 項目の変更は費用・検出の比較に混ざる。e70840a9 は判断の unit test への移動で、定義の変更とは分けて扱った。後の選択の最後の review は01:36 UTCだが、後続の往復は C まで見るため、それ以後の着地も候補一覧に残した。

過去の [review-sendback-reasons](review-sendback-reasons.md) の docs_out_of_sync は手で分類した主8/付いた9件で、現在の docs_drift と検出条件・分母・期間が違う。今回の比較に合算せず、見落としの型の以前の参考値としてのみ引く。0/40の照合記録なしは、名前で候補を探せた証拠ではない（summary の path の有無という弱い判定）。探索句は1/40→20/40だが、違う書き方の見落としもある。work 中央値の+378秒は、実際の探索に費やした秒の上限を直接証明せず、探索以外も入る費用の保守的な見立てである。理由全体の35%が同じ、主理由だけが下がったことから、この標本では改善の根拠を得なかった。

## 選んだ run の一覧

全80 run の ID、claim の build/時刻、review_finished/validation_finished の event ID は runs.csv にあり、以下にも ID を載せる。events の選択一覧は [selected-events.csv](docs-candidate-search/out/selected-events.csv)（ID・kind・時刻・attemptのみ、summaryや理由の原文を公開しない）。

'''
    text+=table(['区間','task','run ID','claim UTC','build'],[[r['interval'],r['task_id'],r['run_id'],r['claimed_at'],r['build']] for r in rows])
    text+='''
## 再現のコマンドと取得件数

Python 標準ライブラリのみ。固定バイナリを PATH に置き repository の worktree で実行する。queue は queue service 経由の読み取り CLI のみで、DB、locate、--db、接続先を変える env は使わない。acceptance-check/fetch.py は変更していない。compute.py は初回 review 前の receipt 取得を validation_receipt_before() として抽出し、mapping() とこの測定が共有する。既存の出力は変更していない。新しい fetch.py は既存 fetch.main() にページ記録の wrapper を付けるだけ。この測定の compute.py は既存の関数を import し、不足する差し戻し/summary/往復の指標と選択条件だけを追加した。history.py は git のみ、render.py は出力 CSV のみを読む。#1444 の文書と script は変更していない。

```sh
C=2026-10-05T09:05:36.831Z
SNAP="$TMPDIR/candidate-snapshot"
PYTHONDONTWRITEBYTECODE=1 python3 docs/plans/docs-candidate-search/fetch.py \\
  --since 2026-10-03T04:12:16.449Z --until "$C" --cutoff "$C" \\
  --stats-since 2026-10-01T00:00:00Z \\
  --watch-task 1429 --watch-task 1460 --watch-task 1688 --out "$SNAP"
PYTHONDONTWRITEBYTECODE=1 python3 docs/plans/docs-candidate-search/compute.py "$SNAP" --cutoff "$C"
PYTHONDONTWRITEBYTECODE=1 python3 docs/plans/docs-candidate-search/history.py "$SNAP"
python3 docs/plans/docs-candidate-search/render.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s docs/plans/docs-candidate-search -p 'test_*.py'
```

既存 fetch は events --full --limit 1000 --after 0 から、非空ページの最大 ID を次の --after に渡し、空ページまで全件を読む。初期の review_finished/review_failed を探し、候補 run とその task の全 kind、境界 task、supervisor/update の event を取得し、ID で重複を除く。task_at_claim の task_edited は C 後も読むが、claim 時点への巻き戻しだけに使う。取得時点の reference は stats・show（層の復元）・marks・status を含む。大きな原文 snapshot は TMPDIR にのみ保存し commit しない（acceptance-check の公開制約に従う）。再取得時の show は後の編集を巻き戻す。C 時点で進行中だった run の stats/work が後から利用可能になった場合は結果の欠測が変わりうるが、今回80件は全て work が取得できた。

'''
    text+=f"events の CLI 呼出し（空ページを含む）は **{meta['pages']}ページ**、非空 **{meta['nonempty_pages']}ページ**、返却 **{meta['paged_events_with_duplicates']}件**（重複込み）、snapshot の ID 重複除去後 **{meta['unique_events']}件**。取得記録は [pages.json](docs-candidate-search/out/pages.json)。候補は177 run、177 task、162着地 commit。前後選択は各40件。\n\n"
    text+='''検証は13本の unit fixture（concern/route優先、legacy concernの次のstartedでの区切り、secondary docs_drift、unsent/再送/別attempt、未送信/未完了/失敗、durationと時刻の片側欠測、待ちの重なり/開いた待ち、summary欠測の分母、初回 receipt と後の最終 receipt の区別）と、同じ snapshot での再集計の全出力一致、cargo fmt --all --check と cargo clippy --locked --all-targets -- -D warnings。Rust の変更は無く、Rust unit test・stress・e2e は追加実行していない。e2e は必要なら runtime が review 通過後に host で実行する。
'''
    (HERE.parent/'docs-candidate-search.md').write_text(text)

if __name__=='__main__':
    main()
