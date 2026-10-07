---
id: plan-review-agent-eval-spike
type: plan
title: スパイク：reviewのagentをevalで測って改善する（外のrepository dagq-agent-evalのSpike〜MVPの結果・決めたこと・所見の行き先）
status: completed
created: 2026-10-06
owners:
  - hisamekms
tags:
  - planning
  - review
  - eval
related:
  - adr-t1728-1
  - adr-t1728-2
  - adr-t1453-1
  - adr-t1895-1
  - adr-t1895-2
  - plan-review-subagents-spike
  - design-agent-eval
---

# スパイク：reviewのagentをevalで測って改善する

goal 125の最初のtask（task 1728）が、dagqの管理外のrepository dagq-agent-eval（`~/ghq/github.com/hisamekms/dagq-agent-eval`）で人が開いたsessionが行ったSpike〜MVPの成果を取り込んだ記録。材料はそのrepositoryのcommit `cfb8a4c`の`REPORT.md`・`PLAN.md`・`notes/`で、対象のdagqは基準のcommit `f46a7963`、Claude Code 2.1.289（model claude-opus-5-5）、codex-cli 0.160.0。runtime・test・設定はそのrepositoryを参照しない。決めたことのうち変えるのに人の判断が要るものは[ADR-t1728-1](../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)（定義とケースの置き場・eval）と[ADR-t1728-2](../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)（道具の宣言）、欄・コマンド・数値の予定は[agent eval](../design/agent-eval.md)が持つ。subagentを親のreview jobの中で動かせるかを確かめた前のスパイクは[review-subagents-spike](review-subagents-spike.md)。

版ごとの成績の記録（Spikeの`results/<agent>/*.json`）はこのrepositoryに写さない（版ごとの成績はqueue側のevalのeventに残す。ADR-t1728-1決定4）。下の表の値は、commit `cfb8a4c`の`results/adr-rules/README.md`と`results/migration-rules/README.md`の表から取った。

## 1. 結果

| agent | 採用した定義 | dev（判定 recall / precision・規則コード recall / precision） | 採用の後の確認 | 閾値 |
|---|---|---|---|---|
| adr-rules | iter6（1,367文字。本番は419） | 0.98 / 0.98・0.92 / 0.94（53件 × k=3） | 生成したhold-out: 1.00 / 1.00・0.93 / 0.89。汚れていないhold-outはまだ無い | devで超えた |
| migration-rules | iter1（704文字。本番は384） | 1.00 / 1.00・1.00 / 1.00（11件 × k=3） | hold-out: 判定のrecall 0.67 / precision 1.00（差は1件。過学習として記録）。production: 誤検出 0 / 14 | devで超えた。hold-outのrecallは未満 |

- 判定は`revise`・`concern`を違反あり、`pass`を違反なしとみて、実行（ケース × 1回）を単位に数えた。規則コードはagentの結果の`reasons[].codes`から取った。
- 版ごとの経過（adr-rulesのiter0〜6、migration-rulesのiter0〜1）は、commit `cfb8a4c`の`notes/iterations-adr-rules.md`・`notes/iterations-migration-rules.md`と上の`results/`の表にある。たとえばadr-rulesの本番の定義（iter0）はdevで判定のprecision 0.73・規則コードのprecision 0.27、短く詰めたiter3はdevと生成したhold-outで1.00だったがproductionで判定のrecall 0.33だった。

本番の定義から変えたこと（両agentに共通の形）:

1. コードごとに規則の1行の要約を書く（束ねたコードではagentがどのコードがどの規則かを知らず、規則コードのprecisionはadr-rules 0.27、migration-rules 0.25だった）。
2. 要約には規則の本文の判断の基準を残す（短く詰めたiter3は本番の差分で違反の3分の2を見落とした）。
3. この規則だけで判定し、規則が許す変更は違反にしない。ほかの規則・文書の内容の正しさは全体のreviewに任せる。
4. reasonsには破った規則だけを書き、コードはその変更が直接破る規則だけに付ける。
5. （adr-rules）コードやdesignで既存のADRの決定を黙って変えるのも、決定の変更として見る。

## 2. 決めたこと

- **置き場**: Spikeは`agents/<agent>/AGENT.md`（dagqの`.dagq/review-agents/<agent>.md`と同じ形）と`agents/<agent>/evals/`（`evals.json`＝dev・`holdout.json`・`production.json`・`files/<case>/case.patch`）。dagqでは`.dagq/agents/<name>/`にし、patchはagentをまたいで共有する（ADR-t1728-1決定1・4）。
- **ケースの欄**: skill-creatorの`evals/evals.json`に、期待する判定・規則コード・許容するコード（`acceptable_codes`）・出どころ・split・ケースごとのbaseのcommit・人の決定（`adjudicated`）・争いの印（`disputed`）を足した。dagqではsplitをファイルで区分し、共通の欄と役割ごとのinput・expectedに分ける（ADR-t1728-1決定3、欄は[agent eval](../design/agent-eval.md)）。
- **実行器**: Spikeは本番の`review_command`と`review_subagents`のargvをそのまま組むPythonの実行器だった。dagqでは本番のrunのreviewのagentのjob（ADR-t1895-1の独立のjob）と同じ経路をsupervisorが使い、eval専用の組み立ては作らない（ADR-t1728-1決定5）。
- **ケースの出どころ**: generated（Codexが規則の本文だけを見て作り、別のCodexの呼び出しが検証）、handmade（改善するsessionが作る。devだけ）、production（本番のreviewの差分。ラベルはCodexが規則の本文だけで付け、本番の判定と食い違えば人の決定か2回目の判定との多数決）。
- **hold-outを改善する側から隔てる**: 別のファイルに置き、agentごとに1回だけ流す。改善するsessionが中身を読んだケースはhold-outから外した（productionのadr-rulesの29件はdevに移した）。dagqではキーを（agent・定義・ケースの集合）にする（ADR-t1728-1決定7）。
- **ラベルの争い**: 改善するsessionは失敗を見た後にラベルを変えず、`disputed`にして主の指標から外し、人に決めてもらう。人にも決められなかった3件は「あいまい」として外した。
- **定義へのケース固有の語の漏れ**: 毎回機械で調べ、どの版も0件だった。
- **閾値**: 判定と規則コードのrecall・precisionがどちらも0.9。規則コードごとの値はケースが少なく1回の外れで割るので、採用の判断にはagentごとの値を使った。

## 3. 過学習の対策の効き目

| 対策 | 効き目 |
|---|---|
| 本番の履歴から作ったケース | 最もよく効いた。adr-rulesのiter3はdevと生成したhold-outで1.00だったが、productionで判定のrecall 0.33（本番の定義は0.80）。生成したケースは本番でよく起きる破り方（本文を変えて`updated`を動かさない、ADRに実装の名前を書く、コードでADRの決定を変える）を持たなかった |
| 生成したhold-out | 生成したdevと同じ分布なので上の過学習を捕まえなかった。migration-rulesではdevとの差0.33（1件）を出した |
| 作る側と改善する側を分ける（Codexが作りClaudeが改善） | 差として測れるほどのケースが無かった |
| 定義へのケース固有の語の漏れの検査 | どの版も0件 |
| 短い定義を優先する | 逆効果になりうる。本番の分布を覆わないケースでは、判断の基準を落としても差が出ない。dagqでは定義の長さを懸念にしない（ADR-t1728-1決定12） |
| hold-outは採用の後に1回 | 守った。ただしadr-rulesはproductionのケースを見た後にdevに移したので、汚れていないhold-outが無い |

REPORT.mdの本文はadr-rulesのiter3のproductionの判定のrecallを0.31、本番の定義を0.77と書くが、`results/adr-rules/README.md`の表の値（0.33・0.80）をここでは使った。

## 4. 費用（実測）

- 合計: Claude $190.20（`total_cost_usd`の和）。Codex（ケースの生成・検証・ラベル）はtokenだけを返し、金額は出ない。
- 1回（ケース × 1）: Claudeで$0.15〜0.24、約30〜40秒（最初の1周の平均は$0.20・36秒）。本番の大きな差分（数十〜数百KB）は$0.30〜0.45、約40〜60秒。Codexで約80秒、入力約100k token。
- 1周: adr-rulesのdev（53件、101回）が$20〜24、10〜20分（並列4）。生成したケースだけの24件 × k=3（72回）が$11〜16。人はrequest 35で、着地の前のdevの1周の費用と時間を許容した。
- 長くした定義の1回あたりの費用は約20%下がった（ADR-t1728-1決定12）。

## 5. 所見の行き先

| 所見（REPORT 5.） | 行き先 |
|---|---|
| 5.1 本番のadr-rulesは担当外の指摘（違反20件のうち7件はdesignの文書の古さ）で差し戻し、本番のmigration-rulesの違反4件は多数決で4件とも誤検出 | 定義の取り込み（goal 125の後続のtask）で、判定の範囲を書いた採用した定義に替えて直す |
| 5.2 定義がコードを束ねると規則コードはほぼ当てにならない（precision 0.25〜0.27） | 定義の取り込みで、コードごとの要約を書いた定義に替えて直す |
| 5.3 adr-rulesの定義が削除済みの規則コードA-151を持つ | 定義の取り込みで直す（採用した定義はA-151を外した） |
| 5.4 A-144は今のrepositoryでは破れない（リリースのtagが`v0.2.0`だけで、検査は`v0.3.0`より新しいtagと比べる） | 記録だけ。違反のケースはリリース済みのmigrationができてから |
| 5.5 「既存のADRの決定をコードで変えたか」は人にも判断がつかない場合がある（3件）。`revise`でなく`concern`で出すほうがverdictの定義に合う | 採用の判定の仕組み（ADR-t1728-1決定10）ができた後の、定義の改善の候補として記録 |
| 5.6 Codexのsub-agentは技術的に動くが、`exec --json`のeventからsub-agentの実行が見えず、`spawn_agent`は既定で親の履歴を全部forkする | 履歴として記録だけ。[ADR-t1895-1](../adr/2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)でrunのreviewのagentを親のjobの中のsubagentからagentごとに1本の独立のheadless jobにした（task 1903が起動し、task 1904が親のjobの中のsubagentの仕組み（`runs_review_subagents`・`review_subagents`・`--agents`・`subagents_unsupported`）を消す）ため、親のjobの中のforkそのものが無くなり、各agentのjobが自分の開始・終わりのeventを持つ。これを確かめて有効化する予定だったtask 1476は1903の重複としてcanceled（event 109604・109605） |
| 5.7 親がsubagentに渡す委任の文がrun dirのどこにも残らない | 記録だけ（独立のagentのjobでは親からの委任が無くなる） |
| 5.8 runのcommitの一部がどのrefからも辿れない | 記録だけ。evalのケースがcommitを指さずpatchを持つ理由の1つ（ADR-t1728-1のAlternatives） |

## 6. 残っていること

goal 125の受け入れ条件と後続のtaskが持つ。REPORT.mdの「7. 残っていること」のうち、adr-rulesの汚れていないhold-out（2026-10-05以降の本番のreview）でiter6を1回流すことと、migration-rulesのhold-outで見落とした1件の原因の確認（新しいケースを足してから）はgoal 125が、コンパイル方式との比較はgoal 125の範囲外が持つ。
