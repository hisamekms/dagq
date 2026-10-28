---
id: design-queue-broker
type: design
title: Queue broker
status: current
created: 2026-10-12
scope: runtime
tags:
  - security
  - runtime
related:
  - adr-t2114-1
  - adr-t2114-2
  - adr-t2177-1
  - adr-t2114-3
  - adr-t1233-4
  - adr-t2113-1
  - adr-t2257-1
  - adr-t2113-2
  - design-queue-service
  - design-container-runtime
  - design-security
---

# Queue broker

containerで動くrunが外へ出る唯一の出口になる、hostのプロセス（制御側）の地図。
queueごとに1つで、podmanでは動かさず、固定バイナリだけで動く（[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)決定1）。
fs・process・git・packageを仲介する撤去済みのresource broker（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)）とは別物で、名前もコードも共有しない。

宛先はqueue service、`api.anthropic.com`（[ADR-t2114-2](../adr/2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)・[ADR-t2177-1](../adr/2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md)）と、`dagq.toml`が挙げるpackage registryへの`CONNECT`（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定3）である。
containerからbrokerまでの経路（containerの中の転送・runごとのunix socket・VMの中のrelay）は[Container runtime](container-runtime.md#brokerへの経路)が持ち、brokerは`127.0.0.1`のTCPのportで受ける。

## 入口

- 要求と応答の形は`domain::queue_broker`のmodule docと定数が持つ。
  拒否のcodeの綴りと意味は`domain::queue_service::ServiceErrorCode`のdoc commentが持つ。
  clientはこの型と定数に従い、ここに書き写さない。
- 1つの要求の判断（宛先・run tokenの検証・serviceへの転送・tunnelを開くか・拒否の記録）は`application::queue_broker::QueueBroker::handle`、tokenとtunnelの宛ての判定は`domain::queue_broker`。
- registryのallowlistの設定の綴り・置き場・意味・既定は`infrastructure::run_env::EGRESS_REGISTRIES`のdoc comment、1つのhostの書き方は`domain::queue_broker::RegistryHost`が持つ。
- Claudeの資格情報の`host.toml`の表の綴り・置き場・意味は`infrastructure::claude_credential`のmodule doc、上流へ送って応答を流すところは`infrastructure::queue_broker::forward_claude`が持つ。
- brokerのプロセスがHTTPを読み書きしてserviceのsocketへ転送するところと、`up`・`down`・supervisorが起動・停止・生死を見る口は`infrastructure::queue_broker`のmodule docから辿る。
  clientの要求の送り方はそのmoduleの送る側の関数が、上の形のまま持つ。
  containerの中のクライアントモードのdagqもこの形で送る（[Queue service](queue-service.md#クライアントモード)）。
- 置き場は`<queue dir>/broker/`（mode 0700）で、記録・lock・logのファイル名は`domain::queue_broker`の定数が持つ。
  portは起動のたびに空いているものを取り、記録（`BrokerRecord`）に残す。
  supervisorはこの記録のportへrelayを向け、brokerが別のportで起動し直せばrelayを起動し直す（記録の区分と寿命は[計測](measurement.md#ssotとビュー)の表）。

## 不変条件

- 宛先: queue serviceの決まったpathと`/anthropic`の下への、loopbackのhostを名指す（か`Host`の無い）要求と、registryのallowlistのhostとportへの`CONNECT`だけを通す。
  それ以外（allowlistの外への`CONNECT`・別のpath・同じpathでも別のhostを名指すもの）は`forbidden_destination`で拒む（`domain::queue_broker::Destination::of`）。
  宛先は要求が名指すhost（`CONNECT`の宛先・絶対URIのhost・`Host`の値）か、無ければmethodとpathで記録する。
- registryのallowlist: runtimeが組み込みで持つ宛先はqueue serviceと`api.anthropic.com`だけで、registryのhostはコードに持たず`dagq.toml`だけが与える。
  brokerは`CONNECT`のたびに、queueが結びついたrepositoryのmain checkoutの`dagq.toml`から読み、runのcloneからは読まない（着地前のworkerの変更でallowlistを変えさせない）。
  上流のgitのremoteのhost（main checkoutのremoteのURLから求める）は書いても拒み、理由を答えとeventに出す（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)決定3）。
  `api.anthropic.com`は書いても`CONNECT`では通さない。
  通した`CONNECT`はtunnelになり、byteを両向きに写す（`infrastructure::queue_broker::tunnel`）。
- tokenの検証: queue serviceへの要求で通すのは、制御側がworker（resumeを含む）のrunに発行したrun token（[ADR-t1233-4](../adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4のtoken）で、要求が名指すrunのものだけ。
  tokenが無い・発行していないか失効した・jobのtokenかrunを持たない・別のrunの・runが終わったかqueueに無い要求は`unauthenticated`で拒む。
  tokenはqueue serviceと同じ記録（`queue_service::lookup`）で引き、runの状態はqueue serviceのユースケース（`broker_run_status`）で読む。
  serviceに届かず読めなければ、その要求は拒む（fail closed）。
  `CONNECT`はそれに加えて、そのrunのverificationのtoken（下の節）も通す。
  package managerはtokenをproxyのURLから送るので、`CONNECT`のrunとtokenはproxyの認証からも受ける（headerの形は`domain::queue_broker`のmodule doc）。
- principal: 通した要求はbodyのtokenの欄を検証したrun tokenで埋めてserviceのunix socketへ送るので、serviceはそのrunのprincipalで認可して書く。
  brokerはqueueのユースケースを自分で答えず、serviceに届かなければ`unreachable`で返し、DBを開いて代わりに答えない。
- 素通し: serviceの答えは長さで切らず、書かれたとおりに返す。
- 拒否を残すこと: 拒んだ要求はqueueのevent `queue_broker_denied`にrunと宛てを残す（欄は`domain::queue_broker::QUEUE_BROKER_DENIED`のdoc comment）。
  HTTPとして読めない要求も、読めたところまでのrunと宛てで残す（`infrastructure::queue_broker::read_request`）。
  何も送らずに閉じた接続は要求ではなく、残さない。
  記録はqueue serviceのユースケース（`broker_record`）を通り、actorはuser（下の「起動と停止」）で、どのbrokerの記録かはpayloadのbrokerのpidとbuildが示す（`domain::queue_broker::stamped`）。
  tokenの値と、要求の中身を引くserdeの文言はevent・log・errorにも答えにも書かない。
- 拒否の記録の量: runとhostはclientが決める値なので、記録の件数とbrokerのmemoryは`domain::queue_broker::DenialLimiter`で窓ごとに絞る（上限と窓はその隣の定数）。
  run・宛て・code・reason・tokenのrunの同じ拒否は、窓ごとに上限の件数まで1件ずつ残し、超えた分はeventを書かずに数える。
  reasonとtokenのrunも同じものだけを束ねるので、同じrunと宛ての安い拒否（tokenの無い要求など）を繰り返しても、別のreasonや別のrunのtokenを使った拒否の記録は埋まらない。
  窓を持つ組の数には、tokenのrunの有るものと無いものの別に上限があり、有る側はtokenのrunごとにも上限がある。
  上限の後の新しい組の拒否は組ごとに残さず、codeとtokenのrunごとの溢れとして数える。
  tokenのrunは制御側が発行したtokenから引くので、tokenの無い要求でも、1つのtokenを持つcontainerでも、有る側を埋めきれない。
  落とし穴: 無い側は作ったrunやhostで埋められ、埋まっている間の新しい組の拒否はcodeごとの件数だけになる（記録の量を抑える代わりの限界）。
  数えた件数は、窓の終わりにbrokerが待ちの合間（と停止の前）に書く1件のevent（同じkind）か、組の窓の後の最初の記録のどちらか先の1件に残す。
  答え（status・code・文言）は上限の外でも変わらず、絞るのは記録だけ。
  落とし穴: brokerが窓の途中で止まるか、件数のeventを書けないと、その件数は残らない。
- 1つのqueueに1つ: 生きている間lockを持ち、2つ目の`queue-broker serve`は止まる。
  見る側はlockとhealthのpathへの問い合わせで、生死とbuildを判定する（queue serviceと同じ形）。

## Claudeの認証の差し替え

- 入口: containerのClaude Codeは`ANTHROPIC_BASE_URL`にbrokerの`/anthropic`、`ANTHROPIC_AUTH_TOKEN`にrun token、runのheaderを受け、brokerはその下のpathをloopbackを名指す要求だけ`api.anthropic.com`の宛てとして扱う。
  `api.anthropic.com`そのものを名指す要求と`CONNECT`は通さない。
- 通すのはqueue serviceと同じworker（resumeを含む）のrun tokenで、要求が名指すrunのものだけ（verificationのtokenも拒む）。
- 資格情報は`host.toml`（queueのものがhost全体のものを表ごと上書きする）で人が選んだ種類だけを要求のたびに読み、長期tokenは`Authorization`のbearer、API keyは`x-api-key`にして送る。
  要求の`Authorization`・`x-api-key`・runのheader・接続のheaderは送らないので、run tokenはbrokerの外へ出ない。
  置き場は0600のファイルか、macOSのkeychainの項目のどちらか1つで、keychainは`security`のコマンドで読み、値はその出力だけで受けて引数に出さない。
  種類が選ばれていない・置き場が無いか2つある・ファイルか項目が無い・0600でない・空・keychainの無いOSで項目を選んだときは送らず`unauthenticated`で拒み、理由と選び方を答えとeventに出す（値は出さない）。
- 応答はstatus・header・body（SSEのstreamを含む）を届いた順にそのまま返し、接続のheaderだけを`Connection: close`に替える。
  上流の認証の失敗・利用上限・残高の不足も素通しで、brokerは別の種類に切り替えない（分類と控えは実行側のClaudeのturnの読み取りが持つ）。
- 落とし穴: 上流へは`curl`の子processで送り、資格情報はそのstdinのconfigで渡して引数に出さず、bodyはbrokerのdirの0600の一時ファイルに置いて送った後に消す。
  hostの`curl`が無いと`unreachable`で拒む。
- containerを使えるかの判断（`ClaudeCredentialGate`）は中身を読まず、種類・置き場の有無・ファイルの権限だけを見て、workerの適用先だけを止める（verificationはClaudeを呼ばない）。
  keychainの項目が空なことはこの判断では分からず、brokerの読み取りで拒む。
  `doctor`の`claude_credential`も同じものだけを出す。

## verificationのtoken

- `integrate`は検証コマンドを流すverificationごとに、制御側としてそのrunのverificationのtokenを発行し、verificationの終わりで（通っても落ちても）失効させる（ADR-t2114-1決定2、`application::queue_broker::VerificationToken`）。
  containerのrunの着地の再確認のcommandも、supervisorが同じく自分のtokenを発行して失効させる。
  tokenは用途（`application::queue_broker::TokenFor`）ごとに別に持ち、同じrunの同じ用途の次の発行がそれを失効させるので、着地と再確認が同時に走ってもお互いを失効させない。
- 途中で`integrate`かsupervisorのプロセスが殺されて残ったtokenは、runがその用途のverificationを流さない状態（queueに無いことを含む）になれば片付ける。
  runを回復したとき（supervisorの回復と`recover`）はそのrunの分を、brokerは起動時に、supervisorは最初のpassとsweepごとに残った全部を見て失効させる。
  発行・失効・片付けはbrokerの置き場の同じlockの下で行い、片付けは判じた発行のtokenだけを失効させるので、判じた後にそのrunの着地が発行し直したtokenは残る。
  失敗は起動を止めずにlogとqueueのevent `queue_broker_tokens_swept`に残す（入口と用途ごとの状態は`application::queue_broker::sweep_left_tokens`・`LeftTokens`・`TokenFor::verifies_while`、欄は`domain::queue_broker::QUEUE_BROKER_TOKENS_SWEPT`のdoc comment）。
  片付けるまでは、brokerはrunがtokenを持つ状態の間そのtokenの`CONNECT`を通す。
- principalはそのrunのverificationで、queue serviceのtokenとは別の置き場（`<queue dir>/broker/`の下、`infrastructure::queue_broker::issue_verification`）に持つので、serviceはこのtokenを受けない。
- brokerはこのtokenでregistryのallowlistへの`CONNECT`だけを通し、queue serviceと`/anthropic`の下への要求は`unauthenticated`で拒む。
  runが終わったtokenも拒む。
- hostで流すverificationはこのtokenを使わず、containerのverificationの実行がtokenのfileを受け取る。

## 起動と停止

queue serviceと同じ手順に乗せる（[Queue service](queue-service.md#起動と停止adr-t1233-4決定12)、ADR-t2114-1決定1）。

- `up`: serviceの後、supervisorより先に`application::queue_broker::ensure`を打つ（このbuildが答えればreuse、居なければ起動、別のbuildか答えないものは入れ替え）。
  起動できなければsupervisorを起動せずに止まる。
- `down`: supervisorの後に、serviceより先に止める。
  drainを待たない`down`は`queue_broker_stop_requested`で、drainを終えた最後のsupervisorに止めさせる。
- `install`と自動更新: execの引き継ぎの後の新しいバイナリのsupervisorの最初の見張りが、別のbuildのbrokerを1回だけ入れ替え、入れ替えて動いたbuildはそのまま受け入れる。
  `install`のdrainの後は`up`が新しいバイナリで起動する。
- 起動するのは`up`かsupervisorを動かしているバイナリ自身で、actorのenvとクライアントモードのenvを外し、起動元から切り離す。
  本番では固定バイナリだけが`up`とsupervisorを動かすので、開発中のバイナリは本番のbrokerを起動しない（`status`・`doctor`などの読み取りはbrokerを起動しない）。
- brokerのプロセスはqueueのDBを開かない。
  自分の要求（runの状態・queueが結びついたrepository・拒否とtokenの片付けの記録）は、actorの環境もtokenも無く、serviceと同じOSのユーザーとしてhostのserviceのsocketへ送るので、serviceはuserとして答える（[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定1、入口は`infrastructure::queue_service::Client::broker`）。
  転送するcontainerの要求は検証したrun tokenを付けたものだけで、自分の要求と混ぜない。
  serviceに届かなければ、記録できない拒否と片付けはwarnのログに残すだけで、DBを開いて代わりに書かない。

## 見張りと知らせ

- supervisor（`--once`でないもの）はserviceと同じ間隔と上限で見て、居ない・答えないbrokerを起動し直す（`supervise::host_ops::queue_broker`）。
  見る時期・窓の数え方・次の一手の判断は`domain::queue_broker::watch_step`ほかで、supervisorは注入した時計の単調時刻を値で渡す。
  drainと引き継ぎの間は見ない。
- 起動し直せない・上限に達したときは、inbox宛てのattention `queue_broker_down`をqueue serviceのユースケース（`record_queue_event`）で記録し、brokerがまた動けば消える。
  serviceに届かず記録できなければ、supervisorはwarnのログを残すだけでDBを開いて代わりに書かない。
- brokerが居ない間、containerは使えない: supervisorと`integrate`は`ContainerRuntime`を`BrokerGate`越しに問い、brokerが動いていなければ理由つきで使えないと答える（[Container runtime](container-runtime.md#fail-closedの境界)）。
  serviceと違い、hostのrunのclaimとjobは控えない。

## まだ無いもの

- `status`・`doctor`でのbrokerの見せ方と、人の手での起動・停止のコマンド。
