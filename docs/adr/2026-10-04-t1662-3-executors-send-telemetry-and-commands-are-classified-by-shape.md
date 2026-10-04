---
id: adr-t1662-3
type: adr
title: sessionの中身はagentではなくそれを動かす側（executor）がTelemetrySink（queue serviceのtelemetry_report）に送り、executorには制御側が対象ごとに発行するtelemetry専用のprincipalを与え、実行環境・資源・送る口を差し替えられる口にし、コマンドは汎用の形（shape）だけを持って台帳を作るときに設定の規則で分類する（ADR-t614-1決定2・ADR-t1233-4決定4・6をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-t614-1 decision 2
  - adr-t1233-4 decision 4
  - adr-t1233-4 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - security
  - observability
related:
  - adr-0033
  - adr-0048
  - adr-t614-1
  - adr-t728-1
  - adr-t1233-1
  - adr-t1233-4
  - adr-t1233-5
  - adr-t1486-1
  - adr-t1662-1
  - adr-t1662-2
  - design-measurement
  - design-queue-service
  - design-security
  - design-authorization
  - design-supervisor-lifecycle-source-repository
---

# ADR-t1662-3: 実行する側と送る口とコマンドの分類

## Context

計測のモデル（[ADR-t1662-1](2026-10-04-t1662-1-runs-are-covering-phase-intervals-folded-into-a-ledger.md)）はsessionの中身をstep（決定7）として持ち、資源と実行環境（決定8）を区間に結びつける。今、workerの中身（何をしていたか）はsessionが閉じたときにruntimeがtranscriptやturnのコマンドのファイルから`session_closed`の`work`に分類して書く（[stats](../design/supervisor-lifecycle/stats.md)の「作業の内訳」）。分類はcargoとこのrepositoryの検証の形（`--test e2e`・`llvm-cov`・全体の`cargo test`）を前提にしたコードで、[ADR-t614-1](2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)決定2の(d)がdagqのソースのrepositoryでだけ動かすと決めた。

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)は制御側と実行側を分け、queue serviceを経路にした。[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4は実行側のAI actorのrun・jobごとのtokenを、決定6はwrapperとhooksの段(5)までのDBの直接の読み書きを決めた。stepを送るのに既存のagentのtokenを使うと、agentに計測を書く権限を広げることになる。runtimeのplanner（`TurnOwner::Planner`）・plan review・goal review・throughput reviewにはrunが無い（[headless-worker](../design/supervisor-lifecycle/headless-worker.md)）。

人は2026-10-04にrequest 13の方針（D8・D9・E1〜E7）を承認し、ADRを3本に分けた（D10）。telemetry専用のprincipalとrunを持たない対象、shapeの形と分類の規則は、plan reviewの指摘（2026-10-04）で足した。

## Decision

### I. 誰が何を送るか

1. **D8 送るのはexecutor。** stepを送るのはagentを動かす側（executor）: session wrapper・runner、非対話のjobを起動するプロセス、`integrate`とe2eの検証を動かすもの。agentは送らない。supervisorは工程（`run_phase_changed`）・claim・slot・ask・`environment_attached`・nodeの時系列・外から測れる資源を自分で書く。stepは30秒ごとにまとめて送る。
2. **E1 送る口はTelemetrySink。** executorはTelemetrySinkのportに送り、今のアダプタはqueue serviceのユースケース`telemetry_report`（APIの版つき）にする。送れなくてもrun・job・sessionの結果は変えない（[ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md)決定10と同じ）。
3. **E2 差し替える3つの口。** 実行環境の口（host・podman・後の別のnode。付いた環境を`environment_attached`で返す）・資源の口（区間とnodeの資源を測る）・送る口（TelemetrySink）を、計測のモデルを変えずに差し替えられるportにする。
4. **E3 資源の共通の単位。** どの資源の口も同じ単位（CPU時間・メモリの最大・読み書きの量・壁時計の時間とnodeの識別）で返し、環境ごとの単位を台帳に持ち込まない。単位の綴りは設計書。
5. **E4 blocker: infra。** 実行環境の用意・故障・資源の不足で進めない時間は、計算（`compute`）ではなく`blocker: infra`の区間にする。
6. **E5 適合test一式。** 3つの口のアダプタは、同じ適合test一式（同じ入力から同じstep・資源・`environment_attached`を返すこと、送れないときに結果を変えないこと、重複と抜けの扱い）を通ってから使う。
7. **E6 OpenTelemetryのspanとの対応。** 区間はspan、stepはその子のspan、タグと属性はspanのattributeに対応させ、外へ出すときはこの対応で写す。SSOTはEventStoreとSessionStepStoreのままで、OTLPを記録の経路にしない（Claude Code自身のOTLPを使わないADR-0048決定10は変えない。[ADR-0033](0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)のtracingの経路とも別）。
8. **E7 範囲。** stepはtokenを運ばない（tokenは[ADR-t1486-1](2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)のExecutionの記録）。

### II. executorのtelemetry専用のprincipal

9. **新しいroleで、telemetry_reportだけを呼べる。** 既存のagent（worker・job）のtokenと認可は流用も拡張もしない。新しいrole（名前は設計書。例`executor_telemetry`）を足し、そのtokenが呼べるのは`telemetry_report`の1つだけで、ほかのユースケースは全て拒む。agentのtoken（worker、review・recovery・plan review・goal review・throughput reviewのjob）は`telemetry_report`を呼べない。
10. **対象は1つ。** principalは対象を1つだけ持つ。runの対象（workerのsessionと、review・recoveryのjob、`integrate`・e2eの検証のようにrunの上で動くもの）は`run_id`、runを持たない対象は種類とid（runtimeのplannerは`planner_id`、plan reviewは`plan_review_id`、goal reviewとthroughput reviewはjobのid。supervisorがagentを起動するrunの無いsessionがほかに在れば同じ形）。他の対象の名で送ったstepは拒む。
11. **発行と失効は制御側だけ。** supervisorだけが、runのclaimとresume、jobの起動、runtimeのplannerを開くときに対象ごとに発行し（resumeでは発行し直して前のものを失効させる）、runの終わり（`run_holds_token`の終わり）・leaseの喪失・jobの終わり・`planner_closed`で失効させる。AI actorにtokenを作るコマンドは持たせない。tokenのファイルはagentのworktree・run dir・plannerのdirの外に置き、agentのenvとargvに渡さない（hostでは助言的。ADR-t1233-4決定5）。
12. **台帳への所属と保存の範囲。** runの対象のstepはそのrunの台帳の行に畳む（親の区間の内側の層）。runを持たない対象のstepは[ADR-t1662-2](2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)のSessionStepStoreに対象の種類とidつきで90日だけ持ち、run・taskの台帳の行に入れず、`final`の条件にも入れない（対象ごとに読める）。
13. **ADR-t1233-4決定4・6をamendsする。** 決定4（tokenはAI actorのrun・jobごと）を、制御側が発行するtelemetry専用のprincipalに広げ、runの無いplannerとjobを対象に持てるようにする。決定6（wrapper・hooks・plannerは段(5)までDBを直接開き、wrapperのprincipalの形は段(5)が決める）には、段(5)より前でもexecutorのtelemetryだけはこの狭いprincipalでqueue serviceを通るという例外を足す。wrapper・hooks・plannerのそれ以外の読み書きは段(5)までDBを直接開くままで、段(5)のwrapper自身のprincipalの形は先取りしない。

### III. D9 コマンドの形と分類

14. **SSOTにはコマンドの汎用の形だけを持つ。** `[telemetry] command_detail`で詳しさを選ぶ: `program`（プログラム名だけ）・`shape`（既定。下の形）・`redacted`（種類と時間だけ）。本文・出力・コマンド全文・値とfilterの平文は持たない。
15. **shapeの形。** シェルのコマンドをパイプ・`&&`・`||`・`;`で分けた部分ごとに、先頭のシェルの予約語（`until`・`while`・`if`・`for`など）・変数の代入の名前（値は落とす）・プログラム名（pathを除いた名前）・引数の語の列を持つ。語は次のどれか。
    - flag: `-`で始まる語。名前は平文、`--name=value`のvalueはdigest。
    - word: それ以外の語。プログラム名の直後から最初のflagまでの先頭2つまでで`^[a-z][a-z0-9-]{0,23}$`に合う語はサブコマンドとして平文、ほかのwordはdigest。
    - `+`で始まる語（cargoのtoolchainなど）: digestに`+`の印を付け、サブコマンドの位置に数えない。
    - 区切りの`--`: 後の語は数だけ持つ。
    - リダイレクト（`2>&1`・`> out`）は落とす。
    - digestはSHA-256(`"dagq-cmd-v1\0"` + 語)の先頭16桁のhex。照合のためで秘匿ではない。値を残したくないrepositoryは`program`か`redacted`を選ぶ。
    - サブコマンドの位置の語は形だけで決めるので、その位置に来た短い小文字の語（`cargo test claim`の`claim`のようなfilter）は平文で残る。決定14の「filterの平文は持たない」はこの位置の外の語についてで、この位置の語を残したくないrepositoryも`program`か`redacted`を選ぶ。
16. **分類は台帳を作るときに設定の規則で行う。** `[[telemetry.commands]]`の配列を上から当て、部分ごとに最初に一致した規則の`class`（と任意の`group`: 上の段の集計の名前、省くと`class`）を付ける。一致しなければ未分類。コマンドの分類は、部分の分類のうち規則の並びで先のもの。規則の項目:
    - `class`（必須。名前は設定が決め、runtimeは意味を持たない）・`program`（必須）
    - `sub`: サブコマンドの列の前方一致（例`["test"]`・`["nextest", "run"]`）
    - `flags`: 全て在るflagの名前。`none_of`: どれも無いflagの名前
    - `flag_values`（`{flag = [literal, ...]}`）: そのflagが在り、その全ての値（`--f=v`のvか直後のword）が列のどれか
    - `takes_value`: 直後のwordを値として扱うflagの名前
    - `positional`: `"any"`（既定）か`"none"`（規則の`sub`が一致した語・`flag_values`と`takes_value`の値・`--`の後を除くwordが無い。サブコマンドの位置の語でも規則の`sub`に一致しなかったものはwordとして数える）
    - `args_any`: どれかのwordがliteralのどれか。`lead`: 部分の先頭の予約語のどれか

    台帳は部分ごとの分類も持ち、回数（`integrate`と重なる検証の数など、1つのコマンドの複数の部分が別々の分類に当たるもの）は部分の分類から数える。literalは平文の語ともdigestとも照合する（runtimeがliteralのdigestを作って比べる）。`program`の詳しさはプログラム名だけ、`redacted`は種類と時間だけを持つので、flagの値やwordを照合する規則はその詳しさでは一致しない（未分類になる）。
17. **今のworktimeの分類が使う区別はこの形で表せる。** 今の分類は、e2eは`--test`の値、`full_test`はtargetの値とfilterの有無で決まり、どちらもflagの名前・値のdigestとliteralの照合・`positional`で表せる。`cargo test --locked --test it`は`sub = ["test"]`・`flag_values = {"--test" = ["it"]}`・`takes_value = ["--test"]`・`positional = "none"`の規則で`full_test`、`cargo test --locked --test plugin`は`plugin`が`["it"]`に無いのでその規則に一致せず、後の`sub = ["test"]`だけの規則で`test`になる（`it`・`plugin`はdigestで持つが、literalのdigestと照合する）。規則の全体と、今のコードと分け方が違う稀な形（`--test e2e --test it`のように`--test`に別々の値を並べたものなど）の一覧は設計書が例として持ち、段2の並走比較が差を説明する。
18. **雛形と提案。** 規則の雛形を配り、`dagq init`が提案する。落ちたtestの名前とpathは設定で有効にし、test runnerの読み方のアダプタを選ぶ。
19. **ADR-t614-1決定2をamendsする。** 決定2の(d)のうち、コマンドの分類と回数（cargo test・llvm-cov・e2eの分類と回数、`integrate`と重なる検証の数）を、dagqのソースかの判定から外し、このrepositoryの`dagq.toml`の`[[telemetry.commands]]`の設定に置き換える。設定の無いrepositoryでは未分類として数えるだけで、cargoを前提にした値を出さない。hostの`rustc`とtoolchainの記録とその分け方は今の判定のまま。決定2の(a)〜(c)（migrationの振り直し・installの手順・自動更新）と決定1・3は変えない。

### IV. 他のADRとの関係

- **[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)は変えない**: executorは制御側に、agentは実行側に残り、stepはqueue serviceのユースケースとservice側の認可を通る。決定5の段(5)までのDBの直接の読み書きにtelemetryは頼らない。
- **[ADR-t1486-1](2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)は変えない**: tokenの記録の主体はsupervisorのままで、stepはtokenの代わりにならない（決定8）。

## Alternatives

- **agentに送らせる**: promptや書き忘れで抜け、書き換えもしやすい。agentの権限も広がる。
- **agentのtokenに`telemetry_report`を足す**: 既存の認可を広げ、計測のための権限がagentの他の操作と混ざる。
- **コマンド全文を持つ**: 秘密や個人の値がSSOTに残る。
- **分類を記録の時にコードで行う（今の形）**: cargoとこのrepositoryの形に縛られ、規則を変えると過去の記録を分け直せない。shapeを持てば台帳を作り直して分け直せる。
- **OTLPを記録の経路にする**: 外のcollectorに頼ることになり、queueのSSOTと二重になる。

## Consequences

- workerのsessionの中身がproviderと実行環境に依らず同じ形で残り、分類の規則を変えたら台帳を作り直して過去も分け直せる。
- queue serviceに新しいroleとユースケースが1つずつ増え、[Security](../design/security.md)・[Authorization](../design/authorization.md)の表とそれを写すpluginの文書を同じ変更で直す（実装のtask）。
- digestは辞書で戻せるので、値を隠したいrepositoryは`program`か`redacted`を選ぶ。
- runを持たない対象のstepは90日で消える。
