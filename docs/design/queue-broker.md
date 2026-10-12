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
  - adr-t2114-3
  - adr-t1233-4
  - adr-t2113-1
  - adr-t2257-1
  - design-queue-service
  - design-container-runtime
  - design-security
---

# Queue broker

containerで動くrunが外へ出る唯一の出口になる、hostのプロセス（制御側）の地図。
queueごとに1つで、podmanでは動かさず、固定バイナリだけで動く（[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)決定1）。
fs・process・git・packageを仲介する撤去済みのresource broker（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)）とは別物で、名前もコードも共有しない。

今の宛先はqueue serviceだけで、`api.anthropic.com`（[ADR-t2114-2](../adr/2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)）と`dagq.toml`が挙げるpackage registry（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定3）は後続の変更が足し、それまで拒む。
containerからbrokerまでの経路（VMの中のrelayとrunごとのunix socket）は[ADR-t2114-3](../adr/2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)が決め、brokerは`127.0.0.1`のTCPのportで受ける。

## 入口

- 要求と応答の形（method・path・run tokenとrunのheader・bodyの`ServiceRequest`・応答の`ServiceResponse`・拒否のcodeとHTTPのstatus）は`domain::queue_broker`のmodule docと定数が持つ。
  拒否のcodeの綴りと意味は`domain::queue_service::ServiceErrorCode`のdoc commentが持つ。
  clientはこの型と定数に従い、ここに書き写さない。
- 1つの要求の判断（宛先・run tokenの検証・serviceへの転送・拒否の記録）は`application::queue_broker::QueueBroker::handle`、tokenの判定は`domain::queue_broker::judge_run_token`。
- brokerのプロセスがHTTPを読み書きしてserviceのsocketへ転送するところと、`up`・`down`・supervisorが起動・停止・生死を見る口は`infrastructure::queue_broker`のmodule docから辿る。
  clientの要求の送り方はそのmoduleの送る側の関数が、上の形のまま持つ。
- 置き場は`<queue dir>/broker/`（mode 0700）で、記録・lock・logのファイル名は`domain::queue_broker`の定数が持つ。
  portは起動のたびに空いているものを取り、記録（`BrokerRecord`）に残す。
  relayはこの記録からportを読む（記録の区分と寿命は[計測](measurement.md#ssotとビュー)の表）。

## 不変条件

- 宛先: queue serviceの決まったpathへの、loopbackのhostを名指す（か`Host`の無い）要求だけを通す。
  それ以外（`CONNECT`・別のpath・同じpathでも別のhostを名指すもの）は`forbidden_destination`で拒む（`domain::queue_broker::Destination::of`）。
  宛先は要求が名指すhost（`CONNECT`の宛先・絶対URIのhost・`Host`の値）か、無ければmethodとpathで記録する。
- tokenの検証: 通すのは、制御側がworker（resumeを含む）のrunに発行したrun token（[ADR-t1233-4](../adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4のtoken）で、要求が名指すrunのものだけ。
  tokenが無い・発行していないか失効した・jobのtokenかrunを持たない・別のrunの・runが終わったかqueueに無い要求は`unauthenticated`で拒む。
  tokenはqueue serviceと同じ記録（`queue_service::lookup`）で引き、runの状態はqueueを読む。
- principal: 通した要求はbodyのtokenの欄を検証したrun tokenで埋めてserviceのunix socketへ送るので、serviceはそのrunのprincipalで認可して書く。
  brokerはqueueのユースケースを自分で答えず、serviceに届かなければ`unreachable`で返し、DBを開いて代わりに答えない。
- 素通し: serviceの答えは長さで切らず、書かれたとおりに返す。
- 拒否を残すこと: 拒んだ要求は全てqueueのevent `queue_broker_denied`にrunと宛てを残す（欄は`domain::queue_broker::QUEUE_BROKER_DENIED`のdoc comment）。
  HTTPとして読めない要求も、読めたところまでのrunと宛てで残す（`infrastructure::queue_broker::read_request`）。
  何も送らずに閉じた接続は要求ではなく、残さない。
  actorはbroker自身（制御側の一部）で、tokenの値と、要求の中身を引くserdeの文言はevent・log・errorにも答えにも書かない。
- 1つのqueueに1つ: 生きている間lockを持ち、2つ目の`queue-broker serve`は止まる。
  見る側はlockとhealthのpathへの問い合わせで、生死とbuildを判定する（queue serviceと同じ形）。

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

## 見張りと知らせ

- supervisor（`--once`でないもの）はserviceと同じ間隔と上限で見て、居ない・答えないbrokerを起動し直す（`supervise::queue_broker`）。
  見る時期・窓の数え方・次の一手の判断は`domain::queue_broker::watch_step`ほかで、supervisorは注入した時計の単調時刻を値で渡す。
  drainと引き継ぎの間は見ない。
- 起動し直せない・上限に達したときは、inbox宛てのattention `queue_broker_down`をDBに直接書き、brokerがまた動けば消える。
- brokerが居ない間、containerは使えない: supervisorと`integrate`は`ContainerRuntime`を`BrokerGate`越しに問い、brokerが動いていなければ理由つきで使えないと答える（[Container runtime](container-runtime.md#fail-closedの境界)）。
  serviceと違い、hostのrunのclaimとjobは控えない。

## まだ無いもの

- `dagq.toml`が挙げるpackage registryの`CONNECT`のallowlistと、`api.anthropic.com`への資格情報の差し替え（ADR-t2114-1決定2、ADR-t2257-1決定3、ADR-t2114-2）。
- integrateのverificationのtoken（届く宛てはregistryのallowlistだけ）。
- `status`・`doctor`でのbrokerの見せ方と、人の手での起動・停止のコマンド。
- 拒否の記録の量の上限。
