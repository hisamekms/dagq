---
id: plan-team-version-control-side
type: plan
title: チームの版の制御側の具体案（HTTPのpathとheader・版の判定・配置の設定の名前と書式・healthと停止の時間・migrateのコマンド・imageの名前と出し方・logの形と置き場への書き込みの許し・配置の文書の置き場と参考の構成）
status: active
created: 2026-11-04
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - queue-service
related:
  - adr-t3990-1
  - adr-t3990-2
  - adr-t3990-3
  - adr-t3990-4
  - adr-t3990-5
  - adr-t3981-1
  - adr-t3985-1
  - adr-t3986-1
  - adr-t3987-1
  - adr-t3989-1
  - adr-t3102-2
  - adr-t3102-3
  - plan-team-version-identity
  - plan-team-version-runner
  - plan-team-version-write-leader
---

# チームの版の制御側の具体案

[ADR-t3990-1](../adr/2026-11-04-t3990-1-team-version-http-api-carries-use-cases-with-dagq-tokens-and-an-api-version-range.md)（HTTPのAPI）、[ADR-t3990-2](../adr/2026-11-04-t3990-2-team-version-deployment-config-outside-dagq-toml-health-ordered-shutdown-and-separate-migrate.md)（設定の入口・healthと停止・migrate）、[ADR-t3990-3](../adr/2026-11-04-t3990-3-team-version-release-publishes-multi-arch-images-to-ghcr-pinned-by-digest.md)（image）、[ADR-t3990-4](../adr/2026-11-04-t3990-4-team-version-control-side-observability-by-stdout-logs-otlp-and-existing-ssot-commands.md)（観測）、[ADR-t3990-5](../adr/2026-11-04-t3990-5-team-version-deploy-documents-and-reference-configurations-without-iac.md)（配置の文書と参考の構成）の決定を実装するときの具体の候補を置く設計資料。
ADRは方針と境界だけを持ち、ここの名前・値・形は候補で、実装のtaskが選んで定義のそばのdoc commentへ移す。
まだ実装されていないので、`docs/design/`には書かない。
runnerの呼び出しの名前は[runnerの具体案](team-version-runner.md)、tokenと登録は[識別の具体案](team-version-identity.md)、係の行と時間は[書き込みの係の計画](team-version-write-leader.md)が持つ。

## HTTPの入口（ADR-t3990-1決定1・2）

| 項目 | 候補 |
| --- | --- |
| ユースケースの要求 | `POST /api/use-case/<ユースケースの名前>`。bodyは今のsocketの要求の1行のJSONと同じ形（`api_version`・宛てのqueue・params）で、pathの名前とbodyの名前が違えば`bad_request` |
| tokenの交換 | `POST /api/token`。bodyに発行元のJWT。答えはdagqの短いtoken（識別の具体案の「token」） |
| 認証のheader | `Authorization: Bearer <dagqのtoken>`。queryの`token`・cookieは読まない |
| health | `GET /healthz`。tokenなし。答えは`{"ok":true,"api_version":N,"min_api_version":M}`か、503と`{"ok":false}` |
| Claudeの中継 | `POST /anthropic/v1/messages`ほか上流のpathをそのまま。`Authorization: Bearer <runnerのtoken>`と`X-Dagq-Lease-Generation`。応答はSSEのstreamのまま流し、大きさの上限を当てない |
| 大きさの上限 | 要求と答えは今の`MAX_REQUEST_BYTES`・`MAX_ANSWER_BYTES`と同じ値をHTTPの層でも当てる |
| 転送のheader | `X-Forwarded-For`・`X-Forwarded-Proto`はlogにだけ写し、識別と認可に使わない |
| 宛てのURL | CLIとrunnerは`https://`だけを受け、`http://`は`127.0.0.1`・`::1`・`localhost`だけ（testと手元の試し用） |
| 届かない | DBに届かないときの答えは`store_unavailable`のcode、交代の間は書き込みの計画の「交代中」のcode |

## 版の判定（ADR-t3990-1決定3）

- 今の`API_VERSION`・`MIN_API_VERSION`をそのまま使い、HTTPでも`api_version_mismatch`で断る。答えのbodyに範囲を入れる（今と同じ）。
- runnerの登録の行に`client_version`（dagqのreleaseの版）と`api_version`を持たせ、生存の報告のたびに更新する。
- `dagq status`の制御側の節に「下限を上げると止まる登録」の数と持ち主を出す。
- 古いrunnerは`api_version_mismatch`を受けたら次の順で終わる（ADR-t3990-1決定3）。
  1. 新しいclaimを止める
  2. 自分が起動した走っているcontainerを全て止め（`podman stop`の後に猶予を過ぎれば`kill`）、止まったことを確かめる
  3. leaseは範囲の外の要求として断られるので返さない（制御側の期限で失われる）。cloneとrun dirは捨てる
  4. 終了の理由をlogに出して非0で終わる。containerが止まったと確かめられなければ、止められなかったcontainerの名前をlogに出してから終わる

## 配置の設定（ADR-t3990-2決定1）

環境変数の名前の候補。`_FILE`で終わる名前はその値をファイルのpathとして読み、両方があれば起動しない。

| 名前 | 中身 |
| --- | --- |
| `DAGQ_STORE` | `postgres`。無ければローカルの既定（SQLite） |
| `DAGQ_DATABASE_URL` / `_FILE` | Postgresの接続の文字列。`sslmode`は`disable`・`prefer`・`require`だけを受け、`prefer`と無指定もTLS必須で平文に落ちない。driverは`verify-full`を受けず、証明書の検証は常に行う |
| `DAGQ_DATABASE_CA_FILE` | 任意のPostgresのCA証明書のPEMファイルのpath |
| `DAGQ_OBJECT_STORE_ENDPOINT`・`DAGQ_OBJECT_STORE_BUCKET`・`DAGQ_OBJECT_STORE_REGION` | S3互換の宛て。endpointは必須（AWSのS3でも`https://s3.<region>.amazonaws.com`を書く） |
| `DAGQ_OBJECT_STORE_ACCESS_KEY_ID`・`DAGQ_OBJECT_STORE_SECRET_ACCESS_KEY` / `_FILE` | 明示の資格。無ければS3のSDKの標準の資格の探し方（AWSに固有のweb identity・containerの資格の宛てを含む。参考の構成のAWSではこれを使う） |
| `DAGQ_OIDC_ISSUER`・`DAGQ_OIDC_AUDIENCE` | 発行元とaudience。許す署名の方式は既定で`RS256`・`ES256` |
| `DAGQ_GITHUB_RUNNER_APP_ID`・`DAGQ_GITHUB_RUNNER_APP_KEY_FILE` | runner用のApp |
| `DAGQ_GITHUB_MAIN_APP_ID`・`DAGQ_GITHUB_MAIN_APP_KEY_FILE` | main用のApp |
| `DAGQ_GITHUB_REPOSITORY` | `owner/name` |
| `DAGQ_ANTHROPIC_API_KEY` / `_FILE` | チームのAPI key（無ければチームのAPI keyを許さない配置） |
| `DAGQ_IMAGE_CI_REPOSITORY`・`DAGQ_IMAGE_CI_WORKFLOW`・`DAGQ_IMAGE_CI_REF` | digestを受けるCIの識別の縛り |
| `DAGQ_TIMEZONE` | 見直しの時間帯（IANAの名前） |
| `DAGQ_LISTEN` | 受けるaddress（既定`0.0.0.0:8080`） |
| `DAGQ_TLS_CERT_FILE`・`DAGQ_TLS_KEY_FILE` | 任意。在れば制御側がTLSで受ける |
| `DAGQ_OTLP_ENDPOINT` | 任意。ADR-0033のOTLPの宛て |

- 起動の確かめ: `DAGQ_STORE=postgres`なら、DB・置き場・OIDC・2つのApp・repositoryが揃わなければ、欠けた名前を全て1回に出して終了コード78（`EX_CONFIG`）で終わる。
- mainの`dagq.toml`はrunner用のAppのinstallation token（`repositories`を1つ、`permissions`を`contents: read`だけに絞って発行）で`GET /repos/{owner}/{repo}/contents/dagq.toml?ref=<mainのsha>`で読み、mainのshaが変わったときだけ読み直す。

## healthと停止（ADR-t3990-2決定2・3）

- healthが悪いのは、停止を始めた後と、supervisorのループかHTTPの受けのtaskが一定の時間（候補: 60秒）進まないとき。DBに届かず`store_unavailable`で断ってpassを終えたloopは進んだ内に数える。
- SIGTERMの順と時間の候補（ECSの`stopTimeout`の既定30秒に収める）:
  1. 直ちにhealthを悪くし、新しいclaim・割り当て・着地を始めない
  2. 受けている要求と副作用の記録を最大20秒待つ
  3. 書き込みの列を空にして係を手放す（最大5秒）
  4. 終わる。猶予が尽きたら手放さずに終わる
- 参考の構成のALBのtarget groupの`deregistration_delay`は20秒、health checkの間隔は10秒・2回で悪い。
- runnerのleaseの測り直し: 係は「報告を受けて書けるようになった時刻」（係になった時刻と、DBに届かない状態から戻った時刻の遅い方）を持ち、runnerのleaseの期限の判定の起点を`max(leaseの最後の報告, その時刻)`にする。
- 下限を上げたbuildの引き継ぎ: 係を取る前に`runner_leases`の持ち主の`api_version`が新しい`MIN_API_VERSION`より古いものを数え、1つでもあれば引き継がずに待ち、待つ理由`min_api_version_blocked`は構造化のlogと、認証した`dagq status`の制御側の節にだけ出す（healthの答えには版の範囲のほかに何も出さない。ADR-t3990-2決定2）。

## migrate（ADR-t3990-2決定4）

- コマンドの候補: `dagq service migrate --once`（入口を開かず、配置の設定を読み、互換のものを適用して終わる）。`--check`は適用せず要るmigrationと互換の区分を出す。
- 非互換のmigrationがあるときは`--allow-incompatible`を要し、同じtransactionで係の行が手放されているか期限切れであることと、runとjobのlease（runnerのものも）が1つも無いことを確かめ、違えば`drain_required`で断る。
- 参考の構成ではECSのone-off task（`run-task`）で、制御側のserviceの更新の前に流す。
- serviceの起動の判定: schemaの版がbuildの要る版より古ければ`schema_behind`で終了コード78。

## image（ADR-t3990-3）

| 項目 | 候補 |
| --- | --- |
| 制御側のimage | `ghcr.io/hisamekms/dagq-control:<版>`。distrolessかscratchの上にdagqのbinaryとCAの証明書だけ |
| runのbase | `ghcr.io/hisamekms/dagq-run-base:<版>`（runnerの具体案の「image」） |
| arch | `linux/arm64`・`linux/amd64`の1つのmanifest list |
| build | `.github/workflows/release.yml`の`v*`のtagのjobに足す。`docker buildx`かpodmanの`--platform` |
| 出どころ | GitHubのartifact attestation（`actions/attest-build-provenance`）。releaseのnoteにdigestを書く |
| tag | 版だけ。`latest`と`main`を置かない |

## 観測（ADR-t3990-4）

- 制御側の指標（OTLPのmetric）: `dagq.write_queue.depth`・`dagq.write_queue.wait_ms`・`dagq.leader.generation`・`dagq.leader.takeovers`・`dagq.refused{reason}`・`dagq.runners{state}`。
- logの形: 1行1つのJSON（`ts`・`level`・`target`・`msg`・`use_case`・`principal`（actorとidだけ）・`status`・`duration_ms`・`request_id`）。今のtracingのJSON linesの形を標準出力に向ける。
- sessionのlogの置き場への書き込み: 制御側がleaseごとに`PUT`の署名つきURL（S3のpresigned URL、期限15分、keyは`runs/<run id>/<世代>/<連番>.log`）を出し、runnerはturnの終わりごとに1つ置いて連番を報告する。
- `run log`・`planner log`は制御側が置き場から読んで返す（大きいものは`GET`の署名つきURLを返す形も候補）。
- 毎時・日次・週次の見直しの時刻は`DAGQ_TIMEZONE`で決め、結論のattentionは当番の宛て（ownerとaskの具体案）に置く。

## 配置の文書と参考の構成（ADR-t3990-5）

- 置き場の候補: `docs/deploy/`。frontmatterの`type: deploy`を足し、`scripts/check-doc-frontmatter.sh`と[frontmatter仕様](../frontmatter.md)と[文書の規則](../development/documents.md)を同じ変更で直す。
- 文書の候補:
  - `docs/deploy/README.md`: 部品の一覧（image・設定・health・停止・migrate）と版の互換の表
  - `docs/deploy/requirements.md`: 配置する側が守ること（ADR-t3990-5決定1の列挙）とbackupと復元・DBの監視
  - `docs/deploy/github-apps.md`: 2つのAppの権限とruleset（runnerの具体案の「GitHub App」を写す）
  - `docs/deploy/reference-aws.md`: 参考の構成（ECS Fargate・ALB・RDS for PostgreSQL Multi-AZ・S3・Secrets Manager）
- 参考の構成の2つの形:
  - NATなし（既定）: ECSのtaskはpublicなsubnetでpublic IPを持ち、security groupはALBのsecurity groupからの受けだけを許す。外へはinternet gatewayで出る（GitHub・Anthropic・IdP・GHCR）。RDSはprivateなsubnet、S3はgateway endpoint。
  - NAT＋固定IP: taskはprivateなsubnetで、NAT gatewayとElastic IPで出る。外の側（GitHub Enterpriseの許可リストなど）が固定IPを求める組織向け。dagqの識別と認可はこのIPに依らない。
- どちらも図（d2）と、要る値（subnet・security group・環境変数・IAMの権限）の表で書き、TerraformやCloudFormationを置かない。
