---
id: plan-team-version-identity
type: plan
title: チームの版の識別の具体案（JWTの検証の欄・tokenの期限と置き場・CLIのコマンド・配置の設定・登録と記録の形）
status: active
created: 2026-11-03
owners:
  - hisamekms
tags:
  - architecture
  - security
  - authorization
related:
  - adr-t3985-1
  - adr-t3981-1
  - design-authorization
  - design-security
---

# チームの版の識別の具体案

[ADR-t3985-1](../adr/2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)の決定を実装するときの具体の候補を置く設計資料。
ADRは方針と境界だけを持ち、ここの名前・値・形は候補で、実装のtaskが選んで定義のそばのdoc commentへ移す。
まだ実装されていないので、`docs/design/`には書かない。

## JWTの検証（決定1）

| 項目 | 候補 |
| --- | --- |
| 鍵の取得 | `<issuer>/.well-known/openid-configuration`の`jwks_uri`。取得した鍵を記憶し、未知の`kid`で1回だけ取り直す（取り直しの間隔は最短60秒）。記憶の期限はHTTPの`Cache-Control`、無ければ1時間 |
| 署名の方式 | `RS256`・`ES256`・`EdDSA`だけ。`none`と`HS*`は拒む |
| `iss` | 配置の設定の発行元と完全一致（末尾の`/`も含めて） |
| `aud` | 配置の設定のaudienceを含む（配列なら要素のどれか） |
| 時刻 | `exp`・`nbf`・`iat`を見て、時計のずれは60秒まで許す |
| 人の同一性 | `(iss, sub)`の組。`email`・`name`は表示にだけ使う |
| GitHub ActionsのOIDC | issuerは`https://token.actions.githubusercontent.com`、`repository`・`workflow_ref`（か`job_workflow_ref`）・`ref`を配置の設定と照らす |

## tokenの期限と置き場（決定2・4）

| token | 期限の候補 | 置き場 |
| --- | --- | --- |
| 人のdagqのtoken | 交換の答えの期限 | 端末の置き場（実装と選び方は`infrastructure::credentials`） |
| 人のIdPの更新の資格 | IdPの既定 | 同上 |
| runnerの登録の値 | 1回きり・15分 | 人が手で渡す |
| runnerの更新の資格 | 30日、使うたびに回す | runnerのhostの`0600`のファイル |
| runnerのdagqのtoken | 15分 | runnerのプロセスの中だけ |

承認のloginの新しさは`domain::queue_service::APPROVAL_LOGIN_SECONDS`のdoc commentを読む。

## CLIの候補

- 人の端末の認証と資格の置き場は`application::login`と`infrastructure::credentials`の定義を読む。
- `dagq member add|role|remove`（管理者）
- `dagq runner register-code`（人が登録の値を得る）・`dagq runner enroll`（runnerの側）・`dagq runner list|revoke|share`

## 配置の設定の候補

配置ごとの設定（`dagq.toml`ではない。ADR-t3981-1決定3）に置く。

```toml
[identity]
issuer = "https://idp.example.com/"
audience = "dagq-cli"
client_id = "dagq-cli"
# GitHub Actions の控えの runner を受けるとき
[identity.github_actions]
repository = "org/repo"
workflow_ref = "org/repo/.github/workflows/runner.yml@refs/heads/main"
```

## 登録と記録の形の候補

- 人の登録: `members(issuer, subject, role, on_call, display_name, created_at, disabled_at)`。`(issuer, subject)`が一意。
- runnerの登録: `runners(id, owner_issuer, owner_subject, shared, approved_by, declared_os, declared_arch, declared_container, declared_credentials, created_at, revoked_at, last_seen_at)`。
- 監査の追跡（ADR-t3985-1決定8）の格納の候補: 認証した呼び出しのeventのactorの欄に`issuer`・`subject`か`runner_id`を足し、append-onlyのeventに残す。
  新しいeventの種類の候補: `token_issued`・`token_revoked`・`member_role_changed`・`runner_registered`・`runner_revoked`・`runner_shared`。
- 承認のeventとsquash commitのmessageには、承認した人の`(issuer, subject)`と表示名を残す（ADR-t3981-1決定7）。

## StaticPolicyへの足し方の候補

- `ActorContext`のuserに`TeamRole`（`Admin`・`Member`）と`on_call`の印を持たせ、ローカルの既定では`None`（今のuserの許可）にする。
- `grants(role)`は今のまま、`TeamRole`による狭めを`StaticPolicy`のresourceの規則の手前で引く。
- 新しいcapabilityの候補: `member.admin`（役割の付け替え）・`runner.register`・`runner.revoke`・`runner.share`。
- `approve_landing`の`land`のanswerは、チームの版の配置で`AnswerAuthority`が人自身（OIDCのprincipal）のときだけ通す。
