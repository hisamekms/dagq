---
id: adr-t1545-1
type: adr
title: runtimeの責務をレイヤーとコンテキスト（計画管理・実行と着地・観測と分析・host運用）の2軸で分け、contextの間を公開したportと値だけでつなぎ、境界をまたぐtransactionを名指しの例外にし、規則の本文を設計文書の1か所に置く（ADR-0013決定1をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0013 decision 1
owners:
  - hisamekms
tags:
  - architecture
  - domain
  - application
  - infrastructure
related:
  - adr-0013
  - adr-0032
  - adr-0054
  - adr-0073
  - adr-t598-1
  - adr-t1091-1
  - adr-t1410-1
  - adr-t1453-1
  - adr-t1453-2
  - design-architecture
  - design-overview
  - design-persistence
---

# ADR-t1545-1: runtimeの責務をレイヤーとコンテキストの2軸で分け、contextの間を公開したportと値だけでつなぐ（ADR-0013決定1をamends）

## Context

[ADR-0013](0013-layered-architecture-and-type-function-style.md)決定1はruntimeをdomain・application・infrastructure・起動部分のレイヤーに分けると決めた。レイヤーの分離は進んだが、レイヤーの中は業務のまとまりで分かれていない。2026-10-03のmainでは、変更と衝突がportの定義・起動部分・supervisorのループ・domainの入口の少数のファイルに集中し、supervisorの1つのstructが計画・実行・観測・hostの運用の状態をまとめて持ち、そのsubmoduleが互いの状態を直接変える。ほとんどのユースケースはqueueの全部のportを合わせた型を受け取り、何を読み書きするかがsignatureから見えない。レイヤーの禁止依存を確かめる機械的な検査もreviewの検査も無く、overviewの規則に反する参照（applicationからレイヤーの外のmoduleへ）が残っている。

人は2026-10-03に、責務をレイヤーと業務のまとまり（context）の2軸で分け、境界を設計文書・reviewのsubagent・機械的な検査で守る方針に合意した（goal 100）。ADR-0013のほかの決定（型＋関数、集約、VO、エラー、時刻とIDの注入、トランザクションの境界など）は変えない。

## Decision

### 1. レイヤーとcontextの2軸

ADR-0013決定1のレイヤー（domain・application・infrastructure・起動部分）と依存の向きはそのまま保ち、もう1つの軸としてcontextを置く。contextは次の4つにする。

- **計画管理**: 何を作るか（goal・task・proposal）と、その検査と採否（plan review・goal review・runtimeのplanner・follow_upとfindingのproposal）。
- **実行と着地**: taskをrunにして動かし、検証し、mainへ着地させること（claim・run・session・review・integrate・resume・triageと復旧・e2eの工程）。
- **観測と分析**: 起きたことを読み、数え、予測し、知らせること（events・watch・stats・KPI・forecast・印・observer・スループットの見直し）。
- **host運用**: runtime自身をhostで動かし続けること（up・down・install・自動更新・broker・queue service・compileの共有・diskの後始末・hostの計測）。

runtimeのコードは1つのレイヤーと1つのcontextに属する。どのcontextにも属さない共有の部品（IDと時刻とID生成、eventの記録と種類、人への問い合わせ、actorと認可）は小さく保ち、業務の判断を持たせない。境界の置き場所（どのtable・event・port・moduleがどのcontextか）は設計文書がコードを根拠に持つ。

### 2. contextの間は公開したportと値だけでつなぐ

- contextは自分の状態（tableの行・eventの種類・常駐プロセスの中の状態）を所有し、それを変えるのは自分の操作だけにする。
- 他のcontextを参照するのは、そのcontextが公開すると決めたportと、値（ID・型付きのevent・読み取りのview）だけにする。他のcontextのstoreを直接書かず、内部の状態を直接変えない。
- 観測と分析は他のcontextの状態とeventを読むだけで、他のcontextの状態を変えない。
- 依存の向きはレイヤーの向き（domainは他のレイヤーに依存しない、applicationはinfrastructureと起動部分に依存しない）を先に守り、その中でcontextの向きを守る。

### 3. 境界をまたぐtransactionは名指しの例外にする

原子性（claimとlease、着地とtaskの完了、verdictの適用など、ADR-0013決定9とADR-0054が守る1つのtransaction）は境界の規則のために弱めない。複数のcontextの状態を1つのtransactionで変えてよいのは、設計文書の一覧に名前・書き手のcontext・理由を持つものだけにする。一覧に無い越境の書き込みは違反として扱い、新しく要るときは同じ変更で一覧に足してreviewを受ける。

### 4. 規則の置き場所と、目的にしないもの

- crateやdirectoryを増やすことを目的にしない。分け方の完了はファイルの置き場所ではなく、状態の所有・portの幅・依存の向きで判断する。レイヤーを分けるためだけのcrateの分割はしない（ADR-0013のAlternativesのまま）。
- 境界の規則の本文は設計文書の1か所に置く。reviewのsubagent（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)の基盤）と機械的な検査は、規則の節への参照と検査項目だけを持ち、本文を写さない（[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)と同じ考え）。
- 今ある違反は、理由と行き先のtaskを持つ許可の一覧にだけ置き、直したtaskが同じ変更で一覧から外す。

## Alternatives

- **crateの分割を先にする**（contextやレイヤーごとのcrate）: 依存の向きをコンパイラが強制するが、今は1つのportの型と1つのsupervisorのstructが全部のcontextをまたいでいて、分割の線を引く前にworkspace・test・buildの組み直しが一度に要る。ADR-0013が退けた理由がそのまま当たる。状態とportを先に分ければ、crateに分けたくなったときにその線が分割線になる。
- **supervisorをactorやcontextごとのprocessに分ける**: 状態の共有は無くなるが、claimとlease・着地・引き継ぎ（handoff）の1つのtransactionと1つのtokenによる所有がprocessの間の協調になり、停止と引き継ぎの規則（ADR-0054・ADR-0073）を作り直すことになる。今の問題は1つのprocessの中の状態の混在で、process境界は要らない。
- **directoryを機械的にcontextで分ける**: 見た目は分かれるが、共有の状態と広いportが残れば変更と衝突の集中は減らない。置き場所を完了条件にしない。
- **contextごとにDBやschemaを分ける、contextの間をmessageでつなぐ**: 境界は強くなるが、schemaとmigrationを変えない制約と、越境の原子性（claim・着地）を1つのSQLiteのtransactionで保つ今の設計に反する。
- **レイヤーだけを保ち、contextの軸を置かない（現状維持）**: 規則は増えないが、portとsupervisorの肥大と衝突の集中が続く。
- **規則の本文をreviewのsubagentやAGENTS.mdにも書く**: 読み手には近いが、本文が複数の場所に分かれて食い違う。ADR-t1453-2で退けた形と同じ。

## Consequences

- 設計文書がcontextごとに所有する状態・判断・操作・公開するport・依存の向き・越境のtransactionと今の違反を持ち、後続のtask（supervisorの状態の分割・portとcompositionの分割・時間に依る判断の切り出し・検査とreview）はその節を参照して進む。
- 越境のtransactionが一覧で見えるので、原子性を保ったまま、どこが境界の例外かをreviewと検査で確かめられる。一覧の外の越境は違反として見つかる。
- 新しいコードはcontextを決めてから置くことになり、portを足すときはどのcontextに公開するかを決める手間が増える。
- 今の違反は許可の一覧に残るあいだ検査を通るので、一覧の項目が行き先のtaskの着地で減ることを見張る必要がある。
