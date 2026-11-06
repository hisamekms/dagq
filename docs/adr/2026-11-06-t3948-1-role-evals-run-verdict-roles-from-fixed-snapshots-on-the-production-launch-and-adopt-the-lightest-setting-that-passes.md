---
id: adr-t3948-1
type: adr
title: 判定を返す役割（plan review・goal review・runのreviewの全体の判定）のevalは、起動の時に固定したsnapshotを入力に本番と同じ起動で1ケース1本のjobを流し、本番の判定の形で採点して、閾値を満たす中で回あたりの費用が一番軽い組を採用する（ADR-t1728-1決定3・5をamends）
status: accepted
created: 2026-11-06
updated: 2026-11-06
accepted_on: 2026-11-06
amends:
  - adr-t1728-1 decision 3
  - adr-t1728-1 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - review
  - eval
related:
  - adr-t1728-1
  - adr-t2229-1
  - adr-t1091-1
  - design-agent-eval
  - plan-review-agent-settings-spike
  - plan-role-eval-snapshot-spike
---

# ADR-t3948-1: 判定を返す役割のevalの形と設定の採用（ADR-t1728-1決定3・5をamends）

## Context

使用量の上限でjobが止まるか切り替わる日があり、軽くてよい役割を軽くすれば上限に当たりにくい。
ところが役割の設定には測った根拠が無く、[ADR-t1728-1](2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)は今作るharnessをreviewのagentだけとし（決定3）、evalの起動経路をreviewのagentのjobとしている（決定5）。

材料は2つのSpikeである。
[role-eval-snapshotのSpike](../plans/role-eval-snapshot-spike.md)は、固定したsnapshotで役割のjobを起動できるかを調べ、3つの役割はCodexなら既存の部品の拡張で作れてClaudeの読み取りの境界だけが残り、復旧のjobは`does_not_hold`とした。
[review-agent-settingsのSpike](../plans/review-agent-settings-spike.md)は設定の候補の比べ方を固めたが判定は`unresolved`で、Claudeの14周は送り先の検査で拒まれ、supervisorの引き継ぎで捨てられた回が7周のうち4周に出て「比べてよい」条件を満たせなかった。

## Decision

具体の閾値の値・判定の並び・式は役割の設定の測定の文書が、設定のkey・欄・pathの綴りは実装のdoc commentが持ち、ここには方針と境界だけを書く。

1. **対象と判定の形。**
   対象は判定を返す3つの役割（plan review・goal review・runのreviewの全体の判定）とする。
   ケース・期待・採点の判定の値は本番の形のまま使い、本番のpromptと判定のschemaを変えず、独自の分類を作らない。
   期待は判定と判定の理由の分類で、理由の分類は本番の判定が持つものを使う（plan reviewとrunのreviewは理由のコード、理由のコードを持たないgoal reviewは未達とした受け入れ条件の項目）。
   採点は判定ごとのrecall・precisionとerrorの数で、reviewのagentの規則コードの採点を理由の分類に当てる。

2. **役割ごとのharness（決定3をamends）。**
   入力は判定の材料の固定したsnapshotで、harnessは役割ごとに持つ。
   - (2a) **snapshot。** 本番の役割のjobの起動の時に固定したもので、組み立ての入力、promptがpathで名指す資料の本文（runのreviewの試みごとの資料と規則の全文、plan reviewの置き換えが参照する保存した作業）、その時のmainのcommitからなる。
     runのreviewの全体はこれに加えて、変更後のtreeを後で作り直せる記録（範囲のbaseとhead、jobが読むtreeの識別、baseからheadへの変更の固定した写し）を持つ。
     pathだけの記録は後で上書き・削除されるので、当時の資料の証明にならない。
     ケースはこの記録が始まった後のjobから作る。
     過去のjobの材料は残っていないので、過去のeventから当時の入力を推測してsnapshotとして扱わない。
   - (2b) **参照するrepositoryの版。** 本番のjobが読むtreeに合わせて役割で分ける。
     plan reviewとgoal reviewが本番で読むのはmain checkoutなので、起動の時のmainのcommitのtreeを置く。
     runのreviewの全体が本番で読むのはrunの変更後のtree（hostのrunはworktree、containerのrunは検証済みの成果のcommitのtree）なので、固定した変更の写しをbaseのcommitに当てて作り直す。
     どちらもagent evalのケースのtreeと同じく自分のGit（ほかのref無し、本番とobjectを共有しない）で、runのreviewはbaseの上に変更を載せた形にする。
     作り直したtreeは、ケースの一覧と期待を除く前に記録したtreeの識別と一致することを確かめ（記録は本番のtreeのものなので、除いた後では一致しない）、一致しないケースはjobを起動しない。
     再現するのはcommitのtreeで、ignoredのファイル（buildの成果など）と未commitの変更は再現しない。
     材料が名指すbaseとheadのcommitの識別は本番のままで、ケースの自分のGitには無い（agent evalのケースと同じ）。
     どちらのtreeにもケースの一覧と期待（agentのケース・役割のケースの置き場・ケースの集合）を置かない既存の境界を保つ。
   - (2c) **追加のdagqの読み取り。** promptが促す追加の読み取りは拒み、主な読み取りは今の組み立てどおり材料の節に前もって含める。
     evalのjobはqueueに届かない起動（資格を発行せず宛先も渡さない）にし、roleはjobのまま残してdagqをfail closedにする。
     promptは変えず、拒んだときの判定の質は測定が測る。
   - (2d) **見せない境界。** 不変条件は「そのケースの材料とtreeは読めて、他のケース、本番の記録（queueのDB・runの記録・元の判定・receipt）、repositoryのcheckoutとGit directoryは読めない」ことで、実現はproviderごとに決める。
     Codexは読める場所の表を役割のjobに当て、queueに届かない起動を足す。
     Claudeは作業dirをケースのtree、追加の作業dirを材料のdirだけにし、作業dirの外の読み取りを拒む設定（Claude Codeの公式文書permissionsの「Block file-tool reads and writes outside the working directories」）を入れる。
     この設定はfile toolを限るものなので、shellの道具を持たせない。
     Readのdeny規則には、allowでも追加の作業dirでも例外を作れない（同じ文書の「Manage permissions」はallowでdenyに例外を作れないとし、追加の作業dirを例外にする記述も無い）ので、ディレクトリごと拒んだ中を開け直す形を前提にせず、ケースのtreeと材料を含むpath（今の配置ではqueueのdata dir）にはdenyを置かず、denyはケース自身を含まないpathだけに置く。
     他のケースとdata dirの中の本番の記録は、作業dirの外の読み取りの拒否が拒む。
     Claudeの境界は実CLIで確かめるまで、Claudeの組を役割の設定に入れる根拠にしない。
     元の判定・後続のevent・期待のラベルはprompt・tree・読める場所・envのどこにも置かない。
   - (2e) **復旧のjobは対象にしない。** Spikeで`does_not_hold`だった（cwdがqueueのdata dirの中のrun directoryで、その時点のrun directoryとworktreeとprocessを残す部品が無い）ので、人の答えによりgoal 254へ移した。
     harnessの口は役割を足せる形にし、復旧のjobを足すときはgoal 254がamendsで決める。

3. **ケースの置き場とsplit。**
   役割のケースは役割の名前の下に置き、dev・holdout・productionはケースの欄でなくファイルで区分する。
   ADR-t1728-1決定1の「役割の区分のディレクトリを作らない」はagentの定義を役割で分けないという決定で、役割の名前の下の置き場はagentを区分しないので両立し、決定1は変えない。

4. **起動経路（決定5をamends）。**
   役割のケースは、本番のその役割のjobと同じ起動（役割のpromptの組み立て・provider・時間の上限・記録）で1ケース1本のjobとして流す。
   決定2の境界に要る起動の違い（読める場所、queueに届かないこと、Claudeのshellの道具）は本番の起動の部品に足し、eval専用の起動の組み立てを作らない。
   決定5のreviewのagentの経路はそのまま保つ。

5. **設定の候補の上書き。**
   devの周のprovider・model・effortの上書きを役割にも当てる。
   送り先の検査はreviewのagentと同じ判断で、依頼のproviderはその役割の本番の設定か、evalの送り先として宣言したものに限る（送り先を設定に限る安全の検査で、Spikeの14周がこれで拒まれた）。
   本番の役割のproviderの解決にevalの宣言を使わない（測るために本番を動かさない）。
   上書きはdevだけで、hold-outとproductionは受けない（ADR-t1728-1決定7とADR-t2229-1を保つ）。

6. **productionのケース。**
   本番の判定のeventと固定したsnapshotから作る。
   ラベルはそのケースの元のjobが実際に起動したproviderと別のproviderが付ける（群とfallbackで役割の設定と別のproviderが動くので、今の設定で決めない）。
   食い違いはreviewのagentのproductionの集合と同じ流れで人が決める。

7. **採用。**
   [review-agent-settingsのSpike](../plans/review-agent-settings-spike.md)の閾値と軽さの物差しに、その推奨を受けた次の直しを合わせる。
   - (7a) **閾値。** 既定はSpikeの物差しのままとする。
     見落としが痛い判定（見落とすと素通りして先へ進む側。runは着地し、planはreadyになり、goalは閉じる）はrecallだけを既定より厳しくし、precisionは既定のままにする。
     見落としは素通りするが誤検出はworkerか人が見て退けられ、痛さが非対称だからである。
     満点を求めないのは、回の数が少ない周で1回の揺れが不合格になるためである。
     閾値を役割や判定ごとに持つ設定の仕組みは作らず、候補の比べで成績と手で比べる。
   - (7b) **比べてよい候補。** 周が完了し、errorが無く、捨てられた回を除いてtokenと費用の記録が揃うものに限る。
     Spikeの条件から捨てられた回を除くのは、supervisorの引き継ぎで起きてやり直され成績に入らない回で、候補の判定の正しさと関係が無く、着地が続く本番では長い周ほど避けられないためである。
   - (7c) **軽さ。** 完了した回あたりに直し、捨てられた回を除く（捨てられた回の見込みの額が費用に足されると実際より重く見えるため）。
     候補の順はproviderをまたいでも回あたりの費用で決め、tokenは参考にして、費用の出どころ（実額か換算か）を書き添える。
     Spikeの表はtokenと費用を並べるだけで順の決め方を持たず、tokenはproviderとmodelごとに重さが違ってまたいでは順を決められないので、順を費用で決める。
   - (7d) **決められないとき。** 拒まれた周・未完了・値の欠けは順位に入れず、軽い根拠にも重い根拠にもしない。
     比べてよい候補が1つも無い役割は`unresolved`、比べてよい候補のどれも閾値を満たさない役割は今の設定のままとし、どちらも設定を変えない。
     設定を軽くするのは閾値を満たした組だけである。
   - (7e) **確かめ。** hold-outの1回の制限のキーは組を含まない（ADR-t1728-1決定7を保つ）ので、既にhold-outを流した定義とケースの組で選んだ組を確かめるには人の再実行の指定が要る。
     役割は定義のファイルを持たないので、キーの定義のdigestに当たる値はharnessの実装が決める。

## Amendsの判断

ADR-t1728-1は番号付きの決定を13持ち、ここで変えるのは決定3の「今作るharnessはreviewだけ」と決定5の起動経路をreviewのagentのjobに限ることだけなので、丸ごと置き換えずamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
決定1・2・4と6以降は変えない。
役割のevalにもそのまま当たる: 依頼の認可（決定6）、hold-outの1回の制限（決定7）、費用の上限（決定8）、実行の枠（決定9）、productionのケースをhold-outとして流した後にtaskで足すこと（決定11）。
programのreviewをケースに当てる決定13は、本番でprogramの段の後にjobが動くrunのreviewの全体のケースに当たり、programの段の無いplan reviewとgoal reviewには当たらない。
決定10は定義のファイルを変えるrunの判定で、役割は定義のファイルを持たないので当たらない。

## Alternatives

- **過去のeventから当時の入力を推測してケースにする**: 本番の材料は「今」のqueueから集めて残らないので、当時の入力の証明にならない。
- **mainのtreeとreviewの材料の差分でrunのreviewの全体のtreeを代える**: 新規のファイルと変更後のファイルの全文を本番と同じように読めない。
- **headのcommitからtreeを置く**: runのbranchが消えるとreviewしたheadはrefから届かなくなり、差し戻された試みや着地しなかったrunのheadも同じで、ケースを作れない（着地の時に残るrefはrebaseの後のcommitを指すことがあり、reviewしたheadの代わりにならない）。
- **時点で切った読み取り専用のqueueを追加の読み取りに供給する**: そのqueueを作る部品が無く、queue serviceは本番のDBを開く。
- **envから宛先を消すだけで読み取りを拒む**: 資格は発行され、Codexのnetworkも開く（Spikeの試作）。
- **Claudeでqueueのdata dir全体をdenyにする**: ケースのtreeと材料がその中にあるので、ケース自身を拒む。
- **ケースをdata dirの外へ移す**: agent evalの配置・片付け・引き取りを変えるわりに、作業dirの外の拒否で同じ境界が得られる。
- **ラベルのproviderを今の役割の設定で決める**: 群とfallbackで元のjobが動かしたproviderと同じになりうる。
- **閾値を役割や判定ごとに持つ設定を作る**: 候補の比べは手で足り、着地を止める判定に使わないので仕組みが要らない。
- **捨てられた回があれば比べない（Spikeの条件のまま）か、やり直しを前提にする**: 長い周ほど比べられず、やり直しても引き継ぎで同じことが起きる。

## Consequences

- 実装は後続のtaskが行う（ケースの型、役割ごとのharness、起動の違い、上書き、productionのケース。Claudeの境界の確かめを含む）。実装までの約束と入口は[agent eval](../design/agent-eval.md)の「役割のharness」が持つ。
- 比べの結果と選んだ組は測定の文書に置き、設定を変えるのは決定7の条件を満たす役割だけである。
- 拒まれる読み取りがあるので、evalの成績は本番より材料が少ない条件の値で、この差は測定の文書が読むときの前提である。
- 記録が始まるまでproductionのケースは作れず、snapshotの保存の分だけ本番のjobの起動に書き込みが増える。
