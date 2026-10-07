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
  - adr-t728-1
  - adr-t1228-2
  - adr-t1533-1
  - adr-t1394-1
  - adr-t728-2
  - adr-t728-3
  - design-authorization
  - design-supervisor-lifecycle-roles
  - design-broker
---

# Security

dagqのセキュリティの模型の全体を1か所にまとめる。決定の理由は[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)（信頼の区分・actor・default denyのcapability・hostは助言的）、[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)（Integrator）、[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)（inboxのanswerと代行）。コマンドごとのcapabilityとresourceの写し、resourceの規則の細部は[Authorization](authorization.md)、actorの型・環境変数・起動・eventのactorは[Roles](supervisor-lifecycle/roles.md#actors)が持ち、この文書はそれらを重複して書かない。

原則: AI actorは提案・生成・評価をしてよいが、自分に権限を与えること、特権の状態遷移を確定すること、割り当ての外の外部副作用を起こすことはできない。認可はRustのコードとpolicyのdataが決め、AIの出力やpromptの指示では決めない。promptの指示はUXで、enforcementではない。

## 信頼の区分

| 区分 | actor | 性質 |
| --- | --- | --- |
| 人（`Human`） | user | `DAGQ_ROLE`の無い呼び出し。host実行の互換のためにそう扱う（下の「host実行は助言的」） |
| 信頼する制御側（`TrustedControlPlane`） | supervisor・wrapper・integrator | 決定的なRustのコード。AIを含まない |
| 信頼しないAI actor（`UntrustedAgent`） | inbox・planner・worker・review-job・recovery-job・plan-review-job・goal-review-job・throughput-review-job・observer | Claude（とCodex）のsession / job。出力はデータ |

`TrustLevel`はroleだけから決まり（`src/domain/actor.rs`）、promptや名前から推し量らない。`DAGQ_ROLE`の未知の値は`unknown DAGQ_ROLE`で止まり、queueを開かない（fail closed）。`desk`（goal 48）と`update-job`は予約の名前で、まだ`ActorRole`に無い。

AI actorの出力は全てデータで、制御側が決定的に遷移へ写す:

- worker: commitとreceipt。validatingとIntegratorが検査し、receiptの主張を信用せずに検証コマンドを流し直す
- review・recovery・plan review・goal reviewのjob: 型付きのverdict（`#[serde(deny_unknown_fields)]`）。supervisorが写像の表どおりに適用し、読めない出力は何も進めない（[Roles](supervisor-lifecycle/roles.md#headlessのjobのverdictと遷移)）
- observer: findingと、findingに紐づく`blocked`のaskだけ
- planner: draftのtaskとproposal。`ready`にするのはplan reviewのverdictをsupervisorが適用するとき（と人の明示の`ready --bypass-review`）
- inbox: 人の言葉を写したanswerと代行の操作。記録で人自身と区別する（下の「answerと代行の記録」）

## actor × capability

`src/domain/authorization.rs`の`StaticPolicy`（`grants(role)`とresourceの規則）の要約。明示して許したもの以外は全て拒む（default deny）。resourceの持ち主や状態が分からないときも拒む（fail closed）。列はcapabilityの群（名前は[Authorization](authorization.md#capability)）で、○は許す、△はresourceの規則つきで許す、—は拒む。

| role | 読み取り・watch | 計画（goal・task・proposal） | `ready` / `--bypass-review` | note・mark | ask を開く | answer / ask close | finding | 計画の依頼（`request add` / `request decline`） | session | 運用（up・down・install・init・migrate・rebind・plan） | supervise・recover・review・observe | 着地の依頼（`integrate`） | 着地の実行・push |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| user | ○ | ○ | ○ / ○ | ○ | ○ | ○ / ○ | ○ | ○ / — | ○ | ○ | ○ | ○ | — |
| inbox | ○ | ○ | ○ / ○ | ○ | ○ | ○ / ○ | ○ | ○ / — | ○ | ○ | ○ | ○ | — |
| planner | ○ | △ draft・submitted・readyのtask（`revisit`はdraftだけで、上限に達したdraftは—）、全状態のfollow_upの所属判断、自分のproposalの取り下げ。`goal ready`・`goal review`は— | — / — | ○（runにはnoteだけ） | △ `planner_question`だけ（依頼に紐づくものは自分が立てられた依頼だけ）、runに紐づくものは— | — / — | resolve・dismiss（recordは—） | — / △ 自分が立てられた依頼だけ | △ 自分のplannerだけ | ○ | — | — | — |
| worker | 読み取り（`watch`・`queue.export`・`ci.read`は—） | — | — / — | △ noteだけ、自分のrunとtask | △ 自分のrunかtaskの`worker_question`だけ | — / — | — | — / — | △ 自分のrunだけ | — | — | — | — |
| review-job | 読み取り | — | — / — | — | — | — / — | — | — / — | — | — | — | — | — |
| recovery-job | 読み取り | — | — / — | — | — | — / — | — | — / — | — | — | — | — | — |
| plan-review-job・goal-review-job・throughput-review-job | 読み取り | — | — / — | — | — | — / — | — | — / — | — | — | — | — | — |
| observer | ○ | — | — / — | — | △ findingに紐づく`blocked`だけ | — / — | △ recordとresolve（dismissは—） | — / — | — | — | — | — | — |
| supervisor | ○ | `goal close`・`cancel`だけ | ○ / — | noteだけ | ○（findingに紐づく`blocked`は—） | — / ○ | record・resolve・dismiss | — / — | ○（recordは—） | ○ | ○ | ○ | — |
| wrapper | 読み取り | — | — / — | — | — | — / — | — | — / — | ○ | — | — | — | — |
| integrator | 読み取り | — | — / — | — | — | — / — | — | — / — | — | — | — | — | ○ |

- review-jobとrecovery-jobの`review.submit` / `triage.submit`は自分のrunだけのcapabilityだが、CLIのコマンドは無く、verdictはsupervisorがデータとして読む。jobの環境には`DAGQ_RUN_ID`が無いので、今は持ち主が分からず拒まれる
- observerの`queue.export`（`graph --out`・`report`）は—。`ci.read`（`ci failures`、[CI watch](supervisor-lifecycle/ci-watch.md)）はuser・inbox・planner・observer・supervisorが持つ。4つのjob・worker・wrapper・integratorは`watch`も`queue.export`も`ci.read`も持たず、状態を変えないコマンド（`watch`・`graph --out`・`report`・`ci failures`）も全roleで`StaticPolicy`が判定するので（下の「判定の場所」）拒まれる（task 859）。読み取り（`queue.read`）は全roleが持つ
- `judge-follow-up`（`follow_up.judge`）はuser・inbox・plannerだけが全状態のfollow_upに記録でき、worker・job・supervisorは拒む。所属変更はdraft/readyだけ。表のplannerのtask変更の例外はこの判断の記録だけである。
- draftの再検討の時刻（`revisit`、[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）は計画の列の`task.write`に含める（許すroleと拒むroleが開始前のtaskの変更と同じなので、新しいcapabilityにしない）。user・inbox・plannerが付け・変え・外し、worker・observer・job・supervisorは拒む。plannerは`draft_planner_exhausted`のあるdraftに付けられない（人・inboxが付けたものだけがplannerの上限を越えて1回立つため）
- user・inboxの計画権限には、最新runが終了し生きているrunの無い`in_progress` taskの`--verify` / `--no-verify`だけを直す`task.verify_edit`も含む（ADR-t883-1）。planner・worker・jobは持たない。
- plannerの行は[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定7のとおり、この段で運用の権限を変えていない（`dagq-planner` skillの「Where your authority ends」はこの行と一致させる。規則は[文書の規則](../development/documents.md)の「権限の表を写す文書」）
- sessionの画面を読む・送る`screen.read`・`screen.send`（`run screen` / `run send`、`planner screen` / `planner send`。backgroundのsessionのlogを読む`run log` / `planner log`も`screen.read`。[ADR-t1228-1](../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定7）はuserとinboxだけが持つ。表のsessionの列のplannerの△とsupervisorの○はこれを含まない（plannerは自分のplannerのものも拒まれ、supervisorは自分の送信の経路を使う）。runには画面が無く、`run screen`は認可を通った後もどのrunにも理由とturnのlogのCLI（`run log RUN [--follow]`）を示して拒む（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、task 1440）
- 終わったrunの残ったworkspaceの片付けの`workspace.cleanup`（`run close-workspaces`）もuserとinboxだけが持ち、ほかのroleは`authorization_denied`で拒まれる。runtimeはrunのworkspaceを開かなくなったので、認可を通った後も理由を示して拒み（ADR-t1433-3の決定3、task 1440）、過去に作られて残ったrunのworkspaceは人が自分のterminalで閉じる。supervisorは終わったrunに残ったbackgroundのwrapperを自分の掃除で止め、このcapabilityを持たない
- 開いている非対話のruntimeのplannerへの続きの依頼の`planner.request`（`planner request`。[ADR-t1533-1](../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)）はuserとinboxだけが持つ。表のsessionの列はこれも含まない（plannerは自分のplannerへのものも拒まれる）。plannerを閉じるCLIは無い
- 計画の依頼の`request.record`（`request add`）はuserとinboxだけが持ち（inboxの記録は人の言葉の代行、[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定3）、`request.decline`（`request decline`）はplannerだけが、自分が立てられた依頼にだけ持つ（決定6）。依頼に紐づく`planner_question`（`ask --request`）も、plannerは自分が立てられた依頼にだけ開ける（task 1564）。拒否は`authorization_denied`に残る
- 予約のcapability（`reserved.filesystem_read`・`reserved.filesystem_write`・`reserved.network`・`reserved.secret_read`）は誰にも与えない。sandboxのbackendが強制するときの名前

この表と`StaticPolicy`は全roleのallowとdenyをunit test（`src/domain/authorization.rs`）が網羅する。表を変えるときは先にコードを変え、この表と[Authorization](authorization.md#policy)の表を合わせる。

## 判定の場所

状態を変える全てのCLIのコマンドは、parseではなくapplicationの層でmutationの前に`Authorizer`を通る（[Authorization](authorization.md#適用の範囲)）。

- 計画系は`Planning`、対話と記録は`Dialogue`、runtimeの操作系は`Operation`（queueを開く・作る・移す前）。Codexのworkerの`dagq ask`は、クライアントモードでqueue serviceのユースケースとして送られ、serviceがworkerのprincipalで判定する（ADR-t1233-5決定5。[Queue service](queue-service.md#クライアントモード)）。supervisorはsandboxの外で動くので、workerが書くrun dirのfileを、supervisorと非対話のwrapperの`LocalRunFiles`とprocessの出力のopenが`O_NOFOLLOW`で開いた記述子に対する操作で扱い、workerの代わりにrunのdirの外を読み書きしない（全文の読みは64MiBまで、FIFOとlinkは読まない。詳細は[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)の「run dirのfile」）。拒否は`authorization_denied`のeventとして、拒まれた呼び出し元をactorに記録し、`{"error": "<role> may not <capability> (<reason>)", "denied": {...}}`を返す
- 着地とpushは`Integrator`がもう一度判定する（下の「reviewのpassとIntegrator」）
- queue service（[Queue service](queue-service.md)）は、`ask`・`show`・`note`・findingと読み取りのユースケースを、tokenから決めたprincipalのactorで同じ`Dialogue`・`Gate`と`StaticPolicy`に通す（service側の判定。`DAGQ_ROLE`は使わない）。拒否は同じ`authorization_denied`に、principalの無い要求は`queue_service_unauthenticated`に残す。worker（resumeを含む）・headlessのjob・observerのプロセスにはqueue DBのpathを渡さず、socketとtokenのfileを渡すので、その`dagq`はクライアントモードでserviceだけを使い、serviceのユースケースでないコマンドと`--db`は拒まれ、serviceに届かなくてもDBを開かない（goal 82の段(3)、[クライアントモード](queue-service.md#クライアントモード)）。閉じたのはCLIを通る経路で、同じユーザーのプロセスはDBのファイルもtokenのファイルも探して読めるので、host構成では助言的なまま（ADR-t1233-1のConsequences）
- 状態を変えないコマンド（読み取り・`watch`・`graph --out`・`report`）は、roleを問わず`check_access`が`StaticPolicy`に通す（default deny、task 859）。拒否は同じ`denied`のJSONを返し、queueがあれば拒まれた呼び出し元をactorにした`authorization_denied`に記録する（task 1151。queueが無いときは記録せずに拒む）。通った読み取りはqueueに書かない
- runtimeがClaudeの設定を書くactor（worker・planner・review job・inbox）の`permissions.deny`には、roleが持たないcommand（状態を変えるものと`watch`・`report`。`graph`は`--out`なしが読み取りなので除く）の`Bash(dagq <command>:*)`と、`DAGQ_ROLE`などactorを名指す変数の書き換えを入れる（`permission_deny(role)`）。これは誤りを早く止めるguardrailで、pathやscriptからの呼び出しは通るので、拒むのはCLIの判定
- inboxとplannerの`permissions.deny`には`Bash(cmux:*)`も入れる（`execution::RAW_CMUX_DENIED`、[ADR-t1228-2](../adr/2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)）。sessionへの操作（画面を読む・決めたキーと答えを送る・plannerへの続きの依頼。runのturnはlogを読む）は判定と記録を通る`run` / `planner`のCLIで行い、記録の残らない生のcmuxにふだん流れないようにする。plannerはruntimeが書くsettings（非対話のturnの`claude-headless-settings.json`と、対話のsessionの`claude-settings.json`）に、inboxは`up`が開くときにqueueのディレクトリに書く`claude-inbox-settings.json`（`permissions.deny`だけ。[`up` / `down`](supervisor-lifecycle/up-down.md)）に持つ。これは**guardrailであってenforcementではない**: denyはClaude Codeのtoolの呼び出しの綴りで照合する助言的な抑止で、絶対pathの`cmux`・scriptの中の呼び出し・別のshellからの実行を止めず、`up`が`reused`で使い続けるinboxと、workspaceで`claude`を打ち直したsessionには効かない（前者は`status` / `doctor`の`inbox_guardrail`が見せる）。hostの判定は助言的なまま（ADR-t728-1決定6）。workerとjobの`cmux`は拒まず、隔離（goal 38・goal 82）に任せる（決定7）。人自身のterminal（`DAGQ_ROLE`なし）はsettingsを持たず、ADR-t1228-1が人に残したcmuxの操作をそこで打つ
- Codexのinbox（goal 77）は、同じ趣旨（`cmux`で始まるコマンドを拒み、dagqのCLIを使わせる）をCodexの手段で持たせる（ADR-t1228-2決定6）。今のproviderの口（`AgentProvider::inbox_command`）でinboxを持つのはClaudeだけで（`up`は`--claude`でinboxを開く）、Codexはinboxを持たない（`inbox_command`は拒み、`inbox_settings`の既定は`None`。settingsを持たないproviderのinboxが入れば`inbox_opened`の`guardrail`は`false`で記録され、`inbox_guardrail`が見せる）。Codexのinboxを作るtaskが手段を持たせるか、手段が無ければこのguardrailが無いことをここに書き、skillの手順をCLIだけにすることで足りるとする

roleごとに拒まれる主なコマンド（skillとAGENTS.mdはこれを説明する）:

- worker: `integrate`・`answer`・`ask close`・`ready`・`cancel`・計画系の全て・`request add`・`request decline`・`recover`・`review`・`supervise`・`observe`・`plan`・`up`・`down`・`install`・`auto-update`・`init`・`migrate`・`rebind`・`finding`・`mark`、自分のrun以外への`ask`・`note`・`session`・`session-event`
- planner: `ready`（`--bypass-review`を含む）・`goal ready`・`goal review`・`request add`・他の依頼の`request decline`と`ask --request`・`integrate`・`review`・`recover`・`supervise`・`observe`・`answer`・`ask close`・`finding record`・`run screen`・`run log`・`run send`・`planner screen`・`planner log`・`planner send`・`planner request`、runの`session`・`session-event`、runに紐づく`ask`、in_progress以降のtaskの変更、draft以外と上限に達したdraftの`revisit`、他のplannerのproposalの取り下げ
- 4つのjob: 状態を変える全て（`reviewer may not change queue state`）
- observer: `finding record`・`finding resolve`・findingに紐づく`blocked`のask以外の全て（`observer may not change queue state`）
- inbox: 人（user）と同じで、拒まれるのは誰にも与えない着地の実行・pushと予約だけ

## host実行は助言的（advisory）

今のactorは全て同じユーザーとしてhostで動く。**host実行はsandboxでも隔離でもない。** この段の認可は、誤りと事故を止め、誰が何をしたかを記録するための論理的な境界で、security boundaryではなく移行の途中の状態である（ADR-t728-1の決定6）。

同じhostの敵対的なプロセスは次で迂回できる:

- envの偽装: `DAGQ_ROLE`を外せばuser、`DAGQ_ROLE=inbox`にすればinboxとして判定される。`DAGQ_RUN_ID`も書き換えられる。`permissions.deny`はClaude Codeのコマンドの先頭の形しか見ないので、pathや`sh -c`やscriptからは通る（inboxとplannerの`Bash(cmux:*)`も同じ）
- DBの直接操作: queueのSQLiteファイルはこのユーザーが読み書きでき、CLIを通らずに状態とeventを書き換えられる
- Gitとファイル: worktree・main checkout・`runs/`・固定バイナリを直接書き換えられ、pushの資格情報もこのユーザーのもの
- プロセス: 他のactorのプロセスにsignalを送れる（`pkill`・`killall`を拒むのもguardrail）
- 古いバイナリへの戻し: 判定は呼ばれたバイナリのpolicyで決まるので、固定バイナリを戻すと、未知の`DAGQ_ROLE`を拒まない（task 729より前の）バイナリへ戻すと、新しいバイナリが起動したjobのroleを制限しない窓ができる（[Authorization](authorization.md#固定バイナリを戻したときの窓)）

`status`と`doctor`は`actors`に、AI actorごとの`backend: host`・`enforcement: advisory`・`sandboxed: false`を出し、隔離していないことを実行時にも明示する（[Roles](supervisor-lifecycle/roles.md#実行のbackendとenforcement)）。Codexのworkerを動かせるsupervisorが居るときは、workerの行の`providers`がCodexを`enforcement: confined`・`sandboxed: false`で出す（下の「Codexのworkerのsandbox」）。ActorExecutorのspec（workspace・capability・timeout）もhostでは記録と整合の検査だけで、プロセスはこのユーザーにできることを全てできる。


### Codexのworkerのsandbox

Codexの非対話のworkerはworkspace-writeのsandbox（macOSはseatbelt）で動き、書いてよい場所（runのworktree・Gitのrun branchのrefとobjects・run dir・cargoのregistry）の外への書き込みと、sandboxの外のプロセスの一覧とsignalをOSが止める（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)、設定は[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)）。ADR-t728-1決定6の言う「同じcapabilityの模型の上に足す強制」の最初のものだが、隔離ではない: 同じユーザーとして動き、読むことは広く（他のrunのworktreeや人の設定も）でき、networkは開いている。そこで`actors`はこれを`advisory`とも隔離の`sandbox`とも別の`confined`とし、`sandboxed`は`false`にする（決定7）。Claudeのworker（対話・非対話）は`advisory`のままで、permissionの仕組みとsettingsのdenyに頼る。

## reviewのpassとIntegrator

着地（rebase・rebase後の再検証・squash・mainの更新）とpushは、信頼する制御側の`Integrator`（actor `integrator:<pid>`）だけが行う（ADR-t728-2、[Authorization](authorization.md#着地とpushintegrator)）。

- review-jobの`pass`は「着地してよい」というデータで、着地の実行ではない。supervisorはそれを`IntegrationRequest`に写してIntegratorに渡すだけ
- 依頼を出せるのは`landing.request`を持つuser・inbox・supervisorだけ。worker・job・observer・plannerは出せない
- Integratorは依頼を受けても、依頼者の権限、runの状態とlease、承認（`integration_approved`か最新のreviewの`pass`）を確かめ、receiptを信用しない再検証・rebase・scopeの判定を通らなければ着地しない
- `push_main`はIntegratorだけが作れる`PushGrant`を要る
- 記録は、着地のeventのactorがintegratorで、`requested_by`が依頼者（`user`・`inbox`・`supervisor:<pid>`）

host実行ではIntegratorもsupervisorや`integrate`と同じプロセスとユーザーで動き、この境界も論理的なもの。

## answerと代行の記録

inboxは全てのaskにanswerでき、`dagq-recover`の手作業（`integrate`・`recover`・`review`・`ready --bypass-review`・`cancel`・`up` / `down` / `install`など）も人の言葉で代行できる（ADR-t728-3の決定1）。そのかわり記録で人自身と区別する（決定2）。

- eventのactor: 人自身は`actor_role`が`user`、inboxの代行は`inbox`（`events --full`・`show --full`の`actor`）。Integratorが着地するときは`requested_by`が`user`か`inbox`
- answer: askの`answer_authority`（`ask_answered`のpayloadの`authority`）が、人自身は`user`、inboxは`delegated`、runtimeが閉じたものは`runtime`。承認に当たるask（`approve_landing`・`decide`・`approve_plan`・`approve_goal`・`correct_goal`・`approve_update`・`update_failed`と、`blocked` / `stalled`の`propose` / `dismiss`）は`answer_approval`が`1`
- 区別はactorの型から決め、answerの文やpromptから推さない
- inboxのterminalで人が打つ`!`のコマンドも`DAGQ_ROLE=inbox`を継ぐので`delegated`として記録される。人自身の操作として残したいときは、`DAGQ_ROLE`の無い別のterminalで打つ

人しか出せない承認（計画のI6）の強制はこの段では行わない（決定3）。inboxが人の言葉を誤って解釈した操作は止まらず、既知の残りのリスクとして後のgoalが扱う。記録から代行の割合を読めるのが、その強制の前提になる。

## 将来の道筋

capabilityの模型（actor・`TrustLevel`・`Capability`・`Resource`・`StaticPolicy`）はbackendを替えても変えない。sandboxやserviceは、同じ模型の上に強制の点を足すだけで、模型を作り直さない（ADR-t728-1の決定6）。

1. **Podman（draftのgoal 38）**: `ExecutorBackend::Podman`は予約の名前で、今は`ensure_implemented`がerrorにし、選ばれたactorを起動しない（hostに黙って戻さない。fail closed）。最初に隔離するのは、最も広いworktreeを持ち信頼しないworkerで、`[actors.worker] backend = "podman"`の形で選ぶ。containerはrunのworktreeだけをmountし、queue DB・main checkout・他のrunを見せず、`EnforcementLevel`は`sandbox`になる。予約のcapability（filesystem・network・secret）はこのbackendが強制する
2. **queue service / broker（goal 82・goal 38）**: DBの直接操作とenvの偽装を塞ぐには、AI actorがqueueのファイルに触れず、制御側のserviceにだけ依頼する形にする。serviceと、workerとjobのdagqのクライアントモード（unix socket・APIのversion・tokenによるprincipalとservice側の判定・`ask`・`show`・`note`・findingと読み取り、claim・resume・jobの起動でのtokenの発行）はある（[Queue service](queue-service.md)、ADR-t1233-1・ADR-t1233-4・ADR-t1233-5）。host構成ではtokenのfileも同じユーザーが読めるので、助言的であることは変わらない（ADR-t1233-4決定5）。serviceは起動した制御側が発行した資格（actor idとrunに紐づくもの）でactorを識別し、`DAGQ_ROLE`を信用しない。判定は今と同じapplicationの境界（`Planning`・`Dialogue`・`Operation`・`Integrator`）で行い、wrapperとhookをworkerの環境から分けてwrapperのactorとして判定する
3. **Integratorの分離**: pushの資格情報をIntegratorのプロセスだけに持たせ、supervisorとAI actorから外す
4. **人しか出せない承認（I6）**: 承認の経路（別のterminal、署名、人の端末からの確認など）を決めてから、`answer_approval`のaskのanswerを人だけに限る

goal 38の「broker」とは別に、fs・process・git・packageを仲介するresource broker（`dagq-broker`、goal 58）がある。workerはhostのまま`preferred`でrunごとのtokenを使ってPodmanのcontainerのbrokerを通すもので、host実行が助言的であることと`actors`の`enforcement: advisory`は変えない（[Resource broker](broker.md)、[ADR-t827-4](../adr/2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)）。

どの段でも、境界がその段の大きさで守れないときは境界を弱めず、follow-upのtaskにする。
