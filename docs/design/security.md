---
id: design-security
type: design
title: Security
status: current
created: 2026-09-28
scope: runtime
tags:
  - security
related:
  - adr-t2113-1
  - adr-t2113-3
  - adr-t2173-1
  - adr-t728-1
  - adr-t1228-2
  - adr-t1533-1
  - adr-t1394-1
  - adr-t728-2
  - adr-t728-3
  - design-authorization
  - design-supervisor-lifecycle-roles
  - adr-t1662-3
---

# Security

dagqのセキュリティの模型の全体を1か所にまとめる。決定の理由は[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)（信頼の区分・actor・default denyのcapability・hostは助言的）、[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)（Integrator）、[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)（inboxのanswerと代行）。コマンドごとのcapabilityとresourceの写し、resourceの規則の細部は[Authorization](authorization.md)、actorの型・環境変数・起動・eventのactorは[Roles](supervisor-lifecycle/roles.md#actors)が持ち、この文書はそれらを重複して書かない。

原則: AI actorは提案・生成・評価をしてよいが、自分に権限を与えること、特権の状態遷移を確定すること、割り当ての外の外部副作用を起こすことはできない。認可はRustのコードとpolicyのdataが決め、AIの出力やpromptの指示では決めない。promptの指示はUXで、enforcementではない。

## 信頼の区分

| 区分 | actor | 性質 |
| --- | --- | --- |
| 人（`Human`） | user | `DAGQ_ROLE`の無い呼び出し。host実行の互換のためにそう扱う（下の「host実行は助言的」） |
| 信頼する制御側（`TrustedControlPlane`） | supervisor・wrapper・integrator・executor-telemetry（agentを動かす側のqueue serviceのprincipalだけ） | 決定的なRustのコード。AIを含まない |
| 信頼しないAI actor（`UntrustedAgent`） | inbox・planner・worker・review-job・recovery-job・plan-review-job・goal-review-job・throughput-review-job・setup-review-job・observer | Claude（とCodex）のsession / job。出力はデータ |

`TrustLevel`はroleだけから決まり（`src/domain/actor.rs`）、promptや名前から推し量らない。
環境のactorを`ActorContext::from_env`で読むコマンドは、`DAGQ_ROLE`の未知の値で止まりqueueを開かない（fail closed。先に分岐して読まない経路は[Roles](supervisor-lifecycle/roles.md#cliでの解釈)）。

AI actorの出力は全てデータで、制御側が決定的に遷移へ写す:

- worker: commitとreceipt。validatingとIntegratorが検査し、receiptの主張を信用せずに検証コマンドを流し直す
  pjのscriptがrun dirに書く手元の検証の記録もworkerが偽りうるdataで、runtimeは古いtreeと壊れた記録を弾くだけで改ざんを検出せず、記録を着地の安全の根拠にしない（advisory、[ADR-t2589-1](../adr/2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)決定4）
  wrapperがexecutor-telemetryのtokenで送り直すstepの番号と未送信のstepはrun dirに置かず、tokenのfileの隣（queueの`service/`、agentのsandboxが書けずcontainerにmountされない）に対象ごとに置くので、workerはそのstepを仕込めない
- review・recovery・plan review・goal reviewのjob: 型付きのverdict。
  supervisorが写像の表どおりに適用し、読めない出力は何も進めない（[Roles](supervisor-lifecycle/roles.md#headlessのjobのverdictと遷移)）
- observer: findingと、findingに紐づく`blocked`のaskだけ
- planner: draftのtaskとproposal。
  `ready`にするのは通常はplan reviewのverdictをsupervisorが適用するとき（と人の明示の`ready --bypass-review`）
  再計画の置換proposalの子はpassの後も`submitted`に残り、`ready`にするのはtrusted runtimeの適用だけ（[Plan review](supervisor-lifecycle/plan-review.md#不変条件)）
- inbox: 人の言葉を写したanswerと代行の操作。記録で人自身と区別する（下の「answerと代行の記録」）

containerのworkerのcloneもdataで、制御側はgitを走らせない（[Container runtime](container-runtime.md#git)）。
その状態のファイルはworkerが偽りうるが（既知の性質）、得るのは未commitの変更の不着地だけで、着地は制御側が確かめたcommitで安全は依らない。

## actor × capability

`src/domain/authorization.rs`の`StaticPolicy`（`grants(role)`とresourceの規則）の要約。明示して許したもの以外は全て拒む（default deny）。resourceの持ち主や状態が分からないときも拒む（fail closed）。列はcapabilityの群（名前は[Authorization](authorization.md#capability)）で、○は許す、△はresourceの規則つきで許す、—は拒む。

| role | 読み取り・watch | 計画（goal・task・proposal） | `ready` / `--bypass-review` | note・mark | ask を開く | answer / ask close | finding | 計画の依頼（`request add` / `request decline`）・再計画（`replan request` / `replan propose`） | session | 運用（up・down・install・init・migrate・rebind・plan・setup-review） | supervise・recover・review・observe | 着地の依頼（`integrate`） | 着地の実行・push | telemetryの送信（`telemetry_report`） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| user | ○ | ○ | ○ / ○ | ○ | ○ | ○ / ○ | ○ | ○ / —・○ / — | ○ | ○ | ○ | ○ | — | — |
| inbox | ○ | ○ | ○ / ○ | ○ | ○ | ○ / ○ | ○ | ○ / —・○ / — | ○ | ○（`--yes`は—） | ○ | ○ | — | — |
| planner | ○ | △ draft・submitted・readyのtask（`revising`のproposalのtaskは持ち主だけ、`revisit`はdraftだけで、上限に達したdraftは—）、全状態のfollow_upの所属判断、自分のproposalの取り下げ。`goal ready`・`goal review`は— | — / — | ○（runにはnoteだけ） | △ `planner_question`だけ（依頼に紐づくものは自分が立てられた依頼だけ）、runに紐づくものは— | — / — | resolve・dismiss（recordは—） | — / △ 自分が立てられた依頼だけ・— / — | — | ○（setup-reviewは—） | — | — | — | — |
| worker | 読み取り（`watch`・`queue.export`・`ci.read`は—） | — | — / — | △ noteだけ、自分のrunとtask | △ 自分のrunかtaskの`worker_question`だけ | — / — | — | — / —・— / △ 自分のrunだけ | — | — | — | — | — | — |
| review-job | 読み取り | — | — / — | — | — | — / — | — | — / —・— / — | — | — | — | — | — | — |
| recovery-job | 読み取り | — | — / — | — | — | — / — | — | — / —・— / — | — | — | — | — | — | — |
| plan-review-job・goal-review-job・throughput-review-job・setup-review-job | 読み取り | — | — / — | — | — | — / — | — | — / —・— / — | — | — | — | — | — | — |
| observer | ○ | — | — / — | — | △ findingに紐づく`blocked`だけ | — / — | △ recordとresolve（dismissは—） | — / —・— / — | — | — | — | — | — | — |
| supervisor | ○ | `goal close`・`cancel`だけ | ○ / — | noteだけ | ○（findingに紐づく`blocked`は—） | — / ○ | record・resolve・dismiss | — / —・— / —（保留と置換は○） | ○（recordは—） | ○（setup-reviewは—） | ○ | ○ | — | — |
| wrapper | 読み取り | — | — / — | — | — | — / — | — | — / —・— / — | △ 自分のrunかplannerだけ | — | — | — | — | — |
| integrator | 読み取り | — | — / — | — | — | — / — | — | — / —・— / — | — | — | — | — | ○ | — |
| executor-telemetry | — | — | — / — | — | — | — / — | — | — / —・— / — | — | — | — | — | — | △ 自分のtokenの対象に登録されたsessionだけ |

- review-jobとrecovery-jobの`review.submit` / `triage.submit`は自分のrunだけのcapabilityだが、CLIのコマンドは無く、verdictはsupervisorがデータとして読む。jobの環境には`DAGQ_RUN_ID`が無いので、今は持ち主が分からず拒まれる
- observerの`queue.export`は—。
  `ci.read`（[CI watch](supervisor-lifecycle/ci-watch.md)）はuser・inbox・planner・observer・supervisorが持つ。
  headlessのjob（`ActorRole::is_headless_job`）・worker・wrapper・integratorは`watch`も`queue.export`も`ci.read`も持たず、状態を変えないコマンドも全roleで`StaticPolicy`が判定するので（下の「判定の場所」）拒まれる。
  読み取り（`queue.read`）はexecutor-telemetryのほかの全roleが持つ
- `judge-follow-up`（`follow_up.judge`）はuser・inbox・plannerだけが全状態のfollow_upに記録でき、worker・job・supervisorは拒む。所属変更はdraft/readyだけ。表のplannerのtask変更の例外はこの判断の記録だけである。
- draftの再検討の時刻（`revisit`、[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）は計画の列の`task.write`に含める。
  user・inbox・plannerが付け・変え・外し、worker・observer・job・supervisorは拒む。
  plannerは`draft_planner_exhausted`のあるdraftに付けられない
- user・inboxの計画権限には、最新runが終了し生きているrunの無い`in_progress` taskのverifyだけを直す`task.verify_edit`も含む（ADR-t883-1）。
  planner・worker・jobは持たない。
- 人の出どころの優先度はuserとinboxだけが変え、planner・supervisor（plan reviewのverdictの適用とruntimeの自動の修正）・jobは計画の列の権限があっても変えられない（[ADR-t1975-1](../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)の決定2〜4）。
  `set-priority`と`goal edit`の変更、依頼の値と違う優先度の`goal add`・`add`は拒んで`authorization_denied`に残し、plan reviewの`lower_priority`は適用せず記録する。
  AIのactorの所属の変更は拒まず、`set-goal`では移動先のgoalが同じ値の人の優先度でなければ移動の前の値を人の個別の優先度としてtaskに残し、follow_upの`out_of_scope`の判断の移動では継いだだけの値を残さない（ADR-t2373-1）。
  拒否と記録は[Authorization](authorization.md#policy)、移動の判断は`task::set_goal`が持つ。
- plannerの行は[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定7のとおり、この段で運用の権限を変えていない（`dagq-planner` skillの「Where your authority ends」はこの行と一致させる。規則は[文書の規則](../development/documents.md)の「権限の表を写す文書」）
- sessionの画面とlogを読む・送る`screen.read`・`screen.send`（[ADR-t1228-1](../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定7）はuserとinboxだけが持つ。
  表のsessionの列のsupervisorの○とwrapperの△はこれを含まない（supervisorは自分の送信の経路を使う）。
  plannerは自分のplannerのものも拒まれる。
  runには画面が無く、`run screen`は認可を通った後もどのrunにも理由とturnのlogのCLIを示して拒む（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）
- 終わったrunの残ったworkspaceの片付けの`workspace.cleanup`もuserとinboxだけが持ち、ほかのroleは`authorization_denied`で拒まれる。
  runtimeはrunのworkspaceを開かないので、認可を通った後も理由を示して拒み（ADR-t1433-3の決定3）、残ったrunのworkspaceは人が自分のterminalで閉じる。
  supervisorは終わったrunに残ったbackgroundのwrapperを自分の掃除で止め、このcapabilityを持たない
- 開いている非対話のruntimeのplannerへの続きの依頼の`planner.request`（[ADR-t1533-1](../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)）はuserとinboxだけが持つ。
  表のsessionの列はこれも含まない（plannerは自分のplannerへのものも拒まれる）。
  plannerを閉じるCLIは無い
- 計画の依頼の`request.record`はuserとinboxだけが持ち（inboxの記録は人の言葉の代行、[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定3）、`request.decline`はplannerだけが、自分が立てられた依頼にだけ持つ（決定6）。
  依頼に紐づく`planner_question`も、plannerは自分が立てられた依頼にだけ開ける。
  拒否は`authorization_denied`に残る
- 再計画（[再計画](../plans/task-replanning.md)）の`replan.request`はuserとinboxだけ、`replan.propose`は自分のrunのworkerだけが持つ。
  runの保留と置換の`replan.park`・`replan.apply`はsupervisorだけが持つ。
  plannerはどれも持たない。
  holdの`task.hold`もuserとinboxだけ
- agentのevalの依頼（`agent eval`、[Agent eval](agent-eval.md)）は、devの`eval.request`をuser・inbox・plannerと自分のrunのworkerが、hold-outとproductionの`eval.request_held_out`とproductionの集合の依頼と決定の`eval.cases`をuser・inbox・plannerが、同じキーの再実行の`eval.rerun`をuserとinboxだけが持つ。
  結果を読む`eval.read`はuser・inbox・planner・observer・supervisorと自分のrunのworkerが持つ。
  supervisorは依頼を実行するだけで依頼のcapabilityを持たず、jobは何も持たない。
  workerはLLMのCLIを打たず、evalのjobはsupervisorだけが起動する。
- executor-telemetryはexecutor（session wrapper・runner、非対話のjobを起動するプロセス、着地の検証）がsessionのstepを送るためのqueue serviceのprincipalで、制御側が対象（runか、runを持たない対象の種類とid）ごとに発行するtokenでだけ名乗れ、processの`DAGQ_ROLE`にはならない（[ADR-t1662-3](../adr/2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)の決定9〜11）。
  持つのは`telemetry.report`（ユースケース`telemetry_report`）だけで、対象はtokenの対象の、制御側がその対象に登録したsessionだけ（他の対象の名で送ったものと、未登録か他の対象のsessionは`authorization_denied`）。
  sessionの持ち主はagentのsessionを起動する制御側だけが登録し（起動の前か、agentが名前を決めるsessionはそれを読んだ時点。[計測](measurement.md#送る口)の「sessionの登録」）、他の対象に登録済みのsessionは登録し直せず、agentとAI actorとserviceのユースケースは登録しない。
  読み取りを含めほかのユースケースは全て`authorization_denied`で拒まれる。
  runの対象のtokenはsupervisorだけが自分のleaseの下で発行し、runの終わりとleaseの喪失で失効させ、発行と失効は互いに排他にする（時点・やり直し・記録は[Queue service](queue-service.md#principalとtoken)と`application::telemetry_tokens`のmodule doc）。
  runを持たない対象のtokenはagentを起動する制御側が発行して対象の終わりで失効させ、起動元が失効の前に落ちても、serviceが要求ごとに対象の終わりをqueueから読んで断る。
  tokenのファイルはqueueの`service/`の下でagentのworktree・run dir・plannerとjobのdirの外にあり、executorは対象から場所を引く口を使い、agentのenvとargvには渡さない（hostでは助言的）
- 予約のcapabilityは誰にも与えない。
  sandboxのbackendが強制するときの名前

この表と`StaticPolicy`は全roleのallowとdenyをunit test（`src/domain/authorization.rs`）が網羅する。表を変えるときは先にコードを変え、この表と[Authorization](authorization.md#policy)の表を合わせる。

## 判定の場所

状態を変える全てのCLIのコマンドは、parseではなくapplicationの層でmutationの前に`Authorizer`を通る（[Authorization](authorization.md#適用の範囲)）。

- 計画系は`Planning`、対話と記録は`Dialogue`、runtimeの操作系は`Operation`（queueを開く・作る・移す前）。
  supervisorはsandboxの外で動くので、workerが書くrun dirのfileを、supervisorと非対話のwrapperの`LocalRunFiles`とprocessの出力のopenが`O_NOFOLLOW`で開いた記述子に対する操作で扱い、workerの代わりにrunのdirの外を読み書きしない（上限は`src/infrastructure/run_files.rs`、[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)の「run dirのfile」）。
  hostのGitが作業するscratch worktree（`already_satisfied`のsnapshotの検証のもの）はrun dirに置かず、queueの`satisfied/`のrunごとのdirに置き、使うたびにそこにある`.git`もlinkも信用せずに消して（linkは辿らない）作り直す（[Validation](supervisor-lifecycle/validation.md#already_satisfiedの検証)）。
  拒否は`authorization_denied`のeventとして、拒まれた呼び出し元をactorに記録し、拒否のerrorを返す
- 着地とpushは`Integrator`がもう一度判定する（下の「reviewのpassとIntegrator」）
- queue serviceは、[ユースケース](queue-service.md#ユースケース)と読み取りを、principalのactorで同じ`Dialogue`・`Gate`と`StaticPolicy`に通す（`DAGQ_ROLE`は使わない）。
  principalはtokenから、tokenもroleも無い要求ではhostのsocketのpeerの資格からuserとして決める（[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定1）。
  拒否は同じ`authorization_denied`に、principalの無い要求は`queue_service_unauthenticated`に残す。
  worker（resumeも）・headlessのjob・observerはserviceだけを使い（[クライアントモード](queue-service.md#クライアントモード)）、人・inbox・plannerはserviceが答えるコマンドをDBを開かず送る（[hostのCLI](queue-service-host-cli.md#送る範囲)）。
  閉じたのはCLIの経路で、同じユーザーのプロセスはDBもtokenも読め、userとしてもつなげるので、hostでは助言的なまま（ADR-t3102-1決定5）
- serviceへ送らない状態を変えないコマンド（制御側の診断・fileを書く読み取りと、supervisorなどのもの）は、roleを問わず`check_access`が`StaticPolicy`に通す（default deny）。
  拒否は同じerrorを返し、queueがあれば拒まれた呼び出し元をactorにした`authorization_denied`に記録する。
  通った読み取りはqueueに書かない
- runtimeがClaudeの設定を書くactor（worker・planner・review job・inbox）の`permissions.deny`には、roleが持たないcommandの`Bash(dagq <command>:*)`と、`DAGQ_ROLE`などactorを名指す変数の書き換えを入れる（`permission_deny(role)`）。
  これは誤りを早く止めるguardrailで、pathやscriptからの呼び出しは通るので、拒むのはCLIの判定
  sessionへの操作（画面を読む・決めたキーと答えを送る・plannerへの続きの依頼。runのturnはlogを読む）は判定と記録を通る`run` / `planner`のCLIで行う。
  plannerはruntimeが書くsettingsに、inboxは`dagq inbox`が開くときにqueueのディレクトリに書くsettings（[`dagq inbox`](supervisor-lifecycle/up-down.md#dagq-inbox)）に持つ。
  これは**guardrailであってenforcementではない**: denyはClaude Codeのtoolの呼び出しの綴りで照合する助言的な抑止で、`dagq inbox`を通さずに開いたinboxと、`claude`を打ち直したsessionには効かない（inboxがguardrailつきで開かれたかは`status` / `doctor`の`inbox_guardrail`が見せる）。
  hostの判定は助言的なまま（ADR-t728-1決定6）。
  人自身のterminal（`DAGQ_ROLE`なし）はsettingsを持たない
- Codexのinboxは、同じ規則を`dagq inbox`が起動の引数で渡すPreToolUseのhookで持つ（ADR-t2159-1決定5。一致の範囲とClaudeとの違いは[Authorization](authorization.md#claudeのpermissionsdenyguardrail)）。
  hookが動かない（trustの欠けなど）Codexのinboxはguardrailの無いinboxで、これも助言的な抑止にとどまる

## host実行は助言的（advisory）

containerに置いたworkerのturnを除き、actorは全て同じユーザーとしてhostで動く。
**host実行はsandboxでも隔離でもない。**
この段の認可は、誤りと事故を止め、誰が何をしたかを記録するための論理的な境界で、security boundaryではなく移行の途中の状態である（ADR-t728-1の決定6）。

同じhostの敵対的なプロセスは次で迂回できる:

- envの偽装: `DAGQ_ROLE`を外せばuser、`DAGQ_ROLE=inbox`にすればinboxとして判定される（queue serviceへはこのユーザーが読めるinboxのtokenのfileも渡す）。
  `DAGQ_RUN_ID`も書き換えられる。
  `permissions.deny`はClaude Codeのコマンドの先頭の形しか見ないので、pathや`sh -c`やscriptからは通る
- DBの直接操作: queueのSQLiteファイルはこのユーザーが読み書きでき、CLIを通らずに状態とeventを書き換えられる
- Gitとファイル: worktree・main checkout・`runs/`・固定バイナリを直接書き換えられ、pushの資格情報もこのユーザーのもの
- プロセス: 他のactorのプロセスにsignalを送れる（`pkill`・`killall`を拒むのもguardrail）
- 古いバイナリへの戻し: 未知の`DAGQ_ROLE`を拒まないバイナリへ戻すと、新しいバイナリが起動したjobのroleを制限しない窓ができる（[Authorization](authorization.md#固定バイナリを戻したときの窓)）

`status`と`doctor`は、AI actorごとのbackendとenforcement（containerの適用先は[Container runtime](container-runtime.md)）を出し、隔離していないことを実行時にも明示する（[Roles](supervisor-lifecycle/roles.md#実行のbackendとenforcement)）。
ActorExecutorのspecもhostでは記録と整合の検査だけで、プロセスはこのユーザーにできることを全てできる。


### Codexのworkerのsandbox

Codexの非対話のworkerはworkspace-writeのsandbox（macOSはseatbelt）で動き、書いてよい場所（runのworktree・Gitのrun branchのrefとobjects・run dir・cargoのregistry）の外への書き込みと、sandboxの外のプロセスの一覧とsignalをOSが止める（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)、設定は[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)）。
ADR-t728-1決定6の言う「同じcapabilityの模型の上に足す強制」の最初のものだが、隔離ではない: 同じユーザーとして動き、読むことは広く（他のrunのworktreeや人の設定も）でき、networkは開いている。
そこでenforcementはこれを`advisory`とも隔離の`sandbox`とも別の`confined`とする（決定7）。
Claudeのworker（対話・非対話）は`advisory`のままで、permissionの仕組みとsettingsのdenyに頼る。

## reviewのpassとIntegrator

着地（rebase・rebase後の再検証・squash・mainの更新）とpushは、信頼する制御側の`Integrator`（actor `integrator:<pid>`）だけが行う（ADR-t728-2、[Authorization](authorization.md#着地とpushintegrator)）。

- review-jobの`pass`は「着地してよい」というデータで、着地の実行ではない。supervisorはそれを`IntegrationRequest`に写してIntegratorに渡すだけ
- 依頼を出せるのは`landing.request`を持つuser・inbox・supervisorだけ。worker・job・observer・plannerは出せない
- Integratorは依頼を受けても、依頼者の権限、runの状態とlease、承認（`integration_approved`か、最新のreviewの`pass`か、上限の後の差し戻しをruntimeが収束と判定した`revise_limit_judged`）を確かめ、receiptを信用しない再検証・rebase・scopeの判定を通らなければ着地しない
- 収束はコードの指摘が1件も無い差し戻しだけで、securityのreviewのagentの指摘はいつもコードの指摘に数えるので、それが残る差し戻しは収束せず人の`approve_landing`に上がる（ADR-t2599-1決定3・ADR-t1906-1、[Review](supervisor-lifecycle/review.md#reviewの流れ)の4a）
- `push_main`はIntegratorだけが作れる`PushGrant`を要る
- 記録は、着地のeventのactorがintegratorで、`requested_by`が依頼者

host実行ではIntegratorもsupervisorや`integrate`と同じプロセスとユーザーで動き、この境界も論理的なもの。

## answerと代行の記録

inboxは全てのaskにanswerでき、`dagq-recover`の手作業も人の言葉で代行できる（ADR-t728-3の決定1）。
そのかわり記録で人自身と区別する（決定2）。

- eventのactorとanswerの権限の出どころ（`AnswerAuthority`）は、人自身とinboxの代行を別の値で残し、answerには承認に当たるか（`answer_approves`）も残す
- 区別はactorの型から決め、answerの文やpromptから推さない
- inboxのterminalで人が打つ`!`のコマンドも`DAGQ_ROLE=inbox`を継ぐのでinboxの代行として記録される。
  人自身の操作として残したいときは、`DAGQ_ROLE`の無い別のterminalで打つ

人しか出せない承認（計画のI6）の強制はこの段では行わない（決定3）。inboxが人の言葉を誤って解釈した操作は止まらず、既知の残りのリスクとして後のgoalが扱う。記録から代行の割合を読めるのが、その強制の前提になる。

## 将来の道筋

capabilityの模型（actor・`TrustLevel`・`Capability`・`Resource`・`StaticPolicy`）はbackendを替えても変えない。sandboxやserviceは、同じ模型の上に強制の点を足すだけで、模型を作り直さない（ADR-t728-1の決定6）。

1. **container**: `dagq.toml`がworker（resumeを含む）とintegrateのverificationの適用先ごとにhostかcontainerを選ぶ（[Container runtime](container-runtime.md)）。
   containerを使えない適用先は控えてattentionで知らせ、hostに黙って戻さない（fail closed、[Container runtime](container-runtime.md#fail-closedの境界)）。
   workerのturn（最初のturn・revise・resume）とintegrateのverificationのコマンドはこの段で動く（[Container runtime](container-runtime.md#verificationの実行)）。
   containerはrunのcloneとrun dirだけをmountし（例外は同じ文書の節）、queue DB・main checkout・他のrunを見せず、`EnforcementLevel`は`sandbox`になる。
   machineはhostの`$HOME`をmountしない（[Container runtime](container-runtime.md#dagq専用のmachine)）。
   予約のcapability（filesystem・network・secret）はこのbackendが強制する。
   containerで動かすのは信頼しないコードを走らせるworker・resumeとintegrateのverificationだけで、読むだけのheadlessのjobの隔離はOSのsandboxにする（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)）。
   containerのrunにはhostのgitの共通dirをmountせず、runごとのcloneを渡す（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)）
2. **queue service / broker**: DBの直接操作とenvの偽装を塞ぐには、AI actorがqueueのファイルに触れず、制御側のserviceにだけ依頼する形にする。
   serviceと、workerとjobのdagqのクライアントモードはある（[Queue service](queue-service.md)、ADR-t1233-1・ADR-t1233-4・ADR-t1233-5）。
   host構成ではtokenのfileも同じユーザーが読めるので、助言的であることは変わらない（ADR-t1233-4決定5）。
   serviceは起動した制御側が発行した資格（actor idとrunに紐づくもの）でactorを識別し、`DAGQ_ROLE`を信用しない。
   判定は今と同じapplicationの境界（`Planning`・`Dialogue`・`Operation`・`Integrator`）で行い、wrapperとhookはworkerの環境から分けたwrapperのprincipalで判定する。
   [queueのbroker](queue-broker.md)の決まった形は、queue serviceと同じ`up` / `down` / execの引き継ぎで動くhostのプロセスで、宛先をqueue service・`api.anthropic.com`と、`dagq.toml`が挙げるpackage registryのhostに限り、allowlistの外を拒む（[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)・[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。
   containerにClaudeの資格情報を置かず、brokerがrun tokenを`host.toml`で人が選んだ長期tokenかAPI keyに差し替える（[ADR-t2177-1](../adr/2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md)）。
   workerのcontainerは`--network none`で、VMの中のrelayからmountしたrunごとのunix socketだけでbrokerに届き、relayはtokenも宛ての判定も持たない（[Container runtime](container-runtime.md#brokerへの経路)）。
3. **Integratorの分離**: pushの資格情報をIntegratorのプロセスだけに持たせ、supervisorとAI actorから外す
4. **人しか出せない承認（I6）**: 承認の経路（別のterminal、署名、人の端末からの確認など）を決めてから、承認に当たるaskのanswerを人だけに限る

上の道筋の「broker」はqueueのbrokerだけを指す。
fs・process・git・packageを仲介する層（resource broker）は置かず、実行側は隔離環境の中で組み込みの道具をそのまま使い、境界は隔離環境が持つ（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)）。

どの段でも、境界がその段の大きさで守れないときは境界を弱めず、follow-upのtaskにする。
