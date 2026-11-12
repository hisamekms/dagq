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
| 信頼しない実行側（`UntrustedAgent`） | runner | 持ち主の人に結びつき、報告は制御側が照合するデータ |
| 信頼しない実行側（`UntrustedAgent`） | image-publisher（mainに縛ったCI） | digestは制御側が検査するデータ。mainを書けない |
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
  supervisorが決定的に遷移へ写し、読めない出力は何も進めない（[Roles](supervisor-lifecycle/roles.md#headlessのjobのverdictと遷移)）
- observer: findingと、findingに紐づく`blocked`のaskだけ
- planner: draftのtaskとproposal。
  `ready`にするのは通常はplan reviewのverdictをsupervisorが適用するとき（と人の明示の`ready --bypass-review`）
  再計画の置換proposalの子はpassの後も`submitted`に残り、`ready`にするのはtrusted runtimeの適用だけ（[Plan review](supervisor-lifecycle/plan-review.md#不変条件)）
- inbox: 人の言葉を写したanswerと代行の操作。記録で人自身と区別する（下の「answerと代行の記録」）

containerのworkerのcloneもdataで、制御側はgitを走らせない（[Container runtime](container-runtime.md#git)）。
その状態のファイルはworkerが偽りうるが（既知の性質）、得るのは未commitの変更の不着地だけで、着地は制御側が確かめたcommitで安全は依らない。

## actor × capability

外向きの入口の人もactorはuserのままで、登録簿の役割が許可を狭める（[Policy](authorization.md#policy)）。

`src/domain/authorization.rs`の`StaticPolicy`の要約。
明示して許したもの以外は全て拒む（default deny）。
列はcapabilityの群（入口は[Authorization](authorization.md#capability)）で、○は許す、△はresourceの規則つきで許す、—は拒む。

|role|読み取り・watch|計画（goal・task・proposal）|`ready` / `--bypass-review`|note・mark|ask を開く|answer / ask close|finding|計画の依頼（`request add` / `request decline`）・再計画（`replan request` / `replan propose` / `replan resolve`）|session|運用（up・down・install・init・migrate・rebind・plan・setup-review）|supervise・recover・review・observe|着地の依頼（`integrate`）|着地の実行・push|telemetryの送信（`telemetry_report`）|runner・image|`audit.read`|
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
|user|○|○|○ / ○|○|○|○ / ○|○|○ / —・○ / — / ○|○|○|○|○|—|—|`runner.share`（管理者）・`runner.list`（所有者か管理者）|○|
|inbox|○|○|○ / ○|○|○|○ / ○|○|○ / —・○ / — / ○|○|○（`--yes`は—）|○|○|—|—|`runner.list`（親の人の範囲）|○|
|planner|○|△ task・proposalの所有・状態はPolicyの規則。`goal ready`・`goal review`は—|— / —|○（runにはnoteだけ）|△ `planner_question`だけ（所有はPolicyの規則）、runに紐づくものは—|— / —|resolve・dismiss（recordは—）|— / △ 自分が立てられた依頼だけ・— / — / —|—|○（setup-reviewは—）|—|—|—|—|—|—|
|worker|読み取り（`watch`・`queue.export`・`ci.read`は—）|—|— / —|△ noteだけ、自分のrunとtask|△ 自分のrunかtaskの`worker_question`だけ|— / —|—|— / —・— / △ 自分のrunだけ / —|—|—|—|—|—|—|—|—|
|review-job|読み取り|—|— / —|—|—|— / —|—|— / —・— / — / —|—|—|—|—|—|—|—|—|
|recovery-job|読み取り|—|— / —|—|—|— / —|—|— / —・— / — / —|—|—|—|—|—|—|—|—|
|plan-review-job・goal-review-job・throughput-review-job・setup-review-job|読み取り|—|— / —|—|—|— / —|—|— / —・— / — / —|—|—|—|—|—|—|—|—|
|observer|○|—|— / —|—|△ findingに紐づく`blocked`だけ|— / —|△ recordとresolve（dismissは—）|— / —・— / — / —|—|—|—|—|—|—|—|—|
|supervisor|○|`goal close`・`cancel`だけ|○ / —|noteだけ|○（findingに紐づく`blocked`は—）|— / ○|record・resolve・dismiss|— / —・— / — / —（保留と置換は○）|○（recordは—）|○（setup-reviewは—）|○|○|—|—|—|—|
|wrapper|読み取り|—|— / —|—|—|— / —|—|— / —・— / — / —|△ 自分のrunかplannerだけ|—|—|—|—|—|—|—|
|integrator|読み取り|—|— / —|—|—|— / —|—|— / —・— / — / —|—|—|—|—|○|—|—|—|
|runner|—|—|—|—|—|—|—|—|—|—|—|—|—|—|`runner.declare`・`runner.claim`・`runner.heartbeat`・`runner.report`（自分）|—|
|image-publisher|—|—|—|—|—|—|—|—|—|—|—|—|—|—|`image.publish`だけ|—|
|executor-telemetry|—|—|— / —|—|—|— / —|—|— / —・— / — / —|—|—|—|—|—|△ 自分のtokenの対象に登録されたsessionだけ|—|—|

`identity.login`・`credential.preference`はuser専用。
goalの印（`goal.landing_mark`）もuserだけで、人はownerか管理者、hostのuserは人のownerの無いgoalだけ。
代わりに答える人（`delegate.write`）はuserとinboxだけで、OIDCの人（inboxは親の人）の分を変える。
空き容量の必要量の緩め（`disk.relax`）もuserとinboxだけ。

権限ごとの持ち主とresourceの条件は[AuthorizationのPolicy](authorization.md#policy)を読む。

- plannerの運用の権限はADR-t728-1（上）の決定7に従う（`dagq-planner` skillの「Where your authority ends」はこの行と一致させる。規則は[文書の規則](../development/documents.md)の「権限の表を写す文書」）
- sessionの画面とlogを読む・送る`screen.read`・`screen.send`（[ADR-t1228-1](../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定7）はuserとinboxだけが持つ。
  表のsessionの列のsupervisorの○とwrapperの△はこれを含まない（supervisorは自分の送信の経路を使う）。
  plannerは自分のplannerのものも拒まれる。
  runには画面が無く、`run screen`は認可を通った後もどのrunにも理由とturnのlogのCLIを示して拒む（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）
- 終わったrunの残ったworkspaceの片付けの`workspace.cleanup`もuserとinboxだけが持ち、ほかのroleは`authorization_denied`で拒まれる。
  runtimeはrunのworkspaceを開かないので、認可を通った後も理由を示して拒み（ADR-t1433-3の決定3）、残ったrunのworkspaceは人が自分のterminalで閉じる。
- 開いている非対話のruntimeのplannerへの続きの依頼の`planner.request`（[ADR-t1533-1](../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)）はuserとinboxだけが持つ。
  表のsessionの列はこれも含まない（plannerは自分のplannerへのものも拒まれる）。
  plannerを閉じるCLIは無い
- executorのtelemetryはtokenの対象に制御側が登録したsessionだけを送れ、agentにはtokenを渡さない（hostでは助言的）。
  発行・登録・失効の入口は`application::telemetry_tokens`、登録の認可は`TelemetryTokens::register_telemetry_session`のdoc commentが持つ（[ADR-t1662-3](../adr/2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)）。

この表と`StaticPolicy`は全roleのallowとdenyをunit test（`src/domain/authorization.rs`）が網羅する。表を変えるときは先にコードを変え、この表と[Authorization](authorization.md#policy)の表を合わせる。

## 判定の場所

状態を変える全てのCLIのコマンドは、parseではなくapplicationの層でmutationの前に`Authorizer`を通る（[Authorization](authorization.md#適用の範囲)）。

- 計画系は`Planning`、対話と記録は`Dialogue`、runtimeの操作系は`Operation`（queueを開く・作る・移す前）。
  supervisorはsandboxの外で動くので、workerが書くrun dirのfileを、supervisorと非対話のwrapperの`LocalRunFiles`とprocessの出力のopenが`O_NOFOLLOW`で開いた記述子に対する操作で扱い、workerの代わりにrunのdirの外を読み書きしない（上限は`src/infrastructure/run_files.rs`、[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)の「run dirのfile」）。
  hostのGitが作業するscratch worktree（`already_satisfied`のsnapshotの検証のもの）はrun dirに置かず、queueの`satisfied/`のrunごとのdirに置き、使うたびにそこにある`.git`もlinkも信用せずに消して（linkは辿らない）作り直す（[Validation](supervisor-lifecycle/validation.md#already_satisfiedの検証)）。
  拒否は`authorization_denied`のeventとして、拒まれた呼び出し元をactorに記録し、拒否のerrorを返す
- 着地とpushは`Integrator`がもう一度判定する（下の「reviewのpassとIntegrator」）
- queue serviceは、[ユースケース](queue-service.md#ユースケース)と読み取りを、principalのactorで`StaticPolicy`に通す（`DAGQ_ROLE`は使わない）。
  principalはtokenから、tokenもroleも無い要求ではhostのsocketのpeerの資格からuserとして決める（[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定1）。
  拒否は同じ`authorization_denied`に、principalの無い要求は`queue_service_unauthenticated`に残す（runner_httpの拒否記録は未実装）。
  worker（resumeも）・headlessのjob・observerはserviceだけを使い（[クライアントモード](queue-service.md#クライアントモード)）、人・inbox・plannerはserviceが答えるコマンドをDBを開かず送る（[hostのCLI](queue-service-host-cli.md#送る範囲)）。
  閉じたのはCLIの経路で、同じユーザーのプロセスはDBもtokenも読め、userとしてもつなげるので、hostでは助言的なまま（ADR-t3102-1決定5）
- serviceへ送らない状態を変えないコマンド（制御側の診断・fileを書く読み取りと、supervisorなどのもの）は、roleを問わず`check_access`が`StaticPolicy`に通す（default deny）。
  拒否は同じerrorを返し、serviceが拒まれたprincipalの`authorization_denied`に記録する（[例外](queue-service-host-cli.md#principal)あり）。
  通った読み取りはqueueに書かない
- Claudeのguardrailの規則とsettingsの置き場は`execution::permission_deny`のdoc commentを入口にする。
  これは誤りを早く止めるguardrailで、pathやscriptからの呼び出しは通るので、拒むのはCLIの判定
  sessionへの操作は判定と記録を通るCLIを使う（[Authorization](authorization.md#runtimeの操作系のコマンドapplication)）。
  これは**guardrailであってenforcementではない**: denyはClaude Codeのtoolの呼び出しの綴りで照合する助言的な抑止で、`dagq inbox`を通さずに開いたinboxと、`claude`を打ち直したsessionには効かない（inboxがguardrailつきで開かれたかは`status` / `doctor`の`inbox_guardrail`が見せる）。
  hostの判定は助言的なまま（ADR-t728-1決定6）。
  人自身のterminal（`DAGQ_ROLE`なし）はsettingsを持たない
- Codexのinboxは、同じ規則を`dagq inbox`が起動の引数で渡すPreToolUseのhookで持つ（ADR-t2159-1決定5。一致の範囲とClaudeとの違いは[Authorization](authorization-guardrail.md)）。
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


### Codexのworkerのsandbox

Codexの非対話のworkerはworkspace-writeのsandbox（macOSはseatbelt）で動き、書いてよい場所（runのworktree・Gitのrun branchのrefとobjects・run dir・cargoのregistry）の外への書き込みと、sandboxの外のプロセスへのsignalをOSが止める（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)、設定は[provider-lifecycle](provider-lifecycle.md#codexの非対話のworker)）。
sandbox内でのプロセスの読み取りとOSの制約は`infrastructure::process_read`のmodule docを入口にする。
ADR-t728-1決定6の強制の最初のものだが、隔離ではない: 同じユーザーとして動き、読むことは広く（他のrunのworktreeや人の設定も）でき、networkは開いている。
そこでenforcementはこれを`advisory`とも隔離の`sandbox`とも別の`confined`とする（決定7）。

## reviewのpassとIntegrator

着地（rebase・rebase後の再検証・squash・mainの更新）とpushは、信頼する制御側の`Integrator`（actor `integrator:<pid>`）だけが行う（ADR-t728-2、[Authorization](authorization.md#着地とpushintegrator)）。

- review-jobの`pass`は「着地してよい」というデータで、着地の実行ではない。supervisorはそれを`IntegrationRequest`に写してIntegratorに渡すだけ
- reviewのpassは着地の関門を免除しない。
  依頼者の認可と承認の検査は`integrate::landing_refusal`、着地の記録と依頼者の区別は`IntegrationRequest`のdoc commentを入口にする。
- 収束はコードの指摘が1件も無い差し戻しだけで、securityのreviewのagentの指摘はいつもコードの指摘に数えるので、それが残る差し戻しは収束せず人の`approve_landing`に上がる（ADR-t2599-1決定3・ADR-t1906-1、[Review](supervisor-lifecycle/review.md#reviewの流れ)の4a）
- `push_main`はIntegratorだけが作れる`PushGrant`を要る

## answerと代行の記録

inboxはaskのanswerと`dagq-recover`の手作業を代行でき、人の役割を持つOIDCの発行元のある配置だけ、権限は親の人の役割で狭まる（[AuthorizationのPolicy](authorization.md#policy)）。
発行元の無いローカルの既定には役割が無く、inboxは今どおり代行できる（ADR-t3985-1決定6・7）。
記録で人自身と区別する（ADR-t728-3決定2）。

- eventのactorとanswerの`AnswerAuthority`は人自身とinboxの代行を区別し、answerには承認の分類（`answer_approves`）も残す
- 区別はactorの型から決め、answerの文やpromptから推さない
- inboxのterminalで人が打つ`!`のコマンドも`DAGQ_ROLE=inbox`を継ぐのでinboxの代行として記録される。
  人自身の操作として残したいときは、`DAGQ_ROLE`の無い別のterminalで打つ

許される代行には、inboxの誤解による操作を止められないリスクが残る。

## 将来の道筋

強制の点を足す約束と隔離・broker・承認の境界は[強制の境界と道筋](security-enforcement.md)、監査は[監査](audit.md)を入口にする。
